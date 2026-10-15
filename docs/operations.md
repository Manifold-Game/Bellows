# Bellows operator runbook

This runbook covers a trusted, single-tenant production deployment of the
compiler cache. Bellows does not terminate TLS and is not a hostile
multi-tenant execution sandbox.

## Secure startup

Generate a high-entropy bearer token, store it in the CI secret manager, and
inject it into both server and clients as `BELLOWS_AUTH_TOKEN`. A listener on a
non-loopback address refuses to start without authentication. The escape hatch
`--allow-insecure-no-auth` is only for isolated disposable development
networks.

```bash
export BELLOWS_AUTH_TOKEN="$(openssl rand -hex 32)"
bellowsd \
  --listen 127.0.0.1:7878 \
  --data-dir /var/lib/bellows \
  --max-blob-mb 512 \
  --max-requests 128
```

Put a TLS reverse proxy or a private authenticated network in front of the
loopback service. Never place the plain HTTP listener directly on the public
internet. Only one `bellowsd` may own a data directory; a second process fails
at startup instead of risking split-brain leases.

The container listens on `0.0.0.0:7878`, so `BELLOWS_AUTH_TOKEN` is mandatory:

```bash
docker run --detach --name bellows \
  --restart unless-stopped \
  --env BELLOWS_AUTH_TOKEN \
  --publish 127.0.0.1:7878:7878 \
  --volume bellows-data:/var/lib/bellows \
  bellows:VERSION
```

`GET /live` is an unauthenticated liveness-only endpoint used by the container
health check. Every `/v1/*` endpoint requires the token when authentication is
configured. Readiness is verified with `bellows doctor` from the same network
and credentials as CI.

## Backups and upgrades

The data directory is a disposable cache, not a system of record. Backups are
optional. If retained, stop the daemon with SIGTERM and wait for a clean exit
before snapshotting the volume. Atomic record publication and directory fsyncs
make ordinary process interruption recoverable; malformed records are moved to
`quarantine/` and become safe cache misses.

Bellows currently has no stable wire or on-disk compatibility guarantee. The
client and server must use the same `PROTOCOL_VERSION`. For an upgrade:

1. Stop CI jobs that publish to the service.
2. Send SIGTERM and wait for `bellowsd` to exit.
3. Keep or snapshot the old data volume for rollback.
4. Deploy matching client and server binaries.
5. Start the daemon and run authenticated `bellows doctor`.
6. If the protocol changed, start with an empty data directory. Old records are
   rejected rather than trusted.
7. Run one cold canary and one fresh-runner warm canary before restoring normal
   concurrency.

Rolling mixed-version deployments are unsupported. A protocol mismatch is a
clean miss or an explicit doctor failure, never authorization to restore an old
artifact.

## Far servers: the relay

Cargo starts a new wrapper process for every rustc invocation, and each one
talks to the server on its own. Against a server tens of milliseconds away,
round trips, not bandwidth, decide how long a warm build takes. Every wrapper
keeps its own requests few and overlapping:

- A restore requests all of a unit's blobs at once, at most
  `BELLOWS_FETCH_CONCURRENCY` (1–64, default 16) at a time, instead of one after
  another. Archive and declared-action restores and uploads do the same.
- The empty blob (most compiler streams) is never requested: its digest is its
  proof.
- Blobs of 256 KiB or less are uploaded without asking first (`HEAD`) whether
  the server has them; the server keeps and refreshes a copy it already holds.
- The reuse mark (`POST …/used`) is sent while the blobs download, not after.
- Over HTTPS the client negotiates HTTP/2, so the requests of one wrapper share
  a single connection and TLS handshake. Plain HTTP stays HTTP/1.1.

A wrapper still pays a TCP and TLS handshake of its own, and waits for its
uploads before Cargo may start the units that depend on it. The **relay**
removes both. It is a loopback HTTP service that holds a warm connection pool
to the server for the whole build:

- A wrapper's request then costs one round trip to the server: a restore is
  about two (candidates, then blobs), however many blobs it needs.
- Uploads are answered once they are spooled to local disk and sent in the
  background, at most `BELLOWS_RELAY_UPLOADS` (1–64, default 8) at a time. A
  record goes up only after every blob accepted before it, a lease release
  only after every record before it, and any other change (archives, declared
  records, execution, collection) waits for earlier uploads before it is
  forwarded. Past `BELLOWS_RELAY_SPOOL_MB` (default 4096) of unsent blobs, a
  further upload waits for room, so the spool cannot grow without bound.
- Reads pass straight through. Wrappers verify every digest, and the relay
  verifies each spooled blob before uploading it.
- Wrappers authenticate to the relay with a random per-session token. The
  server's token stays in the relay process, out of Cargo's environment and
  so out of reach of build scripts and procedural macros.
- The pool uses several HTTP/1.1 connections rather than one HTTP/2 connection,
  so large blobs download on parallel TCP streams.

`bellows run` starts a relay for any server that is not on loopback, and drains
it after the command exits (up to `BELLOWS_RELAY_DRAIN_SECS`, default 600),
printing what it uploaded and any upload that failed. `BELLOWS_RELAY=0` turns
it off; `BELLOWS_RELAY=1` forces it on for a loopback server. A failed
background upload is a cache miss for a later build, never a failed build.

When Cargo runs with `RUSTC_WRAPPER` set directly, as CI usually does, start a
standalone relay before the first build and stop it with SIGTERM after the
last; it drains before it exits:

```bash
bellows relay --server https://bellows.example --env-file "$RUNNER_TEMP/bellows-relay.env" &
echo "BELLOWS_RELAY_PID=$!" >> "$GITHUB_ENV"
# The env file appears once the relay listens; hand its two lines to later steps.
for _ in $(seq 100); do [ -s "$RUNNER_TEMP/bellows-relay.env" ] && break; sleep 0.1; done
cat "$RUNNER_TEMP/bellows-relay.env" >> "$GITHUB_ENV"   # BELLOWS_SERVER, BELLOWS_AUTH_TOKEN
# ... builds ...
kill -TERM "$BELLOWS_RELAY_PID"; wait "$BELLOWS_RELAY_PID"   # in the teardown step
```

The relay reads the server's token from `--token` or `BELLOWS_AUTH_TOKEN`, as
any client does. Its env file (mode 0600) holds the session token, not the
server's. Mask it in CI logs like any secret. Run `bellows doctor` and
`bellows stats` through the relay or straight at the server; both work.

`scripts/latency/bench.sh` measures a warm build through
`scripts/latency/delay-proxy.py`, a TCP proxy that adds a fixed round trip
(and a handshake's worth of round trips per new connection), so changes to
this path can be compared at 0, 50, 100 and 250 ms without root.

### Server memory

`bellowsd` streams blob downloads from disk and blob uploads to a temporary
file beside their final location, hashing as they arrive; the store's
mutation lock is held only for the final rename. A large blob costs the
server a read buffer, not its size in memory, so `--max-requests` bounds
memory rather than `--max-requests` times `--max-blob-mb`. A record upload
rereads and rehashes every blob it names, off the async workers.

## Capacity and recovery

Use `bellows stats` for blob bytes, record counts, candidates, and leases.

Collection is least recently used. Every publication and every reuse appends
one line to the store's access journal (`access/`); clients report reuse with
`POST /v1/actions/{static}/{action}/used`, best effort. A candidate's last use
is the later of its publication and its newest journal entry. Collection reads
every record once, counts blob references, then evicts the least recently used
records (a whole declared or archive record counts as one) until the blobs fit
the budget. A blob is deleted only when no remaining record references it.

- `bellows gc --max-mb MEBIBYTES --dry-run` reports the plan without changing
  anything: totals, a breakdown by protocol, output kind, checkout pinning, age
  of last use and crate, and what each would free. Always run it first.
- `bellows gc --max-mb MEBIBYTES` collects a server (`--server`), the
  daemonless cache (`--local`), or a store directory (`--store-dir`). A
  directory a running `bellowsd` owns accepts only `--dry-run`.
- `--min-protocol N` also evicts every compiler record older than protocol
  `N`, whatever the budget. Current clients can never use them.
- `--max-executable-mb MEBIBYTES` gives final executables (binaries, test
  harnesses, examples) their own budget inside `--max-mb`. The least recently
  used executables are evicted first until the bytes only they reference fit;
  then the overall budget applies to every record as usual. Use it when a
  build links much more than it compiles, such as a CI lane linking hundreds
  of test binaries: by recency alone the links, published last, would evict
  the libraries the next build needs. Libraries, procedural macros, dylibs and
  build-script programs are never evicted to make room for executables. The
  report's kind breakdown lists executables separately from other linked
  outputs. The client refuses to report success from a server too old to
  apply it.
- `bellowsd --max-store-gb GIB` (or `BELLOWS_MAX_STORE_GB`) collects to that
  budget every `--gc-interval-mins` (default 30). It is off unless set.

Collection applies its plan in batches of 256 under the store's mutation
lock, so builds keep publishing while it runs. It never deletes a blob that
was uploaded or offered again since it started, an unreferenced blob younger
than the one-hour publication grace, or a blob of a record published or
reused since it started. A zero-byte target can therefore remain above target
while uploads are active.

Compiler-cache failures are fail-open: the wrapper records the failure and
runs official rustc. A CI job should still fail closed at setup by running
`bellows doctor`, so an unintended cache outage is visible before expensive
work starts. If a daemon is unhealthy:

1. Preserve logs and `quarantine/` for diagnosis.
2. Stop the daemon cleanly.
3. Restart against the same volume and run `bellows doctor`.
4. If corruption persists, move the volume aside and start with an empty one.
5. Run cold and warm canaries; do not copy individual records into the new
   store.

Remote execution remains disabled unless `--enable-execution` is explicitly
set. Keep it disabled for the Manifold compiler-cache deployment.

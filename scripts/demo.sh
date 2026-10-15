#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/bellows-demo.XXXXXX")"
server_pid=""
relay_pid=""

cleanup() {
  if [ -n "$relay_pid" ]; then
    kill "$relay_pid" 2>/dev/null || true
  fi
  if [ -n "$server_pid" ]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$scratch"
}
trap cleanup EXIT

section() {
  echo
  echo "════════════════════════════════════════════════════════════"
  echo "  $1"
  echo "════════════════════════════════════════════════════════════"
}

start_server() {
  "$bellowsd" --listen "127.0.0.1:$port" --data-dir "$data" >"$scratch/server.log" 2>&1 &
  server_pid=$!
  for _ in $(seq 1 50); do
    curl -fsS "$server/v1/health" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  echo "bellowsd did not become ready" >&2
  return 1
}

section "Building Bellows"
cargo build --manifest-path "$root/Cargo.toml" --bins --quiet
bellows="$root/target/debug/bellows"
bellowsd="$root/target/debug/bellowsd"

cp -a "$root/demo" "$scratch/workspace"
workspace="$scratch/workspace"
state="$scratch/client-state"
data="$scratch/server-data"
port="${BELLOWS_DEMO_PORT:-17878}"
server="http://127.0.0.1:$port"

start_server

export BELLOWS_STATE_DIR="$state"
export BELLOWS_L1=0

section "Doctor: official rustc + reachable remote cache"
"$bellows" doctor --server "$server"

section "Cold runner A: compile and publish"
(
  cd "$workspace"
  CARGO_TARGET_DIR="$scratch/runner-a-target" GITHUB_RUN_ID=1001 \
    GITHUB_SHA=aaaaaaaa GITHUB_ENV="$scratch/github-env-a" \
    "$bellows" run --server "$server" -- cargo build
) 2>&1 | tee "$scratch/cold.log"
grep -q 'CACHE MISS.*forge_core' "$scratch/cold.log"

section "Fresh runner B: restore libraries remotely, link locally"
(
  cd "$workspace"
  CARGO_TARGET_DIR="$scratch/runner-b-target" GITHUB_RUN_ID=2002 \
    GITHUB_SHA=bbbbbbbb GITHUB_ENV="$scratch/github-env-b" \
    "$bellows" run --server "$server" -- cargo build
  "$scratch/runner-b-target/debug/forge-cli"
) 2>&1 | tee "$scratch/fresh.log"
grep -q 'CACHE HIT.*forge_core' "$scratch/fresh.log"
grep -q 'Bellows remembered this build' "$scratch/fresh.log"

section "Durability: restart bellowsd and restore from the same CAS"
kill "$server_pid"
wait "$server_pid" 2>/dev/null || true
server_pid=""
start_server
(
  cd "$workspace"
  CARGO_TARGET_DIR="$scratch/runner-restarted-target" \
    "$bellows" run --server "$server" -- cargo build
) 2>&1 | tee "$scratch/restarted.log"
grep -q 'CACHE HIT.*forge_core' "$scratch/restarted.log"

section "Explainable invalidation: change a transitive module"
sed -i 's/    42/    43/' "$workspace/crates/forge-core/src/temperature.rs"
(
  cd "$workspace"
  CARGO_TARGET_DIR="$scratch/runner-c-target" \
    "$bellows" run --server "$server" -- cargo build
) 2>&1 | tee "$scratch/invalidation.log"
"$bellows" explain --limit 6 | tee "$scratch/explain.log"
grep -q 'input changed: .*temperature.rs' "$scratch/invalidation.log"
grep -q 'temperature.rs' "$scratch/explain.log"

section "Fleet single-flight: two cold runners request the same new action"
sed -i 's/    43/    44/' "$workspace/crates/forge-core/src/temperature.rs"
rm -rf "$scratch/runner-d-target" "$scratch/runner-e-target"
(
  cd "$workspace"
  CARGO_TARGET_DIR="$scratch/runner-d-target" BELLOWS_DEMO_COMPILE_DELAY_MS=2000 \
    "$bellows" run --server "$server" -- cargo build
) >"$scratch/runner-d.log" 2>&1 &
first=$!
(
  cd "$workspace"
  CARGO_TARGET_DIR="$scratch/runner-e-target" BELLOWS_DEMO_COMPILE_DELAY_MS=2000 \
    "$bellows" run --server "$server" -- cargo build
) >"$scratch/runner-e.log" 2>&1 &
second=$!
wait "$first"
wait "$second"
cat "$scratch/runner-d.log" "$scratch/runner-e.log" | grep -E 'WAITING|SHARED HIT|CACHE HIT|CACHE MISS' || true
grep -q 'SHARED HIT' "$scratch/runner-d.log" "$scratch/runner-e.log"

section "Relay: CI-style wrappers restore and publish through a standalone relay"
relay_env="$scratch/relay.env"
"$bellows" relay --server "$server" --env-file "$relay_env" >"$scratch/relay.log" 2>&1 &
relay_pid=$!
for _ in $(seq 1 100); do
  [ -s "$relay_env" ] && break
  sleep 0.1
done
relay_server="$(sed -n 's/^BELLOWS_SERVER=//p' "$relay_env")"
relay_token="$(sed -n 's/^BELLOWS_AUTH_TOKEN=//p' "$relay_env")"
[ "$(curl -s -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer wrong' "$relay_server/v1/health")" = 401 ]
relay_build() {
  rm -rf "$scratch/runner-r-target"
  (
    cd "$workspace"
    CARGO_TARGET_DIR="$scratch/runner-r-target" BELLOWS_SERVER="$relay_server" \
      BELLOWS_AUTH_TOKEN="$relay_token" RUSTC_WRAPPER="$bellows" cargo build
  ) 2>&1 | tee "$1"
}
relay_build "$scratch/relay-restore.log"
grep -q 'CACHE HIT.*forge_core' "$scratch/relay-restore.log"
sed -i 's/    44/    45/' "$workspace/crates/forge-core/src/temperature.rs"
# Outside the change guard's window: a source modified within 50 ms of a
# compile's start is kept local, never published.
sleep 0.2
relay_build "$scratch/relay-publish.log"
grep -q 'CACHE MISS.*forge_core' "$scratch/relay-publish.log"
# SIGTERM drains the background uploads before the relay exits.
kill -TERM "$relay_pid"
wait "$relay_pid"
relay_pid=""
cat "$scratch/relay.log"
grep -q 'PUBLISHED.*relay.*record' "$scratch/relay.log"
rm -rf "$scratch/runner-s-target"
(
  cd "$workspace"
  CARGO_TARGET_DIR="$scratch/runner-s-target" BELLOWS_RELAY=0 \
    "$bellows" run --server "$server" -- cargo build
) 2>&1 | tee "$scratch/relay-after.log"
grep -q 'CACHE HIT.*forge_core' "$scratch/relay-after.log"

section "Integrity: corrupt remote content and recover with a safe miss"
while IFS= read -r victim; do
  printf 'deliberately corrupt' > "$victim"
done < <(find "$data/blobs" -type f)
rm -rf "$scratch/runner-f-target"
(
  cd "$workspace"
  CARGO_TARGET_DIR="$scratch/runner-f-target" \
    "$bellows" run --server "$server" -- cargo build
) 2>&1 | tee "$scratch/integrity.log"
grep -q 'REJECTED' "$scratch/integrity.log"

section "Graceful fallback: stop bellowsd and keep building"
kill "$server_pid"
wait "$server_pid" 2>/dev/null || true
server_pid=""
rm -rf "$scratch/runner-g-target"
(
  cd "$workspace"
  CARGO_TARGET_DIR="$scratch/runner-g-target" \
    "$bellows" run --server "$server" -- cargo build
)

section "Demo complete"
echo "Remote hits, transitive invalidation, single-flight, relayed publication,"
echo "integrity recovery and offline fallback all passed. Feed the forge. Skip the wait."

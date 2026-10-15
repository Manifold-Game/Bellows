#!/usr/bin/env bash
# Time a warm remote-cache build through delay-proxy.py at several round-trip times.
#
#   scripts/latency/bench.sh --bin DIR --label NAME [--src DIR] [--rtts "0 50 100 250"]
#                            [--modes "direct run"] [--jobs 6] [--repeat 1] [--data DIR]
#                            [--cold] [-- cargo args...]
#
# DIR holds the `bellows` and `bellowsd` under test. The script starts a fresh
# bellowsd on loopback, seeds it with one cold build of --src (default: this
# checkout's sources, archived at HEAD), then for each RTT and mode rebuilds
# from an empty target and state directory with L1 off, so every unit restores
# from the server through the proxy. --data keeps the server's data directory
# there and seeds only when it is empty, so several clients (same protocol)
# can share one seed. --cold measures publication instead: every build starts
# against an empty server, so every unit compiles and uploads. Modes:
#   direct  RUSTC_WRAPPER set in the environment and plain cargo, as CI runs it
#   run     `bellows run -- cargo ...` (a Bellows parent; BELLOWS_RELAY=1 gives it a relay)
#   relay   a standalone `bellows relay` between the wrappers and the proxy
# Results go to stdout as TSV: label mode rtt_ms seconds connections hits misses
# drain_seconds, where seconds is Cargo's wall time and drain_seconds the time a
# standalone relay then took to finish its uploads after SIGTERM (`run` mode
# drains inside its own seconds).
# Work files live under ${BELLOWS_BENCH_DIR:-~/.cache/bellows-latency}.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
bin=; label=; src=; rtts="0 50 100 250"; modes="direct run"; jobs=6; repeat=1; data=; cold=
while [ $# -gt 0 ]; do
  case $1 in
    --bin) bin=$(cd "$2" && pwd); shift 2 ;;
    --label) label=$2; shift 2 ;;
    --src) src=$(cd "$2" && pwd); shift 2 ;;
    --rtts) rtts=$2; shift 2 ;;
    --modes) modes=$2; shift 2 ;;
    --jobs) jobs=$2; shift 2 ;;
    --repeat) repeat=$2; shift 2 ;;
    --data) data=$2; shift 2 ;;
    --cold) cold=1; shift ;;
    --) shift; break ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
done
[ -n "$bin" ] && [ -n "$label" ] || { echo "usage: bench.sh --bin DIR --label NAME ..." >&2; exit 2; }
cargo_args=("$@")
[ ${#cargo_args[@]} -gt 0 ] || cargo_args=(build --locked)
work=${BELLOWS_BENCH_DIR:-$HOME/.cache/bellows-latency}
mkdir -p "$work"
if [ -z "$src" ]; then
  src=$work/src
  if [ ! -e "$src/Cargo.toml" ]; then
    mkdir -p "$src"
    git -C "$here/../.." archive HEAD | tar -x -C "$src"
  fi
fi
run_dir=$work/run-$label
rm -rf "$run_dir"
mkdir -p "$run_dir"
token=$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')
server_port=$((17000 + RANDOM % 1000))
proxy_port=$((server_port + 1000))
relay_port=$((server_port + 2000))
data=${data:-$run_dir/data}
[ -n "$cold" ] && data=$run_dir/data   # emptied before every cold build
seeded=; [ -n "$(ls -A "$data" 2>/dev/null)" ] && seeded=1
[ -n "$cold" ] && seeded=1
server_pid=
start_server() {
  if [ -n "$server_pid" ]; then kill "$server_pid"; wait "$server_pid" 2>/dev/null || true; fi
  [ -n "$cold" ] && rm -rf "$data"
  BELLOWS_AUTH_TOKEN=$token "$bin/bellowsd" --listen "127.0.0.1:$server_port" \
    --data-dir "$data" >>"$run_dir/bellowsd.log" 2>&1 &
  server_pid=$!
  for _ in $(seq 100); do
    curl -fsS "http://127.0.0.1:$server_port/live" >/dev/null 2>&1 && break
    sleep 0.05
  done
}
cleanup() { kill "$server_pid" 2>/dev/null || true; kill "${proxy_pid:-}" "${relay_pid:-}" 2>/dev/null || true; }
trap cleanup EXIT
start_server

export CARGO_TARGET_DIR=$run_dir/target CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 \
  CARGO_PROFILE_TEST_DEBUG=0 BELLOWS_OUTPUT=summary BELLOWS_L1=0 CARGO_BUILD_JOBS=$jobs \
  BELLOWS_WORKSPACE=$src

# build MODE SERVER_URL: one wrapped build from an empty target and state directory.
build() {
  local mode=$1 url=$2
  rm -rf "$CARGO_TARGET_DIR" "$run_dir/state"
  mkdir -p "$run_dir/state"
  local start end status=0
  start=$(date +%s.%N)
  case $mode in
    direct | relay)
      (cd "$src" && BELLOWS_STATE_DIR=$run_dir/state BELLOWS_SERVER=$url BELLOWS_AUTH_TOKEN=${build_token:-$token} \
        RUSTC_WRAPPER=$bin/bellows cargo "${cargo_args[@]}") >"$run_dir/build.log" 2>&1 || status=$?
      ;;
    run)
      (cd "$src" && BELLOWS_STATE_DIR=$run_dir/state BELLOWS_AUTH_TOKEN=$token BELLOWS_RELAY=1 \
        "$bin/bellows" run --server "$url" -- cargo "${cargo_args[@]}") >"$run_dir/build.log" 2>&1 || status=$?
      ;;
  esac
  end=$(date +%s.%N)
  if [ "$status" -ne 0 ]; then
    echo "build failed ($mode, $url); see $run_dir/build.log" >&2
    tail -20 "$run_dir/build.log" >&2
    exit 1
  fi
  elapsed=$(python3 -c "print(round($end - $start, 1))")
  hits=$(grep -c '"kind":"hit"' "$run_dir/state/events.jsonl" || true)
  misses=$(grep -c '"kind":"miss"' "$run_dir/state/events.jsonl" || true)
}

# Seed: one cold build straight to the server.
if [ -z "$seeded" ]; then
  build direct "http://127.0.0.1:$server_port"
  echo "# seeded $label in ${elapsed}s (misses $misses)" >&2
fi

for rtt in $rtts; do
  for mode in $modes; do
    for _ in $(seq "$repeat"); do
      [ -n "$cold" ] && start_server
      python3 "$here/delay-proxy.py" --listen "127.0.0.1:$proxy_port" \
        --upstream "127.0.0.1:$server_port" --rtt-ms "$rtt" --stats "$run_dir/proxy.json" \
        >"$run_dir/proxy.log" 2>&1 &
      proxy_pid=$!
      for _ in $(seq 100); do
        curl -fsS "http://127.0.0.1:$proxy_port/live" >/dev/null 2>&1 && break
        sleep 0.05
      done
      url="http://127.0.0.1:$proxy_port"
      build_token=
      if [ "$mode" = relay ]; then
        BELLOWS_AUTH_TOKEN=$token "$bin/bellows" relay --server "$url" \
          --listen "127.0.0.1:$relay_port" --env-file "$run_dir/relay.env" \
          >"$run_dir/relay.log" 2>&1 &
        relay_pid=$!
        for _ in $(seq 100); do [ -s "$run_dir/relay.env" ] && break; sleep 0.05; done
        build_token=$(sed -n 's/^BELLOWS_AUTH_TOKEN=//p' "$run_dir/relay.env")
        url=$(sed -n 's/^BELLOWS_SERVER=//p' "$run_dir/relay.env")
      fi
      build "$mode" "$url"
      drain=0
      if [ "$mode" = relay ]; then
        drain_start=$(date +%s.%N)
        kill -TERM "$relay_pid"; wait "$relay_pid" 2>/dev/null || true
        drain=$(python3 -c "print(round($(date +%s.%N) - $drain_start, 1))")
        rm -f "$run_dir/relay.env"
      fi
      kill -TERM "$proxy_pid"; wait "$proxy_pid" 2>/dev/null || true
      connections=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["connections"])' "$run_dir/proxy.json")
      printf '%s\t%s\t%s\t%.1f\t%s\t%s\t%s\t%s\n' "$label" "$mode" "$rtt" "$elapsed" "$connections" "$hits" "$misses" "$drain"
    done
  done
done

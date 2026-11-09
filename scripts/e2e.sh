#!/usr/bin/env bash
# Run the end-to-end suite against `wrangler dev --local`, started from an installation as a
# deployer has it: deploy/wrangler.toml with the release archive unpacked into it, so the tested
# bytes are the shipped bytes.
#
#   scripts/e2e.sh [ARCHIVE]
#
# Without ARCHIVE, runs `just release` and uses dist/worker.tar.gz. Starts three local servers (see
# tests/e2e.rs), each with its own fresh D1 state, and stops them on exit. Extra arguments for
# cargo test go in E2E_CARGO_ARGS.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
archive="${1:-}"

if [[ -z "$archive" ]]; then
  just --justfile "$root/justfile" release
  archive="$root/dist/worker.tar.gz"
fi

if [[ -x "$root/deploy/node_modules/.bin/wrangler" ]]; then
  wrangler=("$root/deploy/node_modules/.bin/wrangler")
else
  wrangler=(npx --yes wrangler@4)
fi

export ATUIN_TEST_USERNAME="${ATUIN_TEST_USERNAME:-e2e-user}"
export ATUIN_TEST_PASSWORD="${ATUIN_TEST_PASSWORD:-e2e-password-0123456789}"

work="$(mktemp -d)"
pids=()
cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
  if [[ -n "${E2E_KEEP:-}" ]]; then echo "kept $work" >&2; else rm -rf "$work"; fi
}
trap cleanup EXIT

# Unpacked the way download-worker.sh does it; wrangler writes its local state here too.
install="$work/install"
mkdir -p "$install"
cp "$root/deploy/wrangler.toml" "$install/"
tar -xzf "$archive" -C "$install" build migrations

printf 'ATUIN_USERNAME=%s\nATUIN_PASSWORD=%s\n' "$ATUIN_TEST_USERNAME" "$ATUIN_TEST_PASSWORD" \
  > "$work/secrets.env"
: > "$work/empty.env"

port_base="${E2E_PORT_BASE:-8790}"

# start NAME OFFSET ENV_FILE: migrate a fresh local database, start a server on it, and set
# $started to its URL
start() {
  local name="$1" offset="$2" env_file="$3"
  local port=$((port_base + offset)) state="$work/$name"
  mkdir -p "$state"
  (cd "$install" && "${wrangler[@]}" d1 migrations apply DB --local --persist-to "$state") \
    > "$work/$name-migrate.log" 2>&1
  (cd "$install" && exec "${wrangler[@]}" dev --local --ip 127.0.0.1 --port "$port" \
    --inspector-port $((port + 1000)) --persist-to "$state" --env-file "$env_file" \
    --show-interactive-dev-session=false) > "$work/$name.log" 2>&1 &
  pids+=("$!")
  started="http://127.0.0.1:$port"
}

start main 0 "$work/secrets.env" && ATUIN_TEST_SERVER_URL="$started"
start scratch 1 "$work/secrets.env" && ATUIN_TEST_SCRATCH_URL="$started"
start unconfigured 2 "$work/empty.env" && ATUIN_TEST_UNCONFIGURED_URL="$started"
export ATUIN_TEST_SERVER_URL ATUIN_TEST_SCRATCH_URL ATUIN_TEST_UNCONFIGURED_URL

for url in "$ATUIN_TEST_SERVER_URL" "$ATUIN_TEST_SCRATCH_URL" "$ATUIN_TEST_UNCONFIGURED_URL"; do
  for _ in $(seq 1 120); do
    curl -fsS "$url/healthz" > /dev/null 2>&1 && continue 2
    sleep 1
  done
  echo "server at $url did not come up; logs in $work" >&2
  E2E_KEEP=1
  exit 1
done

cd "$root"
# shellcheck disable=SC2086
cargo test --test e2e ${E2E_CARGO_ARGS:-} -- --nocapture

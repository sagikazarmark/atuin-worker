#!/usr/bin/env bash
# CLI smoke test: the stock `atuin` client against `wrangler dev --local`, two config dirs standing
# in for two machines.
#
#   scripts/smoke.sh [ARCHIVE]
#
# Runs `atuin` on PATH, which the devenv shell provides at the right version; set ATUIN_BIN to use
# another binary. It must be atuin 18.24.4. Without ARCHIVE, runs `just release` and uses
# dist/worker.tar.gz. Covers: login, recording and syncing history, pulling it on the other
# machine, a repeated sync uploading nothing, `store push --force`, and a password rotation that
# logs every machine out until it logs in again.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
archive="${1:-}"
atuin_bin="${ATUIN_BIN:-atuin}"

if ! "$atuin_bin" --version | grep -q '^atuin 18\.24\.4'; then
  echo "ATUIN_BIN must be atuin 18.24.4, got: $("$atuin_bin" --version)" >&2
  exit 1
fi

if [[ -z "$archive" ]]; then
  just --justfile "$root/justfile" release
  archive="$root/dist/worker.tar.gz"
fi

if [[ -x "$root/deploy/node_modules/.bin/wrangler" ]]; then
  wrangler=("$root/deploy/node_modules/.bin/wrangler")
else
  wrangler=(npx --yes wrangler@4)
fi

username="smoke-user"
password="smoke-password-0123456789"
port="${SMOKE_PORT:-8780}"
url="http://127.0.0.1:$port"
work="$(mktemp -d)"
server_pid=""

cleanup() {
  [[ -n "$server_pid" ]] && kill "$server_pid" 2>/dev/null || true
  wait 2>/dev/null || true
  if [[ -n "${SMOKE_KEEP:-}" ]]; then echo "kept $work" >&2; else rm -rf "$work"; fi
}
trap cleanup EXIT

# An installation as a deployer has it: deploy/wrangler.toml with the archive unpacked into it the
# way download-worker.sh does it. wrangler writes its local state there too.
install="$work/install"
mkdir -p "$install"
cp "$root/deploy/wrangler.toml" "$install/"
tar -xzf "$archive" -C "$install" build migrations

step() { printf '\n== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; SMOKE_KEEP=1; exit 1; }

start_server() {
  printf 'ATUIN_USERNAME=%s\nATUIN_PASSWORD=%s\n' "$username" "$password" > "$work/secrets.env"
  (cd "$install" && exec "${wrangler[@]}" dev --local --ip 127.0.0.1 --port "$port" \
    --inspector-port $((port + 1000)) --persist-to "$work/state" --env-file "$work/secrets.env" \
    --show-interactive-dev-session=false) >> "$work/server.log" 2>&1 &
  server_pid=$!
  for _ in $(seq 1 120); do
    curl -fsS "$url/healthz" > /dev/null 2>&1 && return 0
    sleep 1
  done
  fail "server did not come up; see $work/server.log"
}

stop_server() {
  kill "$server_pid"
  wait "$server_pid" 2>/dev/null || true
  server_pid=""
}

# machine NAME ARGS...: run atuin as one of the two machines.
machine() {
  local name="$1"
  shift
  ATUIN_CONFIG_DIR="$work/$name/config" ATUIN_DATA_DIR="$work/$name/data" \
    ATUIN_ENCRYPTION_KEY="$key" ATUIN_PASSWORD="$password" ATUIN_LOG=error \
    ATUIN_SESSION=0190f1a2000070008000000000000001 \
    "$atuin_bin" "$@" < /dev/null
}

setup_machine() {
  mkdir -p "$work/$1/config" "$work/$1/data"
  cat > "$work/$1/config/config.toml" <<EOF
sync_address = "$url"
sync_protocol = "legacy"
auto_sync = false
update_check = false

[sync]
records = true
EOF
}

record() {
  local name="$1" cmd="$2" id
  id="$(machine "$name" history start -- "$cmd")"
  machine "$name" history end --exit 0 --duration 1 "$id" > /dev/null
}

key="$(head -c 32 /dev/urandom | base64)"

step "server"
(cd "$install" && "${wrangler[@]}" d1 migrations apply DB --local --persist-to "$work/state") \
  > "$work/migrate.log" 2>&1
start_server

step "machine A: login, record, sync"
setup_machine a
machine a login -u "$username"
record a "echo smoke-one"
record a "echo smoke-two"
machine a sync

step "machine B: login, sync, see A's history"
setup_machine b
machine b login -u "$username"
machine b sync
history_b="$(machine b history list --cmd-only)"
grep -q 'echo smoke-one' <<< "$history_b" || fail "B is missing A's history: $history_b"
grep -q 'echo smoke-two' <<< "$history_b" || fail "B is missing A's history: $history_b"

step "machine B: record; machine A pulls it"
record b "echo smoke-three"
machine b sync
machine a sync
machine a history list --cmd-only | grep -q 'echo smoke-three' || fail "A is missing B's history"

step "a repeated sync uploads nothing"
uploads() { grep -c '"route":"POST /api/v0/record"' "$work/server.log" || true; }
before="$(uploads)"
[[ "$before" -ge 1 ]] || fail "no upload was logged"
machine a sync
machine b sync
[[ "$(uploads)" == "$before" ]] || fail "a repeated sync uploaded records"

step "machine B: store push --force rebuilds the remote"
machine b store push --force
deletes="$(grep -c '"route":"DELETE /api/v0/store"' "$work/server.log" || true)"
[[ "$deletes" -ge 1 ]] || fail "store push --force did not wipe the remote"
setup_machine c
machine c login -u "$username"
machine c sync
history_c="$(machine c history list --cmd-only)"
for cmd in smoke-one smoke-two smoke-three; do
  grep -q "echo $cmd" <<< "$history_c" || fail "a fresh machine is missing $cmd after the push"
done

step "password rotation logs every machine out"
stop_server
password="smoke-password-rotated-987654"
start_server
if out="$(machine a sync 2>&1)"; then
  fail "sync succeeded with the old session: $out"
fi
grep -q '403' <<< "$out" || fail "expected a 403 after rotation, got: $out"
grep -q 'session not found' <<< "$out" || fail "expected 'session not found', got: $out"
machine a logout > /dev/null
machine a login -u "$username"
machine a sync

step "ok"

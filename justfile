set lazy
set default-list
set ignore-comments

# Native Rust checks: fmt, clippy, test, audit, deny, doc.
mod rust ".just/rust.just"

# Run all CI jobs but `e2e` and `smoke` (long, and they need wrangler and the atuin CLI).
ci: rust::fmt clippy rust::test rust::doc rust::audit rust::deny

# Run clippy, natively (with the tests) and on wasm32 (the Worker).
clippy: rust::clippy
    cargo clippy --lib --target "wasm32-unknown-unknown" --locked -- --deny warnings

# Build the Worker into build/ with worker-build, from the committed Cargo.lock.
build:
    cargo fetch --locked
    worker-build --release

# Build, then package the release in dist/: worker.tar.gz (build/index.js, build/index_bg.wasm, build/LICENSE, migrations/) and its checksum; with a version, also download-worker.sh, which installs that version.
[script("bash")]
release version="": build
    set -euo pipefail

    dist="{{ absolute_path("dist") }}"
    stage="$(mktemp -d)"
    trap 'rm -rf "$stage"' EXIT

    # Only the Worker, not the rest of worker-build's output. An installation (deploy/, or the
    # repository the Deploy to Cloudflare button creates from it) brings its own wrangler.toml and
    # package.json, so there is no wrapper directory: unpacked inside it, the archive replaces
    # build/ and migrations/.
    mkdir -p "$stage/build" "$stage/migrations" "$dist"
    # The Worker is AGPL-3.0-only; the license goes with it (deploy/ itself is 0BSD).
    cp build/index.js build/index_bg.wasm "$stage/build/"
    cp LICENSE "$stage/build/LICENSE"
    cp migrations/*.sql "$stage/migrations/"

    # COPYFILE_DISABLE keeps macOS tar from adding ._* metadata files.
    COPYFILE_DISABLE=1 tar -C "$stage" -czf "$dist/worker.tar.gz" build migrations
    (cd "$dist" && sha256sum worker.tar.gz > worker.tar.gz.sha256)

    # Never leave a download-worker.sh behind that points at a different archive.
    rm -f "$dist/download-worker.sh"
    if [[ -n "{{ version }}" ]]; then
        read -r sha256 _ < "$dist/worker.tar.gz.sha256"
        sed -e "s|@VERSION@|{{ version }}|g" -e "s|@SHA256@|${sha256}|g" \
            scripts/download-worker.sh.tmpl > "$dist/download-worker.sh"
        sh -n "$dist/download-worker.sh"
    fi

# Run the end-to-end tests (tests/e2e.rs) against `wrangler dev --local` on a release archive; without one, run `release` and use dist/worker.tar.gz.
e2e archive="":
    scripts/e2e.sh "{{ archive }}"

# Run the CLI smoke test with the stock atuin client (ATUIN_BIN, or `atuin` on PATH) on a release archive; without one, run `release` and use dist/worker.tar.gz.
smoke archive="":
    scripts/smoke.sh "{{ archive }}"

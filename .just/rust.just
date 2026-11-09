set working-directory := ".."

# Check formatting.
fmt:
    cargo fmt --all --check

# Run clippy natively, with the tests.
clippy:
    cargo clippy --all-targets --locked -- --deny warnings

# Run the unit tests and the drift test (the vendored types and capability token against upstream).
test:
    cargo test --lib --test drift --locked

# Audit dependencies for security advisories.
audit:
    cargo audit

# Check dependency licenses, bans and sources (advisories are `audit`'s job).
deny:
    cargo deny --locked check bans licenses sources

# Build the documentation, failing on rustdoc warnings; extra arguments go to cargo doc (e.g. `--open`).
doc *args:
    RUSTDOCFLAGS="--deny warnings" cargo doc --no-deps --locked {{ args }}

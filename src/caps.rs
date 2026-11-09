//! The capability document served by `GET /api/v0/capabilities`, and its token.
//!
//! Upstream (`CapServer::bake`) hashes the canonical JSON of the `capabilities` object with xxh3
//! and serves `{"version": <token>, "capabilities": {…}}`. The canonical bytes are written out by
//! hand here rather than produced by `serde_json`: whether `serde_json` keeps insertion order or
//! sorts keys depends on its `preserve_order` feature, which other crates in a build can switch on,
//! and the token must not change with the dependency graph. `tests/drift.rs` checks the bytes
//! against upstream.

/// Request header: the token the client currently knows.
pub const KNOWN_HEADER: &str = "x-atuin-capabilities-known";
/// Response header: the token the server actually has, sent when the client's is stale.
pub const AVAILABLE_HEADER: &str = "x-atuin-capabilities-available";
/// Request header: present when the client wants a stale token rejected with 412.
pub const ENFORCE_HEADER: &str = "x-atuin-capabilities-enforce";

/// The capability document and its token, computed once per isolate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    token: String,
    body: String,
}

impl Capabilities {
    /// The capabilities this server advertises: the capability protocol itself, and the page size
    /// clients should use for uploads and downloads.
    pub fn new(page_size: u64) -> Self {
        // Outer keys sorted by name, as upstream's `BTreeSet`; inner keys in upstream's field
        // declaration order.
        let capabilities = format!(
            concat!(
                r#"{{"sh.atuin.server/capabilities":{{"version":1}},"#,
                r#""sh.atuin.server/records.page_size":{{"version":1,"page_size":{}}}}}"#,
            ),
            page_size
        );
        let token = format!(
            "{:016x}",
            xxhash_rust::xxh3::xxh3_64(capabilities.as_bytes())
        );
        let body = format!(r#"{{"version":"{token}","capabilities":{capabilities}}}"#);
        Self { token, body }
    }

    /// The 16-character lowercase hex token clients echo in `x-atuin-capabilities-known`.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The document served verbatim by `GET /api/v0/capabilities`.
    pub fn body(&self) -> &str {
        &self.body
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn document_has_the_upstream_shape() {
        let caps = Capabilities::new(100);
        let doc: Value = serde_json::from_str(caps.body()).unwrap();
        assert_eq!(
            doc,
            json!({
                "version": caps.token(),
                "capabilities": {
                    "sh.atuin.server/capabilities": {"version": 1},
                    "sh.atuin.server/records.page_size": {"page_size": 100, "version": 1}
                }
            })
        );
    }

    #[test]
    fn token_is_16_lowercase_hex_characters() {
        let token = Capabilities::new(100).token().to_owned();
        assert_eq!(token.len(), 16);
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        );
    }

    #[test]
    fn token_depends_on_the_page_size() {
        assert_eq!(Capabilities::new(100), Capabilities::new(100));
        assert_ne!(
            Capabilities::new(100).token(),
            Capabilities::new(50).token()
        );
    }

    #[test]
    fn token_hashes_the_capabilities_object() {
        let caps = Capabilities::new(100);
        let doc: Value = serde_json::from_str(caps.body()).unwrap();
        let start = caps.body().find(r#""capabilities":"#).unwrap() + r#""capabilities":"#.len();
        let canonical = &caps.body()[start..caps.body().len() - 1];
        assert_eq!(
            serde_json::from_str::<Value>(canonical).unwrap(),
            doc["capabilities"]
        );
        assert_eq!(
            caps.token(),
            format!("{:016x}", xxhash_rust::xxh3::xxh3_64(canonical.as_bytes()))
        );
    }
}

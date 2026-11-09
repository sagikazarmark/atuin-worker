//! The wire types of the Atuin sync protocol, vendored from `atuin-domain` at v18.24.4.
//!
//! `atuin-domain` itself cannot be a dependency: it pulls in `tokio`, `sqlx` and `reqwest`, which
//! do not build for `wasm32-unknown-unknown`. `tests/drift.rs` round-trips every type here through
//! the upstream definitions so this copy cannot silently diverge.
//!
//! Identifiers (record and host IDs) stay strings: the server stores and returns them verbatim and
//! never interprets them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `GET /healthz`.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthResponse {
    pub status: String,
}

/// Every error body: `{"reason": "…"}`.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorResponse {
    pub reason: String,
}

/// `GET /`.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexResponse {
    pub homage: String,
    pub version: String,
}

/// `GET /api/v0/me`.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MeResponse {
    pub username: String,
}

/// `POST /login` request body.
///
/// Deliberately not `Debug`: it carries the submitted password.
#[derive(Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
    /// Accepted for compatibility and ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp_code: Option<String>,
}

/// `POST /login` response body.
///
/// Deliberately not `Debug`: it carries the session token.
#[derive(Serialize, Deserialize)]
pub struct LoginResponse {
    pub session: String,
    /// `"cli"` for CLI session tokens (as opposed to Hub tokens).
    #[serde(default)]
    pub auth: Option<String>,
}

/// The host a record belongs to.
///
/// On the wire this is `{"id": "<uuid>", "name": ""}`: `name` once carried meaning, is now always
/// empty, and is ignored when reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub id: String,
}

impl Serialize for Host {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Wire<'a> {
            id: &'a str,
            name: &'static str,
        }
        Wire {
            id: &self.id,
            name: "",
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Host {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            id: String,
        }
        let Wire { id } = Wire::deserialize(deserializer)?;
        Ok(Self { id })
    }
}

/// A record's encrypted payload: `{"data": "…", "content_encryption_key": "…"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedData {
    /// The encrypted payload. `MAX_RECORD_SIZE` is measured on this field alone, as upstream.
    #[serde(rename = "data", alias = "raw")]
    pub data: String,
    /// The wrapped content encryption key.
    #[serde(rename = "content_encryption_key", alias = "cek")]
    pub cek: String,
}

/// One record of the store, as uploaded and downloaded (`Record<EncryptedData>` upstream).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The client-generated record ID.
    pub id: String,
    /// The position of this record in its (host, tag) series.
    pub idx: u64,
    pub host: Host,
    /// Creation time in nanoseconds since the Unix epoch. Exceeds 2^53, so it must never pass
    /// through a JavaScript number.
    pub timestamp: u64,
    pub version: String,
    pub tag: String,
    pub data: EncryptedData,
}

/// `GET /api/v0/record`: `{"hosts": {<host>: {<tag>: <max idx>}}}`.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordStatus {
    pub hosts: BTreeMap<String, BTreeMap<String, u64>>,
}

impl RecordStatus {
    /// Record `idx` as the head of the (host, tag) series.
    pub fn set(&mut self, host: String, tag: String, idx: u64) {
        self.hosts.entry(host).or_default().insert(tag, idx);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn record() -> Record {
        Record {
            id: "0190f1a2-0000-7000-8000-000000000001".into(),
            idx: 7,
            host: Host {
                id: "0190f1a2-0000-7000-8000-000000000002".into(),
            },
            timestamp: 1_730_000_000_123_456_789,
            version: "v0".into(),
            tag: "history".into(),
            data: EncryptedData {
                data: "v4.local.payload".into(),
                cek: r#"{"wpk":"k4.local-wrap.pie.x","kid":"k4.lid.y"}"#.into(),
            },
        }
    }

    #[test]
    fn record_serializes_in_the_upstream_shape() {
        let value = serde_json::to_value(record()).unwrap();
        assert_eq!(
            value,
            json!({
                "id": "0190f1a2-0000-7000-8000-000000000001",
                "idx": 7,
                "host": {"id": "0190f1a2-0000-7000-8000-000000000002", "name": ""},
                "timestamp": 1_730_000_000_123_456_789_u64,
                "version": "v0",
                "tag": "history",
                "data": {
                    "data": "v4.local.payload",
                    "content_encryption_key": r#"{"wpk":"k4.local-wrap.pie.x","kid":"k4.lid.y"}"#
                }
            })
        );
    }

    #[test]
    fn host_name_is_ignored_when_reading() {
        let host: Host = serde_json::from_str(r#"{"id":"h","name":"laptop"}"#).unwrap();
        assert_eq!(
            serde_json::to_string(&host).unwrap(),
            r#"{"id":"h","name":""}"#
        );
    }

    #[test]
    fn timestamps_above_2_pow_53_round_trip_exactly() {
        let mut r = record();
        r.timestamp = (1 << 53) + 1;
        let text = serde_json::to_string(&r).unwrap();
        assert!(text.contains("\"timestamp\":9007199254740993"));
        let back: Record = serde_json::from_str(&text).unwrap();
        assert_eq!(back.timestamp, (1 << 53) + 1);
    }

    #[test]
    fn encrypted_data_accepts_the_storage_aliases() {
        let data: EncryptedData = serde_json::from_str(r#"{"raw":"a","cek":"b"}"#).unwrap();
        assert_eq!(data.data, "a");
        assert_eq!(data.cek, "b");
    }

    #[test]
    fn login_request_tolerates_a_missing_totp_code() {
        let req: LoginRequest = serde_json::from_str(r#"{"username":"u","password":"p"}"#).unwrap();
        assert_eq!(req.username, "u");
        assert!(req.totp_code.is_none());
    }
}

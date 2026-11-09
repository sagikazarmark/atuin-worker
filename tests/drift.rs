//! Drift test: the vendored wire types and the capability document against `atuin-domain` at the
//! pinned upstream tag. When upstream changes a shape, a string or the capability token, this
//! fails before a client does.

use std::collections::HashMap;

use atuin_domain::api::{
    ATUIN_CARGO_VERSION, ATUIN_HEADER_VERSION, CapabilitiesResponse, ErrorResponse, IndexResponse,
    LoginRequest, LoginResponse, MeResponse,
};
use atuin_domain::caps::{CapServer, CapabilitiesCap, PageSizeCap};
use atuin_domain::record::{
    EncryptedData, Host, HostId, Record, RecordId, RecordStatus, RecordTag,
};
use atuin_worker::{caps, proto, wire};
use secrecy::ExposeSecret;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use uuid::Uuid;

/// Serialize `value`, read it back as `To`, and serialize that: the JSON must not change.
fn round_trip<To: Serialize + DeserializeOwned>(value: &impl Serialize) -> (Value, Value) {
    let original = serde_json::to_value(value).unwrap();
    let converted: To = serde_json::from_value(original.clone()).unwrap();
    (original, serde_json::to_value(converted).unwrap())
}

fn upstream_record(tag: RecordTag, timestamp: u64) -> Record<EncryptedData> {
    Record::builder()
        .id(RecordId(Uuid::now_v7()))
        .host(Host::new(HostId(Uuid::now_v7())))
        .version("v0".to_owned().into())
        .tag(tag)
        .idx(42)
        .timestamp(timestamp)
        .data(EncryptedData {
            raw: "v4.local.payload".to_owned(),
            cek: r#"{"wpk":"k4.local-wrap.pie.x","kid":"k4.lid.y"}"#.to_owned(),
        })
        .build()
}

#[test]
fn records_round_trip_both_ways() {
    for (tag, timestamp) in [
        (RecordTag::History, 1_730_000_000_123_456_789),
        (RecordTag::Kv, (1 << 53) + 1),
        (RecordTag::DotfilesVar, 0),
        (RecordTag::Other("custom".to_owned()), u64::MAX),
    ] {
        let upstream = upstream_record(tag, timestamp);

        let (original, ours) = round_trip::<wire::Record>(&upstream);
        assert_eq!(original, ours);

        let (ours, back) = round_trip::<Record<EncryptedData>>(
            &serde_json::from_value::<wire::Record>(ours).unwrap(),
        );
        assert_eq!(ours, back);

        let decoded: Record<EncryptedData> = serde_json::from_value(back).unwrap();
        assert_eq!(decoded, upstream);
    }
}

#[test]
fn record_status_round_trips() {
    let host = HostId(Uuid::now_v7());
    let mut status = RecordStatus::new();
    status.set_raw(
        atuin_domain::record::RecordSeriesKey::new(host, RecordTag::History),
        7,
    );
    status.set_raw(
        atuin_domain::record::RecordSeriesKey::new(host, RecordTag::Kv),
        0,
    );

    let (original, ours) = round_trip::<wire::RecordStatus>(&status);
    assert_eq!(original, ours);
    let (_, back) = round_trip::<RecordStatus>(&ours);
    assert_eq!(original, back);
}

#[test]
fn login_types_round_trip() {
    let request = LoginRequest {
        username: "alice".to_owned(),
        password: "correct-horse-battery-staple".to_owned().into(),
        totp_code: None,
    };
    let (original, ours) = round_trip::<wire::LoginRequest>(&request);
    assert_eq!(original, ours);

    let with_totp = serde_json::json!({"username": "a", "password": "p", "totp_code": "123456"});
    let ours: wire::LoginRequest = serde_json::from_value(with_totp).unwrap();
    assert_eq!(ours.totp_code.as_deref(), Some("123456"));

    let ours = wire::LoginResponse {
        session: "token".to_owned(),
        auth: Some("cli".to_owned()),
    };
    let upstream: LoginResponse =
        serde_json::from_value(serde_json::to_value(&ours).unwrap()).unwrap();
    assert_eq!(upstream.session.expose_secret(), "token");
    assert_eq!(upstream.auth.as_deref(), Some("cli"));
}

#[test]
fn small_responses_round_trip() {
    let (a, b) = round_trip::<wire::IndexResponse>(&IndexResponse {
        homage: "h".to_owned(),
        version: "18.24.4".to_owned(),
    });
    assert_eq!(a, b);

    let (a, b) = round_trip::<wire::MeResponse>(&MeResponse {
        username: "alice".to_owned(),
    });
    assert_eq!(a, b);

    let (a, b) = round_trip::<wire::ErrorResponse>(&ErrorResponse {
        reason: "session not found".into(),
    });
    assert_eq!(a, b);
}

fn upstream_caps(page_size: u64) -> CapServer {
    // As upstream's `router::capabilities()`.
    CapServer::new()
        .add(CapabilitiesCap { version: 1 })
        .unwrap()
        .add(PageSizeCap {
            version: 1,
            page_size,
        })
        .unwrap()
}

#[test]
fn capability_token_matches_upstream() {
    for page_size in [100, 50, 1] {
        let ours = caps::Capabilities::new(page_size);
        let upstream = upstream_caps(page_size);
        assert_eq!(ours.token(), upstream.token(), "page_size {page_size}");
        assert_eq!(ours.body(), upstream.body(), "page_size {page_size}");
    }

    let doc: CapabilitiesResponse =
        serde_json::from_str(caps::Capabilities::new(100).body()).unwrap();
    let expected: HashMap<String, Value> = HashMap::from([
        (
            "sh.atuin.server/capabilities".to_owned(),
            serde_json::json!({"version": 1}),
        ),
        (
            "sh.atuin.server/records.page_size".to_owned(),
            serde_json::json!({"version": 1, "page_size": 100}),
        ),
    ]);
    assert_eq!(doc.capabilities, expected);
}

#[test]
fn headers_and_version_match_upstream() {
    use atuin_domain::caps::http;

    assert_eq!(proto::ATUIN_VERSION, ATUIN_CARGO_VERSION);
    assert!(proto::VERSION_HEADER.eq_ignore_ascii_case(ATUIN_HEADER_VERSION));
    assert_eq!(caps::KNOWN_HEADER, http::KNOWN_HEADER);
    assert_eq!(caps::AVAILABLE_HEADER, http::AVAILABLE_HEADER);
    assert_eq!(caps::ENFORCE_HEADER, http::ENFORCE_HEADER);
}

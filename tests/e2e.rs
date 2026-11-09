//! End-to-end tests against `wrangler dev --local`, where Miniflare runs real D1 and real
//! rate-limit bindings. `scripts/e2e.sh` starts the servers and runs this file; by hand:
//!
//! ```text
//! ATUIN_TEST_SERVER_URL=http://127.0.0.1:8787 cargo test --test e2e
//! ```
//!
//! | Variable                      | Server                                          |
//! |-------------------------------|-------------------------------------------------|
//! | `ATUIN_TEST_SERVER_URL`       | migrated and configured: everything else        |
//! | `ATUIN_TEST_SCRATCH_URL`      | migrated and configured: rate limit, store wipe |
//! | `ATUIN_TEST_UNCONFIGURED_URL` | no secrets                                      |
//!
//! `ATUIN_TEST_USERNAME` and `ATUIN_TEST_PASSWORD` are the configured secrets (defaults match
//! `scripts/e2e.sh`); `ATUIN_TEST_MAX_RECORD_SIZE` and `ATUIN_TEST_PAGE_SIZE` the vars. A test
//! whose server is not given is skipped with a note on stderr.
//!
//! The main server allows five logins a minute, so only four tests there call `/login`; the rest
//! derive the session token the way the server does.

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::time::Duration;

use atuin_client::api_client;
use atuin_client::record::sqlite_store::SqliteStore;
use atuin_client::record::sync::{ClientSource, Operation, SyncSession};
use atuin_domain::api::LoginRequest;
use atuin_domain::record::{
    DecryptedData, EncryptedData, Host, HostId, Record, RecordIdx, RecordSeriesKey, RecordTag,
    paseto_v4,
};
use atuin_worker::{auth, caps, wire};
use reqwest::StatusCode;
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use uuid::Uuid;

// ---------------------------------------------------------------------------------------------
// Harness

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn username() -> String {
    env_or("ATUIN_TEST_USERNAME", "e2e-user")
}

fn password() -> String {
    env_or("ATUIN_TEST_PASSWORD", "e2e-password-0123456789")
}

fn max_record_size() -> usize {
    env_or("ATUIN_TEST_MAX_RECORD_SIZE", "262144")
        .parse()
        .unwrap()
}

fn page_size() -> u64 {
    env_or("ATUIN_TEST_PAGE_SIZE", "100").parse().unwrap()
}

fn token() -> String {
    auth::derive_token(&username(), &password())
}

/// A server under test, if its variable is set.
fn server(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(url) if !url.is_empty() => Some(url.trim_end_matches('/').to_owned()),
        _ => {
            eprintln!("{var} is not set; skipping");
            None
        }
    }
}

macro_rules! require {
    ($var:literal) => {
        match server($var) {
            Some(url) => url,
            None => return,
        }
    };
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap()
}

fn authed(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    builder.header("authorization", format!("Token {}", token()))
}

async fn json_of(resp: reqwest::Response) -> Value {
    resp.json().await.unwrap()
}

/// Assert a JSON error response, including the protocol headers every response carries.
async fn assert_error(resp: reqwest::Response, status: StatusCode, reason: &str) {
    assert_eq!(resp.status(), status, "{}", resp.url());
    assert_protocol_headers(&resp);
    assert_eq!(json_of(resp).await, json!({ "reason": reason }));
}

fn assert_protocol_headers(resp: &reqwest::Response) {
    assert_eq!(resp.headers()["atuin-version"], "18.24.4", "{}", resp.url());
    assert_eq!(
        resp.headers()["x-clacks-overhead"],
        "GNU Terry Pratchett, Kris Nova"
    );
}

/// A record with a fresh host, as the wire carries it; `data` is not real ciphertext.
fn raw_record(host: &str, tag: &str, idx: u64) -> wire::Record {
    wire::Record {
        id: Uuid::now_v7().to_string(),
        idx,
        host: wire::Host {
            id: host.to_owned(),
        },
        timestamp: 1_730_000_000_000_000_000 + idx,
        version: "v0".to_owned(),
        tag: tag.to_owned(),
        data: wire::EncryptedData {
            data: format!("v4.local.payload-{idx}"),
            cek: "{}".to_owned(),
        },
    }
}

async fn upload(base: &str, records: &[wire::Record]) -> reqwest::Response {
    authed(http().post(format!("{base}/api/v0/record")))
        .json(records)
        .send()
        .await
        .unwrap()
}

async fn status(base: &str) -> wire::RecordStatus {
    let resp = authed(http().get(format!("{base}/api/v0/record")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    resp.json().await.unwrap()
}

async fn page(
    base: &str,
    host: &str,
    tag: &str,
    start: Option<u64>,
    count: u64,
) -> Vec<wire::Record> {
    let mut query = vec![
        ("host", host.to_owned()),
        ("tag", tag.to_owned()),
        ("count", count.to_string()),
    ];
    if let Some(start) = start {
        query.push(("start", start.to_string()));
    }
    let resp = authed(http().get(format!("{base}/api/v0/record/next")))
        .query(&query)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_protocol_headers(&resp);
    resp.json().await.unwrap()
}

fn idxs(records: &[wire::Record]) -> Vec<u64> {
    records.iter().map(|r| r.idx).collect()
}

// ---------------------------------------------------------------------------------------------
// Unchecked routes and the protocol wrapper

#[tokio::test]
async fn healthz_and_capabilities() {
    let base = require!("ATUIN_TEST_SERVER_URL");

    let resp = http().get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_protocol_headers(&resp);
    assert_eq!(json_of(resp).await, json!({"status": "healthy"}));

    let resp = http()
        .get(format!("{base}/api/v0/capabilities"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_protocol_headers(&resp);
    assert_eq!(resp.headers()["content-type"], "application/json");
    assert_eq!(
        resp.text().await.unwrap(),
        caps::Capabilities::new(page_size()).body()
    );
}

#[tokio::test]
async fn index_route() {
    let base = require!("ATUIN_TEST_SERVER_URL");
    let resp = http().get(format!("{base}/")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_protocol_headers(&resp);
    let body = json_of(resp).await;
    assert_eq!(body["version"], "18.24.4");
    assert!(body["homage"].as_str().unwrap().contains("Great A'Tuin"));
}

#[tokio::test]
async fn fallback_is_a_plain_404_with_protocol_headers() {
    let base = require!("ATUIN_TEST_SERVER_URL");
    for (method, path) in [
        ("GET", "/nope"),
        ("GET", "/api/v0/packfiles"),
        ("PUT", "/api/v0/record"),
        ("GET", "/login"),
    ] {
        let resp = http()
            .request(method.parse().unwrap(), format!("{base}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{method} {path}");
        assert_protocol_headers(&resp);
        assert_eq!(resp.text().await.unwrap(), "404 not found");
    }
}

#[tokio::test]
async fn stale_enforced_capabilities_get_412_before_auth() {
    let base = require!("ATUIN_TEST_SERVER_URL");
    let ours = caps::Capabilities::new(page_size());

    let resp = http()
        .get(format!("{base}/api/v0/me"))
        .header("authorization", "Token wrong")
        .header(caps::KNOWN_HEADER, "deadbeefdeadbeef")
        .header(caps::ENFORCE_HEADER, "1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    assert_protocol_headers(&resp);
    assert_eq!(resp.headers()[caps::AVAILABLE_HEADER], ours.token());
    assert_eq!(resp.text().await.unwrap(), "capabilities out of date");

    // Stale without enforcement: served, and told the current token.
    let resp = authed(http().get(format!("{base}/api/v0/me")))
        .header(caps::KNOWN_HEADER, "deadbeefdeadbeef")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[caps::AVAILABLE_HEADER], ours.token());

    // Current: no advertisement.
    let resp = authed(http().get(format!("{base}/api/v0/me")))
        .header(caps::KNOWN_HEADER, ours.token())
        .header(caps::ENFORCE_HEADER, "1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get(caps::AVAILABLE_HEADER).is_none());
}

// ---------------------------------------------------------------------------------------------
// Accounts and auth

#[tokio::test]
async fn refused_routes() {
    let base = require!("ATUIN_TEST_SERVER_URL");

    let resp = http()
        .get(format!("{base}/user/{}", username()))
        .send()
        .await
        .unwrap();
    assert_error(resp, StatusCode::NOT_FOUND, "user not found").await;

    let resp = http()
        .post(format!("{base}/register"))
        .json(&json!({"username": "bob", "email": "b@example.com", "password": "p"}))
        .send()
        .await
        .unwrap();
    assert_error(
        resp,
        StatusCode::BAD_REQUEST,
        "this server is not open for registrations",
    )
    .await;

    let resp = authed(http().patch(format!("{base}/account/password")))
        .json(&json!({"current_password": "a", "new_password": "b"}))
        .send()
        .await
        .unwrap();
    assert_error(
        resp,
        StatusCode::BAD_REQUEST,
        "password is managed by the server operator; change the ATUIN_PASSWORD secret",
    )
    .await;

    let resp = authed(http().delete(format!("{base}/account")))
        .json(&json!({"password": "a"}))
        .send()
        .await
        .unwrap();
    assert_error(
        resp,
        StatusCode::BAD_REQUEST,
        "account is managed by the server operator; to replace the server's data with this \
         machine's, use atuin store push --force",
    )
    .await;

    // Both refused account routes check the token first.
    let resp = http()
        .delete(format!("{base}/account"))
        .send()
        .await
        .unwrap();
    assert_error(
        resp,
        StatusCode::BAD_REQUEST,
        "missing authorization header",
    )
    .await;
}

#[tokio::test]
async fn authorization_errors() {
    let base = require!("ATUIN_TEST_SERVER_URL");
    let url = format!("{base}/api/v0/record");

    let resp = http().get(&url).send().await.unwrap();
    assert_error(
        resp,
        StatusCode::BAD_REQUEST,
        "missing authorization header",
    )
    .await;

    for value in [format!("Bearer {}", token()), "Token".to_owned()] {
        let resp = http()
            .get(&url)
            .header("authorization", value)
            .send()
            .await
            .unwrap();
        assert_error(
            resp,
            StatusCode::BAD_REQUEST,
            "invalid authorization header encoding",
        )
        .await;
    }

    let resp = http()
        .get(&url)
        .header("authorization", "Token not-the-token")
        .send()
        .await
        .unwrap();
    assert_error(resp, StatusCode::FORBIDDEN, "session not found").await;
}

#[tokio::test]
async fn me_returns_the_configured_username() {
    let base = require!("ATUIN_TEST_SERVER_URL");
    let resp = authed(http().get(format!("{base}/api/v0/me")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(json_of(resp).await, json!({"username": username()}));
}

/// Two of the four logins on the main server.
#[tokio::test]
async fn login_failures() {
    let base = require!("ATUIN_TEST_SERVER_URL");

    let resp = http()
        .post(format!("{base}/login"))
        .json(&json!({"username": username(), "password": "not-the-password-at-all"}))
        .send()
        .await
        .unwrap();
    assert_error(resp, StatusCode::UNAUTHORIZED, "password is not correct").await;

    let resp = http()
        .post(format!("{base}/login"))
        .json(&json!({"username": username()}))
        .send()
        .await
        .unwrap();
    assert_error(resp, StatusCode::BAD_REQUEST, "invalid request body").await;
}

// ---------------------------------------------------------------------------------------------
// Records, against real D1

#[tokio::test]
async fn uploads_are_idempotent_and_heads_track_the_max() {
    let base = require!("ATUIN_TEST_SERVER_URL");
    let host = Uuid::now_v7().to_string();
    let all: Vec<wire::Record> = (0..10).map(|i| raw_record(&host, "history", i)).collect();

    assert_eq!(upload(&base, &all[..6]).await.status(), StatusCode::OK);
    // A re-upload of stored records changes nothing, even with different contents.
    let mut altered = all[2..4].to_vec();
    altered[0].data.data = "changed".to_owned();
    assert_eq!(upload(&base, &altered).await.status(), StatusCode::OK);
    // A mixed batch: out of order, duplicates, and a second tag.
    let mut mixed = vec![
        all[9].clone(),
        all[7].clone(),
        all[3].clone(),
        all[9].clone(),
    ];
    mixed.extend((0..3).map(|i| raw_record(&host, "kv", i)));
    mixed.push(all[8].clone());
    mixed.push(all[6].clone());
    let resp = upload(&base, &mixed).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_protocol_headers(&resp);
    assert!(resp.text().await.unwrap().is_empty());
    // An older batch never moves a head back.
    assert_eq!(upload(&base, &all[..2]).await.status(), StatusCode::OK);
    assert_eq!(upload(&base, &[]).await.status(), StatusCode::OK);

    let heads = status(&base).await;
    assert_eq!(heads.hosts[&host]["history"], 9);
    assert_eq!(heads.hosts[&host]["kv"], 2);

    let records = page(&base, &host, "history", None, 100).await;
    assert_eq!(records, all);
}

#[tokio::test]
async fn paging_at_and_past_the_end() {
    let base = require!("ATUIN_TEST_SERVER_URL");
    let host = Uuid::now_v7().to_string();
    let all: Vec<wire::Record> = (0..5).map(|i| raw_record(&host, "history", i)).collect();
    assert_eq!(upload(&base, &all).await.status(), StatusCode::OK);

    assert_eq!(
        idxs(&page(&base, &host, "history", Some(0), 2).await),
        [0, 1]
    );
    assert_eq!(
        idxs(&page(&base, &host, "history", Some(2), 2).await),
        [2, 3]
    );
    assert_eq!(idxs(&page(&base, &host, "history", Some(4), 2).await), [4]);
    assert!(page(&base, &host, "history", Some(5), 2).await.is_empty());
    assert!(page(&base, &host, "history", Some(500), 2).await.is_empty());
    assert!(page(&base, &host, "other-tag", None, 2).await.is_empty());
    assert!(page(&base, &host, "history", None, 0).await.is_empty());
}

#[tokio::test]
async fn count_is_clamped_to_the_page_size() {
    let base = require!("ATUIN_TEST_SERVER_URL");
    let host = Uuid::now_v7().to_string();
    let total = page_size() + 20;
    let all: Vec<wire::Record> = (0..total)
        .map(|i| raw_record(&host, "history", i))
        .collect();
    for chunk in all.chunks(page_size() as usize) {
        assert_eq!(upload(&base, chunk).await.status(), StatusCode::OK);
    }

    let first = page(&base, &host, "history", None, total * 10).await;
    assert_eq!(first.len() as u64, page_size());
    let rest = page(&base, &host, "history", Some(page_size()), total).await;
    assert_eq!(rest.len(), 20);
}

#[tokio::test]
async fn next_requires_its_query() {
    let base = require!("ATUIN_TEST_SERVER_URL");
    let resp = authed(http().get(format!("{base}/api/v0/record/next?host=h&tag=t")))
        .send()
        .await
        .unwrap();
    assert_error(
        resp,
        StatusCode::BAD_REQUEST,
        "invalid query string: missing count",
    )
    .await;
}

#[tokio::test]
async fn timestamps_above_2_pow_53_survive_the_javascript_boundary() {
    let base = require!("ATUIN_TEST_SERVER_URL");
    let host = Uuid::now_v7().to_string();
    let stamps = [(1 << 53) + 1, 1_730_000_000_123_456_789, i64::MAX as u64, 0];
    let records: Vec<wire::Record> = stamps
        .iter()
        .enumerate()
        .map(|(i, &t)| wire::Record {
            timestamp: t,
            ..raw_record(&host, "history", i as u64)
        })
        .collect();
    assert_eq!(upload(&base, &records).await.status(), StatusCode::OK);

    let back = page(&base, &host, "history", None, 10).await;
    assert_eq!(back.iter().map(|r| r.timestamp).collect::<Vec<_>>(), stamps);
}

#[tokio::test]
async fn max_record_size_measures_data_only() {
    let base = require!("ATUIN_TEST_SERVER_URL");
    let host = Uuid::now_v7().to_string();
    let max = max_record_size();

    let mut fits = raw_record(&host, "history", 0);
    fits.data.data = "x".repeat(max);
    fits.data.cek = "k".repeat(4096); // the key does not count
    assert_eq!(
        upload(&base, &[fits.clone()]).await.status(),
        StatusCode::OK
    );

    let mut too_big = raw_record(&host, "history", 1);
    too_big.data.data = "x".repeat(max + 1);
    // One oversized record rejects the whole batch.
    let resp = upload(&base, &[raw_record(&host, "history", 2), too_big]).await;
    assert_error(
        resp,
        StatusCode::BAD_REQUEST,
        "could not add records; record too large",
    )
    .await;

    let stored = page(&base, &host, "history", None, 10).await;
    assert_eq!(stored, vec![fits]);
}

#[tokio::test]
async fn malformed_upload_is_rejected() {
    let base = require!("ATUIN_TEST_SERVER_URL");
    let resp = authed(http().post(format!("{base}/api/v0/record")))
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_error(resp, StatusCode::BAD_REQUEST, "invalid request body").await;
}

// ---------------------------------------------------------------------------------------------
// Scratch server: things that would disturb the other tests

#[tokio::test]
async fn delete_store_wipes_everything() {
    let base = require!("ATUIN_TEST_SCRATCH_URL");
    let host = Uuid::now_v7().to_string();
    let records: Vec<wire::Record> = (0..3).map(|i| raw_record(&host, "history", i)).collect();
    assert_eq!(upload(&base, &records).await.status(), StatusCode::OK);
    assert!(!status(&base).await.hosts.is_empty());

    let resp = authed(http().delete(format!("{base}/api/v0/store")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_protocol_headers(&resp);

    assert!(status(&base).await.hosts.is_empty());
    assert!(page(&base, &host, "history", None, 10).await.is_empty());

    // The same records upload again after a wipe.
    assert_eq!(upload(&base, &records).await.status(), StatusCode::OK);
    assert_eq!(status(&base).await.hosts[&host]["history"], 2);
}

#[tokio::test]
async fn sixth_login_in_a_minute_is_rate_limited() {
    let base = require!("ATUIN_TEST_SCRATCH_URL");
    let mut statuses = Vec::new();
    for _ in 0..6 {
        let resp = http()
            .post(format!("{base}/login"))
            .json(&json!({"username": username(), "password": password()}))
            .send()
            .await
            .unwrap();
        statuses.push(resp.status());
        if resp.status() == StatusCode::TOO_MANY_REQUESTS {
            assert_error(
                resp,
                StatusCode::TOO_MANY_REQUESTS,
                "too many login attempts",
            )
            .await;
        }
    }
    assert_eq!(statuses[..5], [StatusCode::OK; 5]);
    assert_eq!(statuses[5], StatusCode::TOO_MANY_REQUESTS);

    // The client turns the 429 into its rate-limit message; the body is not read first.
    let err = api_client::login(
        &base.parse().unwrap(),
        LoginRequest {
            username: username(),
            password: password().into(),
            totp_code: None,
        },
        &HashMap::new(),
    )
    .await
    .expect_err("the limit still applies");
    assert!(err.to_string().contains("Rate limited"), "{err}");

    // Syncing is not limited.
    let resp = authed(http().get(format!("{base}/api/v0/me")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------------------------
// Broken deployments

#[tokio::test]
async fn missing_secrets_are_a_clear_500() {
    let base = require!("ATUIN_TEST_UNCONFIGURED_URL");
    let resp = http()
        .post(format!("{base}/login"))
        .json(&json!({"username": "a", "password": "b"}))
        .send()
        .await
        .unwrap();
    assert_error(
        resp,
        StatusCode::INTERNAL_SERVER_ERROR,
        "server not configured: ATUIN_USERNAME is not set",
    )
    .await;

    // The unchecked routes still answer.
    let resp = http().get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = http()
        .get(format!("{base}/api/v0/capabilities"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // And the client shows the reason.
    let err = api_client::login(
        &base.parse().unwrap(),
        LoginRequest {
            username: "a".to_owned(),
            password: "b".to_owned().into(),
            totp_code: None,
        },
        &HashMap::new(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("server not configured"), "{err}");
}

// ---------------------------------------------------------------------------------------------
// The real client, adapted from upstream `crates/atuin-server/tests/sync.rs`

fn key() -> paseto_v4::Key {
    paseto_v4::Key::from([7u8; 32])
}

fn client_with(base: &str, session: String) -> api_client::Client {
    let address: reqwest::Url = base.parse().unwrap();
    api_client::Client::new(
        address.clone(),
        &api_client::AuthToken::Token(session.into()),
        Duration::from_secs(5),
        Duration::from_secs(30),
        &HashMap::new(),
        api_client::caps_client_anonymous(&address, &HashMap::new()).unwrap(),
    )
    .unwrap()
}

fn client(base: &str) -> api_client::Client {
    client_with(base, token())
}

fn record(host: HostId, tag: &RecordTag, idx: RecordIdx) -> Record<EncryptedData> {
    Record::builder()
        .host(Host::new(host))
        .version("v0".to_owned().into())
        .tag(tag.clone())
        .idx(idx)
        .data(DecryptedData(b"some data".to_vec()))
        .build()
        .encrypt(&key())
}

/// Every test shares one account, so a sync only acts on the test's own series.
fn only(series: &RecordSeriesKey, operations: Vec<Operation>) -> Vec<Operation> {
    operations
        .into_iter()
        .filter(|op| match op {
            Operation::Upload { series: s, .. }
            | Operation::Download { series: s, .. }
            | Operation::Noop { series: s } => s == series,
        })
        .collect()
}

async fn session(client: api_client::Client, store: SqliteStore, page_size: u64) -> SyncSession {
    SyncSession::builder()
        .store(store)
        .client_source(ClientSource::FromClient(client))
        .build()
        .connect()
        .await
        .unwrap()
        .with_page_size(NonZeroU64::new(page_size).unwrap())
}

/// Remote holds `0..=remote_max`, local `0..=local_max` (or nothing); sync. Returns how many
/// records were downloaded and the local head afterwards.
async fn download(
    base: &str,
    remote_max: RecordIdx,
    local_max: Option<RecordIdx>,
    page_size: u64,
) -> (usize, RecordIdx) {
    let client = client(base);
    let series = RecordSeriesKey::new(
        HostId(Uuid::now_v7()),
        RecordTag::Other(Uuid::now_v7().simple().to_string()),
    );
    let records: Vec<_> = (0..=remote_max)
        .map(|idx| record(series.host_id, &series.tag, idx))
        .collect();
    client.post_records(&records).await.unwrap();

    let store = SqliteStore::in_memory(Duration::from_secs(2))
        .await
        .unwrap();
    if let Some(local_max) = local_max {
        store
            .push_batch(records.iter().take(local_max as usize + 1))
            .await
            .unwrap();
    }

    let session = session(client, store.clone(), page_size).await;
    let (diff, _) = session.diff().await.unwrap();
    let operations = only(&series, SyncSession::operations(diff).unwrap());
    let (_, downloaded) = session.keyed(&key()).sync_remote(operations).await.unwrap();

    let local = store.status().await.unwrap();
    (
        downloaded.len(),
        *local.hosts[&series.host_id].get(&series.tag).unwrap(),
    )
}

/// Local holds `0..=local_max`, remote `0..=remote_max` (or nothing); sync. Returns how many
/// records were uploaded and the remote head afterwards.
async fn upload_sync(
    base: &str,
    local_max: RecordIdx,
    remote_max: Option<RecordIdx>,
    page_size: u64,
) -> (u64, RecordIdx) {
    let client = client(base);
    let series = RecordSeriesKey::new(
        HostId(Uuid::now_v7()),
        RecordTag::Other(Uuid::now_v7().simple().to_string()),
    );
    let records: Vec<_> = (0..=local_max)
        .map(|idx| record(series.host_id, &series.tag, idx))
        .collect();

    let store = SqliteStore::in_memory(Duration::from_secs(2))
        .await
        .unwrap();
    store.push_batch(records.iter()).await.unwrap();
    if let Some(remote_max) = remote_max {
        client
            .post_records(&records[..=remote_max as usize])
            .await
            .unwrap();
    }

    let session = session(client, store, page_size).await;
    let (diff, _) = session.diff().await.unwrap();
    let operations = only(&series, SyncSession::operations(diff).unwrap());
    let (uploaded, _) = session.keyed(&key()).sync_remote(operations).await.unwrap();

    let remote = session.record_status().await.unwrap();
    (
        uploaded,
        *remote.hosts[&series.host_id].get(&series.tag).unwrap(),
    )
}

#[tokio::test]
async fn client_downloads_exactly_the_missing_records() {
    let base = require!("ATUIN_TEST_SERVER_URL");
    for (remote_max, local_max, page_size, expected) in [
        (5, Some(2), 100, 3),
        (5, None, 100, 6),
        (4, Some(0), 4, 4),
        (4, None, 4, 5),
        (0, None, 100, 1),
        (9, Some(1), 4, 8),
        (6, Some(0), 4, 6),
    ] {
        let (downloaded, local) = download(&base, remote_max, local_max, page_size).await;
        assert_eq!(local, remote_max, "local store is missing records");
        assert_eq!(
            downloaded, expected,
            "{remote_max} {local_max:?} {page_size}"
        );
    }
}

#[tokio::test]
async fn client_uploads_exactly_the_missing_records() {
    let base = require!("ATUIN_TEST_SERVER_URL");
    for (local_max, remote_max, page_size, expected) in [
        (5, Some(2), 100, 3),
        (5, None, 100, 6),
        (4, Some(0), 4, 4),
        (4, None, 4, 5),
        (0, None, 100, 1),
        (9, Some(1), 4, 8),
        (6, Some(0), 4, 6),
    ] {
        let (uploaded, remote) = upload_sync(&base, local_max, remote_max, page_size).await;
        assert_eq!(remote, local_max, "remote is missing records");
        assert_eq!(uploaded, expected, "{local_max} {remote_max:?} {page_size}");
    }
}

/// The other two logins on the main server: the real client logs in, then two machines
/// converge through the server with the page size the server advertises.
#[tokio::test]
async fn client_login_then_two_machines_converge() {
    let base = require!("ATUIN_TEST_SERVER_URL");
    let address: reqwest::Url = base.parse().unwrap();

    let wrong = api_client::login(
        &address,
        LoginRequest {
            username: username(),
            password: "definitely-not-the-password".to_owned().into(),
            totp_code: None,
        },
        &HashMap::new(),
    )
    .await
    .unwrap_err();
    assert!(
        wrong.to_string().contains("password is not correct"),
        "{wrong}"
    );

    let login = api_client::login(
        &address,
        LoginRequest {
            username: username(),
            password: password().into(),
            totp_code: None,
        },
        &HashMap::new(),
    )
    .await
    .unwrap();
    assert_eq!(login.session.expose_secret(), token());
    assert_eq!(login.auth.as_deref(), Some("cli"));

    let client = client_with(&base, login.session.expose_secret().to_owned());
    assert_eq!(client.me().await.unwrap().username, username());

    // Machine A uploads its history; machine B, empty, downloads it. The server's advertised
    // page size applies (no override), and 250 records span three pages.
    let series = RecordSeriesKey::new(HostId(Uuid::now_v7()), RecordTag::History);
    let records: Vec<_> = (0..250)
        .map(|idx| record(series.host_id, &series.tag, idx))
        .collect();

    let store_a = SqliteStore::in_memory(Duration::from_secs(2))
        .await
        .unwrap();
    store_a.push_batch(records.iter()).await.unwrap();
    let a = SyncSession::builder()
        .store(store_a)
        .client_source(ClientSource::FromClient(client.clone()))
        .build()
        .connect()
        .await
        .unwrap();
    let (diff, _) = a.diff().await.unwrap();
    let (uploaded, _) = a
        .keyed(&key())
        .sync_remote(only(&series, SyncSession::operations(diff).unwrap()))
        .await
        .unwrap();
    assert_eq!(uploaded, 250);

    // A second sync uploads nothing.
    let (diff, _) = a.diff().await.unwrap();
    let (uploaded, _) = a
        .keyed(&key())
        .sync_remote(only(&series, SyncSession::operations(diff).unwrap()))
        .await
        .unwrap();
    assert_eq!(uploaded, 0);

    let store_b = SqliteStore::in_memory(Duration::from_secs(2))
        .await
        .unwrap();
    let b = SyncSession::builder()
        .store(store_b.clone())
        .client_source(ClientSource::FromClient(client_with(&base, token())))
        .build()
        .connect()
        .await
        .unwrap();
    let (diff, _) = b.diff().await.unwrap();
    let (_, downloaded) = b
        .keyed(&key())
        .sync_remote(only(&series, SyncSession::operations(diff).unwrap()))
        .await
        .unwrap();
    assert_eq!(downloaded.len(), 250);

    let local_b = store_b.status().await.unwrap();
    assert_eq!(local_b.hosts[&series.host_id][&series.tag], 249);
    let mut ids: Vec<_> = records.iter().map(|r| r.id).collect();
    let mut got = downloaded;
    ids.sort();
    got.sort();
    assert_eq!(got, ids);
}

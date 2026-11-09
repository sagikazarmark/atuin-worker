//! The Workers glue: the fetch event, the route `match`, and the D1 and rate-limit calls.
//!
//! Everything protocol-shaped is delegated to the native modules; what is here is what needs a
//! Cloudflare binding. Tested end to end against `wrangler dev --local` (`tests/e2e.rs`).

use std::sync::OnceLock;

use bytes::Bytes;
use http::request::Parts;
use http::{Response, StatusCode};
use http_body_util::{BodyExt, Full};
use worker::wasm_bindgen::JsValue;
use worker::{Context, D1Database, D1PreparedStatement, Date, Env, HttpRequest, event};

use crate::auth::Credentials;
use crate::caps::Capabilities;
use crate::proto::{self, Negotiation, Route, Settings, reason};
use crate::sql::{self, HeadRow, NextParams, Param, StoreRow};
use crate::wire::Record;

/// The rate limiter key for `/login`: a constant, because there is one account to protect.
const LOGIN_LIMIT_KEY: &str = "atuin-worker/login";

/// What is computed once per isolate: the vars, the capability document, and the config check
/// with the derived token.
struct Isolate {
    settings: Settings,
    caps: Capabilities,
    credentials: Result<Credentials, String>,
}

static ISOLATE: OnceLock<Isolate> = OnceLock::new();

impl Isolate {
    fn load(env: &Env) -> Self {
        let settings = Settings::from_vars(
            env.var("PAGE_SIZE").ok().map(|v| v.to_string()).as_deref(),
            env.var("MAX_RECORD_SIZE")
                .ok()
                .map(|v| v.to_string())
                .as_deref(),
        );
        let credentials = Credentials::from_secrets(
            env.secret("ATUIN_USERNAME")
                .ok()
                .map(|s| s.to_string())
                .as_deref(),
            env.secret("ATUIN_PASSWORD")
                .ok()
                .map(|s| s.to_string())
                .as_deref(),
        );
        // Bad vars fail the config check too; the capability document then falls back to the
        // default page size.
        let (settings, credentials) = match settings {
            Ok(settings) => (settings, credentials),
            Err(problem) => (Settings::default(), Err(problem)),
        };
        Self {
            caps: Capabilities::new(settings.page_size),
            settings,
            credentials,
        }
    }
}

/// What one request did, for the log line.
#[derive(Default)]
struct Outcome {
    records: Option<usize>,
    rows_written: Option<usize>,
}

#[event(fetch)]
async fn fetch(req: HttpRequest, env: Env, _ctx: Context) -> worker::Result<Response<Full<Bytes>>> {
    let started = Date::now().as_millis();
    let isolate = ISOLATE.get_or_init(|| Isolate::load(&env));

    let (parts, body) = req.into_parts();
    let route = Route::recognize(&parts.method, parts.uri.path());
    let mut outcome = Outcome::default();
    let mut negotiation = Negotiation::Current;

    // Every body is JSON that has to be parsed whole anyway.
    let mut resp = match body.collect().await {
        Ok(body) => {
            handle(
                route,
                &parts,
                body.to_bytes(),
                &env,
                isolate,
                &mut negotiation,
                &mut outcome,
            )
            .await
        }
        Err(_) => proto::error(StatusCode::BAD_REQUEST, reason::INVALID_BODY),
    };

    proto::finish(&mut resp, negotiation, &isolate.caps);

    // One structured line per request. Never the Authorization header, the token, a body, or a
    // submitted credential; the route is its pattern, not the raw path.
    let line = serde_json::json!({
        "route": route.pattern(),
        "status": resp.status().as_u16(),
        "records": outcome.records,
        "rows_written": outcome.rows_written,
        "duration_ms": Date::now().as_millis().saturating_sub(started),
    });
    worker::console_log!("{line}");

    Ok(resp.map(Full::new))
}

/// The request path: unchecked routes, config check, capability negotiation, token check, route.
async fn handle(
    route: Route,
    parts: &Parts,
    body: Bytes,
    env: &Env,
    isolate: &Isolate,
    negotiation: &mut Negotiation,
    outcome: &mut Outcome,
) -> Response<Bytes> {
    match route {
        Route::Healthz => return proto::healthz(),
        Route::Capabilities => return proto::capabilities(&isolate.caps),
        Route::NotFound => return proto::not_found(),
        _ => {}
    }

    let credentials = match &isolate.credentials {
        Ok(credentials) => credentials,
        Err(problem) => return proto::not_configured(problem),
    };

    *negotiation = proto::negotiate(&parts.headers, isolate.caps.token());
    if *negotiation == Negotiation::Rejected {
        return proto::precondition_failed();
    }

    if route.needs_token()
        && let Err(rejection) = credentials.authorize(&parts.headers)
    {
        return rejection.into_response();
    }

    match route {
        Route::Index => proto::index(),
        Route::GetUser => proto::get_user(),
        Route::Register => proto::register(),
        Route::Login => login(env, credentials, &body).await,
        Route::Me => proto::me(credentials.username()),
        Route::ChangePassword => proto::change_password(),
        Route::DeleteAccount => proto::delete_account(),
        Route::AddRecords | Route::RecordStatus | Route::NextRecords | Route::DeleteStore => {
            let Ok(db) = env.d1("DB") else {
                return proto::error(StatusCode::INTERNAL_SERVER_ERROR, reason::DB_MISSING);
            };
            let settings = &isolate.settings;
            match route {
                Route::AddRecords => add_records(&db, settings, &body, outcome).await,
                Route::RecordStatus => record_status(&db).await,
                Route::NextRecords => next_records(&db, settings, parts.uri.query(), outcome).await,
                _ => delete_store(&db, outcome).await,
            }
        }
        Route::Healthz | Route::Capabilities | Route::NotFound => {
            unreachable!("answered above")
        }
    }
}

/// `POST /login`: the rate limit first, then the body, then the credentials.
async fn login(env: &Env, credentials: &Credentials, body: &[u8]) -> Response<Bytes> {
    let Ok(limiter) = env.rate_limiter("LOGIN_LIMIT") else {
        return proto::error(StatusCode::INTERNAL_SERVER_ERROR, reason::LIMITER_MISSING);
    };
    match limiter.limit(LOGIN_LIMIT_KEY.to_owned()).await {
        Ok(outcome) if outcome.success => credentials.login(body),
        Ok(_) => proto::error(StatusCode::TOO_MANY_REQUESTS, reason::TOO_MANY_LOGINS),
        Err(e) => {
            worker::console_error!("login rate limit failed: {e}");
            proto::error(StatusCode::INTERNAL_SERVER_ERROR, reason::RATE_LIMIT_FAILED)
        }
    }
}

/// `POST /api/v0/record`: every record and every series head in one atomic batch.
async fn add_records(
    db: &D1Database,
    settings: &Settings,
    body: &[u8],
    outcome: &mut Outcome,
) -> Response<Bytes> {
    let Ok(records) = serde_json::from_slice::<Vec<Record>>(body) else {
        return proto::error(StatusCode::BAD_REQUEST, reason::INVALID_BODY);
    };
    outcome.records = Some(records.len());

    if sql::oversized(&records, settings.max_record_size).is_some() {
        return proto::error(StatusCode::BAD_REQUEST, reason::RECORD_TOO_LARGE);
    }
    if records.is_empty() {
        return proto::empty_ok();
    }

    let insert = db.prepare(sql::INSERT_RECORD);
    let upsert = db.prepare(sql::UPSERT_HEAD);
    let statements: worker::Result<Vec<_>> = records
        .iter()
        .map(|r| bind(&insert, &sql::insert_record(r)))
        .chain(sql::batch_heads(&records).iter().map(|h| bind(&upsert, h)))
        .collect();

    let result = match statements {
        Ok(statements) => db.batch(statements).await,
        Err(e) => Err(e),
    };
    match result {
        Ok(results) => {
            outcome.rows_written = rows_written(&results);
            proto::empty_ok()
        }
        Err(e) => {
            worker::console_error!("upload failed: {e}");
            proto::error(StatusCode::INTERNAL_SERVER_ERROR, reason::ADD_RECORD_FAILED)
        }
    }
}

/// `GET /api/v0/record`: the head of every series.
async fn record_status(db: &D1Database) -> Response<Bytes> {
    let rows = match db.prepare(sql::STATUS).all().await {
        Ok(result) => result.results::<HeadRow>(),
        Err(e) => Err(e),
    };
    match rows {
        Ok(rows) => proto::json(StatusCode::OK, &sql::status(rows)),
        Err(e) => {
            worker::console_error!("status failed: {e}");
            proto::error(StatusCode::INTERNAL_SERVER_ERROR, reason::INDEX_FAILED)
        }
    }
}

/// `GET /api/v0/record/next`: one page of one series.
async fn next_records(
    db: &D1Database,
    settings: &Settings,
    query: Option<&str>,
    outcome: &mut Outcome,
) -> Response<Bytes> {
    let params = match NextParams::parse(query, settings.page_size) {
        Ok(params) => params,
        Err(problem) => return proto::error(StatusCode::BAD_REQUEST, problem),
    };

    let rows = match bind(&db.prepare(sql::NEXT_RECORDS), &params.params()) {
        Ok(statement) => match statement.all().await {
            Ok(result) => result.results::<StoreRow>(),
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    };
    let records: Result<Vec<Record>, String> = match rows {
        Ok(rows) => rows.into_iter().map(StoreRow::into_record).collect(),
        Err(e) => Err(e.to_string()),
    };

    match records {
        Ok(records) => {
            outcome.records = Some(records.len());
            proto::json(StatusCode::OK, &records)
        }
        Err(e) => {
            worker::console_error!("next page failed: {e}");
            proto::error(StatusCode::INTERNAL_SERVER_ERROR, reason::INDEX_FAILED)
        }
    }
}

/// `DELETE /api/v0/store`: both tables, one batch.
async fn delete_store(db: &D1Database, outcome: &mut Outcome) -> Response<Bytes> {
    let statements = vec![
        db.prepare(sql::DELETE_RECORDS),
        db.prepare(sql::DELETE_HEADS),
    ];
    match db.batch(statements).await {
        Ok(results) => {
            outcome.rows_written = rows_written(&results);
            proto::empty_ok()
        }
        Err(e) => {
            worker::console_error!("delete store failed: {e}");
            proto::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                reason::DELETE_STORE_FAILED,
            )
        }
    }
}

/// Bind parameters to a copy of a prepared statement. Text crosses as a JavaScript string,
/// integers as a JavaScript number (only used for values far below 2^53).
fn bind(
    statement: &D1PreparedStatement,
    params: &[Param<'_>],
) -> worker::Result<D1PreparedStatement> {
    let values: Vec<JsValue> = params
        .iter()
        .map(|p| match p {
            Param::Text(s) => JsValue::from_str(s),
            Param::Int(n) => JsValue::from_f64(*n as f64),
        })
        .collect();
    statement.clone().bind(&values)
}

/// The rows a batch wrote, from D1's `meta.rows_written`, for the Free-plan budget.
fn rows_written(results: &[worker::D1Result]) -> Option<usize> {
    results
        .iter()
        .map(|r| r.meta().ok().flatten().and_then(|m| m.rows_written))
        .sum()
}

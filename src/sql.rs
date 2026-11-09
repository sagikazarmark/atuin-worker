//! The SQL the Worker runs on D1, and the pure helpers around it.
//!
//! The statements are plain constants so a unit test can `prepare()` every one of them against
//! `migrations/0001_init.sql` in an in-memory SQLite. Parameters are described by [`Param`] and
//! converted to JavaScript values only in `worker.rs`.
//!
//! Nanosecond timestamps exceed 2^53, the largest integer a JavaScript number holds exactly, and
//! D1 parameters and results cross into JavaScript. So `timestamp` is bound as a decimal string
//! and `CAST` to `INTEGER` in SQL, and read back `CAST(… AS TEXT)` and parsed here.

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde::Deserialize;

use crate::wire::{EncryptedData, Host, Record, RecordStatus};

/// Upload, per record. A record that is already stored is left alone, so uploads are idempotent.
pub const INSERT_RECORD: &str = "INSERT INTO store VALUES (?1, ?2, ?3, ?4, CAST(?5 AS INTEGER), ?6, ?7, ?8) ON CONFLICT DO NOTHING";

/// Upload, per series in the batch: move the head forward, never back.
pub const UPSERT_HEAD: &str = "INSERT INTO store_head VALUES (?1, ?2, ?3) ON CONFLICT (host, tag) DO UPDATE SET idx = MAX(idx, excluded.idx)";

/// `GET /api/v0/record`: one row per series.
pub const STATUS: &str = "SELECT host, tag, idx FROM store_head";

/// `GET /api/v0/record/next`: an index range scan over one series.
pub const NEXT_RECORDS: &str = "SELECT client_id, host, idx, CAST(timestamp AS TEXT) AS timestamp, version, tag, data, cek FROM store WHERE host = ?1 AND tag = ?2 AND idx >= ?3 ORDER BY idx LIMIT ?4";

/// `DELETE /api/v0/store`, run as one batch with [`DELETE_HEADS`].
pub const DELETE_RECORDS: &str = "DELETE FROM store";
/// See [`DELETE_RECORDS`].
pub const DELETE_HEADS: &str = "DELETE FROM store_head";

/// Every statement, for the `prepare()` check.
pub const ALL: [&str; 6] = [
    INSERT_RECORD,
    UPSERT_HEAD,
    STATUS,
    NEXT_RECORDS,
    DELETE_RECORDS,
    DELETE_HEADS,
];

/// A bound parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Param<'a> {
    Text(Cow<'a, str>),
    /// Bound as a JavaScript number: only for values far below 2^53 (indexes and counts).
    Int(u64),
}

/// The parameters of [`INSERT_RECORD`] for one record, in column order.
pub fn insert_record(record: &Record) -> [Param<'_>; 8] {
    [
        Param::Text(Cow::Borrowed(&record.host.id)),
        Param::Text(Cow::Borrowed(&record.tag)),
        Param::Int(record.idx),
        Param::Text(Cow::Borrowed(&record.id)),
        Param::Text(Cow::Owned(record.timestamp.to_string())),
        Param::Text(Cow::Borrowed(&record.version)),
        Param::Text(Cow::Borrowed(&record.data.data)),
        Param::Text(Cow::Borrowed(&record.data.cek)),
    ]
}

/// The highest `idx` per (host, tag) in an upload batch, sorted by series: the rows for
/// [`UPSERT_HEAD`].
pub fn batch_heads(records: &[Record]) -> Vec<[Param<'_>; 3]> {
    let mut heads: BTreeMap<(&str, &str), u64> = BTreeMap::new();
    for r in records {
        let head = heads.entry((&r.host.id, &r.tag)).or_insert(r.idx);
        *head = (*head).max(r.idx);
    }
    heads
        .into_iter()
        .map(|((host, tag), idx)| {
            [
                Param::Text(Cow::Borrowed(host)),
                Param::Text(Cow::Borrowed(tag)),
                Param::Int(idx),
            ]
        })
        .collect()
}

/// The first record whose `data` is longer than `max` bytes, if any. As upstream, only the
/// encrypted payload counts, not the key or the other fields.
pub fn oversized(records: &[Record], max: u64) -> Option<&Record> {
    records.iter().find(|r| r.data.data.len() as u64 > max)
}

/// A row of [`STATUS`].
#[derive(Debug, Deserialize)]
pub struct HeadRow {
    pub host: String,
    pub tag: String,
    pub idx: u64,
}

/// Build the `RecordStatus` response from the rows of [`STATUS`].
pub fn status(rows: impl IntoIterator<Item = HeadRow>) -> RecordStatus {
    let mut status = RecordStatus::default();
    for row in rows {
        status.set(row.host, row.tag, row.idx);
    }
    status
}

/// A row of [`NEXT_RECORDS`], with `timestamp` as text.
#[derive(Debug, Deserialize)]
pub struct StoreRow {
    pub client_id: String,
    pub host: String,
    pub idx: u64,
    pub timestamp: String,
    pub version: String,
    pub tag: String,
    pub data: String,
    pub cek: String,
}

impl StoreRow {
    /// Decode the row into a wire record, parsing the textual timestamp exactly.
    pub fn into_record(self) -> Result<Record, String> {
        let timestamp = self
            .timestamp
            .parse::<u64>()
            .map_err(|_| format!("stored timestamp {:?} is not a u64", self.timestamp))?;
        Ok(Record {
            id: self.client_id,
            idx: self.idx,
            host: Host { id: self.host },
            timestamp,
            version: self.version,
            tag: self.tag,
            data: EncryptedData {
                data: self.data,
                cek: self.cek,
            },
        })
    }
}

/// The query of `GET /api/v0/record/next`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextParams {
    pub host: String,
    pub tag: String,
    /// The first `idx` wanted; 0 when absent.
    pub start: u64,
    /// Clamped to the page size, so a page always fits D1's result limit.
    pub count: u64,
}

impl NextParams {
    /// Parse `?host&tag&start&count` (form-urlencoded). `host`, `tag` and `count` are required
    /// as upstream; the error is a reason for a 400.
    pub fn parse(query: Option<&str>, page_size: u64) -> Result<Self, String> {
        let (mut host, mut tag, mut start, mut count) = (None, None, None, None);
        for pair in query.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let value = form_decode(value);
            match form_decode(key).as_str() {
                "host" => host = host.or(Some(value)),
                "tag" => tag = tag.or(Some(value)),
                "start" => start = start.or(Some(value)),
                "count" => count = count.or(Some(value)),
                _ => {}
            }
        }

        let missing = |name: &str| format!("invalid query string: missing {name}");
        let number = |name: &str, value: String| {
            value
                .parse::<u64>()
                .map_err(|_| format!("invalid query string: {name} must be a non-negative integer"))
        };

        Ok(Self {
            host: host.ok_or_else(|| missing("host"))?,
            tag: tag.ok_or_else(|| missing("tag"))?,
            start: start.map(|s| number("start", s)).transpose()?.unwrap_or(0),
            count: number("count", count.ok_or_else(|| missing("count"))?)?.min(page_size),
        })
    }

    /// The parameters of [`NEXT_RECORDS`].
    pub fn params(&self) -> [Param<'_>; 4] {
        [
            Param::Text(Cow::Borrowed(&self.host)),
            Param::Text(Cow::Borrowed(&self.tag)),
            Param::Int(self.start),
            Param::Int(self.count),
        ]
    }
}

/// Decode one `application/x-www-form-urlencoded` component: `+` is a space, `%XX` a byte, and a
/// malformed escape stays as it is.
fn form_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => match (bytes.get(i + 1), bytes.get(i + 2)) {
                (Some(&h), Some(&l)) if h.is_ascii_hexdigit() && l.is_ascii_hexdigit() => {
                    out.push(hex(h) << 4 | hex(l));
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        _ => b - b'A' + 10,
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, params_from_iter};

    use super::*;

    fn record(host: &str, tag: &str, idx: u64) -> Record {
        Record {
            id: format!("id-{host}-{tag}-{idx}"),
            idx,
            host: Host { id: host.into() },
            timestamp: (1 << 53) + idx,
            version: "v0".into(),
            tag: tag.into(),
            data: EncryptedData {
                data: format!("data-{idx}"),
                cek: format!("cek-{idx}"),
            },
        }
    }

    /// An in-memory database with the migrations applied.
    fn database() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(include_str!("../migrations/0001_init.sql"))
            .unwrap();
        db
    }

    fn to_sql(params: &[Param<'_>]) -> Vec<rusqlite::types::Value> {
        params
            .iter()
            .map(|p| match p {
                Param::Text(s) => rusqlite::types::Value::Text(s.to_string()),
                Param::Int(n) => rusqlite::types::Value::Integer(*n as i64),
            })
            .collect()
    }

    fn upload(db: &Connection, records: &[Record]) {
        for r in records {
            db.execute(INSERT_RECORD, params_from_iter(to_sql(&insert_record(r))))
                .unwrap();
        }
        for head in batch_heads(records) {
            db.execute(UPSERT_HEAD, params_from_iter(to_sql(&head)))
                .unwrap();
        }
    }

    fn heads(db: &Connection) -> RecordStatus {
        let mut stmt = db.prepare(STATUS).unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok(HeadRow {
                    host: r.get(0)?,
                    tag: r.get(1)?,
                    idx: r.get::<_, i64>(2)? as u64,
                })
            })
            .unwrap()
            .map(Result::unwrap);
        status(rows)
    }

    fn next(db: &Connection, params: &NextParams) -> Vec<Record> {
        let mut stmt = db.prepare(NEXT_RECORDS).unwrap();
        stmt.query_map(params_from_iter(to_sql(&params.params())), |r| {
            Ok(StoreRow {
                client_id: r.get("client_id")?,
                host: r.get("host")?,
                idx: r.get::<_, i64>("idx")? as u64,
                timestamp: r.get("timestamp")?,
                version: r.get("version")?,
                tag: r.get("tag")?,
                data: r.get("data")?,
                cek: r.get("cek")?,
            })
        })
        .unwrap()
        .map(|row| row.unwrap().into_record().unwrap())
        .collect()
    }

    #[test]
    fn every_statement_prepares_against_the_migrations() {
        let db = database();
        for sql in ALL {
            db.prepare(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
    }

    #[test]
    fn batch_heads_takes_the_max_per_series() {
        let records = [
            record("h1", "history", 3),
            record("h1", "history", 7),
            record("h1", "history", 5),
            record("h1", "kv", 0),
            record("h2", "history", 2),
            record("h1", "history", 7),
        ];
        assert_eq!(
            batch_heads(&records),
            vec![
                [
                    Param::Text("h1".into()),
                    Param::Text("history".into()),
                    Param::Int(7)
                ],
                [
                    Param::Text("h1".into()),
                    Param::Text("kv".into()),
                    Param::Int(0)
                ],
                [
                    Param::Text("h2".into()),
                    Param::Text("history".into()),
                    Param::Int(2)
                ],
            ]
        );
        assert!(batch_heads(&[]).is_empty());
    }

    #[test]
    fn insert_binds_the_timestamp_as_text() {
        let r = record("h", "t", 1);
        let params = insert_record(&r);
        assert_eq!(params[4], Param::Text("9007199254740993".into()));
        assert_eq!(params[2], Param::Int(1));
    }

    #[test]
    fn uploads_are_idempotent_and_heads_never_move_back() {
        let db = database();
        let all: Vec<Record> = (0..10).map(|i| record("h", "history", i)).collect();
        upload(&db, &all[..6]);
        upload(&db, &all[2..4]);
        upload(&db, &all);
        upload(&db, &all[..1]);

        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM store", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 10);
        assert_eq!(heads(&db).hosts["h"]["history"], 9);
    }

    #[test]
    fn rows_decode_with_timestamps_above_2_pow_53() {
        let db = database();
        let mut r = record("h", "history", 0);
        r.timestamp = u64::from(u32::MAX) << 31 | 1; // ~9.2e18, still within i64
        upload(&db, std::slice::from_ref(&r));
        let page = next(
            &db,
            &NextParams::parse(Some("host=h&tag=history&count=10"), 100).unwrap(),
        );
        assert_eq!(page, vec![r]);
    }

    #[test]
    fn row_decoding_rejects_a_bad_timestamp() {
        let row = StoreRow {
            client_id: "i".into(),
            host: "h".into(),
            idx: 0,
            timestamp: "1.5e18".into(),
            version: "v0".into(),
            tag: "t".into(),
            data: "d".into(),
            cek: "c".into(),
        };
        assert!(row.into_record().is_err());
    }

    #[test]
    fn paging_at_and_past_the_end() {
        let db = database();
        let all: Vec<Record> = (0..5).map(|i| record("h", "history", i)).collect();
        upload(&db, &all);
        upload(&db, &[record("h", "kv", 0), record("other", "history", 0)]);

        let page = |q: &str| {
            next(&db, &NextParams::parse(Some(q), 2).unwrap())
                .into_iter()
                .map(|r| r.idx)
                .collect::<Vec<_>>()
        };
        assert_eq!(page("host=h&tag=history&count=2"), vec![0, 1]);
        assert_eq!(page("host=h&tag=history&start=2&count=2"), vec![2, 3]);
        assert_eq!(page("host=h&tag=history&start=4&count=2"), vec![4]);
        assert_eq!(
            page("host=h&tag=history&start=5&count=2"),
            Vec::<u64>::new()
        );
        assert_eq!(page("host=h&tag=history&start=0&count=100"), vec![0, 1]);
    }

    #[test]
    fn delete_empties_both_tables() {
        let db = database();
        upload(&db, &[record("h", "history", 0)]);
        db.execute(DELETE_RECORDS, []).unwrap();
        db.execute(DELETE_HEADS, []).unwrap();
        assert!(heads(&db).hosts.is_empty());
    }

    #[test]
    fn next_params_default_start_and_clamp_count() {
        let p = NextParams::parse(Some("host=h&tag=history&count=500"), 100).unwrap();
        assert_eq!(
            p,
            NextParams {
                host: "h".into(),
                tag: "history".into(),
                start: 0,
                count: 100
            }
        );
        let p = NextParams::parse(Some("count=5&start=7&tag=kv&host=h&extra=1"), 100).unwrap();
        assert_eq!((p.start, p.count), (7, 5));
    }

    #[test]
    fn next_params_decode_form_encoding() {
        let p = NextParams::parse(Some("host=a%2Db&tag=my+tag%21&count=1"), 100).unwrap();
        assert_eq!((p.host.as_str(), p.tag.as_str()), ("a-b", "my tag!"));
        let p = NextParams::parse(Some("host=%zz%4&tag=%&count=1"), 100).unwrap();
        assert_eq!((p.host.as_str(), p.tag.as_str()), ("%zz%4", "%"));
    }

    #[test]
    fn next_params_errors() {
        for (query, reason) in [
            (None, "invalid query string: missing host"),
            (Some("tag=t&count=1"), "invalid query string: missing host"),
            (Some("host=h&count=1"), "invalid query string: missing tag"),
            (Some("host=h&tag=t"), "invalid query string: missing count"),
            (
                Some("host=h&tag=t&count=-1"),
                "invalid query string: count must be a non-negative integer",
            ),
            (
                Some("host=h&tag=t&count=1&start=x"),
                "invalid query string: start must be a non-negative integer",
            ),
        ] {
            assert_eq!(
                NextParams::parse(query, 100).unwrap_err(),
                reason,
                "{query:?}"
            );
        }
    }

    #[test]
    fn oversized_measures_data_only() {
        let mut r = record("h", "t", 0);
        r.data.data = "x".repeat(10);
        r.data.cek = "y".repeat(1000);
        assert!(oversized(std::slice::from_ref(&r), 10).is_none());
        r.data.data.push('x');
        assert!(oversized(std::slice::from_ref(&r), 10).is_some());
    }
}

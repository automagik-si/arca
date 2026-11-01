//! The node's own store, `arca.db` in the state directory: the host metric points and the alert event log in one SQLite file.
//!
//! The file is in WAL mode with `synchronous=FULL`, so every commit is on disk when it returns, as a journal append is. Every
//! connection reads its pragmas back and refuses to open unless the journal mode is `wal` and `synchronous` is 2. The sampler
//! thread is the only writer; every other reader opens [`Store::open_read_only`]. An error text names what failed, never the
//! content of a row.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, Transaction, params};
use serde_json::{Value, json};

/// The store's file name in the state directory.
pub const DB_FILE: &str = "arca.db";
/// The schema version, kept in `PRAGMA user_version`. Migrations only go forward.
pub const SCHEMA_VERSION: i32 = 1;
/// The `schema` of every alert event the store gives out.
pub const ALERT_SCHEMA: &str = "arca.alert.v1";

/// How long a connection waits on a lock another connection holds.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// The tables of schema version 1. `points` is keyed `(tier, at, series_id)`, so each tick appends at the end of the b-tree.
/// `seq` is `AUTOINCREMENT`, so a sequence number is never reused, and `meta` mirrors the highest one.
const SCHEMA: &str = "
CREATE TABLE series (id INTEGER PRIMARY KEY, name TEXT NOT NULL, labels TEXT NOT NULL, kind TEXT NOT NULL,
    UNIQUE (name, labels));
CREATE TABLE points (tier INTEGER NOT NULL, at INTEGER NOT NULL, series_id INTEGER NOT NULL REFERENCES series (id),
    avg REAL NOT NULL, min REAL NOT NULL, max REAL NOT NULL, PRIMARY KEY (tier, at, series_id)) WITHOUT ROWID;
CREATE TABLE alert_events (seq INTEGER PRIMARY KEY AUTOINCREMENT, alert_id INTEGER NOT NULL, rule TEXT NOT NULL,
    severity TEXT NOT NULL, state TEXT NOT NULL, labels TEXT NOT NULL, value REAL, threshold REAL,
    started_at INTEGER NOT NULL, resolved_at INTEGER, reason TEXT, at INTEGER NOT NULL);
CREATE INDEX alert_events_alert ON alert_events (alert_id);
CREATE TABLE meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
";

const EVENT_COLUMNS: &str = "seq, alert_id, rule, severity, state, labels, value, threshold, started_at, resolved_at, reason, at";

/// An open store. The writer opens it with [`Store::open`], and every reader with [`Store::open_read_only`].
pub struct Store {
    conn: Connection,
    path: PathBuf,
}

/// A store just opened for writing, with the pragmas it read back.
pub struct Opened {
    pub store: Store,
    /// The `.corrupt-<ms>` suffix the files of a corrupt store were renamed with. Never set yet: a corrupt file is refused.
    pub reset: Option<String>,
    pub pragmas: Pragmas,
}

/// The pragmas a connection reads back, named as SQLite names them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pragmas {
    pub sqlite_version: String,
    pub journal_mode: String,
    pub synchronous: i64,
    pub auto_vacuum: i64,
}

/// Why the store refused or failed. The text never carries a row's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// A pragma read back a value the store does not run with.
    Refused(String),
    Sqlite(String),
    Io(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(why) => write!(f, "the store refused to open: {why}"),
            Self::Sqlite(why) => write!(f, "the store failed: {why}"),
            Self::Io(why) => write!(f, "the store's files failed: {why}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeriesKind {
    Gauge,
    Counter,
}

impl SeriesKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gauge => "gauge",
            Self::Counter => "counter",
        }
    }
}

/// One reading of one series at a tick. The node is not a label: the store holds one node.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub kind: SeriesKind,
    pub value: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventState {
    Firing,
    Resolved,
}

impl EventState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Firing => "firing",
            Self::Resolved => "resolved",
        }
    }
}

/// Why an alert resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveReason {
    Cleared,
    RuleDisabled,
    SubjectGone,
}

impl ResolveReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cleared => "cleared",
            Self::RuleDisabled => "rule-disabled",
            Self::SubjectGone => "subject-gone",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        [Self::Cleared, Self::RuleDisabled, Self::SubjectGone]
            .into_iter()
            .find(|reason| reason.as_str() == text)
    }
}

/// One alert transition. A firing event's `alert_id` is its own `seq`, and its resolve is a new event with the same
/// `alert_id`. Both are `None` until the event is committed. Times are epoch milliseconds.
#[derive(Debug, Clone, PartialEq)]
pub struct AlertEvent {
    pub seq: Option<u64>,
    pub alert_id: Option<u64>,
    pub rule: String,
    pub severity: String,
    pub state: EventState,
    pub labels: BTreeMap<String, String>,
    pub value: Option<f64>,
    pub threshold: Option<f64>,
    pub started_at: u64,
    pub resolved_at: Option<u64>,
    pub reason: Option<ResolveReason>,
    pub at: u64,
}

impl AlertEvent {
    /// The `arca.alert.v1` object of the event on `node`. Only a resolve carries `reason`.
    pub fn to_json(&self, node: &str) -> Value {
        let mut object = json!({
            "schema": ALERT_SCHEMA,
            "seq": self.seq,
            "alertId": self.alert_id,
            "node": node,
            "rule": self.rule,
            "severity": self.severity,
            "state": self.state.as_str(),
            "labels": self.labels,
            "value": self.value,
            "threshold": self.threshold,
            "startedAt": self.started_at,
            "resolvedAt": self.resolved_at,
            "at": self.at,
        });
        if let (EventState::Resolved, Some(reason)) = (self.state, self.reason) {
            object["reason"] = json!(reason.as_str());
        }
        object
    }
}

/// What a rollup did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RollupReport {
    pub minutes_rolled: u64,
    pub hours_rolled: u64,
    pub deleted_points: u64,
    pub deleted_events: u64,
}

impl Store {
    /// Opens `<state_dir>/arca.db` for writing, creating it at schema version 1 when it is new. `auto_vacuum` is set before
    /// any table exists, then the WAL and durability pragmas, which are read back. A file whose quick check fails is refused.
    pub fn open(state_dir: &Path, _now_ms: u64) -> Result<Opened, StoreError> {
        let path = state_dir.join(DB_FILE);
        let mut conn = Connection::open(&path)?;
        conn.pragma_update(None, "auto_vacuum", 2)?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        conn.pragma_update(None, "synchronous", 2)?;
        conn.pragma_update(None, "wal_autocheckpoint", 1_000)?;
        conn.pragma_update(None, "journal_size_limit", 67_108_864)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let pragmas = Store::check_pragmas(&conn)?;
        let check: String = conn.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if check != "ok" {
            return Err(StoreError::Sqlite(
                "the database failed its quick check".to_string(),
            ));
        }
        migrate(&mut conn)?;
        Ok(Opened {
            store: Store { conn, path },
            reset: None,
            pragmas,
        })
    }

    /// Opens `<state_dir>/arca.db` to read only. The pragmas are read back as for the writer.
    pub fn open_read_only(state_dir: &Path) -> Result<Store, StoreError> {
        let path = state_dir.join(DB_FILE);
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.pragma_update(None, "synchronous", 2)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        Store::check_pragmas(&conn)?;
        Ok(Store { conn, path })
    }

    /// Reads the pragmas of `conn` back, and refuses a connection that is not in WAL mode with `synchronous` 2 (FULL).
    pub fn check_pragmas(conn: &Connection) -> Result<Pragmas, StoreError> {
        let pragmas = Pragmas {
            sqlite_version: conn.query_row("SELECT sqlite_version()", [], |row| row.get(0))?,
            journal_mode: conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?,
            synchronous: conn.pragma_query_value(None, "synchronous", |row| row.get(0))?,
            auto_vacuum: conn.pragma_query_value(None, "auto_vacuum", |row| row.get(0))?,
        };
        if pragmas.synchronous != 2 {
            return Err(StoreError::Refused(format!(
                "synchronous reads {}, not 2 (FULL)",
                pragmas.synchronous
            )));
        }
        if !pragmas.journal_mode.eq_ignore_ascii_case("wal") {
            return Err(StoreError::Refused(format!(
                "journal_mode reads {}, not wal",
                pragmas.journal_mode
            )));
        }
        Ok(pragmas)
    }

    /// Commits one tick in one transaction: the tier-0 points, then the events in order, then the highest `seq`. It returns
    /// the events' sequence numbers. A firing event's `alert_id` is its own `seq`. A resolve without an `alert_id` takes the
    /// `seq` of the newest firing event with the same rule and labels in the batch. A point whose value is not finite is
    /// left out.
    pub fn commit_tick(
        &mut self,
        at: u64,
        points: &[Point],
        events: &[AlertEvent],
    ) -> Result<Vec<u64>, StoreError> {
        let tx = self.conn.transaction()?;
        for point in points.iter().filter(|point| point.value.is_finite()) {
            let series = series_id(&tx, point)?;
            tx.prepare_cached(
                "INSERT OR REPLACE INTO points (tier, at, series_id, avg, min, max) VALUES (0, ?1, ?2, ?3, ?3, ?3)",
            )?
            .execute(params![int(at)?, series, point.value])?;
        }
        let mut seqs: Vec<u64> = Vec::with_capacity(events.len());
        for (index, event) in events.iter().enumerate() {
            let labels = labels_text(&event.labels)?;
            let alert_id = match (event.state, event.alert_id) {
                (EventState::Firing, _) => None,
                (EventState::Resolved, Some(id)) => Some(id),
                (EventState::Resolved, None) => Some(linked_firing(events, &seqs, index)?),
            };
            tx.prepare_cached(
                "INSERT INTO alert_events (alert_id, rule, severity, state, labels, value, threshold, started_at,
                 resolved_at, reason, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?
            .execute(params![
                int(alert_id.unwrap_or(0))?,
                event.rule,
                event.severity,
                event.state.as_str(),
                labels,
                event.value.filter(|v| v.is_finite()),
                event.threshold.filter(|v| v.is_finite()),
                int(event.started_at)?,
                event.resolved_at.map(int).transpose()?,
                event.reason.map(ResolveReason::as_str),
                int(event.at)?,
            ])?;
            let seq = u64::try_from(tx.last_insert_rowid())
                .map_err(|_| StoreError::Sqlite("a sequence number is negative".to_string()))?;
            if alert_id.is_none() {
                tx.prepare_cached("UPDATE alert_events SET alert_id = ?1 WHERE seq = ?1")?
                    .execute(params![int(seq)?])?;
            }
            seqs.push(seq);
        }
        if let Some(max) = seqs.last() {
            tx.prepare_cached(
                "INSERT INTO meta (key, value) VALUES ('max_seq', ?1)
                 ON CONFLICT (key) DO UPDATE SET value = max(value, excluded.value)",
            )?
            .execute(params![int(*max)?])?;
        }
        tx.commit()?;
        Ok(seqs)
    }

    /// Every firing event that has no resolve, oldest first.
    pub fn firing(&self) -> Result<Vec<AlertEvent>, StoreError> {
        self.events(
            &format!(
                "SELECT {EVENT_COLUMNS} FROM alert_events f WHERE state = 'firing' AND NOT EXISTS
                 (SELECT 1 FROM alert_events r WHERE r.alert_id = f.alert_id AND r.state = 'resolved') ORDER BY seq"
            ),
            params![],
        )
    }

    /// At most `limit` events with a `seq` above `after`, oldest first.
    pub fn events_after(&self, after: u64, limit: usize) -> Result<Vec<AlertEvent>, StoreError> {
        self.events(
            &format!(
                "SELECT {EVENT_COLUMNS} FROM alert_events WHERE seq > ?1 ORDER BY seq LIMIT ?2"
            ),
            params![int(after)?, count(limit)],
        )
    }

    /// At most `limit` resolves at or after `since_ms`, newest first.
    pub fn resolved_since(
        &self,
        since_ms: u64,
        limit: usize,
    ) -> Result<Vec<AlertEvent>, StoreError> {
        self.events(
            &format!(
                "SELECT {EVENT_COLUMNS} FROM alert_events WHERE state = 'resolved' AND at >= ?1
                 ORDER BY seq DESC LIMIT ?2"
            ),
            params![int(since_ms)?, count(limit)],
        )
    }

    /// The bytes of the database file and its WAL.
    pub fn bytes(&self) -> u64 {
        let size = |path: &Path| fs::metadata(path).map_or(0, |meta| meta.len());
        let mut wal = self.path.clone().into_os_string();
        wal.push("-wal");
        size(&self.path) + size(Path::new(&wal))
    }

    /// Rolls the closed buckets up and applies retention. Not built yet: it rolls nothing.
    pub fn rollup(&mut self, _now_ms: u64) -> Result<RollupReport, StoreError> {
        Ok(RollupReport::default())
    }

    fn events(
        &self,
        sql: &str,
        args: impl rusqlite::Params,
    ) -> Result<Vec<AlertEvent>, StoreError> {
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map(args, event_of)?;
        rows.map(|row| row.map_err(StoreError::from)).collect()
    }
}

/// Creates schema version 1 on a new file. A file of a newer version is refused.
fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let version: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        0 => {
            let tx = conn.transaction()?;
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
            Ok(())
        }
        SCHEMA_VERSION => Ok(()),
        newer => Err(StoreError::Refused(format!(
            "the schema version is {newer}, newer than {SCHEMA_VERSION}"
        ))),
    }
}

/// The id of the point's series, which is created on its first point.
fn series_id(tx: &Transaction<'_>, point: &Point) -> Result<i64, StoreError> {
    let labels = labels_text(&point.labels)?;
    let found: Option<i64> = tx
        .prepare_cached("SELECT id FROM series WHERE name = ?1 AND labels = ?2")?
        .query_row(params![point.name, labels], |row| row.get(0))
        .optional()?;
    if let Some(id) = found {
        return Ok(id);
    }
    tx.prepare_cached("INSERT INTO series (name, labels, kind) VALUES (?1, ?2, ?3)")?
        .execute(params![point.name, labels, point.kind.as_str()])?;
    Ok(tx.last_insert_rowid())
}

/// The `seq` a resolve without an `alert_id` links to: the newest firing event of the same rule and labels earlier in the
/// batch.
fn linked_firing(events: &[AlertEvent], seqs: &[u64], index: usize) -> Result<u64, StoreError> {
    let resolve = &events[index];
    (0..index)
        .rev()
        .find(|&i| {
            let event = &events[i];
            event.state == EventState::Firing
                && event.rule == resolve.rule
                && event.labels == resolve.labels
        })
        .map(|i| seqs[i])
        .ok_or_else(|| StoreError::Sqlite("a resolve event has no firing alert".to_string()))
}

/// Labels as stored: a JSON object with its keys sorted, so one label set is always one text.
fn labels_text(labels: &BTreeMap<String, String>) -> Result<String, StoreError> {
    serde_json::to_string(labels)
        .map_err(|_| StoreError::Sqlite("the labels do not serialize".to_string()))
}

fn event_of(row: &Row<'_>) -> rusqlite::Result<AlertEvent> {
    let state: String = row.get(4)?;
    let labels: String = row.get(5)?;
    let reason: Option<String> = row.get(10)?;
    let bad = |column: usize| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            "an unknown value".into(),
        )
    };
    Ok(AlertEvent {
        seq: Some(uint(row, 0)?),
        alert_id: Some(uint(row, 1)?),
        rule: row.get(2)?,
        severity: row.get(3)?,
        state: match state.as_str() {
            "firing" => EventState::Firing,
            "resolved" => EventState::Resolved,
            _ => return Err(bad(4)),
        },
        labels: serde_json::from_str(&labels).map_err(|_| bad(5))?,
        value: row.get(6)?,
        threshold: row.get(7)?,
        started_at: uint(row, 8)?,
        resolved_at: match row.get::<_, Option<i64>>(9)? {
            None => None,
            Some(_) => Some(uint(row, 9)?),
        },
        reason: match reason {
            None => None,
            Some(text) => Some(ResolveReason::parse(&text).ok_or_else(|| bad(10))?),
        },
        at: uint(row, 11)?,
    })
}

/// A time or a sequence number as SQLite stores it. Neither ever reaches `i64::MAX`.
fn int(value: u64) -> rusqlite::Result<i64> {
    i64::try_from(value).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

/// A row limit as SQLite takes it.
fn count(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

/// A stored time or sequence number, which is never negative.
fn uint(row: &Row<'_>, column: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(column)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(column, value))
}

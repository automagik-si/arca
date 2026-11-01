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

use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, Row, Transaction, params};
use serde_json::{Value, json};

mod rollup;

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
    /// The `.corrupt-<ms>` suffix that `arca.db`, `arca.db-wal` and `arca.db-shm` (each one that existed) were renamed with
    /// when the file was corrupt. `None` when the file opened as it was.
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

/// What a rollup did: the rows it wrote to each tier, and the rows and events its retention deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RollupReport {
    /// Minute buckets written to tier 1.
    pub minutes_rolled: u64,
    /// Hour buckets written to tier 2.
    pub hours_rolled: u64,
    /// Points deleted from any tier.
    pub deleted_points: u64,
    /// Alert events deleted: resolved alerts past their retention, with their firing events.
    pub deleted_events: u64,
}

/// A tier of the store: the raw points, the one-minute rollup or the one-hour rollup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Raw = 0,
    Minute = 1,
    Hour = 2,
}

/// One stored point of one series. A raw point has `avg == min == max`. A counter's rollup holds its last value in all three.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredPoint {
    pub at: u64,
    pub labels: BTreeMap<String, String>,
    pub avg: f64,
    pub min: f64,
    pub max: f64,
}

/// A failed writer open: a file that is corrupt, which is renamed aside, or any other failure, which is reported as it is.
enum Failure {
    Corrupt,
    Other(StoreError),
}

impl From<rusqlite::Error> for Failure {
    fn from(error: rusqlite::Error) -> Self {
        match error {
            rusqlite::Error::SqliteFailure(failure, _)
                if matches!(
                    failure.code,
                    ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt
                ) =>
            {
                Failure::Corrupt
            }
            other => Failure::Other(other.into()),
        }
    }
}

impl Store {
    /// Opens `<state_dir>/arca.db` for writing, creating it at schema version 1 when it is new. `auto_vacuum` is set before
    /// any table exists, then the WAL and durability pragmas, which are read back. A file that is not a database, or fails
    /// its quick check, is renamed aside with its `-wal` and `-shm` and replaced by a new file whose first `seq` is at least
    /// `now_ms`. A file of a newer schema, or a connection with the wrong pragmas, is refused and never renamed.
    pub fn open(state_dir: &Path, now_ms: u64) -> Result<Opened, StoreError> {
        let path = state_dir.join(DB_FILE);
        if is_corrupt(&path) {
            return reset_corrupt(state_dir, path, now_ms);
        }
        match open_writer(&path) {
            Ok((conn, pragmas)) => Ok(Opened {
                store: Store { conn, path },
                reset: None,
                pragmas,
            }),
            Err(Failure::Corrupt) => reset_corrupt(state_dir, path, now_ms),
            Err(Failure::Other(error)) => Err(error),
        }
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

    /// The points of `tier` for the series named `name` with `from_ms <= at < to_ms`, oldest first, then by labels.
    pub fn points(
        &self,
        tier: Tier,
        name: &str,
        from_ms: u64,
        to_ms: u64,
    ) -> Result<Vec<StoredPoint>, StoreError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT p.at, s.labels, p.avg, p.min, p.max FROM points p JOIN series s ON s.id = p.series_id
             WHERE p.tier = ?1 AND s.name = ?2 AND p.at >= ?3 AND p.at < ?4 ORDER BY p.at, s.labels",
        )?;
        let rows = stmt.query_map(
            params![tier as i64, name, int(from_ms)?, int(to_ms)?],
            |row| {
                let labels: String = row.get(1)?;
                let labels = serde_json::from_str(&labels).map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        "an unknown value".into(),
                    )
                })?;
                Ok(StoredPoint {
                    at: uint(row, 0)?,
                    labels,
                    avg: row.get(2)?,
                    min: row.get(3)?,
                    max: row.get(4)?,
                })
            },
        )?;
        rows.map(|row| row.map_err(StoreError::from)).collect()
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

/// Opens the writer's connection to `path` with its pragmas, checks it and migrates it. A file that is not a database, or
/// fails its quick check, is `Failure::Corrupt`.
fn open_writer(path: &Path) -> Result<(Connection, Pragmas), Failure> {
    let mut conn = Connection::open(path)?;
    conn.pragma_update(None, "auto_vacuum", 2)?;
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
    conn.pragma_update(None, "synchronous", 2)?;
    conn.pragma_update(None, "wal_autocheckpoint", 1_000)?;
    conn.pragma_update(None, "journal_size_limit", 67_108_864)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "foreign_keys", true)?;
    let pragmas = Store::check_pragmas(&conn).map_err(Failure::Other)?;
    let check: String = conn.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if check != "ok" {
        return Err(Failure::Corrupt);
    }
    migrate(&mut conn).map_err(Failure::Other)?;
    Ok((conn, pragmas))
}

/// Whether the file at `path` is corrupt, as a read-only connection judges it. A writable connection that closes on a
/// corrupt file deletes its `-wal` and `-shm`, so the probe must be read-only: it never deletes or checkpoints them on close.
/// A file that cannot be probed is not judged here, and the writer open reports the failure.
fn is_corrupt(path: &Path) -> bool {
    let Ok(conn) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return false;
    };
    match conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0)) {
        Ok(check) => check != "ok",
        Err(error) => matches!(Failure::from(error), Failure::Corrupt),
    }
}

/// Renames the files of a corrupt store aside with one `.corrupt-<ms>` suffix, then creates a new file. Its first `seq` is the
/// larger of the old highest `seq` plus one and `now_ms`, so a reader's cursor never sees a `seq` go backwards.
fn reset_corrupt(state_dir: &Path, path: PathBuf, now_ms: u64) -> Result<Opened, StoreError> {
    let old_max = old_max_seq(&path);
    let suffix = format!(".corrupt-{now_ms}");
    for name in [
        DB_FILE.to_string(),
        format!("{DB_FILE}-wal"),
        format!("{DB_FILE}-shm"),
    ] {
        let from = state_dir.join(&name);
        if from.exists() {
            fs::rename(&from, state_dir.join(format!("{name}{suffix}")))
                .map_err(|error| StoreError::Io(error.to_string()))?;
        }
    }
    let (conn, pragmas) = open_writer(&path).map_err(|failure| match failure {
        Failure::Other(error) => error,
        Failure::Corrupt => StoreError::Sqlite("a new store failed its quick check".to_string()),
    })?;
    let start = (old_max + 1).max(now_ms);
    conn.execute(
        "INSERT INTO sqlite_sequence (name, seq) VALUES ('alert_events', ?1)",
        params![int(start - 1)?],
    )?;
    Ok(Opened {
        store: Store { conn, path },
        reset: Some(suffix),
        pragmas,
    })
}

/// The highest sequence number that a corrupt file still answers for, or 0 when it answers for none.
fn old_max_seq(path: &Path) -> u64 {
    let Ok(conn) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return 0;
    };
    let meta: Option<i64> = conn
        .query_row("SELECT value FROM meta WHERE key = 'max_seq'", [], |row| {
            row.get(0)
        })
        .optional()
        .ok()
        .flatten();
    let events: Option<i64> = conn
        .query_row("SELECT max(seq) FROM alert_events", [], |row| row.get(0))
        .ok()
        .flatten();
    [meta, events]
        .into_iter()
        .flatten()
        .filter_map(|value| u64::try_from(value).ok())
        .max()
        .unwrap_or(0)
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

//! Rollups and retention. The raw points (tier 0) roll into one-minute buckets (tier 1), and the minute buckets roll into
//! one-hour buckets (tier 2). A rollup runs in one transaction: it rolls every closed bucket after the mark it last reached,
//! moves the marks, and only then deletes the rows that are past their retention. A bucket with no data stays a gap.

use rusqlite::{CachedStatement, OptionalExtension, Transaction, params};

use crate::{RollupReport, Store, StoreError, Tier, int, uint};

const MINUTE_MS: u64 = 60_000;
const HOUR_MS: u64 = 3_600_000;
const DAY_MS: u64 = 24 * HOUR_MS;
/// How far back each tier keeps its rows, measured from the rollup's `now`. A row exactly this old is kept.
const RAW_KEEP_MS: u64 = HOUR_MS;
const MINUTE_KEEP_MS: u64 = 7 * DAY_MS;
const HOUR_KEEP_MS: u64 = 90 * DAY_MS;
/// How long a resolved alert is kept after its resolve. A firing alert is never deleted.
const EVENT_KEEP_MS: u64 = 90 * DAY_MS;
/// The `meta` keys of the two marks. A mark is the start of the first bucket that has not been rolled yet.
const MINUTE_MARK: &str = "rolled_minute";
const HOUR_MARK: &str = "rolled_hour";

/// One stored value to roll: a raw point, or a minute bucket. A counter carries its last value in `avg`, and its `min` and
/// `max` equal it.
#[derive(Clone, Copy)]
struct Reading {
    counter: bool,
    avg: f64,
    min: f64,
    max: f64,
}

/// The bucket of one series that is still filling.
struct Bucket {
    series: i64,
    start: u64,
    counter: bool,
    sum: f64,
    count: u64,
    min: f64,
    max: f64,
    last: f64,
}

impl Bucket {
    fn open(series: i64, start: u64, first: Reading) -> Bucket {
        let mut bucket = Bucket {
            series,
            start,
            counter: first.counter,
            sum: 0.0,
            count: 0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            last: first.avg,
        };
        bucket.add(first);
        bucket
    }

    fn add(&mut self, reading: Reading) {
        self.sum += reading.avg;
        self.count += 1;
        self.min = self.min.min(reading.min);
        self.max = self.max.max(reading.max);
        self.last = reading.avg;
    }

    fn flush(&self, write: &mut CachedStatement<'_>, tier: Tier) -> Result<(), StoreError> {
        let (avg, min, max) = if self.counter {
            (self.last, self.last, self.last)
        } else {
            (self.sum / self.count as f64, self.min, self.max)
        };
        write.execute(params![
            tier as i64,
            int(self.start)?,
            self.series,
            avg,
            min,
            max
        ])?;
        Ok(())
    }
}

impl Store {
    /// Rolls every closed minute after `rolled_minute` into tier 1 and every closed hour after `rolled_hour` into tier 2, moves
    /// both marks, and then deletes the rows that the retention has expired. A bucket is closed when it ends at or before
    /// `now_ms`. When an hour closes, the WAL is checkpointed with `TRUNCATE` and the free pages are returned.
    ///
    /// A gauge's minute is the mean of its raw values, with their min and max. A counter's minute is its last value. An hour
    /// is built from the minutes: a gauge's average is the mean of the minute averages, which is the mean of the raw values
    /// when every minute is full, and a counter's hour is its last minute's value.
    pub fn rollup(&mut self, now_ms: u64) -> Result<RollupReport, StoreError> {
        let minute_end = now_ms - now_ms % MINUTE_MS;
        let hour_end = now_ms - now_ms % HOUR_MS;
        let mut report = RollupReport::default();
        let tx = self.conn.transaction()?;

        let minute_from = match mark(&tx, MINUTE_MARK)? {
            Some(from) => from,
            None => first_at(&tx, Tier::Raw)?
                .map_or(minute_end, |at| (at - at % MINUTE_MS).min(minute_end)),
        };
        report.minutes_rolled = roll(
            &tx,
            Tier::Raw,
            Tier::Minute,
            minute_from,
            minute_end,
            MINUTE_MS,
        )?;
        set_mark(&tx, MINUTE_MARK, minute_end.max(minute_from))?;

        let hour_from = match mark(&tx, HOUR_MARK)? {
            Some(from) => from,
            None => first_at(&tx, Tier::Minute)?
                .map_or(hour_end, |at| (at - at % HOUR_MS).min(hour_end)),
        };
        report.hours_rolled = roll(&tx, Tier::Minute, Tier::Hour, hour_from, hour_end, HOUR_MS)?;
        set_mark(&tx, HOUR_MARK, hour_end.max(hour_from))?;
        let hour_closed = hour_from < hour_end;

        report.deleted_points += delete_points(&tx, Tier::Raw, now_ms, RAW_KEEP_MS)?;
        report.deleted_points += delete_points(&tx, Tier::Minute, now_ms, MINUTE_KEEP_MS)?;
        report.deleted_points += delete_points(&tx, Tier::Hour, now_ms, HOUR_KEEP_MS)?;
        report.deleted_events = delete_events(&tx, now_ms)?;
        tx.commit()?;

        if hour_closed {
            self.conn
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
            self.conn.execute_batch("PRAGMA incremental_vacuum")?;
        }
        Ok(report)
    }
}

/// Rolls the `source` rows with `from <= at < to` into `bucket_ms` buckets, one row per series and bucket, in `target`. Returns
/// the number of rows written.
fn roll(
    tx: &Transaction<'_>,
    source: Tier,
    target: Tier,
    from: u64,
    to: u64,
    bucket_ms: u64,
) -> Result<u64, StoreError> {
    if from >= to {
        return Ok(0);
    }
    let mut read = tx.prepare_cached(
        "SELECT p.series_id, p.at, s.kind, p.avg, p.min, p.max FROM points p JOIN series s ON s.id = p.series_id
         WHERE p.tier = ?1 AND p.at >= ?2 AND p.at < ?3 ORDER BY p.series_id, p.at",
    )?;
    let mut write = tx.prepare_cached(
        "INSERT OR REPLACE INTO points (tier, at, series_id, avg, min, max) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    let mut rows = read.query(params![source as i64, int(from)?, int(to)?])?;
    let mut open: Option<Bucket> = None;
    let mut written = 0;
    while let Some(row) = rows.next()? {
        let series: i64 = row.get(0)?;
        let at = uint(row, 1)?;
        let start = at - at % bucket_ms;
        let kind: String = row.get(2)?;
        let reading = Reading {
            counter: kind == "counter",
            avg: row.get(3)?,
            min: row.get(4)?,
            max: row.get(5)?,
        };
        match open.as_mut() {
            Some(bucket) if bucket.series == series && bucket.start == start => bucket.add(reading),
            _ => {
                if let Some(done) = open.take() {
                    done.flush(&mut write, target)?;
                    written += 1;
                }
                open = Some(Bucket::open(series, start, reading));
            }
        }
    }
    if let Some(done) = open.take() {
        done.flush(&mut write, target)?;
        written += 1;
    }
    Ok(written)
}

/// Deletes the rows of `tier` older than `keep_ms` before `now_ms`. Returns the number of rows deleted.
fn delete_points(
    tx: &Transaction<'_>,
    tier: Tier,
    now_ms: u64,
    keep_ms: u64,
) -> Result<u64, StoreError> {
    let cutoff = now_ms.saturating_sub(keep_ms);
    let deleted = tx
        .prepare_cached("DELETE FROM points WHERE tier = ?1 AND at < ?2")?
        .execute(params![tier as i64, int(cutoff)?])?;
    Ok(deleted as u64)
}

/// Deletes resolved alerts whose resolve is older than the event retention, with their firing events. An alert that is still
/// firing is never deleted. Returns the number of events deleted.
fn delete_events(tx: &Transaction<'_>, now_ms: u64) -> Result<u64, StoreError> {
    let cutoff = now_ms.saturating_sub(EVENT_KEEP_MS);
    let deleted = tx
        .prepare_cached(
            "DELETE FROM alert_events WHERE alert_id IN
                (SELECT alert_id FROM alert_events WHERE state = 'resolved' AND at < ?1)
             AND alert_id NOT IN
                (SELECT f.alert_id FROM alert_events f WHERE f.state = 'firing' AND NOT EXISTS
                    (SELECT 1 FROM alert_events r WHERE r.alert_id = f.alert_id AND r.state = 'resolved'))",
        )?
        .execute(params![int(cutoff)?])?;
    Ok(deleted as u64)
}

fn mark(tx: &Transaction<'_>, key: &str) -> Result<Option<u64>, StoreError> {
    let value: Option<i64> = tx
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?;
    value.map(stored_time).transpose()
}

fn set_mark(tx: &Transaction<'_>, key: &str, value: u64) -> Result<(), StoreError> {
    tx.prepare_cached(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )?
    .execute(params![key, int(value)?])?;
    Ok(())
}

/// The oldest `at` of `tier`, or `None` when the tier is empty.
fn first_at(tx: &Transaction<'_>, tier: Tier) -> Result<Option<u64>, StoreError> {
    let at: Option<i64> = tx.query_row(
        "SELECT min(at) FROM points WHERE tier = ?1",
        params![tier as i64],
        |row| row.get(0),
    )?;
    at.map(stored_time).transpose()
}

fn stored_time(value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::Sqlite("a stored time is negative".to_string()))
}

//! The raw tier: the 5-second points, held in memory and never written to the file. Each series keeps its newest
//! [`RAW_SLOTS`] points, one hour at the 5-second tick, and the rollup drops every point older than an hour once it has
//! rolled its minute. A point is 16 bytes, so a full series holds 11.25 KiB of points beside its name and labels: about
//! 1.4 MiB for the 125 series of the measured one-host node. A series whose last point is older than an hour is dropped.
//!
//! The ring starts empty when the store opens. A read of a raw window the ring cannot answer in full says so in words,
//! never with zeros: the samples before the store opened are gone, and only their minute and hour rollups are in the file.

use std::collections::{BTreeMap, VecDeque};

use rusqlite::Connection;

use crate::{Point, SeriesKind, StoreError, StoredPoint, labels_text, uint};

/// How many points each series keeps: one hour at the 5-second tick.
pub const RAW_SLOTS: usize = 720;
/// How long the raw points are kept, measured from the rollup's `now`. A point exactly this old is kept.
pub(crate) const RAW_KEEP_MS: u64 = 3_600_000;

/// The points of one series, oldest first, with no two at the same `at`.
pub(crate) struct RawSeries {
    pub(crate) labels: BTreeMap<String, String>,
    pub(crate) kind: SeriesKind,
    pub(crate) points: VecDeque<(u64, f64)>,
}

/// A raw window as the ring answers it: the points it holds, and, when it cannot answer the whole window, why in words.
#[derive(Debug, Clone, PartialEq)]
pub struct RawWindow {
    /// The points of the series with `from_ms <= at < to_ms`, oldest first, then by labels.
    pub points: Vec<StoredPoint>,
    /// `None` when the ring holds every sample the window asks for. Otherwise the part it cannot answer, in words.
    pub missing: Option<String>,
}

/// Every series' raw points, keyed by name and stored labels text.
#[derive(Default)]
pub(crate) struct RawRing {
    pub(crate) series: BTreeMap<(String, String), RawSeries>,
    /// The oldest `at` the ring has held since the store opened: from here on it is complete, up to its hour.
    since: Option<u64>,
}

impl RawRing {
    /// Adds the tick's points. A point at an `at` the series already holds replaces it. A full series drops its oldest point.
    pub(crate) fn push(&mut self, at: u64, points: &[Point]) -> Result<(), StoreError> {
        for point in points.iter().filter(|point| point.value.is_finite()) {
            let key = (point.name.clone(), labels_text(&point.labels)?);
            let series = self.series.entry(key).or_insert_with(|| RawSeries {
                labels: point.labels.clone(),
                kind: point.kind,
                points: VecDeque::new(),
            });
            insert(&mut series.points, at, point.value);
        }
        if !points.is_empty() {
            self.since = Some(self.since.map_or(at, |since| since.min(at)));
        }
        Ok(())
    }

    /// Reads the raw rows a store written by an earlier release holds, so a minute that was still open when it stopped is
    /// rolled whole. The rollup then deletes every raw row from the file.
    pub(crate) fn load(&mut self, conn: &Connection) -> Result<(), StoreError> {
        let mut stmt = conn.prepare(
            "SELECT p.at, s.name, s.labels, s.kind, p.avg FROM points p JOIN series s ON s.id = p.series_id
             WHERE p.tier = 0 ORDER BY p.at",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let at = uint(row, 0)?;
            let labels: String = row.get(2)?;
            let kind: String = row.get(3)?;
            let point = Point {
                name: row.get(1)?,
                labels: serde_json::from_str(&labels).map_err(|_| {
                    StoreError::Sqlite("a stored label set does not parse".to_string())
                })?,
                kind: if kind == "counter" {
                    SeriesKind::Counter
                } else {
                    SeriesKind::Gauge
                },
                value: row.get(4)?,
            };
            self.push(at, &[point])?;
        }
        Ok(())
    }

    /// The oldest `at` the ring holds, or `None` when it holds none.
    pub(crate) fn first_at(&self) -> Option<u64> {
        self.series
            .values()
            .filter_map(|series| series.points.front().map(|(at, _)| *at))
            .min()
    }

    /// Drops every point older than `keep_ms` before `now_ms`, and every series left with none. Returns the points dropped.
    pub(crate) fn expire(&mut self, now_ms: u64, keep_ms: u64) -> u64 {
        let cutoff = now_ms.saturating_sub(keep_ms);
        let mut dropped = 0;
        for series in self.series.values_mut() {
            while series.points.front().is_some_and(|(at, _)| *at < cutoff) {
                series.points.pop_front();
                dropped += 1;
            }
        }
        self.series.retain(|_, series| !series.points.is_empty());
        dropped
    }

    /// The points of the series named `name` with `from_ms <= at < to_ms`, oldest first, then by labels.
    pub(crate) fn points(&self, name: &str, from_ms: u64, to_ms: u64) -> Vec<StoredPoint> {
        let mut out: Vec<(u64, String, StoredPoint)> = Vec::new();
        for ((series_name, labels), series) in &self.series {
            if series_name != name {
                continue;
            }
            for &(at, value) in series
                .points
                .iter()
                .filter(|(at, _)| *at >= from_ms && *at < to_ms)
            {
                out.push((
                    at,
                    labels.clone(),
                    StoredPoint {
                        at,
                        labels: series.labels.clone(),
                        avg: value,
                        min: value,
                        max: value,
                    },
                ));
            }
        }
        out.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        out.into_iter().map(|(_, _, point)| point).collect()
    }

    /// Why the ring cannot answer `from_ms <= at < to_ms` in full, or `None` when it can. `writer` is false for a read-only
    /// store, whose ring is always empty: the samples are in the writer's memory.
    pub(crate) fn missing(
        &self,
        writer: bool,
        opened_ms: u64,
        from_ms: u64,
        to_ms: u64,
    ) -> Option<String> {
        if from_ms >= to_ms {
            return None;
        }
        if !writer {
            return Some(
                "a read-only store holds no raw samples: the 5-second samples are kept in the server's memory only; \
                 the minute and hour rollups are in the file"
                    .to_string(),
            );
        }
        match (self.since, self.first_at()) {
            (None, _) => Some(format!(
                "no raw sample has been taken since the store opened at {opened_ms} (epoch ms): the 5-second samples are \
                 kept in memory only, so a restart empties them; the minute and hour rollups are in the file"
            )),
            (Some(_), None) => Some(
                "the ring holds no raw sample now: the 5-second samples are kept in memory only, for one hour; the \
                 minute and hour rollups are in the file"
                    .to_string(),
            ),
            (Some(since), Some(first)) => {
                // The ring is complete from the later of when it began filling and its oldest point still held.
                let held_from = since.max(first);
                (from_ms < held_from).then(|| {
                    format!(
                        "the raw samples before {held_from} (epoch ms) are not held: the 5-second samples are kept in \
                         memory only, from when the store opened at {opened_ms} (epoch ms) and for one hour; the minute \
                         and hour rollups are in the file"
                    )
                })
            }
        }
    }
}

/// Inserts `(at, value)` in order. An equal `at` is replaced. Past [`RAW_SLOTS`] the oldest point goes.
fn insert(points: &mut VecDeque<(u64, f64)>, at: u64, value: f64) {
    match points.back() {
        Some((last, _)) if *last < at => points.push_back((at, value)),
        None => points.push_back((at, value)),
        Some(_) => match points.binary_search_by_key(&at, |(at, _)| *at) {
            Ok(index) => points[index].1 = value,
            Err(index) => points.insert(index, (at, value)),
        },
    }
    while points.len() > RAW_SLOTS {
        points.pop_front();
    }
}

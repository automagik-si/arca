//! The store on a temporary state directory: the pragmas it opens with, one tick's commit of points and events, the rollups
//! and their retention, the corrupt-file rename, durability across an abort, and the size at full retention.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use arca_store::{
    AlertEvent, EventState, Point, ResolveReason, SeriesKind, Store, StoreError, Tier,
};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Far enough in the future to read every row, and an integer SQLite can bind.
const FAR: u64 = i64::MAX as u64;
const MINUTE: u64 = 60_000;
const HOUR: u64 = 3_600_000;
const DAY: u64 = 24 * HOUR;
const SERIES: &str = "arca_pool_used_bytes";
const COUNTER: &str = "arca_network_receive_bytes_total";

/// A fresh state directory of mode 0700, removed on drop.
struct StateDir(PathBuf);

impl StateDir {
    fn new(tag: &str) -> StateDir {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("arca-store-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).expect("the state directory is created");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("mode 0700");
        StateDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for StateDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn pool(name: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("pool".to_string(), name.to_string())])
}

fn event(state: EventState, at: u64) -> AlertEvent {
    AlertEvent {
        seq: None,
        alert_id: None,
        rule: "disk-pool-full".to_string(),
        severity: "critical".to_string(),
        state,
        labels: pool("default"),
        value: Some(95.0),
        threshold: Some(90.0),
        started_at: 60_000,
        resolved_at: (state == EventState::Resolved).then_some(at),
        reason: (state == EventState::Resolved).then_some(ResolveReason::Cleared),
        at,
    }
}

mod pragmas {
    use super::*;

    #[test]
    fn a_fresh_store_reads_back_wal_full_and_incremental_vacuum() {
        let dir = StateDir::new("fresh");
        let opened = Store::open(dir.path(), 0).expect("a fresh store opens");
        assert_eq!(opened.pragmas.journal_mode, "wal");
        assert_eq!(opened.pragmas.synchronous, 2);
        assert_eq!(opened.pragmas.auto_vacuum, 2);
        assert!(opened.pragmas.sqlite_version.starts_with("3."));
        assert_eq!(opened.reset, None);
        assert!(dir.path().join(arca_store::DB_FILE).is_file());
        drop(opened);
        let again = Store::open(dir.path(), 0).expect("the store opens again");
        assert_eq!(
            again.pragmas.auto_vacuum, 2,
            "auto_vacuum stays on the file"
        );
        Store::open_read_only(dir.path()).expect("a reader opens with the same read-back");
    }

    #[test]
    fn a_connection_with_synchronous_normal_is_refused() {
        let dir = StateDir::new("normal");
        drop(Store::open(dir.path(), 0).expect("the store is created in WAL mode"));
        let conn = rusqlite::Connection::open(dir.path().join(arca_store::DB_FILE)).expect("open");
        conn.pragma_update(None, "synchronous", "NORMAL")
            .expect("set NORMAL");
        let refused = Store::check_pragmas(&conn);
        assert!(
            matches!(refused, Err(StoreError::Refused(_))),
            "{refused:?}"
        );
    }

    #[test]
    fn a_connection_not_in_wal_mode_is_refused() {
        let dir = StateDir::new("delete");
        let conn = rusqlite::Connection::open(dir.path().join("other.db")).expect("open");
        conn.pragma_update(None, "synchronous", 2)
            .expect("set FULL");
        let refused = Store::check_pragmas(&conn);
        assert!(
            matches!(refused, Err(StoreError::Refused(_))),
            "{refused:?}"
        );
    }
}

mod tick {
    use super::*;

    #[test]
    fn a_commit_returns_increasing_seqs_and_a_firing_event_is_listed_as_firing() {
        let dir = StateDir::new("firing");
        let mut store = Store::open(dir.path(), 0).expect("open").store;
        let points = [Point {
            name: "arca_pool_used_bytes".to_string(),
            labels: pool("default"),
            kind: SeriesKind::Gauge,
            value: 95.0,
        }];
        let first = store
            .commit_tick(60_000, &points, &[event(EventState::Firing, 60_000)])
            .expect("the first tick commits");
        let mut other = event(EventState::Firing, 65_000);
        other.labels = pool("pool1");
        let second = store
            .commit_tick(65_000, &points, &[other])
            .expect("the second tick commits");
        assert_eq!(first.len(), 1);
        assert!(second[0] > first[0], "seqs increase: {first:?} {second:?}");

        let firing = store.firing().expect("the firing alerts are read");
        assert_eq!(firing.len(), 2);
        assert_eq!(firing[0].seq, Some(first[0]));
        assert_eq!(
            firing[0].alert_id,
            Some(first[0]),
            "a firing event is its own alert"
        );
        assert_eq!(firing[0].labels, pool("default"));
        assert_eq!(firing[0].value, Some(95.0));
        let reader = Store::open_read_only(dir.path()).expect("a reader opens");
        assert_eq!(reader.firing().expect("read"), firing);
        assert!(store.bytes() > 0);
    }

    #[test]
    fn a_resolve_in_the_same_batch_links_to_its_firing_event() {
        let dir = StateDir::new("resolve");
        let mut store = Store::open(dir.path(), 0).expect("open").store;
        let seqs = store
            .commit_tick(
                60_000,
                &[],
                &[
                    event(EventState::Firing, 60_000),
                    event(EventState::Resolved, 60_000),
                ],
            )
            .expect("the batch commits");
        assert!(seqs[1] > seqs[0]);
        assert!(
            store.firing().expect("read").is_empty(),
            "the alert resolved"
        );
        let all = store.events_after(0, 10).expect("read");
        assert_eq!(all.len(), 2);
        assert_eq!(
            all[1].alert_id,
            Some(seqs[0]),
            "the resolve shares the alert id"
        );
        assert_eq!(all[1].reason, Some(ResolveReason::Cleared));
        assert_eq!(all[1].to_json("n")["reason"], "cleared");
        assert!(
            all[0].to_json("n").get("reason").is_none(),
            "only a resolve has a reason"
        );
        assert_eq!(
            store.resolved_since(0, 10).expect("read"),
            vec![all[1].clone()]
        );
    }
}

fn gauge(value: f64) -> Point {
    Point {
        name: SERIES.to_string(),
        labels: pool("default"),
        kind: SeriesKind::Gauge,
        value,
    }
}

fn counter(value: f64) -> Point {
    Point {
        name: COUNTER.to_string(),
        labels: BTreeMap::new(),
        kind: SeriesKind::Counter,
        value,
    }
}

mod rollup {
    use super::*;

    #[test]
    fn catch_up() {
        let dir = StateDir::new("catch-up");
        let base = 10 * HOUR;
        // The node samples every 5 s until 90 min 30 s, and rolls up at each minute's first tick. The value is the tick index.
        let stop = base + 90 * MINUTE + 30_000;
        let mut store = Store::open(dir.path(), base).expect("open").store;
        let mut run_minutes = 0;
        let mut at = base;
        let mut tick = 0.0;
        while at < stop {
            store
                .commit_tick(at, &[gauge(tick)], &[])
                .expect("the tick commits");
            if (at - base).is_multiple_of(MINUTE) {
                run_minutes += store.rollup(at).expect("rollup").minutes_rolled;
            }
            at += 5_000;
            tick += 1.0;
        }
        drop(store);

        // Five hours later the node starts again. The minute the stop cut is closed now, and its raw rows are still there.
        let restart = stop + 5 * HOUR;
        let mut store = Store::open(dir.path(), restart).expect("reopen").store;
        let report = store.rollup(restart).expect("catch-up");
        assert_eq!(report.minutes_rolled, 1, "only the minute the stop cut");
        assert_eq!(
            report.hours_rolled, 1,
            "only the hour that closed during the stop"
        );
        assert_eq!(run_minutes, 90, "minutes 0 to 89 rolled while the node ran");
        let again = store.rollup(restart).expect("a second catch-up");
        assert_eq!(
            (
                again.minutes_rolled,
                again.hours_rolled,
                again.deleted_points
            ),
            (0, 0, 0),
            "a second pass rolls nothing"
        );

        let minutes = store.points(Tier::Minute, SERIES, 0, FAR).expect("read");
        assert_eq!(minutes.len(), 91, "every minute with data is rolled once");
        assert_eq!(minutes[0].at, base);
        assert_eq!(minutes[0].avg, 5.5, "the mean of ticks 0 to 11");
        assert_eq!((minutes[0].min, minutes[0].max), (0.0, 11.0));
        let cut = minutes.last().expect("the cut minute");
        assert_eq!(cut.at, base + 90 * MINUTE);
        assert_eq!(
            cut.avg, 1082.5,
            "the mean of ticks 1080 to 1085, rolled from raw rows before the stop"
        );

        let hours = store.points(Tier::Hour, SERIES, 0, FAR).expect("read");
        assert_eq!(
            hours.iter().map(|h| h.at).collect::<Vec<_>>(),
            vec![base, base + HOUR]
        );
        assert_eq!(
            hours[0].avg, 359.5,
            "the mean of the minute averages of hour 0"
        );
        assert_eq!((hours[1].min, hours[1].max), (720.0, 1085.0));
        let expected = (2235.0 * 12.0 + 30.0 * 5.5 + 1082.5) / 31.0;
        assert!(
            (hours[1].avg - expected).abs() < 1e-9,
            "hour 1 averages minutes 60 to 90"
        );
    }

    #[test]
    fn retention() {
        let now = 1_000 * DAY;

        // Raw points: exactly one hour is kept, and one millisecond older is deleted.
        let dir = StateDir::new("retention-raw");
        let mut store = Store::open(dir.path(), now).expect("open").store;
        store
            .commit_tick(now - HOUR, &[gauge(1.0)], &[])
            .expect("kept");
        store
            .commit_tick(now - HOUR - 1, &[gauge(2.0)], &[])
            .expect("deleted");
        store.rollup(now).expect("rollup");
        let raw = store.points(Tier::Raw, SERIES, 0, FAR).expect("read");
        assert_eq!(
            raw.iter().map(|p| p.at).collect::<Vec<_>>(),
            vec![now - HOUR]
        );

        // Minute points: kept at exactly seven days, deleted one millisecond later.
        let dir = StateDir::new("retention-minute");
        let mut store = Store::open(dir.path(), now).expect("open").store;
        let minute = now - 7 * DAY;
        store.commit_tick(minute, &[gauge(3.0)], &[]).expect("tick");
        store
            .rollup(minute + 2 * MINUTE)
            .expect("the minute is rolled");
        assert_eq!(
            store
                .points(Tier::Minute, SERIES, 0, FAR)
                .expect("read")
                .len(),
            1
        );
        store.rollup(now).expect("exactly seven days");
        assert_eq!(
            store
                .points(Tier::Minute, SERIES, 0, FAR)
                .expect("read")
                .len(),
            1
        );
        store.rollup(now + 1).expect("one millisecond later");
        assert_eq!(
            store
                .points(Tier::Minute, SERIES, 0, FAR)
                .expect("read")
                .len(),
            0
        );

        // Hour points: kept at exactly ninety days, deleted one millisecond later.
        let dir = StateDir::new("retention-hour");
        let mut store = Store::open(dir.path(), now).expect("open").store;
        let hour = now - 90 * DAY;
        store.commit_tick(hour, &[gauge(4.0)], &[]).expect("tick");
        store
            .rollup(hour + 2 * MINUTE)
            .expect("the minute is rolled");
        store.rollup(hour + HOUR).expect("the hour is rolled");
        store.rollup(now).expect("exactly ninety days");
        assert_eq!(
            store
                .points(Tier::Hour, SERIES, 0, FAR)
                .expect("read")
                .len(),
            1
        );
        store.rollup(now + 1).expect("one millisecond later");
        assert_eq!(
            store
                .points(Tier::Hour, SERIES, 0, FAR)
                .expect("read")
                .len(),
            0
        );

        // Events: a firing alert is kept whatever its age, and a resolved pair is kept until its resolve is 90 d old.
        let dir = StateDir::new("retention-events");
        let mut store = Store::open(dir.path(), now).expect("open").store;
        let mut firing = event(EventState::Firing, now - 200 * DAY);
        firing.labels = pool("kept");
        store.commit_tick(now, &[], &[firing]).expect("firing");
        let mut edge_fired = event(EventState::Firing, now - 300 * DAY);
        edge_fired.labels = pool("edge");
        let mut edge_resolved = event(EventState::Resolved, now - 90 * DAY);
        edge_resolved.labels = pool("edge");
        store
            .commit_tick(now, &[], &[edge_fired, edge_resolved])
            .expect("a pair whose resolve is exactly 90 d old");
        let mut old_fired = event(EventState::Firing, now - 300 * DAY);
        old_fired.labels = pool("old");
        let mut old_resolved = event(EventState::Resolved, now - 90 * DAY - 1);
        old_resolved.labels = pool("old");
        store
            .commit_tick(now, &[], &[old_fired, old_resolved])
            .expect("a pair whose resolve is one millisecond older");
        let report = store.rollup(now).expect("rollup");
        assert_eq!(report.deleted_events, 2, "the old pair, both events");
        let all = store.events_after(0, 100).expect("read");
        assert_eq!(all.len(), 3);
        assert!(all.iter().all(|event| event.labels != pool("old")));
        let open = store.firing().expect("read");
        assert_eq!(open.len(), 1, "only the firing alert with no resolve");
        assert_eq!(open[0].labels, pool("kept"));
    }

    #[test]
    fn counters_last_value() {
        let dir = StateDir::new("counters");
        let base = 1_000 * DAY;
        let mut store = Store::open(dir.path(), base).expect("open").store;
        // A counter sampled every 5 s for an hour, with value = tick index.
        for tick in 0..720u64 {
            store
                .commit_tick(base + 5_000 * tick, &[counter(tick as f64)], &[])
                .expect("tick");
        }
        store.rollup(base + HOUR + MINUTE).expect("rollup");

        let minutes = store.points(Tier::Minute, COUNTER, 0, FAR).expect("read");
        assert_eq!(minutes.len(), 60);
        assert_eq!(
            (minutes[0].avg, minutes[0].min, minutes[0].max),
            (11.0, 11.0, 11.0)
        );
        assert_eq!(minutes[59].avg, 719.0, "the last value of the minute");

        let hours = store.points(Tier::Hour, COUNTER, 0, FAR).expect("read");
        assert_eq!(hours.len(), 1);
        assert_eq!(
            (hours[0].avg, hours[0].min, hours[0].max),
            (719.0, 719.0, 719.0)
        );
    }

    #[test]
    fn gaps() {
        let dir = StateDir::new("gaps");
        let base = 1_000 * DAY;
        let mut store = Store::open(dir.path(), base).expect("open").store;
        for minute in [0, 1, 3] {
            store
                .commit_tick(base + minute * MINUTE + 5_000, &[gauge(1.0)], &[])
                .expect("tick");
        }
        store.rollup(base + 5 * MINUTE).expect("rollup");
        let rolled: Vec<u64> = store
            .points(Tier::Minute, SERIES, 0, FAR)
            .expect("read")
            .iter()
            .map(|p| p.at)
            .collect();
        assert_eq!(
            rolled,
            vec![base, base + MINUTE, base + 3 * MINUTE],
            "minute 2 stays a gap"
        );
    }
}

mod corrupt_rename {
    use super::*;

    const NOW: u64 = 1_761_998_400_000;

    fn garbage() -> Vec<u8> {
        vec![0x5a; 8_192]
    }

    #[test]
    fn all_three_files_are_renamed_with_one_suffix() {
        let dir = StateDir::new("rename");
        for name in [arca_store::DB_FILE, "arca.db-wal", "arca.db-shm"] {
            fs::write(dir.path().join(name), garbage()).expect("the garbage is written");
        }
        let opened = Store::open(dir.path(), NOW).expect("a corrupt file is replaced, not refused");
        assert_eq!(opened.reset, Some(format!(".corrupt-{NOW}")));
        // The database and its WAL keep their bytes. The `-shm` is a rebuilt index, which the read-only probe may write, so
        // only its rename is checked.
        for name in [arca_store::DB_FILE, "arca.db-wal"] {
            let renamed = dir.path().join(format!("{name}.corrupt-{NOW}"));
            assert_eq!(
                fs::read(&renamed).expect("renamed"),
                garbage(),
                "{name} keeps its bytes"
            );
        }
        assert!(
            dir.path()
                .join(format!("arca.db-shm.corrupt-{NOW}"))
                .is_file()
        );
        assert_eq!(opened.pragmas.journal_mode, "wal");
        assert!(
            opened.store.firing().expect("read").is_empty(),
            "the new file holds no rows"
        );

        let mut store = opened.store;
        let seqs = store
            .commit_tick(NOW, &[], &[event(EventState::Firing, NOW)])
            .expect("the new file takes a commit");
        assert!(
            seqs[0] >= NOW,
            "the first seq is at least the reset ms: {seqs:?}"
        );
    }

    #[test]
    fn only_the_files_that_exist_are_renamed() {
        let dir = StateDir::new("only-db");
        fs::write(dir.path().join(arca_store::DB_FILE), garbage()).expect("written");
        let opened = Store::open(dir.path(), NOW).expect("replaced");
        assert_eq!(opened.reset, Some(format!(".corrupt-{NOW}")));
        assert!(dir.path().join(format!("arca.db.corrupt-{NOW}")).is_file());
        assert!(
            !dir.path()
                .join(format!("arca.db-wal.corrupt-{NOW}"))
                .exists()
        );
        assert!(
            !dir.path()
                .join(format!("arca.db-shm.corrupt-{NOW}"))
                .exists()
        );
    }

    #[test]
    fn the_first_seq_follows_the_old_highest_seq() {
        let dir = StateDir::new("old-max");
        let path = dir.path().join(arca_store::DB_FILE);
        {
            let mut store = Store::open(dir.path(), 0).expect("open").store;
            for at in 1..=5 {
                store
                    .commit_tick(at, &[], &[event(EventState::Firing, at)])
                    .expect("commit");
            }
        }
        // Checkpoint the WAL into the file, then break the page of the index on `alert_id`.
        let conn = rusqlite::Connection::open(&path).expect("open");
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .expect("checkpoint");
        let page_size: i64 = conn
            .query_row("PRAGMA page_size", [], |row| row.get(0))
            .expect("page size");
        let root: i64 = conn
            .query_row(
                "SELECT rootpage FROM sqlite_master WHERE name = 'alert_events_alert'",
                [],
                |row| row.get(0),
            )
            .expect("the index root");
        drop(conn);
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut file = fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("open");
            file.seek(SeekFrom::Start((root as u64 - 1) * page_size as u64))
                .expect("seek");
            file.write_all(&vec![0xff; page_size as usize])
                .expect("write");
        }

        let mut opened = Store::open(dir.path(), 3).expect("a corrupt index is replaced");
        assert_eq!(opened.reset, Some(".corrupt-3".to_string()));
        let seqs = opened
            .store
            .commit_tick(3, &[], &[event(EventState::Firing, 3)])
            .expect("commit");
        assert_eq!(
            seqs,
            vec![6],
            "the old highest seq was 5, so the new file starts at 6"
        );
    }

    #[test]
    fn a_fresh_store_starts_its_seq_at_the_wall_clock() {
        let dir = StateDir::new("fresh-floor");
        let now: u64 = 1_700_000_000_000;
        let mut opened = Store::open(dir.path(), now).expect("open");
        assert_eq!(opened.reset, None, "a new directory has no corrupt file");
        let seqs = opened
            .store
            .commit_tick(now, &[], &[event(EventState::Firing, now)])
            .expect("commit");
        assert!(
            seqs[0] >= now,
            "the first seq {} is below the wall clock {now}",
            seqs[0]
        );
    }
}

mod durability {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;

    /// Set on the child process only. The child test does its work when it is set, and does nothing otherwise.
    const CHILD_DIR: &str = "ARCA_STORE_TEST_CHILD_DIR";
    /// The signal `std::process::abort` raises.
    const SIGABRT: i32 = 6;

    #[test]
    fn child() {
        let Some(dir) = std::env::var_os(CHILD_DIR) else {
            return;
        };
        let mut store = Store::open(Path::new(&dir), 0)
            .expect("the child opens the store")
            .store;
        store
            .commit_tick(60_000, &[], &[event(EventState::Firing, 60_000)])
            .expect("the child commits a firing event");
        std::process::abort();
    }

    #[test]
    fn a_committed_firing_event_survives_an_abort() {
        let dir = StateDir::new("abort");
        let status = Command::new(std::env::current_exe().expect("the test binary"))
            .args(["--exact", "durability::child", "--test-threads=1"])
            .env(CHILD_DIR, dir.path())
            .status()
            .expect("the child runs");
        assert_eq!(
            status.signal(),
            Some(SIGABRT),
            "the child stopped by abort, after its commit: {status:?}"
        );

        let store = Store::open(dir.path(), 0).expect("the store reopens").store;
        let firing = store.firing().expect("read");
        assert_eq!(firing.len(), 1, "the committed event survives");
        assert_eq!(firing[0].state, EventState::Firing);
        assert_eq!(firing[0].seq, Some(1));
    }
}

mod size_estimate {
    use super::*;

    const SERIES_COUNT: i64 = 125;
    const NOW: u64 = 1_761_998_400_000;

    #[test]
    fn full_retention_fits_in_128_mb() {
        let dir = StateDir::new("size");
        drop(Store::open(dir.path(), NOW).expect("the store is created"));
        let path = dir.path().join(arca_store::DB_FILE);
        let mut conn = rusqlite::Connection::open(&path).expect("open");
        let tx = conn.transaction().expect("begin");
        {
            let mut series = tx
                .prepare("INSERT INTO series (name, labels, kind) VALUES (?1, ?2, 'gauge')")
                .expect("prepare");
            for id in 1..=SERIES_COUNT {
                series
                    .execute(rusqlite::params![
                        format!("arca_size_{id}"),
                        format!("{{\"series\":\"{id}\"}}")
                    ])
                    .expect("the series is inserted");
            }
            let mut points = tx
                .prepare("INSERT INTO points (tier, at, series_id, avg, min, max) VALUES (?1, ?2, ?3, ?4, ?4, ?4)")
                .expect("prepare");
            for id in 1..=SERIES_COUNT {
                let value = |k: u64| 0.5 + k as f64 * 0.37 + id as f64 * 0.01;
                for k in 0..720u64 {
                    let at = (NOW - HOUR + k * 5_000) as i64;
                    points
                        .execute(rusqlite::params![0, at, id, value(k)])
                        .expect("a raw row");
                }
                for k in 0..10_080u64 {
                    let at = (NOW - 7 * DAY + k * MINUTE) as i64;
                    points
                        .execute(rusqlite::params![1, at, id, value(k)])
                        .expect("a minute row");
                }
                for k in 0..2_160u64 {
                    let at = (NOW - 90 * DAY + k * HOUR) as i64;
                    points
                        .execute(rusqlite::params![2, at, id, value(k)])
                        .expect("an hour row");
                }
            }
        }
        tx.commit().expect("commit");
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM points", [], |row| row.get(0))
            .expect("count");
        assert_eq!(rows, SERIES_COUNT * (720 + 10_080 + 2_160));
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .expect("checkpoint");
        drop(conn);

        let store = Store::open_read_only(dir.path()).expect("a reader opens");
        let bytes = store.bytes();
        println!(
            "size_estimate: {SERIES_COUNT} series, {rows} rows, {bytes} bytes ({:.1} MiB)",
            bytes as f64 / 1_048_576.0
        );
        assert!(bytes < 128 * 1_048_576, "{bytes} bytes is over 128 MB");
    }
}

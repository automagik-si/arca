//! The store on a temporary state directory: the pragmas it opens with, and one tick's commit of points and events.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use arca_store::{AlertEvent, EventState, Point, ResolveReason, SeriesKind, Store, StoreError};

static SEQ: AtomicU64 = AtomicU64::new(0);

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

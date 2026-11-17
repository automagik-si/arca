//! `journal_list`: paging and the secret-safe projection.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use arca::handlers::JournalListHandler;
use arca::kernel::JOURNAL_FILE;
use arca_api::Scope;
use arca_kernel::canon::Digest;
use arca_kernel::desired::{EntryId, PluginId};
use arca_kernel::journal::{
    BindingEvent, BindingState, EffectClass, EffectRecord, EffectState, FileJournal, Inverse,
    Journal, JournalSink,
};
use arca_kernel::ownership::{IncusKind, ObjectRef};
use arca_mcp::{Caller, ToolHandler};
use serde_json::json;

fn scratch() -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .subsec_nanos();
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "arca-journal-list-{}-{nanos}-{n}",
        std::process::id()
    ));
    fs::create_dir(&dir).expect("state dir");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("0700");
    dir
}

fn object() -> ObjectRef {
    ObjectRef {
        parent: None,
        kind: IncusKind::Instance,
        project: "p".to_string(),
        name: "n".to_string(),
        uuid: None,
    }
}

fn record(id: u64) -> EffectRecord {
    let mut pre_image = BTreeMap::new();
    pre_image.insert(
        "environment.SECRET_TOKEN".to_string(),
        Some("s3cr3t-value".to_string()),
    );
    EffectRecord {
        id,
        entry: EntryId("watch".to_string()),
        class: EffectClass::A,
        op_digest: Digest::of(b"op"),
        target: object(),
        pre_image: Some(pre_image),
        acl_patch: None,
        intent: None,
        plugin: Some(PluginId("arca.postgres".to_string())),
        object: Some(object()),
        inverse: Some(Inverse::Emission),
        state: EffectState::Applied,
        at_ms: 1_000,
    }
}

fn caller() -> Caller {
    Caller {
        token_id: "v".to_string(),
        scope: Scope::Viewer,
        tools: None,
        limits: Default::default(),
    }
}

#[test]
fn journal_list_pages_by_id_and_never_returns_a_pre_image_or_inverse() {
    let dir = scratch();
    let path = dir.join(JOURNAL_FILE);
    let mut body = String::new();
    for id in [1u64, 2] {
        body.push_str(&serde_json::to_string(&record(id)).expect("record JSON"));
        body.push('\n');
    }
    let mut confirmed = record(1);
    confirmed.state = EffectState::Confirmed;
    body.push_str(&serde_json::to_string(&confirmed).expect("confirmed JSON"));
    body.push('\n');
    fs::write(&path, body).expect("journal");
    let handler = JournalListHandler {
        state_dir: dir.clone(),
    };
    let answer = handler
        .call(json!({ "after": 1, "limit": 1 }), &caller())
        .expect("journal_list");
    let text = answer.to_string();
    assert!(!text.contains("s3cr3t-value"), "a secret pre-image leaked");
    assert!(!text.contains("pre_image"));
    assert!(!text.contains("inverse"));
    assert_eq!(answer["records"].as_array().expect("records").len(), 1);
    assert_eq!(answer["records"][0]["id"], json!(2));
    assert_eq!(answer["records"][0]["entry"], json!("watch"));
    assert_eq!(answer["records"][0]["plugin"], json!("arca.postgres"));
    assert_eq!(answer["records"][0]["class"], json!("A"));
    assert_eq!(answer["records"][0]["state"], json!("Applied"));
    let first = handler
        .call(json!({ "after": 0, "limit": 1 }), &caller())
        .expect("journal_list first");
    assert_eq!(first["records"].as_array().expect("records").len(), 1);
    assert_eq!(first["records"][0]["id"], json!(1));
    assert_eq!(first["records"][0]["state"], json!("Confirmed"));
    let keys: Vec<&str> = answer["records"][0]
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    for key in &keys {
        assert!(
            matches!(
                *key,
                "id" | "entry"
                    | "class"
                    | "target"
                    | "plugin"
                    | "state"
                    | "at_ms"
                    | "intent"
                    | "object"
            ),
            "unexpected field {key}"
        );
    }
    let _ = fs::remove_dir_all(dir);
}

fn bound_journal(dir: &std::path::Path, count: u64, data: bool) {
    let mut journal = Journal::open(FileJournal::open(&dir.join(JOURNAL_FILE)).unwrap()).unwrap();
    let instance = |n: u64| ObjectRef {
        project: "arcak-svc".into(),
        name: "sample".into(),
        uuid: Some(format!("copy-{n}")),
        ..object()
    };
    let volume = |n: u64| ObjectRef {
        kind: IncusKind::Volume,
        project: "arcak-svc".into(),
        name: format!("sample-data-{n}"),
        parent: Some(format!("pool-{n}")),
        uuid: None,
    };
    let entry = EntryId("service-sample".into());
    for object in std::iter::once(instance(0)).chain(data.then(|| volume(0))) {
        let mut target = object.clone();
        target.uuid = None;
        let id = journal
            .intend(
                &entry,
                EffectClass::B,
                Digest::of(b"create"),
                target,
                None,
                10,
            )
            .unwrap();
        journal
            .applied(id, object.clone(), Inverse::IncusDelete { object })
            .unwrap();
    }
    let state = |n| BindingState {
        instance: instance(n),
        root_pool: format!("pool-{n}"),
        data_volume: data.then(|| volume(n)),
    };
    for n in 1..=count {
        journal
            .bind(BindingEvent {
                entry: entry.clone(),
                from: state(n - 1),
                to: state(n),
                at_ms: 10 + n,
            })
            .unwrap();
        if n == 1 && data {
            journal.confirmed(1);
        }
    }
}

#[test]
fn a_journal_without_bindings_keeps_the_exact_listing_bytes() {
    let dir = scratch();
    let record = record(1);
    fs::write(
        dir.join(JOURNAL_FILE),
        format!("{}\n", serde_json::to_string(&record).unwrap()),
    )
    .unwrap();
    let answer = JournalListHandler {
        state_dir: dir.clone(),
    }
    .call(json!({}), &caller())
    .unwrap();
    let expected = json!({"records":[{
        "id":1,"entry":"watch","class":"A","target":record.target,
        "plugin":"arca.postgres","state":"Applied","at_ms":1000,"object":record.object
    }]});
    assert_eq!(
        serde_json::to_vec(&answer).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn bindings_are_ordered_separate_from_effects_and_repeated_on_every_page() {
    let dir = scratch();
    bound_journal(&dir, 2, true);
    let handler = JournalListHandler {
        state_dir: dir.clone(),
    };
    let answer = handler.call(json!({"limit":1}), &caller()).unwrap();
    assert_eq!(answer["records"].as_array().unwrap().len(), 1);
    assert_eq!(answer["records"][0]["id"], 1);
    assert_eq!(answer["records"][0]["object"]["uuid"], "copy-2");
    let bindings = answer["bindings"]
        .as_array()
        .expect("binding history is visible");
    assert_eq!(bindings.len(), 2);
    for (n, binding) in bindings.iter().enumerate() {
        assert_eq!(binding["kind"], "binding");
        assert_eq!(binding["entry"], "service-sample");
        assert_eq!(binding["from"]["instance"]["uuid"], format!("copy-{n}"));
        assert_eq!(binding["to"]["instance"]["uuid"], format!("copy-{}", n + 1));
        assert_eq!(
            binding["from"]["data_volume"]["parent"],
            format!("pool-{n}")
        );
        assert_eq!(
            binding["to"]["data_volume"]["name"],
            format!("sample-data-{}", n + 1)
        );
        assert_eq!(binding["at_ms"], 11 + n);
        assert_eq!(binding["afterEffect"], if n == 0 { 2 } else { 1 });
        assert!(binding.get("id").is_none());
        assert!(binding.get("state").is_none());
    }
    let next = handler
        .call(json!({"after":1,"limit":1}), &caller())
        .unwrap();
    assert_eq!(next["records"][0]["id"], 2);
    assert_eq!(next["bindings"], answer["bindings"]);
    let empty = handler
        .call(json!({"after":2,"limit":0}), &caller())
        .unwrap();
    assert_eq!(empty["records"], json!([]));
    assert_eq!(empty["bindings"], answer["bindings"]);
    assert!(answer.get("bindingsTruncated").is_none());
    let schema = (arca_api::verb("journal_list").unwrap().result)();
    arca_api::conforms(&schema, &answer).unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn binding_history_returns_the_newest_hundred_and_marks_truncation() {
    let dir = scratch();
    bound_journal(&dir, 102, false);
    let answer = JournalListHandler {
        state_dir: dir.clone(),
    }
    .call(json!({}), &caller())
    .unwrap();
    let bindings = answer["bindings"].as_array().expect("binding history");
    assert_eq!(bindings.len(), 100);
    assert_eq!(bindings[0]["from"]["instance"]["uuid"], "copy-2");
    assert_eq!(bindings[99]["to"]["instance"]["uuid"], "copy-102");
    assert_eq!(bindings[0]["from"]["data_volume"], json!(null));
    assert_eq!(answer["bindingsTruncated"], true);
    arca_api::conforms(&(arca_api::verb("journal_list").unwrap().result)(), &answer).unwrap();
    fs::remove_dir_all(dir).unwrap();
}

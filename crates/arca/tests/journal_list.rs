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
use arca_kernel::journal::{EffectClass, EffectRecord, EffectState, Inverse};
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

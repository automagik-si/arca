//! Token files and bearer lookup. Each refusal path has its own test: a mode other than 0400, a symlink, a file owned by
//! another uid, a malformed body, a name that is not an id, and a bearer that is malformed or unknown.

mod common;

use std::fs::{self, Permissions};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;

use arca_api::Scope;
use arca_mcp::{Caller, TokenStore, TokenStoreError, mint, mint_with_tools};
use common::{Scratch, mint_into, own_uid, write_hash_file, write_named};
use sha2::{Digest, Sha256};

/// A secret of the right shape that no store holds.
fn unknown_secret() -> String {
    format!("arca_{}", "A".repeat(43))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn file_of(err: &TokenStoreError) -> Option<&str> {
    match err {
        TokenStoreError::BadMode { file }
        | TokenStoreError::Symlink { file }
        | TokenStoreError::NotOwner { file }
        | TokenStoreError::Malformed { file }
        | TokenStoreError::Io { file } => Some(file),
        TokenStoreError::NotDir | TokenStoreError::UnsafeDir { .. } => None,
    }
}

// Minting.

#[test]
fn a_minted_secret_is_arca_then_43_base64url_characters() {
    let (secret, _) = mint(Scope::Viewer);
    let body = secret.strip_prefix("arca_").expect("the arca_ prefix");
    assert_eq!(body.len(), 43, "32 random bytes in unpadded base64url");
    assert!(
        body.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "the body is base64url"
    );
}

#[test]
fn two_mints_never_repeat_a_secret() {
    let (a, _) = mint(Scope::Operator);
    let (b, _) = mint(Scope::Operator);
    assert_ne!(a, b);
}

#[test]
fn the_hash_file_holds_the_scope_and_sha256_of_the_secret_and_never_the_secret() {
    let (secret, text) = mint(Scope::Doctor);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "exactly two lines: {text:?}");
    assert_eq!(lines[0], "scope=doctor");
    let digest = lines[1].strip_prefix("sha256=").expect("a sha256 line");
    assert_eq!(digest, hex(&Sha256::digest(secret.as_bytes())));
    assert!(text.ends_with('\n'), "the file is newline-terminated");
    assert!(!text.contains(&secret), "the secret itself is not stored");
    assert!(!text.contains(&secret[5..]), "not even the secret's body");
}

// Loading a good store.

#[test]
fn a_loaded_store_authenticates_each_secret_as_its_own_token() {
    let scratch = Scratch::new("good");
    let viewer = mint_into(scratch.path(), "ops-viewer", Scope::Viewer);
    let admin = mint_into(scratch.path(), "ops-admin", Scope::Operator);
    let store = TokenStore::load(scratch.path()).expect("a store of two 0400 files loads");
    assert_eq!(
        store.lookup(&viewer),
        Some(Caller {
            token_id: "ops-viewer".into(),
            scope: Scope::Viewer,
            tools: None,
        })
    );
    assert_eq!(
        store.lookup(&admin),
        Some(Caller {
            token_id: "ops-admin".into(),
            scope: Scope::Operator,
            tools: None,
        })
    );
}

#[test]
fn a_store_finds_its_token_among_many() {
    let scratch = Scratch::new("many");
    let secrets: Vec<String> = (0..6)
        .map(|i| mint_into(scratch.path(), &format!("tok-{i}"), Scope::Doctor))
        .collect();
    let store = TokenStore::load(scratch.path()).expect("loads");
    for (i, secret) in secrets.iter().enumerate() {
        let caller = store
            .lookup(secret)
            .expect("every stored secret authenticates");
        assert_eq!(caller.token_id, format!("tok-{i}"));
    }
}

#[test]
fn a_tools_line_is_loaded_into_the_caller_and_a_two_line_file_admits_everything() {
    let scratch = Scratch::new("tools-line");
    let tools = vec!["journal_list".to_string(), "node_metrics".to_string()];
    let (secret, text) = mint_with_tools(Scope::Viewer, Some(&tools));
    write_hash_file(scratch.path(), "v-tools", &text, 0o400);
    let two_line = mint(Scope::Viewer);
    write_hash_file(scratch.path(), "v-all", &two_line.1, 0o400);
    let store = TokenStore::load(scratch.path()).expect("loads");
    let caller = store.lookup(&secret).expect("the allowlisted token");
    assert_eq!(caller.tools, Some(tools.clone()));
    assert!(caller.allows_tool("journal_list"));
    assert!(!caller.allows_tool("watch_report"));
    let all = store.lookup(&two_line.0).expect("the two-line token");
    assert_eq!(all.tools, None);
    assert!(all.allows_tool("watch_report"));
    assert_eq!(
        store.ids(),
        vec![
            ("v-all".to_string(), Scope::Viewer, None),
            ("v-tools".to_string(), Scope::Viewer, Some(tools))
        ]
    );
}

#[test]
fn reload_drops_a_removed_token_without_restarting_the_store() {
    let scratch = Scratch::new("reload");
    let first = mint_into(scratch.path(), "v-first", Scope::Viewer);
    let store = TokenStore::load(scratch.path()).expect("loads");
    let live = store.clone();
    assert!(live.lookup(&first).is_some());
    fs::remove_file(scratch.at("v-first.hash")).expect("remove");
    assert_eq!(live.reload(scratch.path()).expect("reload"), 0);
    assert!(live.lookup(&first).is_none());
    assert!(live.ids().is_empty());
    let second = mint_into(scratch.path(), "v-second", Scope::Doctor);
    assert_eq!(live.reload(scratch.path()).expect("reload"), 1);
    assert_eq!(
        live.lookup(&second).map(|caller| caller.scope),
        Some(Scope::Doctor)
    );
}

#[test]
fn ids_lists_each_token_with_its_scope_and_nothing_else() {
    let scratch = Scratch::new("ids");
    mint_into(scratch.path(), "b-doctor", Scope::Doctor);
    mint_into(scratch.path(), "a-viewer", Scope::Viewer);
    let store = TokenStore::load(scratch.path()).expect("loads");
    assert_eq!(
        store.ids(),
        vec![
            ("a-viewer".to_string(), Scope::Viewer, None),
            ("b-doctor".to_string(), Scope::Doctor, None)
        ]
    );
}

#[test]
fn an_empty_directory_loads_and_authenticates_nothing() {
    let scratch = Scratch::new("empty");
    let store = TokenStore::load(scratch.path()).expect("an empty store is a valid store");
    assert!(store.ids().is_empty());
    assert_eq!(store.lookup(&unknown_secret()), None);
}

// Bearer refusals: nothing authenticates.

#[test]
fn a_malformed_bearer_authenticates_as_nobody() {
    let scratch = Scratch::new("malformed");
    let secret = mint_into(scratch.path(), "ops", Scope::Operator);
    let store = TokenStore::load(scratch.path()).expect("loads");
    let cases = [
        String::new(),
        "arca_".to_string(),
        "arca_short".to_string(),
        format!("ARCA_{}", &secret[5..]),
        format!("{secret} "),
        format!("{secret}x"),
        format!(" {secret}"),
        format!("arca_{}+", &secret[5..48]),
        format!("arca_{}=", &secret[5..47]),
        secret[..secret.len() - 1].to_string(),
    ];
    for bad in &cases {
        assert_eq!(
            store.lookup(bad),
            None,
            "a malformed bearer must not authenticate: {bad:?}"
        );
    }
    assert!(
        store.lookup(&secret).is_some(),
        "the untouched secret still does"
    );
}

#[test]
fn an_unknown_secret_of_the_right_shape_authenticates_as_nobody() {
    let scratch = Scratch::new("unknown");
    mint_into(scratch.path(), "ops", Scope::Operator);
    let store = TokenStore::load(scratch.path()).expect("loads");
    assert_eq!(store.lookup(&unknown_secret()), None);
}

// Store refusals: each path, with its own failing test.

fn refused_with_mode(mode: u32) -> TokenStoreError {
    let scratch = Scratch::new("mode");
    write_hash_file(scratch.path(), "ops", &mint(Scope::Operator).1, mode);
    TokenStore::load(scratch.path()).expect_err("a store with a bad mode must not load")
}

#[test]
fn a_hash_file_with_mode_0440_is_refused_and_named() {
    let err = refused_with_mode(0o440);
    assert_eq!(
        err,
        TokenStoreError::BadMode {
            file: "ops.hash".into()
        }
    );
}

#[test]
fn a_hash_file_with_mode_0600_is_refused_and_named() {
    let err = refused_with_mode(0o600);
    assert_eq!(
        err,
        TokenStoreError::BadMode {
            file: "ops.hash".into()
        }
    );
}

#[test]
fn a_hash_file_with_mode_0644_is_refused_and_named() {
    let err = refused_with_mode(0o644);
    assert_eq!(
        err,
        TokenStoreError::BadMode {
            file: "ops.hash".into()
        }
    );
}

#[test]
fn a_hash_file_with_an_execute_bit_is_refused() {
    let err = refused_with_mode(0o500);
    assert_eq!(
        err,
        TokenStoreError::BadMode {
            file: "ops.hash".into()
        }
    );
}

#[test]
fn a_symlink_to_a_good_hash_file_is_refused_as_a_symlink() {
    let elsewhere = Scratch::new("elsewhere");
    let target = write_hash_file(elsewhere.path(), "real", &mint(Scope::Viewer).1, 0o400);
    let scratch = Scratch::new("symlink");
    symlink(&target, scratch.at("ops.hash")).expect("create the symlink");
    let err = TokenStore::load(scratch.path()).expect_err("a symlink must not load");
    assert_eq!(
        err,
        TokenStoreError::Symlink {
            file: "ops.hash".into()
        }
    );
}

#[test]
fn a_hash_file_owned_by_another_uid_is_refused_as_not_owner() {
    let scratch = Scratch::new("owner");
    mint_into(scratch.path(), "ops", Scope::Operator);
    let mine = own_uid(scratch.path());
    let foreign = mine.wrapping_add(1);
    let err = TokenStore::load_as(scratch.path(), foreign)
        .expect_err("a file of another uid must not load");
    assert_eq!(
        err,
        TokenStoreError::NotOwner {
            file: "ops.hash".into()
        }
    );
    assert!(
        TokenStore::load_as(scratch.path(), mine).is_ok(),
        "the same files load for their own uid"
    );
}

// The token directory itself: a real directory, owned by the server, writable by its owner only.

#[test]
fn a_token_directory_that_is_a_symlink_is_refused() {
    let elsewhere = Scratch::new("dirlink-target");
    mint_into(elsewhere.path(), "ops", Scope::Operator);
    let holder = Scratch::new("dirlink");
    let link = holder.at("tokens");
    symlink(elsewhere.path(), &link).expect("create the symlink");
    let err = TokenStore::load(&link).expect_err("a symlinked token directory must not load");
    assert_eq!(
        err,
        TokenStoreError::UnsafeDir {
            reason: "is a symlink"
        }
    );
}

#[test]
fn a_token_directory_writable_by_a_group_or_others_is_refused() {
    for mode in [0o775, 0o757, 0o777] {
        let scratch = Scratch::new("dirmode");
        mint_into(scratch.path(), "ops", Scope::Operator);
        fs::set_permissions(scratch.path(), Permissions::from_mode(mode))
            .expect("set the directory's mode");
        let err = TokenStore::load(scratch.path())
            .expect_err("a token directory others may write must not load");
        assert_eq!(
            err,
            TokenStoreError::UnsafeDir {
                reason: "is writable by a group or by others"
            },
            "mode {mode:o}"
        );
    }
}

#[test]
fn a_token_directory_only_its_owner_may_write_loads() {
    for mode in [0o755, 0o750, 0o700] {
        let scratch = Scratch::new("dirok");
        mint_into(scratch.path(), "ops", Scope::Operator);
        fs::set_permissions(scratch.path(), Permissions::from_mode(mode))
            .expect("set the directory's mode");
        assert!(
            TokenStore::load(scratch.path()).is_ok(),
            "mode {mode:o} is private enough"
        );
    }
}

#[test]
fn a_token_directory_owned_by_another_uid_is_refused() {
    // Only root can give a directory away, so the test uses `/`, which root owns. Run as root, the server owns `/`, and the
    // refusal cannot be shown; the test then says so and passes.
    let scratch = Scratch::new("dirowner");
    if own_uid(scratch.path()) == 0 {
        eprintln!("skipped: running as root, which owns `/`");
        return;
    }
    let err =
        TokenStore::load(Path::new("/")).expect_err("a directory another uid owns must not load");
    assert_eq!(
        err,
        TokenStoreError::UnsafeDir {
            reason: "is not owned by the server"
        }
    );
}

#[test]
fn a_directory_entry_that_is_not_a_hash_file_is_refused_as_malformed() {
    let scratch = Scratch::new("stray");
    mint_into(scratch.path(), "ops", Scope::Operator);
    write_named(scratch.path(), "notes.txt", &mint(Scope::Viewer).1, 0o400);
    let err = TokenStore::load(scratch.path()).expect_err("a stray file must not load silently");
    assert_eq!(
        err,
        TokenStoreError::Malformed {
            file: "notes.txt".into()
        }
    );
}

#[test]
fn a_subdirectory_named_like_a_hash_file_is_refused_as_malformed() {
    let scratch = Scratch::new("subdir");
    fs::create_dir(scratch.at("ops.hash")).expect("create the subdirectory");
    let err = TokenStore::load(scratch.path()).expect_err("a directory is not a hash file");
    assert_eq!(
        err,
        TokenStoreError::Malformed {
            file: "ops.hash".into()
        }
    );
}

#[test]
fn a_hash_name_that_is_not_a_lowercase_id_is_refused_as_malformed() {
    for name in ["Ops.hash", "-ops.hash", ".hash", "a b.hash", "ops_.hash"] {
        let scratch = Scratch::new("name");
        write_named(scratch.path(), name, &mint(Scope::Viewer).1, 0o400);
        let err =
            TokenStore::load(scratch.path()).expect_err("a name that is not an id must not load");
        assert!(
            matches!(err, TokenStoreError::Malformed { .. }),
            "{name}: {err:?}"
        );
    }
}

#[test]
fn a_hash_file_with_a_bad_body_is_refused_as_malformed() {
    let good = mint(Scope::Viewer).1;
    let digest = good.lines().nth(1).expect("a digest line").to_string();
    let upper = digest.to_uppercase();
    let cases: Vec<(&str, String)> = vec![
        ("empty", String::new()),
        ("no sha256 line", "scope=viewer\n".to_string()),
        ("lines swapped", format!("{digest}\nscope=viewer\n")),
        ("unknown scope", format!("scope=root\n{digest}\n")),
        ("scope with a space", format!("scope= viewer\n{digest}\n")),
        ("short digest", "scope=viewer\nsha256=abcd\n".to_string()),
        ("uppercase digest", format!("scope=viewer\n{upper}\n")),
        ("extra line", format!("scope=viewer\n{digest}\nextra=1\n")),
        ("no trailing newline", format!("scope=viewer\n{digest}")),
        (
            "crlf endings",
            format!("scope=viewer\r\n{}\r\n", digest.replace('\r', "")),
        ),
        ("oversized", "x".repeat(8192)),
    ];
    for (why, body) in cases {
        let scratch = Scratch::new("body");
        write_hash_file(scratch.path(), "bad", &body, 0o400);
        let err = TokenStore::load(scratch.path()).expect_err(why);
        assert_eq!(
            err,
            TokenStoreError::Malformed {
                file: "bad.hash".into()
            },
            "{why}"
        );
    }
}

#[test]
fn a_missing_directory_is_not_a_dir() {
    let scratch = Scratch::new("missing");
    let err = TokenStore::load(&scratch.at("absent")).expect_err("no directory, no store");
    assert_eq!(err, TokenStoreError::NotDir);
}

#[test]
fn a_regular_file_in_place_of_the_directory_is_not_a_dir() {
    let scratch = Scratch::new("filedir");
    let file = scratch.at("plain");
    fs::write(&file, "x").expect("write the file");
    let err = TokenStore::load(&file).expect_err("a file is not a token directory");
    assert_eq!(err, TokenStoreError::NotDir);
}

// Error text names a file and never its content.

#[test]
fn a_refusal_names_its_file_and_never_its_content() {
    let (secret, _) = mint(Scope::Operator);
    let scratch = Scratch::new("content");
    write_hash_file(
        scratch.path(),
        "leaky",
        &format!("scope=viewer\nsha256={secret}\n"),
        0o400,
    );
    let err = TokenStore::load(scratch.path()).expect_err("a malformed digest must not load");
    let shown = format!("{err} / {err:?}");
    assert!(
        shown.contains("leaky.hash"),
        "the error names its file: {shown}"
    );
    assert!(
        !shown.contains(&secret),
        "the error never carries the content"
    );
    assert!(!shown.contains(&secret[5..]), "not even part of it");
    assert_eq!(file_of(&err), Some("leaky.hash"));
}

#[test]
fn a_store_error_is_a_std_error() {
    fn assert_error<E: std::error::Error + Send + Sync + 'static>(_: &E) {}
    assert_error(&TokenStoreError::NotDir);
}

/// The child half of the output test: it prints what a refused load says, and does nothing unless it was started for that.
#[test]
fn child_prints_the_refusal() {
    let Ok(dir) = std::env::var("ARCA_MCP_CHILD_DIR") else {
        return;
    };
    match TokenStore::load(Path::new(&dir)) {
        Err(err) => {
            println!("refused: {err} / {err:?}");
            eprintln!("refused: {err} / {err:?}");
        }
        Ok(_) => println!("loaded"),
    }
}

#[test]
fn no_output_of_a_refused_load_carries_a_secret() {
    let (secret, _) = mint(Scope::Operator);
    let scratch = Scratch::new("output");
    write_hash_file(
        scratch.path(),
        "leaky",
        &format!("scope=operator\nsha256={secret}\n"),
        0o400,
    );
    let exe = std::env::current_exe().expect("the test binary's path");
    let out = Command::new(exe)
        .args([
            "--exact",
            "child_prints_the_refusal",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("ARCA_MCP_CHILD_DIR", scratch.path())
        .output()
        .expect("run the child test");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let both = format!("{stdout}\n{stderr}");
    assert!(
        both.contains("refused: "),
        "the child printed the refusal: {both}"
    );
    assert!(
        both.contains("leaky.hash"),
        "the refusal names its file: {both}"
    );
    assert!(!both.contains(&secret), "no output carries the secret");
    assert!(
        !both.contains(&secret[5..]),
        "no output carries any part of it"
    );
}

/// An older release let a token be minted under the name approved actions run under. Its file is left out: the
/// store loads, every other token works, the secret of the left-out one authenticates nobody, and the store names
/// the file so its loader can say so. The same holds when the file appears before a reload.
#[test]
fn a_file_named_for_approved_actions_is_left_out_and_every_other_token_still_loads() {
    let reserved = format!("{}.hash", arca_mcp::auth::APPROVED_ACTION_ID);
    let scratch = Scratch::new("reserved-id");
    let good = mint_into(scratch.path(), "d-good", Scope::Doctor);
    // The file is a whole, well-formed hash file: only its name sets it apart.
    let old = mint_into(scratch.path(), "d-old", Scope::Operator);
    fs::rename(scratch.at("d-old.hash"), scratch.at(&reserved)).expect("rename");
    let store = TokenStore::load(scratch.path()).expect("the store loads without the file");
    assert_eq!(store.ignored(), vec![reserved.clone()]);
    assert_eq!(
        store.lookup(&good).map(|caller| caller.token_id),
        Some("d-good".to_string())
    );
    assert!(store.lookup(&old).is_none(), "its secret is nobody's");
    assert_eq!(
        store.ids(),
        vec![("d-good".to_string(), Scope::Doctor, None)],
        "it is not listed as a token"
    );

    // Removed, then put back while the store is live: each reload says what it found.
    fs::remove_file(scratch.at(&reserved)).expect("remove");
    assert_eq!(store.reload(scratch.path()).expect("reload"), 1);
    assert_eq!(store.ignored(), Vec::<String>::new());
    let again = mint_into(scratch.path(), "d-again", Scope::Operator);
    fs::rename(scratch.at("d-again.hash"), scratch.at(&reserved)).expect("rename");
    assert_eq!(store.reload(scratch.path()).expect("reload"), 1);
    assert_eq!(store.ignored(), vec![reserved]);
    assert!(store.lookup(&again).is_none());
    assert!(store.lookup(&good).is_some());
}

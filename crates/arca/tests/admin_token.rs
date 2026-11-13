//! The `admin` token scope: it sits above `operator`, and one node holds one admin token at a time. Each test runs the built
//! binary against its own scratch token directory.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use arca_api::Scope;
use arca_mcp::TokenStore;

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .subsec_nanos();
        let dir =
            std::env::temp_dir().join(format!("arca-admin-{}-{nanos}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).expect("scratch");
        Scratch(dir)
    }

    fn at(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn hash_dir(scratch: &Scratch) -> PathBuf {
    let dir = scratch.at("tokens");
    fs::create_dir(&dir).expect("token directory");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    dir
}

fn mint(hash_dir: &Path, scope: &str, id: &str, out: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_arca"))
        .env("HOME", "/nonexistent/arca-test-home")
        .env_remove("ARCA_CONFIG")
        .args(["token", "mint", "--scope", scope, "--id", id, "--hash-dir"])
        .arg(hash_dir)
        .arg("--out")
        .arg(out)
        .args(extra)
        .output()
        .expect("run the binary")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn an_admin_token_mints_and_the_store_reads_it_as_admin_above_operator() {
    let scratch = Scratch::new("mint");
    let dir = hash_dir(&scratch);
    let out_file = scratch.at("a.secret");
    let out = mint(&dir, "admin", "admin-1", &out_file, &[]);
    assert!(out.status.success(), "the mint succeeds: {}", stderr(&out));
    let printed = String::from_utf8_lossy(&out.stdout);
    assert!(
        printed.contains("\"scope\":\"admin\"") || printed.contains("\"scope\": \"admin\""),
        "the printed line names the scope: {printed}"
    );
    let secret = fs::read_to_string(&out_file).expect("secret");
    let store = TokenStore::load(&dir).expect("the store loads an admin hash file");
    let caller = store.lookup(secret.trim_end()).expect("authenticates");
    assert_eq!(caller.scope, Scope::Admin);
    assert!(Scope::Admin > Scope::Operator, "admin is above operator");
}

#[test]
fn a_second_admin_id_is_refused_naming_the_first_and_writes_nothing() {
    let scratch = Scratch::new("second");
    let dir = hash_dir(&scratch);
    assert!(
        mint(&dir, "admin", "admin-1", &scratch.at("a.secret"), &[])
            .status
            .success()
    );
    let second = scratch.at("b.secret");
    let out = mint(&dir, "admin", "admin-2", &second, &[]);
    assert_eq!(out.status.code(), Some(2), "refused: {}", stderr(&out));
    let text = stderr(&out);
    assert!(
        text.contains("admin-1"),
        "it names the existing admin token: {text}"
    );
    assert!(
        text.contains("--replace"),
        "it says how to replace it: {text}"
    );
    assert!(
        !dir.join("admin-2.hash").exists(),
        "no hash file was written"
    );
    assert!(!second.exists(), "no secret file was written");
    assert!(
        dir.join("admin-1.hash").exists(),
        "the first admin token stays"
    );
}

#[test]
fn replace_retires_the_old_admin_token_and_mints_the_new_one() {
    let scratch = Scratch::new("replace");
    let dir = hash_dir(&scratch);
    let first_out = scratch.at("a.secret");
    assert!(
        mint(&dir, "admin", "admin-1", &first_out, &[])
            .status
            .success()
    );
    let old_secret = fs::read_to_string(&first_out).expect("secret");
    let new_out = scratch.at("b.secret");
    let out = mint(&dir, "admin", "admin-2", &new_out, &["--replace"]);
    assert!(out.status.success(), "replaced: {}", stderr(&out));
    assert!(
        !dir.join("admin-1.hash").exists(),
        "the old admin token is retired"
    );
    let store = TokenStore::load(&dir).expect("loads");
    assert!(
        store.lookup(old_secret.trim_end()).is_none(),
        "the old secret is dead"
    );
    let new_secret = fs::read_to_string(&new_out).expect("secret");
    assert_eq!(
        store.lookup(new_secret.trim_end()).expect("new").token_id,
        "admin-2"
    );
}

#[test]
fn replace_with_no_admin_token_just_mints_and_other_scopes_ignore_the_rule() {
    let scratch = Scratch::new("none");
    let dir = hash_dir(&scratch);
    assert!(
        mint(&dir, "operator", "op-1", &scratch.at("o.secret"), &[])
            .status
            .success()
    );
    // `--replace` with nothing to replace is no error.
    let out = mint(
        &dir,
        "admin",
        "admin-1",
        &scratch.at("a.secret"),
        &["--replace"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        dir.join("op-1.hash").exists(),
        "an operator token is never retired"
    );
    // A second operator token is fine beside the admin token.
    assert!(
        mint(&dir, "operator", "op-2", &scratch.at("o2.secret"), &[])
            .status
            .success()
    );
    // Re-minting the admin id itself is the existing id collision.
    let again = mint(
        &dir,
        "admin",
        "admin-1",
        &scratch.at("a2.secret"),
        &["--replace"],
    );
    assert!(
        !again.status.success(),
        "an id that exists is still refused"
    );
}

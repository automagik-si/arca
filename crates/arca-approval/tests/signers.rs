//! The signers file: the root-owned trust rule, the strict shape of its key lines, and the scratch-only injection. Every
//! file here is a temporary file the test wrote, owned by the test's own uid; none is the owner's.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use arca_approval::{ApprovalRefusal, TrustRoot, load_signers, signers_trust};
use common::{EdKey, SK_TYPE, Scratch, SkKey, owner_uid, write_mode};

const PROJECTS_OK: &[&str] = &["arcak-scratch-a"];

fn scratch_trust(dir: &Scratch) -> TrustRoot {
    TrustRoot::Scratch {
        uid: owner_uid(&dir.dir),
        up_to: dir.dir.clone(),
    }
}

fn untrusted(result: Result<impl std::fmt::Debug, ApprovalRefusal>) -> bool {
    matches!(result, Err(ApprovalRefusal::Untrusted(_)))
}

fn projects(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| n.to_string()).collect()
}

// ---- the root-owned rule ----------------------------------------------------------------------------------------------

#[test]
fn the_production_signers_path_is_root_owned_etc_and_names_no_test_file() {
    let (path, trust) = signers_trust(false, &[], None).expect("no injection is named");
    assert_eq!(path, PathBuf::from("/etc/arca/owner-signers"));
    assert_eq!(trust, TrustRoot::Root);
}

#[test]
fn the_root_rule_refuses_a_file_the_test_user_owns() {
    // The rule is the file's owner is uid 0. The test user owns every file it wrote, so the refusal is the rule working.
    let dir = Scratch::new("root-rule");
    let file = dir.path("owner-signers");
    write_mode(
        &file,
        SkKey::from_seed(7).line("arca-kernel-approve").as_bytes(),
        0o644,
    );
    assert!(untrusted(load_signers(&file, &TrustRoot::Root)));
}

#[test]
fn a_path_with_a_parent_component_is_refused() {
    let dir = Scratch::new("dotdot");
    let file = dir.path("owner-signers");
    write_mode(
        &file,
        SkKey::from_seed(7).line("arca-kernel-approve").as_bytes(),
        0o644,
    );
    let tricky = dir.dir.join("sub").join("..").join("owner-signers");
    assert!(untrusted(load_signers(&tricky, &scratch_trust(&dir))));
}

#[test]
fn a_symlinked_signers_file_is_refused() {
    let dir = Scratch::new("symlink-file");
    let real = dir.path("real-signers");
    write_mode(
        &real,
        SkKey::from_seed(7).line("arca-kernel-approve").as_bytes(),
        0o644,
    );
    let link = dir.path("owner-signers");
    std::os::unix::fs::symlink(&real, &link).expect("the symlink is made");
    assert!(untrusted(load_signers(&link, &scratch_trust(&dir))));
}

#[test]
fn a_group_writable_signers_file_is_refused() {
    let dir = Scratch::new("group-writable");
    let file = dir.path("owner-signers");
    write_mode(
        &file,
        SkKey::from_seed(7).line("arca-kernel-approve").as_bytes(),
        0o664,
    );
    assert!(untrusted(load_signers(&file, &scratch_trust(&dir))));
}

#[test]
fn a_world_writable_signers_file_is_refused() {
    let dir = Scratch::new("world-writable");
    let file = dir.path("owner-signers");
    write_mode(
        &file,
        SkKey::from_seed(7).line("arca-kernel-approve").as_bytes(),
        0o646,
    );
    assert!(untrusted(load_signers(&file, &scratch_trust(&dir))));
}

#[test]
fn a_group_writable_directory_above_the_file_is_refused() {
    let dir = Scratch::new("group-dir");
    let sub = dir.path("sub");
    fs::create_dir(&sub).expect("the subdirectory is made");
    fs::set_permissions(&sub, fs::Permissions::from_mode(0o770)).expect("mode is set");
    let file = sub.join("owner-signers");
    write_mode(
        &file,
        SkKey::from_seed(7).line("arca-kernel-approve").as_bytes(),
        0o644,
    );
    assert!(untrusted(load_signers(&file, &scratch_trust(&dir))));
}

#[test]
fn a_directory_above_the_file_that_the_trust_does_not_name_is_refused() {
    let dir = Scratch::new("unnamed-up-to");
    let other = Scratch::new("unnamed-other");
    let file = dir.path("owner-signers");
    write_mode(
        &file,
        SkKey::from_seed(7).line("arca-kernel-approve").as_bytes(),
        0o644,
    );
    let trust = TrustRoot::Scratch {
        uid: owner_uid(&dir.dir),
        up_to: other.dir.clone(),
    };
    assert!(untrusted(load_signers(&file, &trust)));
}

#[test]
fn a_signers_file_over_64_kib_is_refused() {
    let dir = Scratch::new("big");
    let file = dir.path("owner-signers");
    write_mode(&file, &vec![b'#'; 64 * 1024 + 1], 0o644);
    assert!(untrusted(load_signers(&file, &scratch_trust(&dir))));
}

// ---- the lines it keeps -----------------------------------------------------------------------------------------------

#[test]
fn an_sk_line_listing_the_arca_namespace_is_kept_with_its_fingerprint() {
    let dir = Scratch::new("keep");
    let key = SkKey::from_seed(7);
    let file = dir.path("owner-signers");
    write_mode(
        &file,
        format!("# the owner\n\n{}\n", key.line("arca-kernel-approve")).as_bytes(),
        0o644,
    );
    let lines = load_signers(&file, &scratch_trust(&dir)).expect("the file is trusted");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].key_type, SK_TYPE);
    assert_eq!(lines[0].public, key.blob());
    assert_eq!(lines[0].fingerprint, common::fingerprint(&key.blob()));
}

#[test]
fn a_line_listing_several_namespaces_is_kept_when_one_is_the_arca_namespace() {
    let dir = Scratch::new("keep-two");
    let key = SkKey::from_seed(7);
    let file = dir.path("owner-signers");
    write_mode(
        &file,
        format!("{}\n", key.line("droid-upgrade,arca-kernel-approve")).as_bytes(),
        0o644,
    );
    let lines = load_signers(&file, &scratch_trust(&dir)).expect("the file is trusted");
    assert_eq!(lines.len(), 1);
}

#[test]
fn a_line_for_droidds_arca_approve_namespace_alone_is_not_kept() {
    let dir = Scratch::new("droidd-ns");
    let key = SkKey::from_seed(7);
    let file = dir.path("owner-signers");
    write_mode(
        &file,
        format!("{}\n", key.line("arca-approve")).as_bytes(),
        0o644,
    );
    let lines = load_signers(&file, &scratch_trust(&dir)).expect("the file is trusted");
    assert!(
        lines.is_empty(),
        "a key listed only for droidd is not trusted here"
    );
}

#[test]
fn a_plain_ed25519_line_is_not_kept() {
    let dir = Scratch::new("plain");
    let key = EdKey::from_seed(7);
    let file = dir.path("owner-signers");
    write_mode(
        &file,
        format!("{}\n", key.line("arca-kernel-approve")).as_bytes(),
        0o644,
    );
    let lines = load_signers(&file, &scratch_trust(&dir)).expect("the file is trusted");
    assert!(lines.is_empty(), "only hardware keys are kept");
}

#[test]
fn a_line_whose_blob_is_not_the_hardware_key_it_names_refuses_the_file() {
    let dir = Scratch::new("mismatch");
    let plain = EdKey::from_seed(7);
    let file = dir.path("owner-signers");
    let line = format!(
        "owner namespaces=\"arca-kernel-approve\" {SK_TYPE} {} mismatch",
        base64_of(&plain.blob())
    );
    write_mode(&file, format!("{line}\n").as_bytes(), 0o644);
    assert!(untrusted(load_signers(&file, &scratch_trust(&dir))));
}

#[test]
fn a_line_that_is_not_the_template_shape_refuses_the_whole_file() {
    let dir = Scratch::new("shape");
    let key = SkKey::from_seed(7);
    let good = key.line("arca-kernel-approve");
    let cases = [
        format!("{good}\r"),
        format!("other namespaces=\"arca-kernel-approve\" {}", &good[6..]),
        format!(" {good}"),
        good.replace(
            "namespaces=\"arca-kernel-approve\"",
            "namespaces=\"arca-kernel-approve\" ",
        ),
        format!("{good}\u{7f}"),
    ];
    for (index, line) in cases.iter().enumerate() {
        let file = dir.path(&format!("owner-signers-{index}"));
        write_mode(&file, format!("{line}\n").as_bytes(), 0o644);
        assert!(
            untrusted(load_signers(&file, &scratch_trust(&dir))),
            "case {index} must refuse the file"
        );
    }
}

#[test]
fn a_comment_and_blank_lines_are_skipped_and_do_not_refuse() {
    let dir = Scratch::new("comments");
    let file = dir.path("owner-signers");
    write_mode(&file, b"# nothing here\n\n   \n\t# indented\n", 0o644);
    let lines = load_signers(&file, &scratch_trust(&dir)).expect("the file is trusted");
    assert!(lines.is_empty());
}

fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

// ---- the scratch-only injection ---------------------------------------------------------------------------------------

#[test]
fn the_injection_is_refused_when_the_kernel_is_not_scratch() {
    let named = PathBuf::from("/tmp/arca-approval-not-scratch/owner-signers");
    assert!(matches!(
        signers_trust(false, &projects(PROJECTS_OK), Some(named)),
        Err(ApprovalRefusal::Untrusted(_))
    ));
}

#[test]
fn the_injection_is_refused_when_a_project_lacks_the_scratch_prefix() {
    let named = PathBuf::from("/tmp/arca-approval-mixed/owner-signers");
    assert!(matches!(
        signers_trust(
            true,
            &projects(&["arcak-scratch-a", "arcak-svc"]),
            Some(named)
        ),
        Err(ApprovalRefusal::Untrusted(_))
    ));
}

#[test]
fn the_injection_is_refused_for_an_empty_project_list() {
    let named = PathBuf::from("/tmp/arca-approval-empty/owner-signers");
    assert!(matches!(
        signers_trust(true, &[], Some(named)),
        Err(ApprovalRefusal::Untrusted(_))
    ));
}

#[test]
fn the_injection_is_refused_for_a_bare_prefix_project() {
    let named = PathBuf::from("/tmp/arca-approval-bare/owner-signers");
    assert!(matches!(
        signers_trust(true, &projects(&["arcak-scratch-"]), Some(named)),
        Err(ApprovalRefusal::Untrusted(_))
    ));
}

#[test]
fn the_injection_is_refused_for_a_relative_path() {
    assert!(matches!(
        signers_trust(
            true,
            &projects(PROJECTS_OK),
            Some(PathBuf::from("owner-signers"))
        ),
        Err(ApprovalRefusal::Untrusted(_))
    ));
}

#[cfg(debug_assertions)]
#[test]
fn the_injection_is_honoured_for_a_scratch_kernel_in_a_debug_build() {
    let dir = Scratch::new("inject");
    let file = dir.path("owner-signers");
    let (path, trust) = signers_trust(true, &projects(PROJECTS_OK), Some(file.clone()))
        .expect("a scratch kernel's injection is honoured in a debug build");
    assert_eq!(path, file);
    assert!(matches!(trust, TrustRoot::Scratch { .. }));
}

#[cfg(not(debug_assertions))]
#[test]
fn the_injection_is_refused_in_a_build_that_cannot_honour_it() {
    let named = PathBuf::from("/tmp/arca-approval-release/owner-signers");
    assert!(matches!(
        signers_trust(true, &projects(PROJECTS_OK), Some(named)),
        Err(ApprovalRefusal::Untrusted(_))
    ));
}

#![cfg(feature = "write")]
use arca_incus::exec::{
    DashPort, ExecCommand as C, JournalLines, SecretName, ServiceFile, TailnetNode,
};
use serde_json::json;

#[test]
fn every_closed_step_has_the_exact_absolute_timeout_argv_and_request_body() {
    let rows = vec![
        (
            C::Journal(JournalLines::new(7).unwrap()),
            10,
            vec![
                "/usr/bin/journalctl",
                "--no-pager",
                "--quiet",
                "-o",
                "short-iso",
                "-n",
                "7",
            ],
        ),
        (
            C::TailscaleVersion,
            15,
            vec!["/usr/bin/tailscale", "version"],
        ),
        (
            C::StartDaemon,
            60,
            vec!["/usr/bin/systemctl", "start", "tailscaled"],
        ),
        (
            C::StopDaemon,
            30,
            vec!["/usr/bin/systemctl", "stop", "tailscaled"],
        ),
        (
            C::ClearKeyDir,
            30,
            vec!["/usr/bin/rm", "-rf", "--", "/run/arca-ts"],
        ),
        (
            C::MakeKeyDir,
            30,
            vec!["/usr/bin/mkdir", "-m", "0700", "--", "/run/arca-ts"],
        ),
        (
            C::Join(TailnetNode::new("arca-dash").unwrap()),
            120,
            vec![
                "/usr/bin/tailscale",
                "up",
                "--auth-key=file:/run/arca-ts/authkey",
                "--hostname=arca-dash",
                "--advertise-tags=tag:arca-svc",
                "--accept-routes=false",
                "--accept-dns=false",
                "--timeout=60s",
            ],
        ),
        (
            C::TailnetStatus,
            30,
            vec!["/usr/bin/tailscale", "status", "--json"],
        ),
        (
            C::ServeOn(DashPort::new(3080).unwrap()),
            30,
            vec![
                "/usr/bin/tailscale",
                "serve",
                "--bg",
                "--https=8443",
                "http://127.0.0.1:3080",
            ],
        ),
        (
            C::ServeStatus,
            30,
            vec!["/usr/bin/tailscale", "serve", "status", "--json"],
        ),
        (
            C::ServeOff,
            30,
            vec!["/usr/bin/tailscale", "serve", "--https=8443", "off"],
        ),
        (C::Logout, 60, vec!["/usr/bin/tailscale", "logout"]),
    ];
    for (cmd, seconds, argv) in rows {
        let mut expected = vec![
            "/usr/bin/timeout".to_string(),
            "-k".into(),
            "2".into(),
            seconds.to_string(),
        ];
        expected.extend(argv.into_iter().map(str::to_owned));
        assert_eq!(cmd.argv(), expected);
        assert_eq!(cmd.seconds(), seconds);
        assert_eq!(
            cmd.body(),
            json!({"command":expected,"environment":{},"wait-for-websocket":false,"record-output":true,"interactive":false,"width":0,"height":0,"user":0,"group":0,"cwd":""})
        );
    }
}
#[test]
fn closed_arguments_and_file_paths_refuse_injection_and_out_of_range_values() {
    for name in [
        "dash",
        "arca-",
        "arca-a;id",
        "arca-a\n",
        "arca-a/b",
        "arca-a?x=1",
        "arca-A",
    ] {
        assert!(TailnetNode::new(name).is_err());
    }
    for key in ["", "../key", "A/B", "A?x=1", "a\n", "A%2fB"] {
        assert!(SecretName::new(key).is_err());
    }
    assert!(JournalLines::new(0).is_err());
    assert!(JournalLines::new(201).is_err());
    assert!(DashPort::new(0).is_err());
    assert_eq!(ServiceFile::TailnetKey.path(), "/run/arca-ts/authkey");
    assert_eq!(
        ServiceFile::Secret(SecretName::new("TOKEN").unwrap()).path(),
        "/run/secrets/TOKEN"
    );
    // A virtual machine's secrets: one fixed directory in the guest, and a file in it named by the secret.
    assert_eq!(ServiceFile::GuestSecretsDir.path(), "/etc/arca-secrets");
    assert_eq!(
        ServiceFile::GuestSecret(SecretName::new("API_TOKEN").unwrap()).path(),
        "/etc/arca-secrets/API_TOKEN"
    );
}

//! The shipped `nats-primary` and `nats-replica` templates, and the replica's entrypoint script
//! (`templates/nats-replica.sh`) against a fake `nats-server`: no server runs, nothing reaches a registry and no second
//! node takes part. The creates from the owner's card are in the tenants suite (`tenants/nats_pair.rs`). Every address
//! and value is synthetic.
use super::nats::{Ran, conf_store, load, run_template_in, script_in, shells, shipped};
use super::*;

/// The public `nats:2.14.10-alpine` image, by the digest both templates pin, the `nats` template's own.
const NATS_DIGEST: &str = "sha256:311da47e2f31c26e99c623cd78d486a1e8ab039c49b32652770bb91a4f70512e";
const PASSWORD: &str = "Synthetic0Leaf.password_with~safe-chars";
/// The primary's leaf node port, at its address on the private link.
const REMOTE: &str = "10.20.30.7:7422";
const GIB: u64 = 1 << 30;

/// The URL the remote dials, with the user and the password in it. Made from its parts at run time, so no line of
/// the repository is a URL with a password in it.
fn remote_url(password: &str, remote: &str) -> String {
    format!("nats-leaf://{}@{remote}", ["leaf", password].join(":"))
}

fn run_in(shell: &[&str], env: &[(&str, &str)]) -> Ran {
    run_template_in("nats-replica", shell, env)
}

fn good() -> Vec<(&'static str, &'static str)> {
    vec![
        ("NATS_LEAF_PASSWORD", PASSWORD),
        ("NATS_LEAF_REMOTE", REMOTE),
        ("NATS_DOMAIN", "copy"),
    ]
}

/// A shipped template's file, as JSON.
fn file(name: &str) -> Value {
    serde_json::from_str(
        &fs::read_to_string(shipped().join(format!("{name}.json"))).expect("the template"),
    )
    .expect("JSON")
}

#[test]
fn the_primary_template_is_the_nats_template_with_its_leaf_port_offered_on_one_host_address() {
    let (mut nats, mut primary) = (file("nats"), file("nats-primary"));
    assert_eq!(primary["name"], "nats-primary");
    assert_eq!(primary["privatePort"], json!({ "port": 7422 }));
    // Every other key is the nats template's, value for value: the same image by digest, the same script and its last
    // word, the same ports, volume, secret, environment and link. Only the name, the words and the private port differ.
    for template in [&mut nats, &mut primary] {
        let object = template.as_object_mut().expect("an object");
        object.remove("name");
        object.remove("description");
    }
    primary
        .as_object_mut()
        .expect("an object")
        .remove("privatePort");
    assert_eq!(primary, nats);

    let template = load("nats-primary");
    assert_eq!(template.digest.as_deref(), Some(NATS_DIGEST));
    assert_eq!(
        template.private_port,
        Some(service::PrivatePort { port: 7422 })
    );
    // A link still opens 4222, the first port; the private port is the leaf node port and nothing else.
    assert_eq!(template.ports, vec![4222, 7422, 8222]);
    assert!(template.lan.is_none() && !template.publishable);
    // The proxy connects to 127.0.0.1:7422 inside the container, and the script's leaf listener is on every address of
    // the container, the loopback with them.
    let script = fs::read_to_string(shipped().join("nats.sh")).expect("nats.sh");
    assert!(script.contains(r#"listen: "0.0.0.0:7422""#));
    // The nats template itself has no private port, so a node whose config names no listen address creates it as it did.
    assert!(load("nats").private_port.is_none());
}

#[test]
fn the_replica_template_pins_the_same_nats_takes_both_secrets_from_the_owner_and_has_no_port_a_peer_dials()
 {
    let template = load("nats-replica");
    let nats = load("nats");
    assert_eq!(template.kind, "oci");
    assert_eq!(template.image, "docker.io/library/nats");
    assert_eq!(template.digest.as_deref(), Some(NATS_DIGEST));
    assert_eq!(template.digest, nats.digest);
    // Clients first, which a link opens, then monitoring. No leaf node port: the replica dials the primary, and nothing
    // dials the replica.
    assert_eq!(template.ports, vec![4222, 8222]);
    let data = template.data.as_ref().expect("a data volume");
    let of_nats = nats.data.as_ref().expect("nats has one");
    assert_eq!(
        (data.path.as_str(), data.size.as_deref()),
        (of_nats.path.as_str(), of_nats.size.as_deref())
    );
    // Both values are the primary's, so neither is made at create: the owner sets them.
    assert_eq!(
        template.secrets,
        vec!["NATS_LEAF_PASSWORD", "NATS_LEAF_REMOTE"]
    );
    assert!(template.generate.is_empty(), "{:?}", template.generate);
    assert_eq!(
        template.env["NATS_LEAF_PASSWORD"],
        "{{secret:NATS_LEAF_PASSWORD}}"
    );
    assert_eq!(
        template.env["NATS_LEAF_REMOTE"],
        "{{secret:NATS_LEAF_REMOTE}}"
    );
    assert_eq!(template.env["NATS_DOMAIN"], "{{name}}");
    assert_eq!(template.env.len(), 3);
    // A link gives what a link to nats gives, so a consumer on this node can be pointed at the replica.
    assert_eq!(template.provides, nats.provides);
    assert!(!template.publishable && template.lan.is_none() && template.private_port.is_none());
    // Created stopped until the owner has set both.
    let reason = service::template_stopped_reason(
        &template,
        "service-copy",
        &std::env::temp_dir().join(format!(
            "arca-nats-replica-no-secrets-{}",
            std::process::id()
        )),
    )
    .expect("created stopped");
    assert!(
        reason.contains("NATS_LEAF_PASSWORD") && reason.contains("NATS_LEAF_REMOTE"),
        "{reason}"
    );
}

#[test]
fn both_scripts_are_installed_with_their_templates_and_read_whole_into_their_entrypoints() {
    let dir = std::env::temp_dir().join(format!(
        "arca-nats-replica-templates-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    let report = arca::cmd_templates::install_into(&dir, false).expect("install");
    let installed: Vec<&str> = report["installed"]
        .as_array()
        .expect("installed")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for name in ["nats-primary.json", "nats-replica.json", "nats-replica.sh"] {
        assert!(installed.contains(&name), "{name}: {report}");
    }
    let line = |template: &str| {
        let loaded = service::load_template(&dir, template).expect("loads from the installed copy");
        service::entrypoint(&loaded, &dir)
            .expect("its script is installed with it")
            .expect("an entrypoint")
    };
    let script = |name: &str| {
        fs::read_to_string(dir.join(name))
            .expect("the installed script")
            .trim_end_matches('\n')
            .to_string()
    };
    assert_eq!(
        script_in(&line("nats-replica"), "nats-replica"),
        script("nats-replica.sh")
    );
    // The primary runs nats.sh, the line nats runs, to the byte.
    assert_eq!(line("nats-primary"), line("nats"));
    assert_eq!(script_in(&line("nats-primary"), "nats"), script("nats.sh"));
    fs::remove_dir_all(&dir).expect("the scratch directory goes");
}

#[test]
fn the_replica_script_writes_one_leaf_remote_to_the_primary_and_every_other_setting_under_each_shell()
 {
    let size = {
        let data = load("nats-replica").data.expect("a volume");
        data.size
            .as_deref()
            .and_then(|s| s.strip_suffix("GiB"))
            .map(|n| n.parse::<u64>().expect("a number") * GIB)
            .expect("a size in GiB")
    };
    let script = fs::read_to_string(shipped().join("nats-replica.sh")).expect("the script");
    assert!(script.contains("STORE=${ARCA_NATS_STORE_DIR:-/data/jetstream}"));
    assert!(script.contains("SERVER=${ARCA_NATS_SERVER:-/usr/local/bin/nats-server}"));
    for shell in shells() {
        let ran = run_in(&shell, &good());
        assert_eq!(ran.code, Some(0), "{shell:?}: {}", ran.err);
        let conf = ran.conf.expect("a configuration");
        let line = |text: &str| {
            assert!(
                conf.lines().any(|l| l.trim() == text),
                "{shell:?}: missing `{text}` in:\n{conf}"
            )
        };
        line(r#"server_name: "copy""#);
        line("port: 4222");
        line(r#"http: "0.0.0.0:8222""#);
        line("sync_interval: always");
        line(r#"domain: "copy""#);
        line(&format!(r#"store_dir: "{}""#, conf_store(&conf)));
        line("remotes: [");
        line(&format!(r#"url: "{}""#, remote_url(PASSWORD, REMOTE)));
        // One remote, inside the leafnodes block; no listener and no login of its own, so nothing dials a replica.
        assert_eq!(
            conf.lines()
                .filter(|l| l.trim_start().starts_with("url:"))
                .count(),
            1,
            "{conf}"
        );
        let leafnodes = conf.find("leafnodes {").expect("the leaf block");
        assert!(conf.find("remotes: [").expect("remotes") > leafnodes);
        assert!(!conf.contains("listen"), "{shell:?}: {conf}");
        assert!(!conf.contains("authorization"), "{shell:?}: {conf}");
        // The store stays under the volume, as nats's does.
        let max: u64 = conf
            .lines()
            .find_map(|l| l.trim().strip_prefix("max_file_store: "))
            .expect("max_file_store")
            .parse()
            .expect("plain bytes");
        assert!(max < size && max >= size / 2, "{max} of {size}");
        // The server is started on that file and nothing else.
        let args = ran.server_args.expect("the server was started");
        assert_eq!(args.lines().next(), Some("-c"));
        assert!(
            args.lines()
                .nth(1)
                .expect("the file")
                .ends_with("nats.conf"),
            "{args}"
        );
        assert_eq!(args.lines().count(), 2, "{args}");
        // The file holds the password, so only its owner reads it, and neither stream shows it.
        assert_eq!(ran.conf_mode, Some(0o600));
        assert!(!ran.out.contains(PASSWORD) && !ran.err.contains(PASSWORD));
        assert!(ran.store_made, "the store directory was made");
        assert!(
            !ran.used_a_decoy,
            "{shell:?}: NATS_CONF or NATS_STORE_DIR was read"
        );
    }
}

#[test]
fn the_replica_script_refuses_a_value_that_would_put_a_bad_character_in_the_file_or_another_place_in_the_url()
 {
    let with = |key: &str, value: &str| -> Vec<(String, String)> {
        good()
            .iter()
            .filter(|(k, _)| *k != key)
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .chain((!value.is_empty()).then(|| (key.to_string(), value.to_string())))
            .collect()
    };
    let run = |shell: &[&str], env: &[(String, String)]| {
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        run_in(shell, &env)
    };
    for shell in shells() {
        let refused = |env: Vec<(String, String)>, words: &str| {
            let ran = run(&shell, &env);
            assert_ne!(ran.code, Some(0), "{shell:?}: {env:?}");
            assert!(ran.err.contains(words), "{shell:?}: {words}: {}", ran.err);
            assert!(
                ran.conf.is_none() && ran.server_args.is_none(),
                "nothing is written or started: {env:?}"
            );
        };
        refused(
            with("NATS_LEAF_PASSWORD", ""),
            "NATS_LEAF_PASSWORD is not set",
        );
        for bad in [
            "has space.",
            "quote\"here",
            "a@b:c",
            "dollar$x",
            "slash/x",
            "brace}",
        ] {
            refused(
                with("NATS_LEAF_PASSWORD", bad),
                "NATS_LEAF_PASSWORD must be letters",
            );
        }
        refused(with("NATS_LEAF_REMOTE", ""), "NATS_LEAF_REMOTE is not set");
        for bad in [
            "10.20.30.7",
            "10.20.30.7:",
            ":7422",
            "10.20.30.7:0",
            "10.20.30.7:07422",
            "10.20.30.7:65536",
            "10.20.30.7:123456",
            "10.20.30.7:74x2",
            "10.20.30.7:7422:7423",
            "10.20.30.7:7422,10.20.30.8:7422",
            "nats-leaf://10.20.30.7:7422",
            "someone@10.20.30.7:7422",
            "10.20.30.7 :7422",
            "10.20.30.7:7422\"",
            "-x:7422",
            ".x:7422",
        ] {
            refused(
                with("NATS_LEAF_REMOTE", bad),
                "NATS_LEAF_REMOTE must be ADDRESS:PORT",
            );
        }
        refused(with("NATS_DOMAIN", ""), "NATS_DOMAIN is not set");
        for bad in ["Upper", "dot.ted", "-lead", "a b", "x\"y"] {
            refused(with("NATS_DOMAIN", bad), "NATS_DOMAIN must be");
        }
        // The edges of what is taken: the lowest and the highest port, and a host name.
        for remote in [
            "10.20.30.7:1",
            "10.20.30.7:65535",
            "primary-link.example:7422",
        ] {
            let ran = run(&shell, &with("NATS_LEAF_REMOTE", remote));
            assert_eq!(ran.code, Some(0), "{shell:?}: {remote}: {}", ran.err);
            let conf = ran.conf.expect("a configuration");
            assert!(
                conf.lines()
                    .any(|l| l.trim() == format!(r#"url: "{}""#, remote_url(PASSWORD, remote))),
                "{conf}"
            );
        }
    }
}

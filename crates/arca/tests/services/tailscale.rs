//! The shipped `tailscale` connector template and its entrypoint script (`templates/tailscale.sh`), on the service model
//! against the fake Incus and a fake `containerboot`: no node is made and nothing reaches Tailscale. The auth key, the
//! target and every address are synthetic.
use super::*;
use std::process::{Command, Stdio};

const TEMPLATE: &str = "tailscale";
/// The public `tailscale/tailscale` image, release 1.104.7, by the digest of its multi-platform index. Moving it is a
/// change of this test.
const TS_DIGEST: &str = "sha256:72c1e19e0aedca96b8533bbb0b4368564410ff20b17e53b804eb6c1f04a31be4";
/// Shaped like a Tailscale auth key and synthetic.
const KEY: &str = "tskey-auth-synthetic000-SyntheticKeyBodyNotARealOne0123";
/// Where port 443 goes: a linked service's address and port, from the documentation range.
const TARGET: &str = "192.0.2.11:8443";

fn shipped() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates")
}
fn op(rig: &Rig, verb: &str, args: Value) -> Result<Value, CallError> {
    rig.handlers[verb].call(
        args,
        &Caller {
            token_id: "operator".into(),
            scope: Scope::Operator,
            tools: None,
        },
    )
}
fn instance(rig: &Rig, name: &str) -> Value {
    rig.get(&format!("/1.0/instances/{name}?project={PROJECT}"))
        .expect("instance")
}
fn acl(rig: &Rig, name: &str) -> Value {
    rig.get(&format!(
        "/1.0/network-acls/{PROJECT}-svc-{name}?project=default"
    ))
    .expect("acl")
}
/// A node with a storage pool, which the node's state volume needs, the shipped templates installed as `arca templates
/// install` puts them, two plain services the connector could reach, and the connector `edge-ts`.
fn ts_rig() -> Rig {
    let rig = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![],
            networks: vec![],
            pools: vec![("pool1".into(), json!({}))],
        },
        vec![PROJECT.to_string()],
        Some("192.0.2.0/24"),
    );
    arca::cmd_templates::install_into(&rig.dir.join("templates"), false).expect("install");
    for (name, port) in [("web", 8443), ("other", 9090)] {
        rig.template(
            name,
            json!({"name":name,"kind":"oci","image":"docker.io/library/example","digest":DIGEST,"ports":[port],"env":{}}),
        );
        rig.call("service_create", json!({"name":name,"template":name}))
            .expect("create");
    }
    rig.call(
        "service_create",
        json!({"name":"edge-ts","template":TEMPLATE}),
    )
    .expect("create the connector");
    rig
}
/// `instance_start` as `arca serve` builds it.
fn start(rig: &Rig, name: &str) -> Result<Value, CallError> {
    let kernel = Arc::clone(&rig.kernel);
    let lifecycle = Arc::new(arca::lifecycle::InstanceLifecycle::new(
        rig.fake.socket().to_path_buf(),
        arca_incus::ProjectFamily::new([PROJECT], ["other"]).expect("the family"),
        move || {
            kernel
                .lock()
                .map_err(|_| "kernel".to_string())?
                .retained_entries()
                .map_err(|error| error.to_string())
        },
        &rig.dir,
    ));
    let life: HashMap<&'static str, Arc<dyn ToolHandler>> =
        arca::lifecycle::handlers(Arc::clone(&rig.kernel), Arc::new(|| NOW), Some(lifecycle))
            .into_iter()
            .collect();
    life["instance_start"].call(json!({"project":PROJECT,"instance":name}), &doctor())
}

#[test]
fn the_shipped_tailscale_template_loads_pinned_by_digest_with_no_port_a_state_volume_and_no_key() {
    let template = service::load_template(&shipped(), TEMPLATE).expect("the template loads");
    assert_eq!(template.kind, "oci");
    assert_eq!(template.image, "docker.io/tailscale/tailscale");
    assert_eq!(template.digest.as_deref(), Some(TS_DIGEST));
    // Incus pulls the image by that digest and by no tag.
    let source = service::source(&template).expect("source");
    assert_eq!(source["protocol"], "oci");
    assert_eq!(source["server"], "https://docker.io");
    assert_eq!(source["alias"], format!("tailscale/tailscale@{TS_DIGEST}"));
    // The version the digest is of is in the description.
    assert!(
        template
            .description
            .as_deref()
            .unwrap()
            .contains("Tailscale 1.104.7")
    );
    // No port of its own, no host listener, no management port, no publication, no kernel access, no privilege.
    assert!(template.ports.is_empty());
    assert!(
        template.lan.is_none()
            && template.droidd_port.is_none()
            && template.dash_port.is_none()
            && template.private_port.is_none()
            && template.kernel_access.is_none()
    );
    assert!(!template.publishable && template.provides.is_none() && !template.edge);
    assert!(template.raw_lxc.is_empty());
    assert!(template.autostart);
    // The node's state lives on the service's own data volume, so it logs in once.
    let data = template.data.as_ref().expect("a state volume");
    assert_eq!(data.path, "/data");
    // Its script, then the image's own containerboot.
    let line = service::entrypoint(&template, &shipped())
        .unwrap()
        .expect("an entrypoint");
    assert!(
        line.starts_with("/bin/sh -ec '") && line.ends_with("' tailscale"),
        "{line}"
    );
    // The key and the target are the owner's secrets: declared, never generated, in the template only as placeholders.
    assert_eq!(
        template.secrets,
        vec!["TS_AUTHKEY".to_string(), "TS_TARGET".to_string()]
    );
    assert!(template.generate.is_empty());
    assert_eq!(
        template.env,
        std::collections::BTreeMap::from(
            [
                ("TS_AUTHKEY", "{{secret:TS_AUTHKEY}}"),
                ("TS_TARGET", "{{secret:TS_TARGET}}"),
                ("TS_HOSTNAME", "{{name}}"),
                ("TS_STATE_DIR", "/data"),
                ("TS_USERSPACE", "true"),
                ("TS_AUTH_ONCE", "true"),
            ]
            .map(|(key, value)| (key.to_string(), value.to_string()))
        )
    );
}

#[test]
fn a_connector_is_an_unprivileged_container_with_its_state_volume_and_no_port_and_waits_for_its_key()
 {
    let rig = ts_rig();
    let status = rig
        .call("service_status", json!({"name":"edge-ts"}))
        .expect("status");
    assert_eq!(status["template"], TEMPLATE);
    assert_eq!(status["ports"], json!([]));
    assert_eq!(status["lanPort"], Value::Null);
    assert_eq!(status["published"], Value::Null);
    // Without its key and its target it is created and not started.
    assert_eq!(status["state"], "stopped");
    assert!(!rig.service_config("edge-ts").start);
    let live = instance(&rig, "edge-ts");
    assert_eq!(live["type"], "container");
    assert_eq!(live["status"], "Stopped");
    let config = live["config"].as_object().unwrap();
    assert!(config.get(service::PORTS_KEY).is_none());
    assert!(config.get("environment.TS_AUTHKEY").is_none());
    assert!(config.get("environment.TS_TARGET").is_none());
    // Userspace networking: no capability, no privilege, no tun device.
    assert!(config.keys().all(|key| !key.starts_with("security.")));
    assert!(config.get("raw.lxc").is_none());
    assert_eq!(config["environment.TS_USERSPACE"], "true");
    assert_eq!(config["environment.TS_AUTH_ONCE"], "true");
    assert_eq!(config["environment.TS_STATE_DIR"], "/data");
    // The tailnet name is the service's name.
    assert_eq!(config["environment.TS_HOSTNAME"], "edge-ts");
    // Its devices are a root disk, its one NIC and its state volume, there before the first start: no proxy, so
    // nothing listens on the host, and no unix-char device, so no tun.
    let devices = live["devices"].as_object().unwrap();
    assert_eq!(
        devices.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["data", "eth0", "root"]
    );
    assert_eq!(devices["data"]["type"], "disk");
    assert_eq!(devices["data"]["path"], "/data");
    // Nothing may connect to it.
    assert_eq!(acl(&rig, "edge-ts")["ingress"], json!([]));
}

#[test]
fn the_key_and_the_target_are_set_like_a_tunnel_token_and_the_connector_starts_once_both_are() {
    let rig = ts_rig();
    // A secret its template does not declare is refused: nothing else reaches containerboot's environment.
    for key in [
        "TS_EXTRA_ARGS",
        "TS_TAILSCALED_EXTRA_ARGS",
        "TS_SERVE_CONFIG",
    ] {
        assert!(
            op(
                &rig,
                "service_secret_set",
                json!({"name":"edge-ts","key":key,"value":"--synthetic-flag"}),
            )
            .is_err(),
            "{key}"
        );
    }
    // A value with a line break at its end would reach the program; it is refused.
    assert!(
        op(
            &rig,
            "service_secret_set",
            json!({"name":"edge-ts","key":"TS_AUTHKEY","value":format!("{KEY}\n")}),
        )
        .is_err()
    );
    let answer = op(
        &rig,
        "service_secret_set",
        json!({"name":"edge-ts","key":"TS_AUTHKEY","value":KEY}),
    )
    .expect("set the key");
    assert_eq!(answer, json!({"ok":true}));
    // With the key alone it still waits for its target.
    assert_eq!(
        rig.call("service_status", json!({"name":"edge-ts"}))
            .unwrap()["state"],
        "stopped"
    );
    op(
        &rig,
        "service_secret_set",
        json!({"name":"edge-ts","key":"TS_TARGET","value":TARGET}),
    )
    .expect("set the target");
    // The record names a secret file for each. It never holds the value.
    let record = rig.service_config("edge-ts");
    for key in ["TS_AUTHKEY", "TS_TARGET"] {
        for reference in [&record.secret_refs[key], &record.environment[key]] {
            assert!(
                reference.starts_with("arca-secret:service-edge-ts/"),
                "{key}"
            );
        }
    }
    for file in ["desired.json", "journal.jsonl"] {
        if let Ok(text) = fs::read_to_string(rig.dir.join(file)) {
            assert!(!text.contains(KEY), "{file}");
        }
    }
    for (verb, args) in [
        ("service_status", json!({"name":"edge-ts"})),
        ("service_list", json!({})),
    ] {
        assert!(!rig.call(verb, args).unwrap().to_string().contains(KEY));
    }
    // A log line that carries it is answered without it.
    rig.fake
        .set_console_log(PROJECT, "edge-ts", &format!("authkey {KEY} rejected"));
    let logs = rig
        .call("service_logs", json!({"name":"edge-ts"}))
        .expect("logs")
        .to_string();
    assert!(!logs.contains(KEY) && logs.contains("[secret]"));
    // In Incus the one key that holds it is the container's environment, which is how containerboot reads it.
    let live = instance(&rig, "edge-ts");
    for (key, value) in live["config"].as_object().unwrap() {
        assert_eq!(
            value.to_string().contains(KEY),
            key == "environment.TS_AUTHKEY",
            "{key}"
        );
    }
    assert_eq!(live["config"]["environment.TS_TARGET"], TARGET);
    assert!(!live["devices"].to_string().contains(KEY));
    // instance_start starts it, with its state volume still in place.
    start(&rig, "edge-ts").expect("the connector starts");
    let live = instance(&rig, "edge-ts");
    assert_eq!(live["status"], "Running");
    assert_eq!(live["devices"]["data"]["path"], "/data");
    assert_eq!(
        rig.call("service_status", json!({"name":"edge-ts"}))
            .unwrap()["state"],
        "running"
    );
    // The shipped files never change.
    for file in ["tailscale.json", "tailscale.sh"] {
        assert!(
            !fs::read_to_string(rig.dir.join("templates").join(file))
                .unwrap()
                .contains(KEY),
            "{file}"
        );
    }
}

#[test]
fn a_link_from_the_connector_opens_the_linked_services_port_to_it_and_nothing_else() {
    let rig = ts_rig();
    let address = |name: &str| {
        instance(&rig, name)["config"][service::ADDRESS_KEY]
            .as_str()
            .unwrap()
            .to_string()
    };
    for name in ["web", "other", "edge-ts"] {
        assert_eq!(acl(&rig, name)["ingress"], json!([]), "{name}");
    }
    assert_eq!(
        op(
            &rig,
            "service_connect",
            json!({"from":"edge-ts","to":"web"})
        )
        .expect("connect"),
        json!({"env":[]})
    );
    // The linked service admits the connector's address on its one port, over TCP.
    let web = acl(&rig, "web");
    let rules = web["ingress"].as_array().unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0]["action"], "allow");
    assert_eq!(rules[0]["source"], format!("{}/32", address("edge-ts")));
    assert_eq!(rules[0]["protocol"], "tcp");
    assert_eq!(rules[0]["destination_port"], "8443");
    // The service it is not linked to still admits nothing, and nothing may connect to the connector.
    assert_eq!(acl(&rig, "other")["ingress"], json!([]));
    assert_eq!(acl(&rig, "edge-ts")["ingress"], json!([]));
    for name in ["web", "other", "edge-ts"] {
        let nic = &instance(&rig, name)["devices"]["eth0"];
        assert_eq!(nic["security.acls.default.ingress.action"], "reject");
        assert_eq!(nic["security.acls.default.egress.action"], "reject");
    }
    assert_eq!(
        rig.call("service_status", json!({"name":"edge-ts"}))
            .unwrap()["links"],
        json!([{"to":"web","port":8443}])
    );
    // The link changed no egress: the connector's is the one every service has, ending in the internet rule that names
    // no protocol and no port, so Tailscale's control plane and DERP (tcp 443), STUN (udp 3478) and WireGuard (udp
    // 41641) to public addresses pass it. Every private range before it stays closed.
    let egress = acl(&rig, "edge-ts")["egress"].clone();
    assert_eq!(egress, acl(&rig, "other")["egress"]);
    let internet = egress.as_array().unwrap().last().unwrap();
    assert_eq!(internet["action"], "allow");
    for field in ["destination", "protocol", "destination_port"] {
        assert_eq!(internet[field], "", "{field}");
    }
    assert!(
        egress
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .skip(1)
            .all(|rule| rule["action"] == "reject")
    );
}

/// What one run of `tailscale.sh` left: its exit code, what it printed, the serve config the fake containerboot found,
/// and the environment it was started with.
struct Ran {
    code: Option<i32>,
    err: String,
    serve: Option<String>,
    boot_env: Option<String>,
}

/// `tailscale.sh` as the template names it (`<shell> -ec <script> tailscale`), with `env` as its environment and a fake
/// `containerboot` that records the serve config it is pointed at and its own environment.
fn run_in(shell: &[&str], env: &[(&str, &str)]) -> Ran {
    let template = service::load_template(&shipped(), TEMPLATE).expect("the template");
    let parts = template.entrypoint.expect("an entrypoint");
    let file = parts[2]["file"].as_str().expect("the script's file");
    let script = fs::read_to_string(shipped().join(file)).expect("the script");
    let tail: Vec<&str> = parts.iter().filter_map(Value::as_str).skip(2).collect();
    static RUNS: AtomicU64 = AtomicU64::new(0);
    let work = std::env::temp_dir().join(format!(
        "arca-ts-script-{}-{}",
        std::process::id(),
        RUNS.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&work).expect("a work dir");
    let (serve_dir, seen, boot_env) = (work.join("serve"), work.join("seen"), work.join("env"));
    let boot = work.join("containerboot");
    fs::write(
        &boot,
        format!(
            "#!/bin/sh\ncat \"$TS_SERVE_CONFIG\" > '{}'\nenv | sort > '{}'\n",
            seen.display(),
            boot_env.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&boot, fs::Permissions::from_mode(0o755)).unwrap();
    let mut command = Command::new(shell[0]);
    command
        .args(&shell[1..])
        .args(["-ec", script.trim_end_matches('\n')])
        .args(&tail)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("ARCA_TS_SERVE_DIR", &serve_dir)
        .env("ARCA_TS_CONTAINERBOOT", &boot)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().expect("the shell runs");
    let ran = Ran {
        code: output.status.code(),
        err: String::from_utf8_lossy(&output.stderr).into_owned(),
        serve: fs::read_to_string(&seen).ok(),
        boot_env: fs::read_to_string(&boot_env).ok(),
    };
    fs::remove_dir_all(&work).expect("the work dir goes");
    ran
}

fn good() -> Vec<(&'static str, &'static str)> {
    vec![
        ("TS_AUTHKEY", KEY),
        ("TS_TARGET", TARGET),
        ("TS_HOSTNAME", "edge-ts"),
        ("TS_STATE_DIR", "/data"),
        ("TS_USERSPACE", "true"),
        ("TS_AUTH_ONCE", "true"),
    ]
}

#[test]
fn the_script_forwards_tcp_443_to_the_target_with_a_proxy_v2_header_and_execs_containerboot_under_each_shell()
 {
    for shell in super::nats::shells() {
        let ran = run_in(&shell, &good());
        assert_eq!(ran.code, Some(0), "{shell:?}: {}", ran.err);
        // One TCP forward on 443, TLS untouched (no TerminateTLS), PROXY protocol v2, and nothing else served.
        let serve: Value = serde_json::from_str(ran.serve.as_deref().expect("a serve config"))
            .expect("the serve config is JSON");
        assert_eq!(
            serve,
            json!({"TCP":{"443":{"TCPForward":TARGET,"ProxyProtocol":2}}}),
            "{shell:?}"
        );
        // containerboot gets the template's environment as it is, and the serve config's path.
        let env = ran.boot_env.expect("containerboot ran");
        for line in [
            format!("TS_AUTHKEY={KEY}"),
            "TS_HOSTNAME=edge-ts".to_string(),
            "TS_STATE_DIR=/data".to_string(),
            "TS_USERSPACE=true".to_string(),
            "TS_AUTH_ONCE=true".to_string(),
        ] {
            assert!(env.lines().any(|l| l == line), "{shell:?}: {line}");
        }
        assert!(
            env.lines()
                .any(|l| l.starts_with("TS_SERVE_CONFIG=") && l.ends_with("/serve/serve.json")),
            "{shell:?}"
        );
        // The key is never printed.
        assert!(!ran.err.contains(KEY), "{shell:?}");
    }
}

#[test]
fn the_script_refuses_a_target_that_is_not_one_address_and_port_and_starts_nothing() {
    for target in [
        "192.0.2.11",
        "192.0.2.11:",
        "192.0.2.11:0",
        "192.0.2.11:08443",
        "192.0.2.11:65536",
        "192.0.2.11:8443:1",
        ":8443",
        "192.0.2.11:84a3",
        "192.0.2.11:8443\"}",
        "192.0.2.11 :8443",
        "unix:/var/run/tailscale/tailscaled.sock",
        // A tailnet name before the target that is no DNS label, or a second @.
        "@192.0.2.11:8443",
        "-edge@192.0.2.11:8443",
        "edge-@192.0.2.11:8443",
        "Edge@192.0.2.11:8443",
        "edge_a@192.0.2.11:8443",
        "a@b@192.0.2.11:8443",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa@192.0.2.11:8443",
    ] {
        let mut env = good();
        env.retain(|(key, _)| *key != "TS_TARGET");
        env.push(("TS_TARGET", target));
        let ran = run_in(&["/bin/sh"], &env);
        assert_eq!(ran.code, Some(64), "{target:?}: {}", ran.err);
        assert!(ran.err.contains("TS_TARGET"), "{target:?}");
        assert!(ran.serve.is_none() && ran.boot_env.is_none(), "{target:?}");
    }
    // With no target at all it says what to set.
    let mut env = good();
    env.retain(|(key, _)| *key != "TS_TARGET");
    let ran = run_in(&["/bin/sh"], &env);
    assert_ne!(ran.code, Some(0));
    assert!(ran.err.contains("TS_TARGET is not set"), "{}", ran.err);
    assert!(ran.boot_env.is_none());
    // An empty target is no target.
    let mut env = good();
    env.retain(|(key, _)| *key != "TS_TARGET");
    env.push(("TS_TARGET", ""));
    let ran = run_in(&["/bin/sh"], &env);
    assert_ne!(ran.code, Some(0));
    assert!(ran.err.contains("TS_TARGET is not set"), "{}", ran.err);
    assert!(ran.boot_env.is_none());
    // A host name in place of the address is a target too.
    let mut env = good();
    env.retain(|(key, _)| *key != "TS_TARGET");
    env.push(("TS_TARGET", "web.internal:8443"));
    assert_eq!(run_in(&["/bin/sh"], &env).code, Some(0));
}

#[test]
fn a_name_before_the_target_is_the_nodes_tailnet_name_in_place_of_the_services_own() {
    for shell in super::nats::shells() {
        // Without one the node keeps the service's name, which the template gives.
        let ran = run_in(&shell, &good());
        let env = ran.boot_env.expect("containerboot ran");
        assert!(env.lines().any(|l| l == "TS_HOSTNAME=edge-ts"), "{shell:?}");
        // With one, that name, and the forward goes to the target after the @ alone.
        let longest = "a".repeat(63);
        for (name, target) in [
            ("app-1", "app-1@192.0.2.11:8443".to_string()),
            ("a", "a@192.0.2.11:8443".to_string()),
            (longest.as_str(), format!("{longest}@192.0.2.11:8443")),
        ] {
            let mut env = good();
            env.retain(|(key, _)| *key != "TS_TARGET");
            env.push(("TS_TARGET", target.as_str()));
            let ran = run_in(&shell, &env);
            assert_eq!(ran.code, Some(0), "{shell:?} {target}: {}", ran.err);
            let boot = ran.boot_env.expect("containerboot ran");
            assert!(
                boot.lines().any(|l| l == format!("TS_HOSTNAME={name}")),
                "{shell:?} {target}"
            );
            let serve: Value = serde_json::from_str(ran.serve.as_deref().unwrap()).unwrap();
            assert_eq!(
                serve,
                json!({"TCP":{"443":{"TCPForward":TARGET,"ProxyProtocol":2}}}),
                "{shell:?} {target}"
            );
        }
    }
}

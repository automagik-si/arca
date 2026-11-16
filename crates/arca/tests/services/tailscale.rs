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
            limits: Default::default(),
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
    assert_eq!(template.optional_secrets, template.secrets);
    assert_eq!(template.connector.as_deref(), Some("tailscale"));
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
fn a_connector_is_an_unprivileged_container_with_its_state_volume_and_no_port_and_may_start_without_a_key()
 {
    let rig = ts_rig();
    let status = rig
        .call("service_status", json!({"name":"edge-ts"}))
        .expect("status");
    assert_eq!(status["template"], TEMPLATE);
    assert_eq!(status["ports"], json!([]));
    assert_eq!(status["lanPort"], Value::Null);
    assert_eq!(status["published"], Value::Null);
    // Optional secrets are absent; the template does not stop a keyless connector.
    assert_eq!(status["state"], "running");
    assert!(rig.service_config("edge-ts").start);
    let live = instance(&rig, "edge-ts");
    assert_eq!(live["type"], "container");
    assert_eq!(live["status"], "Running");
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
fn the_key_and_the_target_are_optional_overrides_and_keep_their_secret_handling() {
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
    // Setting the key keeps the connector startable without a target override.
    assert_eq!(
        rig.call("service_status", json!({"name":"edge-ts"}))
            .unwrap()["state"],
        "running"
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
    /// The key file containerboot was pointed at, as it found it: its content and its mode.
    key_file: Option<(String, u32)>,
}

/// `tailscale.sh` as the template names it (`<shell> -ec <script> tailscale`), with `env` as its environment and a fake
/// `containerboot` that records the serve config it is pointed at, its own environment and the key file it is given.
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
    let (key_dir, key_seen, key_mode) = (
        work.join("key"),
        work.join("key-seen"),
        work.join("key-mode"),
    );
    let boot = work.join("containerboot");
    fs::write(
        &boot,
        format!(
            "#!/bin/sh\ncat \"$TS_SERVE_CONFIG\" > '{}'\nenv | sort > '{}'\ncase \"$TS_AUTHKEY\" in file:*) [ -f \"${{TS_AUTHKEY#file:}}\" ] && {{ cat \"${{TS_AUTHKEY#file:}}\" > '{}'; stat -c %a \"${{TS_AUTHKEY#file:}}\" > '{}'; }} ;; esac\n",
            seen.display(),
            boot_env.display(),
            key_seen.display(),
            key_mode.display()
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
        .env("ARCA_TS_KEY_DIR", &key_dir)
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
        key_file: fs::read_to_string(&key_seen).ok().map(|text| {
            let mode = fs::read_to_string(&key_mode).expect("the key file's mode");
            (
                text,
                u32::from_str_radix(mode.trim(), 8).expect("an octal mode"),
            )
        }),
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
        // containerboot gets the template's environment as it is, the serve config's path, and the key by file: a file
        // of its own, mode 600, that `tailscale up` reads, so the key is on no command line and in no environment.
        let env = ran.boot_env.expect("containerboot ran");
        assert!(!env.contains(KEY), "{shell:?}");
        assert!(
            env.lines()
                .any(|l| l.starts_with("TS_AUTHKEY=file:") && l.ends_with("/key/authkey")),
            "{shell:?}"
        );
        assert_eq!(ran.key_file, Some((format!("{KEY}\n"), 0o600)), "{shell:?}");
        for line in [
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
        // tailscaled reads a target that starts with unix: as a socket path.
        "unix:443",
        "edge@unix:443",
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
    assert_eq!(ran.code, Some(64));
    assert!(
        ran.err.contains("TS_TARGET") && ran.err.contains("ARCA_EDGE_TARGET"),
        "{}",
        ran.err
    );
    assert!(ran.boot_env.is_none());
    // An empty target is no target.
    let mut env = good();
    env.retain(|(key, _)| *key != "TS_TARGET");
    env.push(("TS_TARGET", ""));
    let ran = run_in(&["/bin/sh"], &env);
    assert_eq!(ran.code, Some(64));
    assert!(
        ran.err.contains("TS_TARGET") && ran.err.contains("ARCA_EDGE_TARGET"),
        "{}",
        ran.err
    );
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
            env.push(("ARCA_EDGE_TARGET", "192.0.2.99:443"));
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

#[test]
fn a_node_with_no_key_is_given_none_and_a_key_by_file_is_passed_as_it_is() {
    // A node that logs in from its state needs no key: nothing is written and containerboot gets no TS_AUTHKEY.
    let mut env = good();
    env.retain(|(key, _)| *key != "TS_AUTHKEY");
    let ran = run_in(&["/bin/sh"], &env);
    assert_eq!(ran.code, Some(0), "{}", ran.err);
    assert!(ran.key_file.is_none());
    let boot = ran.boot_env.expect("containerboot ran");
    assert!(!boot.lines().any(|l| l.starts_with("TS_AUTHKEY=")));
    // A key that is a file reference already is not written again.
    let mut env = good();
    env.retain(|(key, _)| *key != "TS_AUTHKEY");
    env.push(("TS_AUTHKEY", "file:/run/secrets/synthetic-key"));
    let ran = run_in(&["/bin/sh"], &env);
    let boot = ran.boot_env.expect("containerboot ran");
    assert!(
        boot.lines()
            .any(|l| l == "TS_AUTHKEY=file:/run/secrets/synthetic-key")
    );
}

/// The shipped edge on the fake service network. A host listener is unrelated to the connector's link, so this
/// fixture leaves out privatePort; its port, provided values, secrets, script and edge behavior are the shipped ones.
fn linked_edge(rig: &Rig) -> String {
    let mut edge: Value =
        serde_json::from_slice(&fs::read(shipped().join("arca-edge.json")).unwrap()).unwrap();
    edge.as_object_mut().unwrap().remove("privatePort");
    rig.template("arca-edge", edge);
    rig.call(
        "service_create",
        json!({"name":"edge","template":"arca-edge"}),
    )
    .expect("create the edge");
    rig.call("service_connect", json!({"from":"edge-ts","to":"edge"}))
        .expect("link the connector");
    format!("{}:443", rig.service_config("edge").ipv4)
}

/// Feed exactly the fake Incus environment to the shipped script, as the container would see it.
fn run_instance(rig: &Rig) -> Ran {
    let live = instance(rig, "edge-ts");
    let env: Vec<_> = live["config"]
        .as_object()
        .unwrap()
        .iter()
        .filter_map(|(key, value)| Some((key.strip_prefix("environment.")?, value.as_str()?)))
        .collect();
    run_in(&["/bin/sh"], &env)
}

#[test]
fn a_connector_with_only_an_authkey_forwards_to_its_linked_edges_address_and_port() {
    let rig = ts_rig();
    op(
        &rig,
        "service_secret_set",
        json!({"name":"edge-ts","key":"TS_AUTHKEY","value":KEY}),
    )
    .expect("the key");
    let target = linked_edge(&rig);
    start(&rig, "edge-ts").expect("start with no target override");
    let live = instance(&rig, "edge-ts");
    assert_eq!(live["config"]["environment.ARCA_EDGE_TARGET"], target);
    assert!(live["config"].get("environment.TS_TARGET").is_none());
    let ran = run_instance(&rig);
    assert_eq!(ran.code, Some(0), "{}", ran.err);
    let serve: Value = serde_json::from_str(ran.serve.as_deref().expect("serve config")).unwrap();
    assert_eq!(
        serve,
        json!({"TCP":{"443":{"TCPForward":target,"ProxyProtocol":2}}})
    );
    assert!(
        ran.boot_env
            .unwrap()
            .lines()
            .any(|line| line == "TS_HOSTNAME=edge-ts")
    );
}

#[test]
fn a_linked_connector_starts_without_a_key_and_containerboot_receives_no_authkey() {
    let rig = ts_rig();
    let target = linked_edge(&rig);
    assert!(
        rig.service_config("edge-ts").start,
        "environment rendering must allow missing optional secrets"
    );
    start(&rig, "edge-ts").expect("a keyless start");
    assert!(
        instance(&rig, "edge-ts")["config"]
            .get("environment.TS_AUTHKEY")
            .is_none()
    );
    let ran = run_instance(&rig);
    assert_eq!(ran.code, Some(0), "{}", ran.err);
    let serve: Value = serde_json::from_str(ran.serve.as_deref().expect("serve config")).unwrap();
    assert_eq!(serve["TCP"]["443"]["TCPForward"], target);
    assert!(
        !ran.boot_env
            .unwrap()
            .lines()
            .any(|line| line.starts_with("TS_AUTHKEY="))
    );
    assert!(ran.key_file.is_none());
}

#[test]
fn connector_records_keep_the_kind_their_shipped_templates_name() {
    let rig = ts_rig();
    assert_eq!(
        serde_json::to_value(rig.service_config("edge-ts")).unwrap()["connector"],
        "tailscale"
    );
    rig.call(
        "service_create",
        json!({"name":"cf","template":"cloudflare-tunnel"}),
    )
    .expect("cloudflare connector");
    assert_eq!(
        serde_json::to_value(rig.service_config("cf")).unwrap()["connector"],
        "cloudflare"
    );
    assert!(
        serde_json::to_value(rig.service_config("web"))
            .unwrap()
            .get("connector")
            .is_none()
    );
}

#[test]
fn optional_secrets_omit_the_whole_variable_but_an_unset_required_secret_still_stops_start() {
    let rig = ts_rig();
    let template: service::Template = serde_json::from_value(json!({
        "name":"mixed", "kind":"oci", "image":"docker.io/library/example", "digest":DIGEST,
        "secrets":["OPTIONAL", "REQUIRED"], "optionalSecrets":["OPTIONAL"],
        "env":{"OPTIONAL_VALUE":"prefix-{{secret:OPTIONAL}}", "REQUIRED_VALUE":"{{secret:REQUIRED}}", "COMBINED":"{{secret:OPTIONAL}}-{{secret:REQUIRED}}"}
    })).expect("optionalSecrets is a template field");
    let secrets = rig.dir.join("secrets");
    let (env, ready) = service::environment(
        &template,
        "service-mixed",
        "mixed",
        "192.0.2.50",
        "192.0.2.1",
        &secrets,
    )
    .unwrap();
    assert!(env.is_empty());
    assert!(!ready);
    assert_eq!(
        service::template_stopped_reason(&template, "service-mixed", &secrets).as_deref(),
        Some("its template needs the secret REQUIRED, which is not set yet")
    );
    service::write_secret(
        &service::secret_file(&secrets, "service-mixed", "REQUIRED").unwrap(),
        "synthetic-required",
    )
    .unwrap();
    let (env, ready) = service::environment(
        &template,
        "service-mixed",
        "mixed",
        "192.0.2.50",
        "192.0.2.1",
        &secrets,
    )
    .unwrap();
    assert!(ready);
    assert!(!env.contains_key("OPTIONAL_VALUE"));
    assert!(!env.contains_key("COMBINED"));
    assert!(env.contains_key("REQUIRED_VALUE"));
    assert_eq!(
        service::template_stopped_reason(&template, "service-mixed", &secrets),
        None
    );
}

#[test]
fn other_services_take_the_edges_provided_target_and_disconnect_without_losing_their_environment() {
    let rig = ts_rig();
    let target = linked_edge(&rig);
    let before = instance(&rig, "web")["config"].clone();
    let record = rig.service_config("web").environment;
    let linked = rig
        .call("service_connect", json!({"from":"web","to":"edge"}))
        .expect("link an ordinary service");
    assert_eq!(linked["env"], json!(["ARCA_EDGE_TARGET"]));
    let mut expected = before.clone();
    expected["environment.ARCA_EDGE_TARGET"] = json!(target);
    // The link marker and environment-key index also change; every unrelated Incus config key keeps its bytes.
    let config = instance(&rig, "web")["config"].clone();
    for (key, value) in expected.as_object().unwrap() {
        if ![service::LINKS_KEY, service::ENVKEYS_KEY].contains(&key.as_str()) {
            assert_eq!(&config[key], value, "{key}");
        }
    }
    rig.call("service_disconnect", json!({"from":"web","to":"edge"}))
        .expect("disconnect the ordinary service");
    assert!(
        instance(&rig, "web")["config"]
            .get("environment.ARCA_EDGE_TARGET")
            .is_none()
    );
    assert_eq!(rig.service_config("web").environment, record);
    assert_eq!(rig.service_config("edge").incoming.len(), 1);
    assert_eq!(rig.service_config("edge").incoming[0].consumer, "edge-ts");
}

#[test]
fn the_script_uses_the_link_target_when_the_override_is_empty_under_each_shell() {
    for shell in super::nats::shells() {
        let mut env = good();
        env.retain(|(key, _)| *key != "TS_AUTHKEY" && *key != "TS_TARGET");
        env.push(("TS_TARGET", ""));
        env.push(("ARCA_EDGE_TARGET", "192.0.2.99:443"));
        let ran = run_in(&shell, &env);
        assert_eq!(ran.code, Some(0), "{shell:?}: {}", ran.err);
        let serve: Value = serde_json::from_str(ran.serve.as_deref().unwrap()).unwrap();
        assert_eq!(serve["TCP"]["443"]["TCPForward"], "192.0.2.99:443");
        let boot = ran.boot_env.unwrap();
        assert!(boot.lines().any(|line| line == "TS_HOSTNAME=edge-ts"));
        assert!(!boot.lines().any(|line| line.starts_with("TS_AUTHKEY=")));
    }
}

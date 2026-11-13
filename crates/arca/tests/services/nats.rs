//! The shipped `nats` template and its entrypoint script (`templates/nats.sh`), against the fake
//! Incus and a fake `nats-server`: no server runs and nothing reaches a registry. Every address and value is synthetic.
use super::*;
use std::process::{Command, Stdio};

/// The public `nats:2.14.10-alpine` image, by the digest the template pins. Moving it is a change of this test.
const NATS_DIGEST: &str = "sha256:311da47e2f31c26e99c623cd78d486a1e8ab039c49b32652770bb91a4f70512e";
const PASSWORD: &str = "Synthetic0Leaf.password_with~safe-chars";
const GIB: u64 = 1 << 30;

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
fn load(name: &str) -> service::Template {
    service::load_template(&shipped(), name).expect("the shipped template loads")
}
fn rig() -> Rig {
    // A node with a storage pool, which a data volume needs.
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
    rig
}

#[test]
fn the_template_loads_pins_nats_2_14_by_digest_and_keeps_4222_first_and_the_url_free_of_credentials()
 {
    let template = load("nats");
    assert_eq!(template.kind, "oci");
    assert_eq!(template.image, "docker.io/library/nats");
    assert_eq!(template.digest.as_deref(), Some(NATS_DIGEST));
    let source = service::source(&template).expect("source");
    assert_eq!(source["alias"], format!("library/nats@{NATS_DIGEST}"));
    assert!(template.description.as_deref().unwrap().contains("2.14"));
    // A link opens the first port, so the client port is the first; the leaf port and monitoring are declared after it.
    assert_eq!(template.ports, vec![4222, 7422, 8222]);
    let data = template.data.as_ref().expect("a data volume");
    assert_eq!(
        (data.path.as_str(), data.size.as_deref()),
        ("/data", Some("10GiB"))
    );
    // The one secret is the leaf user's password, made at create. The domain is the service's name.
    assert_eq!(template.secrets, vec!["NATS_LEAF_PASSWORD"]);
    assert_eq!(
        template
            .generate
            .get("NATS_LEAF_PASSWORD")
            .map(String::as_str),
        Some("")
    );
    assert_eq!(
        template.env["NATS_LEAF_PASSWORD"],
        "{{secret:NATS_LEAF_PASSWORD}}"
    );
    assert_eq!(template.env["NATS_DOMAIN"], "{{name}}");
    // Omni's client sends no user or password, so what a link hands out is the form it has always been.
    assert_eq!(
        template.provides,
        Some(json!({"env":{"NATS_URL":"nats://{{host}}:{{port}}"}}))
    );
    assert!(!template.publishable && template.lan.is_none());
    assert!(
        service::template_stopped_reason(&template, "service-bus", &rig().dir.join("secrets"))
            .is_none(),
        "every secret it needs is made at create"
    );
}

#[test]
fn the_script_is_installed_with_the_template_and_read_whole_into_its_entrypoint() {
    let rig = rig();
    let dir = rig.dir.join("templates");
    let template = service::load_template(&dir, "nats").expect("loads from the installed copy");
    let line = service::entrypoint(&template, &dir)
        .expect("the script is installed with it")
        .expect("an entrypoint");
    assert!(
        line.starts_with("/bin/sh -ec '") && line.ends_with("' nats"),
        "{line}"
    );
    assert!(line.contains("sync_interval: always"));
}

/// What one run of `nats.sh` left: its exit code, what it printed, the configuration it wrote, and the fake server's arguments.
struct Ran {
    code: Option<i32>,
    out: String,
    err: String,
    conf: Option<String>,
    conf_mode: Option<u32>,
    server_args: Option<String>,
    store_made: bool,
}

/// `nats.sh` as the template names it (`<shell> -ec <script> nats`), with `env` as its environment and a fake
/// `nats-server` that records its arguments. `shell` is the program and the words before `-ec`.
fn run_in(shell: &[&str], env: &[(&str, &str)]) -> Ran {
    let parts = load("nats").entrypoint.expect("an entrypoint");
    let script = fs::read_to_string(shipped().join("nats.sh")).expect("the script");
    let tail: Vec<&str> = parts.iter().filter_map(Value::as_str).skip(2).collect();
    static RUNS: AtomicU64 = AtomicU64::new(0);
    let work = std::env::temp_dir().join(format!(
        "arca-nats-script-{}-{}",
        std::process::id(),
        RUNS.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&work).expect("a work dir");
    let (conf, store, args) = (
        work.join("nats.conf"),
        work.join("data/jetstream"),
        work.join("args"),
    );
    let server = work.join("nats-server");
    fs::write(
        &server,
        format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", args.display()),
    )
    .unwrap();
    fs::set_permissions(&server, fs::Permissions::from_mode(0o755)).unwrap();
    let mut command = Command::new(shell[0]);
    command
        .args(&shell[1..])
        .args(["-ec", script.trim_end_matches('\n')])
        .args(&tail)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("NATS_CONF", &conf)
        .env("NATS_STORE_DIR", &store)
        .env("NATS_SERVER", &server)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().expect("the shell runs");
    let ran = Ran {
        code: output.status.code(),
        out: String::from_utf8_lossy(&output.stdout).into_owned(),
        err: String::from_utf8_lossy(&output.stderr).into_owned(),
        conf: fs::read_to_string(&conf).ok(),
        conf_mode: fs::metadata(&conf)
            .ok()
            .map(|m| m.permissions().mode() & 0o777),
        server_args: fs::read_to_string(&args).ok(),
        store_made: store.is_dir(),
    };
    fs::remove_dir_all(&work).expect("the work dir goes");
    ran
}

fn run(env: &[(&str, &str)]) -> Ran {
    run_in(&["/bin/sh"], env)
}

/// The image's own shell is busybox's `ash`. Where this host has busybox (a distribution's, not a download), the same
/// script runs under it too; where it has none the test says so and runs the host's `sh` alone.
fn shells() -> Vec<Vec<&'static str>> {
    let mut shells = vec![vec!["/bin/sh"]];
    let busybox = ["/usr/bin/busybox", "/bin/busybox"]
        .into_iter()
        .find(|path| std::path::Path::new(path).is_file());
    match busybox {
        Some(path)
            if Command::new(path)
                .args(["ash", "-c", "exit 0"])
                .status()
                .is_ok_and(|s| s.success()) =>
        {
            shells.push(vec![path, "ash"]);
        }
        // A passing test's output is not in a log, so a run that skipped ash must not look like one that ran it: in CI
        // the host has to have it.
        _ => {
            assert!(
                std::env::var_os("CI").is_none(),
                "no busybox ash on this CI host, so the image's shell was not tried: install busybox on the runner"
            );
            eprintln!("nats.sh: no busybox ash on this host; the script was run under /bin/sh only")
        }
    }
    shells
}

fn good() -> Vec<(&'static str, &'static str)> {
    vec![("NATS_LEAF_PASSWORD", PASSWORD), ("NATS_DOMAIN", "bus")]
}

#[test]
fn the_script_writes_every_setting_the_design_names_and_starts_the_server_on_it_under_each_shell() {
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
        line(r#"server_name: "bus""#);
        line("port: 4222");
        line(r#"http: "0.0.0.0:8222""#);
        line("sync_interval: always");
        line(r#"domain: "bus""#);
        line(&format!(r#"store_dir: "{}""#, conf_store(&conf)));
        line(r#"listen: "0.0.0.0:7422""#);
        line(r#"user: "leaf""#);
        line(&format!(r#"password: "{PASSWORD}""#));
        // Clients on 4222 log in with nothing, so the only authorization is the leaf node listener's.
        assert_eq!(
            conf.lines().filter(|l| l.contains("authorization")).count(),
            1,
            "{conf}"
        );
        let leafnodes = conf.find("leafnodes").expect("the leaf block");
        assert!(conf.find("authorization").unwrap() > leafnodes, "{conf}");
        // The server is started on that file and nothing else.
        let args = ran.server_args.expect("the server was started");
        assert_eq!(args.lines().next(), Some("-c"));
        assert!(
            args.lines().nth(1).unwrap().ends_with("nats.conf"),
            "{args}"
        );
        assert_eq!(args.lines().count(), 2, "{args}");
        // The file holds the password, so only its owner reads it; neither stream shows it.
        assert_eq!(ran.conf_mode, Some(0o600));
        assert!(!ran.out.contains(PASSWORD) && !ran.err.contains(PASSWORD));
        assert!(ran.store_made, "the store directory was made");
    }
}

/// The `store_dir` the run wrote, which is under the test's own work directory.
fn conf_store(conf: &str) -> String {
    conf.lines()
        .find_map(|l| l.trim().strip_prefix("store_dir: \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .expect("a store_dir")
        .to_string()
}

#[test]
fn the_store_is_under_the_volume_and_max_file_store_is_below_its_size() {
    let template = load("nats");
    let data = template.data.expect("a volume");
    let size = data
        .size
        .as_deref()
        .and_then(|s| s.strip_suffix("GiB"))
        .map(|n| n.parse::<u64>().unwrap() * GIB)
        .expect("a size in GiB");
    // The script's own default is under the volume's mount, which is what survives an update.
    let script = fs::read_to_string(shipped().join("nats.sh")).unwrap();
    assert!(
        script.contains(&format!(
            "STORE=${{NATS_STORE_DIR:-{}/jetstream}}",
            data.path
        )),
        "store_dir is under {}",
        data.path
    );
    let conf = run(&good()).conf.unwrap();
    let max: u64 = conf
        .lines()
        .find_map(|l| l.trim().strip_prefix("max_file_store: "))
        .expect("max_file_store")
        .parse()
        .expect("plain bytes");
    assert!(
        max < size,
        "max_file_store {max} is below the volume's {size}"
    );
    assert!(max >= size / 2, "and not a token amount");
}

#[test]
fn the_script_refuses_a_value_that_would_put_a_bad_character_in_the_file() {
    let with = |key: &str, value: &str| -> Vec<(String, String)> {
        good()
            .iter()
            .filter(|(k, _)| *k != key)
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .chain((!value.is_empty()).then(|| (key.to_string(), value.to_string())))
            .collect()
    };
    for shell in shells() {
        let refused = |env: Vec<(String, String)>, words: &str| {
            let env: Vec<(&str, &str)> =
                env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let ran = run_in(&shell, &env);
            assert_ne!(ran.code, Some(0), "{shell:?}: {words}");
            assert!(ran.err.contains(words), "{shell:?}: {words}: {}", ran.err);
            assert!(
                ran.conf.is_none() && ran.server_args.is_none(),
                "nothing is written or started: {words}"
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
            "back\\slash",
            "brace}",
        ] {
            refused(
                with("NATS_LEAF_PASSWORD", bad),
                "NATS_LEAF_PASSWORD must be letters",
            );
        }
        refused(with("NATS_DOMAIN", ""), "NATS_DOMAIN is not set");
        for bad in ["Upper", "dot.ted", "-lead", "a b", "x\"y"] {
            refused(with("NATS_DOMAIN", bad), "NATS_DOMAIN must be");
        }
    }
}

#[test]
fn a_created_nats_has_its_own_password_domain_and_script_and_a_link_gives_the_url_it_always_gave() {
    let rig = rig();
    let created = rig
        .call("service_create", json!({"name":"bus","template":"nats"}))
        .expect("create");
    assert_eq!(created["state"], "running", "{created}");
    let live = instance(&rig, "bus");
    let password = live["config"]["environment.NATS_LEAF_PASSWORD"]
        .as_str()
        .expect("a password")
        .to_string();
    assert_eq!(password.len(), 32);
    assert!(password.bytes().all(|b| b.is_ascii_alphanumeric()));
    assert_eq!(live["config"]["environment.NATS_DOMAIN"], "bus");
    assert!(
        live["config"]["oci.entrypoint"]
            .as_str()
            .unwrap()
            .ends_with("' nats")
    );
    assert!(!created.to_string().contains(&password));
    assert_eq!(live["devices"]["data"]["path"], "/data");
    assert_eq!(live["config"][service::PORTS_KEY], "4222,7422,8222");

    rig.template(
        "client",
        json!({"name":"client","kind":"oci","image":"docker.io/library/example","digest":DIGEST,"ports":[],"env":{}}),
    );
    rig.call("service_create", json!({"name":"api","template":"client"}))
        .expect("a consumer");
    assert_eq!(
        op(&rig, "service_connect", json!({"from":"api","to":"bus"})).unwrap(),
        json!({"env":["NATS_URL"]})
    );
    let address = live["config"][service::ADDRESS_KEY]
        .as_str()
        .unwrap()
        .to_string();
    // The old, credential-free form: Omni's client reads no user or password from a URL.
    assert_eq!(
        instance(&rig, "api")["config"]["environment.NATS_URL"],
        json!(format!("nats://{address}:4222"))
    );
    // The link opened 4222 and not the leaf node port or monitoring.
    let rules = acl(&rig, "bus")["ingress"].clone();
    let ports: Vec<_> = rules
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["destination_port"].clone())
        .collect();
    assert_eq!(ports, vec![json!("4222")], "{rules}");
    // A second service has its own password and its own domain.
    rig.call("service_create", json!({"name":"bus2","template":"nats"}))
        .expect("create");
    let other = instance(&rig, "bus2");
    assert_ne!(
        other["config"]["environment.NATS_LEAF_PASSWORD"],
        json!(password)
    );
    assert_eq!(other["config"]["environment.NATS_DOMAIN"], "bus2");
}

/// The template as 0.25.0 shipped it, byte for byte: NATS 2.10, no secret, `--jetstream` on its command line.
const NATS_OF_0_25_0: &str = r#"{
  "name": "nats",
  "description": "NATS 2.10 (the public nats:2.10-alpine by digest) with JetStream on, storing under /data/jetstream. A link gives NATS_URL.",
  "kind": "oci",
  "image": "docker.io/library/nats",
  "digest": "sha256:b83efabe3e7def1e0a4a31ec6e078999bb17c80363f881df35edc70fcb6bb927",
  "entrypoint": ["/usr/local/bin/nats-server", "--jetstream", "--store_dir", "/data/jetstream", "--port", "4222"],
  "ports": [4222],
  "data": { "path": "/data", "uid": 0, "gid": 0, "size": "10GiB" },
  "env": {},
  "provides": { "env": { "NATS_URL": "nats://{{host}}:{{port}}" } },
  "publishable": false
}
"#;

#[test]
fn a_nats_made_before_this_template_still_links_and_unlinks_after_the_templates_are_installed() {
    let rig = rig();
    let dir = rig.dir.join("templates");
    // The node as 0.25.0 left it: the old template, no script.
    fs::write(dir.join("nats.json"), NATS_OF_0_25_0).expect("the old template");
    fs::remove_file(dir.join("nats.sh")).expect("no script");
    rig.template(
        "client",
        json!({"name":"client","kind":"oci","image":"docker.io/library/example","digest":DIGEST,"ports":[],"env":{}}),
    );
    rig.call("service_create", json!({"name":"old","template":"nats"}))
        .expect("the old service");
    let before = instance(&rig, "old");
    assert!(
        before["config"]
            .get("environment.NATS_LEAF_PASSWORD")
            .is_none()
    );
    assert!(
        !before["config"]["oci.entrypoint"]
            .as_str()
            .unwrap()
            .contains("nats.sh")
    );
    // The upgrade installs the templates: the old file is an earlier release's own, so it is replaced without --replace.
    let report = arca::cmd_templates::install_into(&dir, false).expect("upgrade");
    assert_eq!(report["replaced"], json!(["nats.json"]), "{report}");
    // Arca now reads the new template for the old service. Its links work all the same, and give the old URL.
    rig.call("service_create", json!({"name":"api","template":"client"}))
        .expect("a consumer");
    assert_eq!(
        op(&rig, "service_connect", json!({"from":"api","to":"old"})).expect("connect"),
        json!({"env":["NATS_URL"]})
    );
    let address = before["config"][service::ADDRESS_KEY]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        instance(&rig, "api")["config"]["environment.NATS_URL"],
        json!(format!("nats://{address}:4222"))
    );
    op(&rig, "service_disconnect", json!({"from":"api","to":"old"})).expect("disconnect");
    assert!(
        instance(&rig, "api")["config"]
            .get("environment.NATS_URL")
            .is_none()
    );
    // The old service itself is as it was: its command line and environment are its record's, not the new template's.
    let after = instance(&rig, "old");
    assert_eq!(
        after["config"]["oci.entrypoint"],
        before["config"]["oci.entrypoint"]
    );
    assert!(after["config"].get("environment.NATS_DOMAIN").is_none());
}

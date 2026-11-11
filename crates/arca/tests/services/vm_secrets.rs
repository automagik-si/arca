//! A virtual-machine template with a first-boot script, settings and secrets, against a fake Incus. The script and the
//! settings reach the machine as cloud-init keys of its create. A secret never reaches Incus's instance config: it is
//! stored by `service_secret_set` and written into the guest, as a file, by the next `instance_start`. Every value here
//! is synthetic.
use super::*;
use arca::lifecycle::{INSTANCE_ACTIONS_FILE, InstanceLifecycle};
use std::time::Duration;

/// Shaped like an access token and like a private key in PEM form (several lines), and neither is one.
const TOKEN: &str = "synthetic-token-0123456789abcdefghij";
const KEY: &str = "-----BEGIN PRIVATE KEY-----\nc3ludGhldGljLWtleS1saW5lLW9uZQ\nc3ludGhldGljLWtleS1saW5lLXR3bw\n-----END PRIVATE KEY-----\n";
const SCRIPT: &str = "#!/bin/sh\n# a first-boot script\necho ready > /run/first-boot\n";

fn pool_template() -> Value {
    json!({
        "name": "pool",
        "kind": "virtual-machine",
        "image": "images:debian/13/cloud",
        "limits": { "cpu": "2", "memory": "4GiB" },
        "rootSize": "20GiB",
        "autostart": true,
        "firstBoot": { "file": "pool.sh" },
        "settings": { "url": "https://git.example/example-org", "slots": 2, "labels": ["a", "b"] },
        "secrets": ["API_TOKEN", "SIGNING_KEY"]
    })
}

/// A mixed node with the `pool` template and its script installed.
fn pool_rig() -> Rig {
    let rig = Rig::with(mixed_kernel(&["br0"], roomy()), None, vec![]);
    rig.template("pool", pool_template());
    fs::write(rig.dir.join("templates/pool.sh"), SCRIPT).expect("the script");
    rig
}

/// The instance verbs as `arca serve` builds them: a start or a restart writes a virtual machine's secrets into its
/// guest. `wait` is how long the guest agent is waited for.
fn life(rig: &Rig, wait: Duration) -> HashMap<&'static str, Arc<dyn ToolHandler>> {
    let kernel = Arc::clone(&rig.kernel);
    let secrets = arca::guest_secrets::GuestSecrets::new(rig.cfg.clone(), Arc::clone(&rig.kernel))
        .with_wait(wait, Duration::from_millis(10));
    let lifecycle = InstanceLifecycle::new(
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
    )
    .with_after_start(Arc::new(move |project, instance| {
        secrets.deliver(project, instance)
    }));
    arca::lifecycle::handlers(
        Arc::clone(&rig.kernel),
        Arc::new(|| NOW),
        Some(Arc::new(lifecycle)),
    )
    .into_iter()
    .collect()
}

fn start(
    life: &HashMap<&'static str, Arc<dyn ToolHandler>>,
    name: &str,
) -> Result<Value, CallError> {
    life["instance_start"].call(json!({"project":PROJECT,"instance":name}), &doctor())
}

fn guest_file(rig: &Rig, name: &str, key: &str) -> Option<String> {
    rig.fake
        .service_files()
        .get(&format!("{PROJECT}/{name}:/etc/arca-secrets/{key}"))
        .map(|bytes| String::from_utf8(bytes.clone()).expect("text"))
}

/// Every file of the state directory outside `secrets/`, as text: the desired state, the journals and the audit files.
fn state_text(rig: &Rig) -> String {
    fn walk(dir: &std::path::Path, out: &mut String) {
        for file in fs::read_dir(dir).expect("a directory").flatten() {
            let path = file.path();
            if path.file_name().is_some_and(|name| name == "secrets") {
                continue;
            }
            if path.is_dir() {
                walk(&path, out);
            } else if let Ok(bytes) = fs::read(&path) {
                out.push_str(&String::from_utf8_lossy(&bytes));
                out.push('\n');
            }
        }
    }
    let mut out = String::new();
    walk(&rig.dir, &mut out);
    out
}

/// Whether `text` holds a secret: the token, the whole key, or one of the key's own lines.
fn holds_a_secret(text: &str) -> bool {
    text.contains(TOKEN)
        || text.contains(KEY)
        || KEY
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .any(|line| text.contains(line))
        // A JSON document would carry the key with its line breaks escaped.
        || text.contains(&KEY.replace('\n', "\\n"))
}

#[test]
fn a_vm_template_takes_a_first_boot_script_settings_and_secrets_and_no_other_kind_does() {
    let rig = pool_rig();
    let dir = rig.dir.join("templates");
    let template = service::load_template(&dir, "pool").expect("the template loads");
    assert_eq!(template.secrets, vec!["API_TOKEN", "SIGNING_KEY"]);
    assert_eq!(
        service::first_boot(&template, &dir)
            .expect("the script")
            .as_deref(),
        Some(SCRIPT)
    );
    // The settings are one write_files entry whose content is base64, so no setting can change the YAML around it.
    let vendor = service::settings_vendor_data(&template)
        .expect("vendor data")
        .expect("some");
    let (head, content) = vendor.rsplit_once("content: ").expect("the content line");
    assert_eq!(
        head,
        "#cloud-config\nwrite_files:\n- path: /etc/arca/settings.json\n  permissions: '0644'\n  encoding: b64\n  "
    );
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(content.trim_end())
        .expect("base64");
    assert_eq!(
        serde_json::from_slice::<Value>(&decoded).expect("json"),
        pool_template()["settings"]
    );

    let load = |value: Value| {
        rig.template("bad", {
            let mut value = value;
            value["name"] = json!("bad");
            value
        });
        service::load_template(&dir, "bad")
    };
    let with = |key: &str, extra: Value| {
        let mut value = pool_template();
        value[key] = extra;
        value
    };
    for (bad, why) in [
        (
            with("firstBoot", json!("pool.sh")),
            "firstBoot is { \"file\"",
        ),
        (
            with("firstBoot", json!({"file":"../pool.sh"})),
            "firstBoot is { \"file\"",
        ),
        (
            with("firstBoot", json!({"file":"pool.sh","more":1})),
            "firstBoot is { \"file\"",
        ),
        (
            with("settings", json!(["a"])),
            "settings is one JSON object",
        ),
        (
            with("settings", json!({"big": "x".repeat(17 * 1024)})),
            "settings is one JSON object",
        ),
        (with("secrets", json!(["A-B"])), "invalid secret name"),
        (with("secrets", json!(["A", "A"])), "invalid secret name"),
        (
            with("generate", json!({"API_TOKEN":"k_"})),
            "has no env, generate",
        ),
        (
            with("env", json!({"A":"{{secret:API_TOKEN}}"})),
            "has no env, generate",
        ),
    ] {
        let error = load(bad.clone()).expect_err("the template is refused");
        assert!(error.message.contains(why), "{bad}: {}", error.message);
    }
    // A container template has neither.
    for key in ["firstBoot", "settings"] {
        let mut container = nats_template();
        container[key] = pool_template()[key].clone();
        let error = load(container).expect_err("refused");
        assert!(
            error
                .message
                .contains("only a virtual-machine template has a firstBoot or settings"),
            "{}",
            error.message
        );
    }

    // A script that is missing, or is not cloud-init user data, refuses the create before anything is written.
    let before = rig.fake.writes().len();
    fs::write(dir.join("pool.sh"), "echo no header\n").expect("the script");
    assert!(
        refusal(rig.call("service_create", json!({"name":"p1","template":"pool"})))
            .contains("is not cloud-init user data: it begins #! or #cloud-config")
    );
    fs::remove_file(dir.join("pool.sh")).expect("remove");
    assert!(
        refusal(rig.call("service_create", json!({"name":"p1","template":"pool"})))
            .contains("its first-boot script is not installed (templates/pool.sh does not exist)")
    );
    assert_eq!(rig.fake.writes().len(), before, "no refusal wrote anything");
    assert!(rig.entry_ids().iter().all(|id| id != "service-p1"));
}

#[test]
fn a_vm_with_secrets_is_created_stopped_with_its_script_and_incus_never_receives_a_secret_but_as_a_guest_file()
 {
    let rig = pool_rig();
    let created = rig
        .call("service_create", json!({"name":"p1","template":"pool"}))
        .expect("create");
    // It waits for its secrets, as a container whose secret is not set does.
    assert_eq!(created["state"], "stopped");
    assert_eq!(instance(&rig, "p1")["status"], "Stopped");
    let config = instance(&rig, "p1")["config"].clone();
    assert_eq!(config["cloud-init.user-data"], SCRIPT);
    assert!(
        config["cloud-init.vendor-data"]
            .as_str()
            .unwrap()
            .starts_with("#cloud-config\nwrite_files:\n- path: /etc/arca/settings.json\n")
    );
    assert!(
        config
            .as_object()
            .unwrap()
            .keys()
            .all(|key| !key.starts_with("environment.") && !key.starts_with("user.arca.secret"))
    );

    for (key, value) in [("API_TOKEN", TOKEN), ("SIGNING_KEY", KEY)] {
        let answer = rig
            .call(
                "service_secret_set",
                json!({"name":"p1","key":key,"value":value}),
            )
            .expect("the secret is stored");
        assert_eq!(answer, json!({"ok": true}));
    }
    // Stored, and not sent: the machine is stopped, and nothing is in its config or in its guest.
    let record: service::ServiceConfig =
        serde_json::from_value(entry(&rig, "service-p1").config).expect("the record");
    assert!(
        record
            .secret_refs
            .values()
            .all(|reference| reference.starts_with("arca-secret:service-p1/v-"))
    );
    assert!(record.environment.is_empty());
    assert_eq!(rig.fake.service_files().len(), 0);
    assert_eq!(instance(&rig, "p1")["config"], config);

    let life = life(&rig, Duration::from_secs(5));
    let started = start(&life, "p1").expect("the start");
    assert_eq!(started["changed"], true);
    assert_eq!(
        started["message"],
        "started the instance p1 in project arcak-svc. Its 2 secrets (API_TOKEN, SIGNING_KEY) were written into the guest, in /etc/arca-secrets."
    );
    assert_eq!(guest_file(&rig, "p1", "API_TOKEN").as_deref(), Some(TOKEN));
    assert_eq!(guest_file(&rig, "p1", "SIGNING_KEY").as_deref(), Some(KEY));

    // Every request Incus received: a secret is in the body of the file request for its own guest path, and nowhere
    // else. The create, every config write and every read carry none.
    let mut file_posts = Vec::new();
    for request in rig.fake.service_requests() {
        let head = format!("{} {} {:?}", request.method, request.path, request.headers);
        assert!(
            !holds_a_secret(&head),
            "a request line or header holds a secret"
        );
        let body = String::from_utf8_lossy(&request.body).into_owned();
        let files = request.path.split('?').next().unwrap().ends_with("/files");
        if request.method == "POST" && files {
            file_posts.push((request.path.clone(), request.headers.clone(), body));
        } else {
            assert!(
                !holds_a_secret(&body),
                "{} {} holds a secret",
                request.method,
                request.path
            );
        }
    }
    let header = |headers: &[(String, String)], name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    let shape: Vec<_> = file_posts
        .iter()
        .map(|(path, headers, body)| {
            (
                path.rsplit_once("path=").unwrap().1.to_string(),
                header(headers, "x-incus-type").unwrap(),
                header(headers, "x-incus-mode").unwrap(),
                header(headers, "x-incus-uid").unwrap(),
                header(headers, "x-incus-gid").unwrap(),
                body.len(),
            )
        })
        .collect();
    // The private directory first, then one private file of root's per secret.
    let own = |text: &str| text.to_string();
    assert_eq!(
        shape,
        vec![
            (
                own("/etc/arca-secrets"),
                own("directory"),
                own("0700"),
                own("0"),
                own("0"),
                0
            ),
            (
                own("/etc/arca-secrets/API_TOKEN"),
                own("file"),
                own("0600"),
                own("0"),
                own("0"),
                TOKEN.len()
            ),
            (
                own("/etc/arca-secrets/SIGNING_KEY"),
                own("file"),
                own("0600"),
                own("0"),
                own("0"),
                KEY.len()
            ),
        ]
    );
    // Incus's instance holds no secret, and neither does anything Arca keeps outside its secret store: the desired
    // state, the effect journal, the instance action journal and the exec audit.
    assert!(!holds_a_secret(&instance(&rig, "p1").to_string()));
    assert!(!holds_a_secret(&state_text(&rig)));
    // The instance action journal names what was written.
    let actions = fs::read_to_string(rig.dir.join(INSTANCE_ACTIONS_FILE)).expect("the journal");
    let row: Value = serde_json::from_str(actions.lines().last().unwrap()).expect("a row");
    assert_eq!(row["kind"], "guest_secrets");
    assert_eq!(row["outcome"], "written");
    assert_eq!(row["secrets"], json!(["API_TOKEN", "SIGNING_KEY"]));
    no_open_intent(&rig);
}

#[test]
fn a_secret_set_on_a_running_vm_applies_at_its_next_start_which_never_restarts_it() {
    let rig = pool_rig();
    rig.call("service_create", json!({"name":"p1","template":"pool"}))
        .expect("create");
    rig.call(
        "service_secret_set",
        json!({"name":"p1","key":"API_TOKEN","value":TOKEN}),
    )
    .expect("set");
    let life = life(&rig, Duration::from_secs(5));
    assert_eq!(
        start(&life, "p1").expect("start")["message"],
        "started the instance p1 in project arcak-svc. Its secret (API_TOKEN) was written into the guest, in /etc/arca-secrets."
    );
    let uuid = instance(&rig, "p1")["config"]["volatile.uuid"].clone();

    // A new value is stored at once and reaches the guest at the next start, not before.
    let rotated = "synthetic-token-rotated-9876543210";
    rig.call(
        "service_secret_set",
        json!({"name":"p1","key":"API_TOKEN","value":rotated}),
    )
    .expect("set again");
    assert_eq!(guest_file(&rig, "p1", "API_TOKEN").as_deref(), Some(TOKEN));
    let again = start(&life, "p1").expect("a start of a running machine");
    assert_eq!(again["changed"], false);
    assert_eq!(again["status"], "Running");
    assert_eq!(
        again["message"],
        "the instance p1 in project arcak-svc is already running; nothing was started. Its secret (API_TOKEN) was written into the guest, in /etc/arca-secrets."
    );
    assert_eq!(
        guest_file(&rig, "p1", "API_TOKEN").as_deref(),
        Some(rotated)
    );
    assert_eq!(instance(&rig, "p1")["config"]["volatile.uuid"], uuid);

    // A secret the template does not declare, a value with a control character that is no line break, and a
    // container's value with a line break are refused; nothing is stored for them.
    assert_eq!(
        refusal(rig.call(
            "service_secret_set",
            json!({"name":"p1","key":"OTHER","value":"a-long-enough-value"})
        )),
        "template does not declare that secret"
    );
    assert!(matches!(
        rig.call(
            "service_secret_set",
            json!({"name":"p1","key":"API_TOKEN","value":"a-tab\tin-the-value"})
        ),
        Err(CallError::Invalid(why)) if why.contains("may have line breaks, and no other control character")
    ));
    rig.template(
        "tok",
        json!({"name":"tok","kind":"oci","image":"docker.io/library/example","digest":DIGEST,
            "ports":[],"env":{"T":"{{secret:T}}"},"secrets":["T"]}),
    );
    rig.call("service_create", json!({"name":"tok","template":"tok"}))
        .expect("a container");
    assert!(matches!(
        rig.call(
            "service_secret_set",
            json!({"name":"tok","key":"T","value":"two\nlines-of-value"})
        ),
        Err(CallError::Invalid(why)) if why == "a secret is 8 to 4096 characters without control characters"
    ));

    // A virtual machine on a host bridge takes its secrets the same way.
    rig.call(
        "service_create",
        json!({"name":"p2","template":"pool","bridge":"br0"}),
    )
    .expect("a machine on the host bridge");
    rig.call(
        "service_secret_set",
        json!({"name":"p2","key":"SIGNING_KEY","value":KEY}),
    )
    .expect("set");
    start(&life, "p2").expect("start");
    assert_eq!(guest_file(&rig, "p2", "SIGNING_KEY").as_deref(), Some(KEY));
    assert_eq!(guest_file(&rig, "p2", "API_TOKEN"), None);
    // One whose template declares no secret says so.
    rig.call(
        "service_create",
        json!({"name":"ws1","template":"ws","bridge":"br0"}),
    )
    .expect("a plain machine");
    assert_eq!(
        refusal(rig.call(
            "service_secret_set",
            json!({"name":"ws1","key":"TOKEN","value":"a-long-enough-value"})
        )),
        "template does not declare that secret: this virtual machine's template declares none"
    );
    no_open_intent(&rig);
}

#[test]
fn a_guest_agent_that_is_not_up_yet_is_waited_for_and_one_that_never_answers_is_said_in_words() {
    let rig = pool_rig();
    rig.call("service_create", json!({"name":"p1","template":"pool"}))
        .expect("create");
    rig.call(
        "service_secret_set",
        json!({"name":"p1","key":"API_TOKEN","value":TOKEN}),
    )
    .expect("set");
    // A machine that has just started boots before its agent takes a file.
    rig.fake.set_guest_agent_unready(3);
    start(&life(&rig, Duration::from_secs(5)), "p1").expect("the agent is waited for");
    assert_eq!(guest_file(&rig, "p1", "API_TOKEN").as_deref(), Some(TOKEN));

    // An agent that never answers: the machine runs, and the answer says what was not done and how to do it.
    rig.call(
        "service_secret_set",
        json!({"name":"p1","key":"API_TOKEN","value":"synthetic-token-second-value-000"}),
    )
    .expect("set again");
    rig.fake.set_guest_agent_unready(u32::MAX);
    let Err(CallError::Refused { code, message }) =
        start(&life(&rig, Duration::from_millis(50)), "p1")
    else {
        panic!("a refusal");
    };
    assert_eq!(code, "InstanceSecretsNotWritten");
    assert_eq!(
        message,
        "the instance p1 in project arcak-svc is already running; nothing was started. It is running, but its secrets were not written into the guest: the guest took no file within 0 seconds: its agent is not running yet, or its image has none. Run instance_start for it again to write them; that never restarts it"
    );
    assert_eq!(instance(&rig, "p1")["status"], "Running");
    assert_eq!(guest_file(&rig, "p1", "API_TOKEN").as_deref(), Some(TOKEN));
    let actions = fs::read_to_string(rig.dir.join(INSTANCE_ACTIONS_FILE)).expect("the journal");
    let row: Value = serde_json::from_str(actions.lines().last().unwrap()).expect("a row");
    assert_eq!(
        (row["kind"].as_str(), row["outcome"].as_str()),
        (Some("guest_secrets"), Some("failed"))
    );
    assert!(row["secrets"].is_null());
    assert!(!holds_a_secret(&state_text(&rig)));
    assert!(!state_text(&rig).contains("synthetic-token-second-value-000"));
}

#[test]
fn a_vm_without_secrets_and_a_container_start_as_before_and_a_start_writes_nothing_into_them() {
    let rig = pool_rig();
    let created = rig
        .call("service_create", json!({"name":"ws1","template":"ws"}))
        .expect("a plain machine");
    assert_eq!(created["state"], "running");
    let config = instance(&rig, "ws1")["config"].clone();
    assert!(
        config
            .as_object()
            .unwrap()
            .keys()
            .all(|key| !key.starts_with("cloud-init."))
    );
    // Its record is the one an earlier kernel wrote: no key for the script or the settings.
    let record = entry(&rig, "service-ws1").config;
    assert_eq!(record["vm"], json!({"rootSize":"20GiB","mac":null}));
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("a container");
    let life = life(&rig, Duration::from_secs(5));
    for name in ["ws1", "nats"] {
        assert_eq!(
            start(&life, name).expect("start")["message"],
            format!(
                "the instance {name} in project arcak-svc is already running; nothing was started"
            )
        );
    }
    assert_eq!(rig.fake.service_files().len(), 0);
    let actions = fs::read_to_string(rig.dir.join(INSTANCE_ACTIONS_FILE)).expect("the journal");
    assert!(!actions.contains("guest_secrets"));
}

/// The create request Incus receives for a container from an OCI template, a container from a system template and a
/// virtual machine without a script or a secret, compared with the bodies the kernel sent before virtual machines
/// took first-boot scripts and secrets (`tests/golden/service-create-*.json`, written from that kernel).
#[test]
fn the_create_requests_of_an_oci_a_system_and_a_plain_vm_template_are_the_golden_ones() {
    let rig = Rig::with(mixed_kernel(&["br0"], roomy()), None, vec![]);
    rig.template(
        "oci",
        json!({"name":"oci","kind":"oci","image":"docker.io/library/example","digest":DIGEST,
            "entrypoint":["/bin/server","--port","8080"],"ports":[8080],
            "env":{"MODE":"plain","URL":"http://{{host}}:{{port}}/{{name}}"},
            "limits":{"cpu":"1","memory":"1GiB"},"autostart":true,"lan":{"port":8080}}),
    );
    rig.template(
        "sys",
        json!({"name":"sys","kind":"system","image":"images:debian/13","ports":[22],"env":{}}),
    );
    for (name, template, args) in [
        ("c1", "oci", json!({})),
        ("s1", "sys", json!({})),
        ("v1", "ws", json!({})),
        (
            "v2",
            "ws",
            json!({"bridge":"br0","mac":"02:aa:bb:00:00:02"}),
        ),
    ] {
        let mut args = args;
        args["name"] = json!(name);
        args["template"] = json!(template);
        rig.call("service_create", args).expect("create");
    }
    let mut bodies = serde_json::Map::new();
    for request in rig.fake.service_requests() {
        if request.method == "POST" && request.path.starts_with("/1.0/instances?") {
            let body: Value = serde_json::from_slice(&request.body).expect("a create body");
            bodies.insert(body["name"].as_str().unwrap().to_string(), body);
        }
    }
    assert_eq!(bodies.len(), 4);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/service-create-requests.json");
    let found = format!(
        "{}\n",
        serde_json::to_string_pretty(&Value::Object(bodies)).expect("json")
    );
    if std::env::var_os("ARCA_UPDATE_GOLDEN").is_some() {
        fs::write(&path, &found).expect("the golden file is written");
    }
    assert_eq!(
        found,
        fs::read_to_string(&path).expect("the golden file is read"),
        "the create requests differ from tests/golden/service-create-requests.json"
    );
}

//! `service_create` with `start: false`: the service is created stopped, its secrets and links are set while it is
//! stopped, and its first start is `instance_start`. A secret and a link reach a container's environment only at its
//! next start, so this is what lets a service boot for the first time with all of them. Every value here is synthetic.
use super::*;
use std::collections::BTreeMap;

const DB_PASSWORD: &str = "synthetic-database-password";
const ADMIN_PASSWORD: &str = "synthetic-admin-password";
/// What a deploying token's tool allowlist names here.
pub(super) const TOOLS: [&str; 6] = [
    "service_create",
    "service_secret_set",
    "service_connect",
    "instance_start",
    "deploy_confirm",
    "deploy_rollback",
];

/// A database that sets itself up at its first boot, with the password it finds then. The template makes one, so a
/// default create starts it at once.
fn db_template() -> Value {
    json!({
        "name": "db", "kind": "oci", "image": "docker.io/library/db", "digest": DIGEST,
        "ports": [5432],
        "env": { "POSTGRES_PASSWORD": "{{secret:POSTGRES_PASSWORD}}" },
        "secrets": ["POSTGRES_PASSWORD"],
        "generate": { "POSTGRES_PASSWORD": "pg_" },
        "autostart": true,
        "provides": { "env": {
            "DATABASE_URL": "postgresql://app:{{secret:POSTGRES_PASSWORD}}@{{host}}:{{port}}/app"
        } },
        "publishable": false
    })
}

/// An application that reads its admin password and its database at its first boot. Both of its secrets are made by
/// the template, so a default create starts it at once, before any link exists.
fn web_template() -> Value {
    json!({
        "name": "web", "kind": "oci", "image": "docker.io/library/web", "digest": DIGEST,
        "ports": [8080],
        "env": {
            "ADMIN_PASSWORD": "{{secret:ADMIN_PASSWORD}}",
            "SESSION_KEY": "{{secret:SESSION_KEY}}"
        },
        "secrets": ["ADMIN_PASSWORD", "SESSION_KEY"],
        "generate": { "ADMIN_PASSWORD": "adm_", "SESSION_KEY": "sk_" },
        "autostart": true,
        "publishable": false
    })
}

/// A container with no secret at all: a default create starts it.
fn plain_template() -> Value {
    let mut template = app_template("plain");
    template["autostart"] = json!(true);
    template
}

/// A container whose template needs a secret nobody has set: created stopped whatever the call says.
fn tunnel_template() -> Value {
    json!({
        "name": "tunnel", "kind": "oci", "image": "docker.io/library/tunnel", "digest": DIGEST,
        "ports": [], "env": { "TUNNEL_TOKEN": "{{secret:TUNNEL_TOKEN}}" },
        "secrets": ["TUNNEL_TOKEN"], "autostart": true, "publishable": false
    })
}

/// A virtual machine whose template declares a secret: created stopped whatever the call says.
fn pool_template() -> Value {
    let mut template = vm_template();
    template["name"] = json!("pool");
    template["secrets"] = json!(["API_TOKEN"]);
    template["autostart"] = json!(true);
    template
}

const TUNNEL_REASON: &str = "its template needs the secret TUNNEL_TOKEN, which is not set yet";
const POOL_REASON: &str = "its template declares the secret API_TOKEN, and a virtual machine's secrets are written into it by instance_start";
const CALL_REASON: &str = "the call said start: false";

/// A node with tenants `A` and `B` and the templates `plain`, `vm`, `db`, `web`, `tunnel` and `pool`. Every one of
/// them sets `autostart`, so each create shows whether it carries `boot.autostart`.
fn rig() -> Rig {
    let rig = two_tenants(policy());
    let mut vm = vm_template();
    vm["autostart"] = json!(true);
    rig.template("vm", vm);
    rig.template("plain", plain_template());
    rig.template("db", db_template());
    rig.template("web", web_template());
    rig.template("tunnel", tunnel_template());
    rig.template("pool", pool_template());
    rig
}

/// The body of each create Incus was sent for the instance, the first first.
fn create_bodies(rig: &Rig, instance: &str) -> Vec<Value> {
    rig.fake()
        .service_requests()
        .iter()
        .filter(|request| request.method == "POST" && request.path.starts_with("/1.0/instances?"))
        .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
        .filter(|body| body["name"] == instance)
        .collect()
}

/// Everything a rig sent Incus and wrote down: each request with its status, each write with its config keys, each
/// request's body, and the effect journal.
fn everything(rig: &Rig) -> (Vec<String>, Vec<String>, Vec<String>, String) {
    let requests = rig
        .fake()
        .requests()
        .iter()
        .map(|request| format!("{} {} {}", request.method, request.path, request.status))
        .collect();
    let writes = rig
        .fake()
        .writes()
        .iter()
        .map(|write| {
            format!(
                "{} {} {} {}",
                write.method,
                write.path,
                write.status,
                write.config_keys.join(",")
            )
        })
        .collect();
    let bodies = rig
        .fake()
        .service_requests()
        .iter()
        .map(|request| {
            format!(
                "{} {} {}",
                request.method,
                request.path,
                String::from_utf8_lossy(&request.body)
            )
        })
        .collect();
    let journal = fs::read_to_string(rig.dir.join("journal.jsonl")).expect("the effect journal");
    (requests, writes, bodies, journal)
}

/// A request sent to Incus by hand, as an operator would, outside the kernel.
fn by_hand(rig: &Rig, method: &str, path: &str) {
    use std::io::{Read, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(&rig.socket).expect("the fake");
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .expect("the request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("the answer");
    assert!(
        response.starts_with("HTTP/1.1 20"),
        "{method} {path}: {}",
        response.lines().next().unwrap_or("")
    );
}

/// Deletes the instance of the node's own service `name` by hand and has the service plugin apply its record again,
/// as the kernel does for a record whose instance is gone. Answers the body of that second create.
fn created_again_from_its_record(rig: &Rig, name: &str) -> Value {
    use arca_kernel::plugin::Plugin;
    let id = format!("service-{name}");
    let record: arca::service::ServiceConfig =
        serde_json::from_value(rig.entry(&id)).expect("the record");
    by_hand(
        rig,
        "DELETE",
        &format!("/1.0/instances/{name}?project={PROJECT}"),
    );
    assert!(
        rig.get(&format!("/1.0/instances/{name}?project={PROJECT}"))
            .is_none()
    );
    let mut provider = IncusProvider::new(
        rig.socket.clone(),
        Family {
            projects: vec![PROJECT.into()],
            scratch: false,
            droidd_projects: vec!["other".into()],
            reserved_cidrs: vec![],
        },
        rig.dir.join("secrets"),
        arca_providers::incus::RetainedSet::empty(),
    )
    .expect("a provider");
    let mut journal =
        arca_kernel::journal::Journal::open(arca_kernel::journal::MemJournal::default())
            .expect("a journal");
    Service::apply(
        &mut arca_kernel::plugin::Ctx::new(
            arca_kernel::desired::EntryId(id),
            &Service::manifest(),
            &mut provider,
            &mut journal,
            NOW,
        ),
        &record,
    )
    .expect("the create, made again from the record");
    let bodies = create_bodies(rig, name);
    assert_eq!(bodies.len(), 2, "{name} was created twice");
    bodies[1].clone()
}

/// The Incus instance of the service `name`, in the node's own project or in tenant `tenant`.
fn incus(rig: &Rig, tenant: Option<&str>, name: &str) -> Value {
    let (instance, project) = place(tenant, name);
    rig.get(&format!("/1.0/instances/{instance}?project={project}"))
        .unwrap_or_else(|| panic!("the instance of {name}"))
}

/// The instance name and the project of the service `name`.
fn place(tenant: Option<&str>, name: &str) -> (String, String) {
    match tenant {
        Some(id) => (format!("{name}-{id}"), project(id)),
        None => (name.to_string(), PROJECT.to_string()),
    }
}

/// The environment Incus holds for an instance: what its next start gives the container.
fn environment(row: &Value) -> BTreeMap<String, String> {
    row["config"]
        .as_object()
        .expect("config")
        .iter()
        .filter_map(|(key, value)| {
            Some((
                key.strip_prefix("environment.")?.to_string(),
                value.as_str()?.to_string(),
            ))
        })
        .collect()
}

/// The place in the fake's request log of each start, stop or restart sent for the instance, and of each change of
/// its config.
fn state_changes_and_patches(
    rig: &Rig,
    tenant: Option<&str>,
    name: &str,
) -> (Vec<usize>, Vec<usize>) {
    let (instance, project) = place(tenant, name);
    let state = format!("/1.0/instances/{instance}/state?project={project}");
    let config = format!("/1.0/instances/{instance}?project={project}");
    let requests = rig.fake().requests();
    let at = |method: &str, path: &str| {
        requests
            .iter()
            .enumerate()
            .filter(|(_, request)| request.method == method && request.path == path)
            .map(|(index, _)| index)
            .collect::<Vec<_>>()
    };
    (at("PUT", &state), at("PATCH", &config))
}

fn with_tenant(mut args: Value, tenant: Option<&str>) -> Value {
    if let Some(tenant) = tenant {
        args["tenant"] = json!(tenant);
    }
    args
}

/// The rows of a journal file of the state directory.
fn rows(rig: &Rig, file: &str) -> Vec<Value> {
    fs::read_to_string(rig.dir.join(file))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("a journal row"))
        .collect()
}

/// Creates `db` and `web` stopped, links them and sets a secret on each, as `call` sends each verb. Nothing is
/// started.
fn stopped_pair(
    rig: &Rig,
    tenant: Option<&str>,
    deploy: Option<&str>,
    call: &dyn Fn(&str, Value) -> Result<Value, CallError>,
) {
    for name in ["db", "web"] {
        let mut args = with_tenant(
            json!({ "name": name, "template": name, "start": false }),
            tenant,
        );
        if let Some(deploy) = deploy {
            args["deploy"] = json!(deploy);
        }
        let made = call("service_create", args).unwrap_or_else(|error| panic!("{name}: {error:?}"));
        assert_eq!(made["state"], "stopped", "{name}");
        assert_eq!(incus(rig, tenant, name)["status"], "Stopped", "{name}");
    }
    // The link first, then the provider's password: the consumer's link variable follows the new password.
    assert_eq!(
        call(
            "service_connect",
            with_tenant(json!({ "from": "web", "to": "db" }), tenant)
        )
        .expect("a link between two stopped services"),
        json!({ "env": ["DATABASE_URL"] })
    );
    for (name, key, value) in [
        ("db", "POSTGRES_PASSWORD", DB_PASSWORD),
        ("web", "ADMIN_PASSWORD", ADMIN_PASSWORD),
    ] {
        assert_eq!(
            call(
                "service_secret_set",
                with_tenant(json!({ "name": name, "key": key, "value": value }), tenant)
            )
            .unwrap_or_else(|error| panic!("{key}: {error:?}")),
            json!({ "ok": true })
        );
    }
    for name in ["db", "web"] {
        assert_eq!(incus(rig, tenant, name)["status"], "Stopped", "{name}");
        let (state, _) = state_changes_and_patches(rig, tenant, name);
        assert!(state.is_empty(), "nothing started {name}: {state:?}");
    }
}

/// What the first start of each of the pair gave it: every secret and every link variable, set while it was stopped.
/// `started` is the answer of each `instance_start`, the database's first.
fn assert_first_start_held_everything(rig: &Rig, tenant: Option<&str>, started: &[Value]) {
    for (name, answer) in ["db", "web"].iter().zip(started) {
        assert_eq!(answer["action"], "start", "{name}");
        assert_eq!(answer["status"], "Running", "{name}");
        assert_eq!(answer["changed"], true, "{name}");
        // One start, and it is the last thing sent for the instance: every config change came before it.
        let (state, patches) = state_changes_and_patches(rig, tenant, name);
        assert_eq!(state.len(), 1, "{name} was started once: {state:?}");
        assert!(
            !patches.is_empty(),
            "{name} took its secrets as config changes"
        );
        assert!(
            patches.iter().all(|patch| *patch < state[0]),
            "{name}: a config change after its first start reaches no environment until the next start"
        );
    }
    let db = incus(rig, tenant, "db");
    let web = incus(rig, tenant, "web");
    assert_eq!(db["status"], "Running");
    assert_eq!(web["status"], "Running");
    assert_eq!(
        environment(&db),
        BTreeMap::from([("POSTGRES_PASSWORD".to_string(), DB_PASSWORD.to_string())])
    );
    let seen = environment(&web);
    assert_eq!(
        seen.keys().map(String::as_str).collect::<Vec<_>>(),
        ["ADMIN_PASSWORD", "DATABASE_URL", "SESSION_KEY"]
    );
    assert_eq!(seen["ADMIN_PASSWORD"], ADMIN_PASSWORD);
    // The link variable carries the password set after the link, and the database's own address and port.
    let address = db["devices"]["eth0"]["ipv4.address"]
        .as_str()
        .expect("the database's address");
    assert_eq!(
        seen["DATABASE_URL"],
        format!("postgresql://app:{DB_PASSWORD}@{address}:5432/app")
    );
    // The secret nobody set is the one the template made at the create.
    assert!(
        seen["SESSION_KEY"].starts_with("sk_") && seen["SESSION_KEY"].len() == 35,
        "the generated key"
    );
}

#[test]
fn started_and_template_stopped_creates_are_the_same_with_start_left_out_or_true() {
    // The same creates on two nodes, one naming `start: true` and one leaving it out: a container and a virtual
    // machine that start, a container and a virtual machine their template stops, and a container in a tenant's
    // deploy. None of them makes a secret, so the two nodes hold the same bytes.
    let run = |start: Option<bool>| {
        let rig = rig();
        let mut answers = Vec::new();
        for (name, template, tenant, state, reason) in [
            ("plain", "plain", None, "running", None),
            ("box", "vm", None, "running", None),
            ("tunnel", "tunnel", None, "stopped", Some(TUNNEL_REASON)),
            ("pool", "pool", None, "stopped", Some(POOL_REASON)),
            ("plain", "plain", Some(A), "running", None),
        ] {
            let mut args = with_tenant(json!({ "name": name, "template": template }), tenant);
            if tenant.is_some() {
                args["deploy"] = json!("d1");
            }
            if let Some(start) = start {
                args["start"] = json!(start);
            }
            let made = rig
                .call("service_create", args)
                .unwrap_or_else(|error| panic!("{name}: {error:?}"));
            assert_eq!(made["state"], state, "{name}");
            // The answer of a create the template stopped says why; one that started says nothing.
            assert_eq!(
                made.get("stoppedBecause").and_then(Value::as_str),
                reason,
                "{name}"
            );
            // Started or stopped by its template, the create carries the template's autostart, and so does the record.
            let (instance, _) = place(tenant, name);
            let bodies = create_bodies(&rig, &instance);
            assert_eq!(bodies.len(), 1, "{name}");
            assert_eq!(bodies[0]["config"]["boot.autostart"], "true", "{name}");
            assert_eq!(bodies[0]["start"], state == "running", "{name}");
            assert_eq!(
                incus(&rig, tenant, name)["config"]["boot.autostart"],
                "true",
                "{name}"
            );
            answers.push(made);
        }
        let records: Vec<Value> = [
            "service-plain",
            "service-box",
            "service-tunnel",
            "service-pool",
            "t-aaaaaaaa-service-plain",
        ]
        .iter()
        .map(|id| rig.entry(id))
        .collect();
        for record in &records {
            assert_eq!(record["autostart"], true, "{record}");
        }
        let deploys = rows(&rig, "tenant-deploys.jsonl");
        (answers, everything(&rig), records, deploys)
    };
    let left_out = run(None);
    let named = run(Some(true));
    assert_eq!(left_out.0, named.0, "the answers");
    assert_eq!(left_out.1.0, named.1.0, "every request Incus received");
    assert_eq!(left_out.1.1, named.1.1, "every write and its config keys");
    assert_eq!(left_out.1.2, named.1.2, "every request's body");
    assert_eq!(left_out.1.3, named.1.3, "the effect journal");
    assert_eq!(left_out.2, named.2, "the records");
    assert_eq!(left_out.3, named.3, "the deploy journal");
}

#[test]
fn start_false_creates_the_service_stopped_and_nothing_starts_it() {
    let rig = rig();
    // A value that is not true or false is refused before anything is written.
    let before = rig.fake().writes().len();
    assert!(matches!(
        rig.call(
            "service_create",
            json!({ "name": "plain", "template": "plain", "start": "no" })
        ),
        Err(CallError::Invalid(message)) if message == "start: expected true or false"
    ));
    assert_eq!(rig.fake().writes().len(), before);
    assert!(
        !rig.entries()
            .iter()
            .any(|entry| entry.id.0 == "service-plain")
    );

    // A template with no secret at all: a default create would have started it.
    let made = rig
        .call(
            "service_create",
            json!({ "name": "plain", "template": "plain", "start": false }),
        )
        .expect("the create");
    assert_eq!(made["state"], "stopped");
    assert_eq!(made["stoppedBecause"], CALL_REASON);
    assert_eq!(made["name"], "plain");
    assert_eq!(made["template"], "plain");
    assert_eq!(made["ports"], json!([8080, 9090]));
    let row = incus(&rig, None, "plain");
    assert_eq!(row["status"], "Stopped");
    assert_eq!(made["ipv4"], row["devices"]["eth0"]["ipv4.address"]);
    let (state, _) = state_changes_and_patches(&rig, None, "plain");
    assert!(state.is_empty(), "nothing started it: {state:?}");
    // The template sets autostart, and this create carries none: Incus restores the last state of an instance
    // without the key, so a host restart does not boot a service that has never run.
    let body = &create_bodies(&rig, "plain")[0];
    assert_eq!(body["start"], false);
    assert!(body["config"].get("boot.autostart").is_none(), "{body}");
    assert!(row["config"].get("boot.autostart").is_none());
    // The record says both, and the status and the list read the same state from Incus.
    assert_eq!(rig.entry("service-plain")["start"], false);
    assert_eq!(rig.entry("service-plain")["autostart"], false);
    assert_eq!(
        rig.call("service_status", json!({ "name": "plain" }))
            .expect("status")["state"],
        "stopped"
    );
    let listed = rig.call("service_list", json!({})).expect("list");
    assert_eq!(listed["services"][0]["name"], "plain");
    assert_eq!(listed["services"][0]["state"], "stopped");
    // It is the node's as any service is: its ACL exists, and the effect journal holds its create as it holds any
    // create, an intent and then the applied effect with the instance's delete as its inverse.
    assert!(
        rig.get(&format!(
            "/1.0/network-acls/{PROJECT}-svc-plain?project=default"
        ))
        .is_some()
    );
    let effects: Vec<Value> = rows(&rig, "journal.jsonl")
        .into_iter()
        .filter(|row| row["entry"] == "service-plain" && row["target"]["kind"] == "instance")
        .collect();
    assert_eq!(effects.len(), 2, "{effects:?}");
    assert_eq!(effects[0]["state"], "Intent");
    assert_eq!(effects[1]["state"], "Applied");
    assert_eq!(effects[1]["class"], "B");
    assert_eq!(effects[1]["inverse"]["op"], "IncusDelete");
    assert_eq!(effects[1]["object"]["name"], "plain");

    // The first start is instance_start, journalled as any start of a stopped instance. Nothing writes the key then.
    let started = rig
        .call(
            "instance_start",
            json!({ "project": PROJECT, "instance": "plain" }),
        )
        .expect("the first start");
    assert_eq!(started["status"], "Running");
    assert_eq!(started["changed"], true);
    let row = incus(&rig, None, "plain");
    assert_eq!(row["status"], "Running");
    assert!(row["config"].get("boot.autostart").is_none());
    let journal = rows(&rig, "instance-actions.jsonl");
    assert_eq!(journal.len(), 2, "{journal:?}");
    assert_eq!(journal[0]["kind"], "begin");
    assert_eq!(journal[0]["action"], "start");
    assert_eq!(journal[0]["instance"], "plain");
    assert_eq!(journal[1]["kind"], "end");
    assert_eq!(journal[1]["outcome"], "started");
    assert_eq!(journal[1]["status"], "Running");
    assert_eq!(
        rig.call("service_status", json!({ "name": "plain" }))
            .expect("status")["state"],
        "running"
    );
}

#[test]
fn only_an_explicit_start_false_leaves_boot_autostart_out_and_a_create_made_again_is_the_same() {
    // A container and a virtual machine that their template stops, each created with `start` left out and with
    // `start: false`. Both are created stopped. The one difference is `boot.autostart`.
    for (name, template, reason) in [
        ("tunnel", "tunnel", TUNNEL_REASON),
        ("pool", "pool", POOL_REASON),
    ] {
        let run = |start: Option<bool>| {
            let rig = rig();
            let mut args = json!({ "name": name, "template": template });
            if let Some(start) = start {
                args["start"] = json!(start);
            }
            let made = rig.call("service_create", args).expect("the create");
            assert_eq!(made["state"], "stopped", "{name} {start:?}");
            assert_eq!(incus(&rig, None, name)["status"], "Stopped");
            let record = rig.entry(&format!("service-{name}"));
            assert_eq!(record["start"], false, "{name} {start:?}");
            let body = create_bodies(&rig, name)[0].clone();
            (rig, made, record, body)
        };
        let forced = run(None);
        let asked = run(Some(false));
        // The template's own stop keeps the template's autostart, in the record and in the body.
        assert_eq!(forced.1["stoppedBecause"], reason);
        assert_eq!(forced.2["autostart"], true, "{name}");
        assert_eq!(forced.3["config"]["boot.autostart"], "true", "{name}");
        assert_eq!(
            incus(&forced.0, None, name)["config"]["boot.autostart"],
            "true"
        );
        // The call's `start: false` records no autostart and sends none.
        assert_eq!(asked.1["stoppedBecause"], CALL_REASON);
        assert_eq!(asked.2["autostart"], false, "{name}");
        assert!(asked.3["config"].get("boot.autostart").is_none(), "{name}");
        assert!(
            incus(&asked.0, None, name)["config"]
                .get("boot.autostart")
                .is_none()
        );
        // Nothing else differs: the same requests, the same bodies but for that key, the same record but for that field.
        let mut without_key = forced.3.clone();
        without_key["config"]
            .as_object_mut()
            .expect("config")
            .remove("boot.autostart");
        assert_eq!(without_key, asked.3, "{name}: the create body");
        let mut record = forced.2.clone();
        record["autostart"] = json!(false);
        assert_eq!(record, asked.2, "{name}: the record");
        let (forced_log, asked_log) = (everything(&forced.0), everything(&asked.0));
        assert_eq!(forced_log.0, asked_log.0, "{name}: every request");
        let keys = |writes: &[String]| -> Vec<String> {
            writes
                .iter()
                .map(|write| write.replace("boot.autostart,", ""))
                .collect()
        };
        assert_ne!(
            forced_log.1, asked_log.1,
            "{name}: the key is in one create only"
        );
        assert_eq!(keys(&forced_log.1), asked_log.1, "{name}: every write");
        // A create made again from each record sends the body the first one sent.
        for (rig, _, _, body) in [&forced, &asked] {
            assert_eq!(&created_again_from_its_record(rig, name), body, "{name}");
        }
    }
}

#[test]
fn secrets_and_links_set_while_stopped_are_all_in_the_environment_of_the_first_start() {
    let rig = rig();
    stopped_pair(&rig, None, None, &|verb, args| rig.call(verb, args));
    let started: Vec<Value> = ["db", "web"]
        .iter()
        .map(|name| {
            rig.call(
                "instance_start",
                json!({ "project": PROJECT, "instance": name }),
            )
            .unwrap_or_else(|error| panic!("start {name}: {error:?}"))
        })
        .collect();
    assert_first_start_held_everything(&rig, None, &started);
}

#[test]
fn in_a_tenant_a_stopped_deploy_takes_its_secrets_and_links_starts_with_no_approval_and_is_confirmed()
 {
    let rig = rig();
    let cloud = |verb: &str, args: Value| rig.call_token("cloud", verb, args, &TOOLS);
    stopped_pair(&rig, Some(A), Some("d1"), &cloud);
    // One more service of the deploy stays stopped throughout.
    cloud(
        "service_create",
        json!({ "name": "idle", "template": "app", "tenant": A, "deploy": "d1", "start": false }),
    )
    .expect("a third service");
    // Stopped services are provisional like running ones.
    let usage = |rig: &Rig| -> Vec<(String, Value, Value)> {
        rig.call("tenant_usage", json!({ "id": A })).unwrap()["instances"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["service"].as_str().unwrap().to_string(),
                    row["state"].clone(),
                    row["provisional"].clone(),
                )
            })
            .collect()
    };
    assert_eq!(
        usage(&rig),
        [
            ("db".to_string(), json!("stopped"), json!(true)),
            ("idle".to_string(), json!("stopped"), json!(true)),
            ("web".to_string(), json!("stopped"), json!(true)),
        ]
    );
    // The first start needs no approval inside a tenant, from the token whose allowlist names the verb, by the
    // service's name.
    let started: Vec<Value> = ["db", "web"]
        .iter()
        .map(|name| {
            cloud("instance_start", json!({ "tenant": A, "service": name }))
                .unwrap_or_else(|error| panic!("start {name}: {error:?}"))
        })
        .collect();
    assert_eq!(started[1]["project"], project(A));
    assert_eq!(started[1]["instance"], "web-aaaaaaaa");
    assert_first_start_held_everything(&rig, Some(A), &started);
    let journal = rows(&rig, "instance-actions.jsonl");
    let begun: Vec<&Value> = journal
        .iter()
        .filter(|row| row["kind"] == "begin")
        .collect();
    assert_eq!(begun.len(), 2);
    for row in begun {
        assert_eq!(row["token_id"], "cloud");
        assert_eq!(row["tenant"], A);
        assert_eq!(row["proposal"], Value::Null, "no approval was spent");
    }
    // Nothing of tenant B, or of the node's own project, was made or started.
    assert!(
        rig.get(&format!("/1.0/instances/web-{B}?project={}", project(B)))
            .is_none()
    );
    assert!(
        rig.get(&format!("/1.0/instances/web?project={PROJECT}"))
            .is_none()
    );

    // The confirm takes the whole deploy, the service that never started too.
    assert_eq!(
        cloud("deploy_confirm", json!({ "tenant": A, "deploy": "d1" })).expect("the confirm"),
        json!({ "confirmed": true, "deploy": "d1" })
    );
    assert_eq!(
        usage(&rig),
        [
            ("db".to_string(), json!("running"), Value::Null),
            ("idle".to_string(), json!("stopped"), Value::Null),
            ("web".to_string(), json!("running"), Value::Null),
        ]
    );
    let (code, _) = refusal(cloud(
        "deploy_rollback",
        json!({ "tenant": A, "deploy": "d1" }),
    ));
    assert_eq!(code, "refused");
    assert_eq!(incus(&rig, Some(A), "idle")["status"], "Stopped");
}

#[test]
fn in_a_tenant_a_stopped_deploy_is_rolled_back_whole() {
    let rig = rig();
    let cloud = |verb: &str, args: Value| rig.call_token("cloud", verb, args, &TOOLS);
    stopped_pair(&rig, Some(A), Some("d2"), &cloud);
    let secrets = rig.dir.join("secrets");
    for id in ["t-aaaaaaaa-service-db", "t-aaaaaaaa-service-web"] {
        assert!(secrets.join(id).is_dir(), "{id} holds secrets");
    }
    let rolled = cloud("deploy_rollback", json!({ "tenant": A, "deploy": "d2" }))
        .expect("the rollback of a deploy that never started");
    assert_eq!(rolled["rolledBack"], true);
    let mut removed: Vec<&str> = rolled["removed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap())
        .collect();
    removed.sort_unstable();
    assert_eq!(removed, ["db", "web"]);
    // The instances, their ACLs, their records and their secrets are gone, and nothing was ever started.
    for name in ["db", "web"] {
        let (instance, project) = place(Some(A), name);
        assert!(
            rig.get(&format!("/1.0/instances/{instance}?project={project}"))
                .is_none(),
            "{name}"
        );
        assert!(
            rig.get(&format!(
                "/1.0/network-acls/{project}-svc-{name}?project=default"
            ))
            .is_none(),
            "the ACL of {name}"
        );
        assert!(
            !secrets.join(format!("t-aaaaaaaa-service-{name}")).exists(),
            "the secrets of {name}"
        );
        let (state, _) = state_changes_and_patches(&rig, Some(A), name);
        assert!(state.is_empty(), "{name}: {state:?}");
    }
    assert!(
        !rig.entries()
            .iter()
            .any(|entry| entry.id.0.contains("-service-")),
        "no service record is left"
    );
    let journal = rows(&rig, "tenant-deploys.jsonl");
    let last = journal.last().expect("the rollback's row");
    assert_eq!(last["kind"], "rollback");
    assert_eq!(last["deploy"], "d2");
    assert_eq!(last["ok"], true);
}

#[test]
fn a_virtual_machine_without_declared_secrets_is_created_stopped_the_same_way() {
    let rig = rig();
    let cloud = |verb: &str, args: Value| rig.call_token("cloud", verb, args, &TOOLS);
    // In the node's own project and in a tenant's deploy.
    for (tenant, deploy) in [(None, None), (Some(A), Some("d3"))] {
        let mut args = with_tenant(
            json!({ "name": "box", "template": "vm", "start": false }),
            tenant,
        );
        if let Some(deploy) = deploy {
            args["deploy"] = json!(deploy);
        }
        let made = cloud("service_create", args).expect("the machine");
        assert_eq!(made["state"], "stopped", "{tenant:?}");
        assert_eq!(made["stoppedBecause"], CALL_REASON);
        let row = incus(&rig, tenant, "box");
        assert_eq!(row["type"], "virtual-machine");
        assert_eq!(row["status"], "Stopped");
        let (state, _) = state_changes_and_patches(&rig, tenant, "box");
        assert!(state.is_empty(), "nothing started it: {state:?}");
        // The template sets autostart, and neither the create nor the record carries it.
        let (instance, project) = place(tenant, "box");
        let body = &create_bodies(&rig, &instance)[0];
        assert_eq!(body["type"], "virtual-machine");
        assert_eq!(body["start"], false);
        assert!(body["config"].get("boot.autostart").is_none(), "{body}");
        assert!(row["config"].get("boot.autostart").is_none());
        let id = match tenant {
            Some(_) => "t-aaaaaaaa-service-box",
            None => "service-box",
        };
        assert_eq!(rig.entry(id)["start"], false);
        assert_eq!(rig.entry(id)["autostart"], false);
        let start = match tenant {
            Some(tenant) => json!({ "tenant": tenant, "service": "box" }),
            None => json!({ "project": project, "instance": instance }),
        };
        let started = cloud("instance_start", start).expect("the first start");
        assert_eq!(started["status"], "Running");
        assert_eq!(started["changed"], true);
        let row = incus(&rig, tenant, "box");
        assert_eq!(row["status"], "Running");
        assert!(row["config"].get("boot.autostart").is_none());
        let (state, _) = state_changes_and_patches(&rig, tenant, "box");
        assert_eq!(state.len(), 1);
    }
    // A stopped machine of a provisional deploy is confirmed like any other.
    assert_eq!(
        cloud("deploy_confirm", json!({ "tenant": A, "deploy": "d3" })).expect("the confirm"),
        json!({ "confirmed": true, "deploy": "d3" })
    );
}

#[test]
fn the_card_says_what_will_happen_and_an_approved_start_false_create_runs_to_its_first_start() {
    let rig = rig();
    let propose = |args: Value| -> Value {
        rig.call(
            "action_propose",
            json!({ "action": "service_create", "args": args }),
        )
        .expect("the proposal")
    };
    let start_row = |proposal: &Value| -> Option<String> {
        proposal["details"]
            .as_array()
            .expect("details")
            .iter()
            .find(|row| row["label"] == "Start")
            .map(|row| row["value"].as_str().expect("words").to_string())
    };
    // A create that starts has no Start row unless the call names one.
    let plain = propose(json!({ "name": "p0", "template": "plain" }));
    assert_eq!(start_row(&plain), None);
    assert_eq!(
        plain["summary"],
        "Create the service p0 from template plain"
    );
    // Issue 117: the card says what the service takes of the node, read from its template, and how to undo it.
    assert_eq!(
        plain["uses"],
        "Up to 2 CPUs and 2GiB of memory (Arca's default for a service), and the disk its files take."
    );
    assert_eq!(
        plain["undo"],
        "Ask your assistant to remove it. Removing it deletes the service with its data."
    );
    let named = propose(json!({ "name": "p1", "template": "plain", "start": true }));
    assert_eq!(
        start_row(&named).as_deref(),
        Some(arca::action_proposal::start_words(true))
    );
    // A template that is created stopped whatever the call says: the row says so, and why, with `start` left out
    // and with `start: true`. Saying "Yes" there would be untrue.
    for (name, template, reason) in [("t", "tunnel", TUNNEL_REASON), ("m", "pool", POOL_REASON)] {
        for (index, start) in [None, Some(true)].into_iter().enumerate() {
            let mut args = json!({ "name": format!("{name}{index}"), "template": template });
            if let Some(start) = start {
                args["start"] = json!(start);
            }
            let proposal = propose(args);
            assert_eq!(
                start_row(&proposal),
                Some(format!(
                    "No: it is created stopped because {reason}; it can be started later"
                )),
                "{template} {start:?}"
            );
        }
    }
    // The call's own `start: false`: the summary and the row say it.
    let stopped = propose(json!({ "name": "plain", "template": "plain", "start": false }));
    assert_eq!(
        stopped["summary"],
        "Create the service plain from template plain and leave it stopped"
    );
    assert_eq!(
        start_row(&stopped).as_deref(),
        Some("No: it is created stopped, and can be started later")
    );
    assert_eq!(stopped["args"]["start"], false);
    assert!(
        rig.get(&format!("/1.0/instances/plain?project={PROJECT}"))
            .is_none()
    );

    // The owner approves it, and it runs: the same proposal, the real handler.
    let made = rig
        .approved(
            "service_create",
            json!({ "name": "plain", "template": "plain", "start": false }),
        )
        .expect("the approved create");
    assert_eq!(made["state"], "stopped");
    assert_eq!(made["stoppedBecause"], CALL_REASON);
    let row = incus(&rig, None, "plain");
    assert_eq!(row["status"], "Stopped");
    assert!(row["config"].get("boot.autostart").is_none());
    assert_eq!(rig.entry("service-plain")["start"], false);
    // Its first start, and nothing before it.
    let (state, _) = state_changes_and_patches(&rig, None, "plain");
    assert!(state.is_empty(), "{state:?}");
    let started = rig
        .call(
            "instance_start",
            json!({ "project": PROJECT, "instance": "plain" }),
        )
        .expect("the first start");
    assert_eq!(started["status"], "Running");
    assert_eq!(incus(&rig, None, "plain")["status"], "Running");
}

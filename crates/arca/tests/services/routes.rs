//! Routes against the fake Incus: an edge service made from an edge template, an instance of another project it serves,
//! and route_add, route_list and route_remove writing the edge's record, its Caddyfile and its NIC rules. Every name is
//! example.com and every address synthetic.
use super::*;
use arca::route::{self, EdgeRecord, Reach, Route};
use arca_incus::exec::{EDGE_CONFIG, EDGE_CONFIG_KEPT, ExecCommand};
use arca_incus::fake::ExecScript;

/// The address the served instance holds on its own bridge: private, outside the service /24.
const TARGET: &str = "10.20.30.63";

fn edge_template() -> Value {
    json!({"name":"edge","kind":"oci","image":"docker.io/library/example","digest":DIGEST,
        "ports":[443],"env":{},"lan":{"port":443},"edge":true})
}

/// A node with an edge (running) and an instance `app` in the project `other`, which Arca does not manage.
fn edge_rig() -> Rig {
    let rig = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![hand_instance(
                "app",
                "other",
                json!({"eth0":{"type":"nic","name":"eth0","network":"br1","ipv4.address":TARGET}}),
            )],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.to_string()],
        Some("192.0.2.0/24"),
    );
    rig.template("edge", edge_template());
    rig.call("service_create", json!({"name":"edge","template":"edge"}))
        .expect("the edge");
    rig
}

fn reload(rig: &Rig, exit: i64) {
    rig.fake
        .script_exec(ExecScript::new(ExecCommand::EdgeReload.argv(), exit, ""));
}

fn written(rig: &Rig) -> String {
    file(rig, EDGE_CONFIG).expect("the edge's config was written")
}

fn file(rig: &Rig, path: &str) -> Option<String> {
    rig.fake
        .service_files()
        .get(&format!("{PROJECT}/edge:{path}"))
        .map(|bytes| String::from_utf8(bytes.clone()).unwrap())
}

fn record(rig: &Rig) -> EdgeRecord {
    rig.service_config("edge").edge.expect("an edge record")
}

fn egress(rig: &Rig) -> Vec<Value> {
    rig.get(&format!(
        "/1.0/network-acls/{PROJECT}-svc-edge?project=default"
    ))
    .expect("the edge's ACL")["egress"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn opens(rules: &[Value], address: &str, port: u16) -> bool {
    rules.iter().any(|rule| {
        rule["action"] == "allow"
            && rule["destination"].as_str() == Some(&format!("{address}/32"))
            && rule["destination_port"]
                .as_str()
                .is_some_and(|ports| ports.split(',').any(|p| p == port.to_string()))
    })
}

#[test]
fn an_edge_records_routes_and_its_host_port_sends_the_proxy_header() {
    let rig = edge_rig();
    let cfg = rig.service_config("edge");
    assert_eq!(cfg.edge, Some(EdgeRecord::default()));
    let instance = rig
        .get(&format!("/1.0/instances/edge?project={PROJECT}"))
        .expect("the edge instance");
    assert_eq!(instance["devices"]["web"]["proxy_protocol"], "true");
    // An ordinary service has neither.
    rig.template("nats", nats_template());
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("an ordinary service");
    assert!(rig.service_config("nats").edge.is_none());
    assert!(
        serde_json::to_value(rig.service_config("nats"))
            .unwrap()
            .get("edge")
            .is_none()
    );
}

#[test]
fn route_add_records_the_route_writes_the_whole_config_reloads_and_opens_the_target() {
    let rig = edge_rig();
    assert!(!opens(&egress(&rig), TARGET, 8317));
    reload(&rig, 0);
    let answer = rig
        .call(
            "route_add",
            json!({"hostname":"LLM.example.com","instance":"app","project":"other","port":8317}),
        )
        .expect("the route");
    assert_eq!(answer["hostname"], "llm.example.com");
    assert_eq!(answer["url"], "https://llm.example.com");
    assert_eq!(answer["address"], TARGET);
    assert_eq!(answer["reach"], "private");
    assert_eq!(answer["applied"], true);
    assert_eq!(answer["reloaded"], true);
    assert_eq!(answer["edge"]["name"], "edge");
    let expected = Route {
        hostname: "llm.example.com".into(),
        instance: "app".into(),
        project: "other".into(),
        address: TARGET.parse().unwrap(),
        port: 8317,
        reach: Reach::Private,
    };
    assert_eq!(record(&rig).routes, vec![expected]);
    assert_eq!(written(&rig), route::caddyfile(&record(&rig)));
    assert!(written(&rig).contains("reverse_proxy 10.20.30.63:8317"));
    assert!(written(&rig).contains("respond @outside 403"));
    assert!(opens(&egress(&rig), TARGET, 8317));
    // The reload ran in the edge, as the closed command.
    assert!(
        rig.fake
            .service_requests()
            .iter()
            .any(|request| request.path.contains("/instances/edge/exec"))
    );
    let list = rig.call("route_list", json!({})).expect("the list");
    assert_eq!(list["edge"]["name"], "edge");
    assert_eq!(list["routes"][0]["hostname"], "llm.example.com");
    assert_eq!(list["routes"].as_array().map(Vec::len), Some(1));
}

#[test]
fn adding_a_hostname_again_re_points_it_and_remove_takes_it_all_back() {
    let rig = edge_rig();
    reload(&rig, 0);
    rig.call(
        "route_add",
        json!({"hostname":"app.example.com","instance":"app","project":"other","port":8080}),
    )
    .expect("the route");
    reload(&rig, 0);
    rig.call(
        "route_add",
        json!({"hostname":"app.example.com","instance":"app","project":"other","port":9090,"reach":"public"}),
    )
    .expect("re-pointed");
    let routes = record(&rig).routes;
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].port, 9090);
    assert_eq!(routes[0].reach, Reach::Public);
    assert!(!written(&rig).contains("remote_ip"));
    assert!(opens(&egress(&rig), TARGET, 9090));
    assert!(!opens(&egress(&rig), TARGET, 8080));
    reload(&rig, 0);
    let removed = rig
        .call("route_remove", json!({"hostname":"app.example.com"}))
        .expect("removed");
    assert_eq!(removed["removed"], true);
    assert_eq!(removed["reloaded"], true);
    assert!(record(&rig).routes.is_empty());
    assert!(!written(&rig).contains("app.example.com"));
    assert!(!opens(&egress(&rig), TARGET, 9090));
    // A remove of a name that has no route writes the config again and reloads, so it can be retried.
    reload(&rig, 0);
    let again = rig
        .call("route_remove", json!({"hostname":"app.example.com"}))
        .expect("written again");
    assert_eq!(again["removed"], false);
    assert_eq!(again["applied"], true);
    assert_eq!(again["reloaded"], true);
    assert!(record(&rig).routes.is_empty());
}

#[test]
fn a_config_caddy_took_is_kept_on_the_data_volume_and_a_refused_one_is_taken_back() {
    let rig = edge_rig();
    reload(&rig, 0);
    rig.call(
        "route_add",
        json!({"hostname":"app.example.com","instance":"app","project":"other","port":8080}),
    )
    .expect("the route");
    let good = route::caddyfile(&record(&rig));
    assert_eq!(written(&rig), good);
    assert_eq!(file(&rig, EDGE_CONFIG_KEPT).as_deref(), Some(good.as_str()));
    // Caddy refuses the next one: the root file goes back to the config that serves, and the kept copy stays.
    reload(&rig, 1);
    let refused = rig
        .call(
            "route_add",
            json!({"hostname":"two.example.com","instance":"app","project":"other","port":9090}),
        )
        .expect("recorded");
    assert_eq!(refused["applied"], false);
    assert!(
        refused["note"]
            .as_str()
            .is_some_and(|note| note.contains("back in its file")),
        "{refused}"
    );
    assert_eq!(written(&rig), good);
    assert_eq!(file(&rig, EDGE_CONFIG_KEPT).as_deref(), Some(good.as_str()));
    // The same add again retries the write and the reload.
    reload(&rig, 0);
    let retried = rig
        .call(
            "route_add",
            json!({"hostname":"two.example.com","instance":"app","project":"other","port":9090}),
        )
        .expect("retried");
    assert_eq!(retried["reloaded"], true);
    assert!(written(&rig).contains("two.example.com"));
    assert_eq!(file(&rig, EDGE_CONFIG_KEPT), Some(written(&rig)));
}

#[test]
fn a_remove_the_edge_did_not_take_says_the_route_is_gone_and_is_retried_by_removing_again() {
    let rig = edge_rig();
    reload(&rig, 0);
    rig.call(
        "route_add",
        json!({"hostname":"app.example.com","instance":"app","project":"other","port":8080}),
    )
    .expect("the route");
    reload(&rig, 1);
    let failed = rig
        .call("route_remove", json!({"hostname":"app.example.com"}))
        .expect("removed from the record");
    assert_eq!(failed["removed"], true);
    assert_eq!(failed["applied"], false);
    let note = failed["note"].as_str().expect("a note");
    assert!(note.contains("no longer recorded"), "{note}");
    assert!(note.contains("Remove it again"), "{note}");
    assert!(!note.contains("Add the route again"), "{note}");
    assert!(record(&rig).routes.is_empty());
    reload(&rig, 0);
    let again = rig
        .call("route_remove", json!({"hostname":"app.example.com"}))
        .expect("retried");
    assert_eq!(again["removed"], false);
    assert_eq!(again["reloaded"], true);
    assert!(!written(&rig).contains("app.example.com"));
}

/// The template's entrypoint script, run by the host's `sh` with its paths moved into a scratch directory and a
/// stand-in for Caddy that writes down its arguments.
fn edge_script(live: Option<&str>, kept: Option<&str>) -> (String, String, String) {
    use std::os::unix::fs::PermissionsExt;
    let script = fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates/arca-edge.sh"),
    )
    .expect("the script");
    let work = std::env::temp_dir().join(format!(
        "arca-edge-script-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&work).unwrap();
    let (live_path, kept_path, args, caddy) = (
        work.join("Caddyfile"),
        work.join("kept"),
        work.join("args"),
        work.join("caddy"),
    );
    if let Some(text) = live {
        fs::write(&live_path, text).unwrap();
    }
    if let Some(text) = kept {
        fs::write(&kept_path, text).unwrap();
    }
    fs::write(
        &caddy,
        format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", args.display()),
    )
    .unwrap();
    fs::set_permissions(&caddy, fs::Permissions::from_mode(0o755)).unwrap();
    let output = std::process::Command::new("/bin/sh")
        .args(["-ec", &script, "arca-edge"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("ARCA_EDGE_CONFIG", &live_path)
        .env("ARCA_EDGE_KEPT", &kept_path)
        .env("ARCA_EDGE_CADDY", &caddy)
        .output()
        .expect("the shell runs");
    assert!(output.status.success(), "{output:?}");
    let ran = (
        fs::read_to_string(&live_path).unwrap_or_default(),
        fs::read_to_string(&args).unwrap_or_default(),
        fs::read_to_string(&kept_path).unwrap_or_default(),
    );
    fs::remove_dir_all(&work).unwrap();
    ran
}

#[test]
fn the_edge_entrypoint_puts_the_kept_config_back_after_a_rebuild_and_keeps_a_newer_one_of_arcas() {
    let mut routed = EdgeRecord::default();
    routed.put(Route {
        hostname: "app.example.com".into(),
        instance: "app".into(),
        project: "other".into(),
        address: TARGET.parse().unwrap(),
        port: 8080,
        reach: Reach::Private,
    });
    let kept = route::caddyfile(&routed);
    let newer = route::caddyfile(&EdgeRecord::default());
    let stock = ":80 {\n\troot * /usr/share/caddy\n\tfile_server\n}\n";
    // A rebuilt root holds the image's own file: the kept copy goes back, and Caddy runs it.
    let (live, args, _) = edge_script(Some(stock), Some(&kept));
    assert_eq!(live, kept);
    assert!(args.starts_with("run\n--config\n"), "{args}");
    assert!(args.ends_with("--adapter\ncaddyfile\n"), "{args}");
    // A root file of Arca's is the newer (a route changed while the edge was stopped): it stays, and the volume keeps
    // it from now on, so a rebuild after that change brings the change back.
    let (live, _, on_volume) = edge_script(Some(&newer), Some(&kept));
    assert_eq!(live, newer);
    assert_eq!(on_volume, newer);
    let (live, _, on_volume) = edge_script(Some(&newer), None);
    assert_eq!(live, newer);
    assert_eq!(on_volume, newer);
    // Nothing kept yet and nothing of Arca's: the image's file runs as it is, and nothing is kept.
    let (live, _, on_volume) = edge_script(Some(stock), None);
    assert_eq!(live, stock);
    assert_eq!(on_volume, "");
}

#[test]
fn a_reload_caddy_refuses_keeps_the_route_and_says_so() {
    let rig = edge_rig();
    reload(&rig, 1);
    let answer = rig
        .call(
            "route_add",
            json!({"hostname":"app.example.com","instance":"app","project":"other","port":8080}),
        )
        .expect("recorded");
    assert_eq!(answer["applied"], false);
    assert_eq!(answer["reloaded"], false);
    assert!(
        answer["note"]
            .as_str()
            .is_some_and(|note| note.contains("old config still serves")),
        "{answer}"
    );
    assert_eq!(record(&rig).routes.len(), 1);
}

#[test]
fn a_route_needs_an_edge_a_real_instance_and_a_dns_name() {
    let rig = Rig::with(
        FakeState {
            server: json!({}),
            projects: vec![],
            instances: vec![hand_instance(
                "app",
                "other",
                json!({"eth0":{"type":"nic","name":"eth0","ipv4.address":TARGET}}),
            )],
            networks: vec![],
            pools: vec![],
        },
        vec![PROJECT.to_string()],
        Some("192.0.2.0/24"),
    );
    rig.template("nats", nats_template());
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("an ordinary service");
    let no_edge = rig
        .call(
            "route_add",
            json!({"hostname":"app.example.com","instance":"app","project":"other","port":8080}),
        )
        .expect_err("no edge");
    assert!(
        matches!(&no_edge, CallError::Refused { message, .. } if message.contains("arca-edge"))
    );
    assert_eq!(
        rig.call("route_list", json!({})).expect("the list"),
        json!({"edge": null, "routes": []})
    );
    let rig = edge_rig();
    for (args, what) in [
        (
            json!({"hostname":"*.example.com","instance":"app","project":"other","port":8080}),
            "a wildcard",
        ),
        (
            json!({"hostname":"app.example.com\nrespond 200","instance":"app","project":"other","port":8080}),
            "a line break",
        ),
        (
            json!({"hostname":"app","instance":"app","project":"other","port":8080}),
            "one label",
        ),
        (
            json!({"hostname":"app.example.com","instance":"app","project":"other","port":0}),
            "port zero",
        ),
        (
            json!({"hostname":"app.example.com","instance":"app","project":"other","port":8080,"reach":"funnel"}),
            "an unknown reach",
        ),
    ] {
        assert!(
            matches!(rig.call("route_add", args), Err(CallError::Invalid(_))),
            "{what}"
        );
    }
    let missing = rig
        .call(
            "route_add",
            json!({"hostname":"app.example.com","instance":"ghost","project":"other","port":8080}),
        )
        .expect_err("no such instance");
    assert!(matches!(missing, CallError::Refused { code, .. } if code == "not_found"));
    assert!(record(&rig).routes.is_empty());
    let viewer = rig.handlers["route_add"].call(
        json!({"hostname":"app.example.com","instance":"app","project":"other","port":8080}),
        &Caller {
            token_id: "viewer".into(),
            scope: Scope::Viewer,
            tools: None,
            limits: Default::default(),
        },
    );
    assert!(matches!(viewer, Err(CallError::Forbidden { .. })));
}

#[test]
fn a_route_to_an_arca_service_rides_the_edges_link() {
    let rig = edge_rig();
    rig.template("nats", nats_template());
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("a service");
    let unlinked = rig
        .call(
            "route_add",
            json!({"hostname":"bus.example.com","instance":"nats","port":4222}),
        )
        .expect_err("no link yet");
    assert!(
        matches!(&unlinked, CallError::Refused { message, .. } if message.contains("service_connect"))
    );
    rig.call("service_connect", json!({"from":"edge","to":"nats"}))
        .expect("linked");
    let port = rig.service_config("edge").links[0].port;
    reload(&rig, 0);
    let answer = rig
        .call(
            "route_add",
            json!({"hostname":"bus.example.com","instance":"nats","port":port}),
        )
        .expect("the route");
    assert_eq!(answer["project"], PROJECT);
    assert_eq!(answer["address"], rig.service_config("nats").ipv4);
}

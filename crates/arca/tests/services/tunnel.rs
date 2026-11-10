//! The shipped `cloudflare-tunnel` template on the service model, against the fake Incus: no tunnel is made and nothing
//! reaches Cloudflare. The token and every address are synthetic.
use super::*;
use arca::service_exec::AUDIT_FILE;

const TEMPLATE: &str = "cloudflare-tunnel";
/// Shaped like a tunnel token (a long base64 text) and synthetic.
const TOKEN: &str = "eyJhIjoic3ludGhldGljLWFjY291bnQiLCJ0Ijoic3ludGhldGljLXR1bm5lbCIsInMiOiJzeW50aGV0aWMtc2VjcmV0In0";

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
/// The rig every other service test uses (the service project alone, no other kernel setting), with the shipped templates
/// installed as `arca templates install` puts them, a tunnel, and two plain services it could reach.
fn tunnel_rig() -> Rig {
    let rig = Rig::new();
    arca::cmd_templates::install_into(&rig.dir.join("templates"), false).expect("install");
    for (name, port) in [("web", 8080), ("other", 9090)] {
        rig.template(
            name,
            json!({"name":name,"kind":"oci","image":"docker.io/library/example","digest":DIGEST,"ports":[port],"env":{}}),
        );
        rig.call("service_create", json!({"name":name,"template":name}))
            .expect("create");
    }
    rig.call(
        "service_create",
        json!({"name":"tunnel","template":TEMPLATE}),
    )
    .expect("create the tunnel");
    rig
}

#[test]
fn the_shipped_tunnel_template_loads_pinned_by_digest_with_no_port_and_no_token() {
    let template = service::load_template(&shipped(), TEMPLATE).expect("the template loads");
    assert_eq!(template.kind, "oci");
    assert_eq!(template.image, "docker.io/cloudflare/cloudflared");
    let digest = template.digest.as_deref().expect("a digest");
    let hex = digest.strip_prefix("sha256:").expect("sha256");
    assert!(hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()));
    // Incus pulls the image by that digest and by no tag.
    let source = service::source(&template).expect("source");
    assert_eq!(source["protocol"], "oci");
    assert_eq!(source["server"], "https://docker.io");
    assert_eq!(source["alias"], format!("cloudflare/cloudflared@{digest}"));
    // The version the digest is of is in the description.
    assert!(
        template
            .description
            .as_deref()
            .unwrap()
            .contains("cloudflared 2026.10.7")
    );
    // No port of its own, no host listener, no management port, no publication.
    assert!(template.ports.is_empty());
    assert!(
        template.lan.is_none() && template.droidd_port.is_none() && template.dash_port.is_none()
    );
    assert!(!template.publishable && template.provides.is_none() && template.data.is_none());
    // A remotely-managed tunnel: `tunnel --no-autoupdate run`, and the token is no argument.
    assert_eq!(
        service::entrypoint(&template, &shipped())
            .unwrap()
            .as_deref(),
        Some("/usr/local/bin/cloudflared tunnel --no-autoupdate run")
    );
    // The token is the owner's secret: declared, never generated, and in the template only as a placeholder.
    assert_eq!(template.secrets, vec!["TUNNEL_TOKEN".to_string()]);
    assert!(template.generate.is_empty());
    assert_eq!(
        template.env,
        std::collections::BTreeMap::from([(
            "TUNNEL_TOKEN".to_string(),
            "{{secret:TUNNEL_TOKEN}}".to_string()
        )])
    );
}

#[test]
fn a_tunnel_is_an_unprivileged_container_in_the_service_project_with_no_port_and_waits_for_its_token()
 {
    let rig = tunnel_rig();
    let status = rig
        .call("service_status", json!({"name":"tunnel"}))
        .expect("status");
    assert_eq!(status["template"], TEMPLATE);
    assert_eq!(status["ports"], json!([]));
    assert_eq!(status["lanPort"], Value::Null);
    assert_eq!(status["published"], Value::Null);
    // Without its token it is created and not started.
    assert_eq!(status["state"], "stopped");
    assert!(!rig.service_config("tunnel").start);
    let live = instance(&rig, "tunnel");
    assert_eq!(live["type"], "container");
    assert_eq!(live["profiles"], json!([arca_netproject::PROFILE]));
    let config = live["config"].as_object().unwrap();
    assert!(config.get(service::PORTS_KEY).is_none());
    assert!(config.get("environment.TUNNEL_TOKEN").is_none());
    assert!(config.keys().all(|key| !key.starts_with("security.")));
    assert_eq!(
        config["oci.entrypoint"],
        "/usr/local/bin/cloudflared tunnel --no-autoupdate run"
    );
    // Its devices are a root disk and its one NIC: no proxy, so nothing listens on the host.
    let devices = live["devices"].as_object().unwrap();
    assert_eq!(
        devices.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["eth0", "root"]
    );
    // Nothing may connect to it.
    assert_eq!(acl(&rig, "tunnel")["ingress"], json!([]));
}

#[test]
fn the_tunnel_token_is_a_reference_everywhere_arca_records_or_answers_and_only_the_containers_environment_holds_it()
 {
    let rig = tunnel_rig();
    let answer = op(
        &rig,
        "service_secret_set",
        json!({"name":"tunnel","key":"TUNNEL_TOKEN","value":TOKEN}),
    )
    .expect("set the token");
    assert!(!answer.to_string().contains(TOKEN));
    // The record names a secret file. It never holds the value.
    let record = rig.service_config("tunnel");
    for reference in [
        &record.secret_refs["TUNNEL_TOKEN"],
        &record.environment["TUNNEL_TOKEN"],
    ] {
        assert!(reference.starts_with("arca-secret:service-tunnel/"));
    }
    for file in ["desired.json", "journal.jsonl", AUDIT_FILE] {
        if let Ok(text) = fs::read_to_string(rig.dir.join(file)) {
            assert!(!text.contains(TOKEN), "{file}");
        }
    }
    let journal = arca::handlers::JournalListHandler {
        state_dir: rig.dir.clone(),
    }
    .call(
        json!({}),
        &Caller {
            token_id: "viewer".into(),
            scope: Scope::Viewer,
            tools: None,
        },
    )
    .expect("journal");
    assert!(!journal.to_string().contains(TOKEN));
    for (verb, args) in [
        ("service_status", json!({"name":"tunnel"})),
        ("service_list", json!({})),
    ] {
        assert!(!rig.call(verb, args).unwrap().to_string().contains(TOKEN));
    }
    // A log line that carries it is answered without it.
    rig.fake
        .set_console_log(PROJECT, "tunnel", &format!("token {TOKEN} rejected"));
    let logs = rig
        .call("service_logs", json!({"name":"tunnel"}))
        .expect("logs")
        .to_string();
    assert!(!logs.contains(TOKEN) && logs.contains("[secret]"));
    // In Incus the one key that holds it is the container's environment, which is how cloudflared reads it. The
    // command line and every Arca key are without it.
    let live = instance(&rig, "tunnel");
    for (key, value) in live["config"].as_object().unwrap() {
        assert_eq!(
            value.to_string().contains(TOKEN),
            key == "environment.TUNNEL_TOKEN",
            "{key}"
        );
    }
    assert!(!live["devices"].to_string().contains(TOKEN));
    // The shipped file itself never changes.
    assert!(
        !fs::read_to_string(rig.dir.join("templates/cloudflare-tunnel.json"))
            .unwrap()
            .contains(TOKEN)
    );
}

#[test]
fn a_link_from_the_tunnel_opens_the_linked_services_port_to_it_and_nothing_else() {
    let rig = tunnel_rig();
    let address = |name: &str| {
        instance(&rig, name)["config"][service::ADDRESS_KEY]
            .as_str()
            .unwrap()
            .to_string()
    };
    // Before any link no service admits the tunnel.
    for name in ["web", "other", "tunnel"] {
        assert_eq!(acl(&rig, name)["ingress"], json!([]), "{name}");
    }
    assert_eq!(
        op(&rig, "service_connect", json!({"from":"tunnel","to":"web"})).expect("connect"),
        json!({"env":[]})
    );
    // The linked service admits the tunnel's address on its one port, over TCP.
    let web = acl(&rig, "web");
    let rules = web["ingress"].as_array().unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0]["action"], "allow");
    assert_eq!(rules[0]["source"], format!("{}/32", address("tunnel")));
    assert_eq!(rules[0]["protocol"], "tcp");
    assert_eq!(rules[0]["destination_port"], "8080");
    // The service it is not linked to still admits nothing, and nothing may connect to the tunnel.
    assert_eq!(acl(&rig, "other")["ingress"], json!([]));
    assert_eq!(acl(&rig, "tunnel")["ingress"], json!([]));
    // Every NIC refuses what its ACL does not allow, so the unlinked service stays closed to the tunnel.
    for name in ["web", "other", "tunnel"] {
        let nic = &instance(&rig, name)["devices"]["eth0"];
        assert_eq!(nic["security.acls.default.ingress.action"], "reject");
        assert_eq!(nic["security.acls.default.egress.action"], "reject");
    }
    // The link is in the tunnel's status with the address a public hostname points at.
    let status = rig
        .call("service_status", json!({"name":"tunnel"}))
        .unwrap();
    assert_eq!(status["links"], json!([{"to":"web","port":8080}]));
    let target = rig.call("service_status", json!({"name":"web"})).unwrap();
    assert_eq!(target["ipv4"], address("web"));
    assert_eq!(target["ports"], json!([8080]));
    // The link changed no egress: the tunnel's is the one every service has, ending in the internet rule that names
    // no protocol and no port, so cloudflared's outbound 7844 (TCP and UDP) and 443 pass it.
    let egress = acl(&rig, "tunnel")["egress"].clone();
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
    // Taking the link away closes the port again.
    op(
        &rig,
        "service_disconnect",
        json!({"from":"tunnel","to":"web"}),
    )
    .expect("disconnect");
    assert_eq!(acl(&rig, "web")["ingress"], json!([]));
    assert_eq!(
        rig.call("service_status", json!({"name":"tunnel"}))
            .unwrap()["links"],
        json!([])
    );
}

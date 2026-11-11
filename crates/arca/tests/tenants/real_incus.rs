//! The scratch probe: two tenants on a real Incus, on the node's own managed bridge, with no change to the host. It is
//! ignored by default and runs only where `ARCA_TENANT_PROBE_SOCKET` names the local Incus socket:
//!
//! ```sh
//! ARCA_TENANT_PROBE_SOCKET=/var/lib/incus/unix.socket ARCA_TENANT_PROBE_CIDR=<the node's serviceCidr> \
//!   cargo test -p arca --test tenants -- --ignored --nocapture scratch_probe
//! ```
//!
//! The kernel is a scratch one (`kernel.scratch`), with its state in a temporary directory: every project it makes is
//! `arcak-scratch-t-<id>`, and it writes nothing in the node's own project. It removes everything it made, and then lists
//! what is left under the scratch tenant prefix; the test fails if anything is.
use super::*;
use std::process::Command;
use std::time::{Duration, Instant};

fn incus(args: &[&str]) -> (bool, String) {
    let output = Command::new("incus")
        .args(args)
        .output()
        .expect("incus runs");
    (
        output.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

/// Runs a shell line inside a container of `project`, and answers whether it succeeded, with its output.
fn inside(project: &str, name: &str, line: &str) -> (bool, String) {
    incus(&["exec", "--project", project, name, "--", "sh", "-c", line])
}

fn wait_for(what: &str, limit: Duration, mut check: impl FnMut() -> bool) {
    let start = Instant::now();
    while !check() {
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// The two projects the probe makes: its own two tenants, and nothing else under the scratch tenant prefix.
const OWN: [&str; 2] = ["arcak-scratch-t-aaaaaaaa", "arcak-scratch-t-bbbbbbbb"];

/// Whether an Incus name is one of the probe's own: one of its two projects, or an ACL of one of them.
fn own(name: &str) -> bool {
    OWN.iter()
        .any(|project| name == *project || name.starts_with(&format!("{project}-")))
}

/// Every project and ACL of the probe's own two tenants that is on the host.
fn leftovers() -> Vec<String> {
    let mut found = Vec::new();
    for (kind, args) in [
        ("project", &["project", "list", "-f", "csv", "-c", "n"][..]),
        (
            "acl",
            // The ACL listing takes no column flag: the name is its first column.
            &["network", "acl", "list", "-f", "csv"][..],
        ),
    ] {
        // A listing that fails says nothing about what is left: it is a failure, never "nothing left".
        let (listed, text) = incus(args);
        assert!(listed, "incus could not list the {kind}s: {text}");
        for line in text.lines() {
            let name = line.split([',', ' ']).next().unwrap_or("");
            if own(name) {
                found.push(format!("{kind} {name}"));
            }
        }
    }
    found
}

/// Removes by force what the probe itself made and the kernel's own removal left: every instance, profile and image
/// of the probe's own two projects, the projects, and their ACLs ([`OWN`]). It touches nothing else, and no other
/// project under the scratch tenant prefix.
fn force_cleanup() {
    for left in leftovers() {
        let Some(project) = left.strip_prefix("project ") else {
            continue;
        };
        let (_, instances) = incus(&["list", "--project", project, "-f", "csv", "-c", "n"]);
        for name in instances.lines().filter(|name| !name.trim().is_empty()) {
            let (ok, out) = incus(&["delete", "--force", "--project", project, name.trim()]);
            println!(
                "forced: instance {project}/{} removed: {ok} {}",
                name.trim(),
                out.trim()
            );
        }
        let (_, images) = incus(&[
            "image",
            "list",
            "--project",
            project,
            "-f",
            "csv",
            "-c",
            "f",
        ]);
        for image in images.lines().filter(|image| !image.trim().is_empty()) {
            let _ = incus(&["image", "delete", "--project", project, image.trim()]);
        }
        let _ = incus(&["profile", "delete", "--project", project, "arca-service"]);
        let (ok, out) = incus(&["project", "delete", project]);
        println!("forced: project {project} removed: {ok} {}", out.trim());
    }
    for left in leftovers() {
        if let Some(acl) = left.strip_prefix("acl ") {
            let (ok, out) = incus(&["network", "acl", "delete", acl]);
            println!("forced: ACL {acl} removed: {ok} {}", out.trim());
        }
    }
}

#[test]
#[ignore = "writes to a real Incus: set ARCA_TENANT_PROBE_SOCKET and ARCA_TENANT_PROBE_CIDR"]
fn scratch_probe_two_tenants_on_the_nodes_own_bridge() {
    let socket = std::env::var("ARCA_TENANT_PROBE_SOCKET").expect("ARCA_TENANT_PROBE_SOCKET");
    let cidr = std::env::var("ARCA_TENANT_PROBE_CIDR").expect("ARCA_TENANT_PROBE_CIDR");
    let front = std::env::var("ARCA_TENANT_PROBE_FRONT").ok();
    let droidd: Vec<String> = std::env::var("ARCA_TENANT_PROBE_DROIDD")
        .unwrap_or_else(|_| "droid".into())
        .split(',')
        .map(str::to_string)
        .collect();
    assert_eq!(
        leftovers(),
        Vec::<String>::new(),
        "an earlier probe left objects behind"
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let mut tenants = json!({
        "capacity": { "instances": 8, "cpu": 16, "memory": "16GiB", "disk": "100GiB" },
        "maxTenants": 2,
        "defaultCaps": { "instances": 3, "containers": 3, "virtualMachines": 1, "cpu": 6,
            "memory": "6GiB", "disk": "45GiB" },
        "createRate": { "count": 4, "perSeconds": 3600 }
    });
    if let Some(front) = &front {
        tenants["front"] = json!(front);
    }
    let rig = Rig::real(
        socket.into(),
        json!({
            "projects": ["arcak-scratch-probe"],
            "droiddProjects": droidd,
            "scratch": true,
            "serviceCidr": cidr,
            "vmLimits": { "virtualMachines": 2, "cpu": 8, "memory": "8GiB", "disk": "40GiB" },
            "tenants": tenants
        }),
        now,
    );
    rig.template("box", json!({
        "name": "box", "kind": "system", "image": "images:alpine/edge", "ports": [8080], "env": {},
        "provides": { "env": { "BOX_ADDR": "{{host}}:{{port}}" } }, "publishable": false
    }));
    // A virtual machine starts with secure boot, as every machine Arca makes: the image must be signed for it.
    rig.template(
        "vm",
        json!({
            "name": "vm", "kind": "virtual-machine", "image": "images:debian/13",
            "limits": { "cpu": "1", "memory": "1GiB" }, "rootSize": "10GiB", "ports": [22]
        }),
    );
    // An instance in a tenant is named `<service>-<tenant id>`.
    let (web, api, bweb) = ("web-aaaaaaaa", "api-aaaaaaaa", "web-bbbbbbbb");
    let pa = "arcak-scratch-t-aaaaaaaa";
    let pb = "arcak-scratch-t-bbbbbbbb";
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for id in [A, B] {
            let made = rig.create_tenant(json!({ "id": id })).expect("tenant");
            println!("tenant {id}: block {}", made["block"]);
        }
        let create = |name: &str, template: &str, tenant: &str| -> String {
            let made = rig
                .call(
                    "service_create",
                    json!({ "name": name, "template": template, "tenant": tenant }),
                )
                .unwrap_or_else(|error| panic!("create {name} in {tenant}: {error:?}"));
            println!("{tenant}/{name}: {} {}", made["ipv4"], made["state"]);
            made["ipv4"].as_str().unwrap().to_string()
        };
        let web_a = create("web", "box", A);
        let api_a = create("api", "box", A);
        let web_b = create("web", "box", B);
        // DHCP: each container holds the address the kernel reserved for it, on eth0.
        for (project, name, ip) in [(pa, web, &web_a), (pa, api, &api_a), (pb, bweb, &web_b)] {
            wait_for(
                &format!("{project}/{name} to take {ip}"),
                Duration::from_secs(90),
                || {
                    inside(project, name, "ip -4 -o addr show eth0")
                        .1
                        .contains(ip.as_str())
                },
            );
            println!("DHCP ok: {project}/{name} holds {ip}");
        }
        // DNS and the internet.
        let (ok, out) = inside(
            pa,
            web,
            "nslookup -timeout=5 example.com >/dev/null 2>&1 && echo resolved",
        );
        assert!(ok && out.contains("resolved"), "DNS from A: {out}");
        let (ok, out) = inside(
            pa,
            web,
            "ping -c 2 -W 3 1.1.1.1 >/dev/null 2>&1 && echo reached",
        );
        assert!(ok && out.contains("reached"), "the internet from A: {out}");
        println!("DNS ok, internet ok from A/web");
        // From A: B's instance, the node's own services and the host's addresses are unreachable.
        let gateway = rig.entry("t-aaaaaaaa-service-web")["gateway"]
            .as_str()
            .unwrap()
            .to_string();
        let mut blocked = vec![
            ("B's web".to_string(), web_b.clone()),
            ("the bridge gateway".to_string(), gateway.clone()),
        ];
        let (_, own) = incus(&["list", "--project", "arcak-svc", "-f", "csv", "-c", "4"]);
        for line in own.lines() {
            if let Some(ip) = line
                .split(' ')
                .next()
                .filter(|ip| ip.parse::<std::net::Ipv4Addr>().is_ok())
            {
                blocked.push((
                    "a service of the node's own project".to_string(),
                    ip.to_string(),
                ));
            }
        }
        for (what, ip) in &blocked {
            let (ok, _) = inside(pa, web, &format!("ping -c 2 -W 2 {ip} >/dev/null 2>&1"));
            assert!(!ok, "{what} ({ip}) answered a ping from A");
            let (ok, _) = inside(pa, web, &format!("nc -z -w 2 {ip} 8080 >/dev/null 2>&1"));
            assert!(!ok, "{what} ({ip}) accepted tcp 8080 from A");
            println!("unreachable from A/web: {what} ({ip})");
        }
        // A link inside A opens the provider's port to the consumer, and to nothing else.
        let (ok, out) = inside(
            pa,
            web,
            "(nc -lk -p 8080 -e /bin/cat >/dev/null 2>&1 &) ; sleep 1; echo listening",
        );
        assert!(ok, "the listener in A/web: {out}");
        let reach = |project: &str, name: &str| {
            inside(
                project,
                name,
                &format!("nc -z -w 3 {web_a} 8080 >/dev/null 2>&1"),
            )
            .0
        };
        assert!(!reach(pa, api), "A/api reached A/web before any link");
        // The kernel lists every instance on the host before it acts, and a live host can fail that read for a moment
        // while another instance changes: the call is refused before any write, so it is tried again.
        let mut linked = None;
        for attempt in 1..=3 {
            match rig.call(
                "service_connect",
                json!({ "from": "api", "to": "web", "tenant": A }),
            ) {
                Ok(answer) => {
                    linked = Some(answer);
                    break;
                }
                Err(error) => {
                    println!("service_connect attempt {attempt}: {error:?}");
                    std::thread::sleep(Duration::from_secs(3));
                }
            }
        }
        linked.expect("the link inside A");
        wait_for("the link to open", Duration::from_secs(20), || {
            reach(pa, api)
        });
        assert!(!reach(pb, bweb), "B/web reached A/web through A's link");
        println!("link ok: A/api reaches A/web:8080, B/web does not");
        if let Some(front) = &front {
            let ingress = rig
                .get("/1.0/network-acls/arcak-scratch-t-aaaaaaaa-svc-web?project=default")
                .expect("the ACL")["ingress"]
                .to_string();
            assert!(ingress.contains("the node's front service"), "{ingress}");
            println!("front rule present for {front}: {ingress}");
        }
        // An address of B's block, put on A's NIC by hand, is dropped: nothing answers through it.
        let (_, out) = inside(
            pa,
            api,
            &format!(
                "ip addr add {web_b}/24 dev eth0 2>&1; ping -c 2 -W 2 -I {web_b} 1.1.1.1 >/dev/null 2>&1 && echo spoofed; ip addr del {web_b}/24 dev eth0 2>&1"
            ),
        );
        assert!(
            !out.contains("spoofed"),
            "a spoofed source address reached the internet: {out}"
        );
        println!("spoofing ok: a source address of B's block is dropped on A's NIC");
        // IPv6: the bridge has no IPv6 address, but every interface on it has a link-local one. A must not reach B
        // through it.
        let (_, local) = inside(
            pb,
            bweb,
            "ip -6 -o addr show dev eth0 scope link | awk '{print $4}' | cut -d/ -f1",
        );
        let local = local.trim().to_string();
        // No address is no test: the step fails, it never passes in silence.
        assert!(
            local.starts_with("fe80:"),
            "B/web holds no IPv6 link-local address on eth0, so the IPv6 step tested nothing: {local:?}"
        );
        {
            let (ok, _) = inside(
                pa,
                web,
                &format!("ping -6 -c 2 -W 2 {local}%eth0 >/dev/null 2>&1"),
            );
            assert!(!ok, "B answered A over IPv6 link-local");
            let (ok, _) = inside(
                pa,
                web,
                &format!("nc -6 -z -w 2 {local}%eth0 8080 >/dev/null 2>&1"),
            );
            assert!(!ok, "B accepted tcp from A over IPv6 link-local");
            println!(
                "IPv6 ok: B's link-local address answers neither a ping nor tcp 8080 from A/web"
            );
        }
        // The control, run only when asked: the filter is taken off both NICs by hand, and A tries B again. What
        // answers then is what the ACL alone lets through. Nothing is asserted: the run is made to learn it. The
        // filter is put back before anything else runs.
        if std::env::var_os("ARCA_TENANT_PROBE_IPV6_CONTROL").is_some() {
            let filter = |project: &str, name: &str, value: Option<&str>| {
                let (ok, out) = match value {
                    Some(value) => incus(&[
                        "config",
                        "device",
                        "set",
                        "--project",
                        project,
                        name,
                        "eth0",
                        &format!("security.ipv6_filtering={value}"),
                    ]),
                    None => incus(&[
                        "config",
                        "device",
                        "unset",
                        "--project",
                        project,
                        name,
                        "eth0",
                        "security.ipv6_filtering",
                    ]),
                };
                assert!(
                    ok,
                    "the filter of {project}/{name} could not be changed: {out}"
                );
            };
            let (_, _) = inside(
                pb,
                bweb,
                "(nc -lk -p 8080 -e /bin/cat >/dev/null 2>&1 &) ; sleep 1",
            );
            filter(pa, web, None);
            filter(pb, bweb, None);
            let (_, shown) = incus(&[
                "config",
                "device",
                "get",
                "--project",
                pb,
                bweb,
                "eth0",
                "security.ipv6_filtering",
            ]);
            let (ping, _) = inside(
                pa,
                web,
                &format!("ping -6 -c 3 -W 2 {local}%eth0 >/dev/null 2>&1"),
            );
            let (tcp, _) = inside(
                pa,
                web,
                &format!("nc -6 -z -w 3 {local}%eth0 8080 >/dev/null 2>&1"),
            );
            let (_, neighbours) = inside(pa, web, "ip -6 neigh show dev eth0");
            println!(
                "IPv6 CONTROL (security.ipv6_filtering removed from both NICs by hand; it reads {:?} on B): ping answered: {ping}; tcp 8080 accepted: {tcp}; A's IPv6 neighbours: {:?}",
                shown.trim(),
                neighbours.trim()
            );
            filter(pa, web, Some("true"));
            filter(pb, bweb, Some("true"));
            let (ok, _) = inside(
                pa,
                web,
                &format!("ping -6 -c 2 -W 2 {local}%eth0 >/dev/null 2>&1"),
            );
            assert!(
                !ok,
                "B answered A over IPv6 link-local once the filter was back"
            );
            println!("IPv6 control done: the filter is back on both NICs");
        }
        // The host's own addresses, as its interfaces hold them now: none answers A.
        let host = Command::new("ip")
            .args(["-4", "-o", "addr", "show"])
            .output()
            .expect("ip runs");
        let addresses: Vec<String> = String::from_utf8_lossy(&host.stdout)
            .lines()
            .filter_map(|line| {
                let mut words = line.split_whitespace();
                let inet = words.find(|word| *word == "inet")?;
                let _ = inet;
                words.next()?.split('/').next().map(str::to_string)
            })
            .filter(|ip| !ip.starts_with("127."))
            .collect();
        for ip in &addresses {
            let (ok, _) = inside(pa, web, &format!("ping -c 1 -W 2 {ip} >/dev/null 2>&1"));
            assert!(!ok, "a host address answered a ping from A");
            // The host's SSH port, which a host usually listens on: a connect from A is not accepted.
            let (ok, _) = inside(pa, web, &format!("nc -z -w 2 {ip} 22 >/dev/null 2>&1"));
            assert!(!ok, "a host address accepted a tcp connect from A");
        }
        println!(
            "host addresses ok: none of the host's {} IPv4 addresses answers a ping or accepts tcp 22 from A/web",
            addresses.len()
        );
        // A virtual machine in A takes its address by DHCP too.
        let vm = create("box", "vm", A);
        wait_for(
            "the virtual machine's address",
            Duration::from_secs(240),
            || {
                incus(&[
                    "list",
                    "--project",
                    pa,
                    "box-aaaaaaaa",
                    "-f",
                    "csv",
                    "-c",
                    "4",
                ])
                .1
                .contains(vm.as_str())
            },
        );
        println!("VM ok: A/box holds {vm}");
        // The caps: the kernel refuses the fourth instance by name, and Incus refuses it too when asked directly.
        let (code, message, fields) = listed(rig.call(
            "service_create",
            json!({ "name": "extra", "template": "box", "tenant": A }),
        ));
        assert_eq!(code, "capacity");
        assert!(
            message.contains("cap on instances (limits.instances)"),
            "{message}"
        );
        assert_eq!(fields["limit"], "instances");
        let (ok, out) = incus(&[
            "init",
            "images:alpine/edge",
            "byhand",
            "--project",
            pa,
            "--network",
            "arcak-svcbr",
        ]);
        assert!(!ok, "Incus created an instance beyond A's cap: {out}");
        println!(
            "caps ok: kernel: {message}; Incus: {}",
            out.lines().last().unwrap_or("").trim()
        );
    }));
    // Whatever happened, everything the probe made is removed, and what is left is listed.
    let mut removed = true;
    for id in [A, B] {
        match rig.approved("tenant_remove", json!({ "id": id })) {
            Ok(answer) => println!("tenant {id} removed: {answer}"),
            Err(error) => {
                removed = false;
                println!("tenant {id} NOT removed: {error:?}");
            }
        }
    }
    let after_removal = leftovers();
    println!("left behind by the kernel's removal: {after_removal:?}");
    // The kernel's own removal is what is under test, but the host is left clean either way: what it left is removed
    // by force, and the run fails if anything is still there.
    if !removed || !after_removal.is_empty() {
        force_cleanup();
    }
    let left = leftovers();
    println!("left behind: {left:?}");
    assert_eq!(left, Vec::<String>::new(), "objects are left on the host");
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    assert!(
        removed,
        "tenant_remove failed; its leftovers were removed by force"
    );
    assert_eq!(
        after_removal,
        Vec::<String>::new(),
        "the kernel's removal left objects; they were removed by force"
    );
}

/// The environment the first process of a running container was started with, one `NAME=value` a line.
fn first_environment(project: &str, name: &str) -> String {
    let (ok, out) = inside(project, name, "tr '\\0' '\\n' < /proc/1/environ");
    assert!(ok, "the environment of {project}/{name}: {out}");
    out
}

fn status(project: &str, name: &str) -> String {
    incus(&["list", "--project", project, name, "-f", "csv", "-c", "s"])
        .1
        .trim()
        .to_string()
}

/// The create-stopped probe: one tenant on a real Incus, with two containers created stopped in a deploy. Their
/// secrets and their link are set while they are stopped, `instance_start` starts each for the first time, and the
/// environment its first process was given is read from inside it. It makes and removes tenant `aaaaaaaa` only:
///
/// ```sh
/// ARCA_TENANT_PROBE_SOCKET=/var/lib/incus/unix.socket ARCA_TENANT_PROBE_CIDR=<the node's serviceCidr> \
///   cargo test -p arca --test tenants -- --ignored --nocapture created_stopped_probe
/// ```
#[test]
#[ignore = "writes to a real Incus: set ARCA_TENANT_PROBE_SOCKET and ARCA_TENANT_PROBE_CIDR"]
fn created_stopped_probe_the_first_start_holds_every_secret_and_link() {
    let socket = std::env::var("ARCA_TENANT_PROBE_SOCKET").expect("ARCA_TENANT_PROBE_SOCKET");
    let cidr = std::env::var("ARCA_TENANT_PROBE_CIDR").expect("ARCA_TENANT_PROBE_CIDR");
    let droidd: Vec<String> = std::env::var("ARCA_TENANT_PROBE_DROIDD")
        .unwrap_or_else(|_| "droid".into())
        .split(',')
        .map(str::to_string)
        .collect();
    assert_eq!(
        leftovers(),
        Vec::<String>::new(),
        "an earlier probe left objects behind"
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let rig = Rig::real(
        socket.into(),
        json!({
            "projects": ["arcak-scratch-probe"],
            "droiddProjects": droidd,
            "scratch": true,
            "serviceCidr": cidr,
            "vmLimits": { "virtualMachines": 2, "cpu": 8, "memory": "8GiB", "disk": "40GiB" },
            // The policy of the two-tenant probe, which this host has carried.
            "tenants": {
                "capacity": { "instances": 8, "cpu": 16, "memory": "16GiB", "disk": "100GiB" },
                "maxTenants": 2,
                "defaultCaps": { "instances": 3, "containers": 3, "virtualMachines": 1, "cpu": 6,
                    "memory": "6GiB", "disk": "45GiB" },
                "createRate": { "count": 4, "perSeconds": 3600 }
            }
        }),
        now,
    );
    // Both templates make their secrets, so a default create would start each at once, before the link exists, and
    // both set autostart.
    rig.template(
        "db",
        json!({
            "name": "db", "kind": "system", "image": "images:alpine/edge", "ports": [5432],
            "env": { "DB_PASSWORD": "{{secret:DB_PASSWORD}}" },
            "secrets": ["DB_PASSWORD"], "generate": { "DB_PASSWORD": "pg_" },
            "provides": { "env": {
                "DATABASE_URL": "postgresql://app:{{secret:DB_PASSWORD}}@{{host}}:{{port}}/app"
            } },
            "autostart": true,
            "publishable": false
        }),
    );
    rig.template(
        "web",
        json!({
            "name": "web", "kind": "system", "image": "images:alpine/edge", "ports": [8080],
            "env": { "ADMIN_PASSWORD": "{{secret:ADMIN_PASSWORD}}" },
            "secrets": ["ADMIN_PASSWORD"], "generate": { "ADMIN_PASSWORD": "adm_" },
            "autostart": true,
            "publishable": false
        }),
    );
    let (db, web) = ("db-aaaaaaaa", "web-aaaaaaaa");
    let pa = "arcak-scratch-t-aaaaaaaa";
    let (db_password, admin_password) = ("synthetic-database-password", "synthetic-admin-password");
    let cloud = |verb: &str, args: Value| {
        rig.call_token("cloud", verb, args, &super::create_stopped::TOOLS)
    };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let made = rig.create_tenant(json!({ "id": A })).expect("tenant");
        println!("tenant {A}: block {}", made["block"]);
        let mut address = String::new();
        for name in ["db", "web"] {
            let made = cloud(
                "service_create",
                json!({ "name": name, "template": name, "tenant": A, "deploy": "p1", "start": false }),
            )
            .unwrap_or_else(|error| panic!("create {name}: {error:?}"));
            println!("{A}/{name}: {} {}", made["ipv4"], made["state"]);
            assert_eq!(made["state"], "stopped", "{name}");
            if name == "db" {
                address = made["ipv4"].as_str().unwrap().to_string();
            }
        }
        // The kernel lists every instance on the host before it acts, and a live host can fail that read for a moment
        // while another instance changes: the call is refused before any write, so it is tried again.
        let again = |verb: &str, args: Value| -> Value {
            for attempt in 1..=3 {
                match cloud(verb, args.clone()) {
                    Ok(answer) => return answer,
                    Err(error) => {
                        println!("{verb} attempt {attempt}: {error:?}");
                        std::thread::sleep(Duration::from_secs(3));
                    }
                }
            }
            panic!("{verb} was refused three times");
        };
        again(
            "service_connect",
            json!({ "from": "web", "to": "db", "tenant": A }),
        );
        again(
            "service_secret_set",
            json!({ "name": "db", "key": "DB_PASSWORD", "value": db_password, "tenant": A }),
        );
        again(
            "service_secret_set",
            json!({ "name": "web", "key": "ADMIN_PASSWORD", "value": admin_password, "tenant": A }),
        );
        // Both templates set autostart, and neither instance carries the key: Incus then restores the last state
        // when the host restarts, and the last state of an instance that never ran is stopped.
        let autostart = |name: &str| {
            incus(&["config", "get", "--project", pa, name, "boot.autostart"])
                .1
                .trim()
                .to_string()
        };
        for name in [db, web] {
            assert_eq!(status(pa, name), "STOPPED", "{name} before its first start");
            assert_eq!(autostart(name), "", "{name} carries boot.autostart");
        }
        println!(
            "stopped ok: both hold their secrets and the link, neither has run, and neither carries boot.autostart"
        );
        for name in ["db", "web"] {
            let started = again("instance_start", json!({ "tenant": A, "service": name }));
            println!("{A}/{name}: {}", started["message"]);
            assert_eq!(started["changed"], true, "{name}");
        }
        for name in [db, web] {
            wait_for(&format!("{name} to run"), Duration::from_secs(60), || {
                status(pa, name) == "RUNNING"
            });
        }
        // What each first process was given: the secret set while it was stopped, never the one the template made,
        // and for the consumer the link's variable with the provider's password, address and port.
        let seen = first_environment(pa, db);
        assert!(
            seen.lines()
                .any(|line| line == format!("DB_PASSWORD={db_password}")),
            "the database's first start did not hold its password"
        );
        let seen = first_environment(pa, web);
        assert!(
            seen.lines()
                .any(|line| line == format!("ADMIN_PASSWORD={admin_password}")),
            "the application's first start did not hold its admin password"
        );
        let url = format!("DATABASE_URL=postgresql://app:{db_password}@{address}:5432/app");
        assert!(
            seen.lines().any(|line| line == url),
            "the application's first start did not hold its link variable"
        );
        println!(
            "first start ok: {A}/db saw DB_PASSWORD; {A}/web saw ADMIN_PASSWORD and DATABASE_URL (@{address}:5432)"
        );
        let confirmed = again("deploy_confirm", json!({ "tenant": A, "deploy": "p1" }));
        assert_eq!(confirmed["confirmed"], true);
        for name in [db, web] {
            assert_eq!(
                autostart(name),
                "",
                "{name} carries boot.autostart after its start"
            );
        }
        println!("deploy ok: p1 confirmed");
    }));
    // Whatever happened, everything the probe made is removed, and what is left is listed.
    let removed = match rig.approved("tenant_remove", json!({ "id": A })) {
        Ok(answer) => {
            println!("tenant {A} removed: {answer}");
            true
        }
        Err(error) => {
            println!("tenant {A} NOT removed: {error:?}");
            false
        }
    };
    let after_removal = leftovers();
    println!("left behind by the kernel's removal: {after_removal:?}");
    if !removed || !after_removal.is_empty() {
        force_cleanup();
    }
    let left = leftovers();
    println!("left behind: {left:?}");
    assert_eq!(left, Vec::<String>::new(), "objects are left on the host");
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    assert!(
        removed && after_removal.is_empty(),
        "the kernel's removal left objects; they were removed by force"
    );
}

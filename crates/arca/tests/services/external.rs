//! A program that runs outside Incus, recorded by the node: `external_add` and `external_remove` write one line each in
//! the node's own record, `service_list` and `service_status` show the program, and every verb that changes a service
//! refuses it in words that name `external_remove`. Nothing here touches Incus beyond the reads a list makes.
use super::*;
use arca::external::{self, EXTERNAL_FILE};
use std::net::TcpListener;

fn add(rig: &Rig, name: &str, address: &str) -> Result<Value, CallError> {
    rig.call(
        "external_add",
        json!({"name": name, "kind": "host-program", "address": address}),
    )
}

fn refused(result: Result<Value, CallError>) -> String {
    match result {
        Err(CallError::Refused { message, .. }) => message,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn invalid(result: Result<Value, CallError>) -> String {
    match result {
        Err(CallError::Invalid(message)) => message,
        other => panic!("expected an invalid call, got {other:?}"),
    }
}

/// An address whose port has a listener, kept open while the guard lives.
fn listening() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a listener");
    let port = listener.local_addr().expect("its address").port();
    (listener, format!("http://127.0.0.1:{port}/mcp"))
}

/// An address nothing answers. Port 1 is below the ephemeral range, so no test's `bind("127.0.0.1:0")` is ever given
/// it, and an unprivileged test cannot bind it: a parallel test can never take it and answer.
fn closed() -> String {
    "http://127.0.0.1:1/mcp".to_string()
}

fn row(rig: &Rig, name: &str) -> Option<Value> {
    rig.call("service_list", json!({})).expect("list")["services"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["name"] == name)
        .cloned()
}

#[test]
fn an_added_program_is_a_recorded_row_of_kind_host_program_beside_the_services() {
    let rig = Rig::new();
    rig.template("nats", nats_template());
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("a service");
    let (_open, address) = listening();
    let done = rig
        .call(
            "external_add",
            json!({"name":"agent","kind":"host-program","address":address,"health":"arca_status"}),
        )
        .expect("the add runs with a doctor's token and no approval");
    assert_eq!(done["recorded"], true);
    assert_eq!(done["name"], "agent");
    let listed = row(&rig, "agent").expect("service_list shows the program");
    assert_eq!(listed["kind"], "host-program");
    assert_eq!(listed["address"], json!(address));
    assert_eq!(listed["managed"], false);
    assert_eq!(listed["recorded"], true);
    assert_eq!(
        listed["state"], "recorded",
        "the list asks nothing of the program"
    );
    assert_eq!(listed["health"], "arca_status");
    assert!(
        listed["note"]
            .as_str()
            .unwrap()
            .contains("not managed by Arca")
    );
    // Nothing was written outside the record: no desired entry, no instance.
    let entries = rig.kernel.lock().unwrap().entries().len();
    add(&rig, "second", &address).expect("a second add");
    assert_eq!(rig.kernel.lock().unwrap().entries().len(), entries);
    let names: Vec<_> = rig.call("service_list", json!({})).unwrap()["services"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["agent", "nats", "second"]);
}

#[test]
fn the_list_is_sorted_by_name_and_the_record_survives_a_new_handler() {
    let rig = Rig::new();
    let (_open, address) = listening();
    add(&rig, "zed", &address).expect("add zed");
    add(&rig, "alpha", &address).expect("add alpha");
    let names: Vec<_> = rig.call("service_list", json!({})).unwrap()["services"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["alpha", "zed"]);
    // A node restarted reads the same record back from its state directory.
    let held = external::read(&rig.dir).expect("the record reads");
    assert_eq!(held.len(), 2);
    assert_eq!(held[0].name, "alpha");
}

#[test]
fn status_makes_one_tcp_connect_and_says_up_or_down() {
    let rig = Rig::new();
    let (open, address) = listening();
    add(&rig, "agent", &address).expect("add");
    let up = rig
        .call("service_status", json!({"name":"agent"}))
        .expect("status");
    assert_eq!(up["state"], "up");
    assert_eq!(up["kind"], "host-program");
    assert_eq!(up["managed"], false);
    // The program stops answering: the record now names an address nothing answers (a listener that was dropped
    // could have its port taken by a parallel test, which would answer).
    drop(open);
    add(&rig, "agent", &closed()).expect("the same name replaces the record");
    let down = rig
        .call("service_status", json!({"name":"agent"}))
        .expect("status");
    assert_eq!(down["state"], "down");
    add(&rig, "gone", &closed()).expect("add a second program");
    let gone = rig
        .call("service_status", json!({"name":"gone"}))
        .expect("status");
    assert_eq!(gone["state"], "down");
}

#[test]
fn every_verb_that_changes_a_service_refuses_a_program_and_names_external_remove() {
    let rig = Rig::new();
    add(&rig, "agent", &closed()).expect("add");
    for (verb, args) in [
        ("service_remove", json!({"name":"agent","approval":"x"})),
        ("service_update", json!({"name":"agent"})),
        ("service_import", json!({"name":"agent","approval":"x"})),
        ("service_release", json!({"name":"agent","approval":"x"})),
        (
            "service_publish",
            json!({"name":"agent","reach":"tailnet","approval":"x"}),
        ),
        ("service_unpublish", json!({"name":"agent"})),
        ("service_connect", json!({"from":"agent","to":"ghost"})),
        ("service_connect", json!({"from":"ghost","to":"agent"})),
        ("service_disconnect", json!({"from":"agent","to":"ghost"})),
        (
            "service_secret_set",
            json!({"name":"agent","key":"token","value":"not-a-real-value"}),
        ),
        ("service_create", json!({"name":"agent","template":"nats"})),
        ("service_logs", json!({"name":"agent"})),
    ] {
        let message = refused(rig.call(verb, args));
        assert!(
            message.contains("external_remove agent"),
            "{verb}: {message}"
        );
        assert!(message.contains("host-program"), "{verb}: {message}");
        assert!(message.contains(verb), "{verb}: {message}");
    }
    // The record is as it was.
    assert!(row(&rig, "agent").is_some());
}

#[test]
fn a_remove_takes_the_row_out_and_leaves_a_journal_of_both_events() {
    let rig = Rig::new();
    let (_open, address) = listening();
    add(&rig, "agent", &address).expect("add");
    let done = rig
        .call("external_remove", json!({"name":"agent"}))
        .expect("remove");
    assert_eq!(done, json!({"removed": true, "name": "agent"}));
    assert!(row(&rig, "agent").is_none());
    // The name is the service verbs' again.
    let message = refused(
        rig.call("service_status", json!({"name":"agent"}))
            .map(|_| Value::Null),
    );
    assert!(message.contains("not found"), "{message}");
    let file = rig.dir.join(EXTERNAL_FILE);
    let text = fs::read_to_string(&file).expect("the journal");
    let events: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("a whole line"))
        .collect();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["event"], "add");
    assert_eq!(events[0]["record"]["name"], "agent");
    assert_eq!(events[1]["event"], "remove");
    assert_eq!(events[1]["name"], "agent");
    assert_eq!(events[1]["at"], NOW);
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let message = refused(rig.call("external_remove", json!({"name":"agent"})));
    assert!(message.contains("not a recorded program"), "{message}");
}

#[test]
fn an_add_is_held_to_the_service_name_rule_the_kind_the_address_and_a_doctor_token() {
    let rig = Rig::new();
    let (_open, address) = listening();
    for name in ["Agent", "a b", ""] {
        invalid(add(&rig, name, &address));
    }
    let bad = |args: Value| invalid(rig.call("external_add", args));
    bad(json!({"name":"agent","kind":"service","address":address}));
    bad(json!({"name":"agent","kind":"host-program","address":"ftp://127.0.0.1:21"}));
    bad(json!({"name":"agent","kind":"host-program","address":"127.0.0.1:8080"}));
    bad(json!({"name":"agent","kind":"host-program","address":"http://user:pw@127.0.0.1:8080"}));
    bad(json!({"name":"agent","kind":"host-program","address":"http://127.0.0.1:0"}));
    bad(json!({"name":"agent","kind":"host-program","address":address,"health":"two words"}));
    bad(json!({"name":"agent","kind":"host-program","address":address,"extra":1}));
    assert!(
        rig.call("service_list", json!({})).unwrap()["services"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let viewer = Caller {
        token_id: "viewer".into(),
        scope: Scope::Viewer,
        tools: None,
    };
    for (verb, args) in [
        (
            "external_add",
            json!({"name":"agent","kind":"host-program","address":address}),
        ),
        ("external_remove", json!({"name":"agent"})),
    ] {
        let result = rig.handlers[verb].call(args, &viewer);
        assert!(matches!(result, Err(CallError::Forbidden { .. })), "{verb}");
    }
    assert!(!rig.dir.join(EXTERNAL_FILE).exists());
}

#[test]
fn a_name_a_service_holds_is_not_taken_and_an_add_of_the_same_name_replaces_the_record() {
    let rig = Rig::new();
    rig.template("nats", nats_template());
    let (_open, address) = listening();
    rig.call("service_create", json!({"name":"nats","template":"nats"}))
        .expect("a service");
    let message = refused(add(&rig, "nats", &address));
    assert!(message.contains("one name space"), "{message}");
    add(&rig, "agent", &address).expect("add");
    let other = closed();
    add(&rig, "agent", &other).expect("a second add replaces");
    assert_eq!(row(&rig, "agent").unwrap()["address"], json!(other));
}

#[test]
fn the_host_and_the_port_come_from_the_address() {
    assert_eq!(
        external::host_port("http://127.0.0.1:8080/mcp").unwrap(),
        ("127.0.0.1".into(), 8080)
    );
    assert_eq!(
        external::host_port("https://host.example").unwrap(),
        ("host.example".into(), 443)
    );
    assert_eq!(
        external::host_port("http://host.example/x?y=1").unwrap(),
        ("host.example".into(), 80)
    );
    assert_eq!(
        external::host_port("http://[::1]:9000/").unwrap(),
        ("::1".into(), 9000)
    );
    assert!(external::host_port("http://[::1]x").is_err());
    assert!(external::host_port("http://").is_err());
    assert!(external::host_port("http://host:70000").is_err());
}

#[test]
fn a_torn_last_line_is_dropped_and_the_next_add_does_not_join_it() {
    let rig = Rig::new();
    let (_open, address) = listening();
    add(&rig, "agent", &address).expect("add");
    let file = rig.dir.join(EXTERNAL_FILE);
    let mut text = fs::read_to_string(&file).unwrap();
    text.push_str("{\"event\":\"remove\",\"at\":1,\"na");
    fs::write(&file, text).unwrap();
    assert!(
        row(&rig, "agent").is_some(),
        "the torn remove was never made"
    );
    add(&rig, "second", &address).expect("add after the tear");
    assert!(row(&rig, "second").is_some());
    assert!(row(&rig, "agent").is_some());
}

#[test]
fn through_the_door_the_schemas_hold_the_arguments_and_the_rows_and_the_answers_meet_theirs() {
    let rig = Rig::new();
    let door = Door::over(&rig);
    let (_open, address) = listening();
    let good =
        json!({"name":"agent","kind":"host-program","address":address,"health":"arca_status"});
    assert_eq!(door.call("external_add", good), "ok");
    for (why, args) in [
        (
            "a kind that is not host-program",
            json!({"name":"x","kind":"service","address":address}),
        ),
        (
            "an address that is not http",
            json!({"name":"x","kind":"host-program","address":"ftp://127.0.0.1:21"}),
        ),
        (
            "a name that ends in a dash",
            json!({"name":"x-","kind":"host-program","address":address}),
        ),
        (
            "a health with a space",
            json!({"name":"x","kind":"host-program","address":address,"health":"a b"}),
        ),
        (
            "an approval it does not take",
            json!({"name":"x","kind":"host-program","address":address,"approval":"y"}),
        ),
    ] {
        assert_ne!(door.call("external_add", args), "ok", "{why}");
    }
    assert_eq!(
        door.call("service_list", json!({})),
        "ok",
        "the row meets service_list's schema"
    );
    assert_eq!(door.call("service_status", json!({"name":"agent"})), "ok");
    assert_ne!(door.call("service_remove", json!({"name":"agent"})), "ok");
    assert_eq!(door.call("external_remove", json!({"name":"agent"})), "ok");
    assert!(row(&rig, "agent").is_none());
}

/// The MCP door over the rig's handlers, with the real verb table, and one doctor's token.
struct Door {
    socket: PathBuf,
    doctor: String,
    _runtime: tokio::runtime::Runtime,
}

impl Door {
    fn over(rig: &Rig) -> Door {
        use arca_mcp::{Server, TokenStore, mint, serve_unix};
        let tokens = rig.dir.join("door-tokens");
        fs::create_dir(&tokens).expect("the token directory");
        fs::set_permissions(&tokens, fs::Permissions::from_mode(0o700)).expect("0700");
        let (doctor, hash) = mint(Scope::Doctor);
        let path = tokens.join("doctor.hash");
        fs::write(&path, hash).expect("the hash file");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).expect("0400");
        let mut server = Server::new(
            arca_api::verbs(),
            TokenStore::load(&tokens).expect("the tokens load"),
        );
        for (verb, handler) in &rig.handlers {
            server = server.handle(verb, Arc::clone(handler));
        }
        let socket = rig.dir.join("door.sock");
        let runtime = tokio::runtime::Runtime::new().expect("a runtime");
        let served = socket.clone();
        let router = server.router();
        runtime.spawn(async move {
            let _ = serve_unix(router, &served).await;
        });
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !socket.exists() {
            assert!(std::time::Instant::now() < until, "the door started");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Door {
            socket,
            doctor,
            _runtime: runtime,
        }
    }

    /// A `tools/call` through the door: `"ok"`, or what the door said instead.
    fn call(&self, name: &str, arguments: Value) -> String {
        use std::io::{Read as _, Write as _};
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }))
        .expect("the call");
        let head = format!(
            "POST /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.doctor,
            body.len()
        );
        let mut stream =
            std::os::unix::net::UnixStream::connect(&self.socket).expect("connect to the door");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(20)))
            .expect("a read timeout");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&body);
        let mut raw = Vec::new();
        let _ = stream.read_to_end(&mut raw);
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("a response head");
        let reply: Value = serde_json::from_slice(&raw[split + 4..]).expect("a JSON reply");
        if reply["result"]["isError"] == json!(false) {
            return "ok".to_string();
        }
        reply["result"]["content"][0]["text"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| reply["error"]["message"].to_string())
    }
}

#[test]
fn a_node_started_again_reads_the_record_back_through_its_own_handlers() {
    let rig = Rig::new();
    let (_open, address) = listening();
    add(&rig, "agent", &address).expect("add");
    // Fresh handlers over the same kernel and state directory, as a restarted node builds them.
    let again: HashMap<&'static str, Arc<dyn ToolHandler>> =
        service_handlers(rig.kernel.clone(), Arc::new(|| NOW), rig.cfg.clone())
            .into_iter()
            .collect();
    let caller = Caller {
        token_id: "doctor".into(),
        scope: Scope::Doctor,
        tools: None,
    };
    let listed = again["service_list"]
        .call(json!({}), &caller)
        .expect("list");
    assert_eq!(listed["services"][0]["name"], "agent");
    let status = again["service_status"]
        .call(json!({"name":"agent"}), &caller)
        .expect("status");
    assert_eq!(status["state"], "up");
}

#[test]
fn a_lookup_of_a_name_is_held_to_the_limit() {
    // Whatever the resolver answers for a name that does not exist, the check is over within its three seconds.
    let started = std::time::Instant::now();
    let _ = external::reachable("http://no-such-host.invalid:80/");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

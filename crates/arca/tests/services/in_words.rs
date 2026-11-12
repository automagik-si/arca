//! What a caller is told in words, through the MCP door as `arca serve` builds it (the real verb table, so the arguments
//! and the result are checked against the verb's schemas): for each value of a secret a caller may choose, and for a
//! read of Incus that fails or is slow. Nothing here is answered "internal: the tool failed".
//!
//! No value of a secret, and no part of one, is written here or shown by a failed assertion: every value is built
//! when the test runs, and a test compares lengths and digests.
use super::*;
use arca_kernel::canon::Digest;
use std::time::Instant;

/// The door over the rig's service handlers, with one doctor's token.
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
        let until = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(Instant::now() < until, "the door started");
            std::thread::sleep(Duration::from_millis(10));
        }
        Door {
            socket,
            doctor,
            _runtime: runtime,
        }
    }

    /// A `tools/call` through the door; the JSON-RPC reply.
    fn call(&self, name: &str, arguments: Value) -> Value {
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
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("a read timeout");
        // A body the door refuses to read may close the connection before it is all written.
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&body);
        let mut raw = Vec::new();
        let _ = stream.read_to_end(&mut raw);
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("a response head");
        let status = String::from_utf8_lossy(&raw[..split])
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        serde_json::from_slice(&raw[split + 4..]).unwrap_or_else(|_| json!({ "http": status }))
    }
}

/// The code of the refusal the door answered, when it answered one.
fn refusal_code(reply: &Value) -> &str {
    reply["error"]["data"]["refusal"].as_str().unwrap_or("")
}

/// What the door answered, with nothing of the call in it: "ok", or the kind and the words of the failure.
fn answer(reply: &Value) -> String {
    if reply["result"]["isError"] == json!(false) {
        return "ok".to_string();
    }
    if let Some(text) = reply["result"]["content"][0]["text"].as_str() {
        return format!("tool failure: {text}");
    }
    if let Some(status) = reply["http"].as_str() {
        return status.to_string();
    }
    format!(
        "error {}: {}",
        reply["error"]["code"],
        reply["error"]["message"].as_str().unwrap_or("")
    )
}

/// `length` characters shaped like a private key in PEM form: a first line, lines of 64, a last line. Made when the
/// test runs, from a counter; it is no key. It ends with a line feed only when `ends_with_newline`.
fn key_file(length: usize, ends_with_newline: bool) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let word = ["PRI", "VATE"].concat();
    let first = format!("-----BEGIN RSA {word} KEY-----\n");
    let last = format!("-----END RSA {word} KEY-----\n");
    let mut text = first;
    let mut state: u32 = 0x9e37_79b9;
    let mut column = 0;
    while text.len() + last.len() < length {
        if column == 64 || text.len() + last.len() + 1 == length {
            text.push('\n');
            column = 0;
            continue;
        }
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        text.push(ALPHABET[(state >> 24) as usize % ALPHABET.len()] as char);
        column += 1;
    }
    text.push_str(&last);
    assert_eq!(text.len(), length, "the synthetic key file's length");
    if !ends_with_newline {
        text.pop();
    }
    text
}

/// The stored file a service's secret reference names: its length and its digest, never its content.
fn stored(rig: &Rig, name: &str, key: &str) -> Option<(usize, String)> {
    let record: service::ServiceConfig =
        serde_json::from_value(entry(rig, &format!("service-{name}")).config).expect("the record");
    let reference = record.secret_refs.get(key).cloned()?;
    let path = rig
        .dir
        .join("secrets")
        .join(reference.strip_prefix(arca_providers::incus::SECRET_PREFIX)?);
    let bytes = fs::read(path).ok()?;
    Some((bytes.len(), Digest::of(&bytes).to_hex()))
}

fn shape(value: &str) -> (usize, String) {
    (value.len(), Digest::of(value.as_bytes()).to_hex())
}

/// The listing every service call reads: the instances, without their live state.
const LISTING: &str = "/1.0/instances?all-projects=true&recursion=1";
/// The listing only a create reads: each instance with its live state, slow where a guest's agent is slow.
const LISTING_WITH_STATE: &str = "/1.0/instances?all-projects=true&recursion=2";
const MACHINE: &str = "a virtual machine's secret is 8 to 4096 characters, not counting one line break at its end; it may have line breaks, and no other control character";
const CONTAINER: &str =
    "a container's secret is one line of 8 to 4096 characters without control characters";
const ENDS_WITH_A_LINE_BREAK: &str = "this one ends with a line break, and nothing else is wrong with it. Send it without the line break at its end (the program reads it as an environment value, and the line break would be part of it; only a virtual machine's secret, which is a file, may end with one)";
const AGAIN: &str = "The same request may be made again (an approved action is proposed again).";

/// A node with a virtual-machine service `p1` and a container service `tok`, each with a secret to set.
fn machine_and_container() -> Rig {
    let rig = pool_rig();
    rig.call("service_create", json!({"name":"p1","template":"pool"}))
        .expect("create");
    rig.template(
        "tok",
        json!({"name":"tok","kind":"oci","image":"docker.io/library/example","digest":DIGEST,
            "ports":[],"env":{"T":"{{secret:T}}"},"secrets":["T"]}),
    );
    rig.call("service_create", json!({"name":"tok","template":"tok"}))
        .expect("a container");
    rig
}

#[test]
fn a_key_file_read_whole_is_stored_byte_for_byte_with_its_last_newline_and_without_it() {
    let rig = machine_and_container();
    let life = life(&rig, Duration::from_millis(200));
    let door = Door::over(&rig);
    let whole = key_file(1679, true);
    let cut = whole.trim_end_matches('\n').to_string();
    // The longest value there is, with one line break at its end: the line break is not counted.
    let longest = format!("{}\n", "k".repeat(4096));
    // On a machine that is stopped, then on one that runs: stored as given, and the guest's file is the same bytes.
    for running in [false, true] {
        for value in [&cut, &whole, &longest] {
            let ends = value.ends_with('\n');
            let reply = door.call(
                "service_secret_set",
                json!({"name":"p1","key":"SIGNING_KEY","value":value}),
            );
            assert_eq!(
                answer(&reply),
                "ok",
                "{} characters, a line break at the end: {ends}, running: {running}",
                value.chars().count()
            );
            assert_eq!(
                stored(&rig, "p1", "SIGNING_KEY"),
                Some(shape(value)),
                "stored as given; a line break at the end: {ends}, running: {running}"
            );
            start(&life, "p1").expect("start");
            assert_eq!(
                guest_file(&rig, "p1", "SIGNING_KEY").map(|text| shape(&text)),
                Some(shape(value)),
                "the guest's file; a line break at the end: {ends}, running: {running}"
            );
        }
    }
    // A key file saved with carriage returns keeps its last line break too, and each pair is one line feed.
    let reply = door.call(
        "service_secret_set",
        json!({"name":"p1","key":"SIGNING_KEY","value":whole.replace('\n', "\r\n")}),
    );
    assert_eq!(answer(&reply), "ok");
    assert_eq!(stored(&rig, "p1", "SIGNING_KEY"), Some(shape(&whole)));
}

#[test]
fn every_value_a_caller_may_choose_is_stored_or_refused_in_words_that_name_the_rule_and_its_limit()
{
    let rig = machine_and_container();
    let door = Door::over(&rig);
    let line = "v".repeat(20);
    let too_short = "error -32602: /value: shorter than the minimum length of 8 characters";
    let too_long = "error -32602: /value: longer than the maximum length of 4097 characters";
    let not_text = "error -32602: /value: expected string";
    let big = "HTTP/1.1 413 Payload Too Large";
    let machine = |why: &str| format!("error -32602: {MACHINE}: {why}");
    let container = |why: &str| format!("error -32602: {CONTAINER}: {why}");
    let few = "this one has fewer than 8 characters that are not line breaks";
    let breaks = "this one has a line break";
    let nul = "this one has a NUL character";
    let lone = "this one has a carriage return that no line feed follows";
    let ok = || "ok".to_string();
    // Each value, and what a virtual machine's secret and a container's answer for it.
    let cases: Vec<(&str, Value, String, String)> = vec![
        ("empty", json!(""), too_short.into(), too_short.into()),
        (
            "seven characters",
            json!("v".repeat(7)),
            too_short.into(),
            too_short.into(),
        ),
        (
            "eight line breaks",
            json!("\n".repeat(8)),
            machine(few),
            container(breaks),
        ),
        (
            "4096 line breaks",
            json!("\n".repeat(4096)),
            machine(few),
            container(breaks),
        ),
        (
            "seven characters and a line break",
            json!(format!("{}\n", "v".repeat(7))),
            machine(few),
            container(breaks),
        ),
        (
            "one line and a line break",
            json!(format!("{line}\n")),
            ok(),
            container(ENDS_WITH_A_LINE_BREAK),
        ),
        (
            "one line, a carriage return and a line feed",
            json!(format!("{line}\r\n")),
            ok(),
            container(ENDS_WITH_A_LINE_BREAK),
        ),
        (
            "one line and two line breaks",
            json!(format!("{line}\n\n")),
            ok(),
            container(breaks),
        ),
        (
            "a key file",
            json!(key_file(1679, true)),
            ok(),
            container(breaks),
        ),
        (
            "a NUL inside",
            json!(format!("{line}\0{line}")),
            machine(nul),
            container(nul),
        ),
        (
            "a NUL at the end",
            json!(format!("{line}\0")),
            machine(nul),
            container(nul),
        ),
        (
            "a tab",
            json!(format!("{line}\t{line}")),
            machine("this one has a tab"),
            container("this one has a tab"),
        ),
        (
            "a lone carriage return at the end",
            json!(format!("{line}\r")),
            machine(lone),
            container(lone),
        ),
        ("4096 characters", json!("v".repeat(4096)), ok(), ok()),
        (
            "4096 characters and a line break",
            json!(format!("{}\n", "v".repeat(4096))),
            ok(),
            container(ENDS_WITH_A_LINE_BREAK),
        ),
        (
            "4097 characters",
            json!("v".repeat(4097)),
            machine("this one is longer"),
            container("this one is not 8 to 4096 characters"),
        ),
        (
            "4098 characters",
            json!("v".repeat(4098)),
            too_long.into(),
            too_long.into(),
        ),
        (
            "4096 characters of three bytes each",
            json!("\u{20ac}".repeat(4096)),
            ok(),
            ok(),
        ),
        (
            "a megabyte",
            json!("v".repeat(1 << 20)),
            big.into(),
            big.into(),
        ),
        (
            "a number",
            json!(12345678),
            not_text.into(),
            not_text.into(),
        ),
        ("null", Value::Null, not_text.into(), not_text.into()),
    ];
    for (label, value, for_machine, for_container) in &cases {
        for (service, key, expected) in [
            ("p1", "SIGNING_KEY", for_machine),
            ("tok", "T", for_container),
        ] {
            let before = stored(&rig, service, key);
            let reply = door.call(
                "service_secret_set",
                json!({"name":service,"key":key,"value":value}),
            );
            let said = answer(&reply);
            assert_eq!(&said, expected, "{label}, on {service}");
            assert!(!said.contains("internal"), "{label}, on {service}");
            // Nothing of the value is in the answer: not its first characters, when it has any that print.
            if let Some(text) = value.as_str() {
                let first: String = text.chars().take(12).collect();
                if first.chars().count() == 12 && !first.chars().any(char::is_control) {
                    assert!(
                        !reply.to_string().contains(&first),
                        "{label}, on {service}: the answer quotes the value"
                    );
                }
            }
            // A refused value stores nothing; a stored one is the value as given, but a CRLF, kept as a line feed.
            let after = stored(&rig, service, key);
            if said == "ok" {
                let kept = value.as_str().expect("text").replace("\r\n", "\n");
                assert_eq!(after, Some(shape(&kept)), "{label}, on {service}");
            } else {
                assert_eq!(after, before, "{label}, on {service}: nothing was stored");
            }
        }
    }
}

/// How many reads of `path` the fake was asked for.
fn reads_of(rig: &Rig, path: &str) -> usize {
    rig.fake
        .requests()
        .iter()
        .filter(|request| request.method == "GET" && request.path.starts_with(path))
        .count()
}

#[test]
fn a_read_of_incus_that_fails_fast_is_made_once_more_and_one_that_fails_twice_is_refused_in_words()
{
    let rig = machine_and_container();
    let door = Door::over(&rig);
    let value = key_file(1679, true);
    let failed = |subject: &str, changed: &str| {
        format!(
            "error -32000: Incus answered with its own error (status 500) when this node read the instances for {subject}. {changed}{AGAIN}"
        )
    };
    for (verb, arguments, said) in [
        (
            "service_status",
            json!({"name":"p1"}),
            failed("service p1", ""),
        ),
        (
            "service_list",
            json!({}),
            failed("the list of services", ""),
        ),
        (
            "service_logs",
            json!({"name":"tok"}),
            failed("service tok", ""),
        ),
        (
            "service_secret_set",
            json!({"name":"p1","key":"SIGNING_KEY","value":value}),
            failed("service p1", "Nothing was changed. "),
        ),
    ] {
        // Twice in a row: the read and its one second try. Refused in words, and nothing was done.
        rig.fake.fail_reads(LISTING, 2, 500);
        let before = reads_of(&rig, LISTING);
        let reply = door.call(verb, arguments.clone());
        assert_eq!(answer(&reply), said, "{verb}");
        assert_eq!(refusal_code(&reply), "incus_unavailable", "{verb}");
        assert_eq!(
            reads_of(&rig, LISTING),
            before + 2,
            "{verb}: two reads, no more"
        );
        assert_eq!(
            stored(&rig, "p1", "SIGNING_KEY"),
            None,
            "{verb}: nothing was stored"
        );
        // Once: the second try is answered, and the caller never hears of the first.
        rig.fake.fail_reads(LISTING, 1, 500);
        let reply = door.call(verb, arguments);
        assert_ne!(
            refusal_code(&reply),
            "incus_unavailable",
            "{verb}, failing once"
        );
        if verb != "service_logs" {
            assert_eq!(answer(&reply), "ok", "{verb}, failing once");
        }
    }
    assert_eq!(stored(&rig, "p1", "SIGNING_KEY"), Some(shape(&value)));
    // An answer of Incus that is no server error is not "ask again": it has its own code, and is read once.
    rig.fake.fail_reads(LISTING, 1, 403);
    let before = reads_of(&rig, LISTING);
    let reply = door.call("service_status", json!({"name":"p1"}));
    assert_eq!(
        answer(&reply),
        "error -32000: Incus answered with status 403 when this node read the instances for service p1. The same request is likely to be answered the same until Incus answers that read."
    );
    assert_eq!(refusal_code(&reply), "incus_answer");
    assert_eq!(reads_of(&rig, LISTING), before + 1);
}

#[test]
fn a_read_of_incus_slower_than_its_deadline_is_refused_in_words_and_is_not_made_again() {
    let rig = machine_and_container();
    let door = Door::over(&rig);
    // Slower than the ten seconds a read is given: this test waits them out once.
    rig.fake.slow_reads(
        LISTING,
        1,
        arca_incus::read::TIMEOUT + Duration::from_millis(500),
    );
    let before = reads_of(&rig, LISTING);
    let reply = door.call("service_status", json!({"name":"p1"}));
    assert_eq!(
        answer(&reply),
        format!(
            "error -32000: Incus did not answer within 10 seconds when this node read the instances for service p1. {AGAIN}"
        )
    );
    assert_eq!(refusal_code(&reply), "incus_unavailable");
    // A second read would double the wait: after a deadline there is none.
    assert!(reads_of(&rig, LISTING) <= before + 1);
    assert_eq!(
        answer(&door.call("service_status", json!({"name":"p1"}))),
        "ok"
    );
}

#[test]
fn the_longest_secret_and_one_of_many_bytes_are_taken_out_of_a_services_log() {
    let rig = machine_and_container();
    let door = Door::over(&rig);
    // The longest value a machine takes: 4096 characters and its line break, 4097 bytes in its file. And the longest
    // a container takes, of three bytes a character: 12288 bytes. Both are made here, from a counter.
    let mut state: u32 = 0x1234_5678;
    let mut letters = |alphabet: &[char], count: usize| -> String {
        (0..count)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                alphabet[(state >> 24) as usize % alphabet.len()]
            })
            .collect()
    };
    let longest = letters(&['q', 'r', 's', 't', 'u', 'w', 'x', 'z'], 4096);
    let wide = letters(&['\u{20ac}', '\u{00e9}', '\u{4e2d}', '\u{00f1}'], 4096);
    assert_eq!(longest.len(), 4096);
    assert!(wide.len() > 2 * 4096);
    for (service, key, value) in [
        ("p1", "SIGNING_KEY", format!("{longest}\n")),
        ("tok", "T", wide.clone()),
    ] {
        let reply = door.call(
            "service_secret_set",
            json!({"name":service,"key":key,"value":value}),
        );
        assert_eq!(answer(&reply), "ok", "{service}");
    }
    // Each guest prints its secret on its console, between two words.
    for (service, value) in [("p1", &longest), ("tok", &wide)] {
        rig.fake.set_console_log(
            PROJECT,
            service,
            &format!("booting\nloaded {value} done\nready\n"),
        );
        let reply = door.call("service_logs", json!({"name":service}));
        assert_eq!(answer(&reply), "ok", "{service}");
        let lines = reply["result"]["structuredContent"]["lines"].clone();
        // Compared here and not by assert_eq, whose message would print the line with the value in it.
        let hidden = lines == json!(["booting", "loaded [secret] done", "ready"]);
        assert!(hidden, "{service}: the value is not taken out of its line");
        // No piece of it is anywhere in the answer.
        let whole = reply.to_string();
        let piece: String = value.chars().take(24).collect();
        assert!(
            !whole.contains(&piece),
            "{service}: the answer shows the secret"
        );
    }
}

#[test]
fn one_machine_whose_live_state_is_slow_holds_up_only_its_own_status_and_that_still_answers() {
    let rig = machine_and_container();
    // A machine on a host bridge: its address is given by that network, and is only in its live state.
    let mac = "02:aa:bb:00:00:07";
    rig.call(
        "service_create",
        json!({"name":"p2","template":"pool","bridge":"br0","mac":mac}),
    )
    .expect("a machine on the host bridge");
    let life = life(&rig, Duration::from_millis(200));
    start(&life, "p2").expect("start");
    rig.fake.set_lease("br0", mac, "198.51.100.7");
    let door = Door::over(&rig);
    let structured = |reply: &Value| reply["result"]["structuredContent"].clone();

    // With nothing slow, its status reads its one state and shows the address; the list reads no state and says
    // where the address is.
    let status = door.call("service_status", json!({"name":"p2"}));
    assert_eq!(answer(&status), "ok");
    assert_eq!(structured(&status)["ipv4"], "198.51.100.7");
    assert!(structured(&status).get("ipv4Problem").is_none());
    let state_of_p2 = format!("/1.0/instances/p2/state?project={PROJECT}");

    // Now its guest is slow: its own state, and so the listing with every instance's state, take longer than a
    // read is given.
    let slow = arca_incus::read::TIMEOUT + Duration::from_millis(500);
    rig.fake.slow_reads(LISTING_WITH_STATE, 100, slow);
    rig.fake.slow_reads(&state_of_p2, 1, slow);
    let with_state = reads_of(&rig, LISTING_WITH_STATE);
    let states = reads_of(&rig, "/1.0/instances/p2/state");
    let value = key_file(1679, true);
    let began = Instant::now();
    let list = door.call("service_list", json!({}));
    assert_eq!(answer(&list), "ok");
    let rows = structured(&list)["services"].clone();
    let row = |name: &str| {
        rows.as_array()
            .expect("the rows")
            .iter()
            .find(|row| row["name"] == name)
            .cloned()
            .expect("a row")
    };
    // The list: the bridged machine's address is said to be in its state, and no other row carries the key.
    assert_eq!(row("p2")["state"], "running");
    assert_eq!(row("p2")["ipv4"], "");
    assert_eq!(row("p2")["ipv4In"], "state");
    assert!(row("p1").get("ipv4In").is_none());
    assert_ne!(row("p1")["ipv4"], "");
    for (verb, arguments) in [
        ("service_status", json!({"name":"p1"})),
        ("service_status", json!({"name":"tok"})),
        (
            "service_secret_set",
            json!({"name":"p1","key":"SIGNING_KEY","value":value}),
        ),
        (
            "service_secret_set",
            json!({"name":"p2","key":"SIGNING_KEY","value":value}),
        ),
    ] {
        assert_eq!(answer(&door.call(verb, arguments)), "ok", "{verb}");
    }
    assert_eq!(
        answer(&door.call("service_logs", json!({"name":"tok"}))),
        "error -32000: service tok: no log"
    );
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "six calls beside a slow machine took {:?}",
        began.elapsed()
    );
    // None of them read any state: not the listing with it, and not that machine's.
    assert_eq!(reads_of(&rig, LISTING_WITH_STATE), with_state);
    assert_eq!(reads_of(&rig, "/1.0/instances/p2/state"), states);

    // Its own status waits for its state, as long as a read is given, and still answers: everything but the
    // address, and why the address is missing.
    let began = Instant::now();
    let status = door.call("service_status", json!({"name":"p2"}));
    assert!(began.elapsed() >= arca_incus::read::TIMEOUT);
    assert_eq!(answer(&status), "ok");
    let view = structured(&status);
    assert_eq!(view["state"], "running");
    assert_eq!(view["ipv4"], "");
    assert_eq!(
        view["ipv4Problem"],
        "Incus did not answer within 10 seconds when this node read the live state of service p2, which is where the address of a machine on a host bridge is: the address was not read. Every other field is as usual; ask again for the address"
    );
    assert_eq!(view["settings"]["slots"], 2);
    // The state answers again: so does the address.
    let status = door.call("service_status", json!({"name":"p2"}));
    assert_eq!(structured(&status)["ipv4"], "198.51.100.7");
    assert!(structured(&status).get("ipv4Problem").is_none());
}

#[test]
fn a_change_the_kernel_cannot_save_is_refused_in_words_with_its_own_code() {
    let rig = machine_and_container();
    let door = Door::over(&rig);
    let first = key_file(1679, true);
    let reply = door.call(
        "service_secret_set",
        json!({"name":"p1","key":"SIGNING_KEY","value":first}),
    );
    assert_eq!(answer(&reply), "ok");
    // The state directory takes no new file: the kernel cannot write its next state.
    let mode = fs::metadata(&rig.dir)
        .expect("the state directory")
        .permissions();
    fs::set_permissions(&rig.dir, fs::Permissions::from_mode(0o500)).expect("read-only");
    let reply = door.call(
        "service_secret_set",
        json!({"name":"p1","key":"SIGNING_KEY","value":key_file(1700, true)}),
    );
    fs::set_permissions(&rig.dir, mode).expect("writable again");
    assert_eq!(
        answer(&reply),
        "error -32000: this node could not save the change: a file of its state could not be written. The service's record is as it was before the call; the node's log has the reason, and the same request may be made again once it is fixed"
    );
    assert_eq!(refusal_code(&reply), "state_not_saved");
    assert_eq!(stored(&rig, "p1", "SIGNING_KEY"), Some(shape(&first)));
}

//! Group 6's title tests: the Omni verbs on the service model, against a fake Omni. The fake is a local HTTP server that
//! answers the four requests Arca may make as Omni's v2 API does, records every request, and is hostile where Omni could
//! be: its answers carry fields outside Arca's allowlist (the bot token, the WhatsApp JID, the key), its errors echo the
//! number, and it can answer with a redirect to logout. All secrets and addresses are synthetic.
use super::*;
use arca::handlers::omni_handlers;
use arca::omni::{
    BODY_MAX, OmniOptions, PAIR_COOLDOWN_S, REDACTED, TIMEOUT, mask_phone, phone_digits,
    redact_phone,
};
use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

/// The planted secrets: each must turn up nowhere Arca answers or writes outside its secrets directory.
const KEY: &str = "omni_sk_PlantedKey0123456789abcdefABCDEFGH";
const BOT: &str = "7012345678:AAPlantedTelegramTokenThatMustNotLeak_x9";
const PHONE: &str = "+5511987654321";
const OMNI_TOOLS: [&str; 3] = ["omni_instance_create", "omni_pair", "omni_status"];

fn planted() -> Vec<String> {
    vec![
        KEY.into(),
        BOT.into(),
        BOT.split(':').nth(1).unwrap().into(),
        PHONE.into(),
        PHONE[1..].into(),
        KEY[8..16].into(),
    ]
}

// ---- the fake Omni ---------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    url: String,
    key: Option<String>,
    body: Option<Value>,
}

#[derive(Clone)]
struct Instance {
    id: String,
    name: String,
    channel: String,
    telegram: Option<String>,
}

#[derive(Default)]
struct OmniState {
    seen: Vec<Seen>,
    instances: Vec<Instance>,
    /// The key Omni took at its first start.
    key: String,
    /// The next answer, in place of the usual one: status, body, extra headers.
    next: Option<(u16, Value, Vec<(String, String)>)>,
    delay: Duration,
    made: u64,
}

struct FakeOmni {
    addr: SocketAddr,
    state: Arc<Mutex<OmniState>>,
}

impl FakeOmni {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(OmniState::default()));
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = shared.clone();
                std::thread::spawn(move || handle(stream, &state));
            }
        });
        Self { addr, state }
    }
    fn state(&self) -> std::sync::MutexGuard<'_, OmniState> {
        self.state.lock().unwrap()
    }
    fn seen(&self) -> Vec<Seen> {
        self.state().seen.clone()
    }
    fn asked(&self) -> Vec<String> {
        self.seen()
            .iter()
            .map(|s| format!("{} {}", s.method, s.url))
            .collect()
    }
    fn next(&self, status: u16, body: Value) {
        self.state().next = Some((status, body, vec![]));
    }
}

fn handle(stream: TcpStream, state: &Mutex<OmniState>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    if reader.read_line(&mut first).is_err() || first.is_empty() {
        return;
    }
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let url = parts.next().unwrap_or_default().to_string();
    let mut length = 0;
    let mut key = None;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().unwrap(),
            "x-api-key" => key = Some(value.trim().to_string()),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let body: Option<Value> = (!body.is_empty()).then(|| serde_json::from_slice(&body).unwrap());
    let delay = state.lock().unwrap().delay;
    std::thread::sleep(delay);
    let (status, answer, headers) = answer(state, &method, &url, key.clone(), body.clone());
    state.lock().unwrap().seen.push(Seen {
        method,
        url,
        key,
        body,
    });
    let text = answer.to_string();
    let mut out = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        text.len()
    );
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(&text);
    let mut stream = stream;
    let _ = stream.write_all(out.as_bytes());
}

fn answer(
    state: &Mutex<OmniState>,
    method: &str,
    url: &str,
    key: Option<String>,
    body: Option<Value>,
) -> (u16, Value, Vec<(String, String)>) {
    let mut s = state.lock().unwrap();
    if key.as_deref() != Some(s.key.as_str()) {
        return (
            401,
            json!({"error":{"code":"UNAUTHORIZED","message":"Invalid API key"}}),
            vec![],
        );
    }
    if let Some(next) = s.next.take() {
        return next;
    }
    // Every answer carries what Omni must never let out, and Arca must never pass on.
    let leaky = |i: &Instance| {
        json!({"id":i.id,"name":i.name,"channel":i.channel,"isActive":true,"telegramBotToken":i.telegram,
            "apiKey":key,"sessionPath":"/data/sessions/x","ownerIdentifier":format!("{}@s.whatsapp.net",&PHONE[1..]),
            "profilePicUrl":"https://pps.example/x.jpg"})
    };
    let tail = url.strip_prefix("/api/v2/instances").unwrap_or("!");
    if method == "POST" && tail.is_empty() {
        let b = body.unwrap();
        let name = b["name"].as_str().unwrap().to_string();
        if s.instances.iter().any(|i| i.name == name) {
            return (
                409,
                json!({"error":{"code":"CONFLICT","message":format!("Instance {name} already exists")}}),
                vec![],
            );
        }
        s.made += 1;
        let i = Instance {
            id: format!("0f8c4a3e-1b2d-4c5e-8f90-{:012x}", s.made),
            name,
            channel: b["channel"].as_str().unwrap().into(),
            telegram: b["telegramBotToken"].as_str().map(str::to_string),
        };
        s.instances.push(i.clone());
        return (201, json!({"data":leaky(&i)}), vec![]);
    }
    if method == "GET" && tail == "?limit=100" {
        let items: Vec<Value> = s.instances.iter().map(leaky).collect();
        return (200, json!({"items":items,"meta":{"hasMore":false}}), vec![]);
    }
    let (id, rest) = match tail.strip_prefix('/').and_then(|t| t.split_once('/')) {
        Some((id, rest)) => (id.to_string(), format!("/{rest}")),
        None => (String::new(), String::new()),
    };
    let found = s.instances.iter().find(|i| i.id == id).cloned();
    if !id.is_empty() && found.is_none() {
        return (
            404,
            json!({"error":{"code":"NOT_FOUND","message":format!("Instance not found: {id}")}}),
            vec![],
        );
    }
    match (method, rest.as_str(), found) {
        ("GET", "/status", Some(i)) => (
            200,
            json!({"data":{"instanceId":i.id,"state":"connected","isConnected":true,"connectedAt":null,"profileName":"Example Bot",
                "profilePicUrl":"https://pps.example/x.jpg","ownerIdentifier":format!("{}@s.whatsapp.net",&PHONE[1..]),
                "message":format!("logged in as {PHONE} ({}) with {} and {BOT}",&PHONE[1..],key.unwrap())}}),
            vec![],
        ),
        ("POST", "/pair", Some(i)) => {
            if !i.channel.starts_with("whatsapp") {
                return (
                    400,
                    json!({"error":{"code":"INVALID_OPERATION","message":"Pairing code only available for WhatsApp instances"}}),
                    vec![],
                );
            }
            let number = body.unwrap()["phoneNumber"].as_str().unwrap().to_string();
            let masked = format!("{}****{}", &number[..5], &number[number.len() - 2..]);
            (
                200,
                json!({"data":{"code":"ABCD-1234","phoneNumber":masked,"message":"Enter this code","expiresIn":60}}),
                vec![],
            )
        }
        _ => (
            404,
            json!({"error":{"code":"NOT_FOUND","message":format!("no route {method} {url}")}}),
            vec![],
        ),
    }
}

// ---- the rig ---------------------------------------------------------------------------------------

struct OmniRig {
    rig: Rig,
    omni: FakeOmni,
    handlers: HashMap<&'static str, Arc<dyn ToolHandler>>,
    now: Arc<AtomicU64>,
    /// Every result and error text an Omni verb gave.
    said: Mutex<Vec<String>>,
}

#[derive(Default)]
struct Opts {
    base: Option<SocketAddr>,
    timeout: Option<Duration>,
    body_max: Option<usize>,
}

fn node_config(rig: &Rig) -> NodeConfig {
    serde_json::from_value(json!({"stateDir":rig.dir,"incus":{"socket":rig.fake.socket()},"listen":{"unix":rig.dir.join("arca.sock")},
        "tokens":{"hashDir":rig.dir.join("tokens")},"kernel":{"projects":[PROJECT],"droiddProjects":["other"],"serviceCidr":"192.0.2.0/24"}}))
    .unwrap()
}

fn repo_template(rig: &Rig, name: &str) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
    let text = fs::read_to_string(dir.join(format!("{name}.json"))).unwrap();
    rig.template(name, serde_json::from_str(&text).unwrap());
}

fn operator() -> Caller {
    Caller {
        token_id: "operator".into(),
        scope: Scope::Operator,
        tools: None,
    }
}

/// A rig with the Omni handlers pointed at a fake Omni (or at `o.base`), and no service yet.
fn omni_rig(o: Opts) -> OmniRig {
    let rig = Rig::new();
    let omni = FakeOmni::start();
    let base = o.base.unwrap_or(omni.addr);
    let now = Arc::new(AtomicU64::new(NOW));
    let clock = now.clone();
    let handlers = omni_handlers(
        rig.kernel.clone(),
        Arc::new(move || clock.load(Ordering::SeqCst)),
        Arc::new(node_config(&rig)),
        OmniOptions {
            base: Some(Arc::new(move |_, _, _| base)),
            timeout: o.timeout.unwrap_or(Duration::from_secs(3)),
            body_max: o.body_max.unwrap_or(BODY_MAX),
        },
    )
    .into_iter()
    .collect();
    repo_template(&rig, "omni-api");
    repo_template(&rig, "nats");
    OmniRig {
        rig,
        omni,
        handlers,
        now,
        said: Mutex::new(Vec::new()),
    }
}

/// `omni_rig` with the omni-api service created, its key planted and handed to the fake Omni, and, unless `secrets` is
/// false, the owner's two secrets set.
fn ready_with(o: Opts, secrets: bool) -> OmniRig {
    let r = omni_rig(o);
    r.rig
        .call(
            "service_create",
            json!({"name":"omni","template":"omni-api"}),
        )
        .unwrap();
    r.rig
        .call("service_create", json!({"name":"nats","template":"nats"}))
        .unwrap();
    r.secret("OMNI_API_KEY", KEY);
    r.omni.state().key = KEY.into();
    if secrets {
        r.secret("telegram_bot_token", BOT);
        r.secret("whatsapp_phone_number", PHONE);
    }
    r
}

fn ready() -> OmniRig {
    ready_with(Opts::default(), true)
}

/// A refusal as the door names it: its code, its message, and whether it is marked untrusted.
fn refusal(error: &CallError) -> (String, String, bool) {
    match error {
        CallError::Refused { code, message } => (code.clone(), message.clone(), false),
        CallError::UntrustedRefused { code, message } => (code.clone(), message.clone(), true),
        CallError::RefusedWith { code, message, .. } => (code.clone(), message.clone(), false),
        CallError::Invalid(why) => ("invalid".into(), why.clone(), false),
        CallError::Forbidden { verb } => ("forbidden".into(), verb.clone(), false),
        CallError::NotFound => ("not_found".into(), String::new(), false),
        CallError::Internal(_) => ("internal".into(), String::new(), false),
        CallError::Unauthenticated => ("unauthenticated".into(), String::new(), false),
    }
}

impl OmniRig {
    /// The owner sets a secret over the owner's socket (an operator caller).
    fn secret(&self, key: &str, value: &str) {
        self.rig.handlers["service_secret_set"]
            .call(json!({"name":"omni","key":key,"value":value}), &operator())
            .unwrap_or_else(|e| panic!("secret {key}: {e:?}"));
    }
    fn call_as(&self, verb: &str, args: Value, caller: &Caller) -> Result<Value, CallError> {
        let out = self.handlers[verb].call(args, caller);
        let text = match &out {
            Ok(value) => value.to_string(),
            Err(error) => format!("{error:?}"),
        };
        self.said.lock().unwrap().push(text);
        out
    }
    fn call(&self, verb: &str, args: Value) -> Result<Value, CallError> {
        self.call_as(
            verb,
            args,
            &Caller {
                token_id: "doctor".into(),
                scope: Scope::Doctor,
                tools: None,
            },
        )
    }
    /// A call that must pass; its result must conform to the verb's natural schema.
    fn ok(&self, verb: &str, args: Value) -> Value {
        let value = self
            .call(verb, args)
            .unwrap_or_else(|e| panic!("{verb}: {e:?}"));
        let v = arca_api::verb(verb).unwrap();
        assert!(v.untrusted, "{verb}'s result is untrusted");
        arca_api::conforms(&(v.result)(), &value)
            .unwrap_or_else(|why| panic!("{verb}: {why}: {value}"));
        value
    }
    /// A call that must be refused with `code`, its message matching `pattern`; returns the message and the mark.
    fn refused(&self, verb: &str, args: Value, code: &str, pattern: &str) -> (String, bool) {
        let error = self
            .call(verb, args.clone())
            .expect_err(&format!("{verb} {args} passed"));
        let (got, message, untrusted) = refusal(&error);
        assert_eq!(got, code, "{verb} {args}: {message}");
        assert!(
            regex::Regex::new(pattern).unwrap().is_match(&message),
            "{verb} {args}: {message:?} !~ {pattern}"
        );
        (message, untrusted)
    }
    /// A pairing of the instance `id` that is held off for `wait` seconds: a `rate` refusal with its whole sentence,
    /// and the same wait in `retryAfterSeconds`.
    fn held_off(&self, id: &str, wait: u64) {
        assert_eq!(
            common::listed_refusal(self.call("omni_pair", json!({"name":"omni","instance":id}))),
            (
                "rate".to_string(),
                format!(
                    "a pairing code for {id} was issued and lasts {wait} s more: enter that one on the phone; never ask for codes in a loop"
                ),
                json!({ "retryAfterSeconds": wait })
            )
        );
    }
    fn sent(&self) -> usize {
        self.omni.seen().len()
    }
}

// ---- the titles ------------------------------------------------------------------------------------

#[test]
fn g6_tracer_status_goes_through_the_service_key_to_an_allowlisted_untrusted_result() {
    let r = ready();
    assert_eq!(
        r.ok("omni_status", json!({"name":"omni"})),
        json!({"instances":[]})
    );
    let seen = r.omni.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (
            seen[0].method.as_str(),
            seen[0].url.as_str(),
            seen[0].key.as_deref()
        ),
        ("GET", "/api/v2/instances?limit=100", Some(KEY))
    );
    let made = r.ok(
        "omni_instance_create",
        json!({"name":"omni","instance":"wa","channel":"whatsapp-baileys"}),
    );
    let id = made["id"].as_str().unwrap();
    let status = r.ok("omni_status", json!({"name":"omni","instance":id}));
    let mut keys: Vec<_> = status.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        ["instance", "isConnected", "message", "profileName", "state"]
    );
    assert_eq!(
        status,
        json!({"instance":id,"state":"connected","isConnected":true,"profileName":"Example Bot",
            "message":format!("logged in as {REDACTED} ({REDACTED}) with {REDACTED} and {REDACTED}")}),
        "Omni's text, redacted of the three"
    );
    // By name: looked up in the list, then the status route with its id.
    assert_eq!(
        r.ok("omni_status", json!({"name":"omni","instance":"wa"})),
        status
    );
    assert_eq!(
        r.ok("omni_status", json!({"name":"omni"})),
        json!({"instances":[{"id":id,"name":"wa","channel":"whatsapp-baileys","isActive":true}]}),
        "the allowlist: no token, JID, key or path"
    );
    assert!(r.omni.seen().iter().all(|s| s.key.as_deref() == Some(KEY)));
    // The door marks every Omni result untrusted and sums it up as Omni's words.
    let v = arca_api::verb("omni_status").unwrap();
    assert!((v.summary_text)(&status).ends_with("Untrusted: Omni wrote it."));
}

#[test]
fn g6_instance_create_whatsapp_and_telegram_with_the_owners_token_or_a_refusal_naming_the_slot() {
    let r = ready_with(Opts::default(), false);
    r.refused(
        "omni_instance_create",
        json!({"name":"omni","instance":"tg","channel":"telegram"}),
        "refused",
        "^omni has no telegram_bot_token secret yet: the owner sets it with service_secret_set",
    );
    assert_eq!(r.sent(), 0, "nothing was sent");
    let wa = r.ok(
        "omni_instance_create",
        json!({"name":"omni","instance":"wa","channel":"whatsapp-baileys"}),
    );
    let mut keys: Vec<_> = wa.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["channel", "id", "isActive", "name"]);
    assert_eq!(
        r.omni.seen().last().unwrap().body,
        Some(json!({"name":"wa","channel":"whatsapp-baileys"})),
        "no agentId, no token"
    );
    r.secret("telegram_bot_token", BOT);
    let tg = r.ok(
        "omni_instance_create",
        json!({"name":"omni","instance":"tg","channel":"telegram"}),
    );
    assert_eq!(
        r.omni.seen().last().unwrap().body,
        Some(json!({"name":"tg","channel":"telegram","telegramBotToken":BOT}))
    );
    assert_eq!(
        json!({"name":tg["name"],"channel":tg["channel"],"isActive":tg["isActive"]}),
        json!({"name":"tg","channel":"telegram","isActive":true})
    );
    let (_, untrusted) = r.refused(
        "omni_instance_create",
        json!({"name":"omni","instance":"wa","channel":"whatsapp-baileys"}),
        "refused",
        r"answered POST /api/v2/instances with 409: CONFLICT: Instance wa already exists",
    );
    assert!(untrusted, "Omni's words are untrusted");
    let before = r.sent();
    for channel in ["discord", "whatsapp-business", "slack"] {
        r.refused(
            "omni_instance_create",
            json!({"name":"omni","instance":"x","channel":channel}),
            "invalid",
            "",
        );
    }
    for instance in ["", "Has Space", "../x", &"a".repeat(64)] {
        r.refused(
            "omni_instance_create",
            json!({"name":"omni","instance":instance,"channel":"telegram"}),
            "invalid",
            "",
        );
    }
    assert_eq!(r.sent(), before, "no invalid call reached Omni");
}

#[test]
fn g6_pair_masks_the_owners_number_once_per_code_and_refuses_without_a_valid_secret() {
    let r = ready_with(Opts::default(), false);
    let wa = r.ok(
        "omni_instance_create",
        json!({"name":"omni","instance":"wa","channel":"whatsapp-baileys"}),
    );
    let id = wa["id"].as_str().unwrap().to_string();
    let sent = r.sent();
    r.refused(
        "omni_pair",
        json!({"name":"omni","instance":id}),
        "refused",
        "^omni has no whatsapp_phone_number secret yet: the owner sets it with service_secret_set",
    );
    for bad in [
        "+55 11 98765-4321",
        "5511987654321",
        "+0011987654321",
        "+12345678",
    ] {
        r.secret("whatsapp_phone_number", bad);
        let (message, _) = r.refused(
            "omni_pair",
            json!({"name":"omni","instance":"wa"}),
            "refused",
            "whatsapp_phone_number secret is not a number in international form",
        );
        assert!(
            !message.contains(&bad[1..8]),
            "the refusal quotes no part of it"
        );
    }
    assert_eq!(r.sent(), sent, "nothing was sent");
    r.secret("whatsapp_phone_number", PHONE);
    let code = r.ok("omni_pair", json!({"name":"omni","instance":"wa"}));
    assert_eq!(
        code,
        json!({"instance":id,"code":"ABCD-1234","expiresIn":60,"phoneNumber":mask_phone(PHONE)})
    );
    assert_eq!(mask_phone(PHONE), "+55*********21");
    let last = r.omni.seen().last().unwrap().clone();
    assert_eq!(last.body, Some(json!({"phoneNumber":PHONE})));
    assert_eq!(last.url, format!("/api/v2/instances/{id}/pair"));
    let before = r.sent();
    // The code Omni gave lasts 60 seconds, and no time has passed.
    r.held_off(&id, 60);
    assert_eq!(r.sent(), before, "the second ask reached nothing");
    // A Telegram instance has no pairing: Omni says so, and Arca passes it on, marked untrusted.
    r.secret("telegram_bot_token", BOT);
    let tg = r.ok(
        "omni_instance_create",
        json!({"name":"omni","instance":"tg","channel":"telegram"}),
    );
    let (_, untrusted) = r.refused(
        "omni_pair",
        json!({"name":"omni","instance":tg["id"]}),
        "refused",
        "400: INVALID_OPERATION: Pairing code only available for WhatsApp instances",
    );
    assert!(untrusted);
}

#[test]
fn g6_no_other_route_is_reachable_three_tools_no_path_input() {
    let r = ready();
    let wa = r.ok(
        "omni_instance_create",
        json!({"name":"omni","instance":"wa","channel":"whatsapp-baileys"}),
    );
    let id = wa["id"].as_str().unwrap().to_string();
    let sent = r.sent();
    for instance in [
        format!("{id}/logout"),
        format!("{id}/../../agents"),
        "../agents".into(),
        format!("{id}?x=1"),
        format!("{id}%2Flogout"),
        format!("{id}#"),
        "wa/logout".into(),
        "/api/v2/keys".into(),
        id.to_uppercase(),
    ] {
        for tool in ["omni_status", "omni_pair"] {
            r.refused(
                tool,
                json!({"name":"omni","instance":instance}),
                "invalid",
                "",
            );
        }
    }
    assert_eq!(
        r.sent(),
        sent,
        "none of them reached Omni: an instance is a UUID or a plain name, never a path"
    );
    // Three tools, served and in the table; no input names a method, path or URL.
    let names: Vec<&str> = arca_api::verbs()
        .iter()
        .map(|v| v.name)
        .filter(|n| n.starts_with("omni"))
        .collect();
    assert_eq!(names, OMNI_TOOLS);
    for n in OMNI_TOOLS {
        assert!(r.rig.handlers.contains_key(n), "{n} is served");
        let params = (arca_api::verb(n).unwrap().params)();
        for p in params["properties"].as_object().unwrap().keys() {
            assert!(
                ["name", "instance", "channel"].contains(&p.as_str()),
                "{n}: {p}"
            );
        }
    }
    // A field the schema does not name is refused before anything is read.
    r.refused(
        "omni_status",
        json!({"name":"omni","path":"/api/v2/instances/x/logout"}),
        "invalid",
        "",
    );
    // A redirect to logout is not followed.
    r.omni.state().next = Some((
        302,
        json!({}),
        vec![("location".into(), format!("/api/v2/instances/{id}/logout"))],
    ));
    r.refused(
        "omni_status",
        json!({"name":"omni","instance":id}),
        "refused",
        r"with a redirect \(302\); Arca follows none",
    );
    // Every request Arca ever made is one of the four routes.
    let allowed = regex::Regex::new(
        r"^(POST /api/v2/instances|GET /api/v2/instances\?limit=100|GET /api/v2/instances/[0-9a-f-]{36}/status|POST /api/v2/instances/[0-9a-f-]{36}/pair)$",
    )
    .unwrap();
    for line in r.omni.asked() {
        assert!(allowed.is_match(&line), "{line}");
    }
    // A service of another template, a name that is no service, a viewer: refused before any request.
    let before = r.sent();
    r.refused(
        "omni_status",
        json!({"name":"nats"}),
        "refused",
        "^nats is a nats service, not omni-api: the omni tools reach Omni's API alone",
    );
    r.refused("omni_status", json!({"name":"nope"}), "not_found", "");
    let viewer = Caller {
        token_id: "viewer".into(),
        scope: Scope::Viewer,
        tools: None,
    };
    assert!(matches!(
        r.call_as("omni_status", json!({"name":"omni"}), &viewer),
        Err(CallError::Forbidden { .. })
    ));
    assert_eq!(r.sent(), before);
}

#[test]
fn g6_planted_secrets_appear_in_no_result_error_journal_or_state_and_no_process_runs() {
    let r = ready();
    let incus_before = r.rig.fake.requests().len();
    let wa = r.ok(
        "omni_instance_create",
        json!({"name":"omni","instance":"wa","channel":"whatsapp-baileys"}),
    );
    let id = wa["id"].as_str().unwrap().to_string();
    r.ok(
        "omni_instance_create",
        json!({"name":"omni","instance":"tg","channel":"telegram"}),
    );
    r.ok("omni_status", json!({"name":"omni"}));
    r.ok("omni_status", json!({"name":"omni","instance":id}));
    r.ok("omni_pair", json!({"name":"omni","instance":"wa"}));
    // Errors that quote Omni: one that echoes the number, the key and the token.
    for message in [
        format!("Invalid phone number: {PHONE}"),
        format!("bad key {KEY} for {BOT}"),
        format!("JID {}@s.whatsapp.net", &PHONE[1..]),
        format!("masked omni_sk_{}************", &KEY[8..16]),
    ] {
        r.omni.next(
            500,
            json!({"error":{"code":"PAIRING_FAILED","message":message}}),
        );
        let (text, untrusted) = r.refused(
            "omni_status",
            json!({"name":"omni","instance":id}),
            "refused",
            r"answered GET /api/v2/instances/.*/status with 500: PAIRING_FAILED: ",
        );
        assert!(text.contains(REDACTED), "{text}");
        assert!(untrusted, "Omni's words are untrusted");
    }
    r.omni.state().key = "omni_sk_SomeOtherKeyOmniTookFirst0000000".into();
    r.refused(
        "omni_status",
        json!({"name":"omni"}),
        "refused",
        r"^omni refused Arca's OMNI_API_KEY on GET /api/v2/instances \(401, UNAUTHORIZED: Invalid API key\)",
    );
    r.omni.state().key = KEY.into();
    // No process: the Omni verbs asked Incus nothing at all, so no exec and no file push.
    assert_eq!(
        r.rig.fake.requests().len(),
        incus_before,
        "no Incus request"
    );
    // A key of no known shape is redacted as a stored value, not by a credential pattern.
    let plain = "PlainOwnerKeyOfNoKnownShape0123456789";
    r.secret("OMNI_API_KEY", plain);
    r.omni.state().key = plain.into();
    r.omni.next(
        500,
        json!({"error":{"code":"FAILED","message":format!("key {plain} refused")}}),
    );
    let (text, _) = r.refused(
        "omni_status",
        json!({"name":"omni"}),
        "refused",
        &format!("FAILED: key {} refused$", regex::escape(REDACTED)),
    );
    assert!(!text.contains(plain));
    assert!(!r.said.lock().unwrap().join("\n").contains(plain));
    r.secret("OMNI_API_KEY", KEY);
    r.omni.state().key = KEY.into();
    // Every answer and error, and every file the node keeps outside its private secrets directory (the desired state,
    // the effect journal, the exec audit): none holds any of them.
    let said = r.said.lock().unwrap().join("\n");
    for p in planted() {
        assert!(!said.contains(&p), "{}… turned up in an answer", &p[..6]);
    }
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(&r.rig.dir, &mut files);
    let secrets = r.rig.dir.join("secrets");
    let mut read = 0;
    for file in files.iter().filter(|f| !f.starts_with(&secrets)) {
        let Ok(bytes) = fs::read(file) else { continue };
        read += 1;
        for p in planted() {
            assert!(
                !bytes.windows(p.len()).any(|w| w == p.as_bytes()),
                "{}… turned up in {}",
                &p[..6],
                file.display()
            );
        }
    }
    assert!(read > 0, "the node's own files were read");
    assert!(
        r.rig
            .fake
            .service_requests()
            .iter()
            .all(|q| !q.path.contains("/exec"))
    );
}

#[test]
fn g6_failures_read_plainly() {
    let r = ready();
    r.refused(
        "omni_status",
        json!({"name":"omni","instance":"0f8c4a3e-1b2d-4c5e-8f90-ffffffffffff"}),
        "not_found",
        r"answered GET .* with 404 \(NOT_FOUND: Instance not found: [0-9a-f-]{36}\): no such instance",
    );
    r.refused(
        "omni_status",
        json!({"name":"omni","instance":"gone"}),
        "not_found",
        r"omni has no instance\(s\) named gone \(omni_status lists them\)",
    );
    r.omni.next(200, json!({"meta":{}}));
    r.refused(
        "omni_status",
        json!({"name":"omni"}),
        "refused",
        "answered its instance list without items",
    );
    r.omni.next(
        200,
        json!({"items":[{"id":"not-a-uuid","name":"x","channel":"telegram"}]}),
    );
    r.refused(
        "omni_status",
        json!({"name":"omni"}),
        "refused",
        "answered with something that is not an Omni instance",
    );
    r.omni.state().next = Some((200, json!("not an object"), vec![]));
    r.refused("omni_status", json!({"name":"omni"}), "refused", "");
    // A port nothing listens on: one a listener held a moment ago.
    let closed = {
        let held = TcpListener::bind("127.0.0.1:0").unwrap();
        held.local_addr().unwrap()
    };
    let down = omni_rig(Opts {
        base: Some(closed),
        ..Opts::default()
    });
    down.rig
        .call(
            "service_create",
            json!({"name":"omni","template":"omni-api"}),
        )
        .unwrap();
    down.refused(
        "omni_status",
        json!({"name":"omni"}),
        "refused",
        r"^omni does not answer GET /api/v2/instances at 192\.0\.2\.\d+:8882 \(ECONNREFUSED\): is it running, and linked to postgres-autopg and nats\?$",
    );
    // An answer that is not JSON.
    let plain = TcpListener::bind("127.0.0.1:0").unwrap();
    let plain_addr = plain.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut s in plain.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-length: 9\r\nconnection: close\r\n\r\n<html/>!!",
            );
        }
    });
    let html = omni_rig(Opts {
        base: Some(plain_addr),
        ..Opts::default()
    });
    html.rig
        .call(
            "service_create",
            json!({"name":"omni","template":"omni-api"}),
        )
        .unwrap();
    html.refused(
        "omni_status",
        json!({"name":"omni"}),
        "refused",
        r"^omni answered GET /api/v2/instances with 200 and no JSON$",
    );
}

#[test]
fn g6_body_cap_and_timeout_with_1_mib_and_15_s_defaults() {
    assert_eq!(BODY_MAX, 1 << 20);
    assert_eq!(TIMEOUT, Duration::from_secs(15));
    assert_eq!(OmniOptions::default().body_max, BODY_MAX);
    assert_eq!(OmniOptions::default().timeout, TIMEOUT);
    let r = ready_with(
        Opts {
            body_max: Some(512),
            timeout: Some(Duration::from_millis(300)),
            ..Opts::default()
        },
        true,
    );
    assert_eq!(
        r.ok("omni_status", json!({"name":"omni"})),
        json!({"instances":[]}),
        "a small answer passes the cap"
    );
    r.omni.next(200, json!({"items":[],"pad":"x".repeat(2048)}));
    r.refused(
        "omni_status",
        json!({"name":"omni"}),
        "too_large",
        "^Omni answered more than 512 bytes$",
    );
    r.omni.state().delay = Duration::from_millis(1500);
    let t = Instant::now();
    r.refused(
        "omni_status",
        json!({"name":"omni"}),
        "refused",
        r"^omni does not answer GET /api/v2/instances at .* \(no answer in time\)",
    );
    assert!(
        t.elapsed() < Duration::from_millis(1200),
        "it gave up at its timeout, not at Omni's answer"
    );
    r.omni.state().delay = Duration::ZERO;
}

#[test]
fn g6_the_number_is_redacted_however_omni_writes_it() {
    assert_eq!(phone_digits(PHONE), ["5511987654321", "551187654321"]);
    assert_eq!(
        phone_digits("+551187654321"),
        ["551187654321", "5511987654321"]
    );
    assert_eq!(phone_digits("+14155550123"), ["14155550123"]);
    let four = regex::Regex::new("[0-9]{4}").unwrap();
    for c in [
        "+55 11 98765-4321",
        "+55 (11) 98765-4321",
        "55.11.98765.4321",
        "5511987654321@s.whatsapp.net",
        "551187654321@s.whatsapp.net",
        "+55 11 8765-4321",
        "5511987654321:12@s.whatsapp.net",
    ] {
        let out = redact_phone(&format!("id {c} ok"), PHONE);
        assert!(!four.is_match(&out), "{c} -> {out}");
    }
    assert_eq!(
        redact_phone("JID 551187654321@s.whatsapp.net", PHONE),
        format!("JID {REDACTED}@s.whatsapp.net")
    );
    assert_eq!(
        redact_phone("order 155119876543210 and 11987654321", PHONE),
        "order 155119876543210 and 11987654321",
        "longer and partial digit runs are other numbers"
    );
    assert_eq!(
        redact_phone("JID 5511987654321@s.whatsapp.net", "not a phone"),
        "JID 5511987654321@s.whatsapp.net"
    );
    // In a result: Omni's status message; in an error: Omni's echo of an invalid number.
    let r = ready();
    let wa = r.ok(
        "omni_instance_create",
        json!({"name":"omni","instance":"wa","channel":"whatsapp-baileys"}),
    );
    r.omni.next(
        200,
        json!({"data":{"state":"connected","isConnected":true,"message":"linked 551187654321@s.whatsapp.net (+55 11 98765-4321)"}}),
    );
    let status = r.ok("omni_status", json!({"name":"omni","instance":wa["id"]}));
    assert_eq!(
        status["message"],
        format!("linked {REDACTED}@s.whatsapp.net ({REDACTED})")
    );
    r.omni.next(
        500,
        json!({"error":{"code":"PAIRING_FAILED","message":"Invalid phone number: 55 11 8765 4321"}}),
    );
    let (message, _) = r.refused(
        "omni_status",
        json!({"name":"omni","instance":wa["id"]}),
        "refused",
        "",
    );
    assert!(
        message.ends_with(&format!("Invalid phone number: {REDACTED}")),
        "{message}"
    );
}

#[test]
fn g6_pair_cooldown_starts_with_every_attempt_that_reaches_omni() {
    let r = ready();
    let wa = r.ok(
        "omni_instance_create",
        json!({"name":"omni","instance":"wa","channel":"whatsapp-baileys"}),
    );
    let id = wa["id"].clone();
    r.omni.next(
        500,
        json!({"error":{"code":"PAIRING_FAILED","message":"rate limited"}}),
    );
    r.refused(
        "omni_pair",
        json!({"name":"omni","instance":id}),
        "refused",
        "500: PAIRING_FAILED: rate limited",
    );
    let sent = r.sent();
    r.held_off(id.as_str().expect("an id"), PAIR_COOLDOWN_S);
    assert_eq!(r.sent(), sent, "the second ask reached nothing");
    // An answer without a code is an attempt too.
    r.now
        .fetch_add(PAIR_COOLDOWN_S * 1000 + 1, Ordering::SeqCst);
    r.omni.next(200, json!({"data":{"message":"try later"}}));
    r.refused(
        "omni_pair",
        json!({"name":"omni","instance":id}),
        "refused",
        "answered the pairing without a pairing code",
    );
    r.held_off(id.as_str().expect("an id"), PAIR_COOLDOWN_S);
    r.now
        .fetch_add(PAIR_COOLDOWN_S * 1000 + 1, Ordering::SeqCst);
    assert_eq!(
        r.ok("omni_pair", json!({"name":"omni","instance":id}))["code"],
        "ABCD-1234",
        "once it has passed, a code"
    );
    // One that reaches nothing holds nothing off: each try against a closed port reaches the network again.
    let closed = {
        let held = TcpListener::bind("127.0.0.1:0").unwrap();
        held.local_addr().unwrap()
    };
    let down = omni_rig(Opts {
        base: Some(closed),
        ..Opts::default()
    });
    down.rig
        .call(
            "service_create",
            json!({"name":"omni","template":"omni-api"}),
        )
        .unwrap();
    down.secret("whatsapp_phone_number", PHONE);
    for _ in 0..2 {
        down.refused(
            "omni_pair",
            json!({"name":"omni","instance":"0f8c4a3e-1b2d-4c5e-8f90-a1b2c3d4e5f6"}),
            "refused",
            r"does not answer POST .* \(ECONNREFUSED\)",
        );
    }
}

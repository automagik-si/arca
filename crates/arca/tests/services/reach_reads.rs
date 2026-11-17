//! The open reach route bounds its real connector reads; all status values are synthetic.
use super::*;
use webauthn::{Assert, SoftPasskey};

struct PageRig {
    rig: Rig,
    key: SoftPasskey,
    clock: Arc<AtomicU64>,
    socket: PathBuf,
    before_execs: usize,
    _owner: common::Scratch,
    _runtime: tokio::runtime::Runtime,
}

impl PageRig {
    fn new() -> Self {
        let ssh = common::SkKey::from_seed(71);
        let key = SoftPasskey::from_seed(5);
        let owner = common::Scratch::new("reach-read-owner");
        let verifier = common::verifier_with_passkeys(
            &owner,
            &[&ssh],
            &key.line(arca_approval::ARCA_NAMESPACE),
            "reach-read",
        );
        let mut rig = Rig::with_verifier(
            FakeState {
                server: json!({}),
                projects: vec![],
                instances: vec![],
                networks: vec![],
                pools: vec![("pool1".into(), json!({}))],
            },
            vec![PROJECT.into()],
            Some("192.0.2.0/24"),
            Some(verifier),
            None,
        );
        prepare_edge(&rig);
        let clock = Arc::new(AtomicU64::new(NOW));
        let time = clock.clone();
        let clock_fn: arca::handlers::Clock = Arc::new(move || time.load(Ordering::SeqCst));
        let (handlers, reads) = arca::handlers::service_handlers_with_reach(
            rig.kernel.clone(),
            clock_fn.clone(),
            rig.cfg.clone(),
            None,
            Default::default(),
        );
        rig.handlers = handlers.into_iter().collect();
        status(&rig, "NeedsLogin");
        connect(&rig, &ssh, Some("node-ts")).unwrap();
        let before_execs = status_execs(&rig);
        let page = arca::approve_page::Page::new(
            webauthn::ORIGIN.into(),
            rig.kernel.clone(),
            clock_fn,
            rig.dir.clone(),
            Arc::new(arca::approve_page::DesktopDisplay),
        )
        .with_reach(reads);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let socket = rig.dir.join("reads.sock");
        let served = socket.clone();
        let router = page.router(false);
        runtime.spawn(async move {
            arca_mcp::serve_unix(router, &served).await.unwrap();
        });
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !socket.exists() {
            assert!(std::time::Instant::now() < until, "the read page started");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Self {
            rig,
            key,
            clock,
            socket,
            before_execs,
            _owner: owner,
            _runtime: runtime,
        }
    }

    fn get(&self) -> Value {
        let (code, _, rows) = page_request(&self.socket, "GET", "/approve/reach", json!({}));
        assert_eq!(code, 200);
        assert!(!rows.to_string().contains(LOGIN));
        assert!(!rows.to_string().contains(KEY));
        rows
    }

    fn post(&self, body: Value) -> (u16, Value) {
        let (code, _, body) = page_request(&self.socket, "POST", "/approve/reach/sign-in", body);
        (code, body)
    }

    fn begin(&self) -> Value {
        let (code, mut pending) = self.post(json!({"name":"node-ts"}));
        assert_eq!(code, 200);
        assert!(pending.get("signInUrl").is_none());
        pending["name"] = json!("node-ts");
        pending
    }

    fn proof(&self, mut pending: Value) -> Value {
        pending["proof"] = self.key.proof_value(&Assert::over(
            arca_approval::ARCA_NAMESPACE,
            pending["statement"].as_str().unwrap().as_bytes(),
        ));
        pending
    }

    fn status(&self, state: &str, held: bool) {
        let mut script = ExecScript::new(
            arca_incus::exec::ExecCommand::ConnectorStatus.argv(),
            0,
            &json!({"BackendState":state,"AuthURL":LOGIN,"AuthKey":KEY,
                "Self":{"DNSName":"node.example.ts.net."},"TailscaleIPs":["100.64.0.7"]})
            .to_string(),
        );
        script.held = held;
        self.rig.fake.script_exec(script);
    }

    fn execs(&self) -> usize {
        status_execs(&self.rig) - self.before_execs
    }

    fn row_audit(&self) -> (usize, usize) {
        let rows: Vec<Value> =
            fs::read_to_string(self.rig.dir.join(arca::service_exec::AUDIT_FILE))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        let ids: Vec<_> = rows
            .iter()
            .filter(|row| {
                row["kind"] == "start"
                    && row["token_id"] == arca_mcp::auth::APPROVED_ACTION_ID
                    && row["verb"] == "reach_list"
            })
            .map(|row| row["id"].clone())
            .collect();
        let ends = rows
            .iter()
            .filter(|row| row["kind"] == "end" && ids.contains(&row["id"]))
            .count();
        (ids.len(), ends)
    }
}

fn status_execs(rig: &Rig) -> usize {
    let command = json!(arca_incus::exec::ExecCommand::ConnectorStatus.argv());
    rig.fake
        .service_requests()
        .iter()
        .filter(|request| {
            request.method == "POST"
                && request.path.contains("/exec?")
                && serde_json::from_slice::<Value>(&request.body)
                    .is_ok_and(|body| body["command"] == command)
        })
        .count()
}

#[test]
fn many_gets_share_safe_rows_and_one_audit_pair_until_the_window_ends() {
    let page = PageRig::new();
    page.status("NeedsLogin", false);
    let first = page.get();
    assert_eq!(first["connectors"][0]["state"], "waiting-sign-in");
    for _ in 0..12 {
        let answer = page.get();
        assert_eq!(page.execs(), 1);
        assert_eq!(answer, first);
    }
    page.clock.fetch_add(14_999, Ordering::SeqCst);
    assert_eq!(page.get(), first);
    assert_eq!(page.execs(), 1);
    assert_eq!(page.row_audit(), (1, 1));
    page.clock.fetch_add(1, Ordering::SeqCst);
    page.status("Running", false);
    let fresh = page.get();
    assert_eq!(fresh["connectors"][0]["state"], "running");
    assert_eq!(fresh["connectors"][0]["tailnetName"], "node.example.ts.net");
    assert_eq!(page.execs(), 2);
    assert_eq!(page.row_audit(), (2, 2));
    assert_eq!(page.get(), fresh);
    assert_eq!(page.execs(), 2);
    assert_private(&page.rig, LOGIN);
    assert_private(&page.rig, KEY);
}

#[test]
fn concurrent_gets_wait_for_one_in_flight_status_exec() {
    let page = PageRig::new();
    page.status("NeedsLogin", true);
    std::thread::scope(|scope| {
        let barrier = Arc::new(std::sync::Barrier::new(9));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let barrier = barrier.clone();
                let page = &page;
                scope.spawn(move || {
                    barrier.wait();
                    page.get()
                })
            })
            .collect();
        barrier.wait();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let id = loop {
            let operations = page
                .rig
                .get("/1.0/operations?all-projects=true&recursion=1")
                .unwrap();
            if let Some(id) = operations["running"]
                .as_array()
                .and_then(|running| running.first())
                .and_then(|op| op["id"].as_str())
            {
                break id.to_string();
            }
            assert!(std::time::Instant::now() < until, "the status exec started");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        page.rig.fake.release_operation(&arca_incus::OpId(id));
        let answers: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(page.execs(), 1);
        assert!(answers.iter().all(|answer| answer == &answers[0]));
    });
    assert_eq!(page.execs(), 1);
    assert_eq!(page.row_audit(), (1, 1));
}

#[test]
fn only_a_successful_sign_in_invalidates_the_rows_inside_the_window() {
    let page = PageRig::new();
    page.status("NeedsLogin", false);
    let waiting = page.get();
    let pending = page.begin();
    assert_eq!(page.get(), waiting);
    assert_eq!(page.execs(), 1);
    let mut bad = page.proof(pending.clone());
    bad["name"] = json!("another");
    assert_eq!(page.post(bad).0, 403);
    assert_eq!(page.get(), waiting);
    assert_eq!(page.execs(), 1);
    let signed = page.proof(pending);
    page.status("NeedsLogin", false);
    assert_eq!(page.post(signed.clone()), (200, json!({"signInUrl":LOGIN})));
    assert_eq!(page.execs(), 2, "the sign-in read itself still executes");
    page.status("Running", false);
    let connected = page.get();
    assert_eq!(connected["connectors"][0]["state"], "running");
    assert_eq!(
        page.execs(),
        3,
        "a successful sign-in makes the next GET fresh"
    );
    assert_eq!(page.post(signed).0, 403);
    assert_eq!(page.get(), connected);
    assert_eq!(page.execs(), 3);
    assert_eq!(page.row_audit(), (2, 2));
    assert_private(&page.rig, LOGIN);
    assert_private(&page.rig, KEY);
}

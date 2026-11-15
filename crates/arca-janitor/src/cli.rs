//! The command line. Every verb that talks to the daemon returns at once; only `wait` waits, and never past its
//! timeout.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::config::Config;
use crate::daemon::{Daemon, DaemonOptions, Request, request};
use crate::jobs::{Job, JobParams, KINDS};
use crate::platform;
use crate::store::{Store, socket_path};
use crate::util::{human, stamp};

pub const USAGE: &str = "\
arca-janitor: a polite, job-based cleanup runner

  arca-janitor serve [--foreground]          start the daemon (detached unless --foreground)
  arca-janitor submit <kind> [--field value] queue a job; prints its id at once
  arca-janitor list [--json]                 recent jobs
  arca-janitor status <id> [--json]          a job's state and progress
  arca-janitor result <id> [--json]          a finished job's result
  arca-janitor cancel <id>                   stop a queued or running job
  arca-janitor wait <id> [--timeout <s>]     wait for a job, at most <s> seconds (default 60)
  arca-janitor report [--since 24h|7d|DATE] [--json]   the last clean runs, a few lines each
  arca-janitor schema [<kind>]               JSON Schemas of the kinds' params and results
  arca-janitor config                        the effective config and the files it came from
  arca-janitor shutdown                      stop the daemon

kinds: space-report [path], growth [path], big-files [path], clean-plan, clean-apply <plan-id>,
       trash-purge, nightly, insights, probe
fields: --depth N --top N --min-mb N --changed-hours N --limit N --profile full|daily --only a,b
        --older-hours N --max-age-hours N --days N --seconds N --dry-run --budget-min N --json '{...}'

exit codes: 0 done, 1 the job failed or was cancelled, 2 usage or daemon error, 3 wait timed out
Config: cleanup.conf and cleanup.<host>.conf in $JANITOR_CONFIG_DIR (default ~/.config/janitor).
State: $JANITOR_STATE_DIR (default ~/.local/state/janitor). Socket: $XDG_RUNTIME_DIR/janitor.sock.";

fn fail(msg: impl AsRef<str>) -> i32 {
    eprintln!("arca-janitor: {}", msg.as_ref());
    2
}

/// Turn `submit` arguments into a kind's parameters.
pub fn parse_submit(kind: &str, args: &[String]) -> Result<(JobParams, Option<u64>), String> {
    if !KINDS.contains(&kind) {
        return Err(format!("unknown kind {kind} (kinds: {})", KINDS.join(", ")));
    }
    let mut fields = Map::new();
    let mut budget = None;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if let Some(name) = arg.strip_prefix("--") {
            let next = args.get(i + 1).filter(|n| !n.starts_with("--"));
            if name == "json" {
                let text = next.ok_or("--json needs an object")?;
                match serde_json::from_str::<Value>(text) {
                    Ok(Value::Object(m)) => fields.extend(m),
                    _ => return Err("--json needs a JSON object".to_string()),
                }
                i += 2;
                continue;
            }
            if name == "budget-min" {
                let v = next.ok_or("--budget-min needs minutes")?;
                budget = Some(
                    v.parse()
                        .map_err(|_| "--budget-min needs a number".to_string())?,
                );
                i += 2;
                continue;
            }
            let key = name.replace('-', "_");
            let value = match next {
                None => {
                    i += 1;
                    Value::Bool(true)
                }
                Some(v) => {
                    i += 2;
                    field_value(&key, v)
                }
            };
            fields.insert(key, value);
        } else {
            let key = if kind == "clean-apply" {
                "plan"
            } else {
                "path"
            };
            fields.insert(key.to_string(), field_value(key, arg));
            i += 1;
        }
    }
    JobParams::from_kind(kind, Value::Object(fields)).map(|p| (p, budget))
}

fn field_value(key: &str, v: &str) -> Value {
    match key {
        "only" => Value::Array(
            v.split(',')
                .filter(|s| !s.is_empty())
                .map(|s| Value::String(s.to_string()))
                .collect(),
        ),
        "path" => {
            let p = PathBuf::from(v);
            let p = if p.is_absolute() {
                p
            } else {
                std::env::current_dir().map(|d| d.join(&p)).unwrap_or(p)
            };
            Value::String(p.display().to_string())
        }
        "dry_run" => Value::Bool(v != "false"),
        _ => match v.parse::<u64>() {
            Ok(n) => Value::from(n),
            Err(_) => Value::String(v.to_string()),
        },
    }
}

pub fn run(args: Vec<String>) -> i32 {
    let store = Store::from_env();
    let socket = socket_path(&store);
    let json = args.iter().any(|a| a == "--json");
    let Some(cmd) = args.first().map(String::as_str) else {
        println!("{USAGE}");
        return 2;
    };
    let rest = &args[1..];
    let positional = |n: usize| rest.iter().filter(|a| !a.starts_with("--")).nth(n).cloned();
    match cmd {
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            0
        }
        "version" | "--version" => {
            println!("arca-janitor {}", env!("CARGO_PKG_VERSION"));
            0
        }
        "serve" => {
            if rest.iter().any(|a| a == "--foreground") {
                serve_foreground(store, socket, &args)
            } else {
                serve_detached(&store, &socket)
            }
        }
        "submit" => {
            let Some(kind) = rest.first() else {
                return fail("submit needs a kind");
            };
            let (job, budget_min) = match parse_submit(kind, &rest[1..]) {
                Ok(p) => p,
                Err(e) => return fail(e),
            };
            match request(&socket, &Request::Submit { job, budget_min }) {
                Ok(r) if r.ok => {
                    println!("{}", r.id.unwrap_or_default());
                    0
                }
                Ok(r) => fail(r.error.unwrap_or_default()),
                Err(e) => fail(e),
            }
        }
        "list" => match request(&socket, &Request::List) {
            Ok(r) if r.ok => {
                let jobs = r.jobs.unwrap_or_default();
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&jobs).unwrap_or_default()
                    );
                } else {
                    for j in jobs {
                        let extra = j.paused.or(j.error).unwrap_or(j.phase);
                        println!(
                            "{}  {:<12} {:<9} {}  {}",
                            j.id,
                            j.kind,
                            j.state.as_str(),
                            stamp(j.created),
                            extra
                        );
                    }
                }
                0
            }
            Ok(r) => fail(r.error.unwrap_or_default()),
            Err(e) => fail(e),
        },
        "status" | "result" => {
            let Some(id) = positional(0) else {
                return fail(format!("{cmd} needs a job id"));
            };
            match request(&socket, &Request::Status { id: id.clone() }) {
                Ok(r) if r.ok => {
                    let Some(job) = r.job else {
                        return fail("no job in the answer");
                    };
                    if cmd == "status" {
                        if json {
                            let mut v = serde_json::to_value(&job).unwrap_or(Value::Null);
                            if let Value::Object(m) = &mut v {
                                m.remove("result");
                            }
                            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                        } else {
                            print_status(&job);
                        }
                        0
                    } else {
                        print_result(&job, json)
                    }
                }
                Ok(r) => fail(r.error.unwrap_or_default()),
                Err(e) => fail(e),
            }
        }
        "cancel" => {
            let Some(id) = positional(0) else {
                return fail("cancel needs a job id");
            };
            match request(&socket, &Request::Cancel { id }) {
                Ok(r) if r.ok => {
                    println!("{}", r.message.unwrap_or_default());
                    0
                }
                Ok(r) => fail(r.error.unwrap_or_default()),
                Err(e) => fail(e),
            }
        }
        "wait" => {
            let Some(id) = positional(0) else {
                return fail("wait needs a job id");
            };
            let timeout = match rest.iter().position(|a| a == "--timeout") {
                Some(i) => match rest.get(i + 1).and_then(|v| v.parse::<f64>().ok()) {
                    Some(t) if t >= 0.0 => t,
                    _ => return fail("--timeout needs seconds"),
                },
                None => 60.0,
            };
            wait(&socket, &id, Duration::from_secs_f64(timeout))
        }
        "report" => {
            let since = match rest.iter().position(|a| a == "--since") {
                Some(i) => match rest
                    .get(i + 1)
                    .and_then(|v| crate::report::parse_since(v, crate::util::now()))
                {
                    Some(s) => s,
                    None => return fail("--since takes 24h, 7d, 90m or a YYYY-MM-DD date"),
                },
                None => crate::report::default_since(),
            };
            let lines = crate::report::lines(&store, since);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&lines).unwrap_or_default()
                );
            } else if lines.is_empty() {
                println!("no clean runs since {}", stamp(since));
            } else {
                for l in lines {
                    println!("{l}");
                }
            }
            0
        }
        "schema" => {
            let value = match positional(0) {
                Some(kind) => match (crate::schema::params(&kind), crate::schema::result(&kind)) {
                    (Some(p), Some(r)) => serde_json::json!({ "params": p, "result": r }),
                    _ => return fail(format!("unknown kind {kind}")),
                },
                None => crate::schema::all(),
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&value).unwrap_or_default()
            );
            0
        }
        "config" => match Config::from_env() {
            Ok(c) => {
                println!("{}", serde_json::to_string_pretty(&c).unwrap_or_default());
                0
            }
            Err(e) => fail(e.to_string()),
        },
        "shutdown" => match request(&socket, &Request::Shutdown) {
            Ok(r) if r.ok => {
                println!("{}", r.message.unwrap_or_default());
                0
            }
            Ok(r) => fail(r.error.unwrap_or_default()),
            Err(e) => fail(e),
        },
        other => fail(format!("unknown command {other} (see --help)")),
    }
}

fn print_status(job: &Job) {
    let p = &job.progress;
    let mut line = format!("{} {} {}", job.id, job.kind, job.state.as_str());
    if !p.phase.is_empty() {
        line.push_str(&format!(" ({})", p.phase));
    }
    if p.items > 0 || p.bytes > 0 {
        line.push_str(&format!(", {} items, {}", p.items, human(p.bytes)));
    }
    println!("{line}");
    if let Some(paused) = &p.paused {
        println!("{paused}");
    }
    if p.paused_secs > 0 {
        println!("paused {} s so far for pressure", p.paused_secs);
    }
    if let Some(e) = &job.error {
        println!("error: {e}");
    }
    if let Some(pr) = &job.priority {
        println!(
            "priority: nice {}, io {}",
            pr.nice.map_or("?".to_string(), |n| n.to_string()),
            pr.io.clone().unwrap_or_else(|| "?".into())
        );
    }
    for l in job.log.iter().rev().take(5).rev() {
        println!("  {l}");
    }
}

fn print_result(job: &Job, json: bool) -> i32 {
    if !job.state.finished() {
        println!("{} is {}; no result yet", job.id, job.state.as_str());
        return 3;
    }
    let code = if job.state == crate::jobs::State::Done {
        0
    } else {
        1
    };
    let Some(result) = &job.result else {
        println!(
            "{} {}{}",
            job.id,
            job.state.as_str(),
            job.error
                .as_ref()
                .map(|e| format!(": {e}"))
                .unwrap_or_default()
        );
        return code;
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(result).unwrap_or_default()
        );
        return code;
    }
    if let Some(done) = result.get("done").and_then(Value::as_array) {
        print_apply(result, done.len());
    } else if let Some(lines) = result.get("lines").and_then(Value::as_array) {
        for l in lines {
            println!("{}", l.as_str().unwrap_or(""));
        }
    } else if let Some(findings) = result.get("findings").and_then(Value::as_array) {
        for f in findings {
            println!(
                "{}. {} [{}]",
                f["rank"],
                f["title"].as_str().unwrap_or(""),
                human(f["bytes"].as_u64().unwrap_or(0))
            );
            for e in f["evidence"].as_array().into_iter().flatten() {
                println!("   - {}", e.as_str().unwrap_or(""));
            }
            println!("   fix: {}", f["suggestion"].as_str().unwrap_or(""));
        }
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(result).unwrap_or_default()
        );
    }
    code
}

/// A clean run in a few lines: verified done, skipped and why, failed and why.
fn print_apply(r: &Value, done: usize) {
    let n = |k: &str| r[k].as_u64().unwrap_or(0);
    let list = |k: &str| r[k].as_array().cloned().unwrap_or_default();
    if r["dry_run"].as_bool() == Some(true) {
        println!(
            "dry run, nothing changed: would remove {} items, would free {}",
            list("would").len(),
            human(n("would_free_bytes"))
        );
        return;
    }
    println!(
        "done and verified: {done} items, freed {}, moved {} to trash",
        human(n("freed_bytes")),
        human(n("trash_bytes"))
    );
    let skipped = list("skipped");
    if !skipped.is_empty() {
        println!("skipped: {}", skipped.len());
        for s in skipped.iter().take(10) {
            println!(
                "  {}: {}",
                s["label"].as_str().unwrap_or(""),
                s["reason"].as_str().unwrap_or("")
            );
        }
    }
    let failed = list("failed");
    if !failed.is_empty() {
        println!(
            "FAILED: {} items, {} not freed",
            failed.len(),
            human(n("failed_bytes"))
        );
        for f in failed.iter().take(10) {
            println!(
                "  {}: {}",
                f["label"].as_str().unwrap_or(""),
                f["error"].as_str().unwrap_or("")
            );
        }
    }
    for e in list("errors") {
        println!("ERROR {}", e.as_str().unwrap_or(""));
    }
}

fn wait(socket: &std::path::Path, id: &str, timeout: Duration) -> i32 {
    let start = Instant::now();
    loop {
        match request(socket, &Request::Status { id: id.to_string() }) {
            Ok(r) if r.ok => {
                if let Some(job) = r.job {
                    if job.state.finished() {
                        println!("{} {}", job.id, job.state.as_str());
                        return if job.state == crate::jobs::State::Done {
                            0
                        } else {
                            1
                        };
                    }
                    if start.elapsed() >= timeout {
                        println!(
                            "{} still {} after {:.0} s",
                            job.id,
                            job.state.as_str(),
                            timeout.as_secs_f64()
                        );
                        return 3;
                    }
                }
            }
            Ok(r) => return fail(r.error.unwrap_or_default()),
            Err(e) => return fail(e),
        }
        let left = timeout.saturating_sub(start.elapsed());
        std::thread::sleep(left.min(Duration::from_millis(250)));
    }
}

fn serve_foreground(store: Store, socket: PathBuf, args: &[String]) -> i32 {
    let priority = platform::ensure_lowered(args);
    if !priority.lowered {
        eprintln!(
            "arca-janitor: priority is not lowered (nice {:?}, io {:?}); heavy jobs will be refused",
            priority.nice, priority.io
        );
    }
    if platform::euid() == 0 {
        return fail("refused: run the janitor as a user, never as root");
    }
    let loader = Arc::new(|| Config::from_env().map_err(|e| e.to_string()));
    let daemon = match Daemon::start(DaemonOptions {
        store,
        socket: socket.clone(),
        priority: priority.clone(),
        config: loader,
        throttle: None,
    }) {
        Ok(d) => d,
        Err(e) => return fail(e.to_string()),
    };
    eprintln!(
        "arca-janitor {} listening on {} (nice {}, io {})",
        env!("CARGO_PKG_VERSION"),
        socket.display(),
        priority.nice.map_or("?".into(), |n| n.to_string()),
        priority.io.unwrap_or_else(|| "?".into())
    );
    daemon.wait();
    0
}

/// Start `serve --foreground` in the background, its output in <state>/serve.log, and wait until it answers.
fn serve_detached(store: &Store, socket: &std::path::Path) -> i32 {
    if request(socket, &Request::Ping).is_ok() {
        println!("already running on {}", socket.display());
        return 0;
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return fail(e.to_string()),
    };
    if let Err(e) = std::fs::create_dir_all(&store.root) {
        return fail(e.to_string());
    }
    let log_path = store.root.join("serve.log");
    let log = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => f,
        Err(e) => return fail(e.to_string()),
    };
    let err = match log.try_clone() {
        Ok(f) => f,
        Err(e) => return fail(e.to_string()),
    };
    use std::os::unix::process::CommandExt;
    let child = Command::new(exe)
        .args(["serve", "--foreground"])
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(err)
        .process_group(0)
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) => return fail(e.to_string()),
    };
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if let Ok(r) = request(socket, &Request::Ping) {
            let lowered = r.priority.is_some_and(|p| p.lowered);
            println!(
                "started (pid {}) on {}{}",
                child.id(),
                socket.display(),
                if lowered {
                    ""
                } else {
                    "; priority NOT lowered: heavy jobs will be refused"
                }
            );
            return 0;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    fail(format!(
        "the daemon did not answer within 10 s; see {}",
        log_path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn submit_arguments_become_typed_params() {
        let (p, b) = parse_submit(
            "big-files",
            &s(&["/data", "--min-mb", "100", "--changed-hours", "6"]),
        )
        .unwrap();
        assert_eq!(
            p,
            JobParams::BigFiles(BigFilesParams {
                path: Some("/data".into()),
                min_mb: 100,
                changed_hours: Some(6),
                limit: 50
            })
        );
        assert_eq!(b, None);
        let (p, _) = parse_submit("clean-apply", &s(&["p20261115-1"])).unwrap();
        assert_eq!(
            p,
            JobParams::CleanApply(CleanApplyParams {
                plan: "p20261115-1".into()
            })
        );
        let (p, b) = parse_submit(
            "clean-plan",
            &s(&[
                "--profile",
                "daily",
                "--only",
                "caches,docker",
                "--budget-min",
                "5",
            ]),
        )
        .unwrap();
        assert_eq!(
            p,
            JobParams::CleanPlan(CleanPlanParams {
                profile: Profile::Daily,
                only: vec!["caches".into(), "docker".into()]
            })
        );
        assert_eq!(b, Some(5));
        let (p, _) = parse_submit("nightly", &s(&["--dry-run"])).unwrap();
        assert_eq!(p, JobParams::Nightly(NightlyParams { dry_run: true }));
        assert!(parse_submit("big-files", &s(&["--colour", "red"])).is_err());
        assert!(parse_submit("explode", &[]).is_err());
    }
}

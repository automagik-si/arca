//! Running a function: the timer, one run, its limits, the run log and the meter.
//!
//! One run is one start of the function's own container. The kernel gives the container its image's own root again
//! (a rebuild from the copy of the image the node already holds, or from the registry when that copy is gone), so no
//! run finds what an earlier one wrote and none can leave the next something to execute. A root that cannot be put
//! back is a run that does not start. The kernel then clears the container's console log, starts it, and watches it until its program exits or the run passes its time limit, when the kernel stops it (a stop the
//! program has a few seconds to obey, then a forced one). What the program wrote is read from the console log; its last
//! line is its result.
//!
//! **What a run writes, and where.** To Incus: the instance's root disk (rebuilt), its power state and its console
//! log, through `arca_incus::function_run` and nothing else. To the state directory: one line in the run log
//! (`function-runs/runs-<year>-<month>.jsonl`) and the end of the run's output
//! (`function-runs/output/<function>/<run>.log`). A run writes nothing to the desired state and nothing to the effect
//! journal: neither is touched, read apart, by anything in this module. Thousands of runs a day leave both as they were.
//!
//! **The run log is the meter.** One line for each run, never rewritten: the run's id, its function and digest, what
//! triggered it, when it started, where its root came from and how long that, and the start, took, how long it ran, the CPU time and the memory peak when they
//! could be read, the memory it was allowed, its outcome and its result line. A month has its own file, and no file
//! is ever cut, because runs will be billed from them.
//!
//! **Outcomes.** `ok` and `failed` are the program's own word: its last output line is one JSON object with `"ok"`.
//! `timeout`, `memory`, `died`, `interrupted` and `notStarted` are the kernel's: the run passed its time limit; it was
//! killed at its memory limit; it ended without a result line; the kernel stopped it for another reason (its own
//! restart, the function's removal); it was never started, and the line says why.
//!
//! Nothing is retried: the next tick of the timer is the next try.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use arca_incus::function_run::FunctionRun;
use arca_incus::{IncusError, InstanceState, ProjectFamily};
use arca_kernel::desired::Entry;
use serde_json::{Value, json};

use crate::config::NodeConfig;
use crate::handlers::Clock;
use crate::kernel::Kernel;
use crate::service::ServiceConfig;

/// The directory of the run log in the state directory.
pub const RUNS_DIR: &str = "function-runs";
/// How often a running function is looked at.
pub const POLL: Duration = Duration::from_millis(250);
/// How long a program has to exit after it is told to stop, before it is killed.
pub const STOP_GRACE: Duration = Duration::from_secs(5);
/// The least time between the end of one run of a function and the next start of it. Incus is still putting a
/// container away for a moment after its program exits (about 0.6 s on Incus 7.5.1): a start in that moment can be
/// refused, and a log cleared in it can come back with the older run's output in front of the new one's. With two
/// seconds between them neither was seen in forty runs. The shortest timer is far longer, so only a manual run that
/// follows another at once waits for it.
pub const SETTLE: Duration = Duration::from_secs(2);
/// How long the output of a run that has just ended is asked for again while Incus answers the read with an error.
pub const OUTPUT_PATIENCE: Duration = Duration::from_secs(3);
/// The longest result line, in bytes. A longer last line is not a result.
pub const RESULT_MAX: usize = 4096;
/// How much of the end of a run's output is kept, in bytes.
pub const OUTPUT_KEEP: usize = 64 * 1024;
/// How many runs of one function keep their output.
pub const OUTPUTS_KEPT: usize = 20;
/// How many of a function's newest lines are held in memory for `function_runs`.
pub const RECENT: usize = 100;
/// How much of the end of a month's file is read at start to fill the lines held in memory.
const TAIL_BYTES: u64 = 4 * 1024 * 1024;
/// The reads of a running instance that may fail in a row before the run is given up as lost.
const LOST_AFTER: u32 = 20;

/// Why a run did not begin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No function of that name.
    Unknown,
    /// A run of it is going: one run at a time.
    Busy,
    /// It is being removed.
    Retiring,
}

/// One function as the runner sees it between two ticks.
#[derive(Default)]
struct Flight {
    /// When its last run began, in milliseconds. `None` before its first run in this process.
    last_start: Option<u64>,
    /// The run that is going, when one is.
    running: Option<Arc<AtomicBool>>,
    /// When its last run ended, or its instance was last stopped by the runner.
    last_end: Option<Instant>,
    /// No new run begins: the function is being removed.
    retiring: bool,
}

/// A run that has begun and not ended: it holds its function's one place.
pub struct Ticket {
    pub run: String,
    name: String,
    trigger: &'static str,
    started_ms: u64,
    cancel: Arc<AtomicBool>,
}

/// Where the memory and CPU of a running instance are read: the node's own cgroup tree, when the node's Incus is on
/// this machine. Anywhere else nothing is read, and the meter says so with `null`.
pub type CgroupRoot = Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>;

/// The runner of a node's functions.
pub struct FunctionRunner {
    kernel: Arc<Mutex<Kernel>>,
    cfg: Arc<NodeConfig>,
    clock: Clock,
    writer: FunctionRun,
    flights: Mutex<BTreeMap<String, Flight>>,
    log: Mutex<RunLog>,
    sequence: AtomicU64,
    poll: Duration,
    grace: Duration,
    settle: Duration,
    cgroup: CgroupRoot,
}

struct RunLog {
    dir: PathBuf,
    recent: BTreeMap<String, VecDeque<Value>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FunctionRunner {
    /// The runner of the node `cfg` describes. It reads the newest lines of the newest two months of the run log into
    /// memory, when there is a run log. It writes nothing: the run log's directory is made by the first line a run
    /// writes, so a node with no function has none.
    pub fn new(
        kernel: Arc<Mutex<Kernel>>,
        cfg: Arc<NodeConfig>,
        clock: Clock,
    ) -> Result<FunctionRunner, String> {
        let family = ProjectFamily::new(
            [arca_netproject::PROJECT],
            cfg.kernel.droidd_projects.iter().map(String::as_str),
        )?;
        let dir = cfg.state_dir.join(RUNS_DIR);
        // A run log that cannot be read is not a reason to run nothing: the lines held in memory start empty.
        let log = RunLog::open(dir);
        Ok(FunctionRunner {
            writer: FunctionRun::new(cfg.incus.socket.clone(), family),
            kernel,
            cfg,
            clock,
            flights: Mutex::new(BTreeMap::new()),
            log: Mutex::new(log),
            sequence: AtomicU64::new(0),
            poll: POLL,
            grace: STOP_GRACE,
            settle: SETTLE,
            cgroup: Arc::new(|| None),
        })
    }

    /// The same runner, looking at a running function every `poll`, giving a program `grace` to obey a stop, and
    /// keeping `settle` between the end of one run of a function and its next start.
    pub fn with_pace(
        mut self,
        poll: Duration,
        grace: Duration,
        settle: Duration,
    ) -> FunctionRunner {
        self.poll = poll;
        self.grace = grace;
        self.settle = settle;
        self
    }

    /// The same runner, reading memory and CPU under the cgroup tree `root` names at each run.
    pub fn with_cgroup(mut self, root: CgroupRoot) -> FunctionRunner {
        self.cgroup = root;
        self
    }

    /// Every function the kernel records, with its entry.
    pub fn functions(&self) -> Vec<(Entry, ServiceConfig)> {
        // The kernel's lock is held for the copy alone: the records are read after it is given back.
        let entries = lock(&self.kernel).entries().to_vec();
        function_entries(&entries)
    }

    fn function(&self, name: &str) -> Option<(Entry, ServiceConfig)> {
        self.functions()
            .into_iter()
            .find(|(_, config)| config.name == name)
    }

    /// Whether a run of `name` is going now.
    pub fn running(&self, name: &str) -> bool {
        lock(&self.flights)
            .get(name)
            .is_some_and(|flight| flight.running.is_some())
    }

    /// Takes the one place of the function `name` for a run that `trigger` asked for.
    pub fn begin(&self, name: &str, trigger: &'static str) -> Result<Ticket, Refusal> {
        if self.function(name).is_none() {
            return Err(Refusal::Unknown);
        }
        let now = (self.clock)();
        let mut flights = lock(&self.flights);
        let flight = flights.entry(name.to_string()).or_default();
        if flight.retiring {
            return Err(Refusal::Retiring);
        }
        if flight.running.is_some() {
            return Err(Refusal::Busy);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        flight.running = Some(Arc::clone(&cancel));
        flight.last_start = Some(now);
        Ok(Ticket {
            run: format!(
                "fr-{now}-{}",
                self.sequence.fetch_add(1, Ordering::SeqCst) + 1
            ),
            name: name.to_string(),
            trigger,
            started_ms: now,
            cancel,
        })
    }

    /// Runs the run `ticket` holds, to its end, writes its line and gives the function's place back. Returns the line.
    pub fn run(&self, ticket: Ticket) -> Value {
        let line = self.execute(&ticket);
        self.record(&ticket.name, &line);
        if let Some(flight) = lock(&self.flights).get_mut(&ticket.name) {
            flight.running = None;
            flight.last_end = Some(Instant::now());
        }
        line
    }

    /// Begins a run of `name` on a thread of its own and returns its id at once.
    pub fn spawn(self: &Arc<Self>, name: &str, trigger: &'static str) -> Result<String, Refusal> {
        let ticket = self.begin(name, trigger)?;
        let id = ticket.run.clone();
        let runner = Arc::clone(self);
        let name = name.to_string();
        let spawned = std::thread::Builder::new()
            .name(format!("arca-fn-{name}"))
            .spawn(move || {
                runner.run(ticket);
            });
        if spawned.is_err() {
            // No thread, no run: the place is given back, and the next tick tries again.
            if let Some(flight) = lock(&self.flights).get_mut(&name) {
                flight.running = None;
            }
            return Err(Refusal::Busy);
        }
        Ok(id)
    }

    /// One tick of the timer at `now`: every function whose timer is due, and that is not running, begins a run. A
    /// function is due at its first tick in this process, and then every `everySeconds` after its last start. Returns
    /// the runs it began.
    pub fn tick(self: &Arc<Self>, now: u64) -> Vec<String> {
        let mut begun = Vec::new();
        for (_, config) in self.functions() {
            let Some(function) = &config.function else {
                continue;
            };
            let due = {
                let flights = lock(&self.flights);
                match flights.get(&config.name) {
                    None => true,
                    Some(flight) => {
                        !flight.retiring
                            && flight.running.is_none()
                            && flight.last_start.is_none_or(|last| {
                                now >= last.saturating_add(function.every_seconds * 1000)
                            })
                    }
                }
            };
            if due && let Ok(id) = self.spawn(&config.name, "timer") {
                begun.push(id);
            }
        }
        begun
    }

    /// Starts the timer: one thread that ticks every second for as long as the process lives.
    pub fn spawn_timer(self: Arc<Self>) -> std::io::Result<()> {
        std::thread::Builder::new()
            .name("arca-fn-timer".to_string())
            .spawn(move || {
                loop {
                    let now = (self.clock)();
                    self.tick(now);
                    std::thread::sleep(Duration::from_secs(1));
                }
            })
            .map(|_| ())
    }

    /// What a start of the kernel does before the timer runs: a function instance found running was started by a
    /// kernel that is gone, so nobody watches it. It is stopped, and the run log says `interrupted`. Returns the names.
    pub fn sweep(&self) -> Vec<String> {
        let mut stopped = Vec::new();
        for (entry, config) in self.functions() {
            let Ok(seen) = self.writer.read(&config.project, &config.instance) else {
                continue;
            };
            if seen.is_stopped() {
                continue;
            }
            // Only the instance Arca created for the function is stopped: the one whose uuid the effect journal
            // holds for its entry, as a run checks before it starts one.
            let journalled = lock(&self.kernel).service_uuid(&entry.id, &config.instance);
            if journalled.is_none() || journalled != seen.uuid {
                eprintln!(
                    "arca: an instance named {} runs with a function's mark and is not the one Arca created for that function: it was left alone",
                    config.name
                );
                continue;
            }
            let now = (self.clock)();
            let why = match self.writer.stop(&seen, Duration::ZERO) {
                Ok(_) => "a run was still going when the kernel started, with nothing watching it: it was stopped".to_string(),
                Err(error) => format!(
                    "a run was still going when the kernel started, and it could not be stopped: {}",
                    incus_words(&error)
                ),
            };
            let line = self.line(
                &format!(
                    "fr-{now}-{}",
                    self.sequence.fetch_add(1, Ordering::SeqCst) + 1
                ),
                &config,
                "kernel",
                now,
                Measured::default(),
                "interrupted",
                None,
                Some(why),
            );
            self.record(&config.name, &line);
            lock(&self.flights)
                .entry(config.name.clone())
                .or_default()
                .last_end = Some(Instant::now());
            stopped.push(config.name.clone());
        }
        stopped
    }

    /// Stops new runs of `name` and ends the one that is going, for its removal. Returns once no run is going, or
    /// `false` when one still was after `wait`.
    pub fn retire(&self, name: &str, wait: Duration) -> bool {
        let until = Instant::now() + wait;
        loop {
            {
                let mut flights = lock(&self.flights);
                let flight = flights.entry(name.to_string()).or_default();
                flight.retiring = true;
                match &flight.running {
                    None => return true,
                    Some(cancel) => cancel.store(true, Ordering::SeqCst),
                }
            }
            if Instant::now() >= until {
                return false;
            }
            std::thread::sleep(self.poll.min(Duration::from_millis(50)));
        }
    }

    /// Takes back [`FunctionRunner::retire`]: the removal did not happen, so the function runs again.
    pub fn restore(&self, name: &str) {
        if let Some(flight) = lock(&self.flights).get_mut(name) {
            flight.retiring = false;
        }
    }

    /// Forgets a removed function: its place, its lines held in memory and the output its runs kept. Its lines in
    /// the run log stay.
    pub fn forget(&self, name: &str) {
        lock(&self.flights).remove(name);
        let mut log = lock(&self.log);
        log.recent.remove(name);
        let _ = fs::remove_dir_all(log.dir.join("output").join(name));
    }

    /// The newest `limit` lines of the function `name`, newest first.
    pub fn runs(&self, name: &str, limit: usize) -> Vec<Value> {
        lock(&self.log)
            .recent
            .get(name)
            .map(|lines| lines.iter().rev().take(limit).cloned().collect())
            .unwrap_or_default()
    }

    /// The line of the run `run` of the function `name`, and the end of its output when it is still kept.
    pub fn run_output(&self, name: &str, run: &str) -> Option<(Value, Option<String>)> {
        let log = lock(&self.log);
        let line = log
            .recent
            .get(name)?
            .iter()
            .find(|line| line["run"] == run)?
            .clone();
        let output = run_id_ok(run)
            .then(|| fs::read_to_string(log.output_path(name, run)).ok())
            .flatten();
        Some((line, output))
    }

    fn record(&self, name: &str, line: &Value) {
        if let Err(error) = lock(&self.log).append(name, line) {
            eprintln!(
                "arca: the run {} of function {name} was not written to the run log: {error}",
                line["run"].as_str().unwrap_or("?")
            );
        }
    }

    /// The secret values a line or an output must never show: every stored secret of the node.
    fn secrets(&self) -> Vec<String> {
        crate::handlers::service_secret_values(&self.cfg.state_dir.join("secrets"))
    }

    #[allow(clippy::too_many_arguments)]
    fn line(
        &self,
        run: &str,
        config: &ServiceConfig,
        trigger: &str,
        started_ms: u64,
        measured: Measured,
        outcome: &str,
        result: Option<Value>,
        why: Option<String>,
    ) -> Value {
        let function = config.function.as_ref();
        let limit = config
            .limits
            .as_ref()
            .and_then(|limits| limits.memory.as_deref())
            .and_then(crate::service::size_bytes)
            .and_then(|bytes| u64::try_from(bytes).ok());
        let mut line = json!({
            "run": run,
            "function": config.name,
            "digest": function.map(|function| function.digest.clone()),
            "kind": "container",
            "trigger": trigger,
            "startedAt": started_ms,
            "root": measured.root,
            "rootMs": measured.root_ms,
            "startMs": measured.start_ms,
            "durationMs": measured.duration_ms,
            "cpuMs": measured.cpu_ms,
            "memoryPeakBytes": measured.memory_peak,
            "memoryLimitBytes": limit,
            "outcome": outcome,
            "result": result,
        });
        if let Some(why) = why {
            line["why"] = json!(crate::service::redact_log(&why, &self.secrets()));
        }
        line
    }

    fn execute(&self, ticket: &Ticket) -> Value {
        let not_started = |config: &ServiceConfig, why: String| {
            self.line(
                &ticket.run,
                config,
                ticket.trigger,
                ticket.started_ms,
                Measured::default(),
                "notStarted",
                None,
                Some(why),
            )
        };
        let Some((entry, config)) = self.function(&ticket.name) else {
            return json!({
                "run": ticket.run, "function": ticket.name, "kind": "container", "trigger": ticket.trigger,
                "startedAt": ticket.started_ms, "outcome": "notStarted", "result": null,
                "why": "the function was removed before the run began",
            });
        };
        let Some(function) = config.function.clone() else {
            return not_started(&config, "the record is not a function's".to_string());
        };
        let missing = crate::function::missing_secrets(&config);
        if !missing.is_empty() {
            return not_started(
                &config,
                format!(
                    "{} not set: function_secret_set sets {}",
                    match missing.as_slice() {
                        [one] => format!("the secret {one} is"),
                        many => format!("the secrets {} are", many.join(", ")),
                    },
                    if missing.len() == 1 { "it" } else { "each" }
                ),
            );
        }
        let seen = match self.writer.read(&config.project, &config.instance) {
            Ok(seen) => seen,
            Err(error) => {
                return not_started(
                    &config,
                    format!("its instance was not read: {}", incus_words(&error)),
                );
            }
        };
        // The instance must be the one this function's create made: the uuid the effect journal holds for its entry.
        let journalled = lock(&self.kernel).service_uuid(&entry.id, &config.instance);
        if journalled.is_none() || journalled != seen.uuid {
            return not_started(
                &config,
                "its instance is not the one Arca created for it, so nothing was started"
                    .to_string(),
            );
        }
        // Incus needs a moment to put the last run away before the next may start.
        let mut wait = lock(&self.flights)
            .get(&ticket.name)
            .and_then(|flight| flight.last_end)
            .map_or(Duration::ZERO, |end| {
                self.settle.saturating_sub(end.elapsed())
            });
        if !seen.is_stopped() {
            if let Err(error) = self.writer.stop(&seen, Duration::ZERO) {
                return not_started(
                    &config,
                    format!(
                        "its instance was already running and could not be stopped: {}",
                        incus_words(&error)
                    ),
                );
            }
            wait = self.settle;
        }
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
        // Every run starts from its image's own root, or not at all.
        let rooting = Instant::now();
        let root = match self.writer.fresh_root(&seen, &config.source) {
            Ok(root) => root,
            Err(error) => {
                return not_started(
                    &config,
                    format!(
                        "its root could not be put back to its image's own, so nothing was started: {}",
                        incus_words(&error)
                    ),
                );
            }
        };
        let root_ms = millis(rooting.elapsed());
        // A run starts with an empty log, or not at all: what is read after it is then this run's output alone.
        let cleared = self
            .writer
            .clear_output(&seen)
            .and_then(|()| self.writer.output(&seen));
        match cleared {
            Ok(text) if text.trim().is_empty() => {}
            Ok(_) => {
                return not_started(
                    &config,
                    "its console log could not be cleared, so this run's output could not be told from an older run's".to_string(),
                );
            }
            Err(error) => {
                return not_started(
                    &config,
                    format!(
                        "its console log could not be cleared: {}",
                        incus_words(&error)
                    ),
                );
            }
        }
        // A removal that came while this run was getting ready: nothing is started.
        if ticket.cancel.load(Ordering::SeqCst) {
            return self.line(
                &ticket.run,
                &config,
                ticket.trigger,
                ticket.started_ms,
                Measured::default(),
                "interrupted",
                None,
                Some("the function is being removed, so its run was not started".to_string()),
            );
        }
        let begun = Instant::now();
        if let Err(error) = self.writer.start(&seen) {
            // A start that failed may still have left it running.
            let _ = self.writer.stop(&seen, Duration::ZERO);
            return not_started(
                &config,
                format!("Incus did not start it: {}", incus_words(&error)),
            );
        }
        let mut measured = Measured {
            start_ms: Some(millis(begun.elapsed())),
            root: Some(root.as_str()),
            root_ms: Some(root_ms),
            ..Measured::default()
        };
        let running = Instant::now();
        let limit = Duration::from_secs(function.seconds);
        let cgroup = (self.cgroup)().map(|root| {
            root.join(format!(
                "lxc.payload.{}_{}",
                config.project, config.instance
            ))
        });
        let mut ended = Ended::Exited;
        let mut kept_output = String::new();
        let mut failed_reads = 0u32;
        let mut looks = 0u32;
        loop {
            if let Some(cgroup) = &cgroup {
                measured.sample(cgroup);
            }
            match self.writer.status(&seen) {
                Ok(status) if status == "Stopped" => break,
                Ok(_) => failed_reads = 0,
                Err(_) => {
                    failed_reads += 1;
                    if failed_reads >= LOST_AFTER {
                        ended = Ended::Lost;
                        let _ = self.writer.stop(&seen, Duration::ZERO);
                        break;
                    }
                }
            }
            if ticket.cancel.load(Ordering::SeqCst) {
                ended = Ended::Cancelled(self.stop(&seen, Duration::ZERO));
                break;
            }
            if running.elapsed() >= limit {
                if let Some(cgroup) = &cgroup {
                    measured.sample(cgroup);
                }
                ended = Ended::TimedOut(self.stop(&seen, self.grace));
                break;
            }
            // The output so far is kept as it grows, once a second: should the log be gone after the run, the last
            // look still holds what was written.
            looks += 1;
            if looks.is_multiple_of(4)
                && let Ok(text) = self.writer.output(&seen)
                && !text.is_empty()
            {
                kept_output = text;
            }
            std::thread::sleep(self.poll);
        }
        measured.duration_ms = Some(millis(running.elapsed()));
        let output = match self.writer.output_after_exit(&seen, OUTPUT_PATIENCE) {
            Ok(text) if !text.is_empty() => text,
            _ => kept_output,
        };
        let secrets = self.secrets();
        let clean = clean_output(&output, &secrets);
        self.keep_output(&config.name, &ticket.run, &clean);
        let result = result_line(&clean, &secrets);
        let (outcome, why) = match ended {
            Ended::TimedOut(stop) => (
                "timeout",
                Some(match stop {
                    Ok(forced) => format!(
                        "it was still running after {} second{}, its time limit, and was {}",
                        function.seconds,
                        if function.seconds == 1 { "" } else { "s" },
                        if forced { "killed" } else { "stopped" }
                    ),
                    Err(error) => format!(
                        "it was still running after its time limit, and it could not be stopped: {error}"
                    ),
                }),
            ),
            Ended::Cancelled(stop) => (
                "interrupted",
                Some(match stop {
                    Ok(_) => "the function is being removed, so its run was stopped".to_string(),
                    Err(error) => format!(
                        "the function is being removed, and its run could not be stopped: {error}"
                    ),
                }),
            ),
            Ended::Lost => (
                "died",
                Some("its instance could no longer be read, so the run was given up".to_string()),
            ),
            Ended::Exited => match &result {
                Some(result) if result["ok"] == true => ("ok", None),
                Some(_) => ("failed", None),
                None if measured.oom_kills.is_some_and(|kills| kills > 0) => (
                    "memory",
                    Some("it was killed at its memory limit".to_string()),
                ),
                None => (
                    "died",
                    Some("it ended without a result: its last output line is not one JSON object with \"ok\"".to_string()),
                ),
            },
        };
        self.line(
            &ticket.run,
            &config,
            ticket.trigger,
            ticket.started_ms,
            measured,
            outcome,
            result,
            why,
        )
    }

    fn stop(&self, seen: &InstanceState, grace: Duration) -> Result<bool, String> {
        self.writer
            .stop(seen, grace)
            .map_err(|error| incus_words(&error))
    }

    fn keep_output(&self, name: &str, run: &str, clean: &str) {
        let log = lock(&self.log);
        if let Err(error) = log.keep_output(name, run, clean) {
            eprintln!("arca: the output of run {run} of function {name} was not kept: {error}");
        }
    }
}

/// How a run that was started ended.
enum Ended {
    /// Its program exited by itself.
    Exited,
    /// It passed its time limit and the kernel stopped it: whether the stop was forced, or why it failed.
    TimedOut(Result<bool, String>),
    /// The kernel stopped it for the function's removal.
    Cancelled(Result<bool, String>),
    /// Its instance could not be read any more.
    Lost,
}

/// What the meter read of one run. Each is `None` when it was not read.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Measured {
    /// Where the image the root was rebuilt from came from, and how long the rebuild took.
    root: Option<&'static str>,
    root_ms: Option<u64>,
    start_ms: Option<u64>,
    duration_ms: Option<u64>,
    cpu_ms: Option<u64>,
    memory_peak: Option<u64>,
    oom_kills: Option<u64>,
}

impl Measured {
    /// Reads the instance's cgroup files and keeps the highest of each. The files are gone when the instance stops, so
    /// the last read before the exit is what the meter has: a peak that is a lower bound.
    fn sample(&mut self, cgroup: &Path) {
        let number = |file: &str| {
            fs::read_to_string(cgroup.join(file))
                .ok()
                .and_then(|text| text.trim().parse::<u64>().ok())
        };
        let field = |file: &str, key: &str| {
            fs::read_to_string(cgroup.join(file)).ok().and_then(|text| {
                text.lines().find_map(|line| {
                    let (name, value) = line.split_once(' ')?;
                    (name == key).then(|| value.trim().parse::<u64>().ok())?
                })
            })
        };
        let peak = number("memory.peak").or_else(|| number("memory.current"));
        self.memory_peak = self.memory_peak.max(peak);
        self.cpu_ms = self
            .cpu_ms
            .max(field("cpu.stat", "usage_usec").map(|usec| usec / 1000));
        self.oom_kills = self.oom_kills.max(field("memory.events", "oom_kill"));
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// An Incus error in words a run log may hold: never a body, and at most a short reason.
fn incus_words(error: &IncusError) -> String {
    let text = error.to_string();
    text.chars().take(240).collect()
}

/// The function records among `entries`.
pub fn function_entries(entries: &[Entry]) -> Vec<(Entry, ServiceConfig)> {
    entries
        .iter()
        .filter(|entry| entry.plugin.0 == "arca.service")
        .filter_map(|entry| {
            let config = serde_json::from_value::<ServiceConfig>(entry.config.clone()).ok()?;
            config.function.is_some().then(|| (entry.clone(), config))
        })
        .collect()
}

/// A run's output as it is kept and shown: each line cleaned of control sequences, with every known secret value
/// taken out, and only its end when it is long.
pub fn clean_output(output: &str, secrets: &[String]) -> String {
    let patterns = secret_patterns(secrets);
    let redact = |text: &str| -> String {
        let mut out = text.to_string();
        for pattern in &patterns {
            out = pattern
                .replace_all(&out, regex::NoExpand("[secret]"))
                .into_owned();
        }
        crate::service::redact_log(&out, secrets)
    };
    let mut lines = Vec::new();
    for line in output.split(['\n', '\r']) {
        if line.trim().is_empty() {
            continue;
        }
        // A value is taken out of the line as the program wrote it, before its white space is folded: a value
        // that holds two spaces in a row would not be found afterwards. And once more after the folding.
        let redacted = redact(line);
        let clean = crate::service::clean_log(&redacted);
        if !clean.is_empty() {
            lines.push(redact(&clean));
        }
    }
    let mut kept = Vec::new();
    let mut size = 0usize;
    for line in lines.into_iter().rev() {
        size += line.len() + 1;
        if size > OUTPUT_KEEP && !kept.is_empty() {
            break;
        }
        kept.push(line);
    }
    kept.reverse();
    kept.join("\n")
}

/// The result of a run: its last output line, when that line is one JSON object with a boolean `"ok"` and is at most
/// [`RESULT_MAX`] bytes. Anything else is no result. Every string of the decoded object, a name or a value at any
/// depth, has each of `secrets` taken out of it: a value the program wrote in an escaped form no text search of the
/// line finds is plain again once the line is decoded, and it is the decoded object that is kept and shown.
pub fn result_line(clean: &str, secrets: &[String]) -> Option<Value> {
    let last = clean.lines().rev().find(|line| !line.trim().is_empty())?;
    if last.len() > RESULT_MAX {
        return None;
    }
    let mut value: Value = serde_json::from_str(last.trim()).ok()?;
    if !(value.is_object() && value["ok"].is_boolean()) {
        return None;
    }
    redact_value(&mut value, secrets);
    Some(value)
}

/// Takes each of `secrets` out of every string in `value`: each value and each name, at any depth.
fn redact_value(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(text) => *text = crate::service::redact_log(text, secrets),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| redact_value(item, secrets)),
        Value::Object(map) => {
            let fields = std::mem::take(map);
            for (key, mut item) in fields {
                redact_value(&mut item, secrets);
                map.insert(crate::service::redact_log(&key, secrets), item);
            }
        }
        _ => {}
    }
}

/// One pattern for each secret value of 8 characters or more, longest first. A pattern finds the value as it is
/// and as any JSON encoder may write it inside a string: each character stands as itself or as an escape of it, a
/// `\\uXXXX` with its hex digits in either case (two of them for a character outside the basic plane) or, for the
/// characters that have one, the short escape (`\\"`, `\\\\`, `\\/`, `\\'`). Encoders differ in which characters they
/// escape (some write `&`, `<`, `>`, `=`, `'`, `+` or a back quote as `\\uXXXX`, some everything outside ASCII), so the
/// pattern asks for none of them and takes all. A value whose pattern cannot be built is still looked for as it is.
fn secret_patterns(secrets: &[String]) -> Vec<regex::Regex> {
    let mut values: Vec<&String> = secrets.iter().filter(|value| value.len() >= 8).collect();
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    values
        .into_iter()
        .filter_map(|value| {
            let mut pattern = String::new();
            for ch in value.chars() {
                let mut units = [0u16; 2];
                let escaped: String = ch
                    .encode_utf16(&mut units)
                    .iter()
                    .map(|unit| format!(r"\\u(?i:{unit:04x})"))
                    .collect();
                let plain = regex::escape(&ch.to_string());
                let short = match ch {
                    '"' | '\\' | '/' | '\'' => format!(r"|\\{plain}"),
                    _ => String::new(),
                };
                pattern.push_str(&format!("(?:{plain}|{escaped}{short})"));
            }
            regex::RegexBuilder::new(&pattern)
                .size_limit(64 * 1024 * 1024)
                .build()
                .ok()
        })
        .collect()
}

/// Whether `run` is an id this module gives: `fr-<milliseconds>-<number>`.
pub fn run_id_ok(run: &str) -> bool {
    let mut parts = run.split('-');
    parts.next() == Some("fr")
        && parts.next().is_some_and(|ms| {
            !ms.is_empty() && ms.len() <= 16 && ms.bytes().all(|b| b.is_ascii_digit())
        })
        && parts.next().is_some_and(|n| {
            !n.is_empty() && n.len() <= 12 && n.bytes().all(|b| b.is_ascii_digit())
        })
        && parts.next().is_none()
}

impl RunLog {
    /// The run log in `dir`, with its newest lines read into memory when the directory is there. Nothing is made:
    /// a node that never ran a function has no run log.
    fn open(dir: PathBuf) -> RunLog {
        let mut log = RunLog {
            dir,
            recent: BTreeMap::new(),
        };
        if let Err(error) = log.read_recent()
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "arca: the function run log could not be read, so function_runs starts with no line: {error}"
            );
        }
        log
    }

    /// Makes the run log's directories, private to the node's user, when they are not there yet.
    fn ensure(&self) -> std::io::Result<()> {
        for path in [self.dir.clone(), self.dir.join("output")] {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    fn read_recent(&mut self) -> std::io::Result<()> {
        let log = self;
        let mut months: Vec<PathBuf> = fs::read_dir(&log.dir)?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("runs-") && name.ends_with(".jsonl"))
            })
            .collect();
        months.sort();
        let newest = months.len().saturating_sub(2);
        for path in &months[newest..] {
            for line in tail_lines(path)? {
                if let Ok(value) = serde_json::from_str::<Value>(&line)
                    && let Some(name) = value["function"].as_str().map(str::to_string)
                {
                    log.remember(&name, value);
                }
            }
        }
        Ok(())
    }

    fn remember(&mut self, name: &str, line: Value) {
        let lines = self.recent.entry(name.to_string()).or_default();
        lines.push_back(line);
        while lines.len() > RECENT {
            lines.pop_front();
        }
    }

    /// Appends one line to the file of the month its run started in, and holds it in memory.
    fn append(&mut self, name: &str, line: &Value) -> std::io::Result<()> {
        self.remember(name, line.clone());
        self.ensure()?;
        let started = line["startedAt"].as_u64().unwrap_or(0);
        let month: String = crate::handlers::iso_utc(started).chars().take(7).collect();
        let path = self.dir.join(format!("runs-{month}.jsonl"));
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        let mut bytes = serde_json::to_vec(line).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        file.sync_data()
    }

    fn output_path(&self, name: &str, run: &str) -> PathBuf {
        self.dir
            .join("output")
            .join(name)
            .join(format!("{run}.log"))
    }

    /// Keeps the end of one run's output, and of each function only its newest [`OUTPUTS_KEPT`].
    fn keep_output(&self, name: &str, run: &str, clean: &str) -> std::io::Result<()> {
        if !crate::service::service_name_ok(name) || !run_id_ok(run) {
            return Ok(());
        }
        self.ensure()?;
        let dir = self.dir.join("output").join(name);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(self.output_path(name, run))?;
        file.write_all(clean.as_bytes())?;
        // A run's id begins with the time it started, so the oldest files are the ones with the lowest numbers.
        let mut kept: Vec<(u64, u64, PathBuf)> = fs::read_dir(&dir)?
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let stem = path.file_stem()?.to_str()?.to_string();
                let mut parts = stem.strip_prefix("fr-")?.split('-');
                let ms = parts.next()?.parse().ok()?;
                let n = parts.next()?.parse().ok()?;
                Some((ms, n, path))
            })
            .collect();
        kept.sort();
        let extra = kept.len().saturating_sub(OUTPUTS_KEPT);
        for (_, _, path) in &kept[..extra] {
            let _ = fs::remove_file(path);
        }
        Ok(())
    }
}

/// The whole lines in the last [`TAIL_BYTES`] of a file.
fn tail_lines(path: &Path) -> std::io::Result<Vec<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(path)?;
    let size = file.metadata()?.len();
    let from = size.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(from))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    // A read that began inside a line drops that first, partial line.
    if from > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_is_the_last_line_when_it_is_a_json_object_with_ok() {
        assert_eq!(
            result_line("working\n{\"ok\":true,\"published\":3}", &[]),
            Some(json!({ "ok": true, "published": 3 }))
        );
        assert_eq!(
            result_line("{\"ok\":false}\n\n", &[]),
            Some(json!({ "ok": false }))
        );
        for none in [
            "",
            "done",
            "{\"ok\":true}\nand then more",
            "{\"ok\":\"yes\"}",
            "[true]",
            "{\"published\":3}",
        ] {
            assert_eq!(result_line(none, &[]), None, "{none:?}");
        }
        let long = format!("{{\"ok\":true,\"pad\":\"{}\"}}", "x".repeat(RESULT_MAX));
        assert_eq!(result_line(&long, &[]), None);
    }

    #[test]
    fn output_is_cleaned_redacted_and_cut_to_its_end() {
        let clean = clean_output(
            "first\r\n\n\u{1b}[31msecond s3cr3t-value\u{1b}[0m\n{\"ok\":true}\n",
            &["s3cr3t-value".to_string()],
        );
        assert!(!clean.contains("s3cr3t-value"), "{clean}");
        assert!(!clean.contains('\u{1b}'), "{clean}");
        assert_eq!(clean.lines().count(), 3);
        assert_eq!(result_line(&clean, &[]), Some(json!({ "ok": true })));
        let long: String = (0..5000)
            .map(|n| format!("line {n} {}\n", "x".repeat(40)))
            .collect();
        let kept = clean_output(&long, &[]);
        assert!(kept.len() <= OUTPUT_KEEP, "{}", kept.len());
        assert!(kept.ends_with(&format!("line 4999 {}", "x".repeat(40))));
    }

    #[test]
    fn a_secret_is_taken_out_in_each_form_an_encoder_writes_and_out_of_the_decoded_result() {
        // A value with everything an encoder may choose to escape: a letter outside ASCII, the characters some write
        // as \\uXXXX to be safe in HTML or in a script, a slash, a quote, an apostrophe, a back quote and a backslash.
        let secret = "p\u{e4}55&w<rd/=+'`\"x\\y".to_string();
        let known = std::slice::from_ref(&secret);
        for written in [
            // Only what JSON requires.
            "p\u{e4}55&w<rd/=+'`\\\"x\\\\y",
            // & < > as \\u, lower case; then upper case with the slash escaped.
            "p\u{e4}55\\u0026w\\u003crd/=+'`\\\"x\\\\y",
            "p\\u00E455\\u0026w\\u003Crd\\/=+'`\\\"x\\\\y",
            // = and ' as \\u as well.
            "p\u{e4}55\\u0026w\\u003crd/\\u003d+\\u0027`\\\"x\\\\y",
            // The quote, the apostrophe, + and the back quote as \\u, upper case.
            "p\\u00E455\\u0026w\\u003Crd/=\\u002B\\u0027\\u0060\\u0022x\\\\y",
            // Every character as \\u.
            "\\u0070\\u00e4\\u0035\\u0035\\u0026\\u0077\\u003c\\u0072\\u0064\\u002f\\u003d\\u002b\\u0027\\u0060\\u0022\\u0078\\u005c\\u0079",
        ] {
            let line = format!(
                "{{\"ok\":false,\"error\":\"denied for {written}\",\"{written}\":[\"{written}\"]}}"
            );
            let clean = clean_output(&line, known);
            assert!(
                !clean.contains("55") && !clean.contains("0035"),
                "{written}: {clean}"
            );
            assert_eq!(clean.matches("[secret]").count(), 3, "{written}: {clean}");
            // The decoded object holds the value plainly again, as a value, a name and an element: none is kept.
            let result = result_line(&line, known).expect("a result");
            assert!(!result.to_string().contains("55"), "{result}");
            assert_eq!(result["ok"], false);
            assert_eq!(result["error"], "denied for [secret]");
        }
        // The value as it is, outside any JSON.
        assert_eq!(
            clean_output(&format!("token {secret} refused"), known),
            "token [secret] refused"
        );
        // Two spaces in a row inside a value are not folded away before it is looked for.
        let phrase = "correct  horse battery".to_string();
        let clean = clean_output(
            "said: correct  horse battery\n",
            std::slice::from_ref(&phrase),
        );
        assert_eq!(clean, "said: [secret]");
        // A short value is not looked for: it would take ordinary words with it.
        assert_eq!(clean_output("ok then", &["ok".to_string()]), "ok then");
    }

    #[test]
    fn a_run_id_is_only_what_this_module_gives() {
        assert!(run_id_ok("fr-1793361600000-1"));
        for bad in [
            "",
            "fr-1-",
            "fr--1",
            "fr-1-2-3",
            "ia-1-2",
            "fr-1-../x",
            "fr-a-1",
        ] {
            assert!(!run_id_ok(bad), "{bad}");
        }
    }

    #[test]
    fn the_meter_reads_a_cgroup_and_keeps_the_highest() {
        let dir = std::env::temp_dir().join(format!("arca-fn-meter-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch");
        let mut measured = Measured::default();
        measured.sample(&dir);
        assert_eq!(
            measured,
            Measured::default(),
            "nothing to read is nothing read"
        );
        fs::write(dir.join("memory.peak"), "4096\n").expect("peak");
        fs::write(dir.join("cpu.stat"), "usage_usec 12000\nuser_usec 1\n").expect("cpu");
        fs::write(dir.join("memory.events"), "low 0\noom 0\noom_kill 0\n").expect("events");
        measured.sample(&dir);
        fs::write(dir.join("memory.peak"), "1024\n").expect("peak");
        fs::write(dir.join("memory.events"), "low 0\noom 1\noom_kill 1\n").expect("events");
        measured.sample(&dir);
        assert_eq!(measured.memory_peak, Some(4096));
        assert_eq!(measured.cpu_ms, Some(12));
        assert_eq!(measured.oom_kills, Some(1));
        let _ = fs::remove_dir_all(&dir);
    }
}

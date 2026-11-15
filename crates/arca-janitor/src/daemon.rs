//! The job runner: one worker, a FIFO queue, JSON lines over a unix socket.
//!
//! One job runs at a time; the rest wait. A job's record is written to the state folder when it is queued, when it
//! starts, every few seconds while it runs and when it ends, so `status` always has something to show and a restart
//! loses nothing: a job that was running when the daemon stopped is marked failed, queued jobs carry on.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::jobs::{self, Ctx, Job, JobError, JobParams, State, Stop, Throttle};
use crate::platform::Priority;
use crate::store::{Store, valid_id};
use crate::util::{new_id, now};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Request {
    Ping,
    Submit {
        job: JobParams,
        budget_min: Option<u64>,
    },
    List,
    Status {
        id: String,
    },
    Cancel {
        id: String,
    },
    Shutdown,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub error: Option<String>,
    pub id: Option<String>,
    pub job: Option<Job>,
    pub jobs: Option<Vec<JobLine>>,
    pub priority: Option<Priority>,
    pub message: Option<String>,
}

impl Response {
    fn ok() -> Response {
        Response {
            ok: true,
            ..Response::default()
        }
    }
    fn err(msg: impl Into<String>) -> Response {
        Response {
            ok: false,
            error: Some(msg.into()),
            ..Response::default()
        }
    }
}

/// One line of `list`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct JobLine {
    pub id: String,
    pub kind: String,
    pub state: State,
    pub phase: String,
    pub paused: Option<String>,
    pub created: u64,
    pub finished: Option<u64>,
    pub error: Option<String>,
}

impl JobLine {
    fn of(j: &Job) -> JobLine {
        JobLine {
            id: j.id.clone(),
            kind: j.kind.clone(),
            state: j.state,
            phase: j.progress.phase.clone(),
            paused: j.progress.paused.clone(),
            created: j.created,
            finished: j.finished,
            error: j.error.clone(),
        }
    }
}

type ConfigLoader = Arc<dyn Fn() -> Result<Config, String> + Send + Sync>;

pub struct DaemonOptions {
    pub store: Store,
    pub socket: PathBuf,
    /// The priority read back at start; heavy jobs are refused unless it is lowered.
    pub priority: Priority,
    /// Read for every job, so a config edit applies to the next job without a restart.
    pub config: ConfigLoader,
    /// Replaces the config's throttle (tests point it at a fake PSI file with short waits).
    pub throttle: Option<Throttle>,
}

struct Running {
    id: String,
    job: Arc<Mutex<Job>>,
    cancel: Arc<AtomicBool>,
}

struct Inner {
    opts: DaemonOptions,
    queue: Mutex<VecDeque<String>>,
    wake: Condvar,
    running: Mutex<Option<Running>>,
    stop: AtomicBool,
}

pub struct Daemon {
    inner: Arc<Inner>,
    accept: Option<JoinHandle<()>>,
    worker: Option<JoinHandle<()>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Bind the socket, refusing when another daemon answers on it; a dead socket file is replaced.
fn bind(socket: &Path) -> io::Result<UnixListener> {
    if let Some(dir) = socket.parent() {
        fs::create_dir_all(dir)?;
    }
    if fs::symlink_metadata(socket).is_ok() {
        if UnixStream::connect(socket).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("a janitor daemon already answers on {}", socket.display()),
            ));
        }
        fs::remove_file(socket)?;
    }
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

impl Daemon {
    pub fn start(opts: DaemonOptions) -> io::Result<Daemon> {
        fs::create_dir_all(opts.store.jobs_dir())?;
        let listener = bind(&opts.socket)?;
        let inner = Arc::new(Inner {
            opts,
            queue: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            running: Mutex::new(None),
            stop: AtomicBool::new(false),
        });
        inner.recover();
        let worker = {
            let inner = inner.clone();
            thread::Builder::new()
                .name("janitor-worker".into())
                .spawn(move || inner.work())?
        };
        let accept = {
            let inner = inner.clone();
            thread::Builder::new()
                .name("janitor-accept".into())
                .spawn(move || {
                    for stream in listener.incoming() {
                        if inner.stop.load(Ordering::Relaxed) {
                            break;
                        }
                        if let Ok(stream) = stream {
                            let inner = inner.clone();
                            let _ = thread::Builder::new()
                                .name("janitor-conn".into())
                                .spawn(move || inner.serve_conn(stream));
                        }
                    }
                })?
        };
        Ok(Daemon {
            inner,
            accept: Some(accept),
            worker: Some(worker),
        })
    }

    /// Ask the daemon to stop: the running job is cancelled, queued jobs stay queued for the next start.
    pub fn stop(mut self) {
        self.inner.shutdown();
        self.join();
    }

    /// Block until a Shutdown request (or a stop) ends the daemon.
    pub fn wait(mut self) {
        self.join();
    }

    fn join(&mut self) {
        if let Some(a) = self.accept.take() {
            let _ = a.join();
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        let _ = fs::remove_file(&self.inner.opts.socket);
    }
}

impl Inner {
    fn store(&self) -> &Store {
        &self.opts.store
    }

    fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(r) = lock(&self.running).as_ref() {
            r.cancel.store(true, Ordering::Relaxed);
        }
        self.wake.notify_all();
        // Wake the accept loop.
        let _ = UnixStream::connect(&self.opts.socket);
    }

    /// After a restart: a job left running failed; queued jobs carry on, oldest first.
    fn recover(&self) {
        let mut jobs: Vec<Job> = self.store().list_jobs();
        jobs.sort_by_key(|j| (j.created, j.id.clone()));
        let mut queue = lock(&self.queue);
        for mut job in jobs {
            match job.state {
                State::Running => {
                    job.state = State::Failed;
                    job.error = Some("the daemon stopped while this job ran".to_string());
                    job.finished = Some(now());
                    let _ = self.store().save_job(&job.id, &job);
                }
                State::Queued => queue.push_back(job.id),
                _ => {}
            }
        }
    }

    fn serve_conn(&self, stream: UnixStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        let mut reader = BufReader::new(match stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        });
        let mut writer = stream;
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => self.handle(req),
            Err(e) => Response::err(format!("bad request: {e}")),
        };
        let mut out =
            serde_json::to_string(&response).unwrap_or_else(|_| "{\"ok\":false}".to_string());
        out.push('\n');
        let _ = writer.write_all(out.as_bytes());
    }

    pub fn handle(&self, req: Request) -> Response {
        match req {
            Request::Ping => Response {
                priority: Some(self.opts.priority.clone()),
                message: Some(format!("arca-janitor {}", env!("CARGO_PKG_VERSION"))),
                ..Response::ok()
            },
            Request::Submit { job, budget_min } => self.submit(job, budget_min),
            Request::List => {
                let mut jobs: Vec<Job> = self.store().list_jobs();
                if let Some(r) = lock(&self.running).as_ref() {
                    let live = lock(&r.job).clone();
                    jobs.retain(|j| j.id != live.id);
                    jobs.push(live);
                }
                jobs.sort_by_key(|j| std::cmp::Reverse((j.created, j.id.clone())));
                jobs.truncate(50);
                Response {
                    jobs: Some(jobs.iter().map(JobLine::of).collect()),
                    ..Response::ok()
                }
            }
            Request::Status { id } => match self.job(&id) {
                Some(job) => Response {
                    job: Some(job),
                    ..Response::ok()
                },
                None => Response::err(format!("no job {id}")),
            },
            Request::Cancel { id } => self.cancel(&id),
            Request::Shutdown => {
                self.shutdown();
                Response {
                    message: Some("stopping".into()),
                    ..Response::ok()
                }
            }
        }
    }

    fn job(&self, id: &str) -> Option<Job> {
        if !valid_id(id) {
            return None;
        }
        if let Some(r) = lock(&self.running).as_ref()
            && r.id == id
        {
            return Some(lock(&r.job).clone());
        }
        self.store().load_job(id).ok()
    }

    fn submit(&self, params: JobParams, budget_min: Option<u64>) -> Response {
        if self.stop.load(Ordering::Relaxed) {
            return Response::err("the daemon is stopping");
        }
        let p = &self.opts.priority;
        if params.heavy() && !p.lowered {
            return Response::err(format!(
                "refused: {} is heavy and this daemon is not at nice 19 with idle IO (read back: nice {}, io {}). \
                 Start it through the systemd unit (Nice=19, IOSchedulingClass=idle) or with `arca-janitor serve`, \
                 which lowers itself with ionice and nice (taskpolicy -b on macOS).",
                params.kind(),
                p.nice.map_or("unknown".to_string(), |n| n.to_string()),
                p.io.clone().unwrap_or_else(|| "unknown".to_string()),
            ));
        }
        let config = match (self.opts.config)() {
            Ok(c) => c,
            Err(e) => return Response::err(format!("config: {e}")),
        };
        let budget = budget_min.unwrap_or(config.job_budget_min).max(1) * 60;
        let job = Job::new(new_id("j"), params, budget);
        if let Err(e) = self.store().save_job(&job.id, &job) {
            return Response::err(format!("cannot store the job: {e}"));
        }
        let id = job.id.clone();
        lock(&self.queue).push_back(id.clone());
        self.wake.notify_all();
        Response {
            id: Some(id),
            ..Response::ok()
        }
    }

    fn cancel(&self, id: &str) -> Response {
        if let Some(r) = lock(&self.running).as_ref()
            && r.id == id
        {
            r.cancel.store(true, Ordering::Relaxed);
            return Response {
                message: Some("cancelling: the job stops at its next checkpoint".into()),
                ..Response::ok()
            };
        }
        let mut queue = lock(&self.queue);
        if let Some(pos) = queue.iter().position(|q| q == id) {
            queue.remove(pos);
            drop(queue);
            if let Ok(mut job) = self.store().load_job::<Job>(id) {
                job.state = State::Cancelled;
                job.finished = Some(now());
                let _ = self.store().save_job(id, &job);
            }
            return Response {
                message: Some("cancelled before it started".into()),
                ..Response::ok()
            };
        }
        drop(queue);
        match self.job(id) {
            Some(job) => Response::err(format!("job {id} already ended ({})", job.state.as_str())),
            None => Response::err(format!("no job {id}")),
        }
    }

    fn work(&self) {
        loop {
            let id = {
                let mut queue = lock(&self.queue);
                loop {
                    if self.stop.load(Ordering::Relaxed) {
                        return;
                    }
                    if let Some(id) = queue.pop_front() {
                        break id;
                    }
                    queue = self.wake.wait(queue).unwrap_or_else(|e| e.into_inner());
                }
            };
            let Ok(mut job) = self.store().load_job::<Job>(&id) else {
                continue;
            };
            if job.state != State::Queued {
                continue;
            }
            job.state = State::Running;
            job.started = Some(now());
            job.priority = Some(self.opts.priority.clone());
            let _ = self.store().save_job(&id, &job);
            let shared = Arc::new(Mutex::new(job));
            let cancel = Arc::new(AtomicBool::new(false));
            *lock(&self.running) = Some(Running {
                id: id.clone(),
                job: shared.clone(),
                cancel: cancel.clone(),
            });
            self.run_one(shared.clone(), cancel.clone());
            let finished = lock(&shared).clone();
            let _ = self.store().save_job(&id, &finished);
            *lock(&self.running) = None;
        }
    }

    fn run_one(&self, shared: Arc<Mutex<Job>>, cancel: Arc<AtomicBool>) {
        let config = (self.opts.config)();
        let store = self.store().clone();
        let throttle = self.opts.throttle.clone();
        let job_for_thread = shared.clone();
        let cancel_for_thread = cancel.clone();
        // Deep trees recurse; give the job a roomy stack.
        let handle = thread::Builder::new()
            .name("janitor-job".into())
            .stack_size(64 * 1024 * 1024)
            .spawn(move || {
                let config = config.map_err(JobError::Failed)?;
                let params = lock(&job_for_thread).params.clone();
                let mut ctx = Ctx::new(config, store, job_for_thread, cancel_for_thread);
                if let Some(t) = throttle {
                    ctx.throttle = t;
                }
                jobs::run(&params, &ctx)
            });
        let outcome = match handle {
            Ok(h) => h
                .join()
                .unwrap_or_else(|_| Err(JobError::Failed("the job panicked".to_string()))),
            Err(e) => Err(JobError::Failed(format!(
                "cannot start the job thread: {e}"
            ))),
        };
        let mut job = lock(&shared);
        job.finished = Some(now());
        job.progress.paused = None;
        match outcome {
            Ok(outcome) => {
                job.state = if cancel.load(Ordering::Relaxed) {
                    State::Cancelled
                } else if let Some(why) = outcome.failure {
                    // Partly done is failed: the result stays, with every item's outcome.
                    job.error = Some(why);
                    State::Failed
                } else {
                    State::Done
                };
                job.result = Some(outcome.value);
            }
            Err(JobError::Stopped(Stop::Cancelled)) => job.state = State::Cancelled,
            Err(JobError::Stopped(Stop::Budget)) => {
                job.state = State::Failed;
                job.error = Some(format!(
                    "its time budget of {} minutes ran out",
                    job.budget_secs / 60
                ));
            }
            Err(JobError::Failed(e)) => {
                job.state = State::Failed;
                job.error = Some(e);
            }
        }
    }
}

/// Send one request and read one response.
pub fn request(socket: &Path, req: &Request) -> Result<Response, String> {
    let mut stream = UnixStream::connect(socket).map_err(|e| {
        format!(
            "no janitor daemon on {} ({e}); start one with `arca-janitor serve`",
            socket.display()
        )
    })?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let mut line = serde_json::to_string(req).map_err(|e| e.to_string())?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut answer = String::new();
    reader.read_line(&mut answer).map_err(|e| e.to_string())?;
    serde_json::from_str(&answer).map_err(|e| format!("bad answer from the daemon: {e}"))
}

//! One function-shaped container against a REAL Incus. Ignored, and a no-op unless `ARCA_REAL_INCUS_PROOF=1`: it is
//! only ever run by hand, in a scratch project made for it, and it removes the container it makes. It answers what the
//! fake cannot: how long a start and a stop take, whether Incus takes a read-only root for an OCI container, whether the
//! console log is empty after a clear and holds a run's output after it, and what Incus says of a program's exit.
//!
//! - `ARCA_PROOF_SOCKET`: the Incus socket (default `/var/lib/incus/unix.socket`).
//! - `ARCA_PROOF_PROJECT`: the scratch project. Its name must begin with `arcak-scratch-`.
//! - `ARCA_PROOF_POOL`: the storage pool the container's root disk is made in.
//! - `ARCA_PROOF_RUNS`: how many runs of each kind are timed (default 20).
//! - `ARCA_PROOF_GAP_MS`: how long to wait between the end of one run and the next (default 0, back to back). Incus
//!   is still putting a container away for some hundreds of milliseconds after its program exits: back to back, a
//!   start can be refused and a cleared log can come back with the older run's output.
//!
//! The container has no NIC, so the times leave out what a NIC and its rules add to a start.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{FUNCTION_KEY, FunctionRun};
use crate::lifecycle::checked_envelope;
use crate::read::{IncusError, exchange};
use crate::write::ProjectFamily;

/// The public nats image Arca's own `nats` template pins. It is an Alpine image, so it has a shell for the probes.
const IMAGE: &str =
    "library/nats@sha256:b83efabe3e7def1e0a4a31ec6e078999bb17c80363f881df35edc70fcb6bb927";
const NAME: &str = "fnbench";
/// No call of the proof may wait longer than this.
const CALL: Duration = Duration::from_secs(60);

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn send(
    socket: &Path,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, IncusError> {
    let body = body.map(|body| serde_json::to_vec(body).expect("the body serializes"));
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: incus\r\nAccept: application/json\r\nConnection: close\r\n"
    );
    if let Some(body) = &body {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("\r\n");
    let mut request = head.into_bytes();
    if let Some(body) = body {
        request.extend_from_slice(&body);
    }
    checked_envelope(exchange(socket, CALL, &request)?)
}

/// A write, tried again for a few seconds while Incus answers that the instance is busy with an operation of its own.
fn write(socket: &Path, method: &str, path: &str, body: Option<&Value>) -> Result<(), String> {
    let mut last = Ok(());
    for _ in 0..20 {
        last = write_once(socket, method, path, body);
        match &last {
            Err(why) if why.contains("busy") => std::thread::sleep(Duration::from_millis(500)),
            _ => break,
        }
    }
    last
}

/// Sends a write and waits for its operation, at most [`CALL`] in one wait.
fn write_once(socket: &Path, method: &str, path: &str, body: Option<&Value>) -> Result<(), String> {
    let envelope = send(socket, method, path, body).map_err(|error| error.to_string())?;
    let Some(operation) = envelope["operation"]
        .as_str()
        .filter(|text| !text.is_empty())
    else {
        return Ok(());
    };
    let id = operation
        .trim_start_matches("/1.0/operations/")
        .split('?')
        .next()
        .unwrap_or_default()
        .to_string();
    let waited = send(
        socket,
        "GET",
        &format!("/1.0/operations/{id}/wait?timeout=50"),
        None,
    )
    .map_err(|error| error.to_string())?;
    let operation = &waited["metadata"];
    match operation["status_code"].as_u64() {
        Some(200) => Ok(()),
        Some(code) if code < 200 => panic!(
            "PROOF STOPPED: the operation of {method} {path} was still running after 50 seconds ({}); nothing more was sent",
            operation["status"].as_str().unwrap_or("?")
        ),
        _ => Err(operation["err"]
            .as_str()
            .unwrap_or("the operation failed")
            .to_string()),
    }
}

/// Deletes the proof's container. Just after a start that failed Incus is still ending it, so the delete is tried again
/// for a few seconds.
fn delete(socket: &Path, instance: &str) -> Result<(), String> {
    let mut last = Ok(());
    for _ in 0..20 {
        last = write(socket, "DELETE", instance, None);
        match &last {
            Err(why) if why.contains("busy") => std::thread::sleep(Duration::from_millis(500)),
            _ => break,
        }
    }
    last
}

/// Prints the end of Incus's own log of the container's last start.
fn print_lxc_log(socket: &Path, project: &str) {
    let request = format!(
        "GET /1.0/instances/{NAME}/logs/lxc.log?project={project} HTTP/1.1\r\nHost: incus\r\nConnection: close\r\n\r\n"
    );
    if let Ok(framed) = exchange(socket, CALL, request.as_bytes()) {
        let log = String::from_utf8_lossy(&framed.body).into_owned();
        let lines: Vec<&str> = log.lines().collect();
        for line in &lines[lines.len().saturating_sub(12)..] {
            println!("PROOF lxc.log: {line}");
        }
    }
}

fn median_and_worst(times: &[Duration]) -> String {
    let mut sorted: Vec<u128> = times.iter().map(Duration::as_millis).collect();
    sorted.sort_unstable();
    if sorted.is_empty() {
        return "no run".to_string();
    }
    format!(
        "median {} ms, worst {} ms, best {} ms (n={})",
        sorted[sorted.len() / 2],
        sorted[sorted.len() - 1],
        sorted[0],
        sorted.len()
    )
}

#[test]
#[ignore = "touches a real Incus; run by hand with ARCA_REAL_INCUS_PROOF=1"]
fn a_real_incus_runs_a_function_shaped_container() {
    if env("ARCA_REAL_INCUS_PROOF").as_deref() != Some("1") {
        eprintln!("ARCA_REAL_INCUS_PROOF is not 1; nothing was done");
        return;
    }
    let socket = env("ARCA_PROOF_SOCKET").unwrap_or_else(|| "/var/lib/incus/unix.socket".into());
    let socket = Path::new(&socket);
    let project = env("ARCA_PROOF_PROJECT").expect("ARCA_PROOF_PROJECT");
    let pool = env("ARCA_PROOF_POOL").expect("ARCA_PROOF_POOL");
    let runs: usize = env("ARCA_PROOF_RUNS")
        .and_then(|text| text.parse().ok())
        .unwrap_or(20);
    let gap = Duration::from_millis(
        env("ARCA_PROOF_GAP_MS")
            .and_then(|text| text.parse().ok())
            .unwrap_or(0),
    );
    println!("PROOF gap between runs: {} ms", gap.as_millis());
    assert!(
        project.starts_with("arcak-scratch-"),
        "the project must be a scratch project"
    );
    let instance = format!("/1.0/instances/{NAME}?project={project}");
    let entrypoint = |script: &str| format!("/bin/sh -c '{script}'");
    let body = |readonly: bool| {
        let mut root = json!({ "type": "disk", "path": "/", "pool": pool });
        if readonly {
            root["readonly"] = json!("true");
        }
        json!({
            "name": NAME,
            "type": "container",
            "source": { "type": "image", "mode": "pull", "protocol": "oci",
                        "server": "https://docker.io", "alias": IMAGE },
            "config": {
                FUNCTION_KEY: "true",
                "limits.cpu": "1",
                "limits.memory": "128MiB",
                "limits.memory.swap": "false",
                "oci.entrypoint": entrypoint("touch /probe && echo WROTE || echo READONLY; echo {\\\"ok\\\":true}"),
            },
            "devices": { "root": root },
            "start": false,
        })
    };
    let creates = format!("/1.0/instances?project={project}");
    // A container an earlier, broken run of the proof left is removed first.
    if send(socket, "GET", &instance, None).is_ok() {
        delete(socket, &instance).expect("the container of an earlier run is deleted");
    }
    let mut readonly = true;
    let created = Instant::now();
    if let Err(why) = write(socket, "POST", &creates, Some(&body(true))) {
        println!("PROOF read-only root at create: REFUSED: {why}");
        readonly = false;
        let _ = delete(socket, &instance);
        write(socket, "POST", &creates, Some(&body(false))).expect("the container is created");
    }
    println!(
        "PROOF create (with the image pull): {} ms, read-only root asked: {readonly}",
        created.elapsed().as_millis()
    );
    let family =
        ProjectFamily::new([project.as_str()], std::iter::empty::<&str>()).expect("family");
    let run = FunctionRun::new(socket.to_path_buf(), family);
    // 1. The read-only root: the program tries to write it. A start Incus refuses is shown with the end of Incus's own
    // log of it, and the container is made again with a root it can write, so the rest of the proof runs.
    if readonly {
        let seen = run
            .read(&project, NAME)
            .expect("the container is a function's");
        let _ = run.clear_output(&seen);
        match run.start(&seen) {
            Ok(()) => {
                let from = Instant::now();
                while run.status(&seen).expect("status") != "Stopped"
                    && from.elapsed() < Duration::from_secs(30)
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                let output = run.output(&seen).expect("the output");
                println!("PROOF read-only root at start: started; the program printed: {output:?}");
            }
            Err(error) => {
                println!("PROOF read-only root at start: REFUSED: {error}");
                print_lxc_log(socket, &project);
                readonly = false;
                // Incus is still ending the start that failed: the instance reads as running for a moment.
                let from = Instant::now();
                while from.elapsed() < Duration::from_secs(20) {
                    let _ = run.stop(&seen, Duration::ZERO);
                    if run.status(&seen).is_ok_and(|status| status == "Stopped") {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                println!(
                    "PROOF after the refused start the instance read as stopped after {} ms",
                    from.elapsed().as_millis()
                );
                delete(socket, &instance).expect("the read-only container is deleted");
                write(socket, "POST", &creates, Some(&body(false)))
                    .expect("the container is created again");
            }
        }
    }
    println!("PROOF the runs below use a read-only root: {readonly}");
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let seen = run
            .read(&project, NAME)
            .expect("the container is a function's");
        let set_entrypoint = |script: &str| {
            write(
                socket,
                "PATCH",
                &instance,
                Some(&json!({ "config": { "oci.entrypoint": entrypoint(script) } })),
            )
            .expect("the entry point is set");
        };
        // A start, tried again when Incus refuses it: how often that happens is part of what the proof shows.
        let refused_starts = std::cell::Cell::new(0u32);
        let start = || {
            for attempt in 0..6 {
                match run.start(&seen) {
                    Ok(()) => return,
                    Err(error) => {
                        refused_starts.set(refused_starts.get() + 1);
                        println!("PROOF a start was refused (try {attempt}): {error}");
                        if refused_starts.get() == 1 {
                            print_lxc_log(socket, &project);
                        }
                        let _ = run.stop(&seen, Duration::ZERO);
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            }
            panic!("the container did not start in six tries");
        };
        let wait_stopped = |limit: Duration| {
            let from = Instant::now();
            loop {
                if run.status(&seen).expect("status") == "Stopped" {
                    return Some(from.elapsed());
                }
                if from.elapsed() > limit {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        };

        // 2. What Incus says of an exit: a program that exits 3.
        set_entrypoint("echo bye; exit 3");
        run.clear_output(&seen).expect("the log is cleared");
        println!(
            "PROOF console log after a clear: {:?}",
            run.output(&seen).expect("the output after a clear")
        );
        start();
        wait_stopped(Duration::from_secs(30));
        let state = send(
            socket,
            "GET",
            &format!("/1.0/instances/{NAME}/state?project={project}"),
            None,
        )
        .expect("the state");
        let mut state = state["metadata"].clone();
        if let Some(object) = state.as_object_mut() {
            object.remove("network");
            object.remove("disk");
        }
        println!("PROOF state after exit 3: {state}");
        println!(
            "PROOF output after exit 3: {:?}",
            run.output(&seen).expect("the output")
        );

        // 3. Runs that exit by themselves.
        set_entrypoint("echo working; echo {\\\"ok\\\":true}");
        let (mut starts, mut whole, mut exits) = (Vec::new(), Vec::new(), Vec::new());
        let mut wrong = 0;
        let mut early_errors = 0;
        let mut settle = Vec::new();
        for index in 0..runs {
            std::thread::sleep(gap);
            let from = Instant::now();
            run.clear_output(&seen).expect("the log is cleared");
            let empty = run.output(&seen).expect("the output").trim().is_empty();
            let begun = Instant::now();
            start();
            starts.push(begun.elapsed());
            exits.push(wait_stopped(Duration::from_secs(30)).expect("it exits"));
            let early = run.output(&seen);
            let reading = Instant::now();
            let output = run
                .output_after_exit(&seen, Duration::from_secs(5))
                .expect("the output");
            if early.is_err() {
                early_errors += 1;
                settle.push(reading.elapsed());
            }
            whole.push(from.elapsed());
            let lines: Vec<&str> = output
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            if !empty || lines != ["working", "{\"ok\":true}"] {
                wrong += 1;
                println!("PROOF run {index}: empty after clear: {empty}; output: {output:?}");
            }
        }
        println!(
            "PROOF self-exit runs, start call: {}",
            median_and_worst(&starts)
        );
        println!(
            "PROOF self-exit runs, start done to seen stopped: {}",
            median_and_worst(&exits)
        );
        println!(
            "PROOF self-exit runs, whole run (clear, start, exit, read): {}",
            median_and_worst(&whole)
        );
        println!("PROOF self-exit runs with a wrong or stale output: {wrong} of {runs}");
        println!(
            "PROOF starts Incus refused so far: {}",
            refused_starts.get()
        );
        println!(
            "PROOF a read of the output at the instant of the exit was an error in {early_errors} of {runs} runs; it was answered after: {}",
            median_and_worst(&settle)
        );

        // 4. Runs that never exit, stopped by force.
        set_entrypoint("echo started; sleep 600");
        let (mut starts, mut stops, mut whole) = (Vec::new(), Vec::new(), Vec::new());
        let cgroup = Path::new("/sys/fs/cgroup").join(format!("lxc.payload.{project}_{NAME}"));
        let mut peak = None;
        for _ in 0..runs {
            std::thread::sleep(gap);
            let from = Instant::now();
            run.clear_output(&seen).expect("the log is cleared");
            let begun = Instant::now();
            start();
            starts.push(begun.elapsed());
            std::thread::sleep(Duration::from_millis(200));
            peak = std::fs::read_to_string(cgroup.join("memory.peak"))
                .ok()
                .or(peak);
            let stop = Instant::now();
            let forced = run.stop(&seen, Duration::ZERO).expect("it stops");
            assert!(forced);
            stops.push(stop.elapsed());
            let _ = run
                .output_after_exit(&seen, Duration::from_secs(5))
                .expect("the output");
            whole.push(from.elapsed());
        }
        println!(
            "PROOF forced runs, start call: {}",
            median_and_worst(&starts)
        );
        println!(
            "PROOF forced runs, forced stop call: {}",
            median_and_worst(&stops)
        );
        println!(
            "PROOF forced runs, whole run less the 200 ms it ran: {}",
            median_and_worst(
                &whole
                    .iter()
                    .map(|d| d.saturating_sub(Duration::from_millis(200)))
                    .collect::<Vec<_>>()
            )
        );
        println!(
            "PROOF starts Incus refused in all: {}",
            refused_starts.get()
        );
        println!(
            "PROOF cgroup memory.peak of a running one ({}): {:?}",
            cgroup.display(),
            peak.map(|p| p.trim().to_string())
        );

        // 6. A root put back to what it was before each run: a snapshot of the new container, restored.
        write(
            socket,
            "POST",
            &format!("/1.0/instances/{NAME}/snapshots?project={project}"),
            Some(&json!({ "name": "clean", "stateful": false })),
        )
        .expect("the snapshot is taken");
        let mut restores = Vec::new();
        for _ in 0..runs.min(10) {
            let from = Instant::now();
            write(
                socket,
                "PUT",
                &instance,
                Some(&json!({ "restore": "clean" })),
            )
            .expect("it is restored");
            restores.push(from.elapsed());
        }
        println!(
            "PROOF restore of a snapshot before a run: {}",
            median_and_worst(&restores)
        );
        let seen = run
            .read(&project, NAME)
            .expect("still a function's after a restore");
        run.clear_output(&seen).expect("the log is cleared");
        run.start(&seen).expect("it starts after a restore");
        let _ = run.stop(&seen, Duration::ZERO).expect("it stops");

        // 5. A stop the program has a second to obey: a shell as PID 1 ignores it, so it is forced after the grace.
        run.clear_output(&seen).expect("the log is cleared");
        run.start(&seen).expect("it starts");
        let stop = Instant::now();
        let forced = run.stop(&seen, Duration::from_secs(1)).expect("it stops");
        println!(
            "PROOF stop with one second of grace: forced: {forced}, took {} ms",
            stop.elapsed().as_millis()
        );
    }));
    let _ = run
        .read(&project, NAME)
        .and_then(|seen| run.stop(&seen, Duration::ZERO));
    match delete(socket, &instance) {
        Ok(()) => println!("PROOF the container was deleted"),
        Err(why) => println!("PROOF the container was NOT deleted: {why}"),
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// The second proof: how a run can be given the image's own root again. A snapshot restore, and whether it also puts
/// back the instance's config; Incus's rebuild of the root from the image, how long it takes, what it keeps, and what
/// it does when the image is no longer in the project's store. Same switch, same scratch project, same clean-up.
#[test]
#[ignore = "touches a real Incus; run by hand with ARCA_REAL_INCUS_PROOF=1"]
fn a_real_incus_gives_a_container_its_images_root_again() {
    if env("ARCA_REAL_INCUS_PROOF").as_deref() != Some("1") {
        eprintln!("ARCA_REAL_INCUS_PROOF is not 1; nothing was done");
        return;
    }
    let socket = env("ARCA_PROOF_SOCKET").unwrap_or_else(|| "/var/lib/incus/unix.socket".into());
    let socket = Path::new(&socket);
    let project = env("ARCA_PROOF_PROJECT").expect("ARCA_PROOF_PROJECT");
    let pool = env("ARCA_PROOF_POOL").expect("ARCA_PROOF_POOL");
    assert!(
        project.starts_with("arcak-scratch-"),
        "the project must be a scratch project"
    );
    let instance = format!("/1.0/instances/{NAME}?project={project}");
    let source = json!({ "type": "image", "mode": "pull", "protocol": "oci",
                         "server": "https://docker.io", "alias": IMAGE });
    if send(socket, "GET", &instance, None).is_ok() {
        delete(socket, &instance).expect("the container of an earlier run is deleted");
    }
    // The program says whether an earlier run left its file, and leaves one.
    let script = "ls /probe >/dev/null 2>&1 && echo FOUND || echo CLEAN; touch /probe; echo {\\\"ok\\\":true}";
    write(
        socket,
        "POST",
        &format!("/1.0/instances?project={project}"),
        Some(&json!({
            "name": NAME,
            "type": "container",
            "source": source,
            "config": {
                FUNCTION_KEY: "true",
                "limits.cpu": "1",
                "limits.memory": "128MiB",
                "oci.entrypoint": format!("/bin/sh -c '{script}'"),
            },
            "devices": { "root": { "type": "disk", "path": "/", "pool": pool } },
            "start": false,
        })),
    )
    .expect("the container is created");
    let family =
        ProjectFamily::new([project.as_str()], std::iter::empty::<&str>()).expect("family");
    let run = FunctionRun::new(socket.to_path_buf(), family);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // One run, patiently: what the program printed.
        let once = || -> String {
            std::thread::sleep(Duration::from_secs(2));
            let seen = run.read(&project, NAME).expect("a function's");
            run.clear_output(&seen).expect("the log is cleared");
            run.start(&seen).expect("it starts");
            let from = Instant::now();
            while run.status(&seen).expect("status") != "Stopped" {
                assert!(from.elapsed() < Duration::from_secs(30), "it never exited");
                std::thread::sleep(Duration::from_millis(10));
            }
            run.output_after_exit(&seen, Duration::from_secs(5))
                .expect("the output")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        };
        let config = |key: &str| -> Value {
            send(socket, "GET", &instance, None).expect("the instance")["metadata"]["config"][key]
                .clone()
        };
        println!("PROOF first run on a new root: {}", once());
        println!("PROOF second run, nothing done between: {}", once());

        // 1. A snapshot restore: the files, and the config?
        write(
            socket,
            "POST",
            &format!("/1.0/instances/{NAME}/snapshots?project={project}"),
            Some(&json!({ "name": "clean", "stateful": false })),
        )
        .expect("the snapshot is taken");
        write(
            socket,
            "PATCH",
            &instance,
            Some(&json!({ "config": { "user.note": "set-after-the-snapshot" } })),
        )
        .expect("a key is set after the snapshot");
        println!(
            "PROOF config key before the restore: {}",
            config("user.note")
        );
        write(
            socket,
            "PUT",
            &instance,
            Some(&json!({ "restore": "clean" })),
        )
        .expect("it is restored");
        println!(
            "PROOF config key after the restore: {} (null: a restore puts the config back too)",
            config("user.note")
        );
        write(
            socket,
            "DELETE",
            &format!("/1.0/instances/{NAME}/snapshots/clean?project={project}"),
            None,
        )
        .expect("the snapshot is deleted");

        // 2. A rebuild from the image: what it takes, what it keeps.
        write(
            socket,
            "PATCH",
            &instance,
            Some(&json!({ "config": { "user.note": "kept-by-a-rebuild" } })),
        )
        .expect("a key is set");
        println!("PROOF run before the rebuild: {}", once());
        println!(
            "PROOF before a rebuild: volatile.uuid = {}, volatile.uuid.generation = {}",
            config("volatile.uuid"),
            config("volatile.uuid.generation")
        );
        let rebuilds = format!("/1.0/instances/{NAME}/rebuild?project={project}");
        let mut times = Vec::new();
        for index in 0..10 {
            let from = Instant::now();
            match write(
                socket,
                "POST",
                &rebuilds,
                Some(&json!({ "source": source })),
            ) {
                Ok(()) => times.push(from.elapsed()),
                Err(why) => {
                    println!("PROOF rebuild {index} REFUSED: {why}");
                    break;
                }
            }
            if index == 0 {
                println!("PROOF run after the first rebuild: {}", once());
                println!(
                    "PROOF after a rebuild: user.note = {}, function mark = {}, entry point kept = {}",
                    config("user.note"),
                    config(FUNCTION_KEY),
                    config("oci.entrypoint")
                        .as_str()
                        .is_some_and(|text| text.contains("/probe"))
                );
            }
        }
        println!(
            "PROOF rebuild from the image in the store: {}",
            median_and_worst(&times)
        );
        println!("PROOF run after the last rebuild: {}", once());

        // 2b. The same rebuild from the copy the project's store already holds, named by its fingerprint: no registry
        // is asked.
        let images = send(
            socket,
            "GET",
            &format!("/1.0/images?project={project}&recursion=1"),
            None,
        )
        .expect("the images");
        let fingerprint = images["metadata"][0]["fingerprint"]
            .as_str()
            .expect("the store holds the image")
            .to_string();
        println!(
            "PROOF the store's copy: {} images, cached: {}, last used: {}",
            images["metadata"].as_array().map_or(0, Vec::len),
            images["metadata"][0]["cached"],
            images["metadata"][0]["last_used_at"]
        );
        let local = json!({ "source": { "type": "image", "fingerprint": fingerprint } });
        let mut times = Vec::new();
        for index in 0..10 {
            // Back to back, Incus answers that the instance must be stopped: it is still ending the last rebuild.
            std::thread::sleep(Duration::from_millis(500));
            let from = Instant::now();
            match write(socket, "POST", &rebuilds, Some(&local)) {
                Ok(()) => times.push(from.elapsed()),
                Err(why) => {
                    println!("PROOF local rebuild {index} REFUSED: {why}");
                    break;
                }
            }
        }
        println!(
            "PROOF rebuild from the store's own copy, by fingerprint: {}",
            median_and_worst(&times)
        );
        println!("PROOF run after a local rebuild: {}", once());
        println!(
            "PROOF after the rebuilds: volatile.uuid = {}, volatile.uuid.generation = {}",
            config("volatile.uuid"),
            config("volatile.uuid.generation")
        );
        println!(
            "PROOF the image the instance says it was made from (volatile.base_image) is the store's copy: {}",
            config("volatile.base_image") == json!(fingerprint)
        );
        let after = send(
            socket,
            "GET",
            &format!("/1.0/images/{fingerprint}?project={project}"),
            None,
        )
        .expect("the image");
        println!(
            "PROOF the store's copy after the local rebuilds: last used: {}",
            after["metadata"]["last_used_at"]
        );

        // 3. The image gone from the project's store: does a rebuild pull it again, or refuse?
        let images = send(
            socket,
            "GET",
            &format!("/1.0/images?project={project}&recursion=1"),
            None,
        )
        .expect("the images");
        for image in images["metadata"].as_array().into_iter().flatten() {
            if let Some(fingerprint) = image["fingerprint"].as_str() {
                write(
                    socket,
                    "DELETE",
                    &format!("/1.0/images/{fingerprint}?project={project}"),
                    None,
                )
                .expect("the image is deleted");
            }
        }
        let from = Instant::now();
        match write(
            socket,
            "POST",
            &rebuilds,
            Some(&json!({ "source": source })),
        ) {
            Ok(()) => println!(
                "PROOF rebuild with the image gone from the store: done in {} ms (Incus pulled it again)",
                from.elapsed().as_millis()
            ),
            Err(why) => {
                println!("PROOF rebuild with the image gone from the store: REFUSED: {why}")
            }
        }
        println!("PROOF run after that rebuild: {}", once());
        // And by fingerprint, with the copy gone again: this one cannot pull.
        let images = send(
            socket,
            "GET",
            &format!("/1.0/images?project={project}&recursion=1"),
            None,
        )
        .expect("the images");
        for image in images["metadata"].as_array().into_iter().flatten() {
            if let Some(found) = image["fingerprint"].as_str() {
                write(
                    socket,
                    "DELETE",
                    &format!("/1.0/images/{found}?project={project}"),
                    None,
                )
                .expect("the image is deleted");
            }
        }
        match write(socket, "POST", &rebuilds, Some(&local)) {
            Ok(()) => println!("PROOF local rebuild with the copy gone: done"),
            Err(why) => println!("PROOF local rebuild with the copy gone: REFUSED: {why}"),
        }
    }));
    let _ = run
        .read(&project, NAME)
        .and_then(|seen| run.stop(&seen, Duration::ZERO));
    match delete(socket, &instance) {
        Ok(()) => println!("PROOF the container was deleted"),
        Err(why) => println!("PROOF the container was NOT deleted: {why}"),
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// What a rebuild does to a container that has never been started, against a REAL Incus with the public image:
/// (1) create, rebuild from the store's copy, start; (2) create, start, stop, rebuild, start; (3) three runs in the
/// order Arca keeps, through [`FunctionRun::fresh_root`]. Same switch and scratch project; no registry credential is
/// needed. The id-map keys are printed at each step: a rebuild removes
/// `volatile.idmap.next`, and a container that never started has no `volatile.idmap.current` to start from.
#[test]
#[ignore = "touches a real Incus; run by hand with ARCA_REAL_INCUS_PROOF=1"]
fn a_real_incus_cannot_start_a_container_that_was_rebuilt_before_its_first_start() {
    if env("ARCA_REAL_INCUS_PROOF").as_deref() != Some("1") {
        eprintln!("ARCA_REAL_INCUS_PROOF is not 1; nothing was done");
        return;
    }
    let socket = env("ARCA_PROOF_SOCKET").unwrap_or_else(|| "/var/lib/incus/unix.socket".into());
    let socket = Path::new(&socket);
    let project = env("ARCA_PROOF_PROJECT").expect("ARCA_PROOF_PROJECT");
    let pool = env("ARCA_PROOF_POOL").expect("ARCA_PROOF_POOL");
    assert!(
        project.starts_with("arcak-scratch-"),
        "the project must be a scratch project"
    );
    let instance = format!("/1.0/instances/{NAME}?project={project}");
    let source = json!({ "type": "image", "mode": "pull", "protocol": "oci",
                         "server": "https://docker.io", "alias": IMAGE });
    let family =
        ProjectFamily::new([project.as_str()], std::iter::empty::<&str>()).expect("family");
    let run = FunctionRun::new(socket.to_path_buf(), family);
    let create = || {
        if send(socket, "GET", &instance, None).is_ok() {
            delete(socket, &instance).expect("the container of an earlier step is deleted");
        }
        write(
            socket,
            "POST",
            &format!("/1.0/instances?project={project}"),
            Some(&json!({
                "name": NAME,
                "type": "container",
                "source": source,
                "config": { FUNCTION_KEY: "true", "limits.cpu": "1", "limits.memory": "128MiB" },
                "devices": { "root": { "type": "disk", "path": "/", "pool": pool } },
                "start": false,
            })),
        )
        .expect("the container is created");
    };
    let config = || -> serde_json::Map<String, Value> {
        send(socket, "GET", &instance, None).expect("the instance")["metadata"]["config"]
            .as_object()
            .cloned()
            .unwrap_or_default()
    };
    let maps = |when: &str| {
        let config = config();
        let held: Vec<String> = [
            "volatile.idmap.base",
            "volatile.idmap.next",
            "volatile.idmap.current",
            "volatile.last_state.idmap",
            "volatile.last_state.power",
        ]
        .into_iter()
        .map(|key| {
            format!(
                "{key}: {}",
                match config.get(key).and_then(Value::as_str) {
                    None => "absent".to_string(),
                    Some(text) if text.len() > 12 => "a map".to_string(),
                    Some(text) => format!("{text:?}"),
                }
            )
        })
        .collect();
        println!("PROOF {when}: {}", held.join("; "));
    };
    let rebuild = || {
        let image = config()
            .get("volatile.base_image")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let done = write(
            socket,
            "POST",
            &format!("/1.0/instances/{NAME}/rebuild?project={project}"),
            Some(&json!({ "source": { "type": "image", "fingerprint": image } })),
        );
        println!("PROOF a rebuild from the store's copy: {done:?}");
    };
    let start = |when: &str| -> bool {
        let seen = run.read(&project, NAME).expect("a function's");
        match run.start(&seen) {
            Ok(_) => {
                std::thread::sleep(Duration::from_millis(500));
                let seen = run.read(&project, NAME).expect("a function's");
                println!(
                    "PROOF a start {when}: TAKEN; half a second later it is {}",
                    run.status(&seen).expect("status")
                );
                let _ = run.stop(&seen, Duration::from_secs(1));
                let from = Instant::now();
                while run.read(&project, NAME).expect("a function's").status != "Stopped" {
                    assert!(from.elapsed() < Duration::from_secs(30), "it never stopped");
                    std::thread::sleep(Duration::from_millis(100));
                }
                // Incus is still putting the container away for a moment after it stops.
                std::thread::sleep(Duration::from_secs(2));
                true
            }
            Err(error) => {
                println!("PROOF a start {when}: REFUSED: {error}");
                false
            }
        }
    };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // 1. Never started, rebuilt, then started.
        create();
        maps("1. new, never started");
        rebuild();
        maps("1. rebuilt, never started");
        start("of a container rebuilt before its first start");
        maps("1. after that start");

        // 2. Started once, then rebuilt, then started.
        create();
        maps("2. new, never started");
        start("of a new container");
        maps("2. after its first start");
        rebuild();
        maps("2. rebuilt after a start");
        start("of a container rebuilt after its first start");
        maps("2. after that start");

        // 3. The order Arca keeps: a root is asked for before every run, and a container that never started is
        //    given none; after its first start it is rebuilt from the store's copy, and it starts again.
        create();
        let image = config()
            .get("volatile.base_image")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let never = || false;
        for run_number in 1..=3 {
            let seen = run.read(&project, NAME).expect("a function's");
            let from = Instant::now();
            let root = run.fresh_root(&seen, Some(&image), &source, &never);
            println!(
                "PROOF 3. run {run_number}: fresh_root answered {root:?} in {} ms",
                from.elapsed().as_millis()
            );
            maps(&format!("3. run {run_number}, before its start"));
            start(&format!("of run {run_number} in Arca's order"));
        }
    }));
    let deleted = delete(socket, &instance);
    println!("PROOF the container is deleted: {deleted:?}");
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

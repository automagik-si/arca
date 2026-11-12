//! The fuel limit stops a guest that never returns. The guest is `fixtures/loop.wat`, a component whose exported `run` loops
//! forever, so the fuel the store is given is the only thing that can end the call.

use std::time::{Duration, Instant};

use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store, Trap};

/// The fixture: a component whose exported `run` never returns.
const LOOP_WAT: &str = include_str!("fixtures/loop.wat");

/// The longest a trapped call may take, measured around the call alone: the engine and the compiled fixture exist before
/// the clock starts. Ten thousand units of fuel run in microseconds, so ten seconds is room for a loaded machine that keeps
/// the thread waiting, and still tells a trap that comes when the fuel is spent from one that comes much later. That the
/// limit is enforced at all is shown by the trap being `OutOfFuel`, not by this bound.
const TRAP_DEADLINE: Duration = Duration::from_secs(10);

/// How long a test waits for the thread's answer before it fails. A fuel limit that is not enforced leaves the call looping,
/// so the call runs on its own thread and the test fails here instead of hanging: that is all this limit is for. The wait
/// also covers making the engine and compiling the fixture, which took longer than five seconds on a loaded machine and
/// failed both tests with nothing wrong, so the limit is long.
const WAIT_LIMIT: Duration = Duration::from_secs(120);

/// Runs `call_in_store` on its own thread and waits at most `WAIT_LIMIT` for its answer: the engine, the compile, the call.
fn call_run_with_fuel(fuel: u64) -> (wasmtime::Error, Duration) {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(call_in_store(fuel));
    });
    receiver
        .recv_timeout(WAIT_LIMIT)
        .expect("the guest must trap before the wait limit; a call still running after it means fuel is not enforced")
}

/// Calls `run` of the fixture in a store that holds `fuel` units of fuel. Returns the error the call returned and how long
/// the call took.
fn call_in_store(fuel: u64) -> (wasmtime::Error, Duration) {
    let mut config = Config::new();
    config.consume_fuel(true);
    let engine = Engine::new(&config).expect("the engine is configured");
    let component = Component::new(&engine, LOOP_WAT).expect("the fixture compiles from its text");
    let mut store = Store::new(&engine, ());
    store.set_fuel(fuel).expect("fuel consumption is enabled");
    let instance = Linker::new(&engine)
        .instantiate(&mut store, &component)
        .expect("the fixture has no imports");
    let run = instance
        .get_typed_func::<(), ()>(&mut store, "run")
        .expect("the fixture exports `run` as a function without parameters or results");

    let started = Instant::now();
    let error = run
        .call(&mut store, ())
        .expect_err("a loop that never returns must not return");
    (error, started.elapsed())
}

#[test]
fn a_fuel_limit_traps_a_guest_that_never_returns() {
    let (error, elapsed) = call_run_with_fuel(10_000);
    assert_eq!(
        error.downcast_ref::<Trap>(),
        Some(&Trap::OutOfFuel),
        "the error: {error}"
    );
    assert!(elapsed < TRAP_DEADLINE, "the trap took {elapsed:?}");
}

#[test]
fn a_fuel_of_zero_traps_at_once() {
    let (error, elapsed) = call_run_with_fuel(0);
    assert_eq!(
        error.downcast_ref::<Trap>(),
        Some(&Trap::OutOfFuel),
        "the error: {error}"
    );
    assert!(elapsed < TRAP_DEADLINE, "the trap took {elapsed:?}");
}

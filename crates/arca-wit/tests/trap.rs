//! The fuel limit stops a guest that never returns. The guest is `fixtures/loop.wat`, a component whose exported `run` loops
//! forever, so the fuel the store is given is the only thing that can end the call.

use std::time::{Duration, Instant};

use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store, Trap};

/// The fixture: a component whose exported `run` never returns.
const LOOP_WAT: &str = include_str!("fixtures/loop.wat");

/// The longest a trapped call may take. Ten thousand units of fuel run in microseconds, so this bound catches a limit that
/// is not enforced and not a slow machine.
const TRAP_DEADLINE: Duration = Duration::from_secs(1);

/// Calls `run` of the fixture in a store that holds `fuel` units of fuel. Returns the error the call returned and how long
/// the call took.
fn call_run_with_fuel(fuel: u64) -> (wasmtime::Error, Duration) {
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

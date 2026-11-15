//! Automagik Arca's janitor: a polite, job-based cleanup runner for the trash agents leave behind.
//!
//! A daemon (`arca-janitor serve`) runs one job at a time from a queue and answers JSON over a unix socket; the CLI
//! submits a job and returns its id at once. The daemon runs at nice 19 with idle IO (it lowers itself at start), and
//! every job pauses while the machine's IO pressure is high. Job kinds: `space-report`, `growth`, `big-files`,
//! `clean-plan`, `clean-apply`, `trash-purge`, `nightly`, `insights` and `probe`; `arca-janitor schema` prints each
//! one's parameters and result.
//!
//! Cleaning is planned first and applied by plan id; every protection is checked again at apply time. See
//! [`protect`] for what is never touched and [`categories`] for what is cleaned.

pub mod categories;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod jobs;
pub mod platform;
pub mod protect;
pub mod report;
pub mod schema;
pub mod store;
#[doc(hidden)]
pub mod testutil;
pub mod util;
pub mod walk;

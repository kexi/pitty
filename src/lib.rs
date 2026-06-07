//! pitty: a PTY-based E2E testing framework for CLI apps and AI agents.
//!
//! Scenarios are written in YAML and executed against a real pseudo-terminal:
//! the framework spawns the target process inside a PTY, drives stdin (text,
//! raw bytes, named keys), and asserts on streamed output, files, and process
//! state. The hard part — waiting for output without dropping bytes and with a
//! bounded timeout — lives in [`pty::matcher`] and [`pty::reader`].
//!
//! Concurrency model: a single dedicated reader thread drains the blocking PTY
//! master into a shared buffer and notifies a `Condvar`; the public API is
//! fully synchronous ([`pty::PtySession::wait_for`]). We deliberately avoid an
//! async runtime because portable-pty's I/O is blocking.
//!
//! Trust model: pitty is single-trust (unchanged since v0.1) — you run your
//! own scenarios in your own environment. Untrusted YAML is out of scope.

pub mod assert;
pub mod bench;
mod bytes;
pub mod cli;
pub mod config;
pub mod error;
pub mod github;
pub mod matrix;
pub mod pty;
pub mod report;
pub mod runner;
pub mod workspace;

pub use config::Scenario;
pub use error::{PittyError, Result};
pub use report::{Report, Status};
pub use runner::run_scenario;

/// A process-global lock serializing tests that mutate process environment
/// variables.
///
/// Why not let each test toggle its own var freely: `std::env::set_var` mutates
/// process-global state, and Rust runs unit tests on multiple threads within one
/// test binary. Two tests touching the *same* variable (e.g.
/// `PITTY_MATRIX_MAX_CELLS` is read by both `matrix::max_cells` and the CLI's
/// oversized-product test) can interleave a set/remove between another test's set
/// and its assertion, producing a flaky failure unrelated to the code under test.
/// Holding this single mutex for the duration of any env-mutating test serializes
/// them without pulling in a `serial_test` dependency.
#[cfg(test)]
pub(crate) static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

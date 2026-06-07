//! Thin entry point: parse args, dispatch, exit with the resulting code.
//!
//! All real work lives in the library (`ptytest::cli`). `main` only converts
//! the dispatched `u8` exit code into a `std::process::ExitCode` at the
//! process boundary.

use std::process::ExitCode;

fn main() -> ExitCode {
    let code = ptytest::cli::dispatch();
    ExitCode::from(code)
}

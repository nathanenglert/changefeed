//! `cf` binary entry (ARCHITECTURE.md §1). IMPURE: clock, RNG, network, disk, env, stdio, and the
//! ONLY `std::process::exit`. Maps an `ObservationResult` (+ events) to a frozen §4.5 `ExitCode`.

// The cli (§6 step 9) assembles the impure boundary modules into the full command tree. A few
// seam helpers (the `Ctx` DI container, builder knobs) remain available for tests / Phase-2 wiring
// without being exercised by every command path.
#![allow(dead_code)]

mod cli;
mod config_io;
mod ctx;
mod fetch_http;
mod ids_clock;
mod render;
mod store_sqlite;

use std::process::ExitCode as ProcExitCode;

use clap::Parser;

fn main() -> ProcExitCode {
    // clap's default usage-error exit code is 2; the §4.5 contract maps a bad flag / malformed
    // invocation to exit 1 (Usage), reserving 2 for "target not found". We therefore parse
    // explicitly and translate a clap argument error to exit 1 (a `--help`/`--version` display is
    // exit 0 as usual).
    let args = match cli::Cli::try_parse() {
        Ok(args) => args,
        Err(e) => {
            // Print clap's formatted message (help/version to stdout, errors to stderr) ourselves.
            let _ = e.print();
            return match e.kind() {
                clap::error::ErrorKind::DisplayHelp
                | clap::error::ErrorKind::DisplayVersion => ProcExitCode::from(0),
                _ => ProcExitCode::from(cf_core::ExitCode::Usage.code()),
            };
        }
    };
    let code = cli::run(args);
    ProcExitCode::from(code.code())
}

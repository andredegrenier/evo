//! `cargo run -p xtask -- perf`: run evo's timing tests and print the numbers.
//!
//! The measurements themselves live in `src/perf.rs`, next to the code they
//! measure, because they need evo's internals. This is the wrapper that gets
//! the invocation right, and getting it right matters more than it looks:
//! debug builds are five to twenty times slower here, the tests are
//! `#[ignore]`d so an ordinary run skips them, and two of them racing each
//! other for the machine would measure the race.

use std::path::Path;
use std::process::Command;

pub const USAGE: &str = "\
usage: cargo run -p xtask -- perf [test-name-filter...]

    Runs the #[ignore]d perf_* tests in release mode, one at a time, with
    their numbers on stdout. A filter narrows it: `perf` alone runs all of
    them, `perf worker` runs the render-worker ones.

    Set EVO_PDFIUM_PATH to a libpdfium to measure both engines; without it
    only hayro is measured.
    Set EVO_PERF_LIBRARY_DOCS to shrink the ten-thousand-document library
    run, which takes a quarter of an hour at full size.";

pub fn main(args: &[String]) {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return;
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask/ has a parent")
        .to_path_buf();

    let mut command = Command::new(env!("CARGO"));
    command
        .current_dir(&root)
        .args(["test", "--release", "--"])
        .args(["--ignored", "--nocapture", "--test-threads=1"]);
    if args.is_empty() {
        command.arg("perf_");
    } else {
        command.args(args);
    }

    let status = command.status().expect("cargo test runs");
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}

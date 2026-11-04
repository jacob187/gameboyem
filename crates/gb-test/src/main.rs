//! `gb-test`: run every manifest ROM headlessly and print a pass/fail table.
//!
//! Usage: `cargo run -p gb-test --release -- [FILTER...] [--fail-on-mismatch]`
//!
//! `-- --bench [FRAMES] [REPEATS] [--check] [--min-speed N] [--print-pins]` instead runs the
//! speed benchmark (`bench.rs`).
//!
//! Exit status is nonzero with `--fail-on-mismatch` if any result differs from
//! `expected.txt` (or a ROM is missing in CI).

use std::collections::VecDeque;
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::Instant;

mod bench;

use gb_test::{Case, Outcome, in_ci, load_manifest, run_case};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let fail_on_mismatch = args.iter().any(|a| a == "--fail-on-mismatch");
    let filters: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    if args.iter().any(|a| a == "--bench") {
        return bench::run(&args);
    }
    let cases: Vec<Case> = load_manifest()
        .into_iter()
        .filter(|c| filters.is_empty() || filters.iter().any(|f| c.path.contains(f.as_str())))
        .collect();

    let started = Instant::now();
    let queue = Mutex::new(cases.iter().enumerate().collect::<VecDeque<_>>());
    let results = Mutex::new(vec![None; cases.len()]);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let Some((i, case)) = queue.lock().unwrap().pop_front() else {
                        break;
                    };
                    let t = Instant::now();
                    let outcome = run_case(case);
                    results.lock().unwrap()[i] = Some((outcome, t.elapsed()));
                }
            });
        }
    });
    let results = results.into_inner().unwrap();

    let width = cases
        .iter()
        .map(|c| c.name().len())
        .max()
        .unwrap_or(10)
        .min(60);
    let (mut pass, mut fail, mut missing, mut mismatches) = (0, 0, 0, 0);
    println!(
        "{:<width$}  {:<7}  {:>6}  {:<8}  detail",
        "test", "result", "time", "expected"
    );
    println!("{}", "-".repeat(width + 40));
    for (case, r) in cases.iter().zip(results) {
        let (outcome, elapsed) = r.expect("every case ran");
        match outcome {
            Outcome::Pass => pass += 1,
            Outcome::Missing => missing += 1,
            _ => fail += 1,
        }
        let mismatch = match outcome {
            Outcome::Missing => in_ci(),
            ref o => o.passed() != case.expect_pass,
        };
        if mismatch {
            mismatches += 1;
        }
        // Truncate on a char boundary: details come from from_utf8_lossy, so
        // a multi-byte U+FFFD can straddle the cut.
        let detail: String = outcome.detail().chars().take(120).collect();
        println!(
            "{:<width$}  {:<7}  {:>5.1}s  {:<8}  {}",
            case.name(),
            outcome.label(),
            elapsed.as_secs_f32(),
            if mismatch {
                format!("{}!", if case.expect_pass { "pass" } else { "fail" })
            } else if case.expect_pass {
                "pass".into()
            } else {
                "fail".into()
            },
            detail
        );
    }
    println!("{}", "-".repeat(width + 40));
    println!(
        "{pass} passed, {fail} failed, {missing} missing, {mismatches} differ from expected.txt ({:.1}s)",
        started.elapsed().as_secs_f32()
    );
    if fail_on_mismatch && mismatches > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

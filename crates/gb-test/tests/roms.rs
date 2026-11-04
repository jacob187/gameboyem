//! One `cargo test` case per ROM in `expected.txt`.
//!
//! A case passes when the ROM's actual outcome matches its expectation. ROMs that
//! are not present locally are reported as ignored; in CI (`CI` set) a missing
//! ROM is a failure so nothing is silently skipped.

use gb_test::{Outcome, in_ci, load_manifest, run_case};
use libtest_mimic::{Arguments, Failed, Trial};

fn main() {
    let args = Arguments::from_args();
    let ci = in_ci();
    let trials = load_manifest()
        .into_iter()
        .map(|case| {
            let missing = !case.rom_path().is_file();
            let name = case.name();
            Trial::test(name, move || {
                let outcome = run_case(&case);
                match (&outcome, case.expect_pass) {
                    (Outcome::Missing, _) => Err(Failed::from(format!(
                        "ROM not found: {} (run scripts/fetch-test-roms.sh)",
                        case.rom_path().display()
                    ))),
                    (Outcome::Pass, true) => Ok(()),
                    (Outcome::Pass, false) => Err(Failed::from(
                        "now PASSES but expected.txt says fail: flip it to `pass`",
                    )),
                    (o, true) => Err(Failed::from(format!("REGRESSION: {o}"))),
                    (_, false) => Ok(()),
                }
            })
            .with_ignored_flag(missing && !ci)
        })
        .collect();
    libtest_mimic::run(&args, trials).exit();
}

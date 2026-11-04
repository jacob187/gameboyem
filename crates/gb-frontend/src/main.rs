//! `gb` desktop frontend. Phase 1 placeholder: loads a ROM and runs it
//! headlessly for a few frames. The windowed frontend lands in Phase 5.

use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(path) = std::env::args_os().nth(1) else {
        eprintln!("usage: gb <rom.gb>");
        return ExitCode::from(2);
    };
    let rom = match std::fs::read(&path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", path.to_string_lossy());
            return ExitCode::FAILURE;
        }
    };
    let mut gb = match gb_core::GameBoy::new(rom) {
        Ok(gb) => gb,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("loaded \"{}\"", gb.cartridge_header().title);
    for _ in 0..60 {
        gb.step_frame();
    }
    ExitCode::SUCCESS
}

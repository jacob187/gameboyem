//! `gb`: desktop frontend (window, input, audio, saves) for the gb-core emulator.
//! All platform-specific code lives in this crate; `gb-core` stays pure.
#![forbid(unsafe_code)]

mod app;
mod audio;
mod cli;
mod keymap;
mod pacer;
mod save;
mod video;

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use gb_core::GameBoy;
use winit::event_loop::EventLoop;

use crate::app::App;
use crate::audio::AudioOutput;
use crate::cli::Command;
use crate::save::SaveManager;

/// Largest ROM the core accepts (8 MiB); reading stops one byte past it so a
/// bogus huge file or device node cannot exhaust memory.
const MAX_ROM_BYTES: u64 = 8 * 1024 * 1024;

fn read_rom(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut data = Vec::new();
    File::open(path)?
        .take(MAX_ROM_BYTES + 1)
        .read_to_end(&mut data)?;
    Ok(data)
}

fn main() -> ExitCode {
    let opts = match cli::parse(std::env::args_os().skip(1)) {
        Ok(Command::Run(o)) => o,
        Ok(Command::Help) => {
            println!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };

    let rom = match read_rom(&opts.rom) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", opts.rom.display());
            return ExitCode::FAILURE;
        }
    };
    let mut gb = match GameBoy::new(rom) {
        Ok(gb) => gb,
        Err(e) => {
            eprintln!("error: {}: not a usable ROM: {e}", opts.rom.display());
            return ExitCode::FAILURE;
        }
    };
    let header = gb.cartridge_header();
    if !header.header_checksum_ok {
        eprintln!("warning: ROM header checksum is wrong (running anyway)");
    }

    let audio = if opts.mute {
        None
    } else {
        match AudioOutput::start() {
            Ok(a) => {
                gb.set_audio_sample_rate(a.sample_rate);
                Some(a)
            }
            Err(e) => {
                eprintln!("warning: audio disabled: {e}");
                None
            }
        }
    };

    let mut saves = SaveManager::new(&opts.rom, Instant::now());
    saves.load(&mut gb);

    let event_loop = match EventLoop::new() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: cannot start the windowing system: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut app = App::new(
        gb,
        saves,
        audio,
        opts.palette.colors(),
        opts.scale,
        opts.test_pattern,
    );
    let run = event_loop.run_app(&mut app);
    if let Some(e) = app.error.take() {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = run {
        eprintln!("error: event loop failed: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

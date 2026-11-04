//! `gb`: desktop frontend (window, input, audio, saves) for the gb-core emulator.
//! All platform-specific code lives in this crate; `gb-core` stays pure.
#![forbid(unsafe_code)]

mod app;
mod audio;
mod cli;
mod keymap;
mod pacer;
mod rom;
mod save;
mod shutdown;
mod state;
mod video;

use std::process::ExitCode;
use std::time::Instant;

use gb_core::GameBoy;
use winit::event_loop::EventLoop;

use crate::app::{App, Storage};
use crate::audio::AudioOutput;
use crate::cli::Command;
use crate::save::SaveManager;
use crate::shutdown::ShutdownFlag;
use crate::state::StateSlots;

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

    let rom = match rom::read_rom(&opts.rom) {
        Ok(r) => r,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };
    let archive = rom::archive_kind(&rom);
    let model = opts.model.resolve(&rom);
    let mut gb = match GameBoy::with_model(rom, model) {
        Ok(gb) => gb,
        Err(e) => {
            eprintln!("error: {}", rom::cartridge_message(&opts.rom, &e, archive));
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
                // The core clamps the rate; a mismatch would mean wrong pitch.
                match audio::check_core_rate(a.sample_rate, gb.audio_sample_rate()) {
                    Ok(()) => Some(a),
                    Err(e) => {
                        eprintln!("warning: audio disabled: {e}");
                        None
                    }
                }
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
    let shutdown = ShutdownFlag::default();
    {
        let flag = shutdown.clone();
        let proxy = event_loop.create_proxy();
        let installed = ctrlc::set_handler(move || {
            flag.request();
            // Wake a paused or idle loop; an error just means it is already gone.
            let _ = proxy.send_event(());
        });
        if let Err(e) = installed {
            eprintln!("warning: cannot install the Ctrl-C/SIGTERM handler: {e}");
        }
    }
    let storage = Storage {
        saves,
        states: StateSlots::new(&opts.rom),
    };
    let mut app = App::new(
        gb,
        storage,
        audio,
        opts.palette.colors(),
        opts.scale,
        opts.test_pattern,
        shutdown,
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

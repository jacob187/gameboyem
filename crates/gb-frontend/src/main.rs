//! `gb`: desktop frontend (window, input, audio, saves) for the gb-core and gba-core
//! emulators. All platform-specific code lives in this crate; the cores stay pure.
#![forbid(unsafe_code)]

mod app;
mod audio;
mod cli;
mod keymap;
mod pacer;
mod record;
mod rom;
mod save;
mod shutdown;
mod state;
mod video;

use std::process::ExitCode;
use std::time::Instant;

use emu_common::{Core, System};
use gb_core::GameBoy;
use gba_core::Gba;
use winit::event_loop::EventLoop;

use crate::app::{App, Storage};
use crate::audio::AudioOutput;
use crate::cli::Command;
use crate::save::SaveManager;
use crate::shutdown::ShutdownFlag;
use crate::state::StateSlots;

/// A Game Boy core for `rom`, as `--model` asks.
fn build_gb(opts: &cli::Options, rom: Vec<u8>) -> Result<Box<dyn Core>, String> {
    let archive = rom::archive_kind(&rom);
    let model = opts.model.resolve(&rom);
    let gb = GameBoy::with_model(rom, model)
        .map_err(|e| rom::cartridge_message(&opts.rom, &e, archive))?;
    if !gb.cartridge_header().header_checksum_ok {
        eprintln!("warning: ROM header checksum is wrong (running anyway)");
    }
    Ok(Box::new(gb))
}

/// A GBA core for `rom`, booted through `--bios` if given (else the built-in
/// HLE BIOS, DECISIONS #47).
fn build_gba(opts: &cli::Options, rom: Vec<u8>) -> Result<Box<dyn Core>, String> {
    let archive = rom::archive_kind(&rom);
    let bios = match &opts.bios {
        Some(path) => Some(rom::read_bios(path)?),
        None => None,
    };
    let gba =
        Gba::new(rom, bios).map_err(|e| rom::gba_cartridge_message(&opts.rom, &e, archive))?;
    Ok(Box::new(gba))
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

    // The system comes first: the ROM size limit depends on it, and so do the
    // flags that apply (DECISIONS #46).
    let system = match rom::pick_system(&opts.rom, opts.system) {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };
    match cli::check_system_flags(system, &opts) {
        Ok(Some(warning)) => eprintln!("warning: {warning}"),
        Ok(None) => {}
        Err(msg) => {
            eprintln!("error: {msg}\n\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    }
    let rom = match rom::read_rom(&opts.rom, system) {
        Ok(r) => r,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };
    let rom_hash = emu_common::replay::rom_hash(&rom);
    let built = match system {
        System::Gb => build_gb(&opts, rom),
        System::Gba => build_gba(&opts, rom),
    };
    let mut core = match built {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };

    let audio = if opts.mute {
        None
    } else {
        match AudioOutput::start() {
            Ok(a) => {
                core.set_audio_sample_rate(a.sample_rate);
                // The core clamps the rate; a mismatch would mean wrong pitch.
                match audio::check_core_rate(a.sample_rate, core.audio_sample_rate()) {
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
    saves.load(core.as_mut());

    // The recording starts from the battery RAM the session actually powered on with.
    let recorder = match &opts.record {
        None => None,
        Some(path) => {
            let header = record::header_for(core.as_ref(), rom_hash);
            match record::Recorder::create(path, &header) {
                Ok(r) => {
                    eprintln!("recording inputs to {}", path.display());
                    Some(r)
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
    };

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
        core,
        storage,
        audio,
        opts.palette.colors(),
        opts.scale,
        opts.test_pattern,
        shutdown,
    )
    .with_rom_path(opts.rom.clone())
    .with_recorder(recorder);
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

//! `gb`: desktop frontend (window, input, audio, saves) for the gb-core and gba-core
//! emulators. All platform-specific code lives in this crate; the cores stay pure.
#![forbid(unsafe_code)]

mod app;
mod audio;
mod cli;
mod keymap;
mod pacer;
mod picker;
mod record;
mod rom;
mod save;
mod shutdown;
mod state;
mod video;

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use emu_common::{Core, System};
use gb_core::GameBoy;
use gba_core::Gba;
use winit::event_loop::EventLoop;

use crate::app::{App, Storage};
use crate::audio::AudioOutput;
use crate::cli::Command;
use crate::picker::Startup;
use crate::save::SaveManager;
use crate::shutdown::ShutdownFlag;
use crate::state::StateSlots;

/// A Game Boy core for `rom`, as `--model` asks, and the hash of the ROM it hashed.
fn build_gb(
    opts: &cli::Options,
    path: &Path,
    rom: Vec<u8>,
) -> Result<(Box<dyn Core>, u64), String> {
    let archive = rom::archive_kind(&rom);
    let model = opts.model.resolve(&rom);
    let gb =
        GameBoy::with_model(rom, model).map_err(|e| rom::cartridge_message(path, &e, archive))?;
    if !gb.cartridge_header().header_checksum_ok {
        eprintln!("warning: ROM header checksum is wrong (running anyway)");
    }
    let rom_hash = gb.rom_hash();
    Ok((Box::new(gb), rom_hash))
}

/// A GBA core for `rom`, booted through `--bios` if given (else the built-in
/// HLE BIOS, which needs no copyrighted dump), and the hash of the ROM it hashed.
fn build_gba(
    opts: &cli::Options,
    path: &Path,
    rom: Vec<u8>,
) -> Result<(Box<dyn Core>, u64), String> {
    let archive = rom::archive_kind(&rom);
    let bios = match &opts.bios {
        Some(path) => Some(rom::read_bios(path)?),
        None => None,
    };
    let options = gba_core::CartOptions {
        save: opts.save_type,
        rtc: opts.rtc,
    };
    let mut gba = Gba::with_options(rom, bios, options)
        .map_err(|e| rom::gba_cartridge_message(path, &e, archive))?;
    // The save is loaded after this and replaces the clock if it carries one.
    if let Some(start) = opts.rtc_start {
        gba.set_rtc(start);
    }
    let rom_hash = gba.rom_hash();
    Ok((Box::new(gba), rom_hash))
}

/// Why a ROM could not be powered on.
enum PowerOnError {
    /// The options do not fit this ROM's system: a usage error (exit status 2).
    Usage(String),
    /// The file could not be read or the core refused it.
    Load(String),
}

impl PowerOnError {
    fn message(self) -> String {
        match self {
            PowerOnError::Usage(m) | PowerOnError::Load(m) => m,
        }
    }
}

/// Read the ROM at `path` and build its core: system detection, the size caps
/// and the option checks. Used at startup and for a ROM dropped on the window.
fn power_on(opts: &cli::Options, path: &Path) -> Result<(Box<dyn Core>, u64), PowerOnError> {
    // The file is read once; the system (and so the size cap) comes from the
    // flag, the name or the header.
    let (system, rom) = rom::load_rom(path, opts.system).map_err(PowerOnError::Load)?;
    match cli::check_system_flags(system, opts) {
        Ok(Some(warning)) => eprintln!("warning: {warning}"),
        Ok(None) => {}
        Err(msg) => return Err(PowerOnError::Usage(msg)),
    }
    match system {
        System::Gb => build_gb(opts, path, rom),
        System::Gba => build_gba(opts, path, rom),
    }
    .map_err(PowerOnError::Load)
}

fn new_event_loop() -> Result<EventLoop<()>, ExitCode> {
    EventLoop::new().map_err(|e| {
        eprintln!("error: cannot start the windowing system: {e}");
        ExitCode::FAILURE
    })
}

fn main() -> ExitCode {
    let mut opts = match cli::parse(std::env::args_os().skip(1)) {
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

    // Started from a launcher there is no command line: ask for the ROM. When it
    // will ask, the event loop is built first: on macOS winit creates the
    // application object there and sets up its launch handling, and the dialog
    // should run against that. Otherwise it is built after the ROM loads, so a bad
    // path is reported without needing a display.
    let mut event_loop = None;
    let rom_path = match picker::startup(opts.rom.take(), opts.test_pattern) {
        Startup::Given(path) => path,
        Startup::Ask => {
            event_loop = match new_event_loop() {
                Ok(l) => Some(l),
                Err(code) => return code,
            };
            match picker::pick_rom() {
                Some(path) => path,
                None => {
                    eprintln!("no ROM chosen");
                    return ExitCode::SUCCESS;
                }
            }
        }
        Startup::Missing => {
            eprintln!("error: missing ROM path\n\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    let opts = Arc::new(opts);

    // The core hashed the ROM as it was built; the recording header reuses that.
    let (mut core, rom_hash) = match power_on(&opts, &rom_path) {
        Ok(c) => c,
        Err(PowerOnError::Usage(msg)) => {
            eprintln!("error: {msg}\n\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
        Err(PowerOnError::Load(msg)) => {
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

    let mut saves = SaveManager::new(&rom_path, Instant::now());
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

    let event_loop = match event_loop.map_or_else(new_event_loop, Ok) {
        Ok(l) => l,
        Err(code) => return code,
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
        states: StateSlots::new(&rom_path),
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
    .with_rom_path(rom_path)
    .with_recorder(recorder)
    .with_loader({
        let opts = Arc::clone(&opts);
        Box::new(move |path| {
            power_on(&opts, path)
                .map(|(core, _)| core)
                .map_err(PowerOnError::message)
        })
    });
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

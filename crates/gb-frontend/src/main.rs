//! `gb`: desktop frontend (window, input, audio, saves) for the gb-core and gba-core
//! emulators. All platform-specific code lives in this crate; the cores stay pure.
#![forbid(unsafe_code)]

mod app;
mod audio;
mod cli;
mod gui;
mod keymap;
mod library;
mod logging;
mod pacer;
mod paths;
mod picker;
mod record;
mod rom;
mod save;
mod shutdown;
mod state;
mod ui;
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
use crate::library::{GameId, GamePaths, Library};
use crate::paths::DataDir;
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
        log::warn!("ROM header checksum is wrong (running anyway)");
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
        Ok(Some(warning)) => log::warn!("{warning}"),
        Ok(None) => {}
        Err(msg) => return Err(PowerOnError::Usage(msg)),
    }
    match system {
        System::Gb => build_gb(opts, path, rom),
        System::Gba => build_gba(opts, path, rom),
    }
    .map_err(PowerOnError::Load)
}

/// [`power_on`] the ROM at `path` and find the files its player data lives in: the
/// app folder, keyed by the ROM's content, after importing any save next to the ROM
/// (next to the ROM itself if there is no app folder). The ROM is not added to the
/// library.
fn open_game(
    opts: &cli::Options,
    data: Option<&DataDir>,
    path: &Path,
) -> Result<(Box<dyn Core>, u64, GamePaths), PowerOnError> {
    let (core, rom_hash) = power_on(opts, path)?;
    let paths = library::paths_for_opened_rom(data, path, &core.title(), rom_hash);
    Ok((core, rom_hash, paths))
}

/// The placeholder core of a run that starts on the library, before a game is chosen:
/// a blank cartridge with no battery, so it never writes a save.
fn idle_core() -> Result<Box<dyn Core>, String> {
    GameBoy::new(vec![0; 0x8000])
        .map(|gb| Box::new(gb) as Box<dyn Core>)
        .map_err(|e| format!("cannot start without a game: {e}"))
}

/// The first lines of a log: what ran, on what, and where its data is.
fn log_startup(data: Option<&DataDir>) {
    log::info!(
        "gameboyem {} on {} {}; data folder {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        data.map_or_else(
            || "unavailable".to_owned(),
            |d| d.root().display().to_string()
        )
    );
}

/// Shown in the window title when the data folder could not be used.
const NO_DATA_NOTICE: &str = "no data folder: saves next to the ROM";

fn new_event_loop() -> Result<EventLoop<()>, ExitCode> {
    EventLoop::new().map_err(|e| {
        log::error!("cannot start the windowing system: {e}");
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

    // Everything the player makes lives in the app folder, whichever way the ROM is
    // opened. Help and usage errors above do not create it.
    // If there is no usable folder the game still runs: saves, states and
    // screenshots go next to the ROM as in older versions, with a warning.
    let (data, data_problem) = match DataDir::prepare() {
        Ok(d) => (Some(d), None),
        Err(e) => (None, Some(e)),
    };
    logging::init(
        data.as_ref().map(DataDir::log_file).as_deref(),
        logging::level_from_env(),
    );
    if let Some(problem) = &data_problem {
        log::warn!(
            "{problem}; saves, save states and screenshots go next to the ROM instead, \
             and there is no log file or library"
        );
    }
    log_startup(data.as_ref());

    if opts.add {
        let Some(data) = &data else {
            log::error!("--add needs the data folder, which is unavailable");
            return ExitCode::FAILURE;
        };
        return add_to_library(data, opts.rom.take());
    }

    // The game list the UI shows, when there is a data folder to keep it in.
    // (Registering the games bundled with the app belongs right here, after the library
    // is opened and before the window is built.)
    let library = data.as_ref().map(Library::open);

    // Started from a launcher there is no command line, so no ROM: the window opens on
    // the library and the player picks a game there.
    let rom_path = match picker::startup(opts.rom.take(), opts.test_pattern) {
        Startup::Given(path) => Some(path),
        Startup::Library => None,
        Startup::Missing => {
            log::error!("missing ROM path");
            eprintln!("\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    if rom_path.is_none() && opts.record.is_some() {
        log::error!("--record needs a ROM path");
        eprintln!("\n{}", cli::USAGE);
        return ExitCode::from(2);
    }
    let opts = Arc::new(opts);

    // The core hashed the ROM as it was built; the recording header reuses that. With
    // no ROM the core is a blank cartridge that the library screen never runs.
    let (mut core, rom_hash, paths, game_id) = match &rom_path {
        Some(rom_path) => {
            let (core, rom_hash, paths) = match open_game(&opts, data.as_ref(), rom_path) {
                Ok(c) => c,
                Err(PowerOnError::Usage(msg)) => {
                    log::error!("{msg}");
                    eprintln!("\n{}", cli::USAGE);
                    return ExitCode::from(2);
                }
                Err(PowerOnError::Load(msg)) => {
                    log::error!("{msg}");
                    return ExitCode::FAILURE;
                }
            };
            log::info!(
                "ROM {} (title {:?}, hash {rom_hash:016x})",
                rom_path.display(),
                core.title()
            );
            let id = GameId::new(&core.title(), rom_hash);
            (core, rom_hash, paths, Some(id))
        }
        None => match idle_core() {
            Ok(core) => (core, 0, GamePaths::beside(Path::new("library")), None),
            Err(e) => {
                log::error!("{e}");
                return ExitCode::FAILURE;
            }
        },
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
                        log::warn!("audio disabled: {e}");
                        None
                    }
                }
            }
            Err(e) => {
                log::warn!("audio disabled: {e}");
                None
            }
        }
    };

    let mut saves = SaveManager::new(&paths.sav, Instant::now());
    saves.load(core.as_mut());

    // The recording starts from the battery RAM the session actually powered on with.
    let recorder = match &opts.record {
        None => None,
        Some(path) => {
            let header = record::header_for(core.as_ref(), rom_hash);
            match record::Recorder::create(path, &header) {
                Ok(r) => {
                    log::info!("recording inputs to {}", path.display());
                    Some(r)
                }
                Err(e) => {
                    log::error!("{e}");
                    return ExitCode::FAILURE;
                }
            }
        }
    };

    let event_loop = match new_event_loop() {
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
            log::warn!("cannot install the Ctrl-C/SIGTERM handler: {e}");
        }
    }
    let storage = Storage {
        saves,
        states: StateSlots::new(paths.clone()),
        paths,
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
    .with_notice(data_problem.map(|_| NO_DATA_NOTICE.to_owned()))
    .with_recorder(recorder)
    .with_library(library)
    .with_game(game_id)
    .with_loader({
        let opts = Arc::clone(&opts);
        let data = data.clone();
        Box::new(move |path| {
            open_game(&opts, data.as_ref(), path)
                .map(|(core, _, paths)| (core, paths))
                .map_err(PowerOnError::message)
        })
    });
    if rom_path.is_none() {
        app = app.in_library();
    }
    let run = event_loop.run_app(&mut app);
    if let Some(e) = app.error.take() {
        log::error!("{e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = run {
        log::error!("event loop failed: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// `--add`: copy the ROM into the library and exit.
fn add_to_library(data: &DataDir, rom: Option<std::path::PathBuf>) -> ExitCode {
    let Some(rom) = rom else {
        log::error!("--add needs a ROM path");
        return ExitCode::from(2);
    };
    match Library::open(data).add_rom(&rom) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

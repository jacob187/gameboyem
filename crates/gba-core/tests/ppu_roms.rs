//! Real-ROM display checks: each runs a ROM for some frames and dumps the frame
//! as a PNG into `target/gba-ppu/` for manual review. They assert nothing, so
//! they are `#[ignore]`d and never gate. Run with `cargo test -p gba-core --test ppu_roms
//! -- --ignored`. The `roms/` tree is read-only.

mod common;

use gba_core::Gba;
use std::path::Path;

const FRAMES: u32 = 120;

fn dump(rel: &str, name: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../roms/test/gba")
        .join(rel);
    let Ok(rom) = std::fs::read(&path) else {
        eprintln!("skipping {rel}: {} not found", path.display());
        return;
    };
    let mut gba = Gba::new(rom, None).expect("ROM loads");
    for _ in 0..FRAMES {
        gba.step_frame();
    }
    common::write_png(
        gba.framebuffer(),
        &common::out_dir().join(format!("rom_{name}.png")),
    );
}

macro_rules! rom_dump {
    ($fn_name:ident, $rel:expr, $name:expr) => {
        #[test]
        #[ignore = "writes PNGs for manual inspection"]
        fn $fn_name() {
            dump($rel, $name);
        }
    };
}

rom_dump!(jsmolka_hello, "jsmolka/ppu/hello.gba", "jsmolka_hello");
rom_dump!(jsmolka_shades, "jsmolka/ppu/shades.gba", "jsmolka_shades");
rom_dump!(
    jsmolka_stripes,
    "jsmolka/ppu/stripes.gba",
    "jsmolka_stripes"
);
rom_dump!(tonc_m3_demo, "tonc/bin/m3_demo.gba", "tonc_m3_demo");
rom_dump!(tonc_bm_modes, "tonc/bin/bm_modes.gba", "tonc_bm_modes");
rom_dump!(tonc_sbb_reg, "tonc/bin/sbb_reg.gba", "tonc_sbb_reg");
rom_dump!(tonc_obj_demo, "tonc/bin/obj_demo.gba", "tonc_obj_demo");

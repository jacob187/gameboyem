//! Hardware model selection (DECISIONS #31, #33).

use crate::cartridge::{Cartridge, CartridgeHeader};

/// Which console to emulate. Chosen by the host; the core never picks it from
/// the ROM (a CGB-flagged ROM still runs as a DMG under [`Model::Dmg`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Model {
    /// Original Game Boy (the default of [`crate::GameBoy::new`]).
    Dmg,
    /// Game Boy Color.
    Cgb,
}

impl Model {
    /// The model a cartridge asks for: CGB-flagged (0x80 / 0xC0) carts want a
    /// [`Model::Cgb`], everything else a [`Model::Dmg`]. Pure helper for hosts;
    /// the core never calls it.
    pub fn from_header(header: &CartridgeHeader) -> Self {
        if header.supports_cgb() {
            Self::Cgb
        } else {
            Self::Dmg
        }
    }

    /// The byte a save state stores for this model.
    pub(crate) fn id(self) -> u8 {
        match self {
            Self::Dmg => 0,
            Self::Cgb => 1,
        }
    }

    /// The inverse of [`Model::id`]; `None` for a byte no model writes. Public so a
    /// host can match on the model behind `Core::hw_variant` and stay exhaustive.
    pub fn from_id(id: u8) -> Option<Self> {
        match id {
            0 => Some(Self::Dmg),
            1 => Some(Self::Cgb),
            _ => None,
        }
    }
}

/// What the machine behaves like: the model plus, on a CGB, the mode the boot
/// ROM picked from the cartridge header. Fixed at power-on; it is a function of
/// the model and the ROM, so a save state only carries it as a corruption check
/// (a header byte that must equal the machine's own [`Hw::id`]), never to set it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hw {
    Dmg,
    /// A CGB running a cartridge without a CGB flag: the boot ROM selects
    /// DMG-compatibility mode, in which most CGB-only registers are unmapped.
    CgbCompat,
    Cgb,
}

impl Hw {
    pub(crate) fn new(model: Model, header: &CartridgeHeader) -> Self {
        match model {
            Model::Dmg => Self::Dmg,
            Model::Cgb if header.supports_cgb() => Self::Cgb,
            Model::Cgb => Self::CgbCompat,
        }
    }

    /// The byte a save state stores for this variant.
    pub(crate) fn id(self) -> u8 {
        match self {
            Self::Dmg => 0,
            Self::CgbCompat => 1,
            Self::Cgb => 2,
        }
    }

    pub(crate) fn is_cgb(self) -> bool {
        self != Self::Dmg
    }
}

/// B at hand-off in DMG-compatibility mode (Pan Docs "Power Up Sequence"): the
/// sum of the 16 title bytes if the old licensee code is 0x01, or if it is
/// 0x33 and the new licensee code is "01"; otherwise 0.
pub(crate) fn compat_b(cart: &dyn Cartridge) -> u8 {
    let old = cart.read_rom(0x14B);
    let new_is_01 = cart.read_rom(0x144) == b'0' && cart.read_rom(0x145) == b'1';
    if old == 0x01 || (old == 0x33 && new_is_01) {
        (0x134..=0x143u16).fold(0u8, |sum, a| sum.wrapping_add(cart.read_rom(a)))
    } else {
        0
    }
}

#[cfg(test)]
#[path = "cgb_tests.rs"]
mod tests;

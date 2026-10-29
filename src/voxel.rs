//! Voxel representation and the material property database.
//!
//! # Memory layout
//!
//! A [`Voxel`] is **exactly 8 bytes** — the same size as a single pointer on
//! `wasm32`. A `16³` chunk is therefore 32 KiB, i.e. it fits comfortably in L1
//! when the kernels sweep one horizontal slab (`16 × 16 × 8 B = 2 KiB`) at a
//! time:
//!
//! ```text
//! offset 0 : species  u8     0 ..= 21   (indexes MATERIALS)
//! offset 1 : mass     u8     0 ..= 8
//! offset 2 : temp     i16    -273 ..= 2000  (°C)
//! offset 4 : pressure u16    quarters of a pascal
//! offset 6 : flags    u8     bitfield, see `flags::*`
//! offset 7 : aux      u8     packed flow direction / damage / bubble seed
//! ```
//!
//! Everything is `Copy`; there is no heap, no `Rc`, no `Box` anywhere in the
//! simulation path, which is what lets the WASM build run a full chunk tick
//! without touching the allocator (and therefore without a GC-like "memory
//! grow" stall on the host).

use crate::units::{Mass, Press, Temp, MASS_FULL};

// ---------------------------------------------------------------------------
// Species
// ---------------------------------------------------------------------------

/// Every phase / block species the mod knows about.
///
/// The discriminant is stable on the wire: the engine serialises it, so *never*
/// reorder or delete variants. New species are appended (see
/// `horizon-mod.toml::[[blocks]]`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
#[repr(u8)]
pub enum Species {
    /// Empty cell (vacuum-ish air, gets the ambient temperature of the world).
    Air = 0,
    /// Liquid water.
    Water = 1,
    /// Freezing / frozen water; mass 1..3 = thin ice, 8 = solid ice.
    Ice = 2,
    /// Water vapour.
    Steam = 3,
    /// Molten rock.
    Lava = 4,
    /// Solid, still-hot magma source block.
    Magma = 5,
    /// Chilled lava (fast path, low viscosity).
    Basalt = 6,
    /// Chilled lava quenched by water.
    Obsidian = 7,
    /// Chilled lava quenched by water at low pressure (porous, floats).
    Pumice = 8,
    /// Volcanic glass created by explosive quenching.
    VolcanicGlass = 9,
    Stone = 10,
    Dirt = 11,
    Gravel = 12,
    Sand = 13,
    Wood = 14,
    Plank = 15,
    Leaves = 16,
    Glass = 17,
    Metal = 18,
    /// Fire / burning cell, a gas-phase heat source.
    Fire = 19,
    /// Indestructible world floor.
    Bedrock = 20,
    /// Compressed air pocket — no mass, but carries pressure (used by the
    /// pressure solver to keep sealed volumes stable).
    CompressedAir = 21,
    /// Snow layer (very light, melts into water at 0 °C).
    Snow = 22,
    /// Salt water (denser, freezes below -2 °C). Reserved for the "marine"
    /// datapack, included here so the density/phase model is complete.
    SaltWater = 23,
    /// Liquid metal (smelted ore). Boils into... metal steam, practically
    /// never; the phase table sends it to `Metal` at its melt point.
    MoltenMetal = 24,
}

/// Number of species (kept as a const so arrays can be sized).
pub const SPECIES_COUNT: usize = 25;

impl Species {
    /// All variants in wire order.
    pub const ALL: [Species; SPECIES_COUNT] = [
        Species::Air,
        Species::Water,
        Species::Ice,
        Species::Steam,
        Species::Lava,
        Species::Magma,
        Species::Basalt,
        Species::Obsidian,
        Species::Pumice,
        Species::VolcanicGlass,
        Species::Stone,
        Species::Dirt,
        Species::Gravel,
        Species::Sand,
        Species::Wood,
        Species::Plank,
        Species::Leaves,
        Species::Glass,
        Species::Metal,
        Species::Fire,
        Species::Bedrock,
        Species::CompressedAir,
        Species::Snow,
        Species::SaltWater,
        Species::MoltenMetal,
    ];

    /// Decodes a wire byte. Unknown values fall back to [`Species::Air`] so a
    /// corrupted save can never make the server panic.
    #[inline]
    #[must_use]
    pub const fn from_u8(v: u8) -> Species {
        match v {
            0 => Species::Air,
            1 => Species::Water,
            2 => Species::Ice,
            3 => Species::Steam,
            4 => Species::Lava,
            5 => Species::Magma,
            6 => Species::Basalt,
            7 => Species::Obsidian,
            8 => Species::Pumice,
            9 => Species::VolcanicGlass,
            10 => Species::Stone,
            11 => Species::Dirt,
            12 => Species::Gravel,
            13 => Species::Sand,
            14 => Species::Wood,
            15 => Species::Plank,
            16 => Species::Leaves,
            17 => Species::Glass,
            18 => Species::Metal,
            19 => Species::Fire,
            20 => Species::Bedrock,
            21 => Species::CompressedAir,
            22 => Species::Snow,
            23 => Species::SaltWater,
            24 => Species::MoltenMetal,
            _ => Species::Air,
        }
    }

    /// Stable wire byte.
    #[inline]
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Physical class of the material.
    #[inline]
    #[must_use]
    pub const fn class(self) -> Class {
        MATERIALS[self as usize].class
    }

    /// Material descriptor (real-world constants, see [`MATERIALS`]).
    ///
    /// Returned **by value** on purpose: `Material` is `Copy`, and a `const` item
    /// cannot be borrowed into a `&'static` reference from a `const fn` (the array
    /// is inlined at every use site), so copying the 40-byte descriptor is both
    /// the cheapest and the only correct option.
    #[inline]
    #[must_use]
    pub const fn material(self) -> Material {
        MATERIALS[self as usize]
    }

    /// Density in `kg/m³`.
    #[inline]
    #[must_use]
    pub const fn density(self) -> u16 {
        MATERIALS[self as usize].density
    }

    /// `true` for species that the hydro kernel moves around.
    #[inline]
    #[must_use]
    pub const fn is_fluid(self) -> bool {
        matches!(self.class(), Class::Liquid | Class::Gas)
    }

    /// `true` for liquids.
    #[inline]
    #[must_use]
    pub const fn is_liquid(self) -> bool {
        matches!(self.class(), Class::Liquid)
    }

    /// `true` for gases.
    #[inline]
    #[must_use]
    pub const fn is_gas(self) -> bool {
        matches!(self.class(), Class::Gas)
    }

    /// `true` for non-fluid, non-empty species.
    #[inline]
    #[must_use]
    pub const fn is_solid(self) -> bool {
        matches!(self.class(), Class::Solid)
    }

    /// `true` when the cell is transparent to the fluid kernel (air-like).
    #[inline]
    #[must_use]
    pub const fn is_empty_like(self) -> bool {
        matches!(self, Species::Air | Species::CompressedAir)
    }

    /// `true` when the species blocks vertical movement (solid or liquid).
    #[inline]
    #[must_use]
    pub const fn blocks_flow(self) -> bool {
        !matches!(self.class(), Class::Gas | Class::Void)
    }

    /// Light emitted in block-light units (`0..=15`). Lava glows, stone not.
    #[inline]
    #[must_use]
    pub const fn light(self) -> u8 {
        MATERIALS[self as usize].light
    }

    /// Dragon-egg-style hardness used by the pressure fracture model.
    #[inline]
    #[must_use]
    pub const fn yield_pressure(self) -> u16 {
        MATERIALS[self as usize].yield_pressure
    }

    /// Name shown in the Atlas and in `/hd probe` output.
    #[inline]
    #[must_use]
    pub const fn name(self) -> &'static str {
        MATERIALS[self as usize].name
    }

    /// Parses a species from the name used in the manifest and by `/hd set`
    /// (`"water"`, `"Hydrodynamics:Steam"`, `"volcanic_glass"`) or from its
    /// numeric id (`"9"`).
    ///
    /// Matching is case-insensitive and ignores `_`, `-`, spaces and any
    /// namespace prefix, so a datapack author can type `minecraft:water` and a
    /// console user can type `Water` and both resolve. There is no allocation:
    /// the comparison is done in place over a fixed candidate table, which keeps
    /// the parser usable from the wasm command surface where a bad argument must
    /// never panic.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Species> {
        // A bare number is the wire id.
        let digits = name.as_bytes();
        if !digits.is_empty() && digits.iter().all(u8::is_ascii_digit) {
            let mut id = 0u32;
            for b in digits {
                id = id * 10 + u32::from(b - b'0');
                if id > u8::MAX as u32 {
                    return None;
                }
            }
            let sp = Species::from_u8(id as u8);
            // Reject ids that the table pads with Air but that were never named.
            return if sp == Species::Air && id != 0 {
                None
            } else {
                Some(sp)
            };
        }
        // Strip a `namespace:` prefix.
        let name = match name.split_once(':') {
            Some((_, rest)) => rest,
            None => name,
        };
        let mut i = 0;
        while i < MATERIALS.len() {
            let sp = Species::from_u8(i as u8);
            if eq_loose(sp.name(), name) {
                return Some(sp);
            }
            i += 1;
        }
        None
    }
}

/// Case-insensitive, separator-insensitive string equality.
///
/// `"VolcanicGlass"`, `"volcanic_glass"` and `"VOLCANIC-GLASS"` all match, which
/// is what makes a hand-typed `/hd set` argument forgiving.
#[must_use]
fn eq_loose(a: &str, b: &str) -> bool {
    let mut x = a.bytes().filter(|c| !matches!(c, b'_' | b'-' | b' '));
    let mut y = b.bytes().filter(|c| !matches!(c, b'_' | b'-' | b' '));
    loop {
        match (x.next(), y.next()) {
            (None, None) => return true,
            (Some(p), Some(q)) if p.eq_ignore_ascii_case(&q) => {}
            _ => return false,
        }
    }
}

// ---------------------------------------------------------------------------
// Class
// ---------------------------------------------------------------------------

/// Broad physical category; the kernels branch on this instead of on species,
/// which keeps the hot loop to a handful of well-predicted branches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Class {
    /// Empty space: never simulated, always "asleep".
    Void = 0,
    /// Flows, fills, freezes.
    Liquid = 1,
    /// Rises, expands, condenses.
    Gas = 2,
    /// Static geometry, but conducts heat and can be fractured.
    Solid = 3,
}

// ---------------------------------------------------------------------------
// Material database
// ---------------------------------------------------------------------------

/// Real-world material constants, quantised to integers.
///
/// Sources: CRC Handbook (thermal data), USGS basalt thermophysics, ASM Metals
/// Reference (iron), and the Horizon engine block registry for the non-physical
/// entries (`CompressedAir`, `Fire`).
#[derive(Clone, Copy, Debug)]
pub struct Material {
    /// Display name.
    pub name: &'static str,
    /// Physical class.
    pub class: Class,
    /// Density in `kg/m³`.
    pub density: u16,
    /// Thermal conductivity in **`W/(m·K) × 10`** (water `0.6 → 6`).
    pub conductivity: u16,
    /// Specific heat capacity in **`J/(kg·K) / 10`** (water `4186 → 418`).
    pub heat_capacity: u16,
    /// Melting / freezing point in °C (`i16::MIN` when the notion is meaningless).
    pub melt_point: i16,
    /// Species produced when the cell crosses `melt_point` upward.
    pub melts_into: Species,
    /// Species produced when the cell crosses `melt_point` downward.
    pub freezes_into: Species,
    /// Boiling point in °C (`i16::MAX` for solids that never boil).
    pub boil_point: i16,
    /// Species produced when the cell crosses `boil_point` upward.
    pub boils_into: Species,
    /// Latent heat of fusion, `kJ/kg`.
    pub latent_fusion: u16,
    /// Latent heat of vaporisation, `kJ/kg`.
    pub latent_vapor: u16,
    /// Emissivity in percent (`0..=100`) for the T⁴ radiation term.
    pub emissivity: u8,
    /// Hydrostatic head (in [`crate::units::Press`] units, ≈40 per block of
    /// water) above which the block starts to fracture. `u16::MAX` means
    /// "effectively indestructible".
    pub yield_pressure: u16,
    /// Light level emitted (`0..=15`).
    pub light: u8,
    /// Miscibility with water: `0` = pure, `255` = dissolves instantly
    /// (used by the pollution/salinity model).
    pub solubility: u8,
    /// `0xRRGGBB` colour used by the Atlas illustrations and the debug overlay.
    pub map_color: u32,
}

/// `Species` → material constants. Indexed by `Species as usize`.
pub const MATERIALS: [Material; SPECIES_COUNT] = [
    // ---- 0 Air -----------------------------------------------------------
    Material {
        name: "Air",
        class: Class::Void,
        density: 1,
        conductivity: 2,
        heat_capacity: 100,
        melt_point: i16::MIN,
        melts_into: Species::Air,
        freezes_into: Species::Air,
        boil_point: i16::MAX,
        boils_into: Species::Air,
        latent_fusion: 0,
        latent_vapor: 0,
        emissivity: 0,
        yield_pressure: u16::MAX,
        light: 0,
        solubility: 0,
        map_color: 0x00_00_00,
    },
    // ---- 1 Water ---------------------------------------------------------
    Material {
        name: "Water",
        class: Class::Liquid,
        density: 1000,
        conductivity: 6,    // 0.60 W/(m·K)
        heat_capacity: 419, // 4186 J/(kg·K)
        melt_point: 0,
        melts_into: Species::Water,
        freezes_into: Species::Ice,
        boil_point: 100,
        boils_into: Species::Steam,
        latent_fusion: 334, // kJ/kg
        latent_vapor: 2257, // kJ/kg
        emissivity: 96,
        yield_pressure: u16::MAX,
        light: 0,
        solubility: 255,
        map_color: 0x30_64_EE,
    },
    // ---- 2 Ice -----------------------------------------------------------
    Material {
        name: "Ice",
        class: Class::Solid,
        density: 917,
        conductivity: 22,   // 2.2 W/(m·K)
        heat_capacity: 205, // 2050 J/(kg·K)
        melt_point: 0,
        melts_into: Species::Water,
        freezes_into: Species::Ice,
        boil_point: i16::MAX,
        boils_into: Species::Steam,
        latent_fusion: 334,
        latent_vapor: 0,
        emissivity: 97,
        yield_pressure: 240, // brittle: a 6 m head cracks an ice sheet
        light: 0,
        solubility: 0,
        map_color: 0x9F_D8_FF,
    },
    // ---- 3 Steam ---------------------------------------------------------
    Material {
        name: "Steam",
        class: Class::Gas,
        density: 1,         // 0.6 kg/m³ at 100 °C — stored as 1, see `gas_mass_to_density`
        conductivity: 2,    // 0.025 W/(m·K)
        heat_capacity: 201, // 2010 J/(kg·K)
        melt_point: i16::MIN,
        melts_into: Species::Steam,
        freezes_into: Species::Steam,
        boil_point: i16::MIN, // "condensation point" uses `steam_condense_point()`
        boils_into: Species::Steam,
        latent_fusion: 0,
        latent_vapor: 2257,
        emissivity: 90,
        yield_pressure: u16::MAX,
        light: 0,
        solubility: 0,
        map_color: 0xD8_D8_EC,
    },
    // ---- 4 Lava ----------------------------------------------------------
    Material {
        name: "Lava",
        class: Class::Liquid,
        density: 2600,     // basalt melt
        conductivity: 17,  // 1.7 W/(m·K)
        heat_capacity: 84, // 840 J/(kg·K) — actually 0.84 kJ/(kg·K)
        melt_point: 1_050, // solidus of basalt
        melts_into: Species::Lava,
        freezes_into: Species::Basalt,
        boil_point: i16::MAX,
        boils_into: Species::Stone,
        latent_fusion: 400,
        latent_vapor: 0,
        emissivity: 88,
        yield_pressure: u16::MAX,
        light: 15,
        solubility: 0,
        map_color: 0xD4_5A_0A,
    },
    // ---- 5 Magma (hot source block) --------------------------------------
    Material {
        name: "Magma",
        class: Class::Solid,
        density: 2700,
        conductivity: 12,
        heat_capacity: 84,
        melt_point: 1_100,
        melts_into: Species::Lava,
        freezes_into: Species::Basalt,
        boil_point: i16::MAX,
        boils_into: Species::Lava,
        latent_fusion: 400,
        latent_vapor: 0,
        emissivity: 85,
        yield_pressure: 900,
        light: 10,
        solubility: 0,
        map_color: 0x8E_34_0C,
    },
    // ---- 6 Basalt --------------------------------------------------------
    Material {
        name: "Basalt",
        class: Class::Solid,
        density: 2900,
        conductivity: 17,
        heat_capacity: 84,
        melt_point: 1_050,
        melts_into: Species::Lava,
        freezes_into: Species::Basalt,
        boil_point: i16::MAX,
        boils_into: Species::Lava,
        latent_fusion: 400,
        latent_vapor: 0,
        emissivity: 80,
        yield_pressure: 1_200,
        light: 0,
        solubility: 0,
        map_color: 0x4A_4A_52,
    },
    // ---- 7 Obsidian ------------------------------------------------------
    Material {
        name: "Obsidian",
        class: Class::Solid,
        density: 2400,
        conductivity: 12,
        heat_capacity: 84,
        melt_point: 1_150,
        melts_into: Species::Lava,
        freezes_into: Species::Obsidian,
        boil_point: i16::MAX,
        boils_into: Species::Lava,
        latent_fusion: 400,
        latent_vapor: 0,
        emissivity: 72,
        yield_pressure: 1_600,
        light: 0,
        solubility: 0,
        map_color: 0x1A_10_28,
    },
    // ---- 8 Pumice --------------------------------------------------------
    Material {
        name: "Pumice",
        class: Class::Solid,
        density: 600, // floats on water — drives the buoyancy kernel
        conductivity: 3,
        heat_capacity: 84,
        melt_point: 900,
        melts_into: Species::Lava,
        freezes_into: Species::Pumice,
        boil_point: i16::MAX,
        boils_into: Species::Lava,
        latent_fusion: 400,
        latent_vapor: 0,
        emissivity: 75,
        yield_pressure: 90,
        light: 0,
        solubility: 30,
        map_color: 0xB8_B0_A0,
    },
    // ---- 9 Volcanic glass ------------------------------------------------
    Material {
        name: "Volcanic Glass",
        class: Class::Solid,
        density: 2350,
        conductivity: 13,
        heat_capacity: 84,
        melt_point: 1_100,
        melts_into: Species::Lava,
        freezes_into: Species::VolcanicGlass,
        boil_point: i16::MAX,
        boils_into: Species::Lava,
        latent_fusion: 400,
        latent_vapor: 0,
        emissivity: 70,
        yield_pressure: 70, // very brittle
        light: 0,
        solubility: 0,
        map_color: 0x2E_20_34,
    },
    // ---- 10 Stone --------------------------------------------------------
    Material {
        name: "Stone",
        class: Class::Solid,
        density: 2600,
        conductivity: 15,
        heat_capacity: 84,
        melt_point: 1_200,
        melts_into: Species::Lava,
        freezes_into: Species::Stone,
        boil_point: i16::MAX,
        boils_into: Species::Lava,
        latent_fusion: 400,
        latent_vapor: 0,
        emissivity: 78,
        yield_pressure: 1_400,
        light: 0,
        solubility: 0,
        map_color: 0x7A_7A_7A,
    },
    // ---- 11 Dirt ---------------------------------------------------------
    Material {
        name: "Dirt",
        class: Class::Solid,
        density: 1400,
        conductivity: 5,
        heat_capacity: 84,
        melt_point: 1_150,
        melts_into: Species::Lava,
        freezes_into: Species::Dirt,
        boil_point: i16::MAX,
        boils_into: Species::Lava,
        latent_fusion: 400,
        latent_vapor: 0,
        emissivity: 95,
        yield_pressure: 320, // erodes, but survives normal dams
        light: 0,
        solubility: 120,
        map_color: 0x86_5A_3C,
    },
    // ---- 12 Gravel -------------------------------------------------------
    Material {
        name: "Gravel",
        class: Class::Solid,
        density: 1600,
        conductivity: 7,
        heat_capacity: 84,
        melt_point: 1_100,
        melts_into: Species::Lava,
        freezes_into: Species::Gravel,
        boil_point: i16::MAX,
        boils_into: Species::Lava,
        latent_fusion: 400,
        latent_vapor: 0,
        emissivity: 92,
        yield_pressure: 200,
        light: 0,
        solubility: 90,
        map_color: 0x8A_80_78,
    },
    // ---- 13 Sand ---------------------------------------------------------
    Material {
        name: "Sand",
        class: Class::Solid,
        density: 1600,
        conductivity: 3,
        heat_capacity: 80,
        melt_point: 1_700, // glass transition
        melts_into: Species::VolcanicGlass,
        freezes_into: Species::Sand,
        boil_point: i16::MAX,
        boils_into: Species::VolcanicGlass,
        latent_fusion: 140,
        latent_vapor: 0,
        emissivity: 94,
        yield_pressure: 150,
        light: 0,
        solubility: 60,
        map_color: 0xDA_C8_7A,
    },
    // ---- 14 Wood ---------------------------------------------------------
    Material {
        name: "Wood",
        class: Class::Solid,
        density: 700, // oak — floats
        conductivity: 1,
        heat_capacity: 170, // 1700 J/(kg·K)
        melt_point: 300,    // chars/ignites; handled by the fire kernel
        melts_into: Species::Fire,
        freezes_into: Species::Wood,
        boil_point: i16::MAX,
        boils_into: Species::Fire,
        latent_fusion: 0,
        latent_vapor: 0,
        emissivity: 90,
        yield_pressure: 180,
        light: 0,
        solubility: 10,
        map_color: 0x9C_6B_3E,
    },
    // ---- 15 Plank --------------------------------------------------------
    Material {
        name: "Plank",
        class: Class::Solid,
        density: 650,
        conductivity: 1,
        heat_capacity: 170,
        melt_point: 300,
        melts_into: Species::Fire,
        freezes_into: Species::Plank,
        boil_point: i16::MAX,
        boils_into: Species::Fire,
        latent_fusion: 0,
        latent_vapor: 0,
        emissivity: 90,
        yield_pressure: 110, // a plank wall bursts at a moderate head
        light: 0,
        solubility: 10,
        map_color: 0xB0_8A_5A,
    },
    // ---- 16 Leaves -------------------------------------------------------
    Material {
        name: "Leaves",
        class: Class::Solid,
        density: 120,
        conductivity: 1,
        heat_capacity: 170,
        melt_point: 230,
        melts_into: Species::Fire,
        freezes_into: Species::Leaves,
        boil_point: i16::MAX,
        boils_into: Species::Fire,
        latent_fusion: 0,
        latent_vapor: 0,
        emissivity: 88,
        yield_pressure: 26, // water shoves foliage aside
        light: 0,
        solubility: 20,
        map_color: 0x3C_8A_28,
    },
    // ---- 17 Glass --------------------------------------------------------
    Material {
        name: "Glass",
        class: Class::Solid,
        density: 2500,
        conductivity: 10,
        heat_capacity: 84,
        melt_point: 1_400,
        melts_into: Species::Lava,
        freezes_into: Species::Glass,
        boil_point: i16::MAX,
        boils_into: Species::Lava,
        latent_fusion: 400,
        latent_vapor: 0,
        emissivity: 85,
        yield_pressure: 80, // a 2 m aquarium wall is already risky
        light: 0,
        solubility: 0,
        map_color: 0xC8_E8_EE,
    },
    // ---- 18 Metal (iron block) -------------------------------------------
    Material {
        name: "Metal",
        class: Class::Solid,
        density: 7870,
        conductivity: 800, // 80 W/(m·K)
        heat_capacity: 45, // 450 J/(kg·K)
        melt_point: 1_538,
        melts_into: Species::MoltenMetal,
        freezes_into: Species::Metal,
        boil_point: 2_862,
        boils_into: Species::MoltenMetal,
        latent_fusion: 247,
        latent_vapor: 6_090,
        emissivity: 30,
        yield_pressure: 4_000,
        light: 0,
        solubility: 0,
        map_color: 0xD8_D8_D8,
    },
    // ---- 19 Fire ---------------------------------------------------------
    Material {
        name: "Fire",
        class: Class::Gas,
        density: 1,
        conductivity: 5,
        heat_capacity: 100,
        melt_point: i16::MIN,
        melts_into: Species::Fire,
        freezes_into: Species::Air,
        boil_point: i16::MAX,
        boils_into: Species::Air,
        latent_fusion: 0,
        latent_vapor: 0,
        emissivity: 99,
        yield_pressure: u16::MAX,
        light: 15,
        solubility: 0,
        map_color: 0xFF_A5_00,
    },
    // ---- 20 Bedrock ------------------------------------------------------
    Material {
        name: "Bedrock",
        class: Class::Solid,
        density: 3000,
        conductivity: 20,
        heat_capacity: 84,
        melt_point: i16::MAX,
        melts_into: Species::Bedrock,
        freezes_into: Species::Bedrock,
        boil_point: i16::MAX,
        boils_into: Species::Bedrock,
        latent_fusion: 0,
        latent_vapor: 0,
        emissivity: 50,
        yield_pressure: u16::MAX,
        light: 0,
        solubility: 0,
        map_color: 0x28_28_28,
    },
    // ---- 21 Compressed air ----------------------------------------------
    Material {
        name: "Compressed Air",
        class: Class::Gas,
        density: 12,
        conductivity: 3,
        heat_capacity: 100,
        melt_point: i16::MIN,
        melts_into: Species::CompressedAir,
        freezes_into: Species::Air,
        boil_point: i16::MAX,
        boils_into: Species::Air,
        latent_fusion: 0,
        latent_vapor: 0,
        emissivity: 0,
        yield_pressure: u16::MAX,
        light: 0,
        solubility: 0,
        map_color: 0x60_60_70,
    },
    // ---- 22 Snow ---------------------------------------------------------
    Material {
        name: "Snow",
        class: Class::Solid,
        density: 300,
        conductivity: 1,
        heat_capacity: 209,
        melt_point: 0,
        melts_into: Species::Water,
        freezes_into: Species::Snow,
        boil_point: i16::MAX,
        boils_into: Species::Steam,
        latent_fusion: 334,
        latent_vapor: 0,
        emissivity: 98,
        yield_pressure: 20,
        light: 0,
        solubility: 0,
        map_color: 0xF4_F8_FF,
    },
    // ---- 23 Salt water ---------------------------------------------------
    Material {
        name: "Salt Water",
        class: Class::Liquid,
        density: 1025,
        conductivity: 6,
        heat_capacity: 400,
        melt_point: -2,
        melts_into: Species::SaltWater,
        freezes_into: Species::Ice,
        boil_point: 102,
        boils_into: Species::Steam,
        latent_fusion: 334,
        latent_vapor: 2257,
        emissivity: 96,
        yield_pressure: u16::MAX,
        light: 0,
        solubility: 255,
        map_color: 0x25_5A_C8,
    },
    // ---- 24 Molten metal -------------------------------------------------
    Material {
        name: "Molten Metal",
        class: Class::Liquid,
        density: 6980,
        conductivity: 350,
        heat_capacity: 82,
        melt_point: 1_538,
        melts_into: Species::MoltenMetal,
        freezes_into: Species::Metal,
        boil_point: 2_862,
        boils_into: Species::Metal,
        latent_fusion: 247,
        latent_vapor: 6_090,
        emissivity: 35,
        yield_pressure: u16::MAX,
        light: 12,
        solubility: 0,
        map_color: 0xFF_D2_6E,
    },
];

// Compile-time sanity checks on the database. If a variant is added and the
// table is not extended, the build fails here rather than at runtime.
const _: () = {
    assert!(MATERIALS.len() == SPECIES_COUNT);
    let mut i = 0;
    while i < SPECIES_COUNT {
        assert!(
            MATERIALS[i].heat_capacity > 0,
            "heat capacity must be non-zero"
        );
        assert!(MATERIALS[i].density > 0, "density must be non-zero");
        i += 1;
    }
    // Water must be able to hold the most energy per volume: it is the thermal
    // ballast of the whole simulation.
    assert!(
        MATERIALS[Species::Water as usize].heat_capacity
            > MATERIALS[Species::Stone as usize].heat_capacity
    );
};

// ---------------------------------------------------------------------------
// Flags
// ---------------------------------------------------------------------------

/// Per-voxel boolean state, packed into one byte.
pub mod flags {
    /// The voxel differs from its neighbours in mass, temperature or pressure
    /// and must be evaluated by the next kernel tick.
    pub const ACTIVE: u8 = 1 << 0;
    /// The voxel is part of a settled region; skipped by every kernel.
    pub const SLEEPING: u8 = 1 << 1;
    /// Moved during the current tick (used for render-dirty tracking).
    pub const MOVED: u8 = 1 << 2;
    /// A convective bubble is rising *through* this cell.
    pub const BUBBLE: u8 = 1 << 3;
    /// Convective buoyancy should be applied to entities inside this cell.
    pub const UPDRAFT: u8 = 1 << 4;
    /// Flash-vaporisation is queued (water ↔ lava contact).
    pub const FLASHING: u8 = 1 << 5;
    /// The cell was created by a phase transition this tick (prevents
    /// ping-ponging ice → water → ice in a single tick).
    pub const TRANSITIONED: u8 = 1 << 6;
    /// Infinite source (ocean edge, bedrock spring, creative water).
    pub const SOURCE: u8 = 1 << 7;
}

/// Aux byte layout:
/// `bits 0..=2` flow direction (`0..=5`, `7` = none) — used by the renderer to
/// orient the stream texture; `bits 3..=6` fracture damage `0..=15`; `bit 7`
/// "hot spot" marker for the bubble renderer.
pub mod aux {
    pub const FLOW_DIR_MASK: u8 = 0b0000_0111;
    pub const FLOW_NONE: u8 = 0b0000_0111;
    pub const DAMAGE_SHIFT: u8 = 3;
    pub const DAMAGE_MASK: u8 = 0b0111_1000;
    pub const HOT_SPOT: u8 = 1 << 7;

    /// Extracts the stored flow direction (or [`FLOW_NONE`]).
    #[inline]
    #[must_use]
    pub const fn flow_dir(a: u8) -> u8 {
        a & FLOW_DIR_MASK
    }

    /// Extracts the accumulated fracture damage.
    #[inline]
    #[must_use]
    pub const fn damage(a: u8) -> u8 {
        (a & DAMAGE_MASK) >> DAMAGE_SHIFT
    }

    /// Adds fracture damage, saturating at 15.
    #[inline]
    #[must_use]
    pub const fn add_damage(a: u8, amount: u8) -> u8 {
        let d = damage(a) as u16 + amount as u16;
        let d = if d > 15 { 15 } else { d as u8 };
        (a & !DAMAGE_MASK) | (d << DAMAGE_SHIFT)
    }

    /// Writes the flow direction, preserving the other fields.
    #[inline]
    #[must_use]
    pub const fn with_flow(a: u8, dir: u8) -> u8 {
        (a & !FLOW_DIR_MASK) | (dir & FLOW_DIR_MASK)
    }
}

// ---------------------------------------------------------------------------
// Voxel
// ---------------------------------------------------------------------------

/// The simulated unit. Exactly 8 bytes, `Copy`, no interior mutability.
///
/// A voxel always stores its *own* temperature; solids participate in the
/// thermal kernel so that lava can heat the basalt around it and so that an
/// ice sheet can chill the water below it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Voxel {
    /// Species wire byte.
    pub species: u8,
    /// Fluid amount, `0..=MASS_FULL`. Solids use `MASS_FULL` as "solid".
    pub mass: Mass,
    /// Temperature in °C.
    pub temp: Temp,
    /// Pressure in quarter-pascals.
    pub pressure: Press,
    /// See [`flags`].
    pub flags: u8,
    /// See [`aux`].
    pub aux: u8,
}

// The struct must stay 8 bytes; a layout regression doubles the chunk memory.
const _: () = assert!(core::mem::size_of::<Voxel>() == 8);

impl Voxel {
    /// An empty, temperate cell.
    pub const AIR: Voxel = Voxel {
        species: Species::Air as u8,
        mass: 0,
        temp: 20,
        pressure: 408, // 102 kPa ≈ 1 atm, stored in quarter-pascals
        flags: flags::SLEEPING,
        aux: 0,
    };

    /// Vacuum-ish default used for padding in the level-0 mip.
    pub const VOID: Voxel = Voxel {
        species: Species::Air as u8,
        mass: 0,
        temp: 20,
        pressure: 0,
        flags: flags::SLEEPING,
        aux: 0,
    };

    /// Creates a voxel with the given species, temperature and a full mass for
    /// solids / mass 8 for liquids.
    ///
    /// The returned cell is **not** on a work list: the `ACTIVE` flag is owned by
    /// [`crate::grid::Grid`], which sets it exactly when it pushes the cell onto
    /// its double-buffered queue (that is the invariant behind the duplicate-free
    /// work list). A constructor therefore starts `SLEEPING`, and writing the
    /// cell through [`crate::grid::Grid::set_world`] / [`crate::grid::Grid::put`]
    /// schedules it.
    #[inline]
    #[must_use]
    pub const fn new(species: Species, temp: Temp) -> Voxel {
        Voxel {
            species: species as u8,
            // Fluids are created brim-full; solids always carry MASS_FULL as a
            // "solid, not hollow" marker (see `effective_density`).
            mass: MASS_FULL,
            temp,
            pressure: 408,
            flags: flags::SLEEPING,
            aux: aux::FLOW_NONE,
        }
    }

    /// A brim-full water cell at `temp`.
    #[inline]
    #[must_use]
    pub const fn water(temp: Temp) -> Voxel {
        Voxel::new(Species::Water, temp)
    }

    /// A stone cell at `temp`.
    #[inline]
    #[must_use]
    pub const fn stone(temp: Temp) -> Voxel {
        Voxel::new(Species::Stone, temp)
    }

    /// Species accessor.
    #[inline]
    #[must_use]
    pub const fn species(&self) -> Species {
        Species::from_u8(self.species)
    }

    /// Material descriptor of this cell.
    #[inline]
    #[must_use]
    pub const fn material(&self) -> Material {
        Species::from_u8(self.species).material()
    }

    /// Physical class of this cell.
    #[inline]
    #[must_use]
    pub const fn class(&self) -> Class {
        Species::from_u8(self.species).class()
    }

    /// `true` when the cell may be moved by the hydro kernel.
    #[inline]
    #[must_use]
    pub const fn is_fluid(&self) -> bool {
        Species::from_u8(self.species).is_fluid()
    }

    /// `true` for liquids (water, lava, molten metal…).
    #[inline]
    #[must_use]
    pub const fn is_liquid(&self) -> bool {
        matches!(Species::from_u8(self.species).class(), Class::Liquid)
    }

    /// `true` for gases (steam, fire, compressed air).
    #[inline]
    #[must_use]
    pub const fn is_gas(&self) -> bool {
        matches!(Species::from_u8(self.species).class(), Class::Gas)
    }

    /// `true` for empty-like cells.
    #[inline]
    #[must_use]
    pub const fn is_empty_like(&self) -> bool {
        Species::from_u8(self.species).is_empty_like()
    }

    /// `true` when the cell is settled and may be skipped.
    #[inline]
    #[must_use]
    pub const fn is_sleeping(&self) -> bool {
        (self.flags & flags::SLEEPING) != 0 && (self.flags & flags::ACTIVE) == 0
    }

    /// Marks the cell as needing evaluation.
    #[inline]
    pub fn wake(&mut self) {
        self.flags |= flags::ACTIVE;
        self.flags &= !flags::SLEEPING;
    }

    /// Marks the cell as settled.
    #[inline]
    pub fn sleep(&mut self) {
        self.flags &= !flags::ACTIVE;
        self.flags |= flags::SLEEPING;
    }

    /// True when the given flag bit is set.
    #[inline]
    #[must_use]
    pub const fn has(&self, flag: u8) -> bool {
        (self.flags & flag) != 0
    }

    /// Sets or clears a flag bit.
    #[inline]
    pub fn set(&mut self, flag: u8, on: bool) {
        if on {
            self.flags |= flag;
        } else {
            self.flags &= !flag;
        }
    }

    /// Bulk density in `kg/m³` **as actually present in the cell**.
    ///
    /// A half-full water block weighs half as much as a full one — this is what
    /// makes Archimedes' principle fall out naturally instead of needing a
    /// special case in the buoyancy kernel.
    #[inline]
    #[must_use]
    pub fn effective_density(&self) -> u32 {
        let base = Species::from_u8(self.species).density() as u32;
        match Species::from_u8(self.species).class() {
            Class::Liquid | Class::Gas => {
                if self.mass == 0 {
                    0
                } else {
                    (base * self.mass as u32) / MASS_FULL as u32
                }
            }
            Class::Solid => base,
            Class::Void => 0,
        }
    }

    /// Volumetric heat capacity in `J/(m³·K)` — the coefficient actually used
    /// by the diffusion equation (conductivity / heat capacity cancels per cell).
    #[inline]
    #[must_use]
    pub const fn volumetric_heat_capacity(&self) -> u32 {
        let m = self.material();
        (m.density as u32 * m.heat_capacity as u32) / 10
    }

    /// Sensible heat stored in the cell, in joules (temperature above 0 °C).
    #[inline]
    #[must_use]
    pub const fn sensible_heat(&self) -> i32 {
        let m = self.material();
        let joules_per_degree = (m.density as i32 * m.heat_capacity as i32) / 10;
        // Volume is 1 m³ in engine units, so no division by volume is needed.
        let scaled = joules_per_degree / 1_000;
        scaled.saturating_mul(self.temp as i32)
    }

    /// Encodes the voxel into a `u64` for the WASM ABI (JS ↔ Rust boundary).
    #[inline]
    #[must_use]
    pub const fn to_u64(self) -> u64 {
        (self.species as u64)
            | ((self.mass as u64) << 8)
            | ((self.flags as u64) << 16)
            | ((self.aux as u64) << 24)
            | ((self.temp as u16 as u64) << 32)
            | ((self.pressure as u64) << 48)
    }

    /// Decodes a voxel written by [`Voxel::to_u64`].
    #[inline]
    #[must_use]
    pub const fn from_u64(word: u64) -> Voxel {
        Voxel {
            species: (word & 0xFF) as u8,
            mass: ((word >> 8) & 0xFF) as u8,
            flags: ((word >> 16) & 0xFF) as u8,
            aux: ((word >> 24) & 0xFF) as u8,
            temp: ((word >> 32) & 0xFFFF) as u16 as i16,
            pressure: ((word >> 48) & 0xFFFF) as u16,
        }
    }

    /// Encodes the voxel into a compact 5-byte-ish save record (species, mass,
    /// temp, pressure) for chunk persistence. Flags are recomputed on load.
    #[inline]
    #[must_use]
    pub const fn to_save(self) -> (u8, u8, i16, u16) {
        (self.species, self.mass, self.temp, self.pressure)
    }

    /// Rebuilds a voxel from [`Voxel::to_save`] output.
    #[inline]
    #[must_use]
    pub const fn from_save(species: u8, mass: u8, temp: i16, pressure: u16) -> Voxel {
        Voxel {
            species,
            mass,
            temp,
            pressure,
            // Flags are recomputed on load, exactly like the doc above promises:
            // a deserialised cell is not on a work list until the grid schedules
            // it (`Grid::load_chunk` calls `Grid::wake_all`).
            flags: flags::SLEEPING,
            aux: aux::FLOW_NONE,
        }
    }
}

/// `[Voxel::AIR; N]` as a `const`-usable helper.
///
/// Spelled out because `[Voxel::AIR; N]` inside a `const fn` still trips older
/// const-eval limitations when `N` is generic.
#[inline]
#[must_use]
pub const fn air_array<const N: usize>() -> [Voxel; N] {
    [Voxel::AIR; N]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voxel_is_eight_bytes() {
        assert_eq!(core::mem::size_of::<Voxel>(), 8);
        assert_eq!(core::mem::size_of::<[Voxel; 4096]>(), 32 * 1024);
    }

    #[test]
    fn wire_roundtrip() {
        let v = Voxel {
            species: Species::Lava as u8,
            mass: 3,
            temp: -42,
            pressure: 65_535,
            flags: 0xA5,
            aux: 0x3C,
        };
        let back = Voxel::from_u64(v.to_u64());
        assert_eq!(back, v);
    }

    #[test]
    fn save_roundtrip() {
        let v = Voxel::new(Species::Steam, 140);
        let (s, m, t, p) = v.to_save();
        let back = Voxel::from_save(s, m, t, p);
        assert_eq!(back.species(), Species::Steam);
        assert_eq!(back.temp, 140);
        assert_eq!(back.pressure, p);
    }

    #[test]
    fn species_roundtrip_all() {
        for s in Species::ALL {
            assert_eq!(Species::from_u8(s.as_u8()), s);
            assert!(!s.name().is_empty());
        }
        // Out-of-range bytes must degrade to Air instead of panicking.
        assert_eq!(Species::from_u8(200), Species::Air);
    }

    #[test]
    fn materials_are_physically_sane() {
        let water = Species::Water.material();
        let stone = Species::Stone.material();
        assert!(
            water.density < stone.density,
            "water must be lighter than stone"
        );
        assert!(
            Species::Wood.density() < Species::Water.density(),
            "wood must float"
        );
        assert!(
            Species::Pumice.density() < Species::Water.density(),
            "pumice must float"
        );
        assert!(
            Species::Metal.density() > Species::Water.density(),
            "metal must sink"
        );
        assert!(Species::Glass.yield_pressure() < Species::Stone.yield_pressure());
        // Glass bursts at ~2 blocks of water: 2 * 40 = 80 units.
        assert_eq!(Species::Glass.yield_pressure(), 80);
        // Stone holds ~35 blocks: 35 * 40 = 1 400 units.
        assert_eq!(Species::Stone.yield_pressure(), 1_400);
        assert!(Species::Leaves.yield_pressure() < Species::Plank.yield_pressure());
        assert_eq!(Species::Lava.light(), 15);
        assert!(Species::Water.material().latent_vapor > 2_000);
    }

    #[test]
    fn half_full_water_is_lighter() {
        let mut v = Voxel::water(20);
        v.mass = 4;
        assert_eq!(v.effective_density(), 500);
        v.mass = 8;
        assert_eq!(v.effective_density(), 1000);
        v.mass = 0;
        assert_eq!(v.effective_density(), 0);
    }

    #[test]
    fn solids_keep_full_density_regardless_of_mass() {
        let mut v = Voxel::stone(20);
        v.mass = 1;
        assert_eq!(v.effective_density(), 2600);
    }

    #[test]
    fn aux_packing() {
        let a = aux::with_flow(0, crate::dirs::DIR_POS_Y);
        assert_eq!(aux::flow_dir(a), crate::dirs::DIR_POS_Y);
        let a = aux::add_damage(a, 5);
        assert_eq!(aux::damage(a), 5);
        let a = aux::add_damage(a, 200);
        assert_eq!(aux::damage(a), 15, "damage must saturate");
        assert_eq!(
            aux::flow_dir(a),
            crate::dirs::DIR_POS_Y,
            "flow dir preserved"
        );
    }

    #[test]
    fn sleep_wake() {
        let mut v = Voxel::water(20);
        v.wake();
        assert!(!v.is_sleeping());
        v.sleep();
        assert!(v.is_sleeping());
        v.flags |= flags::ACTIVE;
        assert!(!v.is_sleeping(), "ACTIVE must override SLEEPING");
    }

    #[test]
    fn volumetric_capacity_orders_water_above_rock() {
        assert!(
            Voxel::water(20).volumetric_heat_capacity()
                > Voxel::stone(20).volumetric_heat_capacity()
        );
    }
}

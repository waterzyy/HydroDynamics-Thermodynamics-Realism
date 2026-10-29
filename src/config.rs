//! Runtime tunables.
//!
//! The engine parses `horizon-mod.toml` on the host side and forwards the keys it
//! cares about through [`crate::horizon_configure`], which lands in
//! [`Tunables::apply_kv`]. Keeping the schema in Rust (instead of pulling in a
//! TOML parser) is deliberate: `toml` + `serde` would add ~180 KiB to the WASM
//! module and dynamic dispatch to a code path that runs exactly once.
//!
//! Every field is an integer. Nothing here may be mutated from inside a kernel;
//! `step` takes `&Tunables`, so the compiler enforces that.

use crate::units::{Mass, Temp};

/// All gameplay- and physics-facing knobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tunables {
    // ---- world ----------------------------------------------------------
    /// Stable world seed (drives the bubble/landslide jitter).
    pub world_seed: u64,
    /// Equilibrium temperature of open air.
    pub ambient_c: Temp,
    /// Rock temperature gained per 64 blocks of depth (geothermal gradient:
    /// ~25 °C/km in reality, cranked up for a 384-block world height).
    pub rock_gradient_c: Temp,
    /// Ambient pressure at sea level, in quarter-pascals (`~101.3 kPa`).
    pub sea_level_pressure: u16,
    /// Pressure added per block of depth below sea level.
    pub pressure_per_block: u16,

    // ---- tick scheduler -------------------------------------------------
    /// Voxels evaluated per tick before the budget cut ("temporal LOD").
    pub tick_budget: u32,
    /// Fluid sub-steps per tick. 2 gives a 2× faster meniscus without
    /// destabilising the pressure solver.
    pub hydro_substeps: u8,

    // ---- hydrodynamics --------------------------------------------------
    /// Mass units transferred per voxel per substep at full head.
    pub flow_rate: Mass,
    /// Horizontal levelling rate (fraction of `flow_rate`, in 1/8ths).
    pub horizontal_rate_eighths: u8,
    /// Pressure decay per block of distance travelled horizontally (1/256ths).
    pub pressure_decay_256: u16,
    /// Velocity imparted to a jet through a breach, in 1/16 m/s per unit head.
    pub jet_velocity_per_head: i16,
    /// Maximum head used for jet velocity (clamps absurd pressures).
    pub jet_max_head: u32,
    /// Downward acceleration applied to falling fluids per tick, in 1/16 m/s.
    pub fluid_gravity: i16,
    /// Minimum excess head (in 250 Pa units) before the fracture solver is
    /// consulted at all. Pure performance gate: 40 == one block of water, so any
    /// meaningful head is checked, while a puddle costs nothing. The per-material
    /// `yield_pressure` decides what actually breaks.
    pub damage_threshold: u16,
    /// Damage applied per tick above the threshold.
    pub damage_per_tick: u8,
    /// Damage at which a block shatters (0..=15).
    pub damage_break: u8,

    // ---- thermodynamics -------------------------------------------------
    /// Weighted-relaxation coefficient for conduction, Q16 (`0..=65535`).
    pub conduction_q16: u32,
    /// Minimum |ΔT| worth integrating (°C).
    pub conduction_epsilon: u8,
    /// Sky radiation coefficient (per 10⁹ deci-kelvin units of T⁴).
    pub radiation_scale: u32,
    /// Relaxation towards ambient for exposed cells, Q16.
    pub ambient_relax_q16: u32,
    /// Water boiling point at sea level.
    pub boil_c: Temp,
    /// How much the boiling point rises per 1/4 kPa of pressure (1/100 °C units).
    pub boil_pressure_slope: i16,
    /// Temperature at which steam turns back into water at 1 atm.
    pub condense_c: Temp,
    /// A ceiling colder than this condenses steam quickly (drips form).
    pub cold_ceiling_c: Temp,
    /// Steam mass converted per tick when condensing (×1/4 units).
    pub condense_quarter_units: u8,
    /// Ice forming below this temperature (allows supercooled water).
    pub freeze_c: Temp,
    /// Temperature above which ice melts.
    pub melt_c: Temp,
    /// Heat released when 1 mass unit freezes (°C applied to the cell).
    pub latent_freeze_c: i16,
    /// Heat absorbed when 1 mass unit melts (°C).
    pub latent_melt_c: i16,
    /// Water temperature above which convection bubbles nucleate.
    pub bubble_c: Temp,
    /// Upward velocity granted to entities in a bubble column (1/16 m/s).
    pub bubble_lift: i16,
    /// Steam pressure added when water flashes on lava (quarter-pascals).
    pub flash_pressure: u16,
    /// Radius (in blocks) of the quench micro-explosion.
    pub flash_radius: u8,
    /// Mass units of water turned into steam per flash.
    pub flash_mass: Mass,
    /// Temperature of freshly formed obsidian.
    pub obsidian_c: Temp,
    /// Temperature of freshly formed pumice.
    pub pumice_c: Temp,
    /// Water temperature needed to quench lava into obsidian (vs. pumice).
    pub quench_hot_c: Temp,
    /// Wood/leaves self-ignition temperature.
    pub ignition_c: Temp,
    /// Lava solidus: below this, lava becomes basalt.
    pub basalt_c: Temp,

    // ---- presentation / book -------------------------------------------
    /// Version string embedded in the generated Atlas book.
    pub book_version: &'static str,
    /// Grant the Atlas on first join / respawn.
    pub give_book_on_join: bool,
    /// Grant the Atlas on respawn.
    pub give_book_on_respawn: bool,
}

impl Default for Tunables {
    fn default() -> Self {
        Tunables::DEFAULT
    }
}


impl Tunables {
    /// Gentler preset for low-end servers ("potato" profile in the mod config).
    #[must_use]
    pub const fn low_end() -> Tunables {
        let mut t = Tunables::DEFAULT;
        t.tick_budget = 1_536;
        t.hydro_substeps = 1;
        t.flash_radius = 1;
        t
    }

    /// High-fidelity preset for creative/showcase worlds.
    #[must_use]
    pub const fn high_fidelity() -> Tunables {
        let mut t = Tunables::DEFAULT;
        t.tick_budget = 12_288;
        t.hydro_substeps = 3;
        t.conduction_q16 = 28_000;
        t.flash_radius = 3;
        t
    }

    /// The default preset, usable in `const` position.
    pub const DEFAULT: Tunables = Tunables {
        world_seed: 0x48_59_44_52_4F_4E_41_55,
        ambient_c: 20,
        rock_gradient_c: 6,
        sea_level_pressure: 405,
        pressure_per_block: 40,
        tick_budget: 4096,
        hydro_substeps: 2,
        flow_rate: 4,
        horizontal_rate_eighths: 8,
        pressure_decay_256: 12,
        jet_velocity_per_head: 3,
        jet_max_head: 4_000,
        fluid_gravity: 24,
        damage_threshold: 40,
        damage_per_tick: 2,
        damage_break: 12,
        conduction_q16: 21_000,
        conduction_epsilon: 1,
        radiation_scale: 96,
        ambient_relax_q16: 900,
        boil_c: 100,
        boil_pressure_slope: 1,
        condense_c: 95,
        cold_ceiling_c: 45,
        condense_quarter_units: 2,
        freeze_c: -1,
        melt_c: 1,
        latent_freeze_c: 2,
        latent_melt_c: 2,
        bubble_c: 55,
        bubble_lift: 96,
        flash_pressure: 24_000,
        flash_radius: 2,
        flash_mass: 2,
        obsidian_c: 180,
        pumice_c: 320,
        quench_hot_c: 60,
        ignition_c: 320,
        basalt_c: 700,
        book_version: "1.4.0",
        give_book_on_join: true,
        give_book_on_respawn: true,
    };

    /// Parses `key = value` pairs coming from the host TOML bridge.
    ///
    /// Returns `true` when the key was recognised. Values are plain decimal
    /// integers or `true`/`false`; anything else is ignored so a typo in the
    /// config can never crash the sandbox.
    pub fn apply_kv(&mut self, key: &str, value: &str) -> bool {
        let raw = value.trim();
        // Boolean keys first, so that "0"/"1" keep working for flags.
        let bv = match raw {
            "true" | "yes" | "on" => Some(true),
            "false" | "no" | "off" => Some(false),
            _ => None,
        };
        match key {
            "book.give_on_join" => {
                if let Some(b) = bv {
                    self.give_book_on_join = b;
                }
                true
            }
            "book.give_on_respawn" => {
                if let Some(b) = bv {
                    self.give_book_on_respawn = b;
                }
                true
            }
            _ => {
                // Everything else is numeric. An unparsable value is a no-op.
                let i: i64 = match raw.parse() {
                    Ok(v) => v,
                    Err(_) => return false,
                };
                let clamp16 = |v: i64| -> Temp {
                    if v < -273 {
                        -273
                    } else if v > 2000 {
                        2000
                    } else {
                        v as Temp
                    }
                };
                match key {
                    "world.world_seed" => self.world_seed = i as u64,
                    "thermo.ambient_c" => self.ambient_c = clamp16(i),
                    "world.rock_gradient_c" => self.rock_gradient_c = clamp16(i),
                    "world.sea_level_pressure" => {
                        self.sea_level_pressure = i.clamp(0, 65_535) as u16
                    }
                    "world.pressure_per_block" => {
                        self.pressure_per_block = i.clamp(0, 65_535) as u16
                    }
                    "perf.tick_budget" => self.tick_budget = i.clamp(1, 65_535) as u32,
                    "perf.hydro_substeps" => self.hydro_substeps = i.clamp(1, 4) as u8,
                    "hydro.flow_rate" => self.flow_rate = i.clamp(1, 8) as Mass,
                    "hydro.horizontal_rate_eighths" => {
                        self.horizontal_rate_eighths = i.clamp(1, 8) as u8
                    }
                    "hydro.pressure_decay_256" => self.pressure_decay_256 = i.clamp(0, 255) as u16,
                    "hydro.jet_velocity_per_head" => {
                        self.jet_velocity_per_head = i.clamp(0, 1_000) as i16
                    }
                    "hydro.damage_threshold" => self.damage_threshold = i.clamp(0, 65_535) as u16,
                    "hydro.damage_break" => self.damage_break = i.clamp(1, 15) as u8,
                    "thermo.conduction_q16" => self.conduction_q16 = i.clamp(0, 65_535) as u32,
                    "thermo.radiation_scale" => self.radiation_scale = i.clamp(0, 1_000_000) as u32,
                    "thermo.ambient_relax_q16" => {
                        self.ambient_relax_q16 = i.clamp(0, 65_535) as u32
                    }
                    "thermo.boil_c" => self.boil_c = clamp16(i),
                    "thermo.condense_c" => self.condense_c = clamp16(i),
                    "thermo.cold_ceiling_c" => self.cold_ceiling_c = clamp16(i),
                    "thermo.freeze_c" => self.freeze_c = clamp16(i),
                    "thermo.melt_c" => self.melt_c = clamp16(i),
                    "thermo.bubble_c" => self.bubble_c = clamp16(i),
                    "thermo.flash_pressure" => self.flash_pressure = i.clamp(0, 65_535) as u16,
                    "thermo.flash_radius" => self.flash_radius = i.clamp(0, 8) as u8,
                    "thermo.ignition_c" => self.ignition_c = clamp16(i),
                    "thermo.basalt_c" => self.basalt_c = clamp16(i),
                    _ => return false,
                }
                true
            }
        }
    }

    /// Clamps every field into a physically sane range (called after config
    /// loading so a hostile datapack cannot produce e.g. `conduction == 100 %`).
    pub fn sanitize(&mut self) {
        self.tick_budget = self.tick_budget.clamp(1, 65_535);
        self.hydro_substeps = self.hydro_substeps.clamp(1, 4);
        self.conduction_q16 = self.conduction_q16.min(32_000); // ≤ 0.5 → stable
        self.ambient_relax_q16 = self.ambient_relax_q16.min(4_000);
        self.flow_rate = self.flow_rate.clamp(1, 8);
        self.damage_break = self.damage_break.clamp(1, 15);
        self.flash_radius = self.flash_radius.min(8);
        if self.condense_c < self.cold_ceiling_c {
            self.condense_c = self.cold_ceiling_c;
        }
        if self.boil_c <= self.melt_c {
            self.boil_c = self.melt_c + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_sane() {
        let mut t = Tunables::default();
        t.sanitize();
        assert_eq!(t, Tunables::DEFAULT);
        assert_eq!(Tunables::DEFAULT.ambient_c, 20);
    }

    #[test]
    fn kv_bridge() {
        let mut t = Tunables::DEFAULT;
        assert!(t.apply_kv("thermo.boil_c", "120"));
        assert_eq!(t.boil_c, 120);
        assert!(t.apply_kv("book.give_on_join", "false"));
        assert!(!t.give_book_on_join);
        assert!(!t.apply_kv("nope.nothing", "1"), "unknown keys are rejected");
        assert!(!t.apply_kv("thermo.boil_c", "not-a-number"));
        assert_eq!(t.boil_c, 120, "an unparsable value does not corrupt the field");
    }

    #[test]
    fn sanitize_clamps_hostile_values() {
        let mut t = Tunables::DEFAULT;
        t.conduction_q16 = 65_000; // would explode the explicit solver
        t.hydro_substeps = 99;
        t.damage_break = 0;
        t.sanitize();
        assert!(t.conduction_q16 <= 32_000);
        assert_eq!(t.hydro_substeps, 4);
        assert_eq!(t.damage_break, 1);
    }

    #[test]
    fn presets_differ() {
        assert!(Tunables::low_end().tick_budget < Tunables::DEFAULT.tick_budget);
        assert!(Tunables::high_fidelity().tick_budget > Tunables::DEFAULT.tick_budget);
        let mut hi = Tunables::high_fidelity();
        hi.sanitize();
    }
}

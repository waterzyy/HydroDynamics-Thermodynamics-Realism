//! What the fluid does to an entity: buoyancy, updraft, drag, jet push and the
//! compression damage of a deep dive.
//!
//! The engine calls these a handful of times per tick, so they may use richer
//! arithmetic than the voxel kernels. Results are integers in the engine's
//! 1/16 m/s units.

use crate::config::Tunables;
use crate::grid::Grid;
use crate::hydro;
use crate::units::{Vel, GRAVITY_Q4};
use crate::voxel::{flags, Species, Voxel};

/// Density of a boat or a block in `kg/m³` (wood hulls are ~700).
pub const DENSITY_WOOD: u32 = 700;
/// Density of an iron-armoured player with a full inventory.
pub const DENSITY_ARMOURED_PLAYER: u32 = 2_100;
/// Density of a swimmer in leather with a mostly empty inventory.
pub const DENSITY_SWIMMER: u32 = 1_050;

/// Vertical acceleration applied to an entity standing in a fluid cell.
///
/// Combines Archimedes' principle with the convective `UPDRAFT` flag and the
/// vertical component of a jet:
///
/// ```text
/// a = max( g·(ρ_fluid − ρ_entity)/ρ_entity ,  updraft )  +  jet_y
/// ```
///
/// * a plank (700 kg/m³) in water: `+0.42 g` -> floats to the surface;
/// * an armoured player (2 100 kg/m³): `−0.52 g` -> sinks and must swim up;
/// * a lava pool (2 600 kg/m³) floats even a stone golem, lava is dense.
///
/// The updraft of a bubble column is entrainment, not buoyancy: a two-phase
/// column drags whatever is inside it along with the vapour, so the terminal
/// velocity of an object in it is set by the *flow*, not by the object's density.
/// That is why it is applied as a floor (`max`) rather than an addition, an
/// iron-armoured diver in a boiling cauldron is carried up, exactly as the Atlas
/// describes, while a swimmer in still water still sinks.
#[must_use]
pub fn vertical_acceleration(
    grid: &Grid,
    x: i32,
    y: i32,
    z: i32,
    entity_density: u32,
    t: &Tunables,
) -> Vel {
    let cell = match grid.get_world(x, y, z) {
        Some(v) => v,
        None => return 0,
    };
    let fluid_density = if cell.is_fluid() && cell.mass > 0 {
        cell.effective_density()
    } else {
        0
    };
    let mut a = hydro::buoyancy(entity_density, fluid_density);
    if fluid_density == 0 {
        // Not submerged: wind/air does not lift, but a bubble stream still does.
        a = 0;
    }
    if cell.flags & flags::UPDRAFT != 0 {
        // Entrainment: the column's own velocity dominates the density term.
        a = a.max(t.bubble_lift as i32);
    }
    let (coords, idx) = Grid::world_to_local(x, y, z);
    if let Some(slot) = grid.find(coords) {
        let jet = hydro::jet_velocity(grid, slot, idx, t);
        a += jet[1] as i32;
    }
    clamp_vel(a)
}

#[inline]
fn clamp_vel(v: i32) -> Vel {
    if v > Vel::MAX as i32 {
        Vel::MAX
    } else if v < Vel::MIN as i32 {
        Vel::MIN
    } else {
        v as Vel
    }
}

/// Horizontal push an entity receives from a jet, in 1/16 m/s.
#[must_use]
pub fn jet_push(grid: &Grid, x: i32, y: i32, z: i32, t: &Tunables) -> [Vel; 3] {
    let (coords, idx) = Grid::world_to_local(x, y, z);
    match grid.find(coords) {
        Some(slot) => hydro::jet_velocity(grid, slot, idx, t),
        None => [0; 3],
    }
}

/// Drag coefficient in 1/256ths applied to an entity's velocity per tick while
/// submerged. Water is ~800× denser than air, so `200` (≈22 % per tick) is a
/// *gentle* water drag and air gets `4`.
#[must_use]
pub fn drag_256(grid: &Grid, x: i32, y: i32, z: i32) -> u16 {
    match grid.get_world(x, y, z) {
        Some(v) if v.is_fluid() && v.mass > 0 => {
            if v.is_gas() {
                8
            } else {
                180
            }
        }
        _ => 4,
    }
}

/// Applies `drag_256` to a velocity vector.
#[must_use]
pub fn apply_drag(v: [Vel; 3], drag_256: u16) -> [Vel; 3] {
    let keep = 256u32.saturating_sub(drag_256.min(256) as u32);
    let f = |c: Vel| -> Vel {
        let r = (c as i32 * keep as i32) / 256;
        clamp_vel(r)
    };
    [f(v[0]), f(v[1]), f(v[2])]
}

/// Depth in metres below the nearest free surface at the entity's column.
#[must_use]
pub fn submersion_depth(grid: &Grid, x: i32, y: i32, z: i32) -> u16 {
    let mut depth = 0u16;
    let mut yy = y;
    for _ in 0..512 {
        match grid.get_world(x, yy, z) {
            Some(v) if v.is_fluid() && v.mass > 0 && !v.is_gas() => {
                depth += 1;
                yy += 1;
            }
            _ => break,
        }
    }
    depth
}

/// Compression injuries: damage (in half-hearts) per second at `pressure`.
///
/// Humans equalise their sinuses to ~3 atm; below that nothing happens. Above
/// it, barotrauma accumulates quadratically, exactly as the Atlas describes:
/// 30 m is unpleasant, 60 m kills an unarmoured diver in seconds.
#[must_use]
pub fn compression_damage_per_second(pressure: u16, t: &Tunables) -> u16 {
    let per_block = t.pressure_per_block.max(1) as u32;
    let metres = (pressure.saturating_sub(t.sea_level_pressure) as u32) / per_block;
    if metres <= 24 {
        return 0;
    }
    let over = metres - 24;
    ((over * over) / 18).min(40) as u16
}

/// `true` when an entity can breathe here (a gas cell that is not steam).
#[must_use]
pub fn breathable(grid: &Grid, x: i32, y: i32, z: i32) -> bool {
    match grid.get_world(x, y, z) {
        Some(Voxel { species, mass, .. }) => {
            let s = Species::from_u8(species);
            mass == 0 || s == Species::Air
        }
        None => true,
    }
}

/// Terminal fall speed of a loose block in a fluid, in 1/16 m/s.
///
/// Derived from the balance of gravity, buoyancy and quadratic drag:
/// `v_t = sqrt(2·g·(ρ_b − ρ_f) / (C_d·A·ρ_f))`, collapsed into integers.
#[must_use]
pub fn terminal_velocity(block_density: u32, fluid_density: u32, t: &Tunables) -> Vel {
    let _ = t;
    let net = hydro::buoyancy(block_density, fluid_density.max(1));
    if net >= 0 {
        return 0; // it floats
    }
    let speed = crate::units::isqrt_u32((-net as u32) * 64);
    clamp_vel(speed as i32)
}

// ---------------------------------------------------------------------------
// The packed ABI view
// ---------------------------------------------------------------------------

/// Bit flags of [`EntityForces::flags`], mirrored to the host in the packed word.
pub mod force_flags {
    /// The entity is inside a liquid cell (Archimedes applies).
    pub const SUBMERGED: u8 = 1 << 0;
    /// A convective bubble column is carrying the entity upwards.
    pub const UPDRAFT: u8 = 1 << 1;
    /// The cell holds breathable gas.
    pub const BREATHABLE: u8 = 1 << 2;
    /// The entity is lighter than the fluid, so it will surface on its own.
    pub const POSITIVE_BUOYANCY: u8 = 1 << 3;
}

/// Everything the host's entity integrator needs from the fluid field, gathered
/// in one pass so that a query costs one voxel read per axis rather than six
/// separate exported calls.
///
/// This is the ABI surface behind `horizon_entity_forces` /
/// `horizon_entity_status`; the fields are already in the engine's units
/// ("1/16 m/s" for velocities, half-hearts per second for damage), so the host
/// does not have to know any of the mod's internal fixed-point scales.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EntityForces {
    /// Vertical acceleration, jet included. Positive = up.
    pub accel: Vel,
    /// Horizontal jet push along X (`+X` east).
    pub push_x: Vel,
    /// Horizontal jet push along Z (`+Z` south).
    pub push_z: Vel,
    /// Velocity damping in 1/256ths per tick (`0..=255`).
    pub drag: u16,
    /// Depth below the free surface, in blocks.
    pub depth: u16,
    /// Compression (barotrauma) damage, in half-hearts per second.
    pub damage_per_second: u16,
    /// See [`force_flags`].
    pub flags: u8,
}

impl EntityForces {
    /// Packs the word returned by `horizon_entity_forces`:
    ///
    /// ```text
    /// bits  0..16  accel            (i16, 1/16 m/s²)
    /// bits 16..32  push_x           (i16, 1/16 m/s)
    /// bits 32..48  push_z           (i16, 1/16 m/s)
    /// bits 48..56  drag             (u8, 1/256 per tick)
    /// bits 56..64  flags            (u8, see `force_flags`)
    /// ```
    #[must_use]
    pub fn to_u64(self) -> u64 {
        let accel = self.accel as u16 as u64;
        let px = self.push_x as u16 as u64;
        let pz = self.push_z as u16 as u64;
        let drag = self.drag.min(255) as u64;
        accel | (px << 16) | (pz << 32) | (drag << 48) | ((self.flags as u64) << 56)
    }

    /// Inverse of [`EntityForces::to_u64`].
    #[must_use]
    pub fn from_u64(word: u64) -> EntityForces {
        EntityForces {
            accel: (word & 0xFFFF) as u16 as i16,
            push_x: ((word >> 16) & 0xFFFF) as u16 as i16,
            push_z: ((word >> 32) & 0xFFFF) as u16 as i16,
            drag: ((word >> 48) & 0xFF) as u16,
            depth: 0,
            damage_per_second: 0,
            flags: (word >> 56) as u8,
        }
    }

    /// Packs the word returned by `horizon_entity_status`:
    /// `depth | damage_per_second << 16`.
    #[must_use]
    pub fn to_status_u32(self) -> u32 {
        u32::from(self.depth) | (u32::from(self.damage_per_second) << 16)
    }

    /// Inverse of [`EntityForces::to_status_u32`].
    #[must_use]
    pub fn from_status_u32(word: u32) -> (u16, u16) {
        ((word & 0xFFFF) as u16, (word >> 16) as u16)
    }
}

/// Gathers the forces acting on an entity of the given density standing at
/// `(x, y, z)`.
///
/// The four effects the Atlas promises the player are resolved here, in the
/// order they happen physically: buoyancy against the surrounding fluid, the
/// entrainment floor of a bubble column, the horizontal push of a jet, and the
/// damping that fluid applies to motion. Compression damage only depends on the
/// hydrostatic head, so it is read from the column rather than accumulated.
#[must_use]
pub fn query(
    grid: &Grid,
    x: i32,
    y: i32,
    z: i32,
    entity_density: u32,
    t: &Tunables,
) -> EntityForces {
    let cell = match grid.get_world(x, y, z) {
        Some(v) => v,
        None => {
            return EntityForces {
                flags: force_flags::BREATHABLE,
                ..EntityForces::default()
            }
        }
    };
    let liquid = cell.is_fluid() && cell.mass > 0 && !cell.is_gas();
    let fluid_density = if cell.is_fluid() && cell.mass > 0 {
        cell.effective_density()
    } else {
        0
    };

    let mut out = EntityForces {
        drag: drag_256(grid, x, y, z),
        ..EntityForces::default()
    };
    if fluid_density > 0 {
        out.accel = clamp_vel(hydro::buoyancy(entity_density, fluid_density));
    }
    if liquid {
        out.flags |= force_flags::SUBMERGED;
        out.depth = submersion_depth(grid, x, y, z);
        let pressure = max_pressure_in_column(grid, x, y, z).max(cell.pressure);
        out.damage_per_second = compression_damage_per_second(pressure, t);
    }
    if cell.flags & flags::UPDRAFT != 0 || cell.flags & flags::BUBBLE != 0 {
        // Entrainment: the column's own velocity dominates the density term.
        out.flags |= force_flags::UPDRAFT;
        if out.accel < t.bubble_lift {
            out.accel = t.bubble_lift;
        }
    }
    if entity_density < fluid_density {
        out.flags |= force_flags::POSITIVE_BUOYANCY;
    }
    if breathable(grid, x, y, z) {
        out.flags |= force_flags::BREATHABLE;
    }

    let (coords, idx) = Grid::world_to_local(x, y, z);
    if let Some(slot) = grid.find(coords) {
        let jet = hydro::jet_velocity(grid, slot, idx, t);
        out.accel = clamp_vel(out.accel as i32 + jet[1] as i32);
        out.push_x = jet[0];
        out.push_z = jet[2];
    }
    out
}

/// Highest hydrostatic pressure in the water column that contains `(x, y, z)`.
///
/// Damage is dealt by the pressure *at the entity*, which for a diver is the
/// head of everything above them; reading the cell's own field is therefore
/// enough once the column has settled, but taking the max over the column makes
/// the value correct on the very tick the diver enters, before the pressure
/// field has propagated down to them.
fn max_pressure_in_column(grid: &Grid, x: i32, y: i32, z: i32) -> u16 {
    let mut best = 0u16;
    let mut yy = y;
    for _ in 0..512 {
        match grid.get_world(x, yy, z) {
            Some(v) if v.is_fluid() && v.mass > 0 => {
                if v.pressure > best {
                    best = v.pressure;
                }
                yy += 1;
            }
            _ => break,
        }
    }
    best
}

/// Gravity constant exposed for the entity integrator.
pub const GRAVITY: i32 = GRAVITY_Q4;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirs;

    fn world() -> (Grid, Tunables) {
        let mut g = Grid::with_capacity(5, 2);
        g.ensure_chunk([0, 0, 0]);
        (g, Tunables::DEFAULT)
    }

    #[test]
    fn wood_floats_up_and_armour_sinks() {
        let (mut g, t) = world();
        for y in 0..8 {
            g.set_world(3, y, 3, Voxel::water(20));
        }
        let wood = vertical_acceleration(&g, 3, 4, 3, DENSITY_WOOD, &t);
        let iron = vertical_acceleration(&g, 3, 4, 3, DENSITY_ARMOURED_PLAYER, &t);
        assert!(wood > 0, "wood must be pushed up, got {wood}");
        assert!(iron < 0, "armour must sink, got {iron}");
    }

    #[test]
    fn dry_air_has_no_buoyancy() {
        let (g, t) = world();
        assert_eq!(vertical_acceleration(&g, 1, 1, 1, DENSITY_WOOD, &t), 0);
    }

    #[test]
    fn bubbles_lift_entities() {
        let (mut g, t) = world();
        let mut w = Voxel::water(60);
        w.flags |= flags::UPDRAFT;
        g.set_world(3, 3, 3, w);
        let a = vertical_acceleration(&g, 3, 3, 3, 9_000, &t);
        assert!(a > 0, "an updraft must lift even iron: {a}");
    }

    #[test]
    fn compression_damage_curve() {
        let t = Tunables::DEFAULT;
        let at = |m: u32| t.sea_level_pressure + (m as u16) * t.pressure_per_block;
        assert_eq!(compression_damage_per_second(at(10), &t), 0);
        assert_eq!(compression_damage_per_second(at(24), &t), 0);
        let shallow = compression_damage_per_second(at(40), &t);
        let deep = compression_damage_per_second(at(80), &t);
        assert!(shallow > 0);
        assert!(deep > shallow, "damage must grow faster than linearly");
        assert!(deep <= 40);
    }

    #[test]
    fn submersion_depth_counts_upward() {
        let (mut g, _t) = world();
        for y in 2..8 {
            g.set_world(2, y, 2, Voxel::water(20));
        }
        assert_eq!(submersion_depth(&g, 2, 2, 2), 6);
        assert_eq!(submersion_depth(&g, 2, 8, 2), 0);
    }

    #[test]
    fn drag_is_strong_in_water_weak_in_air() {
        let (mut g, _t) = world();
        g.set_world(1, 1, 1, Voxel::water(20));
        let wet = drag_256(&g, 1, 1, 1);
        let dry = drag_256(&g, 5, 5, 5);
        assert!(wet > dry * 10);
        let v = apply_drag([160, 0, -160], wet);
        assert!(v[0] < 160 && v[0] > 0, "{v:?}");
        assert_eq!(v[1], 0);
    }

    #[test]
    fn terminal_velocity_of_a_sinking_block() {
        let t = Tunables::DEFAULT;
        assert!(terminal_velocity(7_870, 1_000, &t) > 0, "iron sinks");
        assert_eq!(terminal_velocity(700, 1_000, &t), 0, "wood floats");
    }

    #[test]
    fn breathability() {
        let (mut g, _t) = world();
        g.set_world(1, 1, 1, Voxel::water(20));
        g.set_world(2, 1, 1, Voxel::AIR);
        assert!(!breathable(&g, 1, 1, 1));
        assert!(breathable(&g, 2, 1, 1));
        assert!(breathable(&g, 9, 9, 9), "unloaded space is breathable");
    }

    #[test]
    fn jet_push_reads_the_flow_hint() {
        let (mut g, t) = world();
        let mut w = Voxel::water(60);
        w.pressure = t.sea_level_pressure + t.pressure_per_block * 6;
        w.aux = crate::voxel::aux::with_flow(0, dirs::DIR_POS_Z);
        g.set_world(4, 4, 4, w);
        let push = jet_push(&g, 4, 4, 4, &t);
        assert!(push[2] > 0, "jet must push along +Z: {push:?}");
    }
}

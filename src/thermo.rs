//! Heat flow and phase changes: conduction, radiation, geothermal warming, and
//! the reactions between water and lava.
//!
//! Temperature is stored in whole degrees Celsius. Conduction relaxes two cells
//! towards the same heat-capacity-weighted equilibrium, which keeps the total
//! energy constant to within rounding and gives the kernel a fixed point to
//! settle into.
//!
//! Phase changes preserve mass: boiling moves units into the vapour, condensing
//! moves them back, and freezing keeps the thickness of the water it came from.

use crate::config::Tunables;
use crate::dirs::{self, Dir, DIR_NEG_Y, DIR_POS_Y};
use crate::grid::Grid;
use crate::units::{
    abs_i32, clamp_temp, deci_kelvin, isqrt_u32, mul_div_i32, Press, Temp, MASS_FULL,
};
use crate::voxel::{aux, flags, Class, Species, Voxel};

/// Directions used for pairwise conduction. Each unordered pair is visited
/// exactly once (the other three directions are covered by the neighbour).
pub const HALF_DIRS: [Dir; 3] = [crate::dirs::DIR_POS_X, DIR_POS_Y, crate::dirs::DIR_POS_Z];

/// Reference volumetric heat capacity used to convert a radiated energy flux
/// into a temperature delta (roughly the mean of the block database).
const REF_CAPACITY: i64 = 30_000;

/// Divisor of the Stefan-Boltzmann term; calibrated so that 2 000 °C lava loses
/// ≈ 2 °C/tick to a 20 °C sky and 100 °C water loses ≈ 0.
const RADIATION_DIV: i64 = 10_000_000_000;

/// Maximum number of cells a steam bubble may shift when it displaces water.
const MAX_BUBBLE_CHAIN: usize = 4;

/// Largest temperature step a single gas cell may take towards the ambient in
/// one tick (°C). See the comment in [`relax_surface`]: it keeps a vapour plume
/// coherent long enough to travel, which is what the condensation/steam-engine
/// gameplay is built on.
const GAS_RELAX_STEP_C: i64 = 8;

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Something the engine (or the Atlas/achievements layer) may want to render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Water crystallised; `mass` is the ice thickness in 1/8 blocks.
    Freeze { slot: usize, idx: usize, mass: u8 },
    /// Ice turned back into water.
    Melt { slot: usize, idx: usize, mass: u8 },
    /// Water flashed into steam.
    Boil { slot: usize, idx: usize, mass: u8 },
    /// Steam condensed on a ceiling.
    Condense { slot: usize, idx: usize, mass: u8 },
    /// Lava was quenched; `result` is what it became.
    Quench {
        slot: usize,
        idx: usize,
        result: Species,
    },
    /// A brittle block was destroyed by hydrostatic pressure.
    Shatter {
        slot: usize,
        idx: usize,
        species: Species,
    },
    /// A flammable block caught fire.
    Ignite { slot: usize, idx: usize },
}

/// Result of evaluating one cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellResult {
    /// Any state changed (the caller must mark the chunk dirty / wake neighbours).
    pub changed: bool,
    /// The cell must stay on the active list for the next tick.
    pub active: bool,
    /// Optional event for the presentation layer.
    pub event: Option<Event>,
    /// Pressure injected into the cell this tick (quench explosions).
    pub pressure_spike: u16,
}

impl CellResult {
    /// A no-op result.
    #[must_use]
    pub const fn idle() -> CellResult {
        CellResult {
            changed: false,
            active: false,
            event: None,
            pressure_spike: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Pressure / temperature environment
// ---------------------------------------------------------------------------

/// Ambient pressure at a given depth below sea level.
///
/// `depth <= 0` returns sea-level pressure. Used by worldgen and by the
/// "compression injuries" rule the Atlas describes: the fluid kernel stores the
/// same value in every submerged cell's `pressure` field.
#[must_use]
pub fn pressure_at_depth(depth: i32, t: &Tunables) -> Press {
    if depth <= 0 {
        return t.sea_level_pressure;
    }
    let extra = (depth as i64) * (t.pressure_per_block as i64);
    let total = t.sea_level_pressure as i64 + extra;
    if total > Press::MAX as i64 {
        Press::MAX
    } else {
        total as Press
    }
}

/// Geothermal temperature at a given depth below sea level.
#[must_use]
pub fn temperature_at_depth(depth: i32, t: &Tunables) -> Temp {
    if depth <= 0 {
        return t.ambient_c;
    }
    let d = (depth / 64) * (t.rock_gradient_c as i32);
    clamp_temp(t.ambient_c as i32 + d)
}

/// Boiling point of water at `press`, in °C.
///
/// Integer Clausius–Clapeyron: every doubling of pressure lifts the boiling
/// point by ≈ 28 °C, linearised through an exact integer `log2`. At 2 atm water
/// boils at ~128 °C, at 0.1 atm at ~40 °C, enough for pressure cookers and for
/// "the kettle boils in the vacuum of a sealed shaft" gameplay.
#[must_use]
pub fn boil_point_c(press: Press, t: &Tunables) -> Temp {
    let sea = t.sea_level_pressure.max(1) as i32;
    let p = press.max(1) as i32;
    let l2_q8 = log2_q8((p * 256) / sea.max(1));
    let delta = (l2_q8 as i64 * 28) / 256;
    // Not below 10 °C: below that the model is out of its validity range and a
    // 0 °C boiling point would break the freeze/boil hysteresis.
    let v = t.boil_c as i64 + delta;
    if v < 10 {
        10
    } else if v > 1_600 {
        1_600
    } else {
        v as Temp
    }
}

/// Condensation point of steam at `press`, in °C.
///
/// 5 °C of hysteresis below the boiling point keeps a boiling cell from
/// condensing on the very tick it evaporated.
#[must_use]
pub fn condense_point_c(press: Press, t: &Tunables) -> Temp {
    let b = boil_point_c(press, t) as i32 - 5;
    let v = if b < t.condense_c as i32 {
        t.condense_c as i32
    } else {
        b
    };
    // Never above the configured condense ceiling.
    if v > t.condense_c as i32 + 40 {
        t.condense_c + 40
    } else {
        v as Temp
    }
}

/// Integer `log2` in Q8 (256 == 1.0). Linear interpolation inside each octave is
/// accurate to ~0.08 octaves, plenty for a boiling-point curve.
#[must_use]
pub fn log2_q8(x: i32) -> i32 {
    if x <= 0 {
        return -8 * 256;
    }
    let mut v = x;
    let mut e: i32 = 0;
    while v >= 512 {
        v >>= 1;
        e += 1;
    }
    while v < 256 {
        v <<= 1;
        e -= 1;
    }
    e * 256 + (v - 256)
}

// ---------------------------------------------------------------------------
// Numeric helpers
// ---------------------------------------------------------------------------

/// Rounds `v / d` half away from zero (`d > 0`).
#[inline]
fn round_div(v: i64, d: i64) -> i64 {
    if d <= 0 {
        return 0;
    }
    if v >= 0 {
        (v + d / 2) / d
    } else {
        -((-v + d / 2) / d)
    }
}

#[inline]
fn sign64(v: i64) -> i64 {
    if v > 0 {
        1
    } else if v < 0 {
        -1
    } else {
        0
    }
}

/// Maps a conductivity (`W/(m·K)·10`) to a relaxation factor in Q8.
///
/// A linear map would give water `6/800 = 0.008` and make a lava-heated pool take
/// hours to warm. Compressing with a square root keeps a 100× conductivity spread
/// a 4× rate spread, fast enough to *feel* right, still physically ordered
/// (metal > stone > water > wood).
#[must_use]
pub fn conductivity_factor_q8(k: u16) -> i32 {
    let f = 48 + (isqrt_u32(k as u32 * 4_096) / 4) as i32;
    f.min(256)
}

// ---------------------------------------------------------------------------
// Per-cell kernel
// ---------------------------------------------------------------------------

/// Advances one voxel by one tick.
///
/// The caller (see [`crate::Sim::tick`]) is responsible for putting the cell
/// on the active list and for re-queueing it when `active == true`.
pub fn tick_cell(grid: &mut Grid, slot: usize, idx: usize, t: &Tunables) -> CellResult {
    if slot >= grid.chunks.len() {
        return CellResult::idle();
    }
    let ambient = grid.chunks[slot].ambient;
    let me = grid.voxel(slot, idx);

    // Air that already matches the ambient is the overwhelming majority of all
    // cells in a world. Bail out before touching anything.
    if me.species == Species::Air as u8 && abs_i32(me.temp as i32 - ambient as i32) <= 2 {
        return CellResult::idle();
    }

    let mut changed = false;

    // ---- 1. conduction -------------------------------------------------
    for d in HALF_DIRS {
        changed |= conduction_pair(grid, slot, idx, d, t);
    }

    // ---- 2. radiation + ambient relaxation -----------------------------
    changed |= relax_surface(grid, slot, idx, t);

    let me2 = grid.voxel(slot, idx);

    // ---- 3. phase transitions ------------------------------------------
    let mut event = None;
    let mut pressure_spike = 0u16;
    if let Some((ev, spike)) = apply_phase(grid, slot, idx, t) {
        changed = true;
        event = Some(ev);
        pressure_spike = spike;
        // A phase change rewrites the cell's density, mass and temperature at
        // once, so every neighbour has to re-evaluate (a frozen neighbour is a
        // wall, a thawed one is a slope). Transitions are rare; the broadcast
        // costs nothing in a settled world.
        grid.wake_with_neighbors(slot, idx);
    }

    // ---- 4. convective nucleation --------------------------------------
    changed |= nucleate(grid, slot, idx, t);

    let me3 = grid.voxel(slot, idx);

    // ---- 5. still-active decision --------------------------------------
    // A cell stays awake while it still has a meaningful gradient, a phase that
    // is mid-transition, or an active bubble column.
    let mut active = me3.flags & (flags::BUBBLE | flags::FLASHING) != 0;
    if !active {
        let nb = grid.neighborhood(slot, idx, me3.temp);
        let mut d = 0;
        while d < 6 {
            let delta = (nb[d] as i32 - me3.temp as i32).unsigned_abs();
            if delta > t.conduction_epsilon as u32 {
                active = true;
                break;
            }
            d += 1;
        }
    }
    // Phase transitions always need at least one more evaluation so that the
    // freshly created cell can settle (and so a spreading fire front advances).
    if me2.species != me3.species {
        active = true;
    }

    CellResult {
        changed,
        active,
        event,
        pressure_spike,
    }
}

/// Exchanges heat between `(slot, idx)` and its neighbour along `d`.
fn conduction_pair(grid: &mut Grid, slot: usize, idx: usize, d: Dir, t: &Tunables) -> bool {
    let a = grid.voxel(slot, idx);
    let n = match grid.neighbor(slot, idx, d) {
        Some(n) => n,
        None => return false,
    };
    let b = grid.chunks[n.chunk].get(n.index);

    // Two voids have nothing to exchange, and a solid-vs-void pair is handled by
    // the radiation path (much cheaper and physically closer).
    if a.class() == Class::Void && b.class() == Class::Void {
        return false;
    }
    let dt = a.temp as i32 - b.temp as i32;
    if dt.unsigned_abs() <= t.conduction_epsilon as u32 {
        return false;
    }

    let cap_a = a.volumetric_heat_capacity() as i64;
    let cap_b = b.volumetric_heat_capacity() as i64;
    let total = cap_a + cap_b;
    if total <= 0 {
        return false;
    }

    let k = a.material().conductivity.min(b.material().conductivity) as i32;
    let rate_q16 = ((t.conduction_q16 as i32) * conductivity_factor_q8(k as u16)) >> 8;
    if rate_q16 <= 0 {
        return false;
    }

    // Weighted equilibrium: the invariant of the pair exchange.
    let eq = (a.temp as i64 * cap_a + b.temp as i64 * cap_b) / total;
    // Energy to move from the hotter side, expressed in degrees of the *other*
    // cell, then back-converted through the capacity ratio.
    let mut da = round_div((eq - a.temp as i64) * rate_q16 as i64, 65_536);
    // Quantisation floor: with a 1 °C resolution a slow exchange could round to
    // zero forever and the pool would never warm. Always move at least one
    // degree towards the equilibrium when a real gradient exists.
    if da == 0 && eq != a.temp as i64 {
        da = sign64(eq - a.temp as i64);
    }
    // Never overshoot the equilibrium (that would oscillate forever).
    let room_a = (a.temp as i64 - eq).unsigned_abs() as i64;
    let da = da.clamp(-room_a, room_a);
    // The partner's step follows from energy conservation: Ca·da + Cb·db == 0.
    let db = if cap_b > 0 {
        round_div(-(da * cap_a), cap_b)
    } else {
        0
    };
    // Never overshoot the partner either.
    let room_b = (b.temp as i64 - eq).unsigned_abs() as i64;
    let db = db.clamp(-room_b, room_b);
    if da == 0 && db == 0 {
        return false;
    }

    let mut a2 = a;
    let mut b2 = b;
    a2.temp = clamp_temp(a.temp as i32 + da as i32);
    b2.temp = clamp_temp(b.temp as i32 + db as i32);

    let mut changed = false;
    if a2.temp != a.temp {
        grid.put(slot, idx, a2);
        changed = true;
    }
    if b2.temp != b.temp {
        grid.put(n.chunk, n.index, b2);
        // The neighbour now owes a re-evaluation; queue it for the next tick so
        // the current sweep cannot ping-pong heat back and forth.
        grid.wake_next(n.chunk, n.index);
        changed = true;
    }
    changed
}

/// Radiative + convective exchange with the ambient air.
fn relax_surface(grid: &mut Grid, slot: usize, idx: usize, t: &Tunables) -> bool {
    let v = grid.voxel(slot, idx);
    if v.class() == Class::Void {
        return false;
    }
    let ambient = grid.chunks[slot].ambient;

    // Only cells that actually touch a gas/void exchange with the sky. That is
    // what keeps a deep sealed aquifer at its own temperature.
    let mut exposed = false;
    let mut d = 0;
    while d < 6 {
        if let Some(nv) = grid.neighbor_voxel(slot, idx, d as Dir) {
            if nv.class() == Class::Void || nv.species == Species::Air as u8 {
                exposed = true;
                break;
            }
        }
        d += 1;
    }
    if !exposed {
        return false;
    }

    let cap = (v.volumetric_heat_capacity() as i64).max(1);
    let inv_cap = (REF_CAPACITY * 256) / cap; // Q8

    let mut delta: i64 = 0;

    // ---- Stefan-Boltzmann (T⁴) against the sky --------------------------
    let emiss = v.material().emissivity as i64;
    if emiss > 0 && t.radiation_scale > 0 {
        let tk = deci_kelvin(v.temp) as i64;
        let ak = deci_kelvin(ambient) as i64;
        let t4 = tk * tk * tk * tk;
        let a4 = ak * ak * ak * ak;
        // Scale down before multiplying so everything stays inside i64.
        let t4m = t4 / 1_000_000;
        let a4m = a4 / 1_000_000;
        let coef = (t.radiation_scale as i64 * emiss) / 100;
        let q = (coef * (t4m - a4m)) / RADIATION_DIV;
        let q = clamp_i32(q);
        delta = -(mul_div_i32(q, inv_cap as i32, 256) as i64);
    }

    // ---- linear relaxation towards ambient ------------------------------
    // Runs when radiation rounds to nothing (everything below red heat), and is
    // what makes a lukewarm pool settle exactly on the ambient temperature.
    if delta == 0 && t.ambient_relax_q16 > 0 {
        let rate = ((t.ambient_relax_q16 as i64) * inv_cap) >> 8;
        delta = round_div((ambient as i64 - v.temp as i64) * rate, 65_536);
        // Quantisation floor: a 1 °C resolution would otherwise stall the cell at
        // a temperature it can never leave. Never overshoot, just step by 1.
        if delta == 0 && ambient != v.temp {
            delta = sign64(ambient as i64 - v.temp as i64);
        }
    }

    // ---- never overshoot the ambient ------------------------------------
    let over = v.temp as i64 - ambient as i64;
    if (delta < 0 && over > 0 && -delta > over) || (delta > 0 && over < 0 && delta > -over) {
        delta = -over;
    }
    // A single tick can never move a cell by more than 64 °C: the sky is a
    // reservoir, not a teleporter. A gas cell gets a much tighter cap: its
    // volumetric heat capacity is ~2 000× smaller than water's, so the raw
    // relaxation term would slam a steam plume from 140 °C to ambient in a
    // single tick and the plume would never be visible (nor would it carry any
    // vapour up a shaft, which is the whole point of the steam engine chapter).
    let step_cap = if v.is_gas() { GAS_RELAX_STEP_C } else { 64 };
    delta = delta.clamp(-step_cap, step_cap);

    if delta == 0 {
        return false;
    }
    let mut v2 = v;
    v2.temp = clamp_temp(v.temp as i32 + clamp_i32(delta));
    if v2.temp == v.temp {
        return false;
    }
    grid.put(slot, idx, v2);
    true
}

#[inline]
fn clamp_i32(v: i64) -> i32 {
    if v > i32::MAX as i64 {
        i32::MAX
    } else if v < i32::MIN as i64 {
        i32::MIN
    } else {
        v as i32
    }
}

// ---------------------------------------------------------------------------
// Phase transitions
// ---------------------------------------------------------------------------

/// Applies the phase machine to a single cell.
///
/// Returns the emitted event (if any) and the pressure spike injected into the
/// cell by an explosive transition.
pub fn apply_phase(grid: &mut Grid, slot: usize, idx: usize, t: &Tunables) -> Option<(Event, u16)> {
    let v = grid.voxel(slot, idx);
    match v.species {
        s if s == Species::Water as u8 || s == Species::SaltWater as u8 => {
            let freeze = if s == Species::SaltWater as u8 {
                t.freeze_c - 2
            } else {
                t.freeze_c
            };
            if v.temp <= freeze && v.mass > 0 {
                return Some(freeze_cell(grid, slot, idx, t));
            }
            if v.temp >= boil_point_c(v.pressure, t) {
                return Some(boil_cell(grid, slot, idx, t));
            }
            None
        }
        s if s == Species::Ice as u8 => {
            if v.temp >= t.melt_c {
                return Some(melt_cell(grid, slot, idx, t, Species::Water));
            }
            None
        }
        s if s == Species::Snow as u8 => {
            if v.temp >= t.melt_c {
                return Some(melt_cell(grid, slot, idx, t, Species::Water));
            }
            None
        }
        s if s == Species::Steam as u8 => {
            // (a) pressure/temperature condensation.
            if v.temp <= condense_point_c(v.pressure, t) {
                return Some(condense_cell(grid, slot, idx, t));
            }
            // (b) cold ceiling: steam touching a cold solid condenses on it and
            //     the resulting droplets fall back down the shaft.
            if let Some(ceil) = grid.neighbor_voxel(slot, idx, DIR_POS_Y) {
                if ceil.class() == Class::Solid && ceil.temp <= t.cold_ceiling_c {
                    return Some(condense_cell(grid, slot, idx, t));
                }
            }
            None
        }
        s if s == Species::Lava as u8 => {
            // Water contact wins over solidification: the quench is explosive and
            // must be resolved before the lava has a chance to cool into basalt.
            if let Some((result, spike)) = try_quench(grid, slot, idx, t) {
                return Some((Event::Quench { slot, idx, result }, spike));
            }
            if v.temp < t.basalt_c {
                let mut nv = v;
                nv.species = Species::Basalt as u8;
                nv.mass = MASS_FULL;
                nv.flags &= !(flags::BUBBLE | flags::UPDRAFT);
                grid.put(slot, idx, nv);
                return None;
            }
            None
        }
        s if s == Species::Magma as u8 => {
            if v.temp < t.basalt_c - 100 {
                let mut nv = v;
                nv.species = Species::Basalt as u8;
                nv.mass = MASS_FULL;
                grid.put(slot, idx, nv);
            }
            None
        }
        s if s == Species::MoltenMetal as u8 => {
            let melt = Species::Metal.material().melt_point;
            if v.temp < melt {
                let mut nv = v;
                nv.species = Species::Metal as u8;
                nv.mass = MASS_FULL;
                grid.put(slot, idx, nv);
            }
            None
        }
        s if s == Species::Wood as u8
            || s == Species::Plank as u8
            || s == Species::Leaves as u8 =>
        {
            if v.temp >= t.ignition_c {
                let mut nv = v;
                nv.species = Species::Fire as u8;
                nv.mass = MASS_FULL;
                nv.flags |= flags::ACTIVE;
                grid.put(slot, idx, nv);
                grid.wake_with_neighbors(slot, idx);
                return Some((Event::Ignite { slot, idx }, 0));
            }
            None
        }
        s if s == Species::Fire as u8 => {
            if v.temp < 150 {
                let mut nv = v;
                nv.species = Species::Air as u8;
                nv.mass = 0;
                nv.flags = flags::ACTIVE;
                grid.put(slot, idx, nv);
                return None;
            }
            None
        }
        _ => None,
    }
}

/// Water -> ice. The ice keeps the water's mass, which is exactly the visual
/// thickness rule: `mass 1..3` is a thin sheet, `mass 8` a solid block.
fn freeze_cell(grid: &mut Grid, slot: usize, idx: usize, t: &Tunables) -> (Event, u16) {
    let v = grid.voxel(slot, idx);
    let mut nv = v;
    nv.species = Species::Ice as u8;
    // Crystallisation releases the latent heat of fusion: the freshly formed ice
    // is warmer than the water it came from, which is why a pond freezes from the
    // top down and not instantly throughout.
    nv.temp = clamp_temp(v.temp as i32 + t.latent_freeze_c as i32);
    nv.flags |= flags::TRANSITIONED;
    let mass = nv.mass;
    grid.put(slot, idx, nv);
    (Event::Freeze { slot, idx, mass }, 0)
}

/// Ice/snow -> water.
fn melt_cell(
    grid: &mut Grid,
    slot: usize,
    idx: usize,
    t: &Tunables,
    into: Species,
) -> (Event, u16) {
    let v = grid.voxel(slot, idx);
    let mut nv = v;
    nv.species = into as u8;
    // Melting absorbs the latent heat, cooling the meltwater below the melting
    // point, the hysteresis band (freeze −1 °C / melt +1 °C) prevents an
    // immediate re-freeze.
    nv.temp = clamp_temp(v.temp as i32 - t.latent_melt_c as i32);
    nv.flags |= flags::TRANSITIONED | flags::ACTIVE;
    let mass = nv.mass.max(1);
    grid.put(slot, idx, nv);
    (Event::Melt { slot, idx, mass }, 0)
}

/// Water -> steam, with mass loss and evaporative cooling.
fn boil_cell(grid: &mut Grid, slot: usize, idx: usize, _t: &Tunables) -> (Event, u16) {
    let v = grid.voxel(slot, idx);
    let mut boiled = 0u8;

    // How much water can escape into the cell above?
    if let Some(n) = grid.neighbor(slot, idx, DIR_POS_Y) {
        let above = grid.chunks[n.chunk].get(n.index);
        let room = match above.class() {
            Class::Void => MASS_FULL,
            Class::Gas => MASS_FULL - above.mass.min(MASS_FULL),
            _ => 0,
        };
        if room > 0 {
            boiled = v.mass.min(room);
            let new_mass = if above.class() == Class::Void {
                boiled
            } else {
                above.mass.saturating_add(boiled).min(MASS_FULL)
            };
            let steam = Voxel {
                species: Species::Steam as u8,
                mass: new_mass,
                temp: clamp_temp(v.temp as i32 + 10),
                pressure: above.pressure,
                flags: flags::ACTIVE | flags::MOVED,
                aux: aux::FLOW_NONE,
            };
            grid.put(n.chunk, n.index, steam);
            grid.wake_with_neighbors(n.chunk, n.index);
        }
    }

    // No headroom above (submerged boiling): vent the vapour through the water
    // column instead. A sealed container simply cannot boil, the pressure solver
    // raises the boiling point first, exactly like a pressure cooker.
    if boiled == 0 {
        if let Some(outlet) = bubble_outlet(grid, slot, idx) {
            let target = grid.chunks[outlet.chunk].get(outlet.index);
            let new_mass = if target.class() == Class::Void {
                1
            } else {
                target.mass.saturating_add(1).min(MASS_FULL)
            };
            let steam = Voxel {
                species: Species::Steam as u8,
                mass: new_mass,
                temp: clamp_temp(v.temp as i32 + 10),
                pressure: target.pressure,
                flags: flags::ACTIVE | flags::MOVED,
                aux: aux::FLOW_NONE,
            };
            grid.put(outlet.chunk, outlet.index, steam);
            grid.wake_with_neighbors(outlet.chunk, outlet.index);
            boiled = 1;
        } else {
            // Nowhere for the vapour to go: highlight the cell as a bubble and
            // give the entity layer an updraft so the column visibly churns.
            let mut v2 = v;
            v2.flags |= flags::BUBBLE | flags::UPDRAFT;
            grid.put(slot, idx, v2);
            grid.wake_with_neighbors(slot, idx);
            return (Event::Boil { slot, idx, mass: 0 }, 0);
        }
    }

    if boiled == 0 {
        return (Event::Boil { slot, idx, mass: 0 }, 0);
    }

    let mut nv = v;
    nv.mass = v.mass.saturating_sub(boiled);
    // Evaporative cooling: removing `boiled` units of the most energetic
    // molecules cools the rest of the cell.
    nv.temp = clamp_temp(v.temp as i32 - 6 * boiled as i32);
    if nv.mass == 0 {
        nv.species = Species::Air as u8;
        nv.mass = 0;
        nv.flags = flags::ACTIVE;
    } else {
        nv.flags |= flags::FLASHING;
    }
    grid.put(slot, idx, nv);
    (
        Event::Boil {
            slot,
            idx,
            mass: boiled,
        },
        0,
    )
}

/// Finds the first gas cell above a submerged boiling cell, at most
/// [`MAX_BUBBLE_CHAIN`] cells away.
///
/// The vapour is created at the outlet and its mass is taken from the boiling
/// cell, so the column carries the churn visually through the `UPDRAFT` flag
/// without any water moving. Every unit removed reappears as one unit of steam.
fn bubble_outlet(grid: &Grid, slot: usize, idx: usize) -> Option<crate::grid::Neighbor> {
    let mut cur = (slot, idx);
    for _ in 0..MAX_BUBBLE_CHAIN {
        let n = grid.neighbor(cur.0, cur.1, DIR_POS_Y)?;
        let nv = grid.chunks[n.chunk].get(n.index);
        match nv.class() {
            Class::Gas | Class::Void => {
                if nv.mass < MASS_FULL {
                    return Some(n);
                }
                // A full gas cell: the bubble has to keep looking upward.
                cur = (n.chunk, n.index);
            }
            Class::Liquid => {
                if nv.species == Species::Water as u8 || nv.species == Species::SaltWater as u8 {
                    cur = (n.chunk, n.index);
                } else {
                    return None; // water under lava: handled by the quench reaction
                }
            }
            Class::Solid => return None,
        }
    }
    None
}

/// Steam -> water droplets.
///
/// The droplet keeps the whole mass of the vapour cell: `mass` is a volume
/// fraction of the block, and every kernel in the mod is required to conserve
/// it (`Grid::total_mass` is the counter the conservation tests assert on). A
/// condensation that rounded a one-unit wisp up to a two-unit droplet would
/// silently create water out of nothing, and a long, cold cave would slowly
/// fill itself.
fn condense_cell(grid: &mut Grid, slot: usize, idx: usize, _t: &Tunables) -> (Event, u16) {
    let v = grid.voxel(slot, idx);
    let mut nv = v;
    nv.species = Species::Water as u8;
    nv.mass = v.mass.clamp(1, MASS_FULL);
    // Condensation releases the latent heat of vaporisation: the droplet is warm.
    nv.temp = clamp_temp(v.temp as i32 + 4);
    nv.flags |= flags::ACTIVE | flags::MOVED;
    nv.flags &= !(flags::BUBBLE | flags::UPDRAFT);
    let mass = nv.mass;
    grid.put(slot, idx, nv);
    grid.wake_with_neighbors(slot, idx);
    (Event::Condense { slot, idx, mass }, 0)
}

// ---------------------------------------------------------------------------
// Lava / water quench
// ---------------------------------------------------------------------------

/// Detects water touching a lava cell and resolves the reaction.
///
/// Physics: 1 kg of water expands to ~1 700 L of steam, so the contact is a
/// *steam explosion*, not a gentle hiss. The outcome depends on how fast the
/// melt is chilled:
///
/// * fast + cold water -> obsidian (glassy, no crystals, dense)
/// * fast + already-cooling lava -> volcanic glass
/// * slow + lots of vapour -> pumice (porous, floats, see the buoyancy kernel)
fn try_quench(grid: &mut Grid, slot: usize, idx: usize, t: &Tunables) -> Option<(Species, u16)> {
    let lava = grid.voxel(slot, idx);
    if lava.species != Species::Lava as u8 {
        return None;
    }
    // Locate the water that is doing the quenching.
    let mut water_at: Option<(usize, usize, Voxel)> = None;
    let mut d = 0;
    while d < 6 {
        if let Some(nv) = grid.neighbor_voxel(slot, idx, d as Dir) {
            if (nv.species == Species::Water as u8 || nv.species == Species::SaltWater as u8)
                && nv.mass > 0
            {
                if let Some(n) = grid.neighbor(slot, idx, d as Dir) {
                    water_at = Some((n.chunk, n.index, nv));
                    break;
                }
            }
        }
        d += 1;
    }
    let (wslot, widx, water) = water_at?;

    // Contact-chilling decides the solid phase.
    let result = if water.mass >= 4 && water.temp <= t.quench_hot_c {
        Species::Obsidian
    } else if lava.temp >= 900 && water.mass >= 2 {
        Species::VolcanicGlass
    } else {
        Species::Pumice
    };
    let new_temp = match result {
        Species::Obsidian => t.obsidian_c,
        Species::VolcanicGlass => t.obsidian_c + 60,
        _ => t.pumice_c,
    };

    // 1. the lava freezes into its quench product
    let mut nv = lava;
    nv.species = result as u8;
    nv.mass = MASS_FULL;
    nv.temp = new_temp;
    nv.flags = flags::ACTIVE | flags::TRANSITIONED;
    grid.put(slot, idx, nv);

    // 2. the water flashes to steam and takes the pressure spike
    let mut steam = water;
    steam.species = Species::Steam as u8;
    steam.mass = water.mass.min(MASS_FULL);
    steam.temp = clamp_temp(lava.temp.max(120) as i32);
    steam.flags = flags::ACTIVE | flags::FLASHING | flags::MOVED;
    steam.pressure = steam.pressure.saturating_add(t.flash_pressure);
    grid.put(wslot, widx, steam);

    // 3. the micro-explosion: pressure wave + shattering of brittle blocks
    let spike = explode(grid, slot, idx, t);
    grid.wake_with_neighbors(slot, idx);
    grid.wake_with_neighbors(wslot, widx);
    Some((result, spike))
}

/// Applies the pressure wave of a quench explosion around `(slot, idx)`.
///
/// Only brittle blocks (leaves, planks, glass, snow, pumice) are destroyed; the
/// point is to blow the *lid* off a sealed lava chamber, not to mine for the
/// player. Fluid cells get the pressure spike and are stirred.
fn explode(grid: &mut Grid, slot: usize, idx: usize, t: &Tunables) -> u16 {
    let radius = t.flash_radius as i32;
    if radius <= 0 {
        return 0;
    }
    let base = grid.chunks[slot].coords;
    let (lx, ly, lz) = dirs::unindex(idx);
    let wx = base[0] * dirs::CHUNK_SIZE as i32 + lx as i32;
    let wy = base[1] * dirs::CHUNK_SIZE as i32 + ly as i32;
    let wz = base[2] * dirs::CHUNK_SIZE as i32 + lz as i32;

    let mut spike = 0u16;
    for dy in -radius..=radius {
        for dz in -radius..=radius {
            for dx in -radius..=radius {
                if dx == 0 && dy == 0 && dz == 0 {
                    continue;
                }
                let dist = dx.abs().max(dy.abs()).max(dz.abs()) as u16;
                // Inverse-square-ish falloff in integer form.
                let energy = (t.flash_pressure as u32) / (1 + (dist as u32) * (dist as u32));
                if energy == 0 {
                    continue;
                }
                let x = wx + dx;
                let y = wy + dy;
                let z = wz + dz;
                let cur = match grid.get_world(x, y, z) {
                    Some(v) => v,
                    None => continue, // not resident: no free chunk creation
                };
                let mut nv = cur;
                match cur.class() {
                    Class::Solid => {
                        let brittle = cur.material().yield_pressure < t.damage_threshold * 3;
                        if brittle {
                            nv.species = Species::Air as u8;
                            nv.mass = 0;
                            nv.flags = flags::ACTIVE;
                        } else {
                            nv.aux = aux::add_damage(cur.aux, (energy / 2_000).min(15) as u8);
                            nv.flags |= flags::ACTIVE;
                        }
                    }
                    _ => {
                        nv.pressure = cur.pressure.saturating_add(energy as u16);
                        nv.flags |= flags::ACTIVE | flags::BUBBLE;
                    }
                }
                grid.set_world(x, y, z, nv);
                if dist <= 1 {
                    spike = spike.max(energy as u16);
                }
            }
        }
    }
    spike
}

// ---------------------------------------------------------------------------
// Convection
// ---------------------------------------------------------------------------

/// Nucleates and sustains convective bubble columns above hot blocks.
///
/// * a water cell sitting on a hot block (lava, magma, fire, or anything above
///   the bubble threshold) becomes a nucleation site;
/// * the `BUBBLE`/`UPDRAFT` flags travel upward through the water column;
/// * the entity layer reads `UPDRAFT` to push mobs, items and the player up, 
///   the "boiling cauldron" effect described in the Atlas.
pub fn nucleate(grid: &mut Grid, slot: usize, idx: usize, t: &Tunables) -> bool {
    let v = grid.voxel(slot, idx);
    let is_water = v.species == Species::Water as u8 || v.species == Species::SaltWater as u8;
    if !is_water {
        return false;
    }
    let below = grid.neighbor_voxel(slot, idx, DIR_NEG_Y);
    let heated_from_below = match below {
        Some(b) => {
            b.species == Species::Lava as u8
                || b.species == Species::Magma as u8
                || b.species == Species::Fire as u8
                || b.temp >= t.boil_c
        }
        None => false,
    };
    if !heated_from_below && v.temp < t.bubble_c {
        return false;
    }

    let mut v2 = v;
    let already = v.flags & (flags::BUBBLE | flags::UPDRAFT) != 0;
    v2.flags |= flags::BUBBLE | flags::UPDRAFT;
    if v2.temp < t.boil_c {
        // Nucleation moves the cell slightly towards boiling: contact heat.
        v2.temp = clamp_temp(v2.temp as i32 + 2);
    }
    let changed = !already || v2.temp != v.temp;
    grid.put(slot, idx, v2);

    // Propagate the column upward; the flags rise one cell per tick.
    let above = grid.neighbor(slot, idx, DIR_POS_Y);
    if let Some(n) = above {
        let av = grid.chunks[n.chunk].get(n.index);
        let gasish = av.class() == Class::Gas
            || av.species == Species::Water as u8
            || av.species == Species::SaltWater as u8;
        if gasish && av.flags & flags::UPDRAFT == 0 {
            let mut anv = av;
            anv.flags |= flags::UPDRAFT | flags::BUBBLE;
            grid.put(n.chunk, n.index, anv);
            grid.wake(n.chunk, n.index);
        }
    }
    changed
}

// ---------------------------------------------------------------------------
// Pressure-driven fracture
// ---------------------------------------------------------------------------

/// Accumulates hydrostatic damage on a brittle block and shatters it when the
/// damage counter saturates.
///
/// This is what makes a glass aquarium burst at depth while a stone dam stands:
/// the yield pressure comes straight from the material database, so a plank wall
/// needs only a 1.5 m head whereas reinforced stone needs 35 m.
pub fn apply_pressure_damage(
    grid: &mut Grid,
    slot: usize,
    idx: usize,
    excess: u16,
    t: &Tunables,
) -> Option<Event> {
    let v = grid.voxel(slot, idx);
    if v.class() != Class::Solid {
        return None;
    }
    let yield_p = v.material().yield_pressure;
    if yield_p == u16::MAX || excess < yield_p {
        return None;
    }
    let species = v.species();
    // How far past its limit the block is, in 6.4 kPa steps (≈ 1.6 blocks of
    // water): every step past the yield adds one damage point per tick.
    let stress = excess.saturating_sub(yield_p) / 64;
    let add = (t.damage_per_tick as u16 + stress).min(15) as u8;
    let mut nv = v;
    nv.aux = aux::add_damage(v.aux, add);
    if aux::damage(nv.aux) >= t.damage_break {
        nv.species = Species::Air as u8;
        nv.mass = 0;
        nv.flags = flags::ACTIVE;
        grid.put(slot, idx, nv);
        grid.wake_with_neighbors(slot, idx);
        return Some(Event::Shatter { slot, idx, species });
    }
    grid.put(slot, idx, nv);
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirs::CHUNK_VOLUME;
    use crate::grid::Grid;

    fn setup() -> (Grid, Tunables) {
        let mut g = Grid::with_capacity(9, 4);
        g.ensure_chunk([0, 0, 0]);
        (g, Tunables::DEFAULT)
    }

    fn slot_of(g: &Grid) -> usize {
        g.find([0, 0, 0]).unwrap()
    }

    #[test]
    fn log2_is_accurate_enough() {
        assert_eq!(log2_q8(256), 0);
        assert_eq!(log2_q8(512), 256);
        assert_eq!(log2_q8(128), -256);
        assert!((log2_q8(300) - 40).abs() < 24, "{}", log2_q8(300));
        assert!(log2_q8(0) < 0);
    }

    #[test]
    fn boiling_point_rises_with_pressure() {
        let t = Tunables::DEFAULT;
        let sea = boiling_at(&t, t.sea_level_pressure);
        let double = boiling_at(&t, t.sea_level_pressure.saturating_mul(2));
        let half = boiling_at(&t, t.sea_level_pressure / 2);
        assert_eq!(sea, 100, "sea level must boil at 100 °C");
        assert!(
            double > sea + 15,
            "2 atm should boil well above 100: {double}"
        );
        assert!(half < sea - 15, "0.5 atm should boil below 100: {half}");
    }

    fn boiling_at(t: &Tunables, p: Press) -> Temp {
        boil_point_c(p, t)
    }

    #[test]
    fn condensation_is_below_boiling() {
        let t = Tunables::DEFAULT;
        let b = boil_point_c(t.sea_level_pressure, &t);
        let c = condense_point_c(t.sea_level_pressure, &t);
        assert!(c < b, "condense {c} must be below boil {b}");
    }

    #[test]
    fn pressure_and_geothermal_depth_curves() {
        let t = Tunables::DEFAULT;
        assert_eq!(pressure_at_depth(0, &t), t.sea_level_pressure);
        assert!(pressure_at_depth(10, &t) > t.sea_level_pressure);
        assert_eq!(pressure_at_depth(-5, &t), t.sea_level_pressure);
        assert_eq!(
            temperature_at_depth(64, &t),
            t.ambient_c + t.rock_gradient_c
        );
        assert!(temperature_at_depth(10_000, &t) <= 2_000);
    }

    #[test]
    fn water_freezes_and_ice_thickness_follows_mass() {
        let (mut g, t) = setup();
        let slot = slot_of(&g);
        let idx = dirs::index(2, 2, 2);
        let mut v = Voxel::water(-5);
        v.mass = 3;
        g.put(slot, idx, v);
        let ev = apply_phase(&mut g, slot, idx, &t).expect("water below 0 must freeze");
        match ev.0 {
            Event::Freeze { mass, .. } => assert_eq!(mass, 3, "thin ice keeps the water mass"),
            other => panic!("expected Freeze, got {other:?}"),
        }
        let ice = g.voxels(slot)[idx];
        assert_eq!(ice.species(), Species::Ice);
        assert_eq!(ice.mass, 3);
        assert!(
            ice.temp > -5,
            "freezing releases latent heat, ice must be warmer: {}",
            ice.temp
        );
    }

    #[test]
    fn ice_melts_back_to_water_but_does_not_ping_pong() {
        let (mut g, t) = setup();
        let slot = slot_of(&g);
        let idx = dirs::index(2, 2, 2);
        g.put(slot, idx, Voxel::new(Species::Ice, 3));
        let ev = apply_phase(&mut g, slot, idx, &t).expect("ice above 1 °C must melt");
        assert!(matches!(ev.0, Event::Melt { .. }));
        let w = g.voxels(slot)[idx];
        assert_eq!(w.species(), Species::Water);
        assert!(w.temp <= 1, "meltwater must be chilled: {}", w.temp);
        // With the hysteresis band the water must NOT re-freeze immediately.
        assert!(apply_phase(&mut g, slot, idx, &t).is_none());
    }

    #[test]
    fn boiling_moves_mass_into_steam_above() {
        let (mut g, t) = setup();
        let slot = slot_of(&g);
        let idx = dirs::index(3, 3, 3);
        let mut v = Voxel::water(120);
        v.mass = 6;
        g.put(slot, idx, v);
        let before = g.total_mass();
        let ev = apply_phase(&mut g, slot, idx, &t).expect("water at 120 °C must boil");
        match ev.0 {
            Event::Boil { mass, .. } => assert!(mass > 0, "some water must evaporate"),
            other => panic!("expected Boil, got {other:?}"),
        }
        let above = g.voxels(slot)[dirs::index(3, 4, 3)];
        assert_eq!(above.species(), Species::Steam, "steam must appear above");
        let after = g.total_mass();
        assert_eq!(before, after, "mass must be conserved through vaporisation");
        let rest = g.voxels(slot)[idx];
        assert!(rest.mass < 6, "the source cell must lose mass");
        assert!(rest.temp < 120, "evaporative cooling must chill the source");
    }

    #[test]
    fn sealed_water_cannot_boil_through_a_solid_ceiling() {
        let (mut g, t) = setup();
        let slot = slot_of(&g);
        let idx = dirs::index(3, 3, 3);
        g.put(slot, dirs::index(3, 4, 3), Voxel::stone(20));
        g.put(slot, idx, Voxel::water(400));
        let ev = apply_phase(&mut g, slot, idx, &t).expect("boiling should still be attempted");
        match ev.0 {
            Event::Boil { mass, .. } => assert_eq!(mass, 0, "no room => no vapour"),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(g.voxels(slot)[idx].species(), Species::Water);
        assert_eq!(
            g.voxels(slot)[dirs::index(3, 4, 3)].species(),
            Species::Stone
        );
    }

    #[test]
    fn submerged_boiling_vents_steam_through_the_column() {
        let (mut g, t) = setup();
        let slot = slot_of(&g);
        // Water column y=2..5, open air at y=6.
        for y in 2..=5 {
            g.put(slot, dirs::index(3, y, 3), Voxel::water(130));
        }
        g.put(slot, dirs::index(3, 6, 3), Voxel::AIR);
        let before = g.total_mass();
        let idx = dirs::index(3, 2, 3);
        let ev = apply_phase(&mut g, slot, idx, &t).expect("submerged boiling");
        assert!(matches!(ev.0, Event::Boil { mass, .. } if mass > 0));
        // The vapour is created at the outlet, above the whole column.
        assert_eq!(
            g.voxels(slot)[dirs::index(3, 6, 3)].species(),
            Species::Steam,
            "steam must vent above the column"
        );
        assert_eq!(g.total_mass(), before, "venting must conserve mass exactly");
        // ... and the source cell gave up exactly one unit.
        assert_eq!(g.voxels(slot)[idx].mass, 7);
    }

    #[test]
    fn steam_condenses_on_a_cold_ceiling() {
        let (mut g, t) = setup();
        let slot = slot_of(&g);
        let idx = dirs::index(4, 4, 4);
        g.put(slot, dirs::index(4, 5, 4), Voxel::stone(-10)); // cold stone ceiling
        g.put(slot, idx, Voxel::new(Species::Steam, 60));
        let ev = apply_phase(&mut g, slot, idx, &t).expect("cold ceiling must condense steam");
        assert!(matches!(ev.0, Event::Condense { .. }));
        assert_eq!(g.voxels(slot)[idx].species(), Species::Water);
    }

    #[test]
    fn hot_steam_does_not_condense() {
        let (mut g, t) = setup();
        let slot = slot_of(&g);
        let idx = dirs::index(4, 4, 4);
        g.put(slot, dirs::index(4, 5, 4), Voxel::stone(400));
        g.put(slot, idx, Voxel::new(Species::Steam, 300));
        assert!(apply_phase(&mut g, slot, idx, &t).is_none());
    }

    #[test]
    fn lava_quenches_into_obsidian_with_water_and_raises_pressure() {
        let (mut g, t) = setup();
        let slot = slot_of(&g);
        let idx = dirs::index(5, 5, 5);
        g.put(slot, idx, Voxel::new(Species::Lava, 1_100));
        let widx = dirs::index(6, 5, 5);
        let mut w = Voxel::water(20);
        w.mass = 8;
        g.put(slot, widx, w);
        let ev = apply_phase(&mut g, slot, idx, &t).expect("lava + water must react");
        match ev.0 {
            Event::Quench { result, .. } => assert_eq!(result, Species::Obsidian),
            other => panic!("expected Quench, got {other:?}"),
        }
        let after = g.voxels(slot)[idx];
        assert_eq!(after.species(), Species::Obsidian);
        assert!(after.temp <= t.obsidian_c + 1, "quenched glass is cold");
        let steam = g.voxels(slot)[widx];
        assert_eq!(steam.species(), Species::Steam, "water flashes to steam");
        assert!(steam.pressure > 1_000, "flash must spike the pressure");
        assert!(ev.1 > 0, "the explosion must report a pressure spike");
    }

    #[test]
    fn quench_into_pumice_when_water_is_shallow() {
        let (mut g, t) = setup();
        let slot = slot_of(&g);
        let idx = dirs::index(5, 5, 5);
        g.put(slot, idx, Voxel::new(Species::Lava, 750));
        let widx = dirs::index(5, 6, 5);
        let mut w = Voxel::water(20);
        w.mass = 1;
        g.put(slot, widx, w);
        match apply_phase(&mut g, slot, idx, &t).expect("must react").0 {
            Event::Quench { result, .. } => assert_eq!(result, Species::Pumice),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn cool_lava_solidifies_into_basalt() {
        let (mut g, t) = setup();
        let slot = slot_of(&g);
        let idx = dirs::index(1, 1, 1);
        g.put(slot, idx, Voxel::new(Species::Lava, 400));
        assert!(apply_phase(&mut g, slot, idx, &t).is_none());
        assert_eq!(g.voxels(slot)[idx].species(), Species::Basalt);
    }

    #[test]
    fn wood_ignites_above_the_ignition_point() {
        let (mut g, t) = setup();
        let slot = slot_of(&g);
        let idx = dirs::index(7, 7, 7);
        g.put(slot, idx, Voxel::new(Species::Wood, 400));
        let ev = apply_phase(&mut g, slot, idx, &t).expect("wood must ignite");
        assert!(matches!(ev.0, Event::Ignite { .. }));
        assert_eq!(g.voxels(slot)[idx].species(), Species::Fire);
    }

    #[test]
    fn conduction_homogenises_temperatures() {
        let (mut g, t) = setup();
        let slot = slot_of(&g);
        let a = dirs::index(4, 4, 4);
        let b = dirs::index(5, 4, 4);
        g.put(slot, a, Voxel::stone(200));
        g.put(slot, b, Voxel::stone(20));
        let mut prev_gap = 180i32;
        for _ in 0..40 {
            conduction_pair(&mut g, slot, a, crate::dirs::DIR_POS_X, &t);
            let gap = (g.voxels(slot)[a].temp as i32 - g.voxels(slot)[b].temp as i32).abs();
            assert!(
                gap <= prev_gap,
                "the gradient must never grow: {gap} > {prev_gap}"
            );
            prev_gap = gap;
        }
        assert!(
            prev_gap < 60,
            "conduction should have closed most of the gap: {prev_gap}"
        );
    }

    #[test]
    fn conduction_respects_heat_capacity() {
        let (mut g, t) = setup();
        let slot = slot_of(&g);
        let a = dirs::index(4, 4, 4);
        let b = dirs::index(5, 4, 4);
        g.put(slot, a, Voxel::stone(200));
        g.put(slot, b, Voxel::water(20));
        for _ in 0..30 {
            conduction_pair(&mut g, slot, a, crate::dirs::DIR_POS_X, &t);
        }
        let stone = g.voxels(slot)[a].temp as i32;
        let water = g.voxels(slot)[b].temp as i32;
        assert!(water > 20, "water must warm up");
        assert!(stone < 200, "stone must cool down");
        // Water's volumetric capacity is ~2× the stone's, so the equilibrium sits
        // closer to the water's starting temperature.
        assert!(
            water - 20 < 200 - stone,
            "water ({water}) must move less than stone ({stone})"
        );
    }

    #[test]
    fn lava_heats_adjacent_stone_over_time() {
        let (mut g, t) = setup();
        let slot = slot_of(&g);
        let lava = dirs::index(4, 4, 4);
        let rock = dirs::index(5, 4, 4);
        g.put(slot, lava, Voxel::new(Species::Lava, 1_200));
        g.put(slot, rock, Voxel::stone(20));
        for _ in 0..200 {
            conduction_pair(&mut g, slot, lava, crate::dirs::DIR_POS_X, &t);
        }
        assert!(
            g.voxels(slot)[rock].temp > 200,
            "stone next to lava must heat up: {}",
            g.voxels(slot)[rock].temp
        );
    }

    #[test]
    fn radiation_cools_hot_cells_and_leaves_cold_ones_alone() {
        let (mut g, t) = setup();
        let slot = slot_of(&g);
        g.chunks[slot].ambient = 20;
        let hot = dirs::index(2, 2, 2);
        let cold = dirs::index(6, 6, 6);
        g.put(slot, hot, Voxel::new(Species::Stone, 2_000));
        g.put(slot, cold, Voxel::water(25));
        for _ in 0..50 {
            relax_surface(&mut g, slot, hot, &t);
            relax_surface(&mut g, slot, cold, &t);
        }
        assert!(
            g.voxels(slot)[hot].temp < 2_000,
            "glowing rock must radiate"
        );
        assert!(
            g.voxels(slot)[cold].temp <= 30,
            "lukewarm water must not cool much: {}",
            g.voxels(slot)[cold].temp
        );
    }

    #[test]
    fn exposed_water_approaches_ambient() {
        let (mut g, t) = setup();
        let slot = slot_of(&g);
        let idx = dirs::index(2, 2, 2);
        g.put(slot, idx, Voxel::water(60));
        for _ in 0..3_000 {
            relax_surface(&mut g, slot, idx, &t);
        }
        let temp = g.voxels(slot)[idx].temp;
        assert!(
            (temp - 20).abs() <= 4,
            "exposed water must settle near ambient, got {temp}"
        );
    }

    #[test]
    fn bubbles_form_above_magma() {
        let (mut g, t) = setup();
        let slot = slot_of(&g);
        let magma = dirs::index(3, 3, 3);
        let water = dirs::index(3, 4, 3);
        g.put(slot, magma, Voxel::new(Species::Magma, 1_200));
        g.put(slot, water, Voxel::water(30));
        assert!(nucleate(&mut g, slot, water, &t));
        let v = g.voxels(slot)[water];
        assert!(v.flags & flags::BUBBLE != 0, "nucleation site expected");
        assert!(
            v.flags & flags::UPDRAFT != 0,
            "updraft expected for entities"
        );
        assert!(v.temp > 30, "contact heating expected");
    }

    #[test]
    fn updraft_propagates_up_the_column() {
        let (mut g, t) = setup();
        let slot = slot_of(&g);
        g.put(slot, dirs::index(3, 3, 3), Voxel::new(Species::Lava, 1_100));
        g.put(slot, dirs::index(3, 4, 3), Voxel::water(30));
        g.put(slot, dirs::index(3, 5, 3), Voxel::water(30));
        nucleate(&mut g, slot, dirs::index(3, 4, 3), &t);
        assert!(g.voxels(slot)[dirs::index(3, 5, 3)].flags & flags::UPDRAFT != 0);
    }

    #[test]
    fn pressure_shatters_glass_but_not_stone() {
        let (mut g, mut t) = setup();
        t.damage_threshold = 300;
        t.damage_break = 10;
        let slot = slot_of(&g);
        let glass = dirs::index(2, 2, 2);
        let stone = dirs::index(4, 4, 4);
        g.put(slot, glass, Voxel::new(Species::Glass, 20));
        g.put(slot, stone, Voxel::stone(20));
        // 1 000 units ≈ 25 blocks of head: far past glass (80), far below stone
        // (1 400).
        let mut shattered = false;
        for _ in 0..40 {
            if apply_pressure_damage(&mut g, slot, glass, 1_000, &t).is_some() {
                shattered = true;
                break;
            }
            assert!(apply_pressure_damage(&mut g, slot, stone, 1_000, &t).is_none());
        }
        assert!(shattered, "glass must eventually burst");
        assert_eq!(g.voxels(slot)[glass].species(), Species::Air);
        assert_eq!(
            g.voxels(slot)[stone].species(),
            Species::Stone,
            "stone holds"
        );
    }

    #[test]
    fn tick_cell_keeps_a_gradient_alive() {
        let (mut g, t) = setup();
        let slot = slot_of(&g);
        let a = dirs::index(4, 4, 4);
        g.put(slot, a, Voxel::stone(500));
        let res = tick_cell(&mut g, slot, a, &t);
        assert!(res.changed);
        assert!(res.active, "a 480 °C gradient must stay on the active list");
    }

    #[test]
    fn tick_cell_lets_equilibrium_sleep() {
        let (mut g, t) = setup();
        let slot = slot_of(&g);
        let a = dirs::index(4, 4, 4);
        for d in dirs::DIRS {
            if let Some(n) = g.neighbor(slot, a, d) {
                g.put(n.chunk, n.index, Voxel::stone(20));
            }
        }
        g.put(slot, a, Voxel::stone(20));
        let res = tick_cell(&mut g, slot, a, &t);
        assert!(
            !res.active,
            "a uniform 20 °C region must be allowed to sleep"
        );
    }

    #[test]
    fn conductivity_factor_is_ordered_and_bounded() {
        let wood = conductivity_factor_q8(1);
        let water = conductivity_factor_q8(6);
        let stone = conductivity_factor_q8(15);
        let metal = conductivity_factor_q8(800);
        assert!(wood < water && water < stone && stone <= metal);
        assert!(metal <= 256);
    }

    #[test]
    fn chunk_volume_constant_is_available() {
        assert_eq!(CHUNK_VOLUME, 4_096);
    }
}

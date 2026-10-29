//! Hydrodynamics: the communicating-vessels cellular automaton, hydrostatic head,
//! pressure gradients, jets and buoyancy.
//!
//! # The automaton
//!
//! A fluid cell owns `mass ∈ 0..=8` (8 == one full block). One tick applies, in
//! order:
//!
//! 1. **Gravity** — if the cell below can accept mass, everything that fits
//!    falls. This is what empties a breached dam in a few ticks.
//! 2. **Levelling by hydraulic-head descent** — otherwise mass moves
//!    horizontally towards the neighbour with the lower free surface, i.e. the
//!    neighbour whose own mass plus the column above it is smaller (see
//!    [`try_level`] and [`hydraulic_head`]). Only head-reducing transfers are
//!    allowed and each one is capped at half the head difference, so the pair
//!    converges monotonically, never overshoots and never cycles: a levelled
//!    body of water is a genuine fixed point and its chunk can go to sleep.
//! 3. **Density inversion** — a gas under a liquid (or a liquid over a gas) swaps
//!    with it. Steam therefore bubbles up through water while the water sinks,
//!    with exact mass conservation and no special-casing.
//! 4. **Hydrostatic head** — `p[i] = p[above] + ρ·g·(mass/8)`, so pressure grows
//!    by one block of water per block of depth. A cell is pressurised by the
//!    column *above* it, not by its own mass.
//! 5. **Fracture** — a pressurised cell grinds down brittle neighbours (glass,
//!    planks, leaves) using the yield pressures from the material database.
//!
//! # Communicating vessels
//!
//! Two vessels joined by a *culvert with free space* (any normal doorway, tunnel
//! or open channel) equalise their levels: the surplus side drains through the
//! free surface until the energy of the pair stops decreasing, and then the
//! simulation quiesces.
//!
//! A gallery that is completely plugged — one block high, packed to 8/8 along
//! its whole length — transmits *pressure* (it drives jets, fracture and the
//! boiling point) but no *mass*: moving a unit through it would need a global
//! incompressible solve, and a local automaton cannot do that without either an
//! energy drift or an oscillation. The Atlas therefore tells the player to build
//! culverts **two blocks high**, which is what real dams do anyway.
//!
//! Every step is integer, allocation-free and branch-predictable. There is no
//! `sqrt` in the flow path: the only root is in [`torricelli_velocity`], computed
//! lazily for entities standing in a jet, never per voxel per tick.

use crate::config::Tunables;
use crate::dirs::{self, Dir, DIR_NEG_X, DIR_NEG_Y, DIR_NEG_Z, DIR_POS_X, DIR_POS_Y, DIR_POS_Z};
use crate::grid::{Grid, Neighbor};
use crate::thermo;
use crate::units::{isqrt_u32, Vel, MASS_FULL};
use crate::voxel::{aux, flags, Class, Species, Voxel};

/// Horizontal directions in deterministic order (X first, then Z).
///
/// The order matters for reproducibility: two neighbours with the same mass must
/// always be resolved the same way on the server and on a client replay.
pub const HORIZONTAL: [Dir; 4] = [DIR_POS_X, DIR_NEG_X, DIR_POS_Z, DIR_NEG_Z];

/// Result of one hydro step on a cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlowResult {
    /// Something moved / changed.
    pub changed: bool,
    /// The cell still has work to do next tick.
    pub active: bool,
    /// Total mass units that left the cell this tick.
    pub moved: u8,
    /// Flow direction that was actually used (for the renderer), if any.
    pub dir: Option<Dir>,
}

impl FlowResult {
    /// No-op result.
    #[must_use]
    pub const fn idle() -> FlowResult {
        FlowResult {
            changed: false,
            active: false,
            moved: 0,
            dir: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Capacity rules
// ---------------------------------------------------------------------------

/// How much mass can `target` accept from `coming`?
///
/// * empty space accepts a full block;
/// * the same liquid species shares volume up to 8;
/// * any gas shares volume with any other gas (steam + fire + air);
/// * a liquid over a gas is handled by [`density_swap`], which is a full swap,
///   so the "capacity" reported here is only 1 to signal that a swap is legal;
/// * different liquids (water vs. lava) never mix — their contact is a chemical
///   reaction, resolved by [`crate::thermo`].
#[inline]
#[must_use]
pub fn room_for(target: Voxel, coming: Voxel) -> u8 {
    match target.class() {
        Class::Void => MASS_FULL,
        Class::Gas => {
            if coming.is_gas() {
                MASS_FULL.saturating_sub(target.mass.min(MASS_FULL))
            } else {
                // A liquid can sink through gas by swapping with it.
                if target.mass < MASS_FULL {
                    1
                } else {
                    0
                }
            }
        }
        Class::Liquid => {
            if coming.is_gas() {
                1 // steam rises through water by swapping
            } else if same_liquid(target, coming) {
                MASS_FULL.saturating_sub(target.mass.min(MASS_FULL))
            } else {
                0
            }
        }
        Class::Solid => 0,
    }
}

/// `true` when two liquid cells hold the same substance (water and salt water
/// are *not* the same, but a fresh cell with `mass == 0` counts as empty).
#[inline]
#[must_use]
pub fn same_liquid(a: Voxel, b: Voxel) -> bool {
    a.species == b.species || (a.mass == 0 && b.mass == 0)
}

/// Density in `kg/m³` of whatever occupies the cell (0 for empty).
#[inline]
#[must_use]
pub fn cell_density(v: Voxel) -> u32 {
    v.effective_density()
}

// ---------------------------------------------------------------------------
// Pressure and elevation
// ---------------------------------------------------------------------------
//
// `pressure` holds the hydrostatic pressure of a cell, i.e. the weight of the
// water column *above* it, in units of `pressure_per_block` (250 Pa) per block
// of water. It is maintained by a local recurrence
//
//   p[i] = p[above] + pressure_per_block · mass[i]/8
//
// which costs one neighbour read per active cell, converges to the exact
// hydrostatic profile while a basin fills, and then stops changing — a settled
// body of water stops touching its pressure field, which is what lets the chunk
// fall asleep.

/// Absolute world elevation of a local voxel.
#[inline]
#[must_use]
pub fn elevation(grid: &Grid, slot: usize, idx: usize) -> i32 {
    grid.chunks[slot].coords[1] * dirs::CHUNK_SIZE as i32 + dirs::unindex(idx).1 as i32
}

/// Elevation of the top of the fluid a cell holds (its local surface).
#[inline]
#[must_use]
pub fn surface(v: Voxel, y: i32, t: &Tunables) -> i32 {
    let perp = t.pressure_per_block as i32;
    if v.is_fluid() && v.mass > 0 {
        y * perp + perp * v.mass as i32 / MASS_FULL as i32
    } else {
        y * perp
    }
}

/// The same surface, expressed as a pressure above sea level.
#[inline]
#[must_use]
pub fn surface_pressure(v: Voxel, y: i32, t: &Tunables) -> u16 {
    let perp = t.pressure_per_block as i32;
    let p = t.sea_level_pressure as i32 + surface(v, y, t) - perp * y;
    if p < 0 {
        0
    } else if p > u16::MAX as i32 {
        u16::MAX
    } else {
        p as u16
    }
}

// ---------------------------------------------------------------------------
// One cell, one tick
// ---------------------------------------------------------------------------

/// Advances a single fluid cell.
pub fn tick_cell(grid: &mut Grid, slot: usize, idx: usize, t: &Tunables) -> FlowResult {
    let v = grid.voxel(slot, idx);
    if !v.is_liquid() && !v.is_gas() {
        return FlowResult::idle();
    }
    if v.mass == 0 || v.has(flags::SOURCE) {
        // A source never moves; it only feeds its neighbours.
        return FlowResult::idle();
    }

    // ---- 0. refresh the hydrostatic pressure ------------------------------
    let head_changed = update_pressure(grid, slot, idx, t);
    let me = grid.voxel(slot, idx);

    // ---- 1. gases are compressible: they rise and spread -------------------
    if me.is_gas() {
        if let Some(res) = try_rise(grid, slot, idx, t) {
            return res;
        }
        if let Some(res) = try_spread(grid, slot, idx, t) {
            return res;
        }
        return FlowResult {
            changed: head_changed,
            active: head_changed,
            moved: 0,
            dir: None,
        };
    }

    // ---- 2. gravity --------------------------------------------------------
    if let Some(res) = try_fall(grid, slot, idx, t) {
        return res;
    }

    // ---- 3. levelling (hydraulic-head descent) ----------------------------
    if let Some(res) = try_level(grid, slot, idx, t) {
        return res;
    }

    FlowResult {
        changed: head_changed,
        active: head_changed || me.flags & (flags::BUBBLE | flags::FLASHING) != 0,
        moved: 0,
        dir: None,
    }
}

/// Gravity: pour the cell into whatever is below it.
fn try_fall(grid: &mut Grid, slot: usize, idx: usize, _t: &Tunables) -> Option<FlowResult> {
    let me = grid.voxel(slot, idx);
    let n = grid.neighbor(slot, idx, DIR_NEG_Y)?;
    let below = grid.chunks[n.chunk].get(n.index);
    let room = room_for(below, me);
    if room == 0 {
        return None;
    }
    if below.is_liquid() && !same_liquid(below, me) {
        return None; // water on lava: leave it for the quench reaction
    }
    if !same_liquid(below, me) && !below.is_empty_like() {
        // Density inversion (a liquid over a gas): swap the two cells wholesale.
        return Some(density_swap(grid, (slot, idx), n, DIR_NEG_Y));
    }
    let amount = me.mass.min(room);
    if amount == 0 {
        return None;
    }
    let moved = pour(grid, (slot, idx), n, amount, DIR_NEG_Y)?;
    Some(FlowResult {
        changed: true,
        active: moved < MASS_FULL,
        moved,
        dir: Some(DIR_NEG_Y),
    })
}

/// Buoyancy: a gas rises into whatever is above it, swapping through liquids.
fn try_rise(grid: &mut Grid, slot: usize, idx: usize, _t: &Tunables) -> Option<FlowResult> {
    let me = grid.voxel(slot, idx);
    let n = grid.neighbor(slot, idx, DIR_POS_Y)?;
    let above = grid.chunks[n.chunk].get(n.index);
    let room = room_for(above, me);
    if room == 0 {
        return None;
    }
    if room == 1 && above.is_liquid() {
        return Some(density_swap(grid, (slot, idx), n, DIR_POS_Y));
    }
    let amount = me.mass.min(room);
    if amount == 0 {
        return None;
    }
    let moved = pour(grid, (slot, idx), n, amount, DIR_POS_Y)?;
    Some(FlowResult {
        changed: true,
        active: moved < MASS_FULL,
        moved,
        dir: Some(DIR_POS_Y),
    })
}

/// A gas expands sideways into any emptier cell (gases are compressible, so the
/// energy argument that forbids a horizontal *liquid* swap does not apply).
fn try_spread(grid: &mut Grid, slot: usize, idx: usize, _t: &Tunables) -> Option<FlowResult> {
    let me = grid.voxel(slot, idx);
    let mut best: Option<(Dir, Neighbor, u8)> = None;
    let mut best_mass = me.mass;
    for d in HORIZONTAL {
        let n = match grid.neighbor(slot, idx, d) {
            Some(n) => n,
            // An open border is an infinite reservoir, not an exit from the
            // scan: keep probing the other three sides.
            None => continue,
        };
        let nv = grid.chunks[n.chunk].get(n.index);
        if nv.is_liquid() {
            continue; // gases only rise through liquids, never sideways
        }
        if room_for(nv, me) == 0 {
            continue;
        }
        if nv.mass < best_mass {
            best_mass = nv.mass;
            best = Some((d, n, nv.mass));
        }
    }
    let (d, n, nb_mass) = best?;
    let diff = me.mass.saturating_sub(nb_mass);
    if diff < 2 {
        return None;
    }
    let amount = (diff / 2).min(me.mass).max(1);
    let moved = pour(grid, (slot, idx), n, amount, d)?;
    Some(FlowResult {
        changed: true,
        active: true,
        moved,
        dir: Some(d),
    })
}

/// Levelling by **hydraulic-head descent**.
///
/// Water moves horizontally from the cell with the higher free surface to the
/// cell with the lower one. The surface of a cell is its own mass plus the
/// column standing above it (see [`hydraulic_head`]), so this is the discrete
/// form of the communicating-vessels law: a body of water flows until both
/// surfaces are level, then stops. It is also what gives a thin trickle its
/// push — a one-unit cell in the middle of a column has a tall head behind it,
/// so it keeps moving, while a one-unit puddle sitting on bare rock has none.
///
/// # Termination
///
/// Let `q = H_a − H_b` be the head difference of a pair. The kernel moves
/// `Δ = min(q/2, …)` units, so `H_a` can never fall below `H_b`: the pair
/// converges monotonically and cannot overshoot into an oscillation. Moves are
/// only considered for `q ≥ 2` (a quarter block of head), and that hysteresis
/// band is what makes a levelled body a genuine fixed point — the reason its
/// chunk can go to sleep with zero CPU cost.
///
/// The single exception is a *supported* cell (it holds a full block, or there
/// is water standing on top of it) pouring one unit into a **completely empty**
/// neighbour. That move is energetically neutral, so it cannot raise the
/// potential, and the receiving cell is a one-unit film with nothing above it
/// that can never push the unit back — a ratchet, not a cycle. It is what lets
/// a culvert charge from a reservoir instead of stalling on the first
/// single-unit frontier it creates.
fn try_level(grid: &mut Grid, slot: usize, idx: usize, t: &Tunables) -> Option<FlowResult> {
    let me = grid.voxel(slot, idx);
    let rate_cap = ((t.flow_rate as u16 * t.horizontal_rate_eighths as u16) / 8).max(1) as u8;
    let my_head = hydraulic_head(me, t);
    let supported = column_above(me, t) > 0 || me.mass >= 2;
    let mut best: Option<(Dir, Neighbor, u8)> = None;

    for d in HORIZONTAL {
        let n = match grid.neighbor(slot, idx, d) {
            Some(n) => n,
            None => continue,
        };
        let nv = grid.chunks[n.chunk].get(n.index);
        let room = room_for(nv, me);
        if room == 0 {
            continue;
        }
        if nv.is_liquid() && !same_liquid(nv, me) {
            continue; // never mix water and lava
        }
        if !nv.is_empty_like() && !same_liquid(nv, me) {
            continue; // different substances only exchange vertically
        }
        let q = my_head - hydraulic_head(nv, t);
        let want = if q >= 2 {
            q / 2
        } else if q == 1 && nv.mass == 0 && supported {
            1
        } else {
            continue;
        };
        let amount = want
            .min(room as i32)
            .min(me.mass as i32)
            .min(rate_cap as i32) as u8;
        if amount == 0 {
            continue;
        }
        if best.is_none() || amount > best.as_ref().map_or(0, |b| b.2) {
            best = Some((d, n, amount));
        }
    }

    let (d, n, amount) = best?;
    let moved = pour(grid, (slot, idx), n, amount, d)?;
    Some(FlowResult {
        changed: true,
        active: true,
        moved,
        dir: Some(d),
    })
}

/// Moves `amount` mass units from `src` into `dst`, honouring the target class.
///
/// Returns the number of units actually transferred.
fn pour(
    grid: &mut Grid,
    src: (usize, usize),
    dst: Neighbor,
    amount: u8,
    dir: Dir,
) -> Option<u8> {
    let s = grid.voxel(src.0, src.1);
    let d = grid.chunks[dst.chunk].get(dst.index);
    if amount == 0 || amount > s.mass {
        return None;
    }

    let mut ns = s;
    let mut nd = d;

    match d.class() {
        Class::Void => {
            nd = s;
            nd.mass = amount;
            nd.flags = flags::ACTIVE | flags::MOVED;
            nd.aux = aux::with_flow(s.aux, dir);
            ns.mass = s.mass - amount;
        }
        Class::Gas => {
            if s.is_gas() {
                nd.mass = d.mass.saturating_add(amount).min(MASS_FULL);
                nd.flags |= flags::ACTIVE | flags::MOVED;
                nd.aux = aux::with_flow(nd.aux, dir);
                ns.mass = s.mass - amount;
            } else {
                return None; // handled by density_swap
            }
        }
        Class::Liquid => {
            if s.is_gas() {
                return None; // handled by density_swap
            }
            if !same_liquid(d, s) {
                return None;
            }
            nd.mass = d.mass.saturating_add(amount).min(MASS_FULL);
            nd.flags |= flags::ACTIVE | flags::MOVED;
            nd.aux = aux::with_flow(nd.aux, dir);
            // Mixing equalises temperature proportionally to the mass moved.
            nd.temp = mix_temp(d, s, amount);
            ns.mass = s.mass - amount;
        }
        Class::Solid => return None,
    }

    if ns.mass == 0 {
        ns.species = Species::Air as u8;
        ns.flags = flags::ACTIVE;
        ns.aux = aux::FLOW_NONE;
    } else {
        ns.flags |= flags::ACTIVE | flags::MOVED;
    }

    grid.put(src.0, src.1, ns);
    grid.put(dst.chunk, dst.index, nd);
    grid.wake_next(src.0, src.1);
    grid.wake_next(dst.chunk, dst.index);
    Some(amount)
}

/// Exchanges the contents of two cells (density inversion / bubble rise).
fn density_swap(grid: &mut Grid, a: (usize, usize), b: Neighbor, dir: Dir) -> FlowResult {
    let va = grid.voxel(a.0, a.1);
    let vb = grid.chunks[b.chunk].get(b.index);
    let mut na = vb;
    let mut nb = va;
    na.flags |= flags::ACTIVE | flags::MOVED;
    nb.flags |= flags::ACTIVE | flags::MOVED;
    na.aux = aux::with_flow(na.aux, dir);
    nb.aux = aux::with_flow(nb.aux, dirs::opposite(dir));
    grid.put(a.0, a.1, na);
    grid.put(b.chunk, b.index, nb);
    grid.wake_next(a.0, a.1);
    grid.wake_next(b.chunk, b.index);
    FlowResult {
        changed: true,
        active: true,
        moved: va.mass,
        dir: Some(dir),
    }
}

/// Mass-weighted temperature of `amount` units of `src` joining `dst`.
#[inline]
fn mix_temp(dst: Voxel, src: Voxel, amount: u8) -> crate::units::Temp {
    let total = dst.mass as i32 + amount as i32;
    if total <= 0 {
        return src.temp;
    }
    let avg = (dst.temp as i32 * dst.mass as i32 + src.temp as i32 * amount as i32) / total;
    crate::units::clamp_temp(avg)
}

// ---------------------------------------------------------------------------
// Hydrostatic pressure
// ---------------------------------------------------------------------------

/// Recomputes the hydrostatic pressure of one cell from the column above it.
///
/// `p[i] = p[above] + ρg·(mass[i]/8)`
///
/// The recurrence is local, so it costs one neighbour read per active cell and
/// needs no column scan. Applied repeatedly while a basin fills it converges to
/// the exact hydrostatic profile — and because it converges to a *fixed point*,
/// a settled basin stops changing pressure and its chunk can sleep.
pub fn update_pressure(grid: &mut Grid, slot: usize, idx: usize, t: &Tunables) -> bool {
    let v = grid.voxel(slot, idx);
    if !v.is_fluid() || v.mass == 0 {
        return false;
    }
    let from_above = match grid.neighbor_voxel(slot, idx, DIR_POS_Y) {
        Some(a) if a.is_fluid() && a.mass > 0 => a.pressure as u32,
        _ => t.sea_level_pressure as u32,
    };
    let head = from_above + (t.pressure_per_block as u32 * v.mass as u32) / MASS_FULL as u32;
    let new_p = if head > u16::MAX as u32 {
        u16::MAX
    } else {
        head as u16
    };
    if new_p == v.pressure {
        return false;
    }
    let mut nv = v;
    nv.pressure = new_p;
    nv.flags |= flags::ACTIVE;
    grid.put(slot, idx, nv);
    // Pressure changes must propagate down the column.
    if let Some(n) = grid.neighbor(slot, idx, DIR_NEG_Y) {
        grid.wake_next(n.chunk, n.index);
    }
    true
}

/// Pressure in quarter-pascals above the local sea level — the "excess head".
#[must_use]
pub fn excess_pressure(v: Voxel, t: &Tunables) -> u16 {
    v.pressure.saturating_sub(t.sea_level_pressure)
}

/// Largest column (in 1/8-block units) the levelling kernel trusts from the
/// pressure field: 512 units == 64 full blocks of water. Deeper than any dam a
/// player will build, and small enough to keep every intermediate `i32` tiny.
pub const MAX_HEAD_UNITS: i32 = 512;

/// Hydraulic head of a liquid cell: the mass standing **at and above** it, in
/// `1/8`-block units.
///
/// The pressure field already stores exactly this quantity — [`update_pressure`]
/// maintains `p = p[above] + ρg·mass/8`, i.e. the weight of the whole column
/// down to the *bottom* of the cell — so the head costs one multiply and one
/// divide instead of a column walk in the hot loop. Gases and empty cells carry
/// no column and report 0.
#[inline]
#[must_use]
pub fn hydraulic_head(v: Voxel, t: &Tunables) -> i32 {
    if !v.is_liquid() {
        return 0;
    }
    let mass = v.mass as i32;
    let per_block = t.pressure_per_block.max(1) as i32;
    let excess = (v.pressure as i32 - t.sea_level_pressure as i32).max(0);
    let column = ((excess * MASS_FULL as i32) / per_block).min(MAX_HEAD_UNITS);
    // A freshly created cell still carries the sea-level default until
    // `update_pressure` runs; never report less than the cell's own mass.
    if column > mass {
        column
    } else {
        mass
    }
}

/// Mass standing *above* this cell — the part of the head that can push a
/// horizontal transfer. Zero for a surface cell sitting on bare rock.
#[inline]
#[must_use]
pub fn column_above(v: Voxel, t: &Tunables) -> i32 {
    (hydraulic_head(v, t) - v.mass as i32).max(0)
}

/// Column height in mass units: how much fluid stands above this cell.
///
/// Returns the sum of `mass` in the vertical run starting at `idx`, walking up.
/// Used by the pressure HUD and by the "how deep am I" calculations.
#[must_use]
pub fn column_mass_above(grid: &Grid, slot: usize, idx: usize) -> u32 {
    let mut total = 0u32;
    let mut cur = (slot, idx);
    // 512 blocks is the world height; the loop is bounded so a corrupted chunk
    // cannot spin forever.
    for _ in 0..512 {
        match grid.neighbor(cur.0, cur.1, DIR_POS_Y) {
            Some(n) => {
                let v = grid.chunks[n.chunk].get(n.index);
                if !v.is_fluid() || v.mass == 0 {
                    break;
                }
                total += v.mass as u32;
                cur = (n.chunk, n.index);
            }
            None => break,
        }
    }
    total
}

// ---------------------------------------------------------------------------
// Jets
// ---------------------------------------------------------------------------

/// Torricelli's law in fixed point: `v = sqrt(2·g·h)`.
///
/// `head` is an excess pressure in quarter-pascals, converted into metres
/// through the configured `pressure_per_block`. The result is in the engine's
/// 1/16 m/s velocity unit, so a 1 m head yields ~70 (4.4 m/s) and a 10 m head
/// ~222 (13.9 m/s) — a jet that visibly crosses a cave.
#[must_use]
pub fn torricelli_velocity(head: u16, t: &Tunables) -> Vel {
    let per_block = t.pressure_per_block.max(1) as u32;
    let head = (head as u32).min(t.jet_max_head);
    let height_cm = (head * 100) / per_block; // height in centimetres
    // v = sqrt(2·9.81·h) m/s, expressed in 1/16 m/s:
    //   v16 = 16·sqrt(2·9.81·h)  →  v16² = 5023·h[cm]/100
    let v16 = isqrt_u32((5_023 * height_cm) / 100);
    // `jet_velocity_per_head` is a tuning multiplier with a neutral value of 3.
    let scaled = (v16 * t.jet_velocity_per_head.max(1) as u32) / 3;
    if scaled > Vel::MAX as u32 {
        Vel::MAX
    } else {
        scaled as Vel
    }
}

/// Velocity of the jet a cell is emitting, in 1/16 m/s per axis.
///
/// The direction comes from the flow hint the automaton left in `aux`; the
/// magnitude from the hydrostatic head. Entities standing in the stream are
/// pushed by the entity layer (see [`crate::entities`]) — this is what makes a
/// breached dam knock a player off their feet.
#[must_use]
pub fn jet_velocity(grid: &Grid, slot: usize, idx: usize, t: &Tunables) -> [Vel; 3] {
    let v = grid.voxel(slot, idx);
    if v.mass == 0 {
        return [0; 3];
    }
    let dir_bits = aux::flow_dir(v.aux);
    if dir_bits >= 6 {
        return [0; 3];
    }
    let speed = torricelli_velocity(excess_pressure(v, t), t);
    let mut out = [0i16; 3];
    match dir_bits {
        DIR_POS_X => out[0] = speed,
        DIR_NEG_X => out[0] = -speed,
        DIR_POS_Y => out[1] = speed,
        DIR_NEG_Y => out[1] = -speed,
        DIR_POS_Z => out[2] = speed,
        _ => out[2] = -speed,
    }
    out
}

// ---------------------------------------------------------------------------
// Fracture
// ---------------------------------------------------------------------------

/// Grinds down brittle neighbours of a pressurised cell.
///
/// The yield pressure comes from the material table, so glass needs a ~2 m head,
/// planks ~2.75 m, leaves a few centimetres — while stone (35 m) and obsidian
/// (40 m) hold a normal dam.
pub fn fracture(grid: &mut Grid, slot: usize, idx: usize, t: &Tunables) -> Option<thermo::Event> {
    let v = grid.voxel(slot, idx);
    if !v.is_fluid() || v.mass == 0 {
        return None;
    }
    let excess = excess_pressure(v, t);
    if excess < t.damage_threshold {
        return None;
    }
    for d in dirs::DIRS {
        let n = match grid.neighbor(slot, idx, d) {
            Some(n) => n,
            None => continue,
        };
        let nv = grid.chunks[n.chunk].get(n.index);
        if nv.class() != Class::Solid {
            continue;
        }
        // `apply_pressure_damage` applies the per-material yield check itself.
        if let Some(ev) = thermo::apply_pressure_damage(grid, n.chunk, n.index, excess, t) {
            return Some(ev);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Buoyancy for entities and loose blocks
// ---------------------------------------------------------------------------

/// Net buoyant acceleration for an entity of the given density inside a fluid.
///
/// `a = g·(ρ_fluid − ρ_entity) / ρ_entity`, in 1/16 m/s². Positive floats.
/// Oak (700 kg/m³) floats with ~+0.42 g, iron armour (7 870) sinks with
/// ~−0.87 g — matching the "heavy armour drags you down" rule of the Atlas.
#[must_use]
pub fn buoyancy(entity_density: u32, fluid_density: u32) -> i32 {
    if entity_density == 0 {
        return 0;
    }
    let g16 = crate::units::GRAVITY_Q4;
    ((fluid_density as i64 - entity_density as i64) * g16 as i64 / entity_density as i64) as i32
}

/// Fluid density at a world position, or 0 when the cell is not submerged.
#[must_use]
pub fn fluid_density_at(grid: &Grid, x: i32, y: i32, z: i32) -> u32 {
    match grid.get_world(x, y, z) {
        Some(v) if v.is_fluid() && v.mass > 0 => cell_density(v),
        _ => 0,
    }
}

/// `true` when a block species floats on water (used for loose blocks/pumice).
#[must_use]
pub fn floats_on_water(species: Species) -> bool {
    species.density() < Species::Water.density() && species.class() == Class::Solid
}

/// Makes a floating solid that is fully submerged rise one cell.
///
/// Only used for *loose* blocks the engine marks as falling entities (pumice,
/// snow) — terrain never moves by itself.
pub fn float_step(grid: &mut Grid, slot: usize, idx: usize) -> bool {
    let v = grid.voxel(slot, idx);
    if v.class() != Class::Solid || !floats_on_water(v.species()) {
        return false;
    }
    if let Some(n) = grid.neighbor(slot, idx, DIR_POS_Y) {
        let above = grid.chunks[n.chunk].get(n.index);
        if above.is_fluid() && above.mass >= 4 {
            return density_swap(grid, (slot, idx), n, DIR_POS_Y).changed;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Grid;

    fn world() -> (Grid, Tunables) {
        let mut g = Grid::with_capacity(3, 4);
        g.ensure_chunk([0, 0, 0]);
        (g, Tunables::DEFAULT)
    }

    fn slot(g: &Grid) -> usize {
        g.find([0, 0, 0]).unwrap()
    }

    /// Total mass of a vertical column at `(x, z)`.
    fn column(g: &Grid, x: usize, z: usize) -> u32 {
        let s = slot(g);
        let mut total = 0;
        for y in 0..16 {
            let v = g.chunks[s].voxels[dirs::index(x, y, z)];
            if v.is_fluid() {
                total += v.mass as u32;
            }
        }
        total
    }

    #[test]
    fn water_falls_into_void() {
        let (mut g, t) = world();
        let s = slot(&g);
        let top = dirs::index(4, 6, 4);
        g.put(s, top, Voxel::water(20));
        let below = dirs::index(4, 5, 4);
        let res = tick_cell(&mut g, s, top, &t);
        assert!(res.changed);
        assert_eq!(res.dir, Some(DIR_NEG_Y));
        assert_eq!(g.chunks[s].voxels[below].species(), Species::Water);
        assert_eq!(g.chunks[s].voxels[below].mass, MASS_FULL);
        assert_eq!(g.chunks[s].voxels[top].mass, 0);
    }

    #[test]
    fn mass_is_conserved_while_falling() {
        let (mut g, t) = world();
        let s = slot(&g);
        let idx = dirs::index(4, 9, 4);
        let mut v = Voxel::water(20);
        v.mass = 5;
        g.put(s, idx, v);
        let before = g.total_mass();
        for _ in 0..8 {
            tick_cell(&mut g, s, idx, &t);
        }
        assert_eq!(g.total_mass(), before);
    }

    #[test]
    fn water_levels_out_horizontally() {
        let (mut g, t) = world();
        let s = slot(&g);
        // Solid floor at y=1 so nothing falls away.
        for x in 0..16 {
            for z in 0..16 {
                g.put(s, dirs::index(x, 1, z), Voxel::stone(20));
            }
        }
        // A tower of water at x=3.
        for y in 2..8 {
            g.put(s, dirs::index(3, y, 3), Voxel::water(20));
        }
        let before = g.total_mass();
        for _ in 0..400 {
            for y in 2..10 {
                for z in 0..12 {
                    for x in 0..12 {
                        tick_cell(&mut g, s, dirs::index(x, y, z), &t);
                    }
                }
            }
        }
        assert_eq!(g.total_mass(), before, "levelling must not create mass");
        assert!(
            column(&g, 8, 3) > 0,
            "water must have spread across the floor: {} vs {}",
            column(&g, 3, 3),
            column(&g, 8, 3)
        );
        // The tower must have collapsed: its column is a puddle now.
        assert!(column(&g, 3, 3) < 4 * MASS_FULL as u32);
    }

    #[test]
    fn a_settled_puddle_stops_changing() {
        let (mut g, t) = world();
        let s = slot(&g);
        for x in 0..16 {
            for z in 0..16 {
                g.put(s, dirs::index(x, 1, z), Voxel::stone(20));
            }
        }
        for y in 2..6 {
            for x in 4..8 {
                g.put(s, dirs::index(x, y, 4), Voxel::water(20));
            }
        }
        let before = g.total_mass();
        let run = |g: &mut Grid| -> usize {
            let mut changed = 0;
            for y in 2..10 {
                for z in 0..10 {
                    for x in 0..12 {
                        let idx = dirs::index(x, y, z);
                        if tick_cell(g, s, idx, &t).changed {
                            changed += 1;
                        }
                    }
                }
            }
            changed
        };
        for _ in 0..200 {
            run(&mut g);
        }
        // Energy descends monotonically, so the automaton must reach a fixed point.
        let last = run(&mut g);
        assert_eq!(last, 0, "a settled puddle must stop moving");
        assert_eq!(g.total_mass(), before);
    }

    #[test]
    fn communicating_vessels_reach_the_same_height() {
        let (mut g, t) = world();
        let s = slot(&g);
        // Floor.
        for x in 0..16 {
            for z in 3..6 {
                g.put(s, dirs::index(x, 1, z), Voxel::stone(20));
            }
        }
        // Side walls of the channel at z=3 and z=5.
        for x in 1..14 {
            for y in 2..12 {
                g.put(s, dirs::index(x, y, 3), Voxel::stone(20));
                g.put(s, dirs::index(x, y, 5), Voxel::stone(20));
            }
        }
        // Dam between the two vessels, with a two-block-high culvert at y=2..3.
        for x in 6..10 {
            for y in 4..12 {
                g.put(s, dirs::index(x, y, 4), Voxel::stone(20));
            }
        }
        // End walls of the vessels.
        for y in 2..12 {
            for z in 3..6 {
                g.put(s, dirs::index(2, y, z), Voxel::stone(20));
                g.put(s, dirs::index(13, y, z), Voxel::stone(20));
            }
        }
        // Fill the left vessel six blocks deep.
        for y in 2..8 {
            for x in 3..6 {
                g.put(s, dirs::index(x, y, 4), Voxel::water(20));
            }
        }
        let before = g.total_mass();
        for _ in 0..600 {
            for y in 2..12 {
                for x in 3..13 {
                    tick_cell(&mut g, s, dirs::index(x, y, 4), &t);
                }
            }
        }
        assert_eq!(g.total_mass(), before, "communicating vessels conserve mass");
        let left = column(&g, 4, 4) as i32;
        let right = column(&g, 11, 4) as i32;
        assert!(right > 0, "water must reach the second vessel");
        assert!(
            (left - right).abs() <= 2 * MASS_FULL as i32,
            "levels must equalise within two blocks: {left} vs {right}"
        );
        assert!(left < 6 * MASS_FULL as i32, "the filled arm must drain");
    }

    #[test]
    fn hydrostatic_pressure_grows_with_depth() {
        let (mut g, t) = world();
        let s = slot(&g);
        for y in 2..10 {
            g.put(s, dirs::index(5, y, 5), Voxel::water(20));
        }
        // Settle the pressure field.
        for _ in 0..20 {
            for y in 2..10 {
                update_pressure(&mut g, s, dirs::index(5, y, 5), &t);
            }
        }
        let top = g.chunks[s].voxels[dirs::index(5, 9, 5)].pressure;
        let bottom = g.chunks[s].voxels[dirs::index(5, 2, 5)].pressure;
        assert!(bottom > top, "deeper cells must be under more pressure");
        let expected = (7 * t.pressure_per_block as u32) as u16;
        assert!(
            bottom >= top + expected - t.pressure_per_block,
            "pressure must grow by ~one block of water per block of depth: {bottom} vs {top}"
        );
    }

    #[test]
    fn pressure_is_independent_of_the_cells_own_mass() {
        let (mut g, t) = world();
        let s = slot(&g);
        let idx = dirs::index(5, 5, 5);
        let mut full = Voxel::water(20);
        full.mass = 8;
        g.put(s, idx, full);
        update_pressure(&mut g, s, idx, &t);
        let p_full = g.chunks[s].voxels[idx].pressure;
        let mut thin = Voxel::water(20);
        thin.mass = 1;
        g.put(s, idx, thin);
        update_pressure(&mut g, s, idx, &t);
        let p_thin = g.chunks[s].voxels[idx].pressure;
        assert!(
            p_thin < p_full,
            "a shallow puddle must press less than a full block: {p_thin} vs {p_full}"
        );
    }

    #[test]
    fn water_flows_through_a_breach_in_a_wall() {
        let (mut g, t) = world();
        let s = slot(&g);
        // Floor and a vertical wall at x=8 with a single hole at y=2.
        for x in 0..16 {
            for z in 0..16 {
                g.put(s, dirs::index(x, 1, z), Voxel::stone(20));
            }
        }
        for y in 2..12 {
            for z in 0..16 {
                g.put(s, dirs::index(8, y, z), Voxel::stone(20));
            }
        }
        g.put(s, dirs::index(8, 2, 4), Voxel::AIR); // the breach
        for y in 2..10 {
            for x in 2..8 {
                g.put(s, dirs::index(x, y, 4), Voxel::water(20));
            }
        }
        let before = g.total_mass();
        for _ in 0..200 {
            for y in 2..12 {
                for z in 2..7 {
                    for x in 0..14 {
                        tick_cell(&mut g, s, dirs::index(x, y, z), &t);
                    }
                }
            }
        }
        assert_eq!(g.total_mass(), before);
        let beyond = column(&g, 10, 4);
        assert!(beyond > 0, "water must jet through the breach");
    }

    #[test]
    fn gas_rises_through_water_by_density_inversion() {
        let (mut g, t) = world();
        let s = slot(&g);
        let steam = dirs::index(4, 3, 4);
        let water = dirs::index(4, 4, 4);
        g.put(s, steam, Voxel::new(Species::Steam, 120));
        g.put(s, water, Voxel::water(20));
        let before = g.total_mass();
        let res = tick_cell(&mut g, s, steam, &t);
        assert!(res.changed);
        assert_eq!(g.chunks[s].voxels[water].species(), Species::Steam);
        assert_eq!(g.chunks[s].voxels[steam].species(), Species::Water);
        assert_eq!(g.total_mass(), before, "a swap cannot create mass");
    }

    #[test]
    fn steam_does_not_rise_into_stone() {
        let (mut g, t) = world();
        let s = slot(&g);
        let idx = dirs::index(4, 4, 4);
        g.put(s, idx, Voxel::new(Species::Steam, 120));
        g.put(s, dirs::index(4, 5, 4), Voxel::stone(20));
        let res = tick_cell(&mut g, s, idx, &t);
        // It may still spread sideways, but never up.
        assert_ne!(res.dir, Some(DIR_POS_Y));
        assert_eq!(g.chunks[s].voxels[dirs::index(4, 5, 4)].species(), Species::Stone);
    }

    #[test]
    fn water_and_lava_never_mix() {
        let (mut g, t) = world();
        let s = slot(&g);
        let w = dirs::index(4, 5, 4);
        let l = dirs::index(4, 4, 4);
        g.put(s, w, Voxel::water(20));
        g.put(s, l, Voxel::new(Species::Lava, 1_100));
        tick_cell(&mut g, s, w, &t);
        assert_eq!(g.chunks[s].voxels[l].species(), Species::Lava);
        assert_eq!(g.chunks[s].voxels[w].species(), Species::Water);
    }

    #[test]
    fn torricelli_matches_physics() {
        let t = Tunables::DEFAULT;
        // 1 m head: v = sqrt(2*9.81*1) = 4.43 m/s → 70 in 1/16 m/s.
        let v1 = torricelli_velocity(t.pressure_per_block, &t);
        assert!((v1 - 70).abs() <= 4, "1 m head → ~70, got {v1}");
        // 10 m head: 14 m/s → 224.
        let v10 = torricelli_velocity(t.pressure_per_block * 10, &t);
        assert!((v10 - 224).abs() <= 10, "10 m head → ~224, got {v10}");
        assert_eq!(torricelli_velocity(0, &t), 0);
        // Monotonic.
        assert!(torricelli_velocity(2_000, &t) > torricelli_velocity(1_000, &t));
    }

    #[test]
    fn jets_point_where_the_flow_went() {
        let (mut g, t) = world();
        let s = slot(&g);
        let idx = dirs::index(5, 5, 5);
        let mut v = Voxel::water(20);
        v.pressure = t.sea_level_pressure + t.pressure_per_block * 4;
        v.aux = aux::with_flow(0, DIR_POS_X);
        g.put(s, idx, v);
        let jet = jet_velocity(&g, s, idx, &t);
        assert!(jet[0] > 100, "a 4 m head must produce a strong +X jet: {jet:?}");
        assert_eq!(jet[1], 0);
    }

    #[test]
    fn buoyancy_floats_wood_and_sinks_iron() {
        assert!(buoyancy(700, 1000) > 0, "oak must float");
        assert!(buoyancy(7_870, 1000) < 0, "iron armour must sink");
        assert!(buoyancy(1_000, 1_000) == 0, "neutral buoyancy");
        assert!(buoyancy(0, 1_000) == 0);
        assert!(floats_on_water(Species::Wood));
        assert!(floats_on_water(Species::Pumice));
        assert!(!floats_on_water(Species::Metal));
        assert!(!floats_on_water(Species::Stone));
    }

    #[test]
    fn pumice_floats_up_through_water() {
        let (mut g, _t) = world();
        let s = slot(&g);
        let low = dirs::index(4, 4, 4);
        let above = dirs::index(4, 5, 4);
        g.put(s, low, Voxel::new(Species::Pumice, 20));
        g.put(s, above, Voxel::water(20));
        assert!(float_step(&mut g, s, low));
        assert_eq!(g.chunks[s].voxels[above].species(), Species::Pumice);
        assert_eq!(g.chunks[s].voxels[low].species(), Species::Water);
    }

    #[test]
    fn pressure_shatters_glass_walls_of_a_deep_tank() {
        let (mut g, mut t) = world();
        t.damage_threshold = 40;
        t.damage_break = 10;
        let s = slot(&g);
        let glass = dirs::index(6, 3, 4);
        g.put(s, glass, Voxel::new(Species::Glass, 20));
        let water = dirs::index(5, 3, 4);
        let mut w = Voxel::water(20);
        w.pressure = t.sea_level_pressure + t.pressure_per_block * 20;
        g.put(s, water, w);
        let mut broke = false;
        for _ in 0..20 {
            if fracture(&mut g, s, water, &t).is_some() {
                broke = true;
                break;
            }
        }
        assert!(broke, "a 20-block head must burst glass");
        assert_eq!(g.chunks[s].voxels[glass].species(), Species::Air);
    }

    #[test]
    fn gentle_pressure_leaves_glass_intact() {
        let (mut g, mut t) = world();
        t.damage_threshold = 40;
        let s = slot(&g);
        let glass = dirs::index(6, 3, 4);
        g.put(s, glass, Voxel::new(Species::Glass, 20));
        let water = dirs::index(5, 3, 4);
        let mut w = Voxel::water(20);
        // Only a 1-block head (40 units) — below the 80-unit glass yield.
        w.pressure = t.sea_level_pressure + 40;
        g.put(s, water, w);
        for _ in 0..50 {
            assert!(fracture(&mut g, s, water, &t).is_none());
        }
        assert_eq!(g.chunks[s].voxels[glass].species(), Species::Glass);
    }

    #[test]
    fn sources_never_move() {
        let (mut g, t) = world();
        let s = slot(&g);
        let idx = dirs::index(4, 4, 4);
        let mut v = Voxel::water(20);
        v.flags |= flags::SOURCE;
        g.put(s, idx, v);
        let res = tick_cell(&mut g, s, idx, &t);
        assert!(!res.changed);
        assert_eq!(g.chunks[s].voxels[idx].mass, MASS_FULL);
    }

    #[test]
    fn column_mass_counts_the_water_above() {
        let (mut g, _t) = world();
        let s = slot(&g);
        for y in 4..8 {
            g.put(s, dirs::index(2, y, 2), Voxel::water(20));
        }
        assert_eq!(column_mass_above(&g, s, dirs::index(2, 4, 2)), 3 * MASS_FULL as u32);
        assert_eq!(column_mass_above(&g, s, dirs::index(2, 7, 2)), 0);
    }

    #[test]
    fn flow_never_creates_or_destroys_mass_in_a_random_walk() {
        let (mut g, t) = world();
        let s = slot(&g);
        let mut state: u64 = 0xDEAD_BEEF;
        for i in 0..300 {
            let x = (i * 7) % 16;
            let y = (i * 3) % 16;
            let z = (i * 5) % 16;
            g.put(s, dirs::index(x, y, z), Voxel::water(30));
        }
        let before = g.total_mass();
        for step in 0..300 {
            state = crate::units::lcg_next(state);
            let x = (crate::units::lcg_below(state, 16)) as usize;
            let y = (crate::units::lcg_below(state, 16)) as usize;
            let z = (crate::units::lcg_below(state, 16)) as usize;
            tick_cell(&mut g, s, dirs::index(x, y, z), &t);
            if step % 50 == 0 {
                assert_eq!(g.total_mass(), before, "mass drift at step {step}");
            }
        }
        assert_eq!(g.total_mass(), before);
    }
}

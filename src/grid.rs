//! The Active Voxel Grid — chunk storage plus the sleep/wake machinery.
//!
//! # Why a separate "active" list
//!
//! The engine calls [`crate::Sim::tick`] once per server tick. A naive
//! implementation walks all `16³` voxels of every loaded chunk; a 12-chunk
//! basin then costs ~49 000 voxel evaluations *per tick*, forever, even when the
//! water has been still for an hour. This module inverts that: only voxels
//! whose state was **touched** (mass moved, temperature changed by more than the
//! epsilon, pressure adjusted) are put on a flat work list. A settled basin has
//! an empty list, so the tick degenerates into "check one `u16` counter" — a
//! true zero-CPU idle state.
//!
//! ```text
//!  ┌────────────── Chunk (32 KiB, cache-aligned) ──────────────┐
//!  │ voxels: [Voxel; 4096]   Y-major, 16×16 horizontal slabs   │
//!  │ flags: HAS_FLOW | HAS_THERMAL | DIRTY | ASLEEP            │
//!  │ stable_ticks: u16       ticks since the last change       │
//!  │ active_count: u16       voxels with the ACTIVE flag       │
//!  └───────────────────────────────────────────────────────────┘
//!          ▲                                   │
//!          │ push (slot << 12 | index)         │ drain
//!  ┌───────┴───────────────────────────────────▼───────────────┐
//!  │ Grid::front / Grid::back : [u32; MAX_ACTIVE]              │
//!  │ double-buffered work lists — this tick / next tick        │
//!  └───────────────────────────────────────────────────────────┘
//! ```
//!
//! Double buffering is what keeps the kernel re-entrancy-free: a voxel that is
//! awakened *by* the current tick lands in `back` and is picked up next tick,
//! so the sweep never revisits a cell (no double integration, no cascading
//! waterfalls that move 100 blocks in one tick).

use core::mem::size_of;

use crate::dirs::{
    self, Dir, BORDER_MASKS, CHUNK_SIZE, CHUNK_VOLUME, DIR_NEG_X, DIR_NEG_Y, DIR_NEG_Z, DIR_POS_X,
    DIR_POS_Y, DIR_POS_Z,
};
use crate::units::{chunk_seed, Mass, Temp};
use crate::voxel::{flags, Species, Voxel};

/// Maximum number of chunks resident in the simulation at once.
///
/// 24 chunks × 32 KiB ≈ 768 KiB — comparable to the engine's own chunk cache and
/// comfortably inside the default 16 MiB WASM heap. Overflowing the pool is not
/// an error: [`Grid::ensure_chunk`] evicts the deepest-sleeping chunk, which is
/// exactly what you want for a distant, settled ocean.
pub const MAX_CHUNKS: usize = 24;

/// Maximum number of active voxels tracked per tick.
///
/// 8192 entries cover two fully-awake chunks. When the list saturates the grid
/// degrades gracefully: the remaining voxels stay `ACTIVE` in their chunk and are
/// picked up on the next tick (temporal LOD instead of a frame spike).
pub const MAX_ACTIVE: usize = 8192;

/// Ticks a chunk must stay quiet before it is marked asleep.
///
/// 20 ticks = 1 s at the vanilla 20 Hz tick rate. A basin that is still for a
/// second is, for every practical purpose, still forever; 1 s is also short
/// enough that the user never notices the transition.
pub const SLEEP_AFTER_TICKS: u16 = 20;

/// Chunk state bitfield.
pub mod cflags {
    /// The chunk contains at least one voxel on the active list.
    pub const HAS_FLOW: u8 = 1 << 0;
    /// The chunk has a non-zero thermal gradient worth integrating.
    pub const HAS_THERMAL: u8 = 1 << 1;
    /// The client must be resent this chunk.
    pub const DIRTY: u8 = 1 << 2;
    /// The chunk is fully settled; every kernel skips it.
    pub const ASLEEP: u8 = 1 << 3;
    /// The chunk touches a neighbour that is not resident; boundary voxels are
    /// treated as infinite reservoirs (ocean edge semantics).
    pub const OPEN_BORDER: u8 = 1 << 4;
    /// A scripted / gameplay event forced this chunk awake this tick.
    pub const EVENT_WAKE: u8 = 1 << 5;
}

/// One simulation chunk: a flat `16³` voxel array plus its bookkeeping.
///
/// There is intentionally **no `Vec` inside**: the voxel store is a fixed array,
/// so the memory of a chunk never moves and the WASM module can hand the engine a
/// stable pointer into linear memory for zero-copy mesh generation.
pub struct Chunk {
    /// Voxels, Y-major (`index = y<<8 | z<<4 | x`).
    pub voxels: [Voxel; CHUNK_VOLUME],
    /// Chunk coordinates in chunk units.
    pub coords: [i32; 3],
    /// See [`cflags`].
    pub flags: u8,
    /// Contiguous quiet ticks, drives the sleep transition.
    pub stable_ticks: u16,
    /// Number of voxels whose `ACTIVE` flag is set.
    pub active_count: u16,
    /// Deterministic RNG state (bubble jitter, tie-breaks).
    pub seed: u64,
    /// Ambient temperature the chunk relaxes towards when nothing else happens.
    pub ambient: Temp,
    /// Monotonic tick index of the last time this chunk was evaluated. Used by
    /// the "too big time step" detector in [`Grid::run_active`].
    pub last_tick: u64,
}

impl Chunk {
    /// Creates an all-air chunk.
    #[must_use]
    pub fn new(coords: [i32; 3], world_seed: u64) -> Chunk {
        Chunk {
            voxels: [Voxel::AIR; CHUNK_VOLUME],
            coords,
            flags: cflags::ASLEEP,
            stable_ticks: SLEEP_AFTER_TICKS,
            active_count: 0,
            seed: chunk_seed(world_seed, coords[0], coords[1], coords[2], 0x48_59_44_52_4F),
            ambient: 20,
            last_tick: 0,
        }
    }

    /// Read access by flat index. Never panics: out-of-range reads yield air,
    /// which the kernels treat as an open border.
    #[inline]
    #[must_use]
    pub fn get(&self, idx: usize) -> Voxel {
        if idx < CHUNK_VOLUME {
            self.voxels[idx]
        } else {
            Voxel::VOID
        }
    }

    /// Write access by flat index (bounds-checked, no panic).
    #[inline]
    pub fn set(&mut self, idx: usize, v: Voxel) {
        if idx >= CHUNK_VOLUME {
            return;
        }
        let before = self.voxels[idx];
        if before.species != v.species || before.mass != v.mass {
            self.flags |= cflags::DIRTY | cflags::HAS_FLOW;
        }
        if before.temp != v.temp {
            self.flags |= cflags::DIRTY | cflags::HAS_THERMAL;
        }
        if before.pressure != v.pressure {
            self.flags |= cflags::DIRTY;
        }
        self.voxels[idx] = v;
    }

    /// `true` when no voxel in this chunk needs evaluation.
    #[inline]
    #[must_use]
    pub fn is_asleep(&self) -> bool {
        self.flags & cflags::ASLEEP != 0 && self.active_count == 0
    }

    /// Forces the chunk out of the sleep state.
    #[inline]
    pub fn wake(&mut self) {
        self.flags &= !cflags::ASLEEP;
        self.flags |= cflags::EVENT_WAKE;
        self.stable_ticks = 0;
    }

    /// Puts the chunk to sleep. `active_count` is left untouched: if it is
    /// stale, [`Grid::run_active`] will notice on the next wake.
    #[inline]
    pub fn sleep_now(&mut self) {
        self.flags |= cflags::ASLEEP;
        self.flags &= !(cflags::HAS_FLOW | cflags::HAS_THERMAL);
    }

    /// Number of fluid voxels (used by the mesh budgeter / debug HUD).
    #[must_use]
    pub fn fluid_count(&self) -> u32 {
        let mut n = 0;
        for v in self.voxels.iter() {
            if v.is_fluid() && v.mass > 0 {
                n += 1;
            }
        }
        n
    }

    /// Raises the `ACTIVE` flag on every voxel of the chunk and clears `ASLEEP`.
    ///
    /// This only touches *chunk* state; the per-voxel `ACTIVE` flag is owned by
    /// the scheduler, because "ACTIVE == on a work list" is the invariant that
    /// keeps [`crate::grid::Grid::wake`] duplicate-free. A wake that also set the
    /// voxel flags without queueing anything would silently strand the whole
    /// chunk (marked awake, never evaluated).
    ///
    /// To actually schedule the work use [`Grid::wake_all`], which pushes every
    /// voxel onto the work list.
    pub fn wake_all(&mut self) {
        self.wake();
        for v in self.voxels.iter_mut() {
            v.flags &= !flags::SLEEPING;
        }
    }

    /// Clears the per-tick `MOVED`/`TRANSITIONED` markers before a kernel run.
    pub fn clear_tick_marks(&mut self) {
        for v in self.voxels.iter_mut() {
            v.flags &= !(flags::MOVED | flags::TRANSITIONED);
        }
    }

    /// Computes the total fluid mass (8 = one full block) — the invariant the
    /// conservation tests assert against.
    #[must_use]
    pub fn total_mass(&self) -> u32 {
        let mut m = 0u32;
        for v in self.voxels.iter() {
            if v.is_fluid() {
                m += v.mass as u32;
            }
        }
        m
    }

    /// Total sensible heat in kilojoules (approximate, used by tests).
    #[must_use]
    pub fn total_heat_kj(&self) -> i64 {
        let mut h = 0i64;
        for v in self.voxels.iter() {
            h += v.sensible_heat() as i64;
        }
        h
    }

    /// Fills the chunk with a species at a temperature (scenarios, tests, the
    /// `/hd fill` command).
    pub fn fill(&mut self, species: Species, temp: Temp, mass: Mass) {
        let mut v = Voxel::new(species, temp);
        v.mass = mass;
        for slot in self.voxels.iter_mut() {
            *slot = v;
        }
        self.wake_all();
    }

    /// Fills a box given in local coordinates.
    pub fn fill_box(
        &mut self,
        from: [usize; 3],
        to: [usize; 3],
        species: Species,
        temp: Temp,
        mass: Mass,
    ) {
        let mut v = Voxel::new(species, temp);
        v.mass = mass;
        for y in from[1]..=to[1].min(CHUNK_SIZE - 1) {
            for z in from[2]..=to[2].min(CHUNK_SIZE - 1) {
                for x in from[0]..=to[0].min(CHUNK_SIZE - 1) {
                    let i = dirs::index(x, y, z);
                    self.voxels[i] = v;
                }
            }
        }
        self.wake_all();
    }
}

impl core::fmt::Debug for Chunk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Chunk")
            .field("coords", &self.coords)
            .field("flags", &self.flags)
            .field("active_count", &self.active_count)
            .field("stable_ticks", &self.stable_ticks)
            .field("ambient", &self.ambient)
            .finish()
    }
}

/// Neighbour resolution result: the chunk slot and the flat index inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Neighbor {
    /// Index into [`Grid::chunks`].
    pub chunk: usize,
    /// Flat voxel index inside that chunk.
    pub index: usize,
}

/// The simulation world: a small pool of resident chunks plus the active list.
pub struct Grid {
    /// Resident chunks. Slots are reused; a free slot has `coords == [i32::MIN; 3]`.
    pub chunks: Vec<Chunk>,
    /// Work list for the current tick (packed `slot << 12 | index`).
    ///
    /// Pre-reserved to [`MAX_ACTIVE`] at construction, so `push` never calls the
    /// allocator inside the simulation loop.
    pub front: Vec<u32>,
    /// Work list produced by the current tick, consumed by the next one.
    pub back: Vec<u32>,
    /// Monotonic tick counter.
    pub tick: u64,
    /// World seed for deterministic jitter.
    pub world_seed: u64,
    /// Global ambient temperature used for newly created chunks.
    pub ambient: Temp,
    /// Diagnostics for the current tick.
    pub stats: Stats,
    /// Number of chunks evicted because the pool was full.
    pub evictions: u32,
}

/// Per-tick diagnostics, surfaced to `/hd stats` and the Atlas "engine" page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Voxels present on the active list at the start of the tick.
    pub active: u32,
    /// Voxels actually evaluated (after the budget cut).
    pub processed: u32,
    /// Newly awakened voxels queued for the next tick.
    pub requeued: u32,
    /// Number of resident chunks.
    pub chunks: u32,
    /// Number of asleep chunks.
    pub asleep: u32,
    /// Total fluid mass in the world (conservation counter).
    pub total_mass: u32,
}

impl core::fmt::Debug for Grid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Grid")
            .field("chunks", &self.resident_chunks())
            .field("active", &self.front.len())
            .field("tick", &self.tick)
            .field("stats", &self.stats)
            .finish()
    }
}

impl Default for Grid {
    fn default() -> Self {
        Grid::new(0x48_59_44_52_4F_4E_41_55)
    }
}

impl Grid {
    /// Creates an empty grid with the default chunk budget.
    #[must_use]
    pub fn new(world_seed: u64) -> Grid {
        Grid::with_capacity(world_seed, MAX_CHUNKS)
    }

    /// Creates an empty grid with an explicit chunk budget.
    ///
    /// This is the **only** heap allocation in the entire mod: `capacity` chunks
    /// are allocated once, at `horizon_on_world_load`, and never again.
    #[must_use]
    pub fn with_capacity(world_seed: u64, capacity: usize) -> Grid {
        let mut chunks = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            let mut c = Chunk::new([i32::MIN; 3], world_seed);
            // A placeholder slot must never appear "resident".
            c.flags |= cflags::ASLEEP;
            chunks.push(c);
        }
        Grid {
            chunks,
            front: Vec::with_capacity(MAX_ACTIVE),
            back: Vec::with_capacity(MAX_ACTIVE),
            tick: 0,
            world_seed,
            ambient: 20,
            stats: Stats::default(),
            evictions: 0,
        }
    }

    // -----------------------------------------------------------------------
    // Chunk table
    // -----------------------------------------------------------------------

    /// Finds a resident chunk by coordinate.
    #[inline]
    #[must_use]
    pub fn find(&self, coords: [i32; 3]) -> Option<usize> {
        if coords[0] == i32::MIN {
            // Sentinel for a free slot: never a valid chunk coordinate.
            return None;
        }
        // Linear scan: the pool holds ≤ 24 chunks and the coordinate compare is
        // a single 12-byte SIMD-able test, which beats hashing a 3-tuple.
        let mut i = 0;
        while i < self.chunks.len() {
            let c = &self.chunks[i];
            if c.coords[0] == coords[0] && c.coords[1] == coords[1] && c.coords[2] == coords[2] {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Returns the slot of a resident chunk, allocating (or evicting) if needed.
    pub fn ensure_chunk(&mut self, coords: [i32; 3]) -> usize {
        if let Some(slot) = self.find(coords) {
            return slot;
        }
        // Free slot?
        let mut victim: Option<usize> = None;
        for (i, c) in self.chunks.iter().enumerate() {
            if c.coords[0] == i32::MIN {
                victim = Some(i);
                break;
            }
        }
        let slot = match victim {
            Some(i) => i,
            None => {
                // Evict the chunk that has been asleep the longest.
                let mut best = 0usize;
                let mut best_ticks = u16::MIN;
                for (i, c) in self.chunks.iter().enumerate() {
                    let score = c.stable_ticks;
                    if score >= best_ticks {
                        best_ticks = score;
                        best = i;
                    }
                }
                self.evictions += 1;
                best
            }
        };
        let seed = self.world_seed;
        let ambient = self.ambient;
        let mut chunk = Chunk::new(coords, seed);
        chunk.ambient = ambient;
        self.chunks[slot] = chunk;
        // Tear down the stale entries that pointed at the old chunk.
        self.purge_slot(slot);
        slot
    }

    /// Removes every active-list entry belonging to `slot`.
    fn purge_slot(&mut self, slot: usize) {
        let shift = 12u32;
        let want = (slot as u32) << shift;
        let mut w = 0;
        for r in 0..self.front.len() {
            let entry = self.front[r];
            if (entry & !0xFFF) != want {
                self.front[w] = entry;
                w += 1;
            }
        }
        self.front.truncate(w);
        let mut w = 0;
        for r in 0..self.back.len() {
            let entry = self.back[r];
            if (entry & !0xFFF) != want {
                self.back[w] = entry;
                w += 1;
            }
        }
        self.back.truncate(w);
    }

    /// Chunk lookup that creates the chunk on demand (used by the scenario and
    /// worldgen hooks).
    pub fn chunk_mut(&mut self, coords: [i32; 3]) -> &mut Chunk {
        let slot = self.ensure_chunk(coords);
        &mut self.chunks[slot]
    }

    /// Read-only chunk access.
    #[must_use]
    pub fn chunk(&self, coords: [i32; 3]) -> Option<&Chunk> {
        self.find(coords).map(|s| &self.chunks[s])
    }

    /// Number of resident (non-placeholder) chunks.
    #[must_use]
    pub fn resident_chunks(&self) -> usize {
        self.chunks.iter().filter(|c| c.coords[0] != i32::MIN).count()
    }

    // -----------------------------------------------------------------------
    // Voxel addressing
    // -----------------------------------------------------------------------

    /// Splits a world-space block coordinate into `(chunk coords, local index)`.
    #[inline]
    #[must_use]
    pub fn world_to_local(x: i32, y: i32, z: i32) -> ([i32; 3], usize) {
        let cs = CHUNK_SIZE as i32;
        // `div_euclid` keeps negative coordinates correct; `>>` on negative
        // values would round towards -inf but then `& mask` on a negative i32
        // relies on two's complement, so be explicit.
        let cx = x.div_euclid(cs);
        let cy = y.div_euclid(cs);
        let cz = z.div_euclid(cs);
        let lx = x.rem_euclid(cs) as usize;
        let ly = y.rem_euclid(cs) as usize;
        let lz = z.rem_euclid(cs) as usize;
        ([cx, cy, cz], dirs::index(lx, ly, lz))
    }

    /// Reads a voxel in world space; `None` when the chunk is not resident.
    #[must_use]
    pub fn get_world(&self, x: i32, y: i32, z: i32) -> Option<Voxel> {
        let (coords, idx) = Grid::world_to_local(x, y, z);
        self.chunk(coords).map(|c| c.voxels[idx])
    }

    /// Writes a voxel in world space, creating the chunk if required and waking
    /// the six neighbours so the change propagates.
    ///
    /// The write goes through [`Grid::put`], so the caller cannot fake the
    /// scheduler state: the cell is queued by `wake`, never by its own flags.
    pub fn set_world(&mut self, x: i32, y: i32, z: i32, v: Voxel) {
        let (coords, idx) = Grid::world_to_local(x, y, z);
        let slot = self.ensure_chunk(coords);
        self.put(slot, idx, v);
        self.wake(slot, idx);
        // Boundary writes must wake the neighbour chunk as well.
        for d in dirs::DIRS {
            if BORDER_MASKS[idx] & (1 << d) != 0 {
                if let Some(n) = self.neighbor(slot, idx, d) {
                    self.wake(n.chunk, n.index);
                }
            }
        }
    }

    /// Resolves the neighbour of `(slot, idx)` along `d`, crossing chunk borders.
    ///
    /// Returns `None` when the neighbour lives in a chunk that is not resident.
    /// Callers treat `None` as an infinite reservoir / open boundary, which is
    /// what makes an ocean edge behave like an ocean edge.
    #[inline]
    #[must_use]
    pub fn neighbor(&self, slot: usize, idx: usize, d: Dir) -> Option<Neighbor> {
        if BORDER_MASKS[idx] & (1 << d) == 0 {
            let step = dirs::step(d);
            let nidx = if step < 0 {
                idx - (-step) as usize
            } else {
                idx + step as usize
            };
            return Some(Neighbor {
                chunk: slot,
                index: nidx,
            });
        }
        let (x, y, z) = dirs::unindex(idx);
        let mut c = self.chunks[slot].coords;
        let cs = CHUNK_SIZE as i32;
        let (nx, ny, nz) = match d {
            DIR_NEG_X => {
                c[0] -= 1;
                (cs - 1, y as i32, z as i32)
            }
            DIR_POS_X => {
                c[0] += 1;
                (0, y as i32, z as i32)
            }
            DIR_NEG_Y => {
                c[1] -= 1;
                (x as i32, cs - 1, z as i32)
            }
            DIR_POS_Y => {
                c[1] += 1;
                (x as i32, 0, z as i32)
            }
            DIR_NEG_Z => {
                c[2] -= 1;
                (x as i32, y as i32, cs - 1)
            }
            _ => {
                c[2] += 1;
                (x as i32, y as i32, 0)
            }
        };
        if c[0] == i32::MIN {
            return None;
        }
        let nslot = self.find(c)?;
        Some(Neighbor {
            chunk: nslot,
            index: dirs::index(nx as usize, ny as usize, nz as usize),
        })
    }

    /// Read access to the whole voxel store of a chunk.
    ///
    /// Out-of-range slots yield a shared all-air chunk instead of panicking.
    #[must_use]
    pub fn voxels(&self, slot: usize) -> &[Voxel; CHUNK_VOLUME] {
        static EMPTY: [Voxel; CHUNK_VOLUME] = [Voxel::AIR; CHUNK_VOLUME];
        if slot < self.chunks.len() {
            &self.chunks[slot].voxels
        } else {
            &EMPTY
        }
    }

    /// Reads a voxel by `(slot, index)`, returning `Voxel::VOID` out of range.
    #[inline]
    #[must_use]
    pub fn voxel(&self, slot: usize, idx: usize) -> Voxel {
        if slot >= self.chunks.len() {
            return Voxel::VOID;
        }
        self.chunks[slot].get(idx)
    }

    /// Writes a voxel by `(slot, index)`. Returns `true` when something changed.
    ///
    /// # Scheduling invariant
    ///
    /// `ACTIVE` means *"this cell is on a work list"*. A write may never
    /// manufacture that state — a kernel that builds a fresh cell (steam from a
    /// boil, the destination of a pour, …) obviously writes `ACTIVE` in its own
    /// flags, but the cell is not on a list yet; keeping the bit would make the
    /// following `wake_next` a no-op and strand the cell forever. So the bit is
    /// preserved only when the cell really was queued before the write.
    #[inline]
    pub fn put(&mut self, slot: usize, idx: usize, mut v: Voxel) -> bool {
        if slot >= self.chunks.len() || idx >= CHUNK_VOLUME {
            return false;
        }
        let old = self.chunks[slot].voxels[idx];
        if old.flags & flags::ACTIVE == 0 {
            v.flags &= !flags::ACTIVE;
        }
        if old == v {
            return false;
        }
        self.chunks[slot].set(idx, v);
        true
    }

    /// Reads the neighbour voxel along `d`; `None` = open boundary.
    #[inline]
    #[must_use]
    pub fn neighbor_voxel(&self, slot: usize, idx: usize, d: Dir) -> Option<Voxel> {
        match self.neighbor(slot, idx, d) {
            Some(n) => Some(self.chunks[n.chunk].get(n.index)),
            None => None,
        }
    }

    /// Temperature of the neighbour along `d`, or `None` for an open boundary.
    #[inline]
    #[must_use]
    pub fn neighbor_temp(&self, slot: usize, idx: usize, d: Dir) -> Option<Temp> {
        self.neighbor_voxel(slot, idx, d).map(|v| v.temp)
    }

    /// Temperatures of the six neighbours; open boundaries report `fallback`.
    ///
    /// One gather, no allocation: this is the canonical way a kernel samples its
    /// neighbourhood, and it is the reason the kernels never need `&mut` twice.
    #[inline]
    #[must_use]
    pub fn neighborhood(&self, slot: usize, idx: usize, fallback: Temp) -> [Temp; 6] {
        let mut out = [fallback; 6];
        let mut d = 0;
        while d < 6 {
            if let Some(t) = self.neighbor_temp(slot, idx, d as Dir) {
                out[d] = t;
            }
            d += 1;
        }
        out
    }

    /// The six neighbour voxels, with `None` for open boundaries.
    #[must_use]
    pub fn neighborhood_voxels(&self, slot: usize, idx: usize) -> [Option<Voxel>; 6] {
        let mut out = [None; 6];
        let mut d = 0;
        while d < 6 {
            out[d] = self.neighbor_voxel(slot, idx, d as Dir);
            d += 1;
        }
        out
    }

    // -----------------------------------------------------------------------
    // Active list
    // -----------------------------------------------------------------------

    /// Packs an entry for the work lists.
    #[inline]
    #[must_use]
    pub const fn pack(slot: usize, idx: usize) -> u32 {
        ((slot as u32) << 12) | (idx as u32 & 0xFFF)
    }

    /// Unpacks an entry into `(slot, index)`.
    #[inline]
    #[must_use]
    pub const fn unpack(entry: u32) -> (usize, usize) {
        ((entry >> 12) as usize, (entry & 0xFFF) as usize)
    }

    /// Puts a voxel on the *current* work list, touching its chunk state.
    ///
    /// Duplicate pushes are suppressed via the `ACTIVE` flag: the work list
    /// therefore never contains the same cell twice, which is what allows the
    /// kernels to be written as a plain `for` loop with no "already visited" set.
    #[inline]
    pub fn wake(&mut self, slot: usize, idx: usize) {
        if slot >= self.chunks.len() || idx >= CHUNK_VOLUME {
            return;
        }
        let already_queued = self.chunks[slot].voxels[idx].flags & flags::ACTIVE != 0;
        let queued_now = {
            let chunk = &mut self.chunks[slot];
            chunk.flags &= !cflags::ASLEEP;
            chunk.stable_ticks = 0;
            if already_queued {
                // Already on a work list: just make sure the chunk is awake.
                false
            } else {
                chunk.voxels[idx].flags |= flags::ACTIVE;
                chunk.voxels[idx].flags &= !flags::SLEEPING;
                chunk.active_count = chunk.active_count.saturating_add(1);
                if chunk.coords[0] != i32::MIN {
                    chunk.flags |= cflags::HAS_FLOW;
                }
                true
            }
        };
        if !queued_now {
            return;
        }
        if self.front.len() < MAX_ACTIVE {
            self.front.push(Grid::pack(slot, idx));
        } else if self.back.len() < MAX_ACTIVE {
            // Budget exhausted: hand the overflow to the next tick.
            self.back.push(Grid::pack(slot, idx));
        }
    }

    /// Queues a voxel for the **next** tick (used by phase transitions so they
    /// cannot cascade inside a single tick).
    ///
    /// The invariant maintained by the scheduler is *`ACTIVE` means queued*: an
    /// entry that is already active — on either list — is left alone, so the work
    /// lists can never contain a duplicate.
    #[inline]
    pub fn wake_next(&mut self, slot: usize, idx: usize) {
        if slot >= self.chunks.len() || idx >= CHUNK_VOLUME {
            return;
        }
        {
            let chunk = &mut self.chunks[slot];
            if chunk.voxels[idx].flags & flags::ACTIVE != 0 {
                return;
            }
            chunk.voxels[idx].flags |= flags::ACTIVE;
            chunk.voxels[idx].flags &= !flags::SLEEPING;
            chunk.flags &= !cflags::ASLEEP;
            chunk.stable_ticks = 0;
            chunk.active_count = chunk.active_count.saturating_add(1);
        }
        if self.back.len() < MAX_ACTIVE {
            self.back.push(Grid::pack(slot, idx));
        }
    }

    /// Wakes a voxel and its six neighbours — the standard "something changed
    /// here" broadcast used by every kernel.
    pub fn wake_with_neighbors(&mut self, slot: usize, idx: usize) {
        self.wake(slot, idx);
        for d in dirs::DIRS {
            if let Some(n) = self.neighbor(slot, idx, d) {
                self.wake(n.chunk, n.index);
            }
        }
        // Boundary voxels: wake the neighbour chunk even when the level of
        // detail says it is asleep, so an arriving chunk re-evaluates its
        // border against the new state.
        let mask = BORDER_MASKS[idx];
        if mask != 0 {
            let mut c = self.chunks[slot].coords;
            for d in dirs::DIRS {
                if mask & (1 << d) == 0 {
                    continue;
                }
                let axis = dirs::axis(d) as usize;
                let saved = c[axis];
                c[axis] += dirs::sign(d);
                self.wake_chunk_border(c);
                c[axis] = saved;
            }
            let _ = c;
        }
    }

    /// Queues **every** voxel of a chunk on the current work list.
    ///
    /// This is the entry point used on chunk load, on a config reload and by the
    /// scenario/debug commands. [`Chunk::wake_all`] only raises the voxel flags;
    /// without this method those voxels would never reach a work list and the
    /// chunk would look awake while doing nothing.
    pub fn wake_all(&mut self, slot: usize) {
        if slot >= self.chunks.len() {
            return;
        }
        // Drop any stale work-list entry for this chunk so nothing is queued twice.
        self.purge_slot(slot);
        // Reset the queue flags so the duplicate check in `wake` cannot skip any
        // voxel, then re-queue everything through the normal path.
        for v in self.chunks[slot].voxels.iter_mut() {
            v.flags &= !flags::ACTIVE;
        }
        self.chunks[slot].active_count = 0;
        self.chunks[slot].wake();
        for idx in 0..CHUNK_VOLUME {
            self.wake(slot, idx);
        }
    }

    /// Wakes every border voxel of a chunk (if resident).
    fn wake_chunk_border(&mut self, coords: [i32; 3]) {
        let slot = match self.find(coords) {
            Some(s) => s,
            None => return,
        };
        if self.chunks[slot].flags & cflags::ASLEEP != 0 {
            self.chunks[slot].wake();
        }
        let mut i = 0;
        while i < CHUNK_VOLUME {
            if BORDER_MASKS[i] != 0
                && self.chunks[slot].voxels[i].is_fluid()
                && self.chunks[slot].voxels[i].flags & flags::ACTIVE == 0
            {
                if self.front.len() < MAX_ACTIVE {
                    self.chunks[slot].voxels[i].flags |= flags::ACTIVE;
                    self.front.push(Grid::pack(slot, i));
                    self.chunks[slot].active_count =
                        self.chunks[slot].active_count.saturating_add(1);
                }
            }
            i += 1;
        }
        self.chunks[slot].stable_ticks = 0;
    }

    /// Removes a voxel from the active bookkeeping (it settled or vanished).
    #[inline]
    pub fn settle(&mut self, slot: usize, idx: usize) {
        if slot >= self.chunks.len() || idx >= CHUNK_VOLUME {
            return;
        }
        let chunk = &mut self.chunks[slot];
        if chunk.voxels[idx].flags & flags::ACTIVE != 0 {
            chunk.voxels[idx].flags &= !flags::ACTIVE;
            chunk.voxels[idx].flags |= flags::SLEEPING;
            chunk.active_count = chunk.active_count.saturating_sub(1);
        }
    }

    /// Enforces the per-tick evaluation budget and rotates the work lists.
    ///
    /// `processed` is the number of leading `front` entries the kernels
    /// evaluated; the remainder is carried into `back` so that no work is lost —
    /// it is only deferred by one tick ("temporal LOD"). The two buffers are then
    /// swapped, which is an `O(1)` pointer exchange because they are `Vec`s.
    pub fn rotate(&mut self, processed: usize) {
        let p = processed.min(self.front.len());
        for r in p..self.front.len() {
            if self.back.len() < MAX_ACTIVE {
                self.back.push(self.front[r]);
            }
        }
        self.stats.processed = p as u32;
        let carried = self.back.len() as u32;
        core::mem::swap(&mut self.front, &mut self.back);
        self.back.clear();
        self.stats.requeued = carried;
    }

    /// Total fluid mass across all resident chunks.
    #[must_use]
    pub fn total_mass(&self) -> u32 {
        self.chunks
            .iter()
            .filter(|c| c.coords[0] != i32::MIN)
            .map(|c| c.total_mass())
            .sum()
    }

    /// Refreshes the diagnostic snapshot.
    pub fn refresh_stats(&mut self) {
        let mut asleep = 0;
        let mut mass = 0;
        let mut resident = 0;
        for c in self.chunks.iter() {
            if c.coords[0] == i32::MIN {
                continue;
            }
            resident += 1;
            if c.is_asleep() {
                asleep += 1;
            }
            mass += c.total_mass();
        }
        self.stats.chunks = resident as u32;
        self.stats.asleep = asleep as u32;
        self.stats.total_mass = mass;
        self.stats.active = self.front.len() as u32;
    }

    /// Advances the sleep bookkeeping for every resident chunk.
    ///
    /// Called at the end of each tick, after the kernels drained the active
    /// list. A chunk with no active voxels for [`SLEEP_AFTER_TICKS`] becomes
    /// `ASLEEP`; its `flags` are cleared so later ticks skip it in O(1).
    pub fn update_sleep(&mut self) {
        for c in self.chunks.iter_mut() {
            if c.coords[0] == i32::MIN {
                continue;
            }
            if c.active_count == 0 {
                if c.stable_ticks < u16::MAX {
                    c.stable_ticks += 1;
                }
                if c.stable_ticks >= SLEEP_AFTER_TICKS {
                    c.sleep_now();
                }
            } else {
                c.stable_ticks = 0;
                c.flags &= !cflags::ASLEEP;
            }
        }
    }

    /// Clears the per-tick render markers across the resident set.
    pub fn begin_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        self.stats.processed = 0;
        self.stats.requeued = 0;
        for c in self.chunks.iter_mut() {
            if c.coords[0] == i32::MIN {
                continue;
            }
            c.clear_tick_marks();
        }
    }

    /// Serialises a chunk to `out` as 8 bytes per voxel. Returns the byte count.
    pub fn save_chunk(&self, slot: usize, out: &mut [u8]) -> usize {
        if slot >= self.chunks.len() {
            return 0;
        }
        let needed = CHUNK_VOLUME * 8;
        if out.len() < needed {
            return 0;
        }
        let c = &self.chunks[slot];
        for (i, v) in c.voxels.iter().enumerate() {
            let word = v.to_u64().to_le_bytes();
            out[i * 8..i * 8 + 8].copy_from_slice(&word);
        }
        needed
    }

    /// Loads a chunk previously written by [`Grid::save_chunk`].
    pub fn load_chunk(&mut self, coords: [i32; 3], data: &[u8]) -> bool {
        let needed = CHUNK_VOLUME * 8;
        if data.len() < needed {
            return false;
        }
        let slot = self.ensure_chunk(coords);
        {
            let c = &mut self.chunks[slot];
            for i in 0..CHUNK_VOLUME {
                let mut word = [0u8; 8];
                word.copy_from_slice(&data[i * 8..i * 8 + 8]);
                c.voxels[i] = Voxel::from_u64(u64::from_le_bytes(word));
            }
        }
        self.wake_all(slot);
        true
    }

    /// Total bytes of voxel storage currently held.
    #[must_use]
    pub fn bytes_in_use(&self) -> usize {
        self.chunks.len() * size_of::<Chunk>()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirs::SLICE_STRIDE;

    fn tiny_grid() -> Grid {
        Grid::with_capacity(1, 4)
    }

    #[test]
    fn chunk_stays_within_its_memory_budget() {
        // 32 KiB of voxels plus a small, cache-line sized header.
        let overhead = size_of::<Chunk>() - 32 * 1024;
        assert!(overhead <= 128, "chunk header grew to {overhead} bytes");
    }

    #[test]
    fn world_to_local_handles_negatives() {
        assert_eq!(Grid::world_to_local(0, 0, 0), ([0, 0, 0], 0));
        assert_eq!(Grid::world_to_local(15, 0, 0), ([0, 0, 0], 15));
        assert_eq!(Grid::world_to_local(16, 0, 0), ([1, 0, 0], 0));
        let (c, i) = Grid::world_to_local(-1, -1, -1);
        assert_eq!(c, [-1, -1, -1]);
        assert_eq!(dirs::unindex(i), (15, 15, 15));
        let (c, i) = Grid::world_to_local(-16, 0, 7);
        assert_eq!(c, [-1, 0, 0]);
        assert_eq!(dirs::unindex(i), (0, 0, 7));
    }

    #[test]
    fn world_roundtrip() {
        for x in -40..40 {
            for y in -8..8 {
                let (c, i) = Grid::world_to_local(x, y, 3);
                let (lx, ly, lz) = dirs::unindex(i);
                let back = (
                    c[0] * CHUNK_SIZE as i32 + lx as i32,
                    c[1] * CHUNK_SIZE as i32 + ly as i32,
                    c[2] * CHUNK_SIZE as i32 + lz as i32,
                );
                assert_eq!(back, (x, y, 3));
            }
        }
    }

    #[test]
    fn ensure_chunk_is_idempotent() {
        let mut g = tiny_grid();
        let a = g.ensure_chunk([0, 0, 0]);
        let b = g.ensure_chunk([0, 0, 0]);
        assert_eq!(a, b);
        assert_eq!(g.resident_chunks(), 1);
    }

    #[test]
    fn neighbour_crosses_chunk_borders() {
        let mut g = tiny_grid();
        g.set_world(15, 5, 5, Voxel::water(20));
        // +X neighbour of (15,5,5) lives in chunk (1,0,0) which is not resident.
        let slot = g.find([0, 0, 0]).unwrap();
        let idx = dirs::index(15, 5, 5);
        assert_eq!(g.neighbor(slot, idx, DIR_POS_X), None);
        // Create it and the neighbour appears.
        g.ensure_chunk([1, 0, 0]);
        let n = g.neighbor(slot, idx, DIR_POS_X).unwrap();
        assert_eq!(n.chunk, g.find([1, 0, 0]).unwrap());
        assert_eq!(dirs::unindex(n.index), (0, 5, 5));
    }

    #[test]
    fn neighbour_interior_is_flat_step() {
        let mut g = tiny_grid();
        g.ensure_chunk([0, 0, 0]);
        let slot = g.find([0, 0, 0]).unwrap();
        let idx = dirs::index(4, 4, 4);
        let up = g.neighbor(slot, idx, DIR_POS_Y).unwrap();
        assert_eq!(up.chunk, slot);
        assert_eq!(up.index, idx + SLICE_STRIDE);
    }

    #[test]
    fn wake_deduplicates() {
        let mut g = tiny_grid();
        g.set_world(0, 0, 0, Voxel::water(20));
        let before = g.front.len();
        let slot = g.find([0, 0, 0]).unwrap();
        let idx = dirs::index(0, 0, 0);
        g.wake(slot, idx);
        g.wake(slot, idx);
        assert_eq!(g.front.len(), before, "duplicate wake must be ignored");
    }

    #[test]
    fn wake_flags_neighbours() {
        let mut g = tiny_grid();
        g.set_world(1, 1, 1, Voxel::water(20));
        // Simulate a completed tick: the work list is drained, which clears the
        // per-voxel queue flag as well (that is what `settle` does in the tick
        // loop). Clearing only the list would leave a stale `ACTIVE` bit behind
        // and `wake` would — correctly — refuse to push the cell twice.
        g.front.clear();
        let slot = g.find([0, 0, 0]).unwrap();
        for v in g.chunks[slot].voxels.iter_mut() {
            v.flags &= !flags::ACTIVE;
        }
        g.chunks[slot].active_count = 0;

        let idx = dirs::index(1, 1, 1);
        g.wake_with_neighbors(slot, idx);
        // centre + 6 neighbours
        assert_eq!(g.front.len(), 7, "expected 7 entries, got {}", g.front.len());
        assert_eq!(g.chunks[slot].active_count, 7, "accounting must match the list");
        let c = &g.chunks[slot];
        for d in dirs::DIRS {
            let (x, y, z) = dirs::unindex(idx);
            let s = dirs::step(d);
            let n = if s < 0 {
                dirs::index(x, y, z).wrapping_sub((-s) as usize)
            } else {
                dirs::index(x, y, z) + s as usize
            };
            assert!(c.voxels[n].flags & flags::ACTIVE != 0, "dir {d} not woken");
        }
    }

    #[test]
    fn chunk_sleeps_after_quiet_ticks() {
        let mut g = tiny_grid();
        let slot = g.ensure_chunk([0, 0, 0]);
        g.chunks[slot].wake_all();
        assert!(!g.chunks[slot].is_asleep());
        // Simulate a full drain of the active list.
        g.chunks[slot].active_count = 0;
        for _ in 0..SLEEP_AFTER_TICKS {
            g.update_sleep();
        }
        assert!(g.chunks[slot].is_asleep(), "chunk must fall asleep");
        assert_eq!(g.chunks[slot].flags & cflags::ASLEEP, cflags::ASLEEP);
        // A wake must resurrect it immediately.
        g.wake(slot, dirs::index(3, 3, 3));
        assert!(!g.chunks[slot].is_asleep());
        assert_eq!(g.chunks[slot].stable_ticks, 0);
    }

    #[test]
    fn set_world_marks_chunk_dirty() {
        let mut g = tiny_grid();
        g.set_world(2, 2, 2, Voxel::stone(20));
        let c = g.chunk([0, 0, 0]).unwrap();
        assert_eq!(c.flags & cflags::DIRTY, cflags::DIRTY);
        assert!(!c.is_asleep());
    }

    #[test]
    fn mass_accounting() {
        let mut g = tiny_grid();
        for x in 0..3 {
            let mut v = Voxel::water(20);
            v.mass = 4;
            g.set_world(x, 0, 0, v);
        }
        assert_eq!(g.total_mass(), 12);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let mut g = tiny_grid();
        g.set_world(3, 4, 5, Voxel::new(Species::Lava, 1_100));
        let slot = g.find([0, 0, 0]).unwrap();
        let mut buf = vec![0u8; CHUNK_VOLUME * 8];
        assert_eq!(g.save_chunk(slot, &mut buf), CHUNK_VOLUME * 8);

        let mut g2 = tiny_grid();
        assert!(g2.load_chunk([0, 0, 0], &buf));
        let v = g2.get_world(3, 4, 5).unwrap();
        assert_eq!(v.species(), Species::Lava);
        assert_eq!(v.temp, 1_100);
    }

    #[test]
    fn eviction_prefers_the_sleepiest_chunk() {
        let mut g = Grid::with_capacity(7, 2);
        g.ensure_chunk([0, 0, 0]);
        g.ensure_chunk([1, 0, 0]);
        g.chunks[0].stable_ticks = 100;
        g.chunks[1].stable_ticks = 3;
        g.ensure_chunk([2, 0, 0]);
        assert_eq!(g.evictions, 1);
        assert!(g.find([0, 0, 0]).is_none(), "sleepiest chunk should be evicted");
        assert!(g.find([1, 0, 0]).is_some());
        assert!(g.find([2, 0, 0]).is_some());
    }

    #[test]
    fn fill_box_stays_in_bounds() {
        let mut g = tiny_grid();
        let c = g.chunk_mut([0, 0, 0]);
        c.fill_box([0, 0, 0], [99, 99, 99], Species::Stone, 20, 8);
        assert_eq!(c.voxels[dirs::index(15, 15, 15)].species(), Species::Stone);
    }
}

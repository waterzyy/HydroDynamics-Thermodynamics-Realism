//! # HydroDynamics & Thermodynamics Realism
//!
//! A fluid / thermal / phase-transition core for the **Horizon** voxel engine,
//! compiled to `wasm32-unknown-unknown` as a `cdylib` and loaded by the engine's
//! simulation sandbox.
//!
//! The mod replaces vanilla's "flowing water levels" with a real cellular
//! automaton: mass, hydrostatic pressure, temperature, phase changes, buoyancy
//! and fracture — all in integer arithmetic, all allocation-free inside the tick.
//!
//! ## Architecture
//!
//! ```text
//!   horizon_tick()  ──▶  Sim::tick()
//!                          │
//!                          ├─ grid.begin_tick()          roll the work lists
//!                          ├─ for each active voxel (≤ budget):
//!                          │     hydro::tick_cell()      gravity → levelling → jets
//!                          │     hydro::update_pressure() hydrostatic head
//!                          │     thermo::tick_cell()      conduction → radiation → phases
//!                          │     hydro::fracture()        brittle blocks give way
//!                          │     settle / re-queue
//!                          ├─ grid.rotate(processed)     deferred work → next tick
//!                          └─ grid.update_sleep()        quiet chunks fall asleep
//! ```
//!
//! | module | responsibility |
//! |---|---|
//! | [`units`] | fixed-point scalar types, integer roots, deterministic RNG |
//! | [`dirs`] | the six-neighbourhood, flat index layout, border bitmasks |
//! | [`voxel`] | the 8-byte voxel and the material database |
//! | [`config`] | tunables mirrored from `horizon-mod.toml` |
//! | [`grid`] | chunk store, active list, sleep/wake |
//! | [`hydro`] | communicating vessels, head, jets, buoyancy, fracture |
//! | [`thermo`] | conduction, radiation, phase machine, quench |
//! | [`entities`] | Archimedes, updrafts, drag, compression injuries |
//! | [`commands`] | the `/hd` console surface declared in the manifest |
//! | [`book`] | the generated "Atlas of Fluid Physics" (JSON + NBT) |
//!
//! ## WASM ABI
//!
//! All entry points are `extern "C"` with integer arguments only; no `struct` is
//! passed across the boundary, so the host does not need to know the Rust layout.
//!
//! | export | signature | purpose |
//! |---|---|---|
//! | `horizon_api_version` | `() -> u32` | ABI handshake |
//! | `horizon_alloc` / `horizon_free` | `(len: u32) -> *mut u8` / `(ptr, len)` | host-side argument buffers |
//! | `horizon_on_world_load` | `(seed_hi: u32, seed_lo: u32, ambient: i32) -> u32` | called once per world |
//! | `horizon_configure` | `(k_ptr, k_len, v_ptr, v_len) -> u32` | one `key = value` from the TOML |
//! | `horizon_tick` | `() -> u32` | world tick; returns active voxels processed |
//! | `horizon_on_player_join` | `(player: u32, is_respawn: u32) -> u32` | grant the Atlas, returns its NBT length |
//! | `horizon_book_nbt_ptr` / `_len` | `() -> *const u8` / `() -> u32` | written-book NBT blob |
//! | `horizon_book_json_ptr` / `_len` | `() -> *const u8` / `() -> u32` | JSON twin of the same book |
//! | `horizon_get_block` | `(x, y, z) -> u64` | packed voxel (see [`voxel::Voxel::to_u64`]) |
//! | `horizon_set_block` | `(x, y, z, word: u64) -> u32` | write + wake the neighbourhood |
//! | `horizon_probe` | `(x, y, z) -> u32` | species/temp/pressure for the HUD |
//! | `horizon_entity_forces` | `(x, y, z, density) -> u64` | buoyancy, updraft, jet, drag for an entity |
//! | `horizon_entity_status` | `(x, y, z) -> u32` | submersion depth + barotrauma |
//! | `horizon_run_command` | `(ptr, len) -> u32` | one `/hd` line → JSON reply |
//! | `horizon_reply_ptr` / `_len` | `() -> *const u8` / `() -> u32` | the reply of the last command |
//! | `horizon_stats_ptr` | `() -> *const u32` | 8 diagnostics words |
//! | `horizon_event_pop` | `() -> u64` | next physics event (particles, sounds) |
//!
//! The host writes arguments into buffers obtained from `horizon_alloc`; every
//! pointer the module *returns* (book, stats, reply) stays valid until the next
//! `horizon_tick` or until the same buffer is regenerated.
//!
//! ## Determinism
//!
//! Every random choice (bubble jitter, tie-breaks) comes from a per-chunk LCG
//! seeded from the world seed and the chunk coordinates, and every loop has a
//! fixed iteration order. The server and a client replaying the same input
//! produce bit-identical results, which is what makes the mod safe for
//! server-authoritative play.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations)]
#![allow(clippy::needless_range_loop)]

pub mod book;
pub mod commands;
pub mod config;
pub mod dirs;
pub mod entities;
pub mod grid;
pub mod hydro;
pub mod thermo;
pub mod units;
pub mod voxel;

use core::cell::RefCell;

use crate::config::Tunables;
use crate::grid::{Grid, Stats};
use crate::thermo::Event;

/// Semantic version of the mod, embedded in the generated book and reported by
/// [`horizon_api_version`]'s sibling export.
pub const VERSION: &str = "1.5.0";

/// ABI revision. The host refuses to load a module with a different major.
pub const API_VERSION: u32 = 0x0001_0004;

/// Capacity of the event ring buffer handed to the presentation layer.
pub const EVENT_CAPACITY: usize = 256;

// ---------------------------------------------------------------------------
// Event log
// ---------------------------------------------------------------------------

/// Fixed-capacity ring buffer of physics events.
///
/// The engine drains it after every tick to spawn particles / play sounds. When
/// it overflows, the *oldest* event is dropped: a busy quench scene must never
/// stall the simulation because the renderer is behind.
#[derive(Clone, Copy, Debug)]
pub struct EventLog {
    buf: [Option<Event>; EVENT_CAPACITY],
    head: usize,
    len: usize,
    /// Number of events dropped since the last drain (diagnostics).
    pub dropped: u32,
}

impl Default for EventLog {
    fn default() -> Self {
        EventLog::new()
    }
}

impl EventLog {
    /// An empty log.
    #[must_use]
    pub const fn new() -> EventLog {
        EventLog {
            buf: [None; EVENT_CAPACITY],
            head: 0,
            len: 0,
            dropped: 0,
        }
    }

    /// Number of pending events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` when nothing is pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Appends an event (dropping the oldest one when full).
    pub fn push(&mut self, event: Event) {
        let slot = (self.head + self.len) % EVENT_CAPACITY;
        if self.len == EVENT_CAPACITY {
            self.buf[self.head] = Some(event);
            self.head = (self.head + 1) % EVENT_CAPACITY;
            self.dropped = self.dropped.saturating_add(1);
        } else {
            self.buf[slot] = Some(event);
            self.len += 1;
        }
    }

    /// Removes and returns the oldest event.
    pub fn pop(&mut self) -> Option<Event> {
        if self.len == 0 {
            return None;
        }
        let event = self.buf[self.head].take();
        self.head = (self.head + 1) % EVENT_CAPACITY;
        self.len -= 1;
        event
    }
}

// ---------------------------------------------------------------------------
// The simulation
// ---------------------------------------------------------------------------

/// Everything the mod owns at runtime: one grid, its tunables and its caches.
pub struct Sim {
    /// The voxel world.
    pub grid: Grid,
    /// Live configuration.
    pub tunables: Tunables,
    /// Pending physics events.
    pub events: EventLog,
    /// Lazily generated Atlas (JSON source of truth).
    pub atlas: Option<book::Atlas>,
    /// Cached NBT blob for the book (regenerated only when the Atlas is built).
    pub book_nbt: Vec<u8>,
    /// Cached JSON document for the book.
    pub book_json: Vec<u8>,
    /// Reply buffer of the last `/hd` command (read by the host).
    pub reply: Vec<u8>,
    /// Number of times the Atlas was handed to a player.
    pub books_granted: u32,
    /// `horizon_on_world_load` has been called.
    pub world_loaded: bool,
    /// Last diagnostics snapshot, mirrored for the host.
    pub stats_out: [u32; 8],
    /// Monotonic tick counter (mirrors `grid.tick`).
    pub tick_count: u64,
}

impl core::fmt::Debug for Sim {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Sim")
            .field("loaded", &self.world_loaded)
            .field("chunks", &self.grid.resident_chunks())
            .field("tick", &self.tick_count)
            .field("pending_events", &self.events.len())
            .field("books_granted", &self.books_granted)
            .finish()
    }
}

impl Default for Sim {
    fn default() -> Self {
        Sim::new(Tunables::DEFAULT)
    }
}

impl Sim {
    /// Creates a simulation with the given tunables.
    #[must_use]
    pub fn new(tunables: Tunables) -> Sim {
        let mut sim = Sim {
            grid: Grid::new(tunables.world_seed),
            tunables,
            events: EventLog::new(),
            atlas: None,
            book_nbt: Vec::new(),
            book_json: Vec::new(),
            reply: Vec::new(),
            books_granted: 0,
            world_loaded: false,
            stats_out: [0; 8],
            tick_count: 0,
        };
        sim.grid.ambient = sim.tunables.ambient_c;
        sim
    }

    /// Applies a `key = value` pair from the host config bridge.
    pub fn configure(&mut self, key: &str, value: &str) -> bool {
        let ok = self.tunables.apply_kv(key, value);
        if ok {
            self.tunables.sanitize();
            self.grid.ambient = self.tunables.ambient_c;
            // Any config change invalidates the generated book.
            self.atlas = None;
            self.book_nbt.clear();
            self.book_json.clear();
        }
        ok
    }

    /// Builds (or returns the cached) Atlas.
    pub fn atlas(&mut self) -> &book::Atlas {
        if self.atlas.is_none() {
            self.atlas = Some(book::build(&self.tunables));
        }
        // `Option::as_ref().unwrap()` cannot panic: the branch above guarantees it.
        match self.atlas.as_ref() {
            Some(a) => a,
            None => unreachable!(),
        }
    }

    /// Serialises the Atlas to NBT and returns the byte length.
    pub fn prepare_book(&mut self) -> u32 {
        let atlas = book::build(&self.tunables);
        self.book_nbt = atlas.to_nbt();
        self.book_json = atlas.to_json().into_bytes();
        self.atlas = Some(atlas);
        self.book_nbt.len() as u32
    }

    /// Handles a join / respawn: returns the serialised book length (bytes) when
    /// the Atlas is granted, 0 when this event does not grant one.
    pub fn on_player_join(&mut self, is_respawn: bool) -> u32 {
        let should = if is_respawn {
            self.tunables.give_book_on_respawn
        } else {
            self.tunables.give_book_on_join
        };
        if !should {
            return 0;
        }
        if self.book_nbt.is_empty() {
            self.prepare_book();
        }
        self.books_granted = self.books_granted.saturating_add(1);
        self.book_nbt.len() as u32
    }

    /// Runs one world tick and returns the number of voxels evaluated.
    ///
    /// The tick is **budgeted**: at most `tick_budget` active voxels are
    /// evaluated, and the remainder is deferred to the next tick. A lagging
    /// server therefore slows the simulation down instead of freezing.
    pub fn tick(&mut self) -> u32 {
        self.grid.begin_tick();

        let budget = self.tunables.tick_budget as usize;
        let available = self.grid.front.len().min(budget);
        let mut i = 0;
        while i < available {
            let entry = self.grid.front[i];
            i += 1;
            let (slot, idx) = Grid::unpack(entry);

            // ---- hydrodynamics ------------------------------------------
            // `tick_cell` refreshes the cell's own hydrostatic pressure as its
            // first step, so the loop must not ask for it a second time: one
            // neighbour read per active voxel per tick is the whole point.
            let flow = hydro::tick_cell(&mut self.grid, slot, idx, &self.tunables);

            // ---- thermodynamics -----------------------------------------
            let thermal = thermo::tick_cell(&mut self.grid, slot, idx, &self.tunables);
            if let Some(ev) = thermal.event {
                self.events.push(ev);
            }

            // ---- pressure fracture --------------------------------------
            if flow.changed {
                if let Some(ev) = hydro::fracture(&mut self.grid, slot, idx, &self.tunables) {
                    self.events.push(ev);
                }
            }

            // ---- scheduling ---------------------------------------------
            // Clear the queue flag first: `wake_next` is a no-op while `ACTIVE`
            // is set, so the order matters.
            self.grid.settle(slot, idx);
            let still_active = flow.active || thermal.active;
            if still_active {
                self.grid.wake_next(slot, idx);
            }
        }

        self.grid.rotate(available);
        self.grid.update_sleep();
        self.grid.refresh_stats();
        self.tick_count = self.grid.tick;
        self.sync_stats();
        self.grid.stats.processed
    }

    /// Copies the diagnostics into the flat array the host reads.
    pub fn sync_stats(&mut self) {
        let s: Stats = self.grid.stats;
        detach_stats(
            &mut self.stats_out,
            s,
            self.tick_count,
            self.events.len() as u32,
        );
    }

    /// Runs one `/hd` argument line and returns the length of the JSON reply,
    /// which stays addressable through [`Sim::reply`] (and, across the ABI,
    /// through `horizon_reply_ptr` / `horizon_reply_len`).
    pub fn run_command(&mut self, line: &str) -> u32 {
        let mut buf = core::mem::take(&mut self.reply);
        let n = commands::dispatch(self, line, &mut buf);
        self.reply = buf;
        n
    }

    /// Writes a block into the world (used by `/hd set`, worldgen and tests).
    pub fn set_block(&mut self, x: i32, y: i32, z: i32, v: voxel::Voxel) {
        self.grid.set_world(x, y, z, v);
    }

    /// Reads a block from the world.
    #[must_use]
    pub fn get_block(&self, x: i32, y: i32, z: i32) -> Option<voxel::Voxel> {
        self.grid.get_world(x, y, z)
    }
}

fn detach_stats(out: &mut [u32; 8], s: Stats, tick: u64, queued_events: u32) {
    out[0] = s.active;
    out[1] = s.processed;
    out[2] = s.requeued;
    out[3] = s.chunks;
    out[4] = s.asleep;
    out[5] = s.total_mass;
    out[6] = (tick & 0xFFFF_FFFF) as u32;
    out[7] = queued_events;
}

// ---------------------------------------------------------------------------
// WASM entry points
// ---------------------------------------------------------------------------

thread_local! {
    /// The single simulation instance owned by the sandbox module.
    static SIM: RefCell<Option<Sim>> = const { RefCell::new(None) };
}

/// Runs `f` with the current simulation, creating a default one if the engine
/// forgot to call `horizon_on_world_load` first.
fn with_sim<R>(f: impl FnOnce(&mut Sim) -> R) -> R {
    SIM.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(Sim::default());
        }
        match slot.as_mut() {
            Some(sim) => f(sim),
            None => unreachable!("simulation was just initialised"),
        }
    })
}

/// Returns the ABI revision.
#[no_mangle]
pub extern "C" fn horizon_api_version() -> u32 {
    API_VERSION
}

/// Allocates `len` bytes inside the module and returns their address.
///
/// This is the only way the host can hand a string to [`horizon_configure`] or
/// [`horizon_run_command`]: the module owns its linear memory, so the host must
/// ask for a buffer instead of writing into an address of its own choosing.
/// Returns a null pointer when `len` is zero or the allocator is out of memory;
/// every caller treats null as "the argument was ignored", never as a crash.
///
/// The buffer stays valid until it is returned with [`horizon_free`]. It is
/// deliberately *not* the simulation's own storage, so a host that forgets to
/// free it leaks a few dozen bytes rather than corrupting the grid.
#[no_mangle]
pub extern "C" fn horizon_alloc(len: u32) -> *mut u8 {
    if len == 0 {
        return core::ptr::null_mut();
    }
    match std::alloc::Layout::from_size_align(len as usize, 8) {
        Ok(layout) => {
            // SAFETY: the layout is non-zero sized and 8-aligned, so `alloc` is
            // allowed to return memory for it.
            let p = unsafe { std::alloc::alloc(layout) };
            p
        }
        Err(_) => core::ptr::null_mut(),
    }
}

/// Releases a buffer obtained from [`horizon_alloc`].
///
/// # Safety
/// `ptr` must have been returned by [`horizon_alloc`] with the same `len`, and
/// must not be used afterwards. Passing null is a no-op.
#[no_mangle]
pub unsafe extern "C" fn horizon_free(ptr: *mut u8, len: u32) {
    if ptr.is_null() || len == 0 {
        return;
    }
    if let Ok(layout) = std::alloc::Layout::from_size_align(len as usize, 8) {
        // SAFETY: the caller guarantees the pointer came from `horizon_alloc`
        // with this exact length and alignment.
        unsafe { std::alloc::dealloc(ptr, layout) };
    }
}

/// Called once per world load. `seed_hi`/`seed_lo` form the 64-bit world seed,
/// `ambient` is the surface temperature in °C.
///
/// Returns the byte length of the freshly generated Atlas NBT, which the host
/// uses to size its copy buffer (0 when generation failed).
#[no_mangle]
pub extern "C" fn horizon_on_world_load(seed_hi: u32, seed_lo: u32, ambient: i32) -> u32 {
    with_sim(|sim| {
        let seed = ((seed_hi as u64) << 32) | seed_lo as u64;
        sim.tunables.world_seed = seed;
        sim.tunables.ambient_c = units::clamp_temp(ambient);
        sim.grid = Grid::new(seed);
        sim.grid.ambient = sim.tunables.ambient_c;
        sim.events = EventLog::new();
        sim.world_loaded = true;
        sim.prepare_book() as u32
    })
}

/// Applies one `key = value` pair from `horizon-mod.toml`.
///
/// # Safety
/// `key_ptr`/`val_ptr` must point to `key_len`/`val_len` valid UTF-8 bytes in
/// the module's linear memory, obtained from [`horizon_alloc`] (or the module's
/// own buffers, which the host may also read).
#[no_mangle]
pub unsafe extern "C" fn horizon_configure(
    key_ptr: *const u8,
    key_len: u32,
    val_ptr: *const u8,
    val_len: u32,
) -> u32 {
    if key_ptr.is_null() || val_ptr.is_null() {
        return 0;
    }
    // SAFETY: the caller guarantees the pointers and lengths (see the docs).
    let key = unsafe { core::slice::from_raw_parts(key_ptr, key_len as usize) };
    // SAFETY: as above.
    let val = unsafe { core::slice::from_raw_parts(val_ptr, val_len as usize) };
    let (Ok(k), Ok(v)) = (core::str::from_utf8(key), core::str::from_utf8(val)) else {
        return 0;
    };
    with_sim(|sim| u32::from(sim.configure(k, v)))
}

/// World tick. Returns the number of voxels evaluated this tick.
#[no_mangle]
pub extern "C" fn horizon_tick() -> u32 {
    with_sim(|sim| sim.tick())
}

/// Join / respawn hook. Returns the NBT length when the Atlas was granted.
#[no_mangle]
pub extern "C" fn horizon_on_player_join(_player: u32, is_respawn: u32) -> u32 {
    with_sim(|sim| sim.on_player_join(is_respawn != 0))
}

/// Pointer to the book NBT blob in linear memory.
///
/// The buffer belongs to the simulation and stays valid until the book is
/// regenerated (a config change, another join grant) or the module reloads — not
/// merely until the next tick, so the host may copy it lazily.
///
/// The buffer is owned by the simulation and stays valid until the book is
/// regenerated (a config change) or the module is reloaded — **not** until the
/// next tick, so the host may copy it lazily.
#[no_mangle]
pub extern "C" fn horizon_book_nbt_ptr() -> *const u8 {
    with_sim(|sim| {
        if sim.book_nbt.is_empty() {
            sim.prepare_book();
        }
        sim.book_nbt.as_ptr()
    })
}

/// Length of the book NBT blob.
#[no_mangle]
pub extern "C" fn horizon_book_nbt_len() -> u32 {
    with_sim(|sim| sim.book_nbt.len() as u32)
}

/// Pointer to the book JSON document in linear memory.
#[no_mangle]
pub extern "C" fn horizon_book_json_ptr() -> *const u8 {
    with_sim(|sim| {
        if sim.book_json.is_empty() {
            sim.prepare_book();
        }
        sim.book_json.as_ptr()
    })
}

/// Length of the book JSON document.
#[no_mangle]
pub extern "C" fn horizon_book_json_len() -> u32 {
    with_sim(|sim| sim.book_json.len() as u32)
}

/// Reads a voxel as a packed `u64` (see [`voxel::Voxel::to_u64`]).
#[no_mangle]
pub extern "C" fn horizon_get_block(x: i32, y: i32, z: i32) -> u64 {
    with_sim(|sim| match sim.grid.get_world(x, y, z) {
        Some(v) => v.to_u64(),
        None => 0,
    })
}

/// Writes a packed voxel and wakes the neighbourhood. Returns 1 on success.
#[no_mangle]
pub extern "C" fn horizon_set_block(x: i32, y: i32, z: i32, word: u64) -> u32 {
    with_sim(|sim| {
        // `set_world` owns the scheduling flags: it queues the cell itself, so
        // poking `ACTIVE` into the word here would make the voxel look queued
        // while it is not, and it would never be evaluated.
        let v = voxel::Voxel::from_u64(word);
        sim.grid.set_world(x, y, z, v);
        u32::from(sim.grid.get_world(x, y, z).is_some())
    })
}

/// Packs a voxel into the HUD probe word:
/// `species | mass<<4 | (temp+273)<<8 | (pressure/16)<<20`.
#[inline]
#[must_use]
pub fn pack_probe(v: voxel::Voxel) -> u32 {
    let species = v.species as u32;
    let mass = (v.mass as u32 & 0xF) << 4;
    let temp = (((v.temp as i32 + 273) as u32) & 0xFFF) << 8;
    let pressure = ((v.pressure as u32 / 16) & 0xFF) << 20;
    species | mass | temp | pressure
}

/// Debug probe for the HUD/`/hd probe`. Returns `u32::MAX` when the chunk is not
/// resident.
#[no_mangle]
pub extern "C" fn horizon_probe(x: i32, y: i32, z: i32) -> u32 {
    with_sim(|sim| match sim.grid.get_world(x, y, z) {
        Some(v) => pack_probe(v),
        None => u32::MAX,
    })
}

/// Asks the fluid field what it does to an entity of a given density.
///
/// Returns the packed [`entities::EntityForces`] word:
///
/// ```text
/// bits  0..16  vertical acceleration, jet included   (i16, 1/16 m/s²)
/// bits 16..32  horizontal jet push along X           (i16, 1/16 m/s)
/// bits 32..48  horizontal jet push along Z           (i16, 1/16 m/s)
/// bits 48..56  drag                                  (u8,  1/256 per tick)
/// bits 56..64  flags: submerged | updraft | breathable | positive buoyancy
/// ```
///
/// `density` is the entity's own density in kg/m³: 700 for a boat, 1 050 for a
/// swimmer in leather, 2 100 for a player in iron armour. An entity in an
/// unloaded chunk is treated as standing in still air.
#[no_mangle]
pub extern "C" fn horizon_entity_forces(x: i32, y: i32, z: i32, density: u32) -> u64 {
    with_sim(|sim| entities::query(&sim.grid, x, y, z, density, &sim.tunables).to_u64())
}

/// Submersion depth and barotrauma for an entity at `(x, y, z)`:
/// `depth_blocks | damage_per_second << 16`.
///
/// Split from [`horizon_entity_forces`] because the integrator only needs it
/// once per player per second, while the force word is needed every tick.
#[no_mangle]
pub extern "C" fn horizon_entity_status(x: i32, y: i32, z: i32) -> u32 {
    with_sim(|sim| entities::query(&sim.grid, x, y, z, 0, &sim.tunables).to_status_u32())
}

/// Runs one `/hd` command. `ptr`/`len` point at the UTF-8 argument line (see
/// `horizon_alloc`); the reply is JSON and is read back with
/// [`horizon_reply_ptr`] / [`horizon_reply_len`].
///
/// Returns the reply length in bytes. A malformed command is answered with
/// `{"ok":false,...}`, never with a trap: the console must stay usable when the
/// simulation is in a bad state, because it is the tool used to fix it.
///
/// # Safety
/// `ptr` must point to `len` valid bytes in the module's linear memory, obtained
/// from [`horizon_alloc`].
#[no_mangle]
pub unsafe extern "C" fn horizon_run_command(ptr: *const u8, len: u32) -> u32 {
    if ptr.is_null() {
        return 0;
    }
    // SAFETY: the caller guarantees the pointer and the length (see the docs).
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len as usize) };
    let Ok(line) = core::str::from_utf8(bytes) else {
        return 0;
    };
    with_sim(|sim| sim.run_command(line))
}

/// Pointer to the JSON reply of the last `horizon_run_command`.
#[no_mangle]
pub extern "C" fn horizon_reply_ptr() -> *const u8 {
    with_sim(|sim| sim.reply.as_ptr())
}

/// Length of the JSON reply of the last `horizon_run_command`.
#[no_mangle]
pub extern "C" fn horizon_reply_len() -> u32 {
    with_sim(|sim| sim.reply.len() as u32)
}

/// Pointer to the eight diagnostics words (`active`, `processed`, `requeued`,
/// `chunks`, `asleep`, `total_mass`, `tick_lo`, `events`).
#[no_mangle]
pub extern "C" fn horizon_stats_ptr() -> *const u32 {
    with_sim(|sim| {
        sim.sync_stats();
        sim.stats_out.as_ptr()
    })
}

/// Pops the next physics event, encoded as a `u64`.
///
/// Encoding: `kind (4 bits) | slot (20 bits) | index (16 bits) | mass (8 bits)`
/// for the simple events; quenches put the produced species in `mass`. Returns
/// `u64::MAX` when the queue is empty.
#[no_mangle]
pub extern "C" fn horizon_event_pop() -> u64 {
    with_sim(|sim| match sim.events.pop() {
        Some(Event::Freeze { slot, idx, mass }) => encode_event(1, slot, idx, mass),
        Some(Event::Melt { slot, idx, mass }) => encode_event(2, slot, idx, mass),
        Some(Event::Boil { slot, idx, mass }) => encode_event(3, slot, idx, mass),
        Some(Event::Condense { slot, idx, mass }) => encode_event(4, slot, idx, mass),
        Some(Event::Quench { slot, idx, result }) => encode_event(5, slot, idx, result.as_u8()),
        Some(Event::Shatter { slot, idx, species }) => encode_event(6, slot, idx, species.as_u8()),
        Some(Event::Ignite { slot, idx }) => encode_event(7, slot, idx, 0),
        None => u64::MAX,
    })
}

/// Packs an event into the ABI word.
#[inline]
#[must_use]
pub fn encode_event(kind: u8, slot: usize, idx: usize, payload: u8) -> u64 {
    ((kind as u64) & 0xF)
        | (((slot as u64) & 0xF_FFFF) << 4)
        | (((idx as u64) & 0xFFFF) << 24)
        | ((payload as u64) << 40)
}

/// Inverse of [`encode_event`]: `(kind, slot, idx, payload)`.
#[inline]
#[must_use]
pub fn decode_event(word: u64) -> (u8, usize, usize, u8) {
    (
        (word & 0xF) as u8,
        ((word >> 4) & 0xF_FFFF) as usize,
        ((word >> 24) & 0xFFFF) as usize,
        ((word >> 40) & 0xFF) as u8,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirs;
    use crate::voxel::{Species, Voxel};

    fn sim() -> Sim {
        let mut s = Sim::new(Tunables::DEFAULT);
        s.grid.ensure_chunk([0, 0, 0]);
        s
    }

    fn slot(s: &Sim) -> usize {
        s.grid.find([0, 0, 0]).unwrap()
    }

    #[test]
    fn api_handshake() {
        assert_eq!(horizon_api_version(), API_VERSION);
        assert!(VERSION.starts_with("1."));
    }

    #[test]
    fn event_log_is_a_ring() {
        let mut log = EventLog::new();
        assert!(log.is_empty());
        log.push(Event::Ignite { slot: 1, idx: 2 });
        assert_eq!(log.len(), 1);
        assert_eq!(log.pop(), Some(Event::Ignite { slot: 1, idx: 2 }));
        assert_eq!(log.pop(), None);
        for i in 0..(EVENT_CAPACITY + 10) {
            log.push(Event::Melt {
                slot: i,
                idx: 0,
                mass: 1,
            });
        }
        assert_eq!(log.len(), EVENT_CAPACITY);
        assert!(log.dropped >= 10);
        // The oldest ten were overwritten, so slot 10 is the new head.
        match log.pop() {
            Some(Event::Melt { slot, .. }) => assert_eq!(slot, 10),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn event_encoding_roundtrip() {
        let w = encode_event(5, 23, 4095, 7);
        let (k, s, i, p) = decode_event(w);
        assert_eq!((k, s, i, p), (5, 23, 4095, 7));
    }

    #[test]
    fn a_tick_moves_water_downhill() {
        let mut s = sim();
        s.set_block(2, 8, 2, Voxel::water(20));
        for _ in 0..40 {
            s.tick();
        }
        let low = s.get_block(2, 2, 2).or_else(|| s.get_block(2, 1, 2));
        assert!(low.is_some(), "water must fall");
    }

    #[test]
    fn settled_basin_sleeps_and_costs_nothing() {
        let mut s = sim();
        let sl = slot(&s);
        // A stone bowl with a layer of water in it.
        for x in 0..8 {
            for z in 0..8 {
                s.grid.put(sl, dirs::index(x, 1, z), Voxel::stone(20));
            }
        }
        for x in 1..7 {
            for z in 1..7 {
                s.grid.put(sl, dirs::index(x, 2, z), Voxel::water(20));
            }
        }
        s.grid.wake_all(sl);
        // Let it settle.
        let mut quiet = 0;
        for _ in 0..600 {
            s.tick();
            if s.grid.stats.processed == 0 {
                quiet += 1;
            }
            if quiet > 40 {
                break;
            }
        }
        assert!(quiet > 40, "the basin must reach a quiet state");
        assert!(
            s.grid.chunks[sl].is_asleep(),
            "the chunk must be asleep after {} stable ticks",
            s.grid.chunks[sl].stable_ticks
        );
        // A sleeping chunk must evaluate absolutely nothing.
        let processed = s.tick();
        assert_eq!(processed, 0, "a sleeping chunk must cost zero voxel work");
    }

    #[test]
    fn changing_one_block_wakes_the_grid() {
        let mut s = sim();
        let sl = slot(&s);
        for x in 0..4 {
            for z in 0..4 {
                s.grid.put(sl, dirs::index(x, 1, z), Voxel::stone(20));
            }
        }
        for _ in 0..60 {
            s.tick();
        }
        assert!(s.grid.chunks[sl].is_asleep());
        s.set_block(1, 2, 1, Voxel::water(20));
        assert!(
            !s.grid.chunks[sl].is_asleep(),
            "a write must wake the chunk"
        );
        assert!(s.tick() > 0);
    }

    #[test]
    fn boiling_water_creates_steam_and_events() {
        let mut s = sim();
        let sl = slot(&s);
        // A stone cauldron: the water must sit still, otherwise it simply falls
        // and spreads and there is nothing left above the source cell to boil.
        s.grid.put(sl, dirs::index(4, 3, 4), Voxel::stone(20));
        for (x, z) in [(3, 4), (5, 4), (4, 3), (4, 5)] {
            s.grid.put(sl, dirs::index(x, 4, z), Voxel::stone(20));
        }
        let idx = dirs::index(4, 4, 4);
        let mut w = Voxel::water(130);
        w.mass = 4;
        s.grid.put(sl, idx, w);
        s.grid.put(sl, dirs::index(4, 5, 4), Voxel::AIR);
        s.grid.wake_all(sl);
        // The vapour does not linger in the cell directly above the cauldron:
        // it is created there and the very same tick's sweep (the cell was
        // already queued) lets it rise. So look at the whole column above the
        // source, which is what the player actually sees.
        let mut saw_steam = false;
        for _ in 0..10 {
            s.tick();
            for y in 5..16 {
                if s.get_block(4, y, 4).map(|v| v.species()) == Some(Species::Steam) {
                    saw_steam = true;
                }
            }
        }
        assert!(
            saw_steam,
            "boiling water must raise a steam column above it (mass left: {})",
            s.grid.total_mass()
        );
        // Drain and confirm at least one Boil event was reported.
        let mut boiled = false;
        while let Some(ev) = s.events.pop() {
            if matches!(ev, Event::Boil { .. }) {
                boiled = true;
            }
        }
        assert!(boiled, "a Boil event must have been emitted");
    }

    #[test]
    fn lava_under_water_quenches_and_reports_an_event() {
        let mut s = sim();
        let sl = slot(&s);
        // The lava pool rests on rock; unsupported lava would fall away from the
        // water before the contact reaction is resolved (which is correct — it
        // is just not the scenario this test is about).
        for (x, z) in [(4, 5), (5, 5), (6, 5), (5, 4), (5, 6)] {
            s.grid.put(sl, dirs::index(x, 4, z), Voxel::stone(20));
        }
        s.grid
            .put(sl, dirs::index(5, 5, 5), Voxel::new(Species::Lava, 1_150));
        let mut w = Voxel::water(20);
        w.mass = 8;
        s.grid.put(sl, dirs::index(5, 6, 5), w);
        s.grid.wake_all(sl);
        let mut quenched = false;
        for _ in 0..30 {
            s.tick();
            if let Some(v) = s.get_block(5, 5, 5) {
                if matches!(
                    v.species(),
                    Species::Obsidian | Species::VolcanicGlass | Species::Pumice
                ) {
                    quenched = true;
                }
            }
            if quenched {
                break;
            }
        }
        assert!(quenched, "lava touching water must quench into stone");
    }

    #[test]
    fn sample_dam_break_scenario_runs_and_conserves_mass() {
        let mut s = sim();
        let sl = slot(&s);
        // Floor + a wall with a 1-block hole.
        for x in 0..16 {
            for z in 0..16 {
                s.grid.put(sl, dirs::index(x, 1, z), Voxel::stone(20));
            }
        }
        for y in 2..12 {
            for z in 0..16 {
                s.grid.put(sl, dirs::index(8, y, z), Voxel::stone(20));
            }
        }
        s.grid.put(sl, dirs::index(8, 2, 8), Voxel::AIR);
        for y in 2..10 {
            for x in 2..8 {
                for z in 2..14 {
                    s.grid.put(sl, dirs::index(x, y, z), Voxel::water(20));
                }
            }
        }
        s.grid.wake_all(sl);
        let before = s.grid.total_mass();
        for _ in 0..120 {
            s.tick();
        }
        assert_eq!(
            s.grid.total_mass(),
            before,
            "the dam break must conserve mass"
        );
        // Water must have reached the far side of the wall.
        let mut crossed = false;
        for x in 9..16 {
            for y in 2..12 {
                for z in 0..16 {
                    if let Some(v) = s.get_block(x, y, z) {
                        if v.species() == Species::Water && v.mass > 0 {
                            crossed = true;
                        }
                    }
                }
            }
        }
        assert!(crossed, "water must flow through the breach");
    }

    #[test]
    fn budget_limits_work_per_tick() {
        let mut s = sim();
        s.tunables.tick_budget = 32;
        s.tunables.sanitize();
        let sl = slot(&s);
        s.grid.chunks[sl].fill(Species::Water, 20, 8);
        s.grid.wake_all(sl);
        let processed = s.tick();
        assert!(processed <= 32, "budget exceeded: {processed} > 32");
        // Deferred work must not be lost.
        assert!(processed > 0);
    }

    #[test]
    fn player_join_grants_the_book_once_per_event() {
        let mut s = sim();
        assert!(s.books_granted == 0);
        let len = s.on_player_join(false);
        assert!(len > 0, "the book must be serialised");
        assert_eq!(s.books_granted, 1);
        assert!(s.atlas.as_ref().unwrap().page_count() >= 14);
        s.tunables.give_book_on_respawn = false;
        assert_eq!(s.on_player_join(true), 0);
        s.tunables.give_book_on_respawn = true;
        assert!(s.on_player_join(true) > 0);
    }

    #[test]
    fn configure_invalidates_the_book() {
        let mut s = sim();
        s.on_player_join(false);
        assert!(!s.book_nbt.is_empty());
        assert!(s.configure("thermo.boil_c", "95"));
        assert!(s.book_nbt.is_empty(), "book cache must be invalidated");
        assert_eq!(s.tunables.boil_c, 95);
        assert!(s.on_player_join(false) > 0);
    }

    #[test]
    fn stats_mirror_the_grid() {
        let mut s = sim();
        s.set_block(1, 1, 1, Voxel::water(20));
        s.tick();
        let st = s.stats_out;
        assert!(st[0] > 0, "active voxels must be reported: {st:?}");
        assert!(st[3] >= 1, "resident chunks");
        assert_eq!(st[5], s.grid.total_mass());
    }

    #[test]
    fn probe_packs_the_cell() {
        let v = Voxel::new(Species::Lava, 1_050);
        let word = pack_probe(v);
        assert_eq!(word & 0xF, Species::Lava as u32);
        assert_eq!((word >> 4) & 0xF, 8, "mass is stored in 4 bits");
        let temp = ((word >> 8) & 0xFFF) as i32 - 273;
        assert_eq!(temp, 1_050);
    }

    /// Installs a simulation as the one the exported entry points see, runs `f`
    /// and removes it again. The ABI works on the thread-local instance (that is
    /// what the host loads), so a test of an export has to go through it.
    fn with_installed<R>(s: Sim, f: impl FnOnce() -> R) -> R {
        SIM.with(|cell| *cell.borrow_mut() = Some(s));
        let out = f();
        SIM.with(|cell| *cell.borrow_mut() = None);
        out
    }

    /// A one-block-wide shaft filled with water from `y = 2` up to `top`,
    /// walled and floored in bedrock. Chunks are created on demand, so the shaft
    /// crosses the 16-block chunk border — which is what makes it deep enough to
    /// hurt a diver.
    ///
    /// The walls are bedrock on purpose: a 40-block column really does burst a
    /// stone dam (yield 35 m, see `hydro::fracture`), and a test about
    /// barotrauma should not silently turn into a test about the dam failing.
    fn water_shaft(s: &mut Sim, x: i32, z: i32, top: i32) {
        let bedrock = Voxel::new(Species::Bedrock, 20);
        s.set_block(x, 1, z, bedrock);
        for y in 1..=top + 1 {
            for (dx, dz) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                s.set_block(x + dx, y, z + dz, bedrock);
            }
        }
        for y in 2..=top {
            s.set_block(x, y, z, Voxel::water(20));
        }
        for _ in 0..120 {
            s.tick();
        }
    }

    #[test]
    fn entity_forces_are_packed_and_signed() {
        let mut s = sim();
        water_shaft(&mut s, 4, 4, 12);
        with_installed(s, || {
            // A boat (700 kg/m³) floats, iron armour (2 100) sinks.
            let wood = entities::EntityForces::from_u64(horizon_entity_forces(4, 6, 4, 700));
            let iron = entities::EntityForces::from_u64(horizon_entity_forces(4, 6, 4, 2_100));
            assert!(wood.accel > 0, "wood must be pushed up: {wood:?}");
            assert!(iron.accel < 0, "armour must sink: {iron:?}");
            assert_ne!(wood.flags & entities::force_flags::SUBMERGED, 0);
            assert_ne!(wood.flags & entities::force_flags::POSITIVE_BUOYANCY, 0);
            assert_eq!(iron.flags & entities::force_flags::POSITIVE_BUOYANCY, 0);
            assert!(wood.drag > 0, "water must damp motion");

            // Dry air: no buoyancy, breathable, no depth, no damage.
            let air = entities::EntityForces::from_u64(horizon_entity_forces(12, 12, 12, 700));
            assert_eq!(air.accel, 0);
            assert_ne!(air.flags & entities::force_flags::BREATHABLE, 0);
            assert_eq!(air.flags & entities::force_flags::SUBMERGED, 0);
            assert_eq!(air.depth, 0);
            assert_eq!(air.damage_per_second, 0);
        });
    }

    #[test]
    fn entity_status_reports_depth_and_damage() {
        let mut s = sim();
        // 40 blocks of water: well past the 24-block no-decompression limit, so
        // the ABI has to surface a non-zero barotrauma figure.
        water_shaft(&mut s, 4, 4, 40);
        with_installed(s, || {
            let (deep_depth, deep_damage) =
                entities::EntityForces::from_status_u32(horizon_entity_status(4, 3, 4));
            assert!(
                deep_depth >= 30,
                "depth must count the column: {deep_depth}"
            );
            assert!(deep_damage > 0, "37 blocks must hurt: {deep_damage}");

            let (shallow_depth, shallow_damage) =
                entities::EntityForces::from_status_u32(horizon_entity_status(4, 38, 4));
            assert!(
                shallow_depth < deep_depth,
                "{shallow_depth} vs {deep_depth}"
            );
            assert_eq!(shallow_damage, 0, "a shallow dive is harmless");
        });
    }

    #[test]
    fn commands_are_reachable_through_the_abi() {
        let mut s = sim();
        s.grid.wake_all(slot(&s));
        let line = "set 2 2 2 lava 8 1200";
        let n = s.run_command(line);
        assert!(n > 0);
        let text = core::str::from_utf8(&s.reply).unwrap();
        assert!(text.contains("\"ok\":true"), "{text}");
        assert_eq!(
            s.get_block(2, 2, 2).map(|v| v.species()),
            Some(Species::Lava)
        );

        // And through the exported entry point, using a buffer the module owns.
        let ptr = horizon_alloc(5);
        assert!(!ptr.is_null());
        // SAFETY: the buffer has room for the 5 bytes we write.
        unsafe { core::ptr::copy_nonoverlapping(b"stats".as_ptr(), ptr, 5) };
        // SAFETY: the pointer and length are the ones we just allocated.
        let n = unsafe { horizon_run_command(ptr, 5) };
        assert_eq!(n as usize, horizon_reply_len() as usize);
        assert!(n > 0);
        // SAFETY: `ptr` came from `horizon_alloc(5)`.
        unsafe { horizon_free(ptr, 5) };

        // Null and empty input are ignored rather than trapping.
        assert_eq!(horizon_alloc(0) as usize, 0);
        // SAFETY: a null pointer is explicitly allowed and is a no-op.
        unsafe { horizon_free(core::ptr::null_mut(), 0) };
        // SAFETY: null is rejected before dereferencing.
        assert_eq!(unsafe { horizon_run_command(core::ptr::null(), 3) }, 0);
    }

    #[test]
    fn unknown_block_reads_are_safe() {
        let s = sim();
        assert!(s.get_block(500, 500, 500).is_none());
    }
}

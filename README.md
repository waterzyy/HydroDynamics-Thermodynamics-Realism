# HydroDynamics & Thermodynamics Realism

A from-scratch **fluid, thermal and phase-transition simulation core** for the
[Horizon](https://github.com/waterzyy/vibecode-test) voxel engine, written in Rust
and compiled to `wasm32-unknown-unknown` as a `cdylib`.

No dependencies. No floating point in the tick. No allocations in the tick.
No work at all for a settled world.

```
 ┌──────────────────────────── Horizon engine (host) ───────────────────────────┐
 │  world_tick ─▶ horizon_tick()        player_join ─▶ horizon_on_player_join() │
 │  set_block  ─▶ horizon_set_block()   renderer    ─▶ horizon_event_pop()      │
 └───────────────────────────────────┬──────────────────────────────────────────┘
                                     │  wasm32 ABI (integers only)
 ┌───────────────────────────────────▼──────────────────────────────────────────┐
 │ Sim                                                                          │
 │  Grid ── 24 × Chunk(16³, 32 KiB) ── active list (double-buffered)            │
 │   │                                                                          │
 │   ├─ hydro::tick_cell      gravity → communicating vessels → gas inversion   │
 │   ├─ hydro::update_pressure hydrostatic head  p = p_above + ρg·m/8           │
 │   ├─ thermo::tick_cell      conduction → radiation → phase machine → quench  │
 │   ├─ hydro::fracture        brittle blocks give way to the head              │
 │   └─ grid.update_sleep      quiet chunks fall asleep (0 CPU footprint)       │
 └──────────────────────────────────────────────────────────────────────────────┘
```

---

## 1. What it simulates

### Thermodynamics and phase transitions

Every voxel — fluid, gas **or solid** — stores `mass: u8 (0..8)`,
`temperature: i16 (-273..2000 °C)` and `pressure: u16` (quarter-pascals). Solids
carry temperature too, which is what lets lava heat the basalt around it and an
ice sheet chill the water beneath it.

| Interaction | Implementation |
|---|---|
| Water below 0 °C freezes | `thermo::freeze_cell`, ice keeps the *mass*, so the sheet is thin at mass 1–3 and solid at 8 (thickness = mass) |
| Freezing releases latent heat | Ice forms **warmer** than the water it came from (hysteresis: freeze −1 °C, melt +1 °C) so nothing ping-pongs |
| Water over hot blocks warms up | Conduction is a heat-capacity-weighted relaxation; a lava cell heats stone, which heats the pool above |
| Ascending convection bubbles | `thermo::nucleate` marks `BUBBLE`/`UPDRAFT`; the flags travel up the water column one cell per tick and the entity layer turns them into lift |
| Water at the boiling point → steam | `thermo::boil_cell` moves mass into the cell above, applies evaporative cooling, conserves mass exactly. Sealed containers *cannot* boil: pressure raises the boiling point first (pressure cooker) |
| Steam rises to a solid ceiling | `hydro::try_rise` + density inversion (steam swaps with water, water sinks) |
| Steam condenses on a cold ceiling | `Class::Solid` at ≤ 45 °C condenses the vapour; the droplets fall back down the shaft |
| Lava gives heat to the rock | `Material::conductivity` from a real block database; strong 80 W/m·K metal vs 0.6 W/m·K water ordering is preserved |
| Boiling water meets lava | `thermo::try_quench`: flash vaporisation, pressure spike, micro-explosion — **obsidian** (cold deep water), **volcanic glass** (hard quench) or **pumice** (shallow splash, and it floats) |
| Metal, wood, fire, snow | Melting (iron 1 538 °C), ignition (320 °C), burnout, snow melting — all in the same phase machine |

### Hydrostatics and pressure

* **Communicating vessels.** The levelling rule moves `(m_self − m_other)/2` into
  the emptier neighbour. Halving a *difference* makes the equal level a true
  fixed point: a U-tube reaches `|Δh| ≤ 1/8 block` and stops — which is also why
  the chunk can fall asleep.
* **Hydrostatic head.** `p[i] = p[above] + ρ·g·(mass/8)`, evaluated locally from
  the column above; pressure grows one block of water per block of depth and
  converges in a few ticks.
* **Jets.** Torricelli's law in fixed point, `v = √(2gh)` in 1/16 m/s:
  a 1-block head → ~4.4 m/s, a 10-block head → ~14 m/s. The jet pushes
  entities (see `hydro::jet_velocity` / `entities::jet_push`).
* **Fracture.** Pressurised cells grind brittle neighbours down using the
  material yield pressures: leaves ≈ 0.7 m, glass ≈ 2 m, planks ≈ 2.75 m,
  dirt 8 m, stone 35 m, obsidian 40 m, bedrock never.
* **Archimedes.** `a = g(ρ_fluid − ρ_entity)/ρ_entity`, with the *actually
  present* density (a half-full water block weighs half). Oak (700 kg/m³) floats,
  iron armour (7 870) sinks, pumice (600) floats *up through* water, lava
  (2 600) floats a stone golem.

### Extreme optimisation

1. **Sleep / wake grid.** Each chunk tracks how many of its voxels are on the
   active list. No active voxels for 20 ticks → the chunk is marked `ASLEEP`;
   every kernel skips it in O(1). A settled basin costs **zero voxel
   evaluations** — measured by `Sim::tick()` returning `0`.
2. **Bitmask neighbourhood.** Chunks are flat `[Voxel; 4096]` arrays, Y-major,
   indexed `(y<<8)|(z<<4)|x`. A precomputed `const` border-mask table
   (`dirs::BORDER_MASKS`, 4 KiB) answers "does this direction leave the chunk?"
   without six comparisons; the interior step is a single integer add. No heap
   access, no `HashMap`, no allocation anywhere in the tick.
3. **Fixed point.** Temperatures in °C, pressure in ¼ Pa, mass in ⅛ blocks,
   velocity in 1/16 m/s, conduction in Q16, radiation in deci-kelvin T⁴. Integer
   square roots use the digit-by-digit bit-pair method (no divides); the thermal
   solver's per-pair energy invariant keeps rounding drift at ±½ °C.
4. **Budgeted, deterministic scheduling.** At most `tick_budget` voxels per tick;
   the remainder is deferred to the next tick instead of freezing the server.
   Work lists are double-buffered, so a cell is evaluated **at most once per
   tick** (no 100-block waterfalls in one frame) and arrival order never changes
   the result.

## 2. In-game education: "Atlas of Fluid Physics"

On first join (and on respawn) the player receives a generated, illustrated,
colour-formatted book. The content is compiled into the WASM module and
serialised on demand — as **NBT** (a vanilla-openable written book) *and* as
**JSON** text components for datapacks and the guide overlay.

Chapters:

1. **Communicating Vessels & Dam Engineering** — Сообщающиеся сосуды и строительство дамб
2. **Thermal Springs: Heating Pools, Melting Metals** — Термальные источники
3. **Steam Engines & Condensation in the Mines** — Паровые двигатели и конденсация в шахтах
4. **Deep Diving: Compression Injuries** — Опасность глубоких погружений: компрессионные травмы
5. **Inside the Simulation** — почему это быстро (sleep grid, bitmasks, fixed point)
6. **Field Reference** — *generated at runtime* from the material database and the
   live `[tunables]`, so a modpack that boils water at 80 °C gets a book that
   says 80 °C

Pages carry hex colours, chapter ribbons, illustration frames
(`hydrodynamics:textures/book/*.png`), bullet lists, formula blocks and tables.

## 3. Repository layout

| Path | Contents |
|---|---|
| `Cargo.toml` | `crate-type = ["cdylib", "rlib"]`, size-first release profile |
| `horizon-mod.toml` | engine manifest: exports, events, fluids, gases, blocks, book, commands, tunables, presets |
| `src/lib.rs` | WASM entry points, tick orchestrator, event ring buffer |
| `src/units.rs` | fixed-point types, integer roots, deterministic LCG |
| `src/dirs.rs` | six-neighbourhood, flat chunk indexing, border bitmasks |
| `src/voxel.rs` | the 8-byte voxel + real-world material database |
| `src/config.rs` | `Tunables` and the `key = value` bridge |
| `src/grid.rs` | chunk store, active work lists, sleep/wake, save/load |
| `src/hydro.rs` | the communicating-vessels automaton, head, jets, fracture, buoyancy |
| `src/thermo.rs` | conduction, radiation, geothermal, phase machine, quench |
| `src/entities.rs` | Archimedes for entities, updrafts, drag, compression injuries |
| `src/book.rs` | the Atlas generator (JSON + hand-written NBT encoder) |
| `tests/physics.rs` | end-to-end physics scenarios |

## 4. Build

```bash
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
# -> target/wasm32-unknown-unknown/release/hydrodynamics.wasm
```

Optional size pass (the module has no imports, so it links standalone):

```bash
wasm-opt -Oz --strip-debug hydrodynamics.wasm -o hydrodynamics.min.wasm
```

Native test suite (runs on the host, no WASM runtime needed):

```bash
cargo test --all-targets          # 100+ unit + integration assertions
cargo clippy --all-targets -- -D warnings
```

## 5. WASM ABI

All exports take and return integers; no Rust type ever crosses the boundary.

| Export | Signature | Notes |
|---|---|---|
| `horizon_api_version` | `() -> u32` | `0x0001_0004` |
| `horizon_on_world_load` | `(seed_hi: u32, seed_lo: u32, ambient_c: i32) -> u32` | returns the book NBT length |
| `horizon_configure` | `(k_ptr, k_len, v_ptr, v_len) -> u32` | one `key = value` pair |
| `horizon_tick` | `() -> u32` | voxels evaluated this tick |
| `horizon_on_player_join` | `(player: u32, is_respawn: u32) -> u32` | grants the Atlas |
| `horizon_book_nbt_ptr` / `_len` | `() -> *const u8` / `() -> u32` | written-book blob |
| `horizon_book_json_ptr` / `_len` | `() -> *const u8` / `() -> u32` | JSON twin |
| `horizon_get_block` / `horizon_set_block` | `(x,y,z) -> u64` / `(x,y,z,word) -> u32` | packed voxel |
| `horizon_probe` | `(x,y,z) -> u32` | species/mass/temp/pressure for the HUD |
| `horizon_stats_ptr` | `() -> *const u32` | `[active, processed, requeued, chunks, asleep, total_mass, tick, events]` |
| `horizon_event_pop` | `() -> u64` | `kind|slot|index|payload`, `u64::MAX` when empty |

The host owns the memory: pointers stay valid until the next `horizon_tick`.

## 6. Tuning

`horizon-mod.toml → [tunables]` is forwarded through `horizon_configure`. Unknown
keys are ignored, unparsable values are no-ops, and every value is clamped by
`Tunables::sanitize` so a hostile datapack cannot destabilise the solver
(e.g. `thermo.conduction_q16` is capped at 0.5, the explicit-solver stability
limit).

Presets: `/hd profile low|default|high`
(`Tunables::low_end()` / `DEFAULT` / `high_fidelity()`).

## 7. Known limitations & next steps

* Fluids are a cellular automaton on a 8-level mass field, not a Navier–Stokes
  solver: there is no momentum field, so whirlpools and waves are approximated by
  the jet vector, not simulated.
* Gas volume does not expand with temperature (no ideal-gas law for steam yet);
  pressure is hydrostatic + explosive spikes.
* Boiling is limited by free volume above the cell, which is a deliberate design
  choice that gives pressure cookers for free.
* The book ships in English with Russian chapter subtitles; a `lang` layer for
  full localisation is straightforward (all strings live in `src/book.rs`).

## 8. License

MIT OR Apache-2.0.

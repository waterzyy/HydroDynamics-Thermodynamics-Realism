# HydroDynamics & Thermodynamics Realism

[![CI](https://github.com/waterzyy/vibecode-test/actions/workflows/ci.yml/badge.svg)](https://github.com/waterzyy/vibecode-test/actions)
![target](https://img.shields.io/badge/target-wasm32--unknown--unknown-blueviolet)
![deps](https://img.shields.io/badge/dependencies-none-success)

A fluid, thermal and phase-transition core for the **Horizon** voxel engine,
compiled to `wasm32-unknown-unknown` as a `cdylib`.

It replaces vanilla's "flowing water levels" with a mass- and energy-conserving
cellular automaton: water finds its level through a hydraulic head gradient,
pressure grows with depth, magma boils pools into steam columns, steam condenses
on cold ceilings and drips back down, and a breached dam knocks a player off
their feet. Every kernel is integer-only, allocation-free and deterministic, so
a settled world costs **zero CPU**.

---

## Кратко (по-русски)

* **Термодинамика и фазовые переходы.** Каждый воксель хранит `mass 0..8`,
  `temperature −273..+2000 °C` и `pressure` (250 Па). Вода замерзает в лёд
  толщиной по массе, кипит при 100 °C (точка кипения растёт с давлением), пар
  поднимается до холодного потолка и конденсируется каплями; лава отдаёт тепло
  породе и при контакте с водой даёт обсидиан/пемзу/вулканическое стекло +
  микровзрыв давления.
* **Гидростатика.** Сообщающиеся сосуды выравнивают горизонт через градиент
  гидравлического напора; давление столба растёт с глубиной и разрушает хрупкие
  блоки (стекло, доски, листва), пробитая дамба даёт струю по Торричелли, а
  Архимед топит броню и держит дерево и пемзу на плаву.
* **Производительность.** Спящие чанки не считаются вообще; поиск шести
  соседей — плоские 1D-массивы и битовые маски; вся арифметика целочисленная
  (fixed-point); бюджет вокселей на тик переносит работу, а не отбрасывает её.
* **Атлас гидролога.** `src/book.rs` генерирует иллюстрированную книгу
  «Atlas of Fluid Physics» (NBT для `written_book` + JSON-двойник) и выдаёт её
  при первом входе и респавне.
* **Проверка.** `cargo test` — 160 тестов (юнит + интеграционные сценарии),
  `wasm` собирается в ~95 КБ без единой внешней зависимости.

---

## Physics

### Thermodynamics and phase transitions (`src/thermo.rs`)

Every cell carries `mass ∈ 0..=8`, `temperature ∈ −273..=2000 °C` and a
hydrostatic `pressure`. Three energy paths are integrated per tick:

| Path | Model |
|---|---|
| Conduction | heat-capacity-weighted relaxation of each *pair* towards `T_eq = (CₐTₐ+C_bT_b)/(Cₐ+C_b)`, so `CₐTₐ+C_bT_b` is invariant to ±½ °C |
| Radiation | integer Stefan–Boltzmann `q ∝ ε(T_k⁴ − T_amb⁴)` in deci-kelvin |
| Geothermal | relaxation towards the chunk ambient, which the engine derives from depth |

The phase machine is hysteretic (`freeze −1 °C` / `melt +1 °C`, `boil 100 °C` /
`condense 95 °C`) so a cell cannot ping-pong between two species and keep its
chunk awake forever:

```
Water ──(−1 °C)──▶ Ice ──(+1 °C)──▶ Water ──(100 °C @1 atm)──▶ Steam
Lava  ──(<700 °C)──▶ Basalt        Steam ──(≤ condense point)──▶ Water droplets
Lava  + water ──▶ Obsidian | Volcanic glass | Pumice + Steam + 💥 pressure spike
Wood/Plank/Leaves ──(≥320 °C)──▶ Fire ──(<150 °C)──▶ Air
```

* **Ice thickness follows the freezing mass** — `mass 1..3` is a thin sheet,
  `mass 8` a solid block; freezing releases latent heat, which is why a pond
  freezes from the surface down.
* **The boiling point tracks pressure** through an integer Clausius–Clapeyron
  (`+28 °C` per doubling): a sealed shaft boils at ~130 °C, a vacuum kettle at
  ~45 °C. Water under a solid ceiling therefore *cannot* boil — pressure first.
* **Convection** nucleates above magma/lava/fire, sets `BUBBLE|UPDRAFT` flags
  that travel up the water column, and the entity layer turns them into an
  updraft (see below).
* **Quenching** is a steam explosion: the water flashes to vapour, the lava
  freezes into obsidian (cold, deep water), volcanic glass (already-cooling
  melt) or pumice (slow chilling), and a pressure wave shatters brittle blocks
  in the blast radius.

### Hydrostatics and pressure (`src/hydro.rs`)

```
p[i] = p[above] + ρg · mass[i]/8          (hydrostatic recurrence, 1 neighbour read)
H[i] = mass[i] + column_above[i]          (hydraulic head, straight from p[i])
```

Water moves horizontally from the higher head to the lower one, by at most half
the head difference — the discrete form of **communicating vessels**. Because a
pair can never overshoot (`Δ ≤ q/2`) and moves need a head margin (`q ≥ 2`), a
levelled body is a true fixed point: no oscillation, no cycle, and the chunk can
fall asleep. A *supported* cell (a full block, or something standing on it) may
additionally push one unit into a completely empty neighbour — that neutral move
is a one-way ratchet, which is what lets a thin trickle cross a flat floor.

* **Toricelli jets** — `v = √(2gh)` in fixed point, so a 1 m head yields ~4.4 m/s
  and a 10 m head ~14 m/s, applied to entities standing in the stream.
* **Fracture** — the excess head is compared against each material's yield
  pressure: leaves burst at 0.65 m, planks at 2.75 m, glass at 2 m, ice at 6 m,
  stone holds 35 m, obsidian 40 m.
* **Buoyancy** — `a = g(ρ_fluid−ρ_entity)/ρ_entity`: oak floats (+0.42 g), iron
  armour sinks (−0.87 g), lava floats even a golem.
* **Updrafts** are entrainment, not buoyancy: a two-phase bubble column imposes
  its own velocity, so `a = max(buoyancy, bubble_lift)` lifts even a fully
  armoured diver out of a boiling cauldron.

### Entities (`src/entities.rs`)

Archimedes, drag (`180/256` per tick in water vs `4/256` in air), jet push,
terminal velocity of loose blocks, breathability and the **compression-injury**
curve of the Atlas' diving chapter (nothing below 24 m, quadratic after that,
saturating at 40 half-hearts/s).

---

## Performance engineering

| Technique | Where | Effect |
|---|---|---|
| **Sleep / wake grid** | `grid.rs` — `front`/`back` work lists, `ASLEEP` chunks | a settled basin costs one `u16` comparison per tick, no voxel work at all |
| **Double-buffered active list** | `grid.rs` | a voxel woken *by* the current tick is evaluated next tick: no double integration, no cascading waterfalls in one tick |
| **Bitmask neighbourhood** | `dirs.rs` — `BORDER_MASKS[4096]`, `step()` | six comparisons per neighbour query collapse into a table lookup; interior steps are a bare pointer add |
| **Fixed-point math** | `units.rs` | no `f32`/`f64` anywhere in the tick path; `i16` temperature, `u16` pressure (250 Pa), integer sqrt/log2/exp helpers |
| **Flat, allocation-free storage** | `voxel.rs`, `grid.rs` | `Voxel` is exactly 8 B, a chunk is 32 KiB of `[Voxel; 4096]` (Y-major, so a column walk stays on one cache line set); the only heap allocation in the whole mod is the chunk pool at world load |
| **Tick budget / temporal LOD** | `lib.rs`, `grid.rs::rotate` | at most `perf.tick_budget` voxels per tick; the remainder is *deferred one tick*, never dropped |
| **Determinism** | `units.rs::chunk_seed` | per-chunk LCG seeded from the world seed + coordinates, fixed iteration order: server and client replays are bit-identical |

Memory: 24 resident chunks × 32 KiB ≈ 768 KiB — inside the manifest's 16 MiB
`memory` budget, with room for the book blobs.

---

## Inside the loop

```
horizon_tick() ─▶ Sim::tick()
                    ├─ grid.begin_tick()             roll the work lists
                    ├─ for each active voxel (≤ budget):
                    │    hydro::tick_cell()          pressure → gravity → levelling → buoyancy
                    │    hydro::update_pressure()    hydrostatic head
                    │    thermo::tick_cell()         conduction → radiation → phases → convection
                    │    hydro::fracture()           brittle blocks give way
                    │    settle() / wake_next()      re-queue or sleep
                    ├─ grid.rotate(processed)        deferred work → next tick
                    ├─ grid.update_sleep()           quiet chunks fall asleep
                    └─ stats / events
```

| module | responsibility |
|---|---|
| `units.rs` | fixed-point scalars, integer roots/logs, deterministic RNG |
| `dirs.rs` | six-neighbourhood, flat index layout, border bitmasks |
| `voxel.rs` | the 8-byte voxel + material database (density, conductivity, yield, colours) |
| `config.rs` | tunables mirrored from `horizon-mod.toml` |
| `grid.rs` | chunk pool, active list, sleep/wake, save/load, diagnostics |
| `hydro.rs` | communicating vessels, head, jets, buoyancy, fracture |
| `thermo.rs` | conduction, radiation, phase machine, quench, convection |
| `entities.rs` | Archimedes, updrafts, drag, compression injuries, the packed entity word |
| `commands.rs` | the `/hd` console surface declared in the manifest (JSON replies) |
| `book.rs` | the generated *Atlas of Fluid Physics* (JSON + NBT) |
| `lib.rs` | the WASM ABI, the sim facade, the event ring |

---

## The Atlas of Fluid Physics

`src/book.rs` builds the in-game written book from the live tunables, so the
numbers a player reads are the numbers the simulator uses.

| Chapter | Content |
|---|---|
| Сообщающиеся сосуды и строительство дамб | U-tubes, culverts **two blocks high**, dam profiles, pressure arrows |
| Термальные источники: подогрев водоёмов и плавка | magma-heated pools, bubble columns, boiling point vs. pressure |
| Паровые двигатели и конденсация в шахтах | shafts, cold roofs, drip-back, sealed-volume boiling |
| Опасность глубоких погружений: компрессионные травмы | the 24 m free zone, the quadratic damage curve |
| Внутри симуляции: почему это быстро | sleep/wake, bitmasks, fixed point, budget |
| Справочник: материалы и константы | the material table straight out of `voxel::MATERIALS` |

Export: `horizon_book_nbt_ptr/len` (a `written_book` NBT blob the host hands
straight to the inventory) and `horizon_book_json_ptr/len` (the same content as
JSON text components, one `#RRGGBB` colour per chapter). `horizon_on_player_join`
grants it on first join and on respawn; the byte length is the return value.
Illustration briefs live in [`docs/book-assets.md`](docs/book-assets.md).

---

## WASM ABI

All entry points are `extern "C"` with integer arguments only; the module has
**zero host imports** and never allocates inside the tick. Strings travel the
other way — the host asks for a buffer with `horizon_alloc`, fills it, and calls
`horizon_configure` / `horizon_run_command` with that pointer.

| export | signature | purpose |
|---|---|---|
| `horizon_api_version` | `() -> u32` | ABI handshake (`0x0001_0004`) |
| `horizon_alloc` / `horizon_free` | `(len) -> *mut u8` / `(ptr, len)` | argument buffers owned by the module |
| `horizon_on_world_load` | `(seed_hi, seed_lo, ambient_c) -> u32` | build the grid + the Atlas, return its NBT length |
| `horizon_configure` | `(k_ptr, k_len, v_ptr, v_len) -> u32` | one `key = value` from the manifest (1 = accepted) |
| `horizon_tick` | `() -> u32` | one world tick; returns voxels evaluated |
| `horizon_on_player_join` | `(player, is_respawn) -> u32` | grant the Atlas, returns the blob length |
| `horizon_book_nbt_ptr` / `_len` | `() -> *const u8` / `u32` | written-book NBT |
| `horizon_book_json_ptr` / `_len` | `() -> *const u8` / `u32` | JSON twin of the book |
| `horizon_get_block` | `(x, y, z) -> u64` | packed voxel (`Voxel::to_u64`) |
| `horizon_set_block` | `(x, y, z, word) -> u32` | write + wake the neighbourhood |
| `horizon_probe` | `(x, y, z) -> u32` | species/mass/temp/pressure for the HUD |
| `horizon_entity_forces` | `(x, y, z, density) -> u64` | buoyancy + updraft + jet push + drag, packed |
| `horizon_entity_status` | `(x, y, z) -> u32` | submersion depth + barotrauma (`depth | damage << 16`) |
| `horizon_run_command` | `(ptr, len) -> u32` | one `/hd` line → JSON reply |
| `horizon_reply_ptr` / `_len` | `() -> *const u8` / `u32` | the reply of the last command |
| `horizon_stats_ptr` | `() -> *const u32` | 8 diagnostics words: active, processed, requeued, chunks, asleep, mass, tick, events |
| `horizon_event_pop` | `() -> u64` | next physics event (`kind | slot | idx | payload`, `u64::MAX` = empty) |

Events: `1 freeze`, `2 melt`, `3 boil`, `4 condense`, `5 quench (payload = product species)`,
`6 shatter (payload = species)`, `7 ignite`.

### The entity word

```text
bits  0..16  vertical acceleration, jet included   i16, 1/16 m/s²
bits 16..32  horizontal jet push along X           i16, 1/16 m/s
bits 32..48  horizontal jet push along Z           i16, 1/16 m/s
bits 48..56  drag                                   u8, 1/256 per tick
bits 56..64  flags: submerged | updraft | breathable | positive buoyancy
```

The integrator calls it once per entity per tick with the entity's own density
(700 for a boat, 1 050 for a swimmer, 2 100 for iron armour) and gets back
everything the fluid does to it — no per-entity knowledge of the mod's internals.

### `/hd` console

The manifest's command tree is implemented in the module, not in the host, so a
tunable changed from the console goes through the same `Tunables::apply_kv` path
as the TOML loader:

| command | effect |
|---|---|
| `/hd stats` | tick, active/processed/deferred voxels, chunks, asleep, mass, events |
| `/hd book [chapter]` | grant the Atlas, optionally report one chapter |
| `/hd probe <x> <y> <z>` | dump one voxel (species, mass, temperature, pressure, flow) |
| `/hd set <x> <y> <z> <species> [mass] [temp]` | write a voxel through `Grid::set_world` |
| `/hd wake <radius> [x y z]` | re-queue resident chunks ("the water looks stuck") |
| `/hd profile <low\|default\|high>` | apply a preset, keeping the seed and book policy |

Every reply is JSON: `{"ok":true,...}` or `{"ok":false,"error":"..."}`. A bad
argument is a message, never a trap — the console has to stay usable when the
simulation is the thing that is broken.

---

## Build and test

```bash
cargo test --all-targets              # 160 tests: unit + end-to-end scenarios
cargo clippy --all-targets
cargo doc --no-deps

rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
./scripts/build-wasm.sh --opt          # optional wasm-opt -Oz pass + size report
```

The crate builds both `cdylib` (what the engine loads) and `rlib` (so the whole
physics suite runs natively, no WASM runtime needed). Release profile: `opt-level
= "s"`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, `strip = true`
→ **~115 KiB** of WASM.

CI (`.github/workflows/ci.yml`) runs clippy, the full test suite, `cargo doc`, a
`wasm32-unknown-unknown` release build and a 2 MiB artifact-size gate.

## Repository layout

```
horizon-mod.toml     mod manifest: fluids, gases, blocks, the book, events, tunables
Cargo.toml           cdylib + rlib, zero dependencies
rust-toolchain.toml  stable + wasm32-unknown-unknown
src/                 the engine (see the module table above)
tests/physics.rs     end-to-end scenarios driven through the public API only
docs/                illustration briefs for the Atlas
scripts/             wasm build helper
```

---

## Known and deliberate limitations

The automaton is local by construction (that is what makes it fast and
sleepable), so a few situations are modelled with an explicit, documented
compromise instead of a global solve:

* **A plugged gallery transmits pressure but not mass.** A one-block-high tunnel
  packed to 8/8 along its whole length drives jets, fracture and the boiling
  point, but water will not flow through it: doing so needs an incompressible
  global solve. The Atlas therefore tells players to build culverts **two blocks
  high** — which is what real dams do anyway.
* **The 1/8-block resolution floor.** Mass moves in eighths, so a pair of cells
  that differ by exactly one eighth is a *discrete* fixed point even though a
  real liquid would keep levelling. The kernel breaks exactly one of those ties —
  a unit may roll **downhill** into a one-unit-lower neighbour when the cell
  beyond it is lower still (an energy-neutral roll that can never be played
  backwards) — and refuses all the others. That is why one unit on bare rock is
  a settled film instead of a cell that roams forever, and why a breached dam
  drains hard for a while and then quiesces with a very shallow sheet outside
  rather than equalising all the way: liquid never climbs, so an open plain
  floods one block deep, not to the height of the reservoir.
* **Rock products leave the water tally.** Water/ice/steam/snow are a closed
  cycle and are conserved exactly (`Chunk::total_mass`); lava solidifying into
  obsidian, pumice or basalt is a phase change into *terrain*, and solids carry
  `MASS_FULL` as a "not hollow" marker rather than a conserved amount.
* **Gas heat capacity is per voxel, not per kilogram** — deliberately generous
  relaxation is capped at 8 °C/tick so a steam plume survives long enough to
  travel up a shaft.

## License

MIT OR Apache-2.0.

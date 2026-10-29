# HydroDynamics

Water and heat for the Horizon voxel engine. The simulation is a WebAssembly
module (`wasm32-unknown-unknown`, `cdylib`) with no dependencies.

Vanilla water flows by level, which makes dams, siphons, steam and diving awkward
to build because nothing in the world has weight. Here each voxel stores mass,
temperature and pressure, and water moves because pressure differs between
neighbours. Pools warm up and boil, steam rises until it meets something cold,
ice grows thicker where the water is deeper, and a hole at the bottom of a
reservoir throws a jet.

Settled water stops costing CPU.

## Install

Take `hydrodynamics-1.0.0.hmod` from the [releases page](../../releases), put it
in the world's `mods/` folder, restart the server. The archive is a plain ZIP:

```
hydrodynamics-1.0.0.hmod
├── horizon-mod.toml    manifest; wasm path already points inside the archive
├── hydrodynamics.wasm  the module, about 115 KiB
├── README.md
├── CHANGELOG.md
└── SHA256SUMS
```

Checking a download:

```bash
sha256sum hydrodynamics-1.0.0.hmod
unzip -p hydrodynamics-1.0.0.hmod SHA256SUMS | sha256sum -c -
```

## What it simulates

### Heat and phase changes (`src/thermo.rs`)

Water freezes below 0 °C and the ice keeps the thickness of the water it came
from, so a deep lake freezes into a full block and a puddle into a thin sheet.
Lava and magma warm the rock around them and the water above them. At 100 °C
water boils into steam, and the boiling point rises with depth because pressure
raises it. Steam climbs until it reaches a cold ceiling, condenses and drips
back. Water touching lava flashes to vapour, the pressure spike breaks the
surrounding blocks, and the lava cools into obsidian, pumice or volcanic glass
depending on how much water reached it.

### Water finding its level (`src/hydro.rs`)

Two vessels joined by an open culvert end up at the same height. Depth adds
pressure, and enough of it cracks glass, planks and leaves while stone holds. A
breach at the bottom of a dam produces a jet, and the jet pushes whatever stands
in it. Wood and pumice float, iron armour does not.

### Entities (`src/entities.rs`)

The engine asks for one packed word per entity per tick and gets buoyancy,
updraft, jet push and drag back. A bubble column lifts anything inside it,
including an armoured diver, because the vapour drags it along. Below 24 metres
the water starts doing damage, and the damage grows with the square of the depth
past that point.

## Performance

A chunk that has stopped changing goes to sleep and is skipped entirely.

| What | How |
|---|---|
| Storage | 8 bytes per voxel, 32 KiB per chunk, flat `[Voxel; 4096]` |
| Neighbours | flat 1D array plus a bitmask per position |
| Math | fixed point integers, no floats in the tick |
| Work queue | two `Vec<u32>` swapped per tick, no allocation |
| Budget | a tick evaluates at most `tick_budget` voxels and defers the rest |

The module is 114 KB. The test suite has 160 tests and the crate has no
dependencies.

## The book

First join and every respawn hand the player a written book, *Atlas of Fluid
Physics* with the Russian subtitle «Атлас гидролога». Pages are generated from
the live config, so the text quotes the numbers the simulation actually uses: the
24 metre diving limit, the boiling point at the current pressure, the yield
pressures of the materials. Six chapters cover communicating vessels and dams,
thermal springs and smelting, steam engines and condensation in mines, deep
diving injuries, how the simulation works, and a material reference.

`horizon_book_nbt_ptr/len` returns the NBT for a `written_book`;
`horizon_book_json_ptr/len` returns the same content as JSON text components.

## API

Every entry point is `extern "C"` with integer arguments. The module imports
nothing.

| Export | Signature | What it does |
|---|---|---|
| `horizon_api_version` | `() -> u32` | ABI revision, `0x0001_0004` |
| `horizon_alloc` / `horizon_free` | `(len)` / `(ptr, len)` | buffers for arguments |
| `horizon_on_world_load` | `(seed_hi, seed_lo, ambient_c) -> u32` | builds the grid and the book |
| `horizon_configure` | `(k_ptr, k_len, v_ptr, v_len) -> u32` | one `key = value` pair, 1 on success |
| `horizon_tick` | `() -> u32` | one tick, returns voxels evaluated |
| `horizon_on_player_join` | `(player, is_respawn) -> u32` | grants the book, returns its length |
| `horizon_get_block` | `(x, y, z) -> u64` | reads a voxel |
| `horizon_set_block` | `(x, y, z, word) -> u32` | writes a voxel and wakes the neighbours |
| `horizon_probe` | `(x, y, z) -> u32` | species, mass, temperature, pressure for the HUD |
| `horizon_entity_forces` | `(x, y, z, density) -> u64` | buoyancy, updraft, jet, drag |
| `horizon_entity_status` | `(x, y, z) -> u32` | depth and compression damage |
| `horizon_run_command` | `(ptr, len) -> u32` | one `/hd` line, JSON back |
| `horizon_reply_ptr` / `_len` | `() -> *const u8` / `u32` | the last reply |
| `horizon_stats_ptr` | `() -> *const u32` | eight counters |
| `horizon_event_pop` | `() -> u64` | next event for particles and sounds |

Events: 1 freeze, 2 melt, 3 boil, 4 condense, 5 quench, 6 shatter, 7 ignite.

### The entity word

```
bits  0..16  vertical acceleration, jet included   i16, 1/16 m/s²
bits 16..32  jet push along X                      i16, 1/16 m/s
bits 32..48  jet push along Z                      i16, 1/16 m/s
bits 48..56  drag                                  u8, 1/256 per tick
bits 56..64  submerged, updraft, breathable, positive buoyancy
```

Call it once per entity per tick with the entity's density: 700 for a boat, 1050
for a swimmer, 2100 for a player in iron armour.

### `/hd` commands

`stats`, `book [chapter]`, `probe <x> <y> <z>`,
`set <x> <y> <z> <species> [mass] [temp]`, `wake <radius> [x y z]`,
`profile <low|default|high>`, `help`. Replies are JSON, and a bad argument comes
back as an error instead of a trap, since the console is what you reach for when
the simulation is misbehaving. `/hd set` goes through `Grid::set_world`, so the
scheduler still owns the wake flags, and `/hd profile` keeps the world seed.

## Building

```bash
cargo test --all-targets
cargo clippy --all-targets
cargo doc --no-deps

rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
./scripts/build-wasm.sh --opt     # optional wasm-opt -Oz pass
```

The crate builds both `cdylib` for the engine and `rlib` so the physics tests
run natively. The release profile uses `opt-level = "s"`, fat LTO,
`panic = "abort"` and `strip = true`.

To cut a release:

```bash
python3 scripts/package.py \
    --wasm target/wasm32-unknown-unknown/release/hydrodynamics.wasm \
    --out  dist/hydrodynamics-1.0.0.hmod
git tag -a v1.0.0 -m "1.0.0"
git push origin v1.0.0
```

CI runs clippy, the tests, a doc build, the wasm build and
`scripts/check-exports.py`, which reads the export section of the built module
and fails when it no longer matches the manifest.

## Layout

```
horizon-mod.toml   manifest: fluids, gases, blocks, the book, events, tunables
Cargo.toml         cdylib and rlib, no dependencies
CHANGELOG.md
src/units.rs       fixed point types, integer roots, RNG
src/dirs.rs        six neighbours, flat index, border masks
src/voxel.rs       8 byte voxel and the material table
src/grid.rs        chunk pool, work queue, sleep and wake
src/hydro.rs       levelling, pressure, jets, fracture
src/thermo.rs      conduction, radiation, phase changes, quench
src/entities.rs    buoyancy, updraft, drag, damage
src/book.rs        the Atlas
src/commands.rs    the /hd commands
src/lib.rs         the wasm surface
tests/physics.rs   end to end scenarios
```

## Where the model is approximate

A one block high tunnel packed full of water carries pressure but no flow.
Moving mass through it needs a global solve that a local automaton cannot do, so
the book tells players to build culverts two blocks high, which is what dams do
in reality anyway.

Mass moves in eighths of a block, so two cells that differ by exactly one eighth
are a discrete fixed point. The kernel breaks one of those ties: a unit may roll
downhill when the cell past it is lower still. A staircase keeps draining, while
a film on flat rock stays put. Liquid never climbs, so an open plain outside a
breached dam floods one block deep instead of reaching the reservoir's height.

Lava that cools into obsidian or pumice leaves the water tally, because it
becomes terrain. Water, ice, snow and steam stay in a closed cycle and are
conserved to the unit.

Gas heat capacity is per voxel rather than per kilogram, and relaxation is capped
at 8 °C per tick so that a steam plume survives the climb up a shaft.

## По-русски

Мод добавляет в Horizon настоящую воду и тепло: у каждого вокселя есть масса,
температура и давление. Вода находит уровень, давит на глубине, кипит,
замерзает в лёд нужной толщины, а пар поднимается и конденсируется на холодном
потолке. Пробитая дамба бьёт струёй, стекло и доски под напором ломаются,
дерево и пемза плавают, броня тонет, а ниже 24 метров начинаются
компрессионные травмы.

Спящие чанки не считаются вообще, вся арифметика целочисленная, тик ничего не
выделяет в куче.

Установка: файл `.hmod` из раздела releases кладётся в папку `mods/` мира,
сервер перезапускается. Книга «Атлас гидролога» выдаётся при первом входе и
после респавна.

## License

MIT or Apache-2.0, whichever you prefer.

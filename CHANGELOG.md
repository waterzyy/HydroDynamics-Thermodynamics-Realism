# Changelog

All notable changes to **HydroDynamics & Thermodynamics Realism** are documented
here. The project follows [Semantic Versioning](https://semver.org): the major
number tracks the engine ABI (`horizon-mod/1.x`), the minor number the physics and
the ABI surface, the patch number fixes.

## [1.5.0] — 2026-10-29

First public release. The physics core is complete and every promise in the
manifest is backed by an exported entry point and an automated test.

### Added

- **Entity hydraulics over the ABI.** `horizon_entity_forces(x, y, z, density)`
  returns one packed word with vertical acceleration (jet included), the
  horizontal jet push along X and Z, the drag coefficient and four flags
  (submerged, updraft, breathable, positive buoyancy);
  `horizon_entity_status(x, y, z)` returns submersion depth and barotrauma. The
  engine can now ask what the water does to a boat, a swimmer or an armoured
  player without knowing anything about the mod's internals.
- **The `/hd` console, implemented in the module** (`src/commands.rs`):
  `stats`, `book [chapter]`, `probe`, `set`, `wake`, `profile`, `help`. Replies
  are JSON; a malformed command is a message, never a trap. `/hd set` writes
  through `Grid::set_world`, so the scheduler keeps sole ownership of the
  `ACTIVE` flag, and `/hd profile` preserves the world seed and book policy.
- **A real allocator export.** `horizon_alloc` / `horizon_free` let the host
  place UTF-8 argument buffers in the module's memory — the contract that
  `horizon_configure` had always documented but never had.
- **`Species::from_name`** for `/hd set` and datapack authors: case- and
  separator-insensitive, accepts a namespace prefix or a numeric id.
- **`scripts/package.py`** builds the `.hmod` archive (flat ZIP: manifest with
  `[wasm].file` rewritten to the packaged artifact, the wasm, README, changelog
  and `SHA256SUMS`), reproducibly, so the published checksum is verifiable.
- **`scripts/check-exports.py`** parses the export section of the built module
  and fails CI when the artifact stops exporting a symbol `horizon-mod.toml`
  promises — the failure mode that otherwise shows up on a player's machine.
- **A tag-driven release workflow**: version/tag agreement, full test suite,
  export check, size gate, archive layout check, then the GitHub release.

### Changed

- **Levelling kernel.** A transfer, a pressure refresh and a phase change now
  broadcast to the whole neighbourhood instead of waking only the two cells that
  moved. A cell that settled while the head was still wrong used to be stranded
  forever; a breached reservoir now drains instead of stopping at the first
  block next to the breach.
- **Sub-resolution slope creep.** A unit may roll one eighth of a block downhill
  when the cell beyond it is lower still. The move is energy-neutral and
  one-way, so it cannot oscillate — it just stops a staircase of one-unit steps
  from freezing mid-slope. Pairs that differ by more than that keep the `q >= 2`
  hysteresis, which is what lets a settled body of water go to sleep.
- **`horizon-mod.toml` now mirrors the code**: yield pressures match
  `voxel::MATERIALS`, `thermo.ambient_c` replaced the drifting
  `world.ambient_c`, and every one of the 27 tunables is accepted by
  `Tunables::apply_kv`.
- **CI is stricter**: rustfmt is enforced rather than advisory, and the format
  patch job no longer posts an empty diff.

### Fixed

- Mass conservation across condensation (vapour keeps its units when it becomes
  a droplet instead of vanishing or duplicating).
- Steam plumes survive long enough to climb a shaft: gas relaxation is capped at
  8 °C/tick instead of 64.
- `horizon_on_player_join` documentation claimed it returned a page count; it
  returns the NBT length, which is what the host needs to size its buffer.

### Known and deliberate limitations

Documented in the README rather than hidden: a plugged one-block gallery
transmits pressure but not mass (build culverts two blocks high, as real dams
do), the eighth-of-a-block resolution floor makes a settled film a fixed point,
rock products leave the water tally, and gas heat capacity is per voxel.

## [1.0.0 – 1.4.0]

Internal development builds: the fixed-point kernel, the six-neighbour
automaton, the thermal and phase machine, the sleeping grid, the generated
*Atlas of Fluid Physics* and its NBT/JSON serialisers.

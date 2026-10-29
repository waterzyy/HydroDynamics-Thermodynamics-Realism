# Changelog

## 1.0.0 (2026-10-29)

First release. The simulation is complete: everything the manifest declares has
an exported entry point and a test behind it.

### Simulation

- Water moves by pressure. A cell refreshes its hydrostatic head from the column
  above it, falls if it can, and otherwise levels towards the neighbour with the
  lower head. Gas under a liquid swaps with it, so steam bubbles up through
  water.
- Conduction relaxes two cells towards the same heat-capacity-weighted
  equilibrium, which keeps the total energy constant to within rounding.
  Radiation and geothermal warming run on top of that.
- Freezing keeps the thickness of the water it came from. Boiling moves mass into
  the vapour and condensation moves it back, so water, ice, snow and steam are a
  closed cycle. The boiling point follows pressure.
- Water touching lava flashes to vapour, the pressure spike breaks nearby blocks,
  and the lava cools into obsidian, pumice or volcanic glass.
- A pressurised cell grinds down brittle neighbours using the yield pressures in
  the material table.
- Chunks sleep. A settled world evaluates no voxels, and a tick that runs out of
  budget defers work instead of dropping it.

### API

- Entity integration: `horizon_entity_forces` returns one packed word per entity
  per tick with buoyancy, updraft, jet push and drag; `horizon_entity_status`
  returns submersion depth and barotrauma.
- The `/hd` console is implemented in the module: `stats`, `book`, `probe`,
  `set`, `wake`, `profile`, `help`. Replies are JSON.
- `horizon_alloc` and `horizon_free` let the host place argument strings in the
  module's memory.
- The book serialises to a written-book NBT blob and to JSON text components.

### Packaging

- `scripts/package.py` builds the `.hmod`, a plain ZIP with the manifest, the
  module, the docs and `SHA256SUMS`. It rewrites the manifest's wasm path so the
  archive is installable as-is.
- `scripts/check-exports.py` reads the export section of the built module and
  fails CI when it stops matching the manifest.
- Releases are built from a tag: version check, tests, export check, size gate,
  then the upload.

### Known limits

Written down in the README rather than hidden. A plugged one-block gallery
carries pressure but no flow. Mass moves in eighths of a block, so a settled
film is a fixed point. Rock products leave the water tally. Gas heat capacity is
per voxel.

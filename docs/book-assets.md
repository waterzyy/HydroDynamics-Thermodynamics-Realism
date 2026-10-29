# Atlas of Fluid Physics — illustration manifest

The book generator (`src/book.rs`) renders an **illustration frame** for every
`Block::Illustration`: the frame shows the texture path, and the engine replaces
it with the picture. The textures themselves live in the mod's resource pack, not
in this WASM module (the module only carries the layout).

| Texture path | Suggested size | Art brief |
|---|---|---|
| `hydrodynamics:textures/book/vessels_u_tube.png` | 256 × 128 | A cut-away U-shaped trench in stone; water in both arms at exactly the same level, with a dashed line across the two surfaces. |
| `hydrodynamics:textures/book/dam_profile.png` | 256 × 160 | Stepped stone dam, wide base, thin crest; water on the left with depth arrows showing pressure growing downwards. |
| `hydrodynamics:textures/book/thermal_spring.png` | 256 × 160 | Magma block under a pool; a column of bubbles rising; temperature shading from orange at the bottom to blue at the surface. |
| `hydrodynamics:textures/book/quench_obsidian.png` | 256 × 160 | A water channel meeting a lava flow; obsidian forming at the contact, pumice floating away downstream, a steam plume above. |
| `hydrodynamics:textures/book/steam_shaft.png` | 160 × 256 | A vertical mine shaft: boiler at the bottom, steam rising, cold stone roof at the top with drips falling back down. |
| `hydrodynamics:textures/book/compression_zones.png` | 160 × 256 | A depth gauge: 0–24 blocks green, 25–40 amber, 60+ red, with a diver silhouette at each band. |

## Style

* 16-colour voxel palette, no anti-aliasing, hard 1-pixel outlines (matches the
  vanilla book GUI).
* Each illustration is placed on its own line in the page JSON and is followed by
  an italic grey caption, so the picture must read at 128 px wide without text.
* Keep the background transparent: the book GUI supplies the parchment.

## Regenerating

The paths come from `src/book.rs → PAGES`; the `illustrations_reference_existing_namespace`
test asserts that every frame uses the `hydrodynamics:textures/book/` namespace,
so renaming a file requires touching one constant.

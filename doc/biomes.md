# Biomes

New worlds contain five climate regions. They use the world's seed and
absolute coordinates, so exploration order and chunk boundaries do not
change the result.

| Biome | Landscape |
| --- | --- |
| Plains | Grassy terrain with scattered trees |
| Forest | Grassy terrain with denser tree cover |
| Desert | Sand surface and subsoil, without trees |
| Tundra | Snow over soil, without trees |
| Mountains | Higher terrain with exposed rock and snowy peaks |

Temperature, moisture and relief vary over hundreds of blocks. Terrain
height blends the continuous fields rather than stepping between discrete
biome heights. The near terrain and distant LOD terrain sample the same
height and surface rules. Caves and ore veins continue beneath the surface.
Snow is a normal placeable and harvestable block with its own texture and
isometric inventory icon.

## Saved worlds

Metadata format 7 stores a `GenerationVersion`. Fresh worlds use `Biomes`;
worlds from formats 1–6 retain `Legacy`, including after saving again. This
keeps regenerated chunks consistent with existing terrain and player edits.
Create a new world to explore biomes. Modified chunks, inventories, farming
progress and factories continue to use the existing persistence system.

`WorldGenerator::new(seed)` selects the current generation version.
`WorldGenerator::with_version(seed, version)` recreates a saved world.
`biome_at`, `column_height` and `surface_block_at` provide the shared column
queries; stable biome names live in `crates/world/src/biome.rs`.

Tests cover the old generator's fixed output, deterministic biome sampling,
terrain transitions, vegetation boundaries, LOD surface agreement and save
version migration. Network protocol ID 5 includes the extended block/item
catalog; multiplayer clients and servers must use the same build.

## Visual checks

The fixture command writes disposable worlds under `target/biomes-qa`, with
seed 2026 and a camera near each biome. For example:

```sh
cargo test -p tsumiki-server write_biome_verification_worlds -- --ignored --nocapture
cargo run -- --world target/biomes-qa/tundra --screenshot target/biomes-qa/tundra.png
cargo run -- --world target/biomes-qa/forest --screenshot target/biomes-qa/forest.png
```

The full-world screenshot waits for nearby chunks, lighting and distant LOD.
Use it when checking terrain continuity across the horizon.

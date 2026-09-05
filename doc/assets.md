# Tsumiki — Asset Pipeline

## 1. Art direction

- **Format**: 16×16 block textures and authored item art; inventory icons
  occupy 32×32 cells so block faces remain readable in isometric projection.
- **Tone**: pop and toy-like ("tsumiki" = toy building blocks). Deliberately
  non-realistic:
  - Bright values, moderately high saturation, warm bias.
  - No pure black and no pure white: the darkest color is a warm dark
    navy/brown, the lightest is a slightly warm off-white. This alone pushes
    the look away from realism and toward toys.
  - Short color ramps (3–4 shades per hue) with hue-shifted shadows (shadows
    lean toward blue/purple, highlights toward yellow), not plain darkening —
    this is the standard trick that makes pixel art read as "pop".
  - Rounded silhouettes and thick borders where applicable (machine faces,
    icons); avoid noisy high-frequency detail.
- **Coherence** comes from a single shared palette: every texture, whatever
  its source, is quantized to it as the final pipeline step.

## 1.1 Font

All UI text uses **Misaki Gothic** (美咲ゴシック, `assets/fonts/misaki_gothic.ttf`)
— an 8×8 Japanese bitmap font by Num Kadoma (https://littlelimit.net/misaki.htm),
which matches the pixel-art direction and covers Japanese. Free license
(unlimited use/copy/distribution, commercial included, no warranty); the
original license text ships alongside the font as
`assets/fonts/LICENSE-misaki.txt`. Render at multiples of 8 px so the bitmap
grid stays crisp.

## 2. Sources and generation

The shared 32-color palette is `assets-src/palette.json`. Every opaque
texture/icon pixel belongs to this palette. `blocks.toml` composes seeded
procedural layers, palette swaps, and overlays into natural materials,
woodwork, ores, and machine faces. `items.toml` maps placeable items to their
block textures and defines silhouettes for materials and the three tool tiers.
Placeable cube icons project the block's top, side, and front textures onto
three contiguous faces at a 30-degree isometric angle, with a bright top and
shaded sides. They have no surrounding outline. Authored item art is enlarged
with nearest sampling; the torch uses a narrow silhouette matching its world
geometry.

Run from the repository root with Python 3.12+ and uv installed:

```sh
uv sync --project assets-src --locked
uv run --project assets-src python assets-src/generate.py
uv run --project assets-src python assets-src/generate.py --check
uv run --project assets-src pytest assets-src/tests
uv run --project assets-src ruff check assets-src
```

The locked uv environment contains Pillow and the validation tools. The game
reads committed output, so running or building the Rust workspace does not
require Python. Fixed seeds, explicit palette indices, and stable packing
make regeneration deterministic. `--check` fails if any output differs from
the committed files; it does not write them.

The generator verifies the edges that each recipe declares as tileable.
Most faces repeat on both axes. Grass sides repeat horizontally: their top
is turf and their bottom is soil. Quantization and tiling checks happen
before packing. Tests also cover catalog coverage, transparency, generated
bytes, and stale-output detection. Rust tests compare asset IDs, names,
face order, and placement mappings with the gameplay registries.
CI runs regeneration checks, pytest, and Ruff alongside the Rust checks.
Generated JSON keeps LF endings on every platform so Git checkout cannot
invalidate the byte comparison.

## 3. Output and rendering

| File | Contents |
| --- | --- |
| `assets/atlas.png` | 128×240 block atlas, 8 columns of 16×16 tiles |
| `assets/atlas.json` | Block IDs, names, six face rectangles, tiling axes |
| `assets/icons.png` | 256×128 item atlas, 8 columns of 32×32 cells; cell zero is transparent |
| `assets/icons.json` | Item IDs, names, placement mappings, icon rectangles |
| `assets/lod_colors.json` | Per-block top, side, and bottom representative colors |

Block tile indices are `block_id * 6 + face`, where face order is
`[-X, +X, -Y, +Y, -Z, +Z]`. Machine fronts use `-Z`. Icon cell indices equal
item IDs. The registry and rendering tests enforce these packing contracts.

Near terrain fetches exact texels from the atlas and repeats the pattern
once per world block, including on merged greedy quads and across negative
chunk coordinates. Adjacent atlas tiles cannot bleed into each other.
Vertex attributes carry the tile choice and propagated RGB/skylight
separately. The same lighting multiplies the sampled texture; torches keep
their narrow wood shaft and glowing head.

LOD meshes use colors averaged from the textures in linear light, then
encoded as sRGB. Those generated values are embedded in the block registry
at Rust compile time and share the near terrain's day/night material.

Inventory, hotbar, container, furnace, cursor, and recipe icons use the item
atlas with nearest sampling. Normal slot icons render at 32×32 pixels.
Stack counts, durability bars, and recipe affordability remain separate UI
layers. Dropped items use the same icons on rotating, double-sided cards;
voxel lighting tints them as it does the world.

## 4. Visual verification

```sh
cargo test -p tsumiki-server write_texture_verification_world -- --ignored --nocapture
cargo run -- --world target/texture-qa/gallery --cave-screenshot target/texture-qa/gallery.png
cargo run -- --ephemeral --seed 2026 --inventory-screenshot target/texture-qa/inventory.png
cargo run -- --ephemeral --seed 2026 --screenshot target/texture-qa/horizon.png
```

The gallery contains every block, a large merged stone floor, and sample
dropped items in daylight. The inventory capture includes every item and
partially worn tools. Screenshot capture waits for the texture assets,
nearby terrain, and light to load. See [lighting.md](lighting.md) for paired
lit/dark cave captures.

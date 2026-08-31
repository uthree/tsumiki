# Tsumiki — Asset Pipeline

## 1. Art direction

- **Format**: 16×16 pixel art for all block/item textures.
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

## 2. Shared palette

- One global palette, target size **32 colors**, committed as
  `assets-src/palette.json` (single source of truth).
- Structure: ~8 hue ramps × 3–4 shades:
  - warm gray (stone/rock), brown (wood/dirt), green (foliage), blue
    (water/sky accents), red, yellow/orange, cool gray (metal/machines),
    plus a small neutral/accent set.
- Changing the palette and regenerating rebuilds every asset consistently.

## 3. Pipeline overview

The generator script is the **only** build path for shipped textures. Hand
drawing and AI generation are just ways to produce *inputs* to it.

```
assets-src/
├── palette.json          # global palette (source of truth)
├── blocks.toml           # declarative texture recipes, one entry per block
├── layers/               # input tiles: procedural cache, curated AI output,
│                         #   hand-drawn pieces (all treated identically)
└── generate.py           # the generator (Python + Pillow, run under uv)

assets/                   # generated output, consumed by the game build
├── atlas.png             # packed texture atlas
├── atlas.json            # UV mapping: block id → atlas rect
└── lod_colors.json       # per-block representative color (for far LOD, see design.md §3)
```

Pipeline steps, per block texture:

1. **Compose** from three primitives, declared in `blocks.toml`:
   - `noise`: parameterized tileable noise base (stone, dirt, sand are
     parameter variations of the same primitive);
   - `swap`: palette swap of an existing layer (ore variants, tier recolors);
   - `overlay`: alpha-composite layers (ore specks on rock, machine face on
     casing).
2. **Quantize** to `palette.json` (nearest color, no dithering by default —
   dithering reads as noisy at 16×16).
3. **Verify tiling** (left/right and top/bottom edge continuity) for
   world-facing textures; fail the build on violation.
4. **Pack** into the atlas and emit `atlas.json`.
5. **Extract** the average color per texture into `lod_colors.json`.

The build is deterministic: fixed RNG seeds per recipe, so regeneration is
reproducible and diffs are meaningful.

### 3.1 Item icons

Items need icons as well as blocks (roadmap M5). Two kinds:

- **Placeable items** (stone, planks, chest): the icon is *derived*, not
  authored — render the block's own textures as a small isometric cube.
  One generator rule covers the whole placeable catalog, so adding a block
  never means drawing an icon.
- **Non-placeable items** (sticks now; ingots and tools in M6): authored as
  ordinary 16×16 recipes in `items.toml`, same primitives as blocks.

Until the atlas exists the client draws every item as a flat colored square
from `ItemDef::color`, so icons can land independently of gameplay work.

## 4. Input sources

- **Procedural (default)**: most of the catalog — natural materials as noise
  variants, ores as base + swap + overlay, machines as casing + face overlay.
- **AI-assisted (optional)**: ComfyUI may be used to produce base material
  tiles or concepts. Output must be downscaled, made tileable, and quantized,
  then curated by hand into `layers/`. It is never a direct-to-game path.
- **Hand-drawn (exceptions)**: assets that are exceptionally complex or
  identity-defining (logo, UI icons, distinctive machine faces) are drawn by
  hand (e.g. Aseprite), saved into `layers/`, and still pass through
  quantization and packing like everything else.

## 5. Tooling

- Python + Pillow, managed with `uv` (`uv run generate.py`).
- The generator is a build-time tool; the game only ever reads `assets/`.
- `assets/` is committed (small, and keeps the game buildable without
  Python); regenerating it must be a no-op unless sources changed.

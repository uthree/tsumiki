# Caves and voxel lighting

Caves use seeded 3D noise, with occasional entrances on dry land. They are
carved before ore placement, so exploration exposes the same coal and iron
veins used by the tool progression. The bottom three layers and roofs under
ocean beds stay intact. Seed `2026` has a walkable entrance near `(-2, 42, -7)`.

One coal and one stick craft four torches by hand. Place them like other
blocks; a torch emits warm RGB light and can be recovered by mining it. Its
narrow shape does not block movement. Creative inventories include torches
among the placeable blocks in the backpack.

Creative inventories also include red, green, and blue demo lamps. See
[controls.md](controls.md#rgb-lighting-demo) for use and retirement instructions.

## Light data and updates

Each light sample packs four 4-bit channels into a `u16`: red, green, blue,
then sky. `LightChunk` compresses those values with a palette and runs.
Lighting is derived from blocks, so world saves keep their existing format;
loading a world rebuilds its light from the saved blocks and generated terrain.

The server solves a full-height chunk column with a 15-block horizontal halo.
Direct sky travels down clear columns without loss. A single flood combines
RGB and sky, losing at least one level on each subsequent step. Opaque blocks
stop light; water attenuates it. The halo covers the maximum horizontal reach,
so neighboring chunks agree without depending on their loading order.

Two background workers process bounded queues. Editing a block invalidates
affected columns, cancels obsolete snapshots, and rebuilds from sources. This
handles both removing a torch and opening or closing a skylight. Updated light
chunks are sent to subscribed clients; distant derived columns are evicted.

The near mesher samples light on the exposed side of each face and includes
all four channels in its merge key. A shared material multiplies the block
texture by the light. Sky tint and brightness change with the day/night uniform,
without recalculating propagation or rebuilding terrain meshes. Distant LOD
uses the same material with full sky exposure.

## Reproducing visual checks

Run these commands from the repository root:

```sh
cargo test -p tsumiki-server write_lighting_verification_worlds -- --ignored --nocapture
cargo run -- --world target/m7-qa/dark --cave-screenshot target/m7-qa/dark.png
cargo run -- --world target/m7-qa/lit --cave-screenshot target/m7-qa/lit.png
cargo run -- --ephemeral --seed 2026 --screenshot target/m7-qa/horizon.png
```

The first command writes disposable worlds under `target/m7-qa`. Both contain
the same enclosed chamber across a chunk boundary and use the same saved
camera. Only one contains a torch. The subsequent commands exercise world
loading, server light generation, streaming, meshing, and the actual shader.
`--cave-screenshot` preserves the saved camera instead of moving it above the
terrain. Screenshot runs print frame-rate and readiness information.

Automated tests cover cave accessibility, ore exposure, water isolation,
light-source mixing and removal, shadow updates, chunk boundaries, compressed
light transport, and torch crafting, placement, persistence, and recovery.

## M7 lighting baseline on 2026-09-06

Windows 11, Ryzen 7 9800X3D, Radeon RX 9070 XT (Vulkan), 2560×1440, optimized
development build before textures, default view distance of 12 chunks with five LOD levels:
the settled seed-2026 horizon capture measured **59.7 fps**. All 1,764 received
block chunks had lighting; 1,508 interior chunks and 2,226 LOD chunks had been
meshed. Outer full-resolution chunks await neighbors outside the requested
radius, as in the existing meshing policy. This is a screenshot benchmark on
this hardware, not a minimum-spec guarantee.

The representative seed-42 mesher probe measured 1,404→1,682 quads for a
surface chunk and 1,314→1,723 for a cave chunk when adding light to the merge
key (about 20% and 31%). Mean CPU remesh times over 100 iterations were
1.60 ms and 1.75 ms. Repeat it with:

```sh
cargo test -p tsumiki-client lighting_mesh_probe -- --ignored --nocapture
```

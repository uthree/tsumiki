# Tsumiki — Roadmap

Milestone-based; each milestone ends in something playable and verified.
Ordering rationale: editing + persistence come first because everything else
assumes them; real networking comes early because multiplayer is a core
pillar and retrofitting sync onto grown features is what kills it; the two
big unique pillars (LOD, factory) come next; free-rigid-body contraptions
last because they are the hardest and benefit from a stable world.

The asset pipeline (doc/assets.md) is an independent parallel track — it can
land whenever, since block visuals are swappable by design.

## M0 — Rendering prototype ✅ (2026-08-31)

Workspace split (protocol/world/server/client), in-process transport,
palette-compressed chunks, deterministic worldgen, greedy meshing, fly
camera, screenshot verification.

## M1 — "Walk, dig, build, and it stays"

- Player collision + walking/jumping (voxel AABB physics; fly mode kept as a
  toggle). First-person block highlight (raycast).
- Server-authoritative block edit protocol: break/place messages, validation,
  re-mesh on change (including neighbor chunks at borders).
- World persistence: chunks + player state saved server-side to disk
  (format: the existing palette serialization; per-region files).
- Hotbar with the prototype block set.

Done when: two sessions in a row can build on the same terrain, and edits at
chunk borders re-mesh seamlessly.

## M2 — Real multiplayer

- renet (UDP) transport implementing the existing `ServerTransport` /
  `ClientTransport` traits; dedicated-server binary flag.
- Player entities: join/leave, name tags, position replication with
  interpolation. Edits broadcast to all clients.
- Basic interest management: only replicate what is within view distance.

Done when: two machines on a LAN can see each other move and build, and the
in-process singleplayer path still works unchanged.

## M3 — Distant Horizons (LOD)

- Server-side LOD pyramid (2× per level) generated per chunk column, cached,
  streamed by distance band (design.md §3).
- Client LOD meshing (flat per-block colors from the build-time color table)
  and level transitions; boundary stitching/skirts between LOD levels.
- Decoration pass for cross-chunk features (trees) so the treeless border
  bands from M0 disappear at full resolution.

Done when: view distance reaches the horizon (≥ 1000 blocks) at 60 fps with
bounded memory, with no visible cracks between LOD rings.

## M4 — Items and the factory graph

- Item catalog v1 (hard cap ~40; see design.md §0) with inventory UI.
- Factory graph runtime (design.md §4): rate-based machine nodes, transport
  edges, event-driven lazy evaluation, running independent of chunk load
  state ("factories run while you sleep").
- First machine set: miner, belt, furnace/press, storage. Belt items as
  client-side cosmetics derived from flow rates.
- Power as aggregate supply/demand (one generator type).

Done when: a player can leave a mining+smelting line, disconnect, return
later, and find the correct amount produced — computed, not ticked.

## M5 — Contraptions

- Assembly/disassembly: grid ⇄ contraption entity with merged-box colliders
  (design.md §5), kinematically driven first.
- avian3d integration: Free mode (server-authoritative rigid bodies,
  snapshot+interpolation), then Jointed mode (bearings, doors, pistons with
  scalar-state sync), then Path mode (spline rails, trains).
- Chunk-loader behavior for moving contraptions.

Done when: a player-built vehicle drives over terrain in multiplayer without
desync, and a bearing-driven door syncs via its single angle.

## Parallel track — Asset pipeline

- Palette + generator script per doc/assets.md; texture atlas replaces
  vertex-color rendering; LOD color table derived from textures.

## Deliberately later

Biomes/caves, mobs, sound, translucent water, nested contraptions,
client-side prediction for piloted vehicles.

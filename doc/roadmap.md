# Tsumiki — Roadmap

Milestone-based; each milestone ends in something playable and verified.
Ordering rationale: editing + persistence come first because everything else
assumes them; real networking comes early because multiplayer is a core
pillar and retrofitting sync onto grown features is what kills it; LOD comes
next as the first of the unique pillars. The game then has to stand up as a
survival game (M5–M8) before automation arrives, because a factory is only
interesting if it automates manual work the player already values — and
because hand-crafting recipes are literally the data the factory graph runs
on. Free-rigid-body contraptions come after the factory: they are the
hardest part and benefit from a stable world.

The asset pipeline (doc/assets.md) is an independent parallel track — it can
land whenever, since block visuals are swappable by design.

## M0 — Rendering prototype ✅ (2026-08-31)

Workspace split (protocol/world/server/client), in-process transport,
palette-compressed chunks, deterministic worldgen, greedy meshing, fly
camera, screenshot verification.

## M1 — "Walk, dig, build, and it stays" ✅ (2026-08-31)

- Player collision + walking/jumping (voxel AABB physics; fly mode kept as a
  toggle). First-person block highlight (raycast).
- Server-authoritative block edit protocol: break/place messages, validation,
  re-mesh on change (including neighbor chunks at borders).
- World persistence: chunks + player state saved server-side to disk
  (format: the existing palette serialization; per-region files).
- Hotbar with the prototype block set.

Done when: two sessions in a row can build on the same terrain, and edits at
chunk borders re-mesh seamlessly.

## M2 — Real multiplayer (loopback-verified 2026-08-31; LAN check pending)

- renet (UDP) transport implementing the existing `ServerTransport` /
  `ClientTransport` traits; dedicated-server binary flag.
- Player entities: join/leave, name tags, position replication with
  interpolation. Edits broadcast to all clients.
- Basic interest management: only replicate what is within view distance.

Done when: two machines on a LAN can see each other move and build, and the
in-process singleplayer path still works unchanged.

## M3 — Distant Horizons (LOD) ✅ (2026-08-31)

- Server-side LOD pyramid (2× per level) generated per chunk column, cached,
  streamed by distance band (design.md §3).
- Client LOD meshing (flat per-block colors from the build-time color table)
  and level transitions; boundary stitching/skirts between LOD levels.
- Decoration pass for cross-chunk features (trees) so the treeless border
  bands from M0 disappear at full resolution.

Done when: view distance reaches the horizon (≥ 1000 blocks) at 60 fps with
bounded memory, with no visible cracks between LOD rings.

## M4 — Survival core ✅ (2026-08-31)

Rationale: the factory loop needs an economy first. Today blocks cost
nothing and yield nothing (de-facto creative mode), so production would be
meaningless. This milestone makes the world consequential without turning
the game into a combat-survival title (fighting is deliberately NOT part of
this milestone; see M7).

- Inventory and scarcity: mining drops the block as an item, placing
  consumes one; the hotbar becomes a real inventory view. Dropped-item
  entities (also needed for death drops).
- Server-authoritative validation of edits: reach distance and inventory
  ownership checked server-side (closes the M1 trust gap).
- Per-block breaking time (hold to mine; no tool tiers yet — tools are a
  future factory product, not a starting feature).
- Health and respawn: fall and drowning damage; on death the inventory
  drops in place (Minecraft-style) and the player respawns at spawn.
- Swimming (water is currently a walk-through void).
- Day/night cycle (atmosphere; motivates light sources later).
- Game mode is a server/world setting: `survival` (default) or `creative`
  (free blocks, no health — building showcases and testing).

Done when: a player must mine to build, can die and recover their dropped
items, and a creative-mode world still allows free building.

## M5 — Items and crafting

Rationale for coming before the factory (decided 2026-08-31): hand-crafting
recipes *are* the data the factory graph automates, so the recipe registry
has to exist first — otherwise hand-crafting gets retrofitted onto a format
designed for machines. And the `ItemId`/`BlockId` split is cheapest now,
before factory code is written against today's "an item is a block"
assumption. Automation is only interesting once there is manual work worth
automating.

- `ItemId` / `ItemStack` separate from `BlockId`; item registry; block→drop
  and item→placeable-block mappings. Stack sizes.
- Real inventory: 36 slots (27 + 9 hotbar), server-authoritative slot
  operations (move/split/swap/cursor stack), inventory screen with
  drag-and-drop.
- Recipe registry: recipes chosen from a list, not arranged in a grid; a
  crafting table unlocks the recipes that need a station. The recipe type is
  declarative input→output, so M9's machine nodes consume the same data.
- Containers: chest with a shared server-side inventory and a container UI
  (the generic "open a container" protocol, reused by furnace and machines).
- Dropped items generalize from block-only to `ItemStack`; throwing items.

Done when: a player can chop wood, craft planks → crafting table → chest
from the recipe list, store items in the chest, and find everything intact
after a restart — with a second player seeing the same chest contents.

## Interlude — world management ✅ (2026-09-01)

Not a milestone; slotted in after M5 because game mode had become a real
choice and there was no way to make it except a CLI flag.

- Title menu gains a Minecraft-style world list (name, mode, last played)
  with play, delete-with-confirmation, and a create form (name, seed,
  game mode).
- Named worlds live under `worlds/<name>/`; the old single `world/`
  directory migrates itself on first run.
- The client still never touches the filesystem: the launcher injects
  list/create/delete/start hooks, as it already did for transports
  (design.md §1).

## M6 — Tools and smelting

- Ore veins (coal, iron) with depth-dependent frequency; stone drops
  cobblestone.
- Tools: wood/stone/iron × pickaxe/axe/shovel, with break-speed multipliers,
  harvest levels (the right tool tier gates the drop), and durability.
- Furnace: fuel + input → output over time. Its recipe format is the one
  M9's factory nodes reuse; the furnace is deliberately the bridge block.

Done when: the hand → wood pickaxe → stone → furnace → iron progression
works end to end, and mining the wrong tier yields nothing.

## M7 — Caves and light

- Cave carving (3D noise) so ore hunting means going underground.
- Light engine, **RGB from the start** (decided 2026-08-31): 4 bits per
  channel, RGB 12 bits + sky light 4 bits = `u16` per block, palette/RLE
  compressed like block data. One BFS propagation carrying `[u8; 3]`,
  re-enqueueing when any channel improves.
- Sky light is a separate channel multiplied by the day/night sun color at
  render time, so sunsets tint the world for free (reuses M4's
  `lighting_for_time`).
- Torch as the first light source; darkness makes caves consequential.
- Watch item: light values enter the greedy mesher's merge key and will
  fragment quads. Measure; if it hurts, move to interpolated per-face light.

Done when: a torch-lit cave is explorable, an unlit one is genuinely dark,
and far-view performance still holds at 60 fps.

## M8 — Food and farming

- Hunger gauge: depletes with activity, gates regeneration, damages at zero.
- Farmland, wheat, seeds; bread via crafting, cooked food via the furnace.

Done when: a player can sustain themselves indefinitely from a farm they
built, and starving is a real (but slow) failure state.

## M9 — The factory graph

- Factory graph runtime (design.md §4): rate-based machine nodes, transport
  edges, event-driven lazy evaluation, running independent of chunk load
  state ("factories run while you sleep").
- First machine set: miner, belt, powered furnace, storage — automating the
  M6/M8 recipes players already know by hand. Belt items as client-side
  cosmetics derived from flow rates.
- Power as aggregate supply/demand (one generator type).

Done when: a player can leave a mining+smelting line, disconnect, return
later, and find the correct amount produced — computed, not ticked.

## M10 — Contraptions

- Assembly/disassembly: grid ⇄ contraption entity with merged-box colliders
  (design.md §5), kinematically driven first.
- avian3d integration: Free mode (server-authoritative rigid bodies,
  snapshot+interpolation), then Jointed mode (bearings, doors, pistons with
  scalar-state sync), then Path mode (spline rails, trains).
- Chunk-loader behavior for moving contraptions.

Done when: a player-built vehicle drives over terrain in multiplayer without
desync, and a bearing-driven door syncs via its single angle.

## M11 — Hostile mobs (unscheduled)

Combat is on the roadmap but intentionally last: the game's tension comes
from logistics and scale, not fighting. Scope when it arrives: a small
number of enemy types (respecting the small-catalog philosophy), threats
that pressure factories/logistics rather than twitch combat. To be designed
after the factory loop proves itself.

## Parallel track — Asset pipeline

- Palette + generator script per doc/assets.md; texture atlas replaces
  vertex-color rendering; LOD color table derived from textures.

## Deliberately later

Biomes, sound, translucent water (also fixes the LOD water seam lines),
nested contraptions, client-side prediction for piloted vehicles.

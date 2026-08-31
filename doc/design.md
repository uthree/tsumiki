# Tsumiki — Design Document

A Minecraft-like 3D sandbox game built with Bevy, centered on three pillars:

1. **Long view distance** via server-generated LOD (Distant Horizons-style).
2. **Factory automation** with incremental-game pacing (factories keep producing
   even while their chunks are unloaded).
3. **Player-built vehicles** ("contraptions") driven by free rigid-body physics
   (Valkyrien Skies-style), with joint- and path-constrained modes on top.

Multiplayer is a core feature, not an add-on. Everything below assumes a
server-authoritative architecture at all times.

Design discipline — one rule, no quota:

> **No parallel material lines.** A new tier means "the same thing, faster or
> in parallel", never a new material with its own chain of intermediates.

Depth comes from combination and throughput scaling, not variety. There is
deliberately no numeric cap on the catalog: what confuses players is a
thousand types with branching chains, not the count itself, and a target
number just talks you out of additions the game actually needs. Add what the
rule allows; revisit if the catalog ever approaches the hundreds.

---

## 1. Architecture

### 1.1 Integrated-server model

Even in singleplayer, the game runs a local server that the client connects to
(as Minecraft does). The client/server boundary is therefore exercised in every
play session, which keeps the decoupling honest.

Cargo workspace layout:

```
tsumiki/
├── crates/
│   ├── protocol/   # message enums + serialization; depended on by both sides
│   ├── world/      # block/item definitions, chunk data structures, worldgen
│   ├── server/     # world simulation; headless Bevy (MinimalPlugins), zero render deps
│   └── client/     # rendering, input, UI (full Bevy)
└── src/main.rs     # launcher binary (singleplayer spawns both; dedicated server spawns one)
```

### 1.2 Transport abstraction

`protocol` defines all messages. The transport is a trait with two
implementations:

- **In-process channel** for singleplayer (no sockets, no serialization cost on
  the hot path if avoidable).
- **renet (UDP)** for multiplayer.

Game logic never knows which transport is in use.

### 1.3 Networking strategy

Two layers, because voxel games have two very different kinds of traffic:

- **Entity state** (players, dropped items, contraptions): renet channels with
  snapshot + interpolation. Standard reliable/unreliable channel split.
- **Chunk streaming**: custom messages (this is the bulk of the bandwidth and
  is out of scope for generic replication libraries). Full-resolution chunks
  near the player, LOD chunks far away (see §3).

### 1.4 Tick model

- Server: fixed tick (20–30 TPS) for world/entity simulation and physics.
- Client: renders at display rate, interpolates entity state between snapshots.
- The factory simulation does NOT run on the tick — it is event-driven (see §4).

---

## 2. World Data

- Chunk size: **32³** blocks.
- Block ID: `u16`. Block state kept minimal.
- **Palette compression** per chunk (Minecraft 1.13+ style): a per-chunk
  palette of occurring block types + bit-packed indices. Used uniformly for
  memory, disk, and network representation.

---

## 3. LOD (Distant Horizons-style far rendering)

Key decision: **LOD data is generated server-side and streamed**, so clients
can render to the horizon without ever holding full-resolution far chunks.

- LOD0 = full-resolution chunk (player vicinity only).
- LOD(n) = 2³ blocks of LOD(n-1) collapsed into one block (most-frequent-block
  wins), forming a pyramid.
- Distant clients receive only the LOD level appropriate for their distance;
  network cost and client meshing cost fall off cubically with distance.
- Client meshes every LOD level with greedy meshing. Far LODs render as flat
  color (per-block representative color), near chunks render textured.
- Representative colors are extracted automatically from block textures at
  build time — no hand-authored LOD assets.
- The LOD pyramid doubles as map data for a future map feature.

---

## 4. Factory Simulation (incremental automation)

### 4.1 The factory graph is the single source of truth

Factories are NOT simulated by ticking blocks in loaded chunks. Instead:

- Placing a machine block registers a node in a server-side **factory graph**.
  - Node = machine: a recipe (declarative input→output rates) + input/output
    buffers.
  - Edge = transport link (belt/pipe): a throughput cap.
- The graph simulation runs continuously on the server, **completely
  independent of chunk load state**. This dissolves the "offline progress"
  problem: there is no separate abstract simulation to keep consistent,
  because the abstract simulation is the only one.
- Blocks in the world are merely the UI for editing and observing the graph.
  Items visibly moving on belts are a client-side cosmetic derived from flow
  rates, not authoritative entities (as in Satisfactory).

### 4.2 Event-driven lazy evaluation

Running "continuously" is nearly free because nothing is computed between
events:

1. Each buffer stores `(amount at time t₀, current rate)`. Current amount is
   computed on demand as `amount + rate × elapsed`.
2. Rates only change at **events**: a buffer fills, a buffer empties, a recipe
   toggles. The next event time for each node is a linear-equation prediction,
   kept in a priority queue.
3. When an event fires, the affected node's rate is updated and the change
   propagates only along graph edges to dependent nodes, whose predicted event
   times are recomputed. No world scan, no per-tick cost for steady-state
   factories of any size.

### 4.3 The rate rule

Everything on the graph must obey one discipline:

> **Every machine's behavior must be expressible as declarative rates**
> (e.g. "2 iron ore + 1 fuel → 1 iron ingot per second").

No probabilistic drops, no special neighbor effects, no complex internal state.
Upgrades are rate multipliers. Power is handled in the same framework:
aggregate supply vs. demand rates; deficit slows the whole network
proportionally.

This constraint dovetails with the small-item-catalog policy.

### 4.4 What stays off the graph

Dropped items, mobs (if any), and any future logic circuits run only in loaded
chunks, as usual. The player-facing rule is simple and honest: *factories run
while you sleep; everything else pauses.*

---

## 5. Contraptions (vehicles and machines)

### 5.1 Core data structure

A contraption is: **a set of blocks (with a local grid) + a constraint mode**.

Assembly detaches a connected group of blocks from the world grid into a
contraption entity; disassembly merges it back. Assembly/disassembly, collider
generation, and rendering are shared across all modes — only the physics
attachment and the synchronized state differ.

Physics engine: **avian3d** (Bevy-native), running server-side.
Server-authoritative; clients interpolate.

Colliders (both for contraptions and for terrain chunks) are built by greedily
merging blocks into boxes — never one collider per block. Terrain collider
quality is not negotiable, since vehicles roll and slide across it.

### 5.2 Constraint modes

1. **Free** (6 DOF): ships, cars. Full rigid-body simulation. Synchronized via
   transform snapshots + interpolation.
2. **Jointed** (1–2 DOF): player-built gates, drawbridges, bearings, pistons —
   via avian3d's revolute/prismatic/fixed joints, with limits and motors.
   Because only the joint coordinate is free, synchronization is a single
   scalar (angle or stroke): precise and cheap.
3. **Path-constrained** (1 DOF): trains. Rails are extracted from placed track
   blocks into a spline; the train's state is (distance along spline `s`,
   speed `ṡ`). The body is kinematic in the physics world — positioned from
   the spline each tick — so it still pushes dynamic bodies (players get run
   over, cargo rides on top). Sync is just `(s, ṡ)`. This is the Create-mod
   approach.
   - Derailment can later be expressed as a discrete transition to Free mode
     on excessive impact. A custom spline constraint (XPBD) is a possible
     future upgrade if physically simulated derailment is wanted.

Mode transitions are first-class: bearing detached → Jointed becomes Free;
derail → Path becomes Free.

### 5.3 Deliberate limits (v1)

- **No nested contraptions** (no minecart on a ship). Constraint parents are
  world-fixed only, but the data model reserves a parent-body ID so nesting
  can be added later without a redesign.
- Simple stock doors are plain blocks with block state + client-side
  animation, NOT contraptions. Physics doors are reserved for things players
  actually build.
- Piloting: the client sends inputs; the server simulates; the pilot sees
  interpolated results. Client-side prediction of vehicles is explicitly
  deferred.
- Moving contraptions act as chunk loaders (they keep the chunks around them
  loaded); this is designed in from the start.

### 5.4 Implementation order (risk hedge)

Build assembly/disassembly (grid ⇄ rigid body conversion) first, drive it
kinematically without gravity, then swap in full avian3d dynamics. The
architecture assumes free rigid bodies from day one; this ordering just keeps
a retreat path open.

---

## 6. Rendering

- Texture atlas (or array texture) for 16×16 block textures.
- Greedy meshing for all LOD levels.
- Near: textured; far: per-block flat color from the build-time color table
  (§3).

### 6.1 Lighting (RGB from the start)

Voxel light is stored per block as `u16`: **4 bits per RGB channel** (block
light) plus 4 bits of sky light. Colored light sources are therefore possible
from day one instead of being a retrofit of a scalar light level — the
expensive part of that decision is the storage and propagation, and paying it
up front costs far less than migrating later.

- Propagation is a single BFS carrying `[u8; 3]`, re-enqueueing a neighbour
  when *any* channel improves. This is cheaper than three independent floods
  and keeps light-source colors mixing correctly.
- Sky light is a separate channel, multiplied at render time by the day/night
  sun color. Sunsets therefore tint the whole world without any extra data.
- Light data is palette/RLE compressed per chunk like block data (caves and
  open sky are both large regions of one value).
- Rendering multiplies vertex color by light color. Because the renderer is
  already vertex-color based, this is nearly free.
- Known cost: light values join the greedy mesher's merge key and fragment
  quads. Measure before optimizing; the fallback is interpolated per-face
  light instead of per-block.

---

## 7. Items, inventory and recipes

Items are **not** blocks. `BlockId` identifies something that occupies a cell
in the world; `ItemId` identifies something that occupies an inventory slot.
The two are related by two explicit mappings — a block's drop, and an item's
placeable block — and plenty of items (sticks, ingots, tools) have neither.

- An inventory is a flat `Vec<Option<ItemStack>>`; a stack is `(ItemId, count)`
  plus optional per-stack state (durability). Slot layout is a UI concern.
- **The server owns every inventory.** The client sends slot operations
  (move/split/swap, craft) and renders what comes back. This is the same trust
  boundary as block edits (§1.1): the client never decides what it holds.
- Containers (chest, furnace, later machines) are inventories the server
  attaches to a block position. Opening one is a generic protocol exchange, so
  every future container reuses it.
- A recipe is declarative: a set of input stacks → one output stack, plus an
  optional **station** (a crafting table) that must be open to reach it.
  There is deliberately **no spatial pattern**; players pick from a recipe
  list. Memorising grid patterns is a tax every new player pays forever, and
  it buys only ritual — the station is what keeps a crafting table
  meaningful, by unlocking part of the list rather than being a place to
  arrange squares.
- That makes the recipe table the **same data the factory graph consumes**
  (§4.3): a machine node is a recipe plus a rate. A grid would have had to be
  discarded at that boundary anyway.

## 8. Assets

- Visual style: 16×16 pixel-art textures, pop / toy-like tone.
- The catalog grows slowly (see the design discipline above), and assets are
  script-generated, so asset count is not a bottleneck.
- Generation pipeline: script-generated from a shared palette, with AI or
  hand-drawn input for exceptional assets — see `doc/assets.md`.

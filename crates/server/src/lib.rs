//! Headless game server (design.md §1).
//!
//! Owns the authoritative world state and serves it to clients over a
//! [`ServerTransport`]. No rendering dependencies. Runs as a headless Bevy
//! app (`MinimalPlugins` + `ScheduleRunnerPlugin` at a fixed tick).
//!
//! Per-tick work:
//! 0. `transport.tick(dt)` at the very start, `transport.flush()` at the very
//!    end, so transports that need driving (UDP) get pumped every tick; the
//!    in-process transport's defaults are no-ops.
//! 1. Pump all pending transport messages:
//!    - `Hello` → reply `Welcome` (looking up any saved state for that name,
//!      including survival health/inventory) and, in survival, follow with
//!      `InventoryUpdate`/`HealthUpdate`; in creative, prefill the hotbar
//!      with every placeable item (doc/roadmap.md M5) and still send
//!      `InventoryUpdate` so the client actually sees it; every client also
//!      gets an `ItemSpawned` for each dropped item already in the world.
//!    - `RequestChunks` → enqueue positions into that client's own queue
//!      (deduplicated only against the queue itself, so a re-request for a
//!      chunk the client has forgotten and walked back to is served again;
//!      out-of-bounds Y is ignored; a single message is capped so it cannot
//!      dominate the queue or force an unbounded insert).
//!    - `BreakBlock` → names a hotbar slot, like `PlaceBlock` (roadmap M6;
//!      an out-of-range slot rejects the whole message). Validated
//!      server-side (reach, block solidity), then broadcasts `BlockChanged`,
//!      invalidates LOD, drops a broken chest's or furnace's contents into
//!      the world (any mode) and closes its UI for every viewer, and, in
//!      survival, gates `ItemRegistry::drop_of` behind `tool::can_harvest`
//!      for whatever tool sits in the named slot, wearing that tool by one
//!      use either way (overflow drops as an item entity).
//!    - `PlaceBlock` → names a hotbar slot, not a block (doc/roadmap.md M5):
//!      resolves the held item, requires it to place a block, validates as
//!      before, and in survival consumes one from that slot.
//!    - `SlotClick`/`DropSlot` → server-authoritative inventory/container
//!      slot operations (doc/roadmap.md M5); every change answers with a
//!      fresh `InventoryUpdate`, and container changes broadcast
//!      `ContainerUpdate` to every other viewer. A furnace's input/fuel
//!      slots additionally reject whatever doesn't belong there, and its
//!      output slot can only ever be taken from (roadmap M6, see `furnace`).
//!    - `Craft` → crafts a recipe by id from the recipe list rather than a
//!      grid (roadmap M5 revision): validated against `RecipeRegistry`
//!      (recipe exists, reachable from whatever station -- if any -- the
//!      player currently has open) and against the player's actual
//!      materials; overflow output drops as an item entity at the player,
//!      same as break-overflow. Answers with a fresh `InventoryUpdate`.
//!    - `OpenContainer`/`CloseContainer` → the generic container protocol
//!      (chest, crafting table, or furnace); closing (also on disconnect or
//!      death) drops the cursor stack into the world so it can never be
//!      parked in a closed UI.
//!    - `ReportDamage`/`Respawn` → survival-only health transitions; death
//!      drops the player's whole main inventory (plus cursor) as dropped
//!      items.
//!    - `UpdatePlayer` → record the client's latest state, relay it as
//!      `PlayerMoved` to observers currently seeing this client, persist it
//!      under that client's name, and auto-close any container UI the
//!      client has walked out of reach of.
//!    - `Goodbye` → save, drop the cursor leftover, broadcast `PlayerLeft`
//!      to this client's observers, and forget the client. Idempotent: a
//!      second `Goodbye` for an already-removed client is a no-op (a
//!      network transport can synthesize one on disconnect that duplicates
//!      an explicit one already received).
//! 2. Passive per-tick simulation (doc/roadmap.md M4): the day/night clock
//!    advances and periodically broadcasts `TimeUpdate`; survival health
//!    regenerates; dropped items expire or get picked up (all-or-nothing,
//!    doc/roadmap.md M5); every furnace burns fuel and cooks its input,
//!    regardless of whether anyone has it open, and whoever does have one
//!    open gets a throttled `FurnaceProgress` (roadmap M6, see `furnace`).
//!    Driven by the server's fixed tick interval rather than measured
//!    wall-clock time (see `SimRes`), so behavior is deterministic and
//!    testable, and so a furnace never credits time that elapsed while the
//!    server itself was stopped.
//! 3. If any client's state changed this tick, recompute interest
//!    (`recompute_interest`): every pair of clients with known state is
//!    checked against [`INTEREST_RADIUS`], sending `PlayerJoined`/
//!    `PlayerLeft` for pairs that crossed the threshold.
//! 4. Serve up to [`CHUNK_SEND_BUDGET`] queued requests, round-robin across
//!    clients so one client's backlog cannot starve another. One shared
//!    queue (and budget) covers both full-resolution chunk requests and LOD
//!    chunk requests (doc/design.md §3): generate/build the chunk if it is
//!    not already cached, cache it, and send `ChunkData`/`LodChunkData`.
//! 5. An accepted edit invalidates every LOD level's cache entry that covers
//!    the edited chunk (rebuilt lazily on next access) and, for any client
//!    that was already sent one of those LOD chunks, enqueues an unsolicited
//!    rebuilt re-send through the same budgeted queue.
//! 6. Bounded memory (doc/roadmap.md M3): pristine (unmodified) level-0
//!    chunks and LOD chunks are evicted least-recently-used once their caches
//!    exceed [`MAX_PRISTINE_CHUNKS`] / [`MAX_LOD_CACHE`]. Both regenerate
//!    deterministically from the seed (and, for LOD, from whatever level-0
//!    chunks are still cached), so eviction is invisible to correctness.

mod furnace;
mod harvest;
mod persist;
mod sim;
mod slots;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::prelude::*;

use bevy_math::{IVec3, UVec3, Vec3};
use tsumiki_protocol::{
    ClientId, ClientToServer, ContainerKind, GameMode, MAX_HP, PlayerSave, SERVER_REACH,
    ServerToClient, ServerTransport,
};
use tsumiki_world::lod::{self, MAX_LOD};
use tsumiki_world::{
    BlockId, BlockInteraction, BlockRegistry, Chunk, CraftingStation, HOTBAR_SIZE, Inventory,
    ItemStack, MAIN_INVENTORY_SIZE, WORLD_HEIGHT_BLOCKS, WORLD_HEIGHT_CHUNKS, WorldGenerator,
    blocks, split_block_pos,
};

use persist::{ItemRecord, Persistence, PlayerRecord};
pub use persist::{PeekedMeta, create_world_meta, peek_meta};
use slots::CraftingRes;

/// Maximum chunks generated + sent per tick, to keep tick times bounded.
/// Shared by full-resolution chunk requests and LOD chunk requests alike --
/// they are served from one unified round-robin queue (see module docs).
///
/// A client at the new maximum view distance (24 chunks) wants roughly 7,172
/// level-0 chunks (see [`MAX_PRISTINE_CHUNKS`]'s derivation) plus about 8,520
/// LOD chunks across levels 1..=`tsumiki_world::lod::MAX_LOD` (five bands of
/// ~1,420 chunks each -- see that module's docs for why every level costs
/// about the same count) -- roughly 15,700 total, a ~4.8x jump from the old
/// 4..=12-chunk range's worst case (~3,236). Doubled rather than scaled by
/// that full 4.8x: generating a chunk (a worldgen height sample per column,
/// or an LOD downsample pass) is not free, and this budget exists precisely
/// to keep that cost bounded per tick -- doubling it noticeably shortens a
/// max-view-distance join's fill time without risking the 30 Hz tick budget.
pub const CHUNK_SEND_BUDGET: usize = 64;

/// Two players are mutually visible for replication purposes when within
/// this many blocks of each other (doc/roadmap.md M2, "basic interest
/// management").
///
/// Matches the client's new maximum view distance (`VIEW_DISTANCE_RANGE`
/// ends at 24 chunks = 768 blocks, `crates/client/src/settings.rs`): interest
/// is meant to cover exactly "what is within view distance" (doc/roadmap.md
/// M2), so a player configured to the maximum must never be able to render a
/// peer's avatar without that peer already being replicated to them.
pub const INTEREST_RADIUS: f32 = 768.0;

/// Maximum positions accepted from a single `RequestChunks` or
/// `RequestLodChunks` message. Set with headroom above the client's own
/// per-frame cap (`MAX_CHUNK_REQUESTS_PER_FRAME = 64` in
/// `crates/client/src/net.rs`), so a legitimate client's burst always fits in
/// one message while a malformed or hostile message cannot force an
/// unbounded synchronous insert into the pending queues.
///
/// Unlike the caches below, this doesn't scale with view distance: it bounds
/// how many *newly-wanted* positions one message can carry (client requests
/// are already chunked into per-frame trickles capped well under this, both
/// on a fresh join and while walking), not the total number of chunks a
/// large view distance implies -- so the old headroom over the client's
/// per-frame cap stays valid unchanged.
const MAX_CHUNK_REQUESTS_PER_MESSAGE: usize = 128;

/// Cap on cached level-0 chunks that are *not* in the persistence `modified`
/// set (doc/roadmap.md M3, "bounded memory"). Modified chunks are the only
/// on-disk copy of a player's edits and are never evicted; pristine chunks
/// regenerate deterministically from the seed, so evicting them is invisible
/// to correctness -- only to how often they're regenerated.
///
/// Derived from the client's new view-distance range (`4..=24` chunks,
/// `VIEW_DISTANCE_RANGE` in `crates/client/src/settings.rs`): a single player
/// at the new maximum (24 chunks) has a level-0 request footprint of a
/// horizontal disk of that radius times [`tsumiki_world::WORLD_HEIGHT_CHUNKS`]
/// (4) -- 1,793 chunk columns in the disk (`dx*dx + dz*dz <= 24*24`) times 4
/// = 7,172 chunks. Below that floor, a lone max-distance player would evict
/// (and later re-generate) chunks still inside their own view every tick.
/// Sized to ~2.3x that floor -- 16,384 -- the same headroom the old cache
/// (4,096) gave its own max-view-distance total (1,764, at the old `4..=12`
/// range): enough for a couple of concurrent max-distance players, or one
/// player crossing a lot of terrain, without constant eviction churn.
///
/// Memory: a `Chunk` is a palette + bit-packed 32^3 index array (see
/// `tsumiki_world::chunk`); a typical multi-material terrain chunk (4-8
/// distinct blocks, 3-4 bits/index) is roughly 8-16 KiB, while a uniform
/// chunk (solid stone, or air) is tens of bytes. At 16,384 entries that is
/// very roughly 128-256 MiB in the worst realistic case -- real, but not
/// reckless for a dedicated game server, and most chunks in practice (deep
/// underground, high in the sky) are far cheaper than the worst case.
///
/// Overridden to a tiny value under `cfg(test)` so the eviction test doesn't
/// need to push thousands of chunks through the request/serve budget to
/// observe eviction.
#[cfg(not(test))]
pub const MAX_PRISTINE_CHUNKS: usize = 16384;
#[cfg(test)]
pub const MAX_PRISTINE_CHUNKS: usize = 8;

/// Cap on cached LOD chunks (all levels combined). Unlike level-0 chunks, LOD
/// chunks are never persisted at all -- design.md §3 derives them from
/// worldgen plus whatever level-0 chunks happen to be cached -- so every
/// entry is evictable.
///
/// Derived the same way as [`MAX_PRISTINE_CHUNKS`]: a single player at the
/// new maximum view distance (24 chunks) wants, across LOD levels
/// `1..=tsumiki_world::lod::MAX_LOD` (now 5, was 3), about 8,520 LOD chunks
/// total -- roughly 1,420 per level, since (per that module's docs) each
/// level's band doubles in both span and horizon, holding the count roughly
/// constant across levels; level 1 counts double that (738) for its extra
/// vertical layer. Sized to ~1.44x that floor -- 12,288 -- close to the old
/// cache's own ~1.39x headroom (2,048 over a 1,472 max-view-distance total at
/// the old `4..=12`/`MAX_LOD = 3` numbers). LOD chunks tend to be cheaper
/// than level-0 ones (fewer distinct materials after downsampling), so this
/// cache's memory footprint is in the same ballpark as or smaller than
/// [`MAX_PRISTINE_CHUNKS`]'s.
///
/// Also overridden under `cfg(test)`; see [`MAX_PRISTINE_CHUNKS`].
#[cfg(not(test))]
pub const MAX_LOD_CACHE: usize = 12288;
#[cfg(test)]
pub const MAX_LOD_CACHE: usize = 4;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub seed: u64,
    pub tick_hz: f64,
    /// Directory to persist chunks + player state in. `None` means an
    /// ephemeral server: nothing is read or written to disk.
    pub world_dir: Option<PathBuf>,
    /// How often (in seconds) dirty world state is flushed to disk.
    pub autosave_interval_secs: f64,
    /// The world's game mode (doc/roadmap.md M4). Only consulted for a
    /// brand-new world (no saved `meta.bin`): a loaded world's own persisted
    /// mode always wins, and a world saved before modes existed (format v1
    /// or v2) migrates to [`GameMode::Creative`] regardless of this setting
    /// (see `persist::decode_meta`). `None` (the default) means Survival for
    /// a brand-new world.
    pub game_mode: Option<GameMode>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            seed: 0,
            tick_hz: 30.0,
            world_dir: None,
            autosave_interval_secs: 10.0,
            game_mode: None,
        }
    }
}

/// Wraps the transport as a Bevy resource. Generic over the transport type so
/// both the in-process and (future) renet transports can drive the same
/// server systems.
#[derive(Resource)]
struct TransportRes<T: ServerTransport>(T);

#[derive(Resource)]
struct WorldGenRes(WorldGenerator);

/// The world seed actually in effect (from `meta.bin` if a saved world was
/// loaded, otherwise from `ServerConfig`). Stored separately because
/// `WorldGenerator` doesn't expose the seed it was built with.
#[derive(Resource)]
struct WorldSeed(u64);

#[derive(Resource)]
struct BlockRegistryRes(BlockRegistry);

/// Persisted player records keyed by name (M2: real multiplayer distinguishes
/// clients by identity, so each name gets its own slot; M4 extends the
/// record with health and inventory; M5 replaces the block-count inventory
/// with a real slotted item inventory). `Welcome.player` is looked up here by
/// the connecting client's `Hello` name. Kept continuously in sync with
/// every live change via [`sync_player_record`], the same write-through
/// pattern this map has used since M2.
#[derive(Resource, Default)]
struct PlayersRes(HashMap<String, PlayerRecord>);

/// Cached level-0 chunks plus, per chunk, the tick it was last touched
/// (served to a client, or edited) -- the LRU signal for pristine-chunk
/// eviction (see [`evict_pristine_chunks`]).
#[derive(Resource, Default)]
struct ChunkCache {
    chunks: HashMap<IVec3, Chunk>,
    last_access: HashMap<IVec3, u64>,
}

/// Cached LOD chunks (design.md §3), keyed by `(level, position)`, plus a
/// last-access tick per entry for LRU eviction. Built on demand in
/// [`get_or_build_lod_chunk`] and invalidated wholesale (removed, not
/// patched) whenever an underlying level-0 chunk changes; there is
/// deliberately no separate "dirty" bit since a cache miss already triggers
/// exactly the rebuild a dirty entry would.
#[derive(Resource, Default)]
struct LodCache {
    chunks: HashMap<(u8, IVec3), Chunk>,
    last_access: HashMap<(u8, IVec3), u64>,
}

/// Monotonic tick counter, used only as an LRU timestamp source for
/// [`ChunkCache`] / [`LodCache`] eviction.
#[derive(Resource, Default)]
struct ServerTick(u64);

/// Bundles every M4 survival-simulation resource into one, so `tick_server`
/// (already close to Bevy's 16-parameter function-system limit) spends only
/// one parameter on all of it.
#[derive(Resource)]
struct SimRes {
    /// Fixed for the whole session (doc/roadmap.md M4: "the world's rules...
    /// Fixed for the session", per `ServerToClient::Welcome`'s docs).
    game_mode: GameMode,
    world_time: sim::WorldTimeRes,
    items: sim::ItemsRes,
    /// Monotonic game-time clock (fixed-tick-driven, not wall-clock -- see
    /// its own docs), used for item pickup-delay/expiry timestamps.
    clock: sim::GameClock,
    /// Seconds per server tick, `1.0 / ServerConfig::tick_hz`. Drives every
    /// fixed-step simulation timer (day cycle, regen, item timers) instead
    /// of measured wall-clock `Time`, so tests can simulate arbitrary
    /// elapsed time deterministically just by controlling how many ticks
    /// run (or by mutating this value directly for a single large jump).
    tick_interval_secs: f64,
}

/// A single unqueued unit of work for the shared chunk/LOD-chunk send queue
/// (see module docs point 3): one full-resolution chunk, or one LOD chunk at
/// a given level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ChunkRequest {
    Level0(IVec3),
    Lod { level: u8, pos: IVec3 },
}

/// Per-client bookkeeping for replication, survival state, and inventory/
/// container UI state (roadmap M5).
struct ClientState {
    /// Name from `Hello`. Empty if a client somehow sent other messages
    /// before `Hello` (shouldn't happen with a well-behaved client, but keeps
    /// this defensively `Default`-constructible).
    name: String,
    /// Latest state from `UpdatePlayer`. `None` until the first one arrives,
    /// during which this client is not visible to anyone, and edits are
    /// rejected (no position to validate reach against).
    save: Option<PlayerSave>,
    /// Other client ids currently visible to this client. The interest rule
    /// is symmetric (a mutual distance check), so this same set also holds
    /// exactly the clients currently observing *this* one -- used to route
    /// `PlayerMoved`/`PlayerLeft` without a separate reverse index.
    visible: HashSet<ClientId>,
    /// `(level, pos)` LOD chunks this client has been sent. Unlike level-0
    /// chunks, membership here doesn't gate re-requests (a re-request is
    /// still served from cache as normal) -- it exists so a later edit
    /// knows which clients need an unsolicited rebuilt re-send.
    sent_lod: HashSet<(u8, IVec3)>,
    /// Current health (only meaningful in survival; doc/roadmap.md M4). `0`
    /// for a client that has never received a `Hello` reply -- harmless,
    /// since it can't yet be broadcasting or editing.
    hp: u16,
    /// Seconds accumulated toward the next health-regen tick.
    hp_regen_accum: f32,
    /// The player's real inventory: [`MAIN_INVENTORY_SIZE`] slots, `0..9`
    /// being the hotbar (roadmap M5). Loaded from and written back to the
    /// player's persisted record by name. In creative mode the hotbar is
    /// prefilled with every placeable item at join (see [`ClientState`]'s
    /// callers in `tick_server`) rather than being tracked per-tick.
    main: Inventory,
    /// The stack held by the mouse cursor, if any.
    cursor: Option<ItemStack>,
    /// The container UI this client currently has open, if any: a chest
    /// (with its own slots, shared server-side) or a crafting table (which
    /// only widens the crafting grid).
    open_container: Option<(IVec3, ContainerKind)>,
}

impl Default for ClientState {
    fn default() -> Self {
        Self {
            name: String::new(),
            save: None,
            visible: HashSet::new(),
            sent_lod: HashSet::new(),
            hp: 0,
            hp_regen_accum: 0.0,
            main: Inventory::new(MAIN_INVENTORY_SIZE),
            cursor: None,
            open_container: None,
        }
    }
}

/// Cross-client request queues and per-client sent-chunk tracking.
///
/// Requests are served round-robin across clients (see `tick_server`) so one
/// client's backlog can never starve another: `rotation` holds the client IDs
/// that currently have a non-empty `pending` queue, in service order.
#[derive(Resource, Default)]
struct ServerState {
    clients: HashMap<ClientId, ClientState>,
    /// Per-client FIFO of not-yet-served requests (chunk or LOD chunk).
    pending: HashMap<ClientId, VecDeque<ChunkRequest>>,
    /// Mirrors `pending`'s contents for O(1) dedup checks.
    pending_set: HashSet<(ClientId, ChunkRequest)>,
    /// Round-robin order of clients with a non-empty `pending` queue.
    rotation: VecDeque<ClientId>,
}

/// Runs the server until the process exits. Blocking; callers usually spawn
/// a dedicated thread for it.
///
/// If `config.world_dir` is set and holds a previously-saved world, that
/// world's own seed, game mode, and chunks take precedence over
/// `config.seed`/`config.game_mode`: the seed is what terrain generation must
/// stay consistent with (re-deriving it from a fresh generator would desync
/// already-saved, deterministically-regenerated chunks from newly-generated
/// ones), and the mode is a fixed property of that world, not a per-launch
/// setting.
pub fn run_server<T: ServerTransport>(transport: T, config: ServerConfig) {
    let mut app = App::new();
    app.add_plugins(
        MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(
            1.0 / config.tick_hz,
        ))),
    );
    app.insert_resource(TransportRes(transport));

    let mut persistence = Persistence::new(config.world_dir.clone(), config.autosave_interval_secs);
    let loaded = persistence
        .load()
        .expect("failed to load persisted world state");
    let (
        seed,
        game_mode,
        world_time_of_day,
        players,
        loaded_items,
        loaded_containers,
        loaded_furnaces,
        loaded_chunks,
    ) = match loaded {
        Some(world) => {
            if world.seed != config.seed {
                eprintln!(
                    "tsumiki-server: world_dir has saved seed {}, overriding config seed {}",
                    world.seed, config.seed
                );
            }
            (
                world.seed,
                world.game_mode,
                world.world_time_of_day,
                world.players,
                world.items,
                world.containers,
                world.furnaces,
                world.chunks,
            )
        }
        None => (
            config.seed,
            config.game_mode.unwrap_or(GameMode::Survival),
            0.0,
            HashMap::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ),
    };

    let mut cache = ChunkCache::default();
    for (pos, chunk) in loaded_chunks {
        cache.chunks.insert(pos, chunk);
    }

    let mut items_res = sim::ItemsRes::default();
    for record in loaded_items {
        items_res.insert_loaded(record.pos, record.stack, 0.0);
    }

    let mut crafting = CraftingRes::default();
    for (pos, slots) in loaded_containers {
        crafting
            .containers
            .insert(pos, Inventory::from_slots(slots));
    }
    for (pos, record) in loaded_furnaces {
        crafting
            .furnaces
            .states
            .insert(pos, furnace::FurnaceState::from_record(record));
    }

    app.insert_resource(WorldGenRes(WorldGenerator::new(seed)));
    app.insert_resource(WorldSeed(seed));
    app.insert_resource(BlockRegistryRes(BlockRegistry::prototype()));
    app.insert_resource(PlayersRes(players));
    app.insert_resource(SimRes {
        game_mode,
        world_time: sim::WorldTimeRes::new(world_time_of_day),
        items: items_res,
        clock: sim::GameClock::default(),
        tick_interval_secs: 1.0 / config.tick_hz,
    });
    app.insert_resource(cache);
    app.init_resource::<LodCache>();
    app.init_resource::<ServerTick>();
    app.insert_resource(persistence);
    app.init_resource::<ServerState>();
    app.insert_resource(crafting);
    app.add_systems(Update, tick_server::<T>);
    app.run();
}

/// Recomputes player-interest visibility across every client with a known
/// state, sending the resulting `PlayerJoined`/`PlayerLeft` messages.
///
/// This is a full O(n^2) pass over connected clients-with-state. That is
/// fine at M2 scale (doc/roadmap.md M2 targets a LAN game, not a public
/// server with hundreds of concurrent players); a spatial index would be the
/// natural upgrade if that ever changes. Called once per tick, and only when
/// at least one client's state changed this tick.
fn recompute_interest<T: ServerTransport>(
    clients: &mut HashMap<ClientId, ClientState>,
    transport: &mut T,
) {
    let ids: Vec<ClientId> = clients
        .iter()
        .filter(|(_, c)| c.save.is_some())
        .map(|(&id, _)| id)
        .collect();

    for i in 0..ids.len() {
        for j in (i + 1)..ids.len() {
            let a = ids[i];
            let b = ids[j];
            let pos_a = clients[&a].save.unwrap().pos;
            let pos_b = clients[&b].save.unwrap().pos;
            let within = pos_a.distance(pos_b) <= INTEREST_RADIUS;
            let currently_visible = clients[&a].visible.contains(&b);

            if within && !currently_visible {
                clients.get_mut(&a).unwrap().visible.insert(b);
                clients.get_mut(&b).unwrap().visible.insert(a);

                let (b_name, b_state) = {
                    let cb = &clients[&b];
                    (cb.name.clone(), cb.save.unwrap())
                };
                transport.send(
                    a,
                    ServerToClient::PlayerJoined {
                        id: b,
                        name: b_name,
                        state: b_state,
                    },
                );

                let (a_name, a_state) = {
                    let ca = &clients[&a];
                    (ca.name.clone(), ca.save.unwrap())
                };
                transport.send(
                    b,
                    ServerToClient::PlayerJoined {
                        id: a,
                        name: a_name,
                        state: a_state,
                    },
                );
            } else if !within && currently_visible {
                clients.get_mut(&a).unwrap().visible.remove(&b);
                clients.get_mut(&b).unwrap().visible.remove(&a);

                transport.send(a, ServerToClient::PlayerLeft { id: b });
                transport.send(b, ServerToClient::PlayerLeft { id: a });
            }
        }
    }
}

/// Enqueues `req` for `client_id` unless it is already pending, and puts the
/// client into `rotation`'s round-robin service order if this is the first
/// thing it has had queued. Shared by `RequestChunks`, `RequestLodChunks`,
/// and the unsolicited LOD re-send queued on an accepted edit.
fn enqueue_request(
    pending: &mut HashMap<ClientId, VecDeque<ChunkRequest>>,
    pending_set: &mut HashSet<(ClientId, ChunkRequest)>,
    rotation: &mut VecDeque<ClientId>,
    client_id: ClientId,
    req: ChunkRequest,
) {
    let queue = pending.entry(client_id).or_default();
    let was_empty = queue.is_empty();
    if pending_set.insert((client_id, req)) {
        queue.push_back(req);
    }
    if was_empty && !queue.is_empty() {
        rotation.push_back(client_id);
    }
}

/// Builds (or returns the cached) level-`level` LOD chunk at `pos`: pristine
/// terrain from [`WorldGenerator::generate_lod_chunk`], overlaid with every
/// cached level-0 chunk whose position falls inside `pos`'s footprint (see
/// `tsumiki_world::lod` module docs). Caches the result and bumps its
/// last-access tick either way (cache hit or freshly built).
fn get_or_build_lod_chunk(
    lod_cache: &mut LodCache,
    cache: &ChunkCache,
    world_gen: &WorldGenerator,
    level: u8,
    pos: IVec3,
    tick: u64,
) -> Chunk {
    if let Some(chunk) = lod_cache.chunks.get(&(level, pos)) {
        let chunk = chunk.clone();
        lod_cache.last_access.insert((level, pos), tick);
        return chunk;
    }

    let mut chunk = world_gen.generate_lod_chunk(level, pos);

    // The footprint of a level-`level` LOD chunk is a `2^level`-cube of
    // level-0 chunk positions; overlay whichever of those happen to be
    // cached (generated-and-possibly-edited), leaving the rest as pristine
    // terrain.
    let scale = 1i32 << level;
    let base = pos * scale;
    for dz in 0..scale {
        for dy in 0..scale {
            for dx in 0..scale {
                let source_pos = base + IVec3::new(dx, dy, dz);
                if source_pos.y < 0 || source_pos.y >= WORLD_HEIGHT_CHUNKS {
                    continue;
                }
                if let Some(source) = cache.chunks.get(&source_pos) {
                    lod::overlay_downsampled(&mut chunk, level, pos, source, source_pos);
                }
            }
        }
    }

    lod_cache.chunks.insert((level, pos), chunk.clone());
    lod_cache.last_access.insert((level, pos), tick);
    chunk
}

/// Evicts least-recently-used *pristine* level-0 chunks (i.e. not tracked as
/// `modified` by persistence) once their count exceeds
/// [`MAX_PRISTINE_CHUNKS`]. Modified chunks are never evicted -- they are the
/// only copy of a player's edits; pristine chunks regenerate deterministically
/// from the seed, so evicting them only costs a future re-generation, never
/// correctness.
fn evict_pristine_chunks(cache: &mut ChunkCache, persistence: &Persistence) {
    let mut pristine: Vec<(IVec3, u64)> = cache
        .chunks
        .keys()
        .filter(|&&pos| !persistence.is_modified(pos))
        .map(|&pos| (pos, cache.last_access.get(&pos).copied().unwrap_or(0)))
        .collect();
    if pristine.len() <= MAX_PRISTINE_CHUNKS {
        return;
    }
    pristine.sort_by_key(|&(_, last_access)| last_access);
    let excess = pristine.len() - MAX_PRISTINE_CHUNKS;
    for (pos, _) in pristine.into_iter().take(excess) {
        cache.chunks.remove(&pos);
        cache.last_access.remove(&pos);
    }
}

/// Evicts least-recently-used LOD chunks once the cache exceeds
/// [`MAX_LOD_CACHE`]. Every entry is evictable: LOD chunks are never
/// persisted and always rebuild deterministically on demand.
fn evict_lod_cache(lod_cache: &mut LodCache) {
    if lod_cache.chunks.len() <= MAX_LOD_CACHE {
        return;
    }
    let mut entries: Vec<((u8, IVec3), u64)> = lod_cache
        .chunks
        .keys()
        .map(|&key| (key, lod_cache.last_access.get(&key).copied().unwrap_or(0)))
        .collect();
    entries.sort_by_key(|&(_, last_access)| last_access);
    let excess = entries.len() - MAX_LOD_CACHE;
    for (key, _) in entries.into_iter().take(excess) {
        lod_cache.chunks.remove(&key);
        lod_cache.last_access.remove(&key);
    }
}

/// Invalidates every LOD level's cache entry covering `chunk_pos` (rebuilt
/// lazily -- the overlay pass in [`get_or_build_lod_chunk`] picks up
/// whatever the caller already applied to the level-0 chunk in `cache`), and
/// re-queues an unsolicited re-send for every client that was already sent
/// one of those LOD chunks. Shared by `BreakBlock` and `PlaceBlock`.
fn invalidate_lod_for_edit(
    chunk_pos: IVec3,
    lod_cache: &mut LodCache,
    clients: &HashMap<ClientId, ClientState>,
    pending: &mut HashMap<ClientId, VecDeque<ChunkRequest>>,
    pending_set: &mut HashSet<(ClientId, ChunkRequest)>,
    rotation: &mut VecDeque<ClientId>,
) {
    for level in 1..=MAX_LOD {
        let lod_pos = lod::lod_pos_of_chunk(level, chunk_pos);
        lod_cache.chunks.remove(&(level, lod_pos));
        lod_cache.last_access.remove(&(level, lod_pos));

        let recipients: Vec<ClientId> = clients
            .iter()
            .filter(|(_, c)| c.sent_lod.contains(&(level, lod_pos)))
            .map(|(&id, _)| id)
            .collect();
        for recipient in recipients {
            enqueue_request(
                pending,
                pending_set,
                rotation,
                recipient,
                ChunkRequest::Lod {
                    level,
                    pos: lod_pos,
                },
            );
        }
    }
}

/// `true` if `save`'s position is within [`SERVER_REACH`] of `block_pos`'s
/// center. A client that has never sent `UpdatePlayer` (`save` is `None`)
/// always fails closed -- there is no position to validate against, so the
/// edit is rejected rather than the check being skipped (doc/roadmap.md M4:
/// "should not happen in practice, then reject").
fn within_server_reach(save: Option<PlayerSave>, block_pos: IVec3) -> bool {
    let Some(save) = save else {
        return false;
    };
    let center = Vec3::new(
        block_pos.x as f32 + 0.5,
        block_pos.y as f32 + 0.5,
        block_pos.z as f32 + 0.5,
    );
    save.pos.distance(center) <= SERVER_REACH
}

/// Writes `client`'s current save/health/inventory into `players` under its
/// name, keeping the persisted-at-rest map in sync with every live change
/// (the same write-through pattern this map has used since M2, just now
/// carrying more than position). A `None` save (the client hasn't sent one
/// yet) falls back to any previously-known save, or a zero default; this can
/// only happen for a health/inventory change before the client's first
/// `UpdatePlayer`, since edits (which require a save for the reach check)
/// can't trigger it.
fn sync_player_record(players: &mut PlayersRes, client: &ClientState) {
    if client.name.is_empty() {
        return;
    }
    let save = client.save.unwrap_or_else(|| {
        players
            .0
            .get(&client.name)
            .map(|r| r.save)
            .unwrap_or(PlayerSave {
                pos: Vec3::ZERO,
                yaw: 0.0,
                pitch: 0.0,
            })
    });
    players.0.insert(
        client.name.clone(),
        PlayerRecord {
            save,
            hp: client.hp,
            main: client.main.to_vec(),
        },
    );
}

/// Converts the live dropped-item set into its persisted form (see
/// `persist::ItemRecord` for why ids and ages are dropped).
fn item_records(items: &sim::ItemsRes) -> Vec<ItemRecord> {
    items
        .items
        .values()
        .map(|it| ItemRecord {
            pos: it.pos,
            stack: it.stack,
        })
        .collect()
}

/// Converts the live chest map into its persisted form.
fn container_records(
    containers: &HashMap<IVec3, Inventory>,
) -> Vec<(IVec3, Vec<Option<ItemStack>>)> {
    containers
        .iter()
        .map(|(&pos, inv)| (pos, inv.to_vec()))
        .collect()
}

/// Converts the live furnace map into its persisted form (roadmap M6).
fn furnace_records(
    furnaces: &HashMap<IVec3, furnace::FurnaceState>,
) -> Vec<(IVec3, furnace::FurnaceRecord)> {
    furnaces
        .iter()
        .map(|(&pos, state)| (pos, state.to_record()))
        .collect()
}

/// Current slots of the container at `pos` -- chest or furnace, whichever it
/// is -- marking the matching persistence-dirty flag as a side effect.
/// `None` if `pos` names neither (shouldn't happen within one message's
/// handling, since nothing else can remove a container between the click
/// being applied and this lookup, but callers treat it as "nothing to
/// broadcast" rather than unwrapping).
fn container_snapshot(
    crafting: &CraftingRes,
    persistence: &mut Persistence,
    pos: IVec3,
) -> Option<Vec<Option<ItemStack>>> {
    if let Some(inv) = crafting.containers.get(&pos) {
        persistence.mark_containers_dirty();
        return Some(inv.to_vec());
    }
    if let Some(state) = crafting.furnaces.states.get(&pos) {
        persistence.mark_furnaces_dirty();
        return Some(state.inv.to_vec());
    }
    None
}

/// Broadcasts a fresh `ContainerUpdate` for the chest or furnace at `pos` to
/// every client currently viewing it (i.e. whose `open_container` names that
/// position and one of those two kinds -- a crafting table holds no slots
/// and is therefore never a target).
fn broadcast_container_update<T: ServerTransport>(
    transport: &mut T,
    clients: &HashMap<ClientId, ClientState>,
    pos: IVec3,
    slots: &[Option<ItemStack>],
) {
    for (&id, c) in clients {
        if matches!(
            c.open_container,
            Some((p, ContainerKind::Chest | ContainerKind::Furnace)) if p == pos
        ) {
            transport.send(
                id,
                ServerToClient::ContainerUpdate {
                    slots: slots.to_vec(),
                },
            );
        }
    }
}

/// Closes `open_container` (chest or crafting table) for every client
/// currently viewing block position `pos`, sending them `ContainerClosed`.
/// Used when the block housing a container UI is broken.
fn close_container_at<T: ServerTransport>(
    transport: &mut T,
    clients: &mut HashMap<ClientId, ClientState>,
    pos: IVec3,
) {
    for (&id, c) in clients.iter_mut() {
        if matches!(c.open_container, Some((p, _)) if p == pos) {
            c.open_container = None;
            transport.send(id, ServerToClient::ContainerClosed);
        }
    }
}

// Bevy systems take their dependencies as parameters; the count is inherent
// (already reduced by bundling M4's survival-simulation state into `SimRes`
// and M5's crafting/container state into `CraftingRes` -- Bevy's
// function-system tuple impl tops out at 16).
#[allow(clippy::too_many_arguments)]
fn tick_server<T: ServerTransport>(
    mut transport: ResMut<TransportRes<T>>,
    mut state: ResMut<ServerState>,
    world_gen: Res<WorldGenRes>,
    seed: Res<WorldSeed>,
    registry: Res<BlockRegistryRes>,
    mut cache: ResMut<ChunkCache>,
    mut lod_cache: ResMut<LodCache>,
    mut tick: ResMut<ServerTick>,
    mut persistence: ResMut<Persistence>,
    mut players: ResMut<PlayersRes>,
    time: Res<Time>,
    mut exit: MessageWriter<AppExit>,
    mut sim: ResMut<SimRes>,
    mut crafting: ResMut<CraftingRes>,
) {
    // Pump hook: transports that need driving (UDP) get a chance to receive
    // packets and process timeouts before we touch anything else this tick.
    transport.0.tick(time.delta_secs());
    tick.0 = tick.0.wrapping_add(1);

    let ServerState {
        clients,
        pending,
        pending_set,
        rotation,
    } = &mut *state;

    let game_mode = sim.game_mode;
    let SimRes {
        world_time,
        items,
        clock,
        tick_interval_secs,
        ..
    } = &mut *sim;
    let dt = *tick_interval_secs as f32;

    // Set when any client's state changed this tick, so interest is
    // recomputed at most once per tick regardless of how many `UpdatePlayer`
    // messages arrived.
    let mut interest_dirty = false;

    while let Some((client_id, msg)) = transport.0.try_recv() {
        match msg {
            ClientToServer::Hello { name } => {
                let record = players.0.get(&name).cloned();
                let entry = clients.entry(client_id).or_default();
                entry.name = name.clone();
                entry.hp = record.as_ref().map(|r| r.hp).unwrap_or(MAX_HP);
                entry.main = record
                    .as_ref()
                    .map(|r| Inventory::from_slots(r.main.clone()))
                    .unwrap_or_else(|| Inventory::new(MAIN_INVENTORY_SIZE));
                entry.cursor = None;
                entry.open_container = None;
                entry.hp_regen_accum = 0.0;

                // Creative has no scarcity: the hotbar always starts full of
                // every placeable item (doc/roadmap.md M5), refreshed at
                // join/respawn rather than tracked per-tick.
                if game_mode == GameMode::Creative {
                    let refill: Vec<(usize, ItemStack)> = crafting
                        .items
                        .placeable()
                        .take(HOTBAR_SIZE)
                        .enumerate()
                        .map(|(i, item)| (i, ItemStack::new(item, crafting.items.max_stack(item))))
                        .collect();
                    let entry = clients.get_mut(&client_id).unwrap();
                    for (i, stack) in refill {
                        entry.main.set_slot(i, Some(stack));
                    }
                }

                let saved_player = record.map(|r| r.save);
                transport.0.send(
                    client_id,
                    ServerToClient::Welcome {
                        client_id,
                        player: saved_player,
                        game_mode,
                        time_of_day: world_time.time_of_day,
                    },
                );

                // Every mode sees its own inventory now (M5's item catalog,
                // including creative's prefilled hotbar); only survival has
                // health to report.
                let client = clients.get(&client_id).unwrap();
                transport
                    .0
                    .send(client_id, slots::inventory_snapshot(client));
                if game_mode == GameMode::Survival {
                    transport
                        .0
                        .send(client_id, ServerToClient::HealthUpdate { hp: client.hp });
                }

                for (&id, item) in &items.items {
                    transport.0.send(
                        client_id,
                        ServerToClient::ItemSpawned {
                            id,
                            pos: item.pos,
                            stack: item.stack,
                        },
                    );
                }
            }
            ClientToServer::RequestChunks { positions } => {
                clients.entry(client_id).or_default();
                for pos in positions.into_iter().take(MAX_CHUNK_REQUESTS_PER_MESSAGE) {
                    if pos.y < 0 || pos.y >= WORLD_HEIGHT_CHUNKS {
                        continue;
                    }
                    // Dedup only against the pending queue, not against
                    // chunks already served: a client that despawned and
                    // forgot chunks beyond its view distance (then walked
                    // back) must have re-requests honored, served from
                    // cache, not silently dropped.
                    enqueue_request(
                        pending,
                        pending_set,
                        rotation,
                        client_id,
                        ChunkRequest::Level0(pos),
                    );
                }
            }
            ClientToServer::RequestLodChunks { level, positions } => {
                clients.entry(client_id).or_default();
                if !(1..=MAX_LOD).contains(&level) {
                    // Invalid level: the whole message is meaningless (level
                    // is not per-position), so drop it silently.
                    continue;
                }
                let max_y = lod::world_height_lod_chunks(level);
                for pos in positions.into_iter().take(MAX_CHUNK_REQUESTS_PER_MESSAGE) {
                    if pos.y < 0 || pos.y >= max_y {
                        continue;
                    }
                    // Unlike level-0 chunks, a LOD re-request could in
                    // principle be served straight from `lod_cache` even if
                    // it were tracked as "already sent" -- but dedup is only
                    // against the pending queue anyway, for the same
                    // walked-away-and-back reasoning as `RequestChunks`.
                    enqueue_request(
                        pending,
                        pending_set,
                        rotation,
                        client_id,
                        ChunkRequest::Lod { level, pos },
                    );
                }
            }
            ClientToServer::BreakBlock { pos, hotbar } => {
                if pos.y < 0 || pos.y >= WORLD_HEIGHT_BLOCKS {
                    continue;
                }
                // A malformed slot index makes the whole message meaningless
                // (there is no "which tool" to fall back to), so it is
                // rejected outright -- the same treatment `PlaceBlock` gives
                // an out-of-range `hotbar`, rather than silently downgrading
                // to a bare-handed break.
                if hotbar as usize >= HOTBAR_SIZE {
                    continue;
                }
                let Some(client) = clients.get(&client_id) else {
                    continue;
                };
                if game_mode == GameMode::Survival && client.hp == 0 {
                    // Dead players cannot edit.
                    continue;
                }
                if !within_server_reach(client.save, pos) {
                    continue;
                }

                let (chunk_pos, local) = split_block_pos(pos);
                let local = UVec3::new(local.x as u32, local.y as u32, local.z as u32);
                let chunk = cache
                    .chunks
                    .entry(chunk_pos)
                    .or_insert_with(|| world_gen.0.generate_chunk(chunk_pos));

                let existing = chunk.get(local);
                if existing.is_air() || !registry.0.get(existing).solid {
                    continue;
                }

                chunk.set(local, BlockId::AIR);
                cache.last_access.insert(chunk_pos, tick.0);
                persistence.mark_chunk_dirty(chunk_pos);

                for &known_client in clients.keys() {
                    transport.0.send(
                        known_client,
                        ServerToClient::BlockChanged {
                            pos,
                            block: BlockId::AIR,
                        },
                    );
                }
                invalidate_lod_for_edit(
                    chunk_pos,
                    &mut lod_cache,
                    clients,
                    pending,
                    pending_set,
                    rotation,
                );

                // A broken chest's contents must never vanish silently
                // (roadmap M5): drop everything it held and forget it,
                // regardless of game mode -- container contents aren't
                // subject to the survival/creative scarcity rule, only the
                // miner's own credit below is.
                if existing == blocks::CHEST
                    && let Some(mut inv) = crafting.containers.remove(&pos)
                {
                    let recipients: Vec<ClientId> = clients.keys().copied().collect();
                    let drop_pos =
                        Vec3::new(pos.x as f32 + 0.5, pos.y as f32 + 0.5, pos.z as f32 + 0.5);
                    for stack in inv.drain() {
                        sim::spawn_item(
                            &mut transport.0,
                            &recipients,
                            items,
                            &mut cache,
                            &world_gen.0,
                            &registry.0,
                            tick.0,
                            clock.0,
                            drop_pos,
                            stack,
                        );
                    }
                    persistence.mark_containers_dirty();
                }
                // A broken furnace behaves the same way (roadmap M6): its
                // contents (including whatever fuel/input/output it was
                // mid-smelt with) drop rather than vanish, regardless of
                // game mode.
                if existing == blocks::FURNACE
                    && let Some(state) = crafting.furnaces.states.remove(&pos)
                {
                    let recipients: Vec<ClientId> = clients.keys().copied().collect();
                    let drop_pos =
                        Vec3::new(pos.x as f32 + 0.5, pos.y as f32 + 0.5, pos.z as f32 + 0.5);
                    let mut inv = state.inv;
                    for stack in inv.drain() {
                        sim::spawn_item(
                            &mut transport.0,
                            &recipients,
                            items,
                            &mut cache,
                            &world_gen.0,
                            &registry.0,
                            tick.0,
                            clock.0,
                            drop_pos,
                            stack,
                        );
                    }
                    persistence.mark_furnaces_dirty();
                }
                // Any UI open on the broken position (chest, furnace, or
                // crafting table) closes for every viewer.
                close_container_at(&mut transport.0, clients, pos);

                if game_mode == GameMode::Survival {
                    // No break-time enforcement server-side yet: movement
                    // (and, for now, mining duration) is client-authoritative
                    // by the same decision as M1's block-edit trust model --
                    // the server validates reach and solidity, not timing.
                    //
                    // Harvest gating (roadmap M6): the right tool at or above
                    // the block's tier, in the named `hotbar` slot, is
                    // required for a drop at all.
                    let block_def = registry.0.get(existing);
                    let recipients: Vec<ClientId> = clients.keys().copied().collect();
                    let drop_pos =
                        Vec3::new(pos.x as f32 + 0.5, pos.y as f32 + 0.5, pos.z as f32 + 0.5);
                    let client = clients.get_mut(&client_id).unwrap();

                    let outcome = harvest::resolve_harvest(
                        block_def,
                        &client.main,
                        hotbar as usize,
                        &crafting.items,
                    );
                    let mut main_changed = outcome.tool_slot.is_some();
                    harvest::wear_tool(&mut client.main, outcome.tool_slot, &crafting.items);

                    if outcome.drop_allowed
                        && let Some(drop) = crafting.items.drop_of(existing)
                    {
                        sim::credit_or_drop(
                            &mut transport.0,
                            &recipients,
                            items,
                            &mut cache,
                            &world_gen.0,
                            &registry.0,
                            &crafting.items,
                            tick.0,
                            clock.0,
                            &mut client.main,
                            drop,
                            drop_pos,
                        );
                        main_changed = true;
                    }

                    if main_changed {
                        transport
                            .0
                            .send(client_id, slots::inventory_snapshot(client));
                        sync_player_record(&mut players, client);
                        persistence.mark_player_dirty();
                    }
                }
            }
            ClientToServer::PlaceBlock { pos, hotbar } => {
                if pos.y < 0 || pos.y >= WORLD_HEIGHT_BLOCKS {
                    continue;
                }
                if hotbar as usize >= HOTBAR_SIZE {
                    continue;
                }
                let Some(client) = clients.get(&client_id) else {
                    continue;
                };
                if game_mode == GameMode::Survival && client.hp == 0 {
                    continue;
                }
                if !within_server_reach(client.save, pos) {
                    continue;
                }
                let Some(held) = client.main.slot(hotbar as usize) else {
                    continue;
                };
                let Some(block) = crafting.items.places(held.item) else {
                    // The held item doesn't place a block (a client cannot
                    // ask to place something it doesn't actually have
                    // selected).
                    continue;
                };
                if !registry.0.is_valid(block) || block.is_air() {
                    continue;
                }

                let (chunk_pos, local) = split_block_pos(pos);
                let local = UVec3::new(local.x as u32, local.y as u32, local.z as u32);
                let chunk = cache
                    .chunks
                    .entry(chunk_pos)
                    .or_insert_with(|| world_gen.0.generate_chunk(chunk_pos));
                let existing = chunk.get(local);
                if !(existing.is_air() || existing == blocks::WATER) {
                    continue;
                }
                chunk.set(local, block);
                cache.last_access.insert(chunk_pos, tick.0);
                persistence.mark_chunk_dirty(chunk_pos);

                if game_mode == GameMode::Survival {
                    let client = clients.get_mut(&client_id).unwrap();
                    client.main.take_from(hotbar as usize, 1);
                    transport
                        .0
                        .send(client_id, slots::inventory_snapshot(client));
                    sync_player_record(&mut players, client);
                    persistence.mark_player_dirty();
                }

                for &known_client in clients.keys() {
                    transport
                        .0
                        .send(known_client, ServerToClient::BlockChanged { pos, block });
                }
                invalidate_lod_for_edit(
                    chunk_pos,
                    &mut lod_cache,
                    clients,
                    pending,
                    pending_set,
                    rotation,
                );
            }
            ClientToServer::SlotClick { slot, right, shift } => {
                let Some(client) = clients.get_mut(&client_id) else {
                    continue;
                };
                let changed_container =
                    slots::handle_slot_click(client, &mut crafting, slot, right, shift);

                let client = clients.get(&client_id).unwrap();
                transport
                    .0
                    .send(client_id, slots::inventory_snapshot(client));
                sync_player_record(&mut players, client);
                persistence.mark_player_dirty();

                if let Some(pos) = changed_container
                    && let Some(snapshot) = container_snapshot(&crafting, &mut persistence, pos)
                {
                    broadcast_container_update(&mut transport.0, clients, pos, &snapshot);
                }
            }
            ClientToServer::DropSlot { slot, all } => {
                let Some(client) = clients.get(&client_id) else {
                    continue;
                };
                let Some(drop_pos) = client.save.map(|s| s.pos) else {
                    continue;
                };
                let client = clients.get_mut(&client_id).unwrap();
                let (taken, changed_container) =
                    slots::handle_drop_slot(client, &mut crafting, slot, all);

                if let Some(stack) = taken {
                    let recipients: Vec<ClientId> = clients.keys().copied().collect();
                    sim::spawn_item(
                        &mut transport.0,
                        &recipients,
                        items,
                        &mut cache,
                        &world_gen.0,
                        &registry.0,
                        tick.0,
                        clock.0,
                        drop_pos,
                        stack,
                    );
                    let client = clients.get(&client_id).unwrap();
                    transport
                        .0
                        .send(client_id, slots::inventory_snapshot(client));
                    sync_player_record(&mut players, client);
                    persistence.mark_player_dirty();

                    if let Some(pos) = changed_container
                        && let Some(snapshot) = container_snapshot(&crafting, &mut persistence, pos)
                    {
                        broadcast_container_update(&mut transport.0, clients, pos, &snapshot);
                    }
                }
            }
            ClientToServer::OpenContainer { pos } => {
                if pos.y < 0 || pos.y >= WORLD_HEIGHT_BLOCKS {
                    continue;
                }
                let Some(client) = clients.get(&client_id) else {
                    continue;
                };
                if !within_server_reach(client.save, pos) {
                    continue;
                }

                let (chunk_pos, local) = split_block_pos(pos);
                let local = UVec3::new(local.x as u32, local.y as u32, local.z as u32);
                let chunk = cache
                    .chunks
                    .entry(chunk_pos)
                    .or_insert_with(|| world_gen.0.generate_chunk(chunk_pos));
                let block = chunk.get(local);
                cache.last_access.insert(chunk_pos, tick.0);

                let Some(interaction) = registry.0.get(block).interaction else {
                    continue;
                };
                let (kind, slots) = match interaction {
                    BlockInteraction::Container => {
                        let inv = crafting.containers.entry(pos).or_insert_with(|| {
                            Inventory::new(tsumiki_world::inventory::CHEST_SIZE)
                        });
                        (ContainerKind::Chest, inv.to_vec())
                    }
                    BlockInteraction::CraftingTable => (ContainerKind::CraftingTable, Vec::new()),
                    BlockInteraction::Furnace => {
                        let state = crafting.furnaces.states.entry(pos).or_default();
                        (ContainerKind::Furnace, state.inv.to_vec())
                    }
                };
                let client = clients.get_mut(&client_id).unwrap();
                client.open_container = Some((pos, kind));
                transport.0.send(
                    client_id,
                    ServerToClient::ContainerOpened { kind, pos, slots },
                );
                // A furnace's viewer gets its progress immediately rather
                // than waiting up to `furnace::BROADCAST_INTERVAL_SECS` for
                // the next periodic broadcast, so the bar doesn't start
                // looking wrong (frozen at 0) for a lit furnace opened
                // mid-burn.
                if kind == ContainerKind::Furnace
                    && let Some(state) = crafting.furnaces.states.get(&pos)
                {
                    let (cook, fuel) = state.progress(&crafting.smelting);
                    transport
                        .0
                        .send(client_id, ServerToClient::FurnaceProgress { cook, fuel });
                }
            }
            ClientToServer::CloseContainer => {
                let recipients: Vec<ClientId> = clients.keys().copied().collect();
                let Some(client) = clients.get_mut(&client_id) else {
                    continue;
                };
                client.open_container = None;
                sim::drop_ui_leftovers(
                    &mut transport.0,
                    &recipients,
                    items,
                    &mut cache,
                    &world_gen.0,
                    &registry.0,
                    tick.0,
                    clock.0,
                    client,
                );
                transport.0.send(client_id, ServerToClient::ContainerClosed);
                let client = clients.get(&client_id).unwrap();
                transport
                    .0
                    .send(client_id, slots::inventory_snapshot(client));
            }
            ClientToServer::Craft { recipe, all } => {
                let Some(client) = clients.get(&client_id) else {
                    continue;
                };
                // The station the player currently has open, if any; `None`
                // (no crafting table open) still reaches every hand recipe.
                let station = matches!(
                    client.open_container,
                    Some((_, ContainerKind::CraftingTable))
                )
                .then_some(CraftingStation::CraftingTable);
                // Reject an unknown recipe id, or one whose station isn't
                // the one currently open -- a client may name any id it
                // likes, so this must never panic.
                if !crafting.recipes.is_reachable(recipe, station) {
                    continue;
                }
                let Some(recipe_def) = crafting.recipes.get(recipe).cloned() else {
                    continue;
                };
                let drop_pos = client.save.map(|s| s.pos).unwrap_or(Vec3::ZERO);

                let client = clients.get_mut(&client_id).unwrap();
                let times = if all { u32::MAX } else { 1 };
                let (_, overflow) = tsumiki_world::recipe::craft(
                    &recipe_def,
                    times,
                    &mut client.main,
                    &crafting.items,
                );

                if !overflow.is_empty() {
                    let recipients: Vec<ClientId> = clients.keys().copied().collect();
                    for stack in overflow {
                        sim::spawn_item(
                            &mut transport.0,
                            &recipients,
                            items,
                            &mut cache,
                            &world_gen.0,
                            &registry.0,
                            tick.0,
                            clock.0,
                            drop_pos,
                            stack,
                        );
                    }
                }

                let client = clients.get(&client_id).unwrap();
                transport
                    .0
                    .send(client_id, slots::inventory_snapshot(client));
                sync_player_record(&mut players, client);
                persistence.mark_player_dirty();
            }
            ClientToServer::ReportDamage { amount, cause: _ } => {
                // Damage is client-detected but server-applied; `cause`
                // doesn't currently change server behavior (no cause-specific
                // rules yet), but is kept on the wire for future use (e.g.
                // death messages) and client-side UI.
                if game_mode != GameMode::Survival {
                    continue;
                }
                let Some(client) = clients.get_mut(&client_id) else {
                    continue;
                };
                if client.hp == 0 {
                    // Already dead: ignore further damage.
                    continue;
                }
                let amount = amount.min(MAX_HP);
                client.hp = client.hp.saturating_sub(amount);
                let new_hp = client.hp;
                persistence.mark_player_dirty();
                transport
                    .0
                    .send(client_id, ServerToClient::HealthUpdate { hp: new_hp });

                if new_hp == 0 {
                    let drop_pos = client.save.map(|s| s.pos).unwrap_or(Vec3::ZERO);
                    let dropped: Vec<ItemStack> = client.main.drain();
                    let recipients: Vec<ClientId> = clients.keys().copied().collect();

                    // Items can never be parked in a closed UI (roadmap M5):
                    // the cursor drops too, and any open container UI
                    // closes.
                    let client = clients.get_mut(&client_id).unwrap();
                    sim::drop_ui_leftovers(
                        &mut transport.0,
                        &recipients,
                        items,
                        &mut cache,
                        &world_gen.0,
                        &registry.0,
                        tick.0,
                        clock.0,
                        client,
                    );
                    if client.open_container.take().is_some() {
                        transport.0.send(client_id, ServerToClient::ContainerClosed);
                    }
                    sync_player_record(&mut players, client);

                    for stack in dropped {
                        sim::spawn_item(
                            &mut transport.0,
                            &recipients,
                            items,
                            &mut cache,
                            &world_gen.0,
                            &registry.0,
                            tick.0,
                            clock.0,
                            drop_pos,
                            stack,
                        );
                    }
                    persistence.mark_items_dirty();

                    let client = clients.get(&client_id).unwrap();
                    transport
                        .0
                        .send(client_id, slots::inventory_snapshot(client));
                    transport
                        .0
                        .send(client_id, ServerToClient::Died { at: drop_pos });
                }
            }
            ClientToServer::Respawn => {
                if game_mode != GameMode::Survival {
                    continue;
                }
                let Some(client) = clients.get_mut(&client_id) else {
                    continue;
                };
                if client.hp != 0 {
                    // Only meaningful while dead.
                    continue;
                }
                client.hp = MAX_HP;
                sync_player_record(&mut players, client);
                persistence.mark_player_dirty();
                transport
                    .0
                    .send(client_id, ServerToClient::HealthUpdate { hp: MAX_HP });
            }
            ClientToServer::UpdatePlayer(save) => {
                let Some(client) = clients.get_mut(&client_id) else {
                    // No Hello yet; there's no name to key persistence or
                    // interest off of, so there's nothing sound to do here.
                    continue;
                };
                client.save = Some(save);
                // Relay to whoever currently observes this client *before*
                // interest is recomputed below, so an observer that becomes
                // newly visible this tick gets `PlayerJoined` (which already
                // carries the fresh state) instead of a redundant
                // `PlayerMoved` on top of it.
                let observers: Vec<ClientId> = client.visible.iter().copied().collect();

                // An open container UI auto-closes once its viewer walks out
                // of reach (roadmap M5); unlike an explicit `CloseContainer`
                // this doesn't drop anything, since the cursor is untouched
                // by simply losing sight of a chest.
                let closed_container = match client.open_container {
                    Some((pos, _)) if !within_server_reach(client.save, pos) => {
                        client.open_container = None;
                        true
                    }
                    _ => false,
                };

                sync_player_record(&mut players, client);
                persistence.mark_player_dirty();
                interest_dirty = true;

                for observer in observers {
                    transport.0.send(
                        observer,
                        ServerToClient::PlayerMoved {
                            id: client_id,
                            state: save,
                        },
                    );
                }
                if closed_container {
                    transport.0.send(client_id, ServerToClient::ContainerClosed);
                }
            }
            ClientToServer::Goodbye => {
                // Idempotent: a network transport synthesizes a Goodbye on
                // disconnect, which can duplicate (or race with) an explicit
                // one already processed for the same client.
                let Some(mut leaving) = clients.remove(&client_id) else {
                    continue;
                };

                // Items can never be parked in a closed UI (roadmap M5),
                // disconnecting included.
                let recipients: Vec<ClientId> = clients.keys().copied().collect();
                sim::drop_ui_leftovers(
                    &mut transport.0,
                    &recipients,
                    items,
                    &mut cache,
                    &world_gen.0,
                    &registry.0,
                    tick.0,
                    clock.0,
                    &mut leaving,
                );

                persistence
                    .save(
                        seed.0,
                        game_mode,
                        world_time.time_of_day,
                        &players.0,
                        &item_records(items),
                        &container_records(&crafting.containers),
                        &furnace_records(&crafting.furnaces.states),
                        &cache.chunks,
                    )
                    .expect("failed to save world on goodbye");

                for &observer in &leaving.visible {
                    if let Some(obs) = clients.get_mut(&observer) {
                        obs.visible.remove(&client_id);
                    }
                    transport
                        .0
                        .send(observer, ServerToClient::PlayerLeft { id: client_id });
                }

                pending.remove(&client_id);
                pending_set.retain(|&(cid, _)| cid != client_id);
                rotation.retain(|&cid| cid != client_id);

                if clients.is_empty() {
                    exit.write(AppExit::Success);
                }
            }
        }
    }

    // Passive per-tick simulation (doc/roadmap.md M4): day/night clock,
    // health regen, and item pickup/expiry. Driven by the fixed tick
    // interval, not measured wall-clock time (see `SimRes::tick_interval_secs`
    // docs), so this is deterministic and simulable in tests.
    clock.0 += dt;
    sim::tick_world_time(&mut transport.0, clients, world_time, dt);

    // Furnaces tick every server tick regardless of whether anyone has one
    // open (roadmap M6) -- see `furnace`'s module docs for why this cannot
    // credit time retroactively across a server restart. `dt` is the fixed
    // tick interval, the same clock every other passive system here uses.
    //
    // Destructured through `&mut *crafting` (rather than repeated
    // `crafting.field` projections) so the borrow checker sees these as the
    // disjoint fields they are -- `crafting` is a `ResMut`, and borrowing
    // through its `DerefMut` repeatedly would otherwise look like one
    // overlapping borrow.
    {
        let slots::CraftingRes {
            furnaces,
            smelting,
            items: item_reg,
            ..
        } = &mut *crafting;
        let furnace_changed = furnace::tick_furnaces(&mut furnaces.states, smelting, item_reg, dt);
        if !furnace_changed.is_empty() {
            persistence.mark_furnaces_dirty();
        }

        // Broadcast progress to whoever has a furnace open, throttled to
        // `furnace::BROADCAST_INTERVAL_SECS` so a lit furnace doesn't flood
        // its viewer with an update every tick.
        furnaces.broadcast_accum += dt;
        while furnaces.broadcast_accum >= furnace::BROADCAST_INTERVAL_SECS {
            furnaces.broadcast_accum -= furnace::BROADCAST_INTERVAL_SECS;
            for (&id, c) in clients.iter() {
                if let Some((pos, ContainerKind::Furnace)) = c.open_container
                    && let Some(state) = furnaces.states.get(&pos)
                {
                    let (cook, fuel) = state.progress(smelting);
                    transport
                        .0
                        .send(id, ServerToClient::FurnaceProgress { cook, fuel });
                }
            }
        }
    }

    let regen_changed = if game_mode == GameMode::Survival {
        sim::tick_regen(&mut transport.0, clients, dt)
    } else {
        Vec::new()
    };
    if !regen_changed.is_empty() {
        for &id in &regen_changed {
            if let Some(client) = clients.get(&id) {
                sync_player_record(&mut players, client);
            }
        }
        persistence.mark_player_dirty();
    }

    let pickup_changed = sim::tick_items(
        &mut transport.0,
        clients,
        items,
        &crafting.items,
        clock.0,
        game_mode == GameMode::Survival,
    );
    // Always mark items dirty: expiry can silently remove items even when no
    // pickup happened, and tracking that separately isn't worth the
    // complexity (a save's meta.bin is small).
    persistence.mark_items_dirty();
    if !pickup_changed.is_empty() {
        for &id in &pickup_changed {
            if let Some(client) = clients.get(&id) {
                transport.0.send(id, slots::inventory_snapshot(client));
                sync_player_record(&mut players, client);
            }
        }
        persistence.mark_player_dirty();
    }

    if interest_dirty {
        recompute_interest(clients, &mut transport.0);
    }

    // Round-robin across clients so one client's backlog cannot starve
    // another: each iteration serves at most one request (chunk or LOD
    // chunk) from the next client in `rotation`, re-queuing that client at
    // the back if it still has more pending. One shared budget governs both
    // request kinds.
    let mut served = 0;
    while served < CHUNK_SEND_BUDGET {
        let Some(client_id) = rotation.pop_front() else {
            break;
        };
        let Some(queue) = pending.get_mut(&client_id) else {
            continue;
        };
        let Some(req) = queue.pop_front() else {
            continue;
        };
        pending_set.remove(&(client_id, req));

        match req {
            ChunkRequest::Level0(pos) => {
                let chunk = cache
                    .chunks
                    .entry(pos)
                    .or_insert_with(|| world_gen.0.generate_chunk(pos))
                    .clone();
                cache.last_access.insert(pos, tick.0);
                transport
                    .0
                    .send(client_id, ServerToClient::ChunkData { pos, chunk });
            }
            ChunkRequest::Lod { level, pos } => {
                let chunk = get_or_build_lod_chunk(
                    &mut lod_cache,
                    &cache,
                    &world_gen.0,
                    level,
                    pos,
                    tick.0,
                );
                if let Some(client) = clients.get_mut(&client_id) {
                    client.sent_lod.insert((level, pos));
                }
                transport.0.send(
                    client_id,
                    ServerToClient::LodChunkData { level, pos, chunk },
                );
            }
        }

        served += 1;
        if !queue.is_empty() {
            rotation.push_back(client_id);
        }
    }

    // Bounded memory (doc/roadmap.md M3): keep both caches from growing
    // without limit. Both regenerate deterministically, so eviction never
    // loses data -- only trades memory for a future re-generation.
    evict_pristine_chunks(&mut cache, &persistence);
    evict_lod_cache(&mut lod_cache);

    // Periodic autosave: only touches disk when the clock has crossed the
    // interval AND something actually changed since the last save.
    if persistence.autosave_due(time.delta_secs_f64()) && persistence.has_dirty() {
        persistence
            .save(
                seed.0,
                game_mode,
                world_time.time_of_day,
                &players.0,
                &item_records(items),
                &container_records(&crafting.containers),
                &furnace_records(&crafting.furnaces.states),
                &cache.chunks,
            )
            .expect("failed to autosave world");
    }

    transport.0.flush();
}

#[cfg(test)]
mod tests;

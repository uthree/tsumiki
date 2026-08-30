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
//!    - `Hello` → reply `Welcome` (looking up any saved state for that name).
//!    - `RequestChunks` → enqueue positions into that client's own queue
//!      (deduplicated only against the queue itself, so a re-request for a
//!      chunk the client has forgotten and walked back to is served again;
//!      out-of-bounds Y is ignored; a single message is capped so it cannot
//!      dominate the queue or force an unbounded insert).
//!    - `UpdatePlayer` → record the client's latest state, relay it as
//!      `PlayerMoved` to observers currently seeing this client, and persist
//!      it under that client's name.
//!    - `Goodbye` → save, broadcast `PlayerLeft` to this client's observers,
//!      and forget the client. Idempotent: a second `Goodbye` for an already
//!      -removed client is a no-op (a network transport can synthesize one on
//!      disconnect that duplicates an explicit one already received).
//! 2. If any client's state changed this tick, recompute interest
//!    (`recompute_interest`): every pair of clients with known state is
//!    checked against [`INTEREST_RADIUS`], sending `PlayerJoined`/
//!    `PlayerLeft` for pairs that crossed the threshold.
//! 3. Serve up to [`CHUNK_SEND_BUDGET`] queued requests, round-robin across
//!    clients so one client's backlog cannot starve another. One shared
//!    queue (and budget) covers both full-resolution chunk requests and LOD
//!    chunk requests (doc/design.md §3): generate/build the chunk if it is
//!    not already cached, cache it, and send `ChunkData`/`LodChunkData`.
//! 4. An accepted `SetBlock` invalidates every LOD level's cache entry that
//!    covers the edited chunk (rebuilt lazily on next access) and, for any
//!    client that was already sent one of those LOD chunks, enqueues an
//!    unsolicited rebuilt re-send through the same budgeted queue.
//! 5. Bounded memory (doc/roadmap.md M3): pristine (unmodified) level-0
//!    chunks and LOD chunks are evicted least-recently-used once their caches
//!    exceed [`MAX_PRISTINE_CHUNKS`] / [`MAX_LOD_CACHE`]. Both regenerate
//!    deterministically from the seed (and, for LOD, from whatever level-0
//!    chunks are still cached), so eviction is invisible to correctness.

mod persist;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::prelude::*;

use bevy_math::{IVec3, UVec3};
use tsumiki_protocol::{ClientId, ClientToServer, PlayerSave, ServerToClient, ServerTransport};
use tsumiki_world::lod::{self, MAX_LOD};
use tsumiki_world::{
    BlockRegistry, Chunk, WORLD_HEIGHT_BLOCKS, WORLD_HEIGHT_CHUNKS, WorldGenerator, split_block_pos,
};

use persist::Persistence;

/// Maximum chunks generated + sent per tick, to keep tick times bounded.
/// Shared by full-resolution chunk requests and LOD chunk requests alike --
/// they are served from one unified round-robin queue (see module docs).
pub const CHUNK_SEND_BUDGET: usize = 32;

/// Two players are mutually visible for replication purposes when within
/// this many blocks of each other (doc/roadmap.md M2, "basic interest
/// management").
pub const INTEREST_RADIUS: f32 = 320.0;

/// Maximum positions accepted from a single `RequestChunks` or
/// `RequestLodChunks` message. Set with headroom above the client's own
/// per-frame cap (`MAX_CHUNK_REQUESTS_PER_FRAME = 64` in
/// `crates/client/src/net.rs`), so a legitimate client's burst always fits in
/// one message while a malformed or hostile message cannot force an
/// unbounded synchronous insert into the pending queues.
const MAX_CHUNK_REQUESTS_PER_MESSAGE: usize = 128;

/// Cap on cached level-0 chunks that are *not* in the persistence `modified`
/// set (doc/roadmap.md M3, "bounded memory"). Modified chunks are the only
/// on-disk copy of a player's edits and are never evicted; pristine chunks
/// regenerate deterministically from the seed, so evicting them is invisible
/// to correctness -- only to how often they're regenerated.
///
/// Overridden to a tiny value under `cfg(test)` so the eviction test doesn't
/// need to push thousands of chunks through the request/serve budget to
/// observe eviction.
#[cfg(not(test))]
pub const MAX_PRISTINE_CHUNKS: usize = 4096;
#[cfg(test)]
pub const MAX_PRISTINE_CHUNKS: usize = 8;

/// Cap on cached LOD chunks (all levels combined). Unlike level-0 chunks, LOD
/// chunks are never persisted at all -- design.md §3 derives them from
/// worldgen plus whatever level-0 chunks happen to be cached -- so every
/// entry is evictable.
///
/// Also overridden under `cfg(test)`; see [`MAX_PRISTINE_CHUNKS`].
#[cfg(not(test))]
pub const MAX_LOD_CACHE: usize = 2048;
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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            seed: 0,
            tick_hz: 30.0,
            world_dir: None,
            autosave_interval_secs: 10.0,
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

/// Persisted player saves keyed by name (M2: real multiplayer distinguishes
/// clients by identity, so each name gets its own slot). `Welcome.player` is
/// looked up here by the connecting client's `Hello` name.
#[derive(Resource, Default)]
struct PlayersRes(HashMap<String, PlayerSave>);

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

/// A single unqueued unit of work for the shared chunk/LOD-chunk send queue
/// (see module docs point 3): one full-resolution chunk, or one LOD chunk at
/// a given level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ChunkRequest {
    Level0(IVec3),
    Lod { level: u8, pos: IVec3 },
}

/// Per-client bookkeeping for replication.
#[derive(Default)]
struct ClientState {
    /// Name from `Hello`. Empty if a client somehow sent other messages
    /// before `Hello` (shouldn't happen with a well-behaved client, but keeps
    /// this defensively `Default`-constructible).
    name: String,
    /// Latest state from `UpdatePlayer`. `None` until the first one arrives,
    /// during which this client is not visible to anyone.
    save: Option<PlayerSave>,
    /// Other client ids currently visible to this client. The interest rule
    /// is symmetric (a mutual distance check), so this same set also holds
    /// exactly the clients currently observing *this* one -- used to route
    /// `PlayerMoved`/`PlayerLeft` without a separate reverse index.
    visible: HashSet<ClientId>,
    /// `(level, pos)` LOD chunks this client has been sent. Unlike level-0
    /// chunks, membership here doesn't gate re-requests (a re-request is
    /// still served from cache as normal) -- it exists so a later `SetBlock`
    /// knows which clients need an unsolicited rebuilt re-send.
    sent_lod: HashSet<(u8, IVec3)>,
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
/// world's own seed and chunks take precedence over `config.seed`: the seed
/// is what terrain generation must stay consistent with, and re-deriving it
/// from a fresh generator would desync already-saved (and unmodified,
/// deterministically-regenerated) chunks from newly-generated ones.
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
    let (seed, players, loaded_chunks) = match loaded {
        Some(world) => {
            if world.seed != config.seed {
                eprintln!(
                    "tsumiki-server: world_dir has saved seed {}, overriding config seed {}",
                    world.seed, config.seed
                );
            }
            (world.seed, world.players, world.chunks)
        }
        None => (config.seed, HashMap::new(), Vec::new()),
    };

    let mut cache = ChunkCache::default();
    for (pos, chunk) in loaded_chunks {
        cache.chunks.insert(pos, chunk);
    }

    app.insert_resource(WorldGenRes(WorldGenerator::new(seed)));
    app.insert_resource(WorldSeed(seed));
    app.insert_resource(BlockRegistryRes(BlockRegistry::prototype()));
    app.insert_resource(PlayersRes(players));
    app.insert_resource(cache);
    app.init_resource::<LodCache>();
    app.init_resource::<ServerTick>();
    app.insert_resource(persistence);
    app.init_resource::<ServerState>();
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
/// and the unsolicited LOD re-send queued on `SetBlock`.
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

// Bevy systems take their dependencies as parameters; the count is inherent.
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

    // Set when any client's state changed this tick, so interest is
    // recomputed at most once per tick regardless of how many `UpdatePlayer`
    // messages arrived.
    let mut interest_dirty = false;

    while let Some((client_id, msg)) = transport.0.try_recv() {
        match msg {
            ClientToServer::Hello { name } => {
                let saved = players.0.get(&name).copied();
                // A client is "known" (a broadcast target) from Hello
                // onward, even before it ever requests a chunk. `or_default`
                // preserves any state already recorded under this id (e.g. a
                // stray RequestChunks that arrived first).
                clients.entry(client_id).or_default().name = name;
                transport.0.send(
                    client_id,
                    ServerToClient::Welcome {
                        client_id,
                        player: saved,
                    },
                );
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
            ClientToServer::SetBlock { pos, block } => {
                if pos.y < 0 || pos.y >= WORLD_HEIGHT_BLOCKS {
                    continue;
                }
                if !registry.0.is_valid(block) {
                    continue;
                }

                let (chunk_pos, local) = split_block_pos(pos);
                let local = UVec3::new(local.x as u32, local.y as u32, local.z as u32);
                let chunk = cache
                    .chunks
                    .entry(chunk_pos)
                    .or_insert_with(|| world_gen.0.generate_chunk(chunk_pos));

                if chunk.get(local) == block {
                    // No-op edit: skip silently, no broadcast.
                    continue;
                }
                chunk.set(local, block);
                cache.last_access.insert(chunk_pos, tick.0);
                persistence.mark_chunk_dirty(chunk_pos);

                for &known_client in clients.keys() {
                    transport
                        .0
                        .send(known_client, ServerToClient::BlockChanged { pos, block });
                }

                // Invalidate every LOD level's cache entry covering the
                // edited chunk (rebuilt lazily -- the overlay pass in
                // `get_or_build_lod_chunk` will pick up the edit we just
                // applied above, since the level-0 chunk is still in
                // `cache`), and re-queue an unsolicited re-send for every
                // client that was already sent one of those LOD chunks. The
                // usual pending-queue dedup collapses a burst of edits to the
                // same chunk into a single queued re-send per level.
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
            ClientToServer::UpdatePlayer(save) => {
                let Some(client) = clients.get_mut(&client_id) else {
                    // No Hello yet; there's no name to key persistence or
                    // interest off of, so there's nothing sound to do here.
                    continue;
                };
                client.save = Some(save);
                let name = client.name.clone();
                // Relay to whoever currently observes this client *before*
                // interest is recomputed below, so an observer that becomes
                // newly visible this tick gets `PlayerJoined` (which already
                // carries the fresh state) instead of a redundant
                // `PlayerMoved` on top of it.
                let observers: Vec<ClientId> = client.visible.iter().copied().collect();

                players.0.insert(name, save);
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
            }
            ClientToServer::Goodbye => {
                // Idempotent: a network transport synthesizes a Goodbye on
                // disconnect, which can duplicate (or race with) an explicit
                // one already processed for the same client.
                let Some(leaving) = clients.remove(&client_id) else {
                    continue;
                };

                persistence
                    .save(seed.0, &players.0, &cache.chunks)
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
            .save(seed.0, &players.0, &cache.chunks)
            .expect("failed to autosave world");
    }

    transport.0.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::thread;
    use std::time::Instant;

    use tsumiki_protocol::ClientTransport;
    use tsumiki_protocol::local::{LOCAL_CLIENT_ID, pair};

    use bevy_math::Vec3;
    use tsumiki_world::{BlockId, CHUNK_SIZE};

    /// In-memory multi-client transport for exercising `tick_server` directly
    /// (the real `local` transport hardcodes a single client, which can't
    /// reproduce a two-client scenario). Also counts `tick`/`flush` calls so
    /// tests can confirm the pump hooks fire every server tick.
    #[derive(Default)]
    struct MockTransport {
        incoming: VecDeque<(ClientId, ClientToServer)>,
        outgoing: HashMap<ClientId, Vec<ServerToClient>>,
        tick_calls: u32,
        flush_calls: u32,
    }

    impl MockTransport {
        fn push(&mut self, client_id: ClientId, msg: ClientToServer) {
            self.incoming.push_back((client_id, msg));
        }

        /// Removes and returns everything sent to `client_id` so far, so a
        /// test can check what arrives *after* this point without earlier
        /// messages (e.g. a `Welcome`) getting in the way.
        fn take(&mut self, client_id: ClientId) -> Vec<ServerToClient> {
            self.outgoing.remove(&client_id).unwrap_or_default()
        }
    }

    impl ServerTransport for MockTransport {
        fn try_recv(&mut self) -> Option<(ClientId, ClientToServer)> {
            self.incoming.pop_front()
        }

        fn send(&mut self, to: ClientId, msg: ServerToClient) {
            self.outgoing.entry(to).or_default().push(msg);
        }

        fn tick(&mut self, _dt: f32) {
            self.tick_calls += 1;
        }

        fn flush(&mut self) {
            self.flush_calls += 1;
        }
    }

    /// Builds a minimal `App` wired the same way `run_server` would, but
    /// without `MinimalPlugins`/the schedule runner, so tests can drive
    /// `tick_server` tick-by-tick via `app.update()`, using a caller-supplied
    /// `Persistence` (ephemeral or backed by a real directory).
    fn new_test_app_with<T: ServerTransport>(
        transport: T,
        seed: u64,
        persistence: Persistence,
    ) -> App {
        let mut app = App::new();
        app.insert_resource(TransportRes(transport));
        app.insert_resource(WorldGenRes(WorldGenerator::new(seed)));
        app.insert_resource(WorldSeed(seed));
        app.insert_resource(BlockRegistryRes(BlockRegistry::prototype()));
        app.init_resource::<PlayersRes>();
        app.init_resource::<ChunkCache>();
        app.init_resource::<LodCache>();
        app.init_resource::<ServerTick>();
        app.insert_resource(persistence);
        app.init_resource::<ServerState>();
        app.init_resource::<Time>();
        app.add_systems(Update, tick_server::<T>);
        app
    }

    /// Ephemeral (no persistence) variant of [`new_test_app_with`], for tests
    /// that don't care about disk state.
    fn new_test_app<T: ServerTransport>(transport: T, seed: u64) -> App {
        new_test_app_with(transport, seed, Persistence::new(None, 10.0))
    }

    /// Regression test for the starvation bug: a flooding client must not
    /// delay another client's very first chunk by more than one tick.
    #[test]
    fn round_robin_prevents_starvation() {
        let mut app = new_test_app(MockTransport::default(), 0);

        const CLIENT_A: ClientId = 1;
        const CLIENT_B: ClientId = 2;

        // Client A's initial view-distance burst: far more positions than
        // CHUNK_SEND_BUDGET, all queued before B is even in the picture.
        let flood: Vec<IVec3> = (0..200).map(|i| IVec3::new(i, 0, 0)).collect();
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(CLIENT_A, ClientToServer::RequestChunks { positions: flood });
        app.update();

        // B's request arrives only after A's burst is already queued -- the
        // ordinary case of two players joining around the same time.
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(
                CLIENT_B,
                ClientToServer::RequestChunks {
                    positions: vec![IVec3::new(0, 0, 0)],
                },
            );
        app.update();

        let transport = &app.world().resource::<TransportRes<MockTransport>>().0;
        let b_received = transport.outgoing.get(&CLIENT_B).map(Vec::len).unwrap_or(0);
        assert_eq!(
            b_received, 1,
            "client B must receive its chunk on the same tick as its request, \
             not after client A's entire backlog drains"
        );
    }

    /// A single `RequestChunks` message must not enqueue more than
    /// `MAX_CHUNK_REQUESTS_PER_MESSAGE` positions, regardless of how many it
    /// contains.
    #[test]
    fn request_chunks_message_is_capped() {
        let mut app = new_test_app(MockTransport::default(), 0);

        const CLIENT: ClientId = 1;
        let oversized: Vec<IVec3> = (0..10_000).map(|i| IVec3::new(i, 0, 0)).collect();
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(
                CLIENT,
                ClientToServer::RequestChunks {
                    positions: oversized,
                },
            );
        app.update();

        let state = app.world().resource::<ServerState>();
        let queued: usize = state.pending.values().map(VecDeque::len).sum();
        let served = app
            .world()
            .resource::<TransportRes<MockTransport>>()
            .0
            .outgoing
            .get(&CLIENT)
            .map(Vec::len)
            .unwrap_or(0);
        assert_eq!(
            queued + served,
            MAX_CHUNK_REQUESTS_PER_MESSAGE,
            "oversized RequestChunks message was not capped"
        );
    }

    fn recv_within(client: &mut impl ClientTransport, timeout: Duration) -> Option<ServerToClient> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(msg) = client.try_recv() {
                return Some(msg);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn hello_and_chunk_requests() {
        let (server_transport, mut client) = pair();

        // The server exits once its last client says Goodbye, but this test
        // never sends one; leaking the thread is fine.
        thread::spawn(move || {
            run_server(
                server_transport,
                ServerConfig {
                    seed: 42,
                    tick_hz: 60.0,
                    ..Default::default()
                },
            );
        });

        client.send(ClientToServer::Hello {
            name: "tester".into(),
        });

        let welcome =
            recv_within(&mut client, Duration::from_secs(5)).expect("expected a Welcome reply");
        match welcome {
            ServerToClient::Welcome { client_id, player } => {
                assert_eq!(client_id, LOCAL_CLIENT_ID);
                assert_eq!(
                    player, None,
                    "fresh ephemeral server must have no saved player"
                );
            }
            other => panic!("expected Welcome, got {other:?}"),
        }

        let valid_a = IVec3::new(0, 0, 0);
        let valid_b = IVec3::new(1, 1, -1);
        let out_of_bounds = IVec3::new(0, WORLD_HEIGHT_CHUNKS, 0);

        client.send(ClientToServer::RequestChunks {
            positions: vec![valid_a, valid_b, out_of_bounds],
        });

        let mut received = HashSet::new();
        while received.len() < 2 {
            match recv_within(&mut client, Duration::from_secs(5)) {
                Some(ServerToClient::ChunkData { pos, .. }) => {
                    assert!(
                        pos == valid_a || pos == valid_b,
                        "received unexpected chunk position {pos:?}"
                    );
                    received.insert(pos);
                }
                Some(other) => panic!("expected ChunkData, got {other:?}"),
                None => panic!("timed out waiting for chunk data"),
            }
        }
        assert_eq!(received.len(), 2);

        // The out-of-bounds position must never arrive.
        assert!(recv_within(&mut client, Duration::from_millis(200)).is_none());

        // Re-requesting an already-sent chunk is honored again: a client
        // that despawned and forgot a chunk beyond its view distance, then
        // walked back and re-requested it, must be served from cache rather
        // than silently ignored (see `rerequest_after_forget_is_served`).
        client.send(ClientToServer::RequestChunks {
            positions: vec![valid_a],
        });
        match recv_within(&mut client, Duration::from_millis(500)) {
            Some(ServerToClient::ChunkData { pos, .. }) => assert_eq!(pos, valid_a),
            other => panic!("expected the re-requested chunk to be served again, got {other:?}"),
        }
    }

    /// A chunk column at chunk-Y 3 (world-space Y 96..127) is always pure air
    /// under the prototype worldgen recipe, for every seed: max terrain
    /// height is `BASE_HEIGHT + HEIGHT_AMPLITUDE` = 64, and `column_block`
    /// only ever places non-air blocks at or below the surface (see
    /// `tsumiki_world::worldgen`, and its `high_altitude_chunk_is_air` test).
    /// Editing a block there is therefore guaranteed to be a real change,
    /// regardless of seed or (x, z).
    fn guaranteed_air_edit(chunk_x: i32, chunk_z: i32) -> (IVec3, IVec3) {
        let chunk_pos = IVec3::new(chunk_x, 3, chunk_z);
        let edit_pos = IVec3::new(
            chunk_pos.x * CHUNK_SIZE as i32 + 5,
            chunk_pos.y * CHUNK_SIZE as i32 + 5,
            chunk_pos.z * CHUNK_SIZE as i32 + 5,
        );
        (chunk_pos, edit_pos)
    }

    fn sample_chunk(chunk: &Chunk) -> Vec<BlockId> {
        let mut out = Vec::with_capacity(CHUNK_SIZE.pow(3));
        for y in 0..CHUNK_SIZE {
            for z in 0..CHUNK_SIZE {
                for x in 0..CHUNK_SIZE {
                    out.push(chunk.get(UVec3::new(x as u32, y as u32, z as u32)));
                }
            }
        }
        out
    }

    #[test]
    fn set_block_edit_is_visible_on_reload_and_broadcast_to_all_known_clients() {
        let mut app = new_test_app(MockTransport::default(), 0);

        const CLIENT_A: ClientId = 1;
        const CLIENT_B: ClientId = 2;
        const CLIENT_C: ClientId = 3;

        // A and B both say hello first, so they're registered broadcast
        // targets even though neither has requested a chunk yet.
        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            transport
                .0
                .push(CLIENT_A, ClientToServer::Hello { name: "a".into() });
            transport
                .0
                .push(CLIENT_B, ClientToServer::Hello { name: "b".into() });
        }
        app.update();

        let (chunk_pos, edit_pos) = guaranteed_air_edit(0, 0);
        let new_block = BlockId(1);
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(
                CLIENT_A,
                ClientToServer::SetBlock {
                    pos: edit_pos,
                    block: new_block,
                },
            );
        app.update();

        {
            let transport = &app.world().resource::<TransportRes<MockTransport>>().0;
            for client in [CLIENT_A, CLIENT_B] {
                let msgs = transport
                    .outgoing
                    .get(&client)
                    .unwrap_or_else(|| panic!("client {client} should have received messages"));
                let got_it = msgs.iter().any(|m| {
                    matches!(
                        m,
                        ServerToClient::BlockChanged { pos, block }
                            if *pos == edit_pos && *block == new_block
                    )
                });
                assert!(got_it, "client {client} did not receive BlockChanged");
            }
        }

        // A fresh client requesting the same chunk sees the edit baked in.
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(
                CLIENT_C,
                ClientToServer::RequestChunks {
                    positions: vec![chunk_pos],
                },
            );
        app.update();

        let transport = &app.world().resource::<TransportRes<MockTransport>>().0;
        let chunk = transport
            .outgoing
            .get(&CLIENT_C)
            .into_iter()
            .flatten()
            .find_map(|m| match m {
                ServerToClient::ChunkData { pos, chunk } if *pos == chunk_pos => Some(chunk),
                _ => None,
            })
            .expect("expected ChunkData for the edited chunk");

        let (_, local) = split_block_pos(edit_pos);
        let local = UVec3::new(local.x as u32, local.y as u32, local.z as u32);
        assert_eq!(chunk.get(local), new_block);
    }

    #[test]
    fn set_block_rejects_out_of_bounds_y_and_invalid_block_without_broadcast_or_panic() {
        let mut app = new_test_app(MockTransport::default(), 0);

        const CLIENT: ClientId = 1;
        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            transport.0.push(
                CLIENT,
                ClientToServer::Hello {
                    name: "solo".into(),
                },
            );
        }
        app.update();

        let invalid_block = BlockId(u16::MAX); // far past the prototype registry's length
        let below_bounds = ClientToServer::SetBlock {
            pos: IVec3::new(0, -1, 0),
            block: BlockId(1),
        };
        let above_bounds = ClientToServer::SetBlock {
            pos: IVec3::new(0, WORLD_HEIGHT_BLOCKS, 0),
            block: BlockId(1),
        };
        let bad_block = ClientToServer::SetBlock {
            pos: IVec3::new(0, 10, 0),
            block: invalid_block,
        };

        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            transport.0.push(CLIENT, below_bounds);
            transport.0.push(CLIENT, above_bounds);
            transport.0.push(CLIENT, bad_block);
        }
        // Must not panic.
        app.update();

        let transport = &app.world().resource::<TransportRes<MockTransport>>().0;
        let msgs = transport
            .outgoing
            .get(&CLIENT)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, ServerToClient::BlockChanged { .. })),
            "invalid SetBlock requests must never broadcast a change"
        );
    }

    /// Blocks until `handle`'s thread finishes, or panics after `timeout`.
    /// Used to confirm `run_server` returns on its own once its last client
    /// says `Goodbye`, instead of leaking the thread as other tests do.
    fn join_within(handle: thread::JoinHandle<()>, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                panic!("server thread did not exit within {timeout:?}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        handle.join().expect("server thread panicked");
    }

    #[test]
    fn persistence_roundtrip_across_restart() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let world_dir = dir.path().to_path_buf();
        let (target_chunk, edit_pos) = guaranteed_air_edit(2, -1);
        let saved_player = PlayerSave {
            pos: Vec3::new(3.0, 70.0, -8.0),
            yaw: 1.0,
            pitch: -0.5,
        };

        // --- Session 1: edit a block, save a player, then disconnect. ---
        {
            let (server_transport, mut client) = pair();
            let handle = thread::spawn({
                let world_dir = world_dir.clone();
                move || {
                    run_server(
                        server_transport,
                        ServerConfig {
                            seed: 7,
                            tick_hz: 60.0,
                            world_dir: Some(world_dir),
                            autosave_interval_secs: 9999.0,
                        },
                    );
                }
            });

            client.send(ClientToServer::Hello { name: "p1".into() });
            recv_within(&mut client, Duration::from_secs(5)).expect("expected Welcome");

            client.send(ClientToServer::RequestChunks {
                positions: vec![target_chunk],
            });
            recv_within(&mut client, Duration::from_secs(5)).expect("expected ChunkData");

            client.send(ClientToServer::SetBlock {
                pos: edit_pos,
                block: BlockId(1),
            });
            match recv_within(&mut client, Duration::from_secs(5)) {
                Some(ServerToClient::BlockChanged { pos, block }) => {
                    assert_eq!(pos, edit_pos);
                    assert_eq!(block, BlockId(1));
                }
                other => panic!("expected BlockChanged, got {other:?}"),
            }

            client.send(ClientToServer::UpdatePlayer(saved_player));
            client.send(ClientToServer::Goodbye);

            join_within(handle, Duration::from_secs(5));
        }

        // Only the region containing the edited chunk should exist on disk.
        let region = (target_chunk.x.div_euclid(8), target_chunk.z.div_euclid(8));
        let expected_region_file = format!("r.{}.{}.bin", region.0, region.1);
        let region_files: Vec<String> = fs::read_dir(world_dir.join("regions"))
            .expect("regions dir should exist after a dirty save")
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            region_files,
            vec![expected_region_file],
            "only the region containing the edited chunk should have been written"
        );

        // --- Session 2: restart on the same directory; everything comes back. ---
        {
            let (server_transport, mut client) = pair();
            let handle = thread::spawn({
                let world_dir = world_dir.clone();
                move || {
                    run_server(
                        server_transport,
                        ServerConfig {
                            seed: 7,
                            tick_hz: 60.0,
                            world_dir: Some(world_dir),
                            autosave_interval_secs: 9999.0,
                        },
                    );
                }
            });

            client.send(ClientToServer::Hello { name: "p1".into() });
            match recv_within(&mut client, Duration::from_secs(5)) {
                Some(ServerToClient::Welcome { player, .. }) => {
                    assert_eq!(player, Some(saved_player), "expected the saved player back");
                }
                other => panic!("expected Welcome, got {other:?}"),
            }

            client.send(ClientToServer::RequestChunks {
                positions: vec![target_chunk],
            });
            match recv_within(&mut client, Duration::from_secs(5)) {
                Some(ServerToClient::ChunkData { pos, chunk }) => {
                    assert_eq!(pos, target_chunk);
                    let (_, local) = split_block_pos(edit_pos);
                    let local = UVec3::new(local.x as u32, local.y as u32, local.z as u32);
                    assert_eq!(
                        chunk.get(local),
                        BlockId(1),
                        "edited block did not survive the restart"
                    );
                }
                other => panic!("expected ChunkData, got {other:?}"),
            }

            client.send(ClientToServer::Goodbye);
            join_within(handle, Duration::from_secs(5));
        }
    }

    #[test]
    fn seed_authority_saved_seed_overrides_config_seed_on_restart() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let world_dir = dir.path().to_path_buf();
        // An ordinary ground-level chunk, so its terrain actually varies by
        // seed (unlike the guaranteed-air high-altitude chunks used above).
        let untouched_chunk = IVec3::new(0, 1, 0);

        let sample_from_seed = |seed: u64, world_dir: PathBuf| -> Vec<BlockId> {
            let (server_transport, mut client) = pair();
            let handle = thread::spawn(move || {
                run_server(
                    server_transport,
                    ServerConfig {
                        seed,
                        tick_hz: 60.0,
                        world_dir: Some(world_dir),
                        autosave_interval_secs: 9999.0,
                    },
                );
            });

            client.send(ClientToServer::Hello {
                name: "solo".into(),
            });
            recv_within(&mut client, Duration::from_secs(5)).expect("expected Welcome");

            client.send(ClientToServer::RequestChunks {
                positions: vec![untouched_chunk],
            });
            let chunk = match recv_within(&mut client, Duration::from_secs(5)) {
                Some(ServerToClient::ChunkData { chunk, .. }) => chunk,
                other => panic!("expected ChunkData, got {other:?}"),
            };

            // Nothing was edited, but Goodbye must still persist the seed
            // itself (via meta.bin) so the next session can honor it.
            client.send(ClientToServer::Goodbye);
            join_within(handle, Duration::from_secs(5));

            sample_chunk(&chunk)
        };

        let first = sample_from_seed(100, world_dir.clone());
        let second = sample_from_seed(999, world_dir);

        assert_eq!(
            first, second,
            "restarting with a different config.seed must still generate from the saved world seed"
        );
    }

    #[test]
    fn transport_pump_ticked_and_flushed_each_server_tick() {
        let mut app = new_test_app(MockTransport::default(), 0);

        app.update();
        app.update();
        app.update();

        let transport = &app.world().resource::<TransportRes<MockTransport>>().0;
        assert_eq!(
            transport.tick_calls, 3,
            "tick() must run once per server tick"
        );
        assert_eq!(
            transport.flush_calls, 3,
            "flush() must run once per server tick"
        );
    }

    fn player_save(x: f32, z: f32) -> PlayerSave {
        PlayerSave {
            pos: Vec3::new(x, 70.0, z),
            yaw: 0.0,
            pitch: 0.0,
        }
    }

    #[test]
    fn interest_join_move_leave() {
        let mut app = new_test_app(MockTransport::default(), 0);

        const A: ClientId = 1;
        const B: ClientId = 2;
        const C: ClientId = 3;

        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            transport
                .0
                .push(A, ClientToServer::Hello { name: "a".into() });
            transport
                .0
                .push(B, ClientToServer::Hello { name: "b".into() });
            transport
                .0
                .push(C, ClientToServer::Hello { name: "c".into() });
            // A and B are within INTEREST_RADIUS of each other; C is far away.
            transport
                .0
                .push(A, ClientToServer::UpdatePlayer(player_save(0.0, 0.0)));
            transport
                .0
                .push(B, ClientToServer::UpdatePlayer(player_save(100.0, 0.0)));
            transport
                .0
                .push(C, ClientToServer::UpdatePlayer(player_save(10_000.0, 0.0)));
        }
        app.update();

        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            let a_msgs = transport.0.take(A);
            assert!(
                a_msgs.iter().any(
                    |m| matches!(m, ServerToClient::PlayerJoined { id, name, .. } if *id == B && name == "b")
                ),
                "A should see B join: {a_msgs:?}"
            );
            let b_msgs = transport.0.take(B);
            assert!(
                b_msgs.iter().any(
                    |m| matches!(m, ServerToClient::PlayerJoined { id, name, .. } if *id == A && name == "a")
                ),
                "B should see A join: {b_msgs:?}"
            );
            let c_msgs = transport.0.take(C);
            assert!(
                !c_msgs.iter().any(|m| matches!(
                    m,
                    ServerToClient::PlayerJoined { .. } | ServerToClient::PlayerLeft { .. }
                )),
                "C is out of range and should see no join/leave: {c_msgs:?}"
            );
        }

        // B moves, still within range: A gets PlayerMoved.
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(B, ClientToServer::UpdatePlayer(player_save(120.0, 0.0)));
        app.update();
        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            let a_msgs = transport.0.take(A);
            assert!(
                a_msgs
                    .iter()
                    .any(|m| matches!(m, ServerToClient::PlayerMoved { id, .. } if *id == B)),
                "A should receive PlayerMoved when B moves within range: {a_msgs:?}"
            );
        }

        // B walks far away: A gets PlayerLeft.
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(B, ClientToServer::UpdatePlayer(player_save(10_000.0, 0.0)));
        app.update();
        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            let a_msgs = transport.0.take(A);
            assert!(
                a_msgs
                    .iter()
                    .any(|m| matches!(m, ServerToClient::PlayerLeft { id } if *id == B)),
                "A should receive PlayerLeft when B leaves range: {a_msgs:?}"
            );
        }

        // B walks back into range: A gets PlayerJoined again.
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(B, ClientToServer::UpdatePlayer(player_save(100.0, 0.0)));
        app.update();
        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            let a_msgs = transport.0.take(A);
            assert!(
                a_msgs
                    .iter()
                    .any(|m| matches!(m, ServerToClient::PlayerJoined { id, .. } if *id == B)),
                "A should receive PlayerJoined again when B re-enters range: {a_msgs:?}"
            );
        }
    }

    #[test]
    fn moved_not_echoed_to_self() {
        let mut app = new_test_app(MockTransport::default(), 0);

        const A: ClientId = 1;
        const B: ClientId = 2;

        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            transport
                .0
                .push(A, ClientToServer::Hello { name: "a".into() });
            transport
                .0
                .push(B, ClientToServer::Hello { name: "b".into() });
            transport
                .0
                .push(A, ClientToServer::UpdatePlayer(player_save(0.0, 0.0)));
            transport
                .0
                .push(B, ClientToServer::UpdatePlayer(player_save(10.0, 0.0)));
        }
        app.update();

        // A moves again once both are already visible to each other.
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(A, ClientToServer::UpdatePlayer(player_save(1.0, 0.0)));
        app.update();

        let transport = &app.world().resource::<TransportRes<MockTransport>>().0;
        for &(id, other) in &[(A, B), (B, A)] {
            let msgs = transport.outgoing.get(&id).cloned().unwrap_or_default();
            for m in &msgs {
                match m {
                    ServerToClient::PlayerJoined { id: subject, .. } => {
                        assert_ne!(
                            *subject, id,
                            "client {id} received PlayerJoined about itself"
                        );
                    }
                    ServerToClient::PlayerMoved { id: subject, .. } => {
                        assert_ne!(
                            *subject, id,
                            "client {id} received PlayerMoved about itself"
                        );
                    }
                    _ => {}
                }
            }
            let _ = other;
        }
    }

    #[test]
    fn disconnect_broadcasts_left_once() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let mut app = new_test_app_with(
            MockTransport::default(),
            0,
            Persistence::new(Some(dir.path().to_path_buf()), 9999.0),
        );

        const A: ClientId = 1;
        const B: ClientId = 2;
        let b_save = player_save(5.0, 5.0);

        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            transport
                .0
                .push(A, ClientToServer::Hello { name: "a".into() });
            transport
                .0
                .push(B, ClientToServer::Hello { name: "b".into() });
            transport
                .0
                .push(A, ClientToServer::UpdatePlayer(player_save(0.0, 0.0)));
            transport.0.push(B, ClientToServer::UpdatePlayer(b_save));
        }
        app.update();
        // Sanity: A actually sees B before it disconnects.
        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            let a_msgs = transport.0.take(A);
            assert!(
                a_msgs
                    .iter()
                    .any(|m| matches!(m, ServerToClient::PlayerJoined { id, .. } if *id == B))
            );
        }

        // Two Goodbyes for B in the same tick: an explicit one plus a
        // transport-synthesized one racing/duplicating it.
        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            transport.0.push(B, ClientToServer::Goodbye);
            transport.0.push(B, ClientToServer::Goodbye);
        }
        app.update();

        let transport = &app.world().resource::<TransportRes<MockTransport>>().0;
        let a_msgs = transport.outgoing.get(&A).cloned().unwrap_or_default();
        let left_count = a_msgs
            .iter()
            .filter(|m| matches!(m, ServerToClient::PlayerLeft { id } if *id == B))
            .count();
        assert_eq!(
            left_count, 1,
            "A must get exactly one PlayerLeft for B: {a_msgs:?}"
        );

        // B's state was saved under its name.
        let mut reload = Persistence::new(Some(dir.path().to_path_buf()), 9999.0);
        let loaded = reload
            .load()
            .expect("failed to reload persisted world")
            .expect("expected a saved world");
        assert_eq!(loaded.players.get("b"), Some(&b_save));
    }

    #[test]
    fn per_name_persistence() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let world_dir = dir.path().to_path_buf();

        let alice_save = PlayerSave {
            pos: Vec3::new(1.0, 65.0, 2.0),
            yaw: 0.1,
            pitch: 0.0,
        };
        let bob_save = PlayerSave {
            pos: Vec3::new(-5.0, 68.0, 9.0),
            yaw: 2.0,
            pitch: 0.3,
        };

        let save_session = |name: &str, save: PlayerSave, world_dir: PathBuf| {
            let (server_transport, mut client) = pair();
            let handle = thread::spawn({
                let world_dir = world_dir.clone();
                move || {
                    run_server(
                        server_transport,
                        ServerConfig {
                            seed: 1,
                            tick_hz: 60.0,
                            world_dir: Some(world_dir),
                            autosave_interval_secs: 9999.0,
                        },
                    );
                }
            });
            client.send(ClientToServer::Hello { name: name.into() });
            recv_within(&mut client, Duration::from_secs(5)).expect("expected Welcome");
            client.send(ClientToServer::UpdatePlayer(save));
            client.send(ClientToServer::Goodbye);
            join_within(handle, Duration::from_secs(5));
        };

        save_session("alice", alice_save, world_dir.clone());
        save_session("bob", bob_save, world_dir.clone());

        // Each name gets its own state back, independent of the other.
        for (name, expected) in [("alice", alice_save), ("bob", bob_save)] {
            let (server_transport, mut client) = pair();
            let handle = thread::spawn({
                let world_dir = world_dir.clone();
                move || {
                    run_server(
                        server_transport,
                        ServerConfig {
                            seed: 1,
                            tick_hz: 60.0,
                            world_dir: Some(world_dir),
                            autosave_interval_secs: 9999.0,
                        },
                    );
                }
            });
            client.send(ClientToServer::Hello { name: name.into() });
            match recv_within(&mut client, Duration::from_secs(5)) {
                Some(ServerToClient::Welcome { player, .. }) => {
                    assert_eq!(
                        player,
                        Some(expected),
                        "{name} did not get its own saved state back"
                    );
                }
                other => panic!("expected Welcome, got {other:?}"),
            }
            client.send(ClientToServer::Goodbye);
            join_within(handle, Duration::from_secs(5));
        }

        // A v1 meta.bin (single global player slot) migrates its player into
        // players["player"] on load. Constructed by hand with a local struct
        // matching the legacy layout -- postcard's wire format only depends
        // on field order/types, not the Rust type name, so this is a faithful
        // black-box test of the on-disk format contract.
        let v1_dir = tempfile::tempdir().expect("failed to create tempdir");
        let legacy_player = PlayerSave {
            pos: Vec3::new(3.0, 70.0, 1.0),
            yaw: 0.0,
            pitch: 0.0,
        };
        #[derive(serde::Serialize)]
        struct LegacyMeta {
            version: u32,
            seed: u64,
            player: Option<PlayerSave>,
        }
        let legacy = LegacyMeta {
            version: 1,
            seed: 42,
            player: Some(legacy_player),
        };
        let bytes = postcard::to_allocvec(&legacy).expect("failed to encode legacy meta.bin");
        fs::write(v1_dir.path().join("meta.bin"), bytes).expect("failed to write legacy meta.bin");

        let (server_transport, mut client) = pair();
        let handle = thread::spawn({
            let world_dir = v1_dir.path().to_path_buf();
            move || {
                run_server(
                    server_transport,
                    ServerConfig {
                        seed: 42,
                        tick_hz: 60.0,
                        world_dir: Some(world_dir),
                        autosave_interval_secs: 9999.0,
                    },
                );
            }
        });
        client.send(ClientToServer::Hello {
            name: "player".into(),
        });
        match recv_within(&mut client, Duration::from_secs(5)) {
            Some(ServerToClient::Welcome { player, .. }) => {
                assert_eq!(
                    player,
                    Some(legacy_player),
                    "v1 meta.bin's player did not migrate to players[\"player\"]"
                );
            }
            other => panic!("expected Welcome, got {other:?}"),
        }
        client.send(ClientToServer::Goodbye);
        join_within(handle, Duration::from_secs(5));
    }

    #[test]
    fn rerequest_after_forget_is_served() {
        let mut app = new_test_app(MockTransport::default(), 0);

        const CLIENT: ClientId = 1;
        let pos = IVec3::new(5, 0, -3);

        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(
                CLIENT,
                ClientToServer::RequestChunks {
                    positions: vec![pos],
                },
            );
        app.update();
        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            let msgs = transport.0.take(CLIENT);
            assert!(
                msgs.iter()
                    .any(|m| matches!(m, ServerToClient::ChunkData { pos: p, .. } if *p == pos)),
                "expected the chunk to be served the first time: {msgs:?}"
            );
        }

        // The client despawned and forgot this chunk (walked beyond view
        // distance) and now re-requests it after walking back.
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(
                CLIENT,
                ClientToServer::RequestChunks {
                    positions: vec![pos],
                },
            );
        app.update();

        let transport = &app.world().resource::<TransportRes<MockTransport>>().0;
        let msgs = transport.outgoing.get(&CLIENT).cloned().unwrap_or_default();
        assert!(
            msgs.iter()
                .any(|m| matches!(m, ServerToClient::ChunkData { pos: p, .. } if *p == pos)),
            "re-requested chunk was not served again (sent-set regression): {msgs:?}"
        );
    }

    #[test]
    fn lod_request_served() {
        let mut app = new_test_app(MockTransport::default(), 0);

        const CLIENT: ClientId = 1;
        let pos = IVec3::new(0, 0, 0);

        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(
                CLIENT,
                ClientToServer::RequestLodChunks {
                    level: 1,
                    positions: vec![pos],
                },
            );
        app.update();
        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            let msgs = transport.0.take(CLIENT);
            assert!(
                msgs.iter().any(|m| matches!(
                    m,
                    ServerToClient::LodChunkData { level, pos: p, .. } if *level == 1 && *p == pos
                )),
                "expected a LodChunkData for level 1, pos {pos:?}: {msgs:?}"
            );
        }

        // Unlike level-0 chunks, a re-request is served again as normal
        // (from cache, since nothing invalidated it).
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(
                CLIENT,
                ClientToServer::RequestLodChunks {
                    level: 1,
                    positions: vec![pos],
                },
            );
        app.update();
        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            let msgs = transport.0.take(CLIENT);
            assert!(
                msgs.iter().any(|m| matches!(
                    m,
                    ServerToClient::LodChunkData { level, pos: p, .. } if *level == 1 && *p == pos
                )),
                "re-requested LOD chunk was not served again: {msgs:?}"
            );
        }

        // Invalid level (0, and one past MAX_LOD) and an out-of-range y are
        // all silently dropped: no LodChunkData for any of them.
        let bad_y = lod::world_height_lod_chunks(1);
        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            transport.0.push(
                CLIENT,
                ClientToServer::RequestLodChunks {
                    level: 0,
                    positions: vec![IVec3::new(1, 0, 0)],
                },
            );
            transport.0.push(
                CLIENT,
                ClientToServer::RequestLodChunks {
                    level: MAX_LOD + 1,
                    positions: vec![IVec3::new(1, 0, 0)],
                },
            );
            transport.0.push(
                CLIENT,
                ClientToServer::RequestLodChunks {
                    level: 1,
                    positions: vec![IVec3::new(0, bad_y, 0)],
                },
            );
        }
        app.update();

        let transport = &app.world().resource::<TransportRes<MockTransport>>().0;
        let msgs = transport.outgoing.get(&CLIENT).cloned().unwrap_or_default();
        assert!(
            msgs.is_empty(),
            "invalid level / out-of-range y requests must be silently dropped: {msgs:?}"
        );
    }

    #[test]
    fn lod_reflects_edits() {
        let mut app = new_test_app(MockTransport::default(), 0);
        const CLIENT: ClientId = 1;

        // A guaranteed-air chunk (see `guaranteed_air_edit`) with a 2x2x2
        // stone cube aligned to a level-1 cell (cell size = 2 blocks), so the
        // majority rule guarantees that cell is stone in the LOD overlay.
        let chunk_pos = IVec3::new(0, 3, 0);
        let base_local = UVec3::new(4, 4, 4);
        let cube_positions: Vec<IVec3> = (0..2)
            .flat_map(|dx| (0..2).flat_map(move |dy| (0..2).map(move |dz| (dx, dy, dz))))
            .map(|(dx, dy, dz): (i32, i32, i32)| {
                IVec3::new(
                    chunk_pos.x * CHUNK_SIZE as i32 + base_local.x as i32 + dx,
                    chunk_pos.y * CHUNK_SIZE as i32 + base_local.y as i32 + dy,
                    chunk_pos.z * CHUNK_SIZE as i32 + base_local.z as i32 + dz,
                )
            })
            .collect();

        let level = 1u8;
        let lod_pos = lod::lod_pos_of_chunk(level, chunk_pos);
        let scale = 1i32 << level;
        let offset_in_footprint = chunk_pos - lod_pos * scale;
        let sub_block_cells = CHUNK_SIZE as i32 / scale;
        let cell_local = base_local / (lod::cell_size(level) as u32);
        let expected_cell = UVec3::new(
            (offset_in_footprint.x * sub_block_cells) as u32 + cell_local.x,
            (offset_in_footprint.y * sub_block_cells) as u32 + cell_local.y,
            (offset_in_footprint.z * sub_block_cells) as u32 + cell_local.z,
        );

        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            for &p in &cube_positions {
                transport.0.push(
                    CLIENT,
                    ClientToServer::SetBlock {
                        pos: p,
                        block: BlockId(1),
                    },
                );
            }
        }
        app.update();
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);

        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(
                CLIENT,
                ClientToServer::RequestLodChunks {
                    level,
                    positions: vec![lod_pos],
                },
            );
        app.update();
        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            let msgs = transport.0.take(CLIENT);
            let chunk = msgs
                .iter()
                .find_map(|m| match m {
                    ServerToClient::LodChunkData {
                        level: l,
                        pos: p,
                        chunk,
                    } if *l == level && *p == lod_pos => Some(chunk),
                    _ => None,
                })
                .expect("expected LodChunkData for the covering LOD chunk");
            assert_eq!(
                chunk.get(expected_cell),
                BlockId(1),
                "overlaid LOD cell did not reflect the edit"
            );
        }

        // A further edit inside the same level-0 chunk must invalidate the
        // LOD cache entry and push an unsolicited rebuilt re-send to this
        // client, since it was already sent that LOD chunk above.
        let other_pos = IVec3::new(
            chunk_pos.x * CHUNK_SIZE as i32 + 6,
            chunk_pos.y * CHUNK_SIZE as i32 + 6,
            chunk_pos.z * CHUNK_SIZE as i32 + 6,
        );
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(
                CLIENT,
                ClientToServer::SetBlock {
                    pos: other_pos,
                    block: BlockId(1),
                },
            );
        app.update();
        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            let msgs = transport.0.take(CLIENT);
            let resends = msgs
                .iter()
                .filter(|m| {
                    matches!(
                        m,
                        ServerToClient::LodChunkData { level: l, pos: p, .. }
                            if *l == level && *p == lod_pos
                    )
                })
                .count();
            assert_eq!(
                resends, 1,
                "expected exactly one unsolicited LOD re-send: {msgs:?}"
            );
        }

        // A burst of several more edits to the same chunk before the next
        // serve must still queue only one re-send (dedup against `pending`).
        let burst_positions = [
            IVec3::new(
                chunk_pos.x * CHUNK_SIZE as i32 + 7,
                chunk_pos.y * CHUNK_SIZE as i32 + 6,
                chunk_pos.z * CHUNK_SIZE as i32 + 6,
            ),
            IVec3::new(
                chunk_pos.x * CHUNK_SIZE as i32 + 8,
                chunk_pos.y * CHUNK_SIZE as i32 + 6,
                chunk_pos.z * CHUNK_SIZE as i32 + 6,
            ),
            IVec3::new(
                chunk_pos.x * CHUNK_SIZE as i32 + 9,
                chunk_pos.y * CHUNK_SIZE as i32 + 6,
                chunk_pos.z * CHUNK_SIZE as i32 + 6,
            ),
        ];
        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            for &p in &burst_positions {
                transport.0.push(
                    CLIENT,
                    ClientToServer::SetBlock {
                        pos: p,
                        block: BlockId(1),
                    },
                );
            }
        }
        app.update();

        let transport = &app.world().resource::<TransportRes<MockTransport>>().0;
        let msgs = transport.outgoing.get(&CLIENT).cloned().unwrap_or_default();
        let resends = msgs
            .iter()
            .filter(|m| {
                matches!(
                    m,
                    ServerToClient::LodChunkData { level: l, pos: p, .. }
                        if *l == level && *p == lod_pos
                )
            })
            .count();
        assert_eq!(
            resends, 1,
            "a burst of edits before the next serve must queue only one re-send: {msgs:?}"
        );
    }

    /// A flood of mixed chunk + LOD requests must never exceed
    /// `CHUNK_SEND_BUDGET` sends in a single tick, since both request kinds
    /// share one queue and one budget.
    #[test]
    fn budget_shared() {
        let mut app = new_test_app(MockTransport::default(), 0);
        const CLIENT: ClientId = 1;

        let chunk_positions: Vec<IVec3> = (0..100).map(|i| IVec3::new(i, 0, 0)).collect();
        let lod_positions: Vec<IVec3> = (0..100).map(|i| IVec3::new(i, 1, 0)).collect();

        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            transport.0.push(
                CLIENT,
                ClientToServer::RequestChunks {
                    positions: chunk_positions,
                },
            );
            transport.0.push(
                CLIENT,
                ClientToServer::RequestLodChunks {
                    level: 1,
                    positions: lod_positions,
                },
            );
        }

        for _ in 0..8 {
            app.update();
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            let sent = transport.0.take(CLIENT).len();
            assert!(
                sent <= CHUNK_SEND_BUDGET,
                "a tick sent {sent} messages, exceeding the shared CHUNK_SEND_BUDGET of {CHUNK_SEND_BUDGET}"
            );
        }
    }

    /// Bounded memory (doc/roadmap.md M3): pristine level-0 chunks and LOD
    /// chunks are evicted LRU past their caps; modified chunks survive; and
    /// an evicted-then-re-requested chunk regenerates identical content.
    #[test]
    fn eviction() {
        let mut app = new_test_app(MockTransport::default(), 0);
        const CLIENT: ClientId = 1;

        // One modified chunk (a real edit), which must survive eviction no
        // matter how many pristine chunks pile up around it.
        let (modified_chunk, edit_pos) = guaranteed_air_edit(0, 0);
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(
                CLIENT,
                ClientToServer::SetBlock {
                    pos: edit_pos,
                    block: BlockId(1),
                },
            );
        app.update();

        // Flood the cache with far more pristine chunks than
        // `MAX_PRISTINE_CHUNKS` (overridden to a tiny value under
        // `cfg(test)`, see its doc comment).
        let flood_count = MAX_PRISTINE_CHUNKS * 3;
        let flood_positions: Vec<IVec3> = (0..flood_count as i32)
            .map(|i| IVec3::new(i + 1000, 0, 0))
            .collect();
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(
                CLIENT,
                ClientToServer::RequestChunks {
                    positions: flood_positions.clone(),
                },
            );
        for _ in 0..(flood_count / CHUNK_SEND_BUDGET + 2) {
            app.update();
        }

        {
            let cache = app.world().resource::<ChunkCache>();
            let pristine_count = cache
                .chunks
                .keys()
                .filter(|&&p| p != modified_chunk)
                .count();
            assert!(
                pristine_count <= MAX_PRISTINE_CHUNKS,
                "pristine cache was not evicted down to the cap: {pristine_count} entries"
            );
            assert!(
                cache.chunks.contains_key(&modified_chunk),
                "modified chunk must survive eviction"
            );
        }

        // A re-request of a (near-certainly evicted) flood position yields
        // content identical to a fresh generator at the same seed --
        // eviction only costs a regeneration, never correctness.
        let evicted_candidate = flood_positions[0];
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(
                CLIENT,
                ClientToServer::RequestChunks {
                    positions: vec![evicted_candidate],
                },
            );
        app.update();
        let chunk = {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            transport
                .0
                .take(CLIENT)
                .into_iter()
                .find_map(|m| match m {
                    ServerToClient::ChunkData { pos, chunk } if pos == evicted_candidate => {
                        Some(chunk)
                    }
                    _ => None,
                })
                .expect("expected the re-requested chunk to be served")
        };
        let fresh = WorldGenerator::new(0).generate_chunk(evicted_candidate);
        assert_eq!(
            sample_chunk(&chunk),
            sample_chunk(&fresh),
            "regenerated chunk after eviction must be identical (deterministic worldgen)"
        );

        // Same idea for the LOD cache: flood past `MAX_LOD_CACHE`.
        let lod_flood_count = MAX_LOD_CACHE * 3;
        let lod_positions: Vec<IVec3> = (0..lod_flood_count as i32)
            .map(|i| IVec3::new(i, 0, 0))
            .collect();
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(
                CLIENT,
                ClientToServer::RequestLodChunks {
                    level: 1,
                    positions: lod_positions,
                },
            );
        for _ in 0..(lod_flood_count / CHUNK_SEND_BUDGET + 2) {
            app.update();
        }
        let lod_cache = app.world().resource::<LodCache>();
        assert!(
            lod_cache.chunks.len() <= MAX_LOD_CACHE,
            "LOD cache was not evicted down to the cap: {} entries",
            lod_cache.chunks.len()
        );
    }
}

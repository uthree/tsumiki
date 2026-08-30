//! Headless game server (design.md §1).
//!
//! Owns the authoritative world state and serves it to clients over a
//! [`ServerTransport`]. No rendering dependencies. Runs as a headless Bevy
//! app (`MinimalPlugins` + `ScheduleRunnerPlugin` at a fixed tick).
//!
//! Per-tick work:
//! 1. Pump all pending transport messages:
//!    - `Hello` → reply `Welcome`.
//!    - `RequestChunks` → enqueue positions into that client's own queue
//!      (deduplicated against both the queue and already-sent chunks;
//!      out-of-bounds Y is ignored; a single message is capped so it cannot
//!      dominate the queue or force an unbounded insert).
//! 2. Serve up to [`CHUNK_SEND_BUDGET`] queued chunk requests, round-robin
//!    across clients so one client's backlog cannot starve another: generate
//!    the chunk if it is not already cached, cache it, and send `ChunkData`.

mod persist;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::prelude::*;

use bevy_math::{IVec3, UVec3};
use tsumiki_protocol::{ClientId, ClientToServer, PlayerSave, ServerToClient, ServerTransport};
use tsumiki_world::{
    BlockRegistry, Chunk, WORLD_HEIGHT_BLOCKS, WORLD_HEIGHT_CHUNKS, WorldGenerator, split_block_pos,
};

use persist::Persistence;

/// Maximum chunks generated + sent per tick, to keep tick times bounded.
pub const CHUNK_SEND_BUDGET: usize = 32;

/// Maximum chunk positions accepted from a single `RequestChunks` message.
/// Set with headroom above the client's own per-frame cap
/// (`MAX_CHUNK_REQUESTS_PER_FRAME = 64` in `crates/client/src/net.rs`), so a
/// legitimate client's burst always fits in one message while a malformed or
/// hostile message cannot force an unbounded synchronous insert into the
/// pending queues.
const MAX_CHUNK_REQUESTS_PER_MESSAGE: usize = 128;

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

/// The single M1 player save slot, echoed to newly-connecting clients as
/// `Welcome.player`. Per-name keying for multiple players is an M2 concern.
#[derive(Resource, Default)]
struct PlayerRes(Option<PlayerSave>);

#[derive(Resource, Default)]
struct ChunkCache(HashMap<IVec3, Chunk>);

/// Per-client bookkeeping: which chunk positions have already been sent, so a
/// repeated `RequestChunks` for the same position is a no-op.
#[derive(Default)]
struct ClientState {
    sent: HashSet<IVec3>,
}

/// Cross-client request queues and per-client sent-chunk tracking.
///
/// Requests are served round-robin across clients (see `tick_server`) so one
/// client's backlog can never starve another: `rotation` holds the client IDs
/// that currently have a non-empty `pending` queue, in service order.
#[derive(Resource, Default)]
struct ServerState {
    clients: HashMap<ClientId, ClientState>,
    /// Per-client FIFO of not-yet-served chunk positions.
    pending: HashMap<ClientId, VecDeque<IVec3>>,
    /// Mirrors `pending`'s contents for O(1) dedup checks.
    pending_set: HashSet<(ClientId, IVec3)>,
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
    let (seed, player, loaded_chunks) = match loaded {
        Some(world) => {
            if world.seed != config.seed {
                eprintln!(
                    "tsumiki-server: world_dir has saved seed {}, overriding config seed {}",
                    world.seed, config.seed
                );
            }
            (world.seed, world.player, world.chunks)
        }
        None => (config.seed, None, Vec::new()),
    };

    let mut cache = ChunkCache::default();
    for (pos, chunk) in loaded_chunks {
        cache.0.insert(pos, chunk);
    }

    app.insert_resource(WorldGenRes(WorldGenerator::new(seed)));
    app.insert_resource(WorldSeed(seed));
    app.insert_resource(BlockRegistryRes(BlockRegistry::prototype()));
    app.insert_resource(PlayerRes(player));
    app.insert_resource(cache);
    app.insert_resource(persistence);
    app.init_resource::<ServerState>();
    app.add_systems(Update, tick_server::<T>);
    app.run();
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
    mut persistence: ResMut<Persistence>,
    mut player: ResMut<PlayerRes>,
    time: Res<Time>,
    mut exit: MessageWriter<AppExit>,
) {
    let ServerState {
        clients,
        pending,
        pending_set,
        rotation,
    } = &mut *state;

    while let Some((client_id, msg)) = transport.0.try_recv() {
        match msg {
            ClientToServer::Hello { .. } => {
                // A client is "known" (a broadcast target) from Hello onward,
                // even before it ever requests a chunk.
                clients.entry(client_id).or_default();
                transport.0.send(
                    client_id,
                    ServerToClient::Welcome {
                        client_id,
                        player: player.0,
                    },
                );
            }
            ClientToServer::RequestChunks { positions } => {
                let client = clients.entry(client_id).or_default();
                let queue = pending.entry(client_id).or_default();
                let was_empty = queue.is_empty();
                for pos in positions.into_iter().take(MAX_CHUNK_REQUESTS_PER_MESSAGE) {
                    if pos.y < 0 || pos.y >= WORLD_HEIGHT_CHUNKS {
                        continue;
                    }
                    if client.sent.contains(&pos) {
                        continue;
                    }
                    let key = (client_id, pos);
                    if pending_set.insert(key) {
                        queue.push_back(pos);
                    }
                }
                if was_empty && !queue.is_empty() {
                    rotation.push_back(client_id);
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
                    .0
                    .entry(chunk_pos)
                    .or_insert_with(|| world_gen.0.generate_chunk(chunk_pos));

                if chunk.get(local) == block {
                    // No-op edit: skip silently, no broadcast.
                    continue;
                }
                chunk.set(local, block);
                persistence.mark_chunk_dirty(chunk_pos);

                for &known_client in clients.keys() {
                    transport
                        .0
                        .send(known_client, ServerToClient::BlockChanged { pos, block });
                }
            }
            ClientToServer::UpdatePlayer(save) => {
                // Single global player slot for M1 (client/server is
                // effectively singleplayer so far); per-name keying for
                // multiple persisted players is an M2 concern once real
                // multiplayer distinguishes clients by identity.
                player.0 = Some(save);
                persistence.mark_player_dirty();
            }
            ClientToServer::Goodbye => {
                persistence
                    .save(seed.0, player.0, &cache.0)
                    .expect("failed to save world on goodbye");

                clients.remove(&client_id);
                pending.remove(&client_id);
                pending_set.retain(|&(cid, _)| cid != client_id);
                rotation.retain(|&cid| cid != client_id);

                if clients.is_empty() {
                    exit.write(AppExit::Success);
                }
            }
        }
    }

    // Round-robin across clients so one client's backlog cannot starve
    // another: each iteration serves at most one position from the next
    // client in `rotation`, re-queuing that client at the back if it still
    // has more pending.
    let mut served = 0;
    while served < CHUNK_SEND_BUDGET {
        let Some(client_id) = rotation.pop_front() else {
            break;
        };
        let Some(queue) = pending.get_mut(&client_id) else {
            continue;
        };
        let Some(pos) = queue.pop_front() else {
            continue;
        };
        pending_set.remove(&(client_id, pos));

        let chunk = cache
            .0
            .entry(pos)
            .or_insert_with(|| world_gen.0.generate_chunk(pos))
            .clone();
        clients.entry(client_id).or_default().sent.insert(pos);
        transport
            .0
            .send(client_id, ServerToClient::ChunkData { pos, chunk });

        served += 1;
        if !queue.is_empty() {
            rotation.push_back(client_id);
        }
    }

    // Periodic autosave: only touches disk when the clock has crossed the
    // interval AND something actually changed since the last save.
    if persistence.autosave_due(time.delta_secs_f64()) && persistence.has_dirty() {
        persistence
            .save(seed.0, player.0, &cache.0)
            .expect("failed to autosave world");
    }
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
    /// reproduce a two-client scenario).
    #[derive(Default)]
    struct MockTransport {
        incoming: VecDeque<(ClientId, ClientToServer)>,
        outgoing: HashMap<ClientId, Vec<ServerToClient>>,
    }

    impl MockTransport {
        fn push(&mut self, client_id: ClientId, msg: ClientToServer) {
            self.incoming.push_back((client_id, msg));
        }
    }

    impl ServerTransport for MockTransport {
        fn try_recv(&mut self) -> Option<(ClientId, ClientToServer)> {
            self.incoming.pop_front()
        }

        fn send(&mut self, to: ClientId, msg: ServerToClient) {
            self.outgoing.entry(to).or_default().push(msg);
        }
    }

    /// Builds a minimal `App` wired the same way `run_server` would, but
    /// without `MinimalPlugins`/the schedule runner, so tests can drive
    /// `tick_server` tick-by-tick via `app.update()`. Ephemeral (no
    /// persistence) unless the caller inserts a `Persistence` afterwards.
    fn new_test_app<T: ServerTransport>(transport: T, seed: u64) -> App {
        let mut app = App::new();
        app.insert_resource(TransportRes(transport));
        app.insert_resource(WorldGenRes(WorldGenerator::new(seed)));
        app.insert_resource(WorldSeed(seed));
        app.insert_resource(BlockRegistryRes(BlockRegistry::prototype()));
        app.init_resource::<PlayerRes>();
        app.init_resource::<ChunkCache>();
        app.insert_resource(Persistence::new(None, 10.0));
        app.init_resource::<ServerState>();
        app.init_resource::<Time>();
        app.add_systems(Update, tick_server::<T>);
        app
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

        // Re-requesting an already-sent chunk must not resend it.
        client.send(ClientToServer::RequestChunks {
            positions: vec![valid_a],
        });
        assert!(
            recv_within(&mut client, Duration::from_millis(500)).is_none(),
            "server resent an already-sent chunk"
        );
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
}

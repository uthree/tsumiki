use super::*;
use std::fs;
use std::thread;
use std::time::Instant;

use tsumiki_protocol::ClientTransport;
use tsumiki_protocol::DamageCause;
use tsumiki_protocol::local::{LOCAL_CLIENT_ID, pair};
use tsumiki_protocol::{SlotArea, SlotRef};

use bevy_math::Vec3;
use tsumiki_world::{BlockId, CHUNK_SIZE, ItemId, ItemRegistry, items};

/// Builds a [`PlayerSave`] at an exact position (yaw/pitch zeroed -- nothing
/// here cares about facing).
fn save_at(pos: Vec3) -> PlayerSave {
    PlayerSave {
        pos,
        yaw: 0.0,
        pitch: 0.0,
    }
}

/// Builds a [`PlayerSave`] positioned at `pos`'s center, trivially within
/// [`SERVER_REACH`] of `pos` itself -- the standard way these tests put a
/// player in reach of a block they're about to edit.
fn save_near(pos: IVec3) -> PlayerSave {
    save_at(Vec3::new(
        pos.x as f32 + 0.5,
        pos.y as f32 + 0.5,
        pos.z as f32 + 0.5,
    ))
}

/// Directly seeds `block` at `pos` in the server's chunk cache, generating
/// the chunk first if needed. Used to set up a known-solid (or known-air)
/// block for a test without going through a validated edit message.
fn seed_block(app: &mut App, pos: IVec3, block: BlockId) {
    let (chunk_pos, local) = split_block_pos(pos);
    let local = UVec3::new(local.x as u32, local.y as u32, local.z as u32);
    let seed = app.world().resource::<WorldSeed>().0;
    let mut cache = app.world_mut().resource_mut::<ChunkCache>();
    let chunk = cache
        .chunks
        .entry(chunk_pos)
        .or_insert_with(|| WorldGenerator::new(seed).generate_chunk(chunk_pos));
    chunk.set(local, block);
}

/// Directly seeds `stack` into `client_id`'s main-inventory slot `index`,
/// bypassing the network protocol -- the standard way these tests give a
/// player starting items without a full break/craft chain.
fn seed_main_slot(app: &mut App, client_id: ClientId, index: usize, stack: ItemStack) {
    let mut state = app.world_mut().resource_mut::<ServerState>();
    let client = state.clients.get_mut(&client_id).unwrap();
    client.main.set_slot(index, Some(stack));
}

/// The total count of `item` across the `main` field of the most recent
/// `InventoryUpdate` among `msgs` (`None` if there is no such update at all;
/// `Some(0)` if there is one but it doesn't mention `item`).
fn latest_main_count(msgs: &[ServerToClient], item: ItemId) -> Option<u32> {
    msgs.iter().rev().find_map(|m| match m {
        ServerToClient::InventoryUpdate { main, .. } => Some(
            main.iter()
                .flatten()
                .filter(|s| s.item == item)
                .map(|s| s.count)
                .sum(),
        ),
        _ => None,
    })
}

/// The cursor field of the most recent `InventoryUpdate` among `msgs`.
fn latest_cursor(msgs: &[ServerToClient]) -> Option<Option<ItemStack>> {
    msgs.iter().rev().find_map(|m| match m {
        ServerToClient::InventoryUpdate { cursor, .. } => Some(*cursor),
        _ => None,
    })
}

/// The `craft_output` field of the most recent `InventoryUpdate` among
/// `msgs`.
fn latest_craft_output(msgs: &[ServerToClient]) -> Option<Option<ItemStack>> {
    msgs.iter().rev().find_map(|m| match m {
        ServerToClient::InventoryUpdate { craft_output, .. } => Some(*craft_output),
        _ => None,
    })
}

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
/// `Persistence` (ephemeral or backed by a real directory) and game mode.
///
/// `SimRes::tick_interval_secs` defaults to `1.0` (one tick = one simulated
/// second) rather than the real `1.0 / tick_hz`, since this harness never
/// runs a real schedule loop (`Time`'s delta stays 0 without `TimePlugin`,
/// same as it always has for autosave in these tests) -- fixed-step M4
/// timers (day cycle, regen, item pickup/expiry) are driven by this value
/// instead, so tests get deterministic, easily-reasoned-about timing just by
/// controlling tick count, and can mutate it directly for a single large
/// time jump (see `pickup_delay_and_expiry`).
fn new_test_app_with<T: ServerTransport>(
    transport: T,
    seed: u64,
    persistence: Persistence,
    mode: GameMode,
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
    app.init_resource::<CraftingRes>();
    app.insert_resource(SimRes {
        game_mode: mode,
        world_time: sim::WorldTimeRes::new(0.0),
        items: sim::ItemsRes::default(),
        clock: sim::GameClock::default(),
        tick_interval_secs: 1.0,
    });
    app.init_resource::<Time>();
    app.add_systems(Update, tick_server::<T>);
    app
}

/// Ephemeral (no persistence), Creative-mode variant of [`new_test_app_with`],
/// for tests that don't care about disk state or survival mechanics (most of
/// the pre-M4 infrastructure tests: chunk/LOD serving, interest, eviction).
fn new_test_app<T: ServerTransport>(transport: T, seed: u64) -> App {
    new_test_app_with(
        transport,
        seed,
        Persistence::new(None, 10.0),
        GameMode::Creative,
    )
}

/// Creative places always use hotbar slot 0, since the join-time prefill
/// (see `tick_server`'s `Hello` handling) puts `ItemRegistry::placeable()`'s
/// first item there -- stone, which places `BlockId(1)`, matching every
/// pre-M5 test's use of that id.
const CREATIVE_STONE_HOTBAR: u8 = 0;

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
        ServerToClient::Welcome {
            client_id,
            player,
            game_mode,
            time_of_day,
        } => {
            assert_eq!(client_id, LOCAL_CLIENT_ID);
            assert_eq!(
                player, None,
                "fresh ephemeral server must have no saved player"
            );
            assert_eq!(
                game_mode,
                GameMode::Survival,
                "a brand-new world with no configured mode defaults to Survival"
            );
            assert_eq!(time_of_day, 0.0, "a brand-new world starts at sunrise");
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
            // Survival onboarding (InventoryUpdate/HealthUpdate/ItemSpawned,
            // sent right after Welcome) can interleave with chunk delivery;
            // this test only cares about ChunkData, so anything else is
            // ignored rather than treated as a failure.
            Some(_) => {}
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
    let new_block = BlockId(1); // stone: what creative's hotbar slot 0 places.
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(CLIENT_A, ClientToServer::UpdatePlayer(save_near(edit_pos)));
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            CLIENT_A,
            ClientToServer::PlaceBlock {
                pos: edit_pos,
                hotbar: CREATIVE_STONE_HOTBAR,
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
fn edits_reject_out_of_bounds_and_malformed_input_without_broadcast_or_panic() {
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
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);

    let below_bounds = ClientToServer::PlaceBlock {
        pos: IVec3::new(0, -1, 0),
        hotbar: CREATIVE_STONE_HOTBAR,
    };
    let above_bounds = ClientToServer::PlaceBlock {
        pos: IVec3::new(0, WORLD_HEIGHT_BLOCKS, 0),
        hotbar: CREATIVE_STONE_HOTBAR,
    };
    // A hotbar index far past HOTBAR_SIZE: malformed input from an untrusted
    // client must be rejected, not panic on an out-of-range slot access.
    let bad_hotbar = ClientToServer::PlaceBlock {
        pos: IVec3::new(0, 10, 0),
        hotbar: 200,
    };

    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(CLIENT, below_bounds);
        transport.0.push(CLIENT, above_bounds);
        transport.0.push(CLIENT, bad_hotbar);
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
        "invalid PlaceBlock requests must never broadcast a change: {msgs:?}"
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
                        game_mode: Some(GameMode::Creative),
                    },
                );
            }
        });

        client.send(ClientToServer::Hello { name: "p1".into() });
        recv_within(&mut client, Duration::from_secs(5)).expect("expected Welcome");

        client.send(ClientToServer::RequestChunks {
            positions: vec![target_chunk],
        });
        loop {
            match recv_within(&mut client, Duration::from_secs(5)) {
                Some(ServerToClient::ChunkData { .. }) => break,
                Some(_) => continue,
                None => panic!("timed out waiting for ChunkData"),
            }
        }

        client.send(ClientToServer::UpdatePlayer(save_near(edit_pos)));
        client.send(ClientToServer::PlaceBlock {
            pos: edit_pos,
            hotbar: CREATIVE_STONE_HOTBAR,
        });
        loop {
            match recv_within(&mut client, Duration::from_secs(5)) {
                Some(ServerToClient::BlockChanged { pos, block }) => {
                    assert_eq!(pos, edit_pos);
                    assert_eq!(block, BlockId(1));
                    break;
                }
                Some(_) => continue,
                None => panic!("timed out waiting for BlockChanged"),
            }
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
                        game_mode: Some(GameMode::Creative),
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
        loop {
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
                    break;
                }
                Some(_) => continue,
                None => panic!("timed out waiting for ChunkData"),
            }
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
                    game_mode: None,
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
        // Survival onboarding (InventoryUpdate/HealthUpdate/ItemSpawned,
        // sent right after Welcome) can interleave with chunk delivery;
        // this test only cares about the chunk itself.
        let chunk = loop {
            match recv_within(&mut client, Duration::from_secs(5)) {
                Some(ServerToClient::ChunkData { chunk, .. }) => break chunk,
                Some(_) => continue,
                None => panic!("timed out waiting for ChunkData"),
            }
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
        GameMode::Creative,
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
    assert_eq!(loaded.players.get("b").map(|r| r.save), Some(b_save));
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
                        game_mode: None,
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
                        game_mode: None,
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
                    // Deliberately Survival, to confirm the v1-predates-modes
                    // migration to Creative overrides it.
                    game_mode: Some(GameMode::Survival),
                },
            );
        }
    });
    client.send(ClientToServer::Hello {
        name: "player".into(),
    });
    match recv_within(&mut client, Duration::from_secs(5)) {
        Some(ServerToClient::Welcome {
            player, game_mode, ..
        }) => {
            assert_eq!(
                player,
                Some(legacy_player),
                "v1 meta.bin's player did not migrate to players[\"player\"]"
            );
            assert_eq!(
                game_mode,
                GameMode::Creative,
                "a v1 (predates modes) world must migrate to Creative, \
                 overriding the config's requested Survival"
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
        transport
            .0
            .push(CLIENT, ClientToServer::Hello { name: "lod".into() });
        // Positioned near the cube (and, per the reach-distance check below,
        // still within `SERVER_REACH` of every other edit this test makes
        // in the same chunk).
        transport.0.push(
            CLIENT,
            ClientToServer::UpdatePlayer(save_near(cube_positions[0])),
        );
        for &p in &cube_positions {
            transport.0.push(
                CLIENT,
                ClientToServer::PlaceBlock {
                    pos: p,
                    hotbar: CREATIVE_STONE_HOTBAR,
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
            ClientToServer::PlaceBlock {
                pos: other_pos,
                hotbar: CREATIVE_STONE_HOTBAR,
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
                ClientToServer::PlaceBlock {
                    pos: p,
                    hotbar: CREATIVE_STONE_HOTBAR,
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
        // Only chunk/LOD-chunk sends are governed by CHUNK_SEND_BUDGET;
        // an unrelated periodic broadcast (e.g. TimeUpdate, doc/roadmap.md
        // M4) sharing the same tick is not part of what this budget bounds.
        let sent = transport
            .0
            .take(CLIENT)
            .iter()
            .filter(|m| {
                matches!(
                    m,
                    ServerToClient::ChunkData { .. } | ServerToClient::LodChunkData { .. }
                )
            })
            .count();
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
    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(
            CLIENT,
            ClientToServer::Hello {
                name: "evict".into(),
            },
        );
        transport
            .0
            .push(CLIENT, ClientToServer::UpdatePlayer(save_near(edit_pos)));
        transport.0.push(
            CLIENT,
            ClientToServer::PlaceBlock {
                pos: edit_pos,
                hotbar: CREATIVE_STONE_HOTBAR,
            },
        );
    }
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
                ServerToClient::ChunkData { pos, chunk } if pos == evicted_candidate => Some(chunk),
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

// ---------------------------------------------------------------------
// M4/M5: survival core, items, inventory, crafting, and containers
// (doc/roadmap.md M4/M5).
// ---------------------------------------------------------------------

#[test]
fn break_credits_item_and_place_consumes() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;
    let stone_block = BlockId(1);

    // A guaranteed-air chunk (see `guaranteed_air_edit`) with one seeded
    // solid block to break, and a second, untouched (still air) position to
    // prove a rejected placement never broadcasts.
    let chunk_pos = IVec3::new(0, 3, 0);
    let pos_a = IVec3::new(
        chunk_pos.x * CHUNK_SIZE as i32 + 5,
        chunk_pos.y * CHUNK_SIZE as i32 + 5,
        chunk_pos.z * CHUNK_SIZE as i32 + 5,
    );
    let pos_b = IVec3::new(pos_a.x + 1, pos_a.y, pos_a.z);
    seed_block(&mut app, pos_a, stone_block);

    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(
            CLIENT,
            ClientToServer::Hello {
                name: "miner".into(),
            },
        );
        transport
            .0
            .push(CLIENT, ClientToServer::UpdatePlayer(save_near(pos_a)));
        transport
            .0
            .push(CLIENT, ClientToServer::BreakBlock { pos: pos_a });
    }
    app.update();
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                ServerToClient::BlockChanged { pos, block } if *pos == pos_a && block.is_air()
            )),
            "expected BlockChanged to air after breaking: {msgs:?}"
        );
        assert_eq!(
            latest_main_count(&msgs, items::STONE),
            Some(1),
            "breaking a stone block must credit 1 stone item to the miner's inventory: {msgs:?}"
        );
    }

    // Stone the item landed in main slot 0 (first empty slot), which is
    // also hotbar slot 0 -- place it back: consumes the 1, inventory goes
    // to 0.
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            CLIENT,
            ClientToServer::PlaceBlock {
                pos: pos_a,
                hotbar: 0,
            },
        );
    app.update();
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                ServerToClient::BlockChanged { pos, block } if *pos == pos_a && *block == stone_block
            )),
            "expected BlockChanged back to stone: {msgs:?}"
        );
        assert_eq!(
            latest_main_count(&msgs, items::STONE),
            Some(0),
            "placing the block back must consume the 1: {msgs:?}"
        );
    }

    // Placing again with an empty hotbar slot is rejected: no broadcast at
    // all.
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            CLIENT,
            ClientToServer::PlaceBlock {
                pos: pos_b,
                hotbar: 0,
            },
        );
    app.update();
    {
        let transport = &app.world().resource::<TransportRes<MockTransport>>().0;
        let msgs = transport.outgoing.get(&CLIENT).cloned().unwrap_or_default();
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, ServerToClient::BlockChanged { .. })),
            "placing with an empty hotbar slot must not broadcast a change: {msgs:?}"
        );
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, ServerToClient::InventoryUpdate { .. })),
            "a rejected placement must not send an InventoryUpdate: {msgs:?}"
        );
    }
}

#[test]
fn reach_rejected() {
    // Reach validation applies regardless of game mode; Creative is used
    // here purely to keep the test free of inventory setup.
    let mut app = new_test_app(MockTransport::default(), 0);
    const CLIENT: ClientId = 1;

    let chunk_pos = IVec3::new(0, 3, 0);
    let pos = IVec3::new(
        chunk_pos.x * CHUNK_SIZE as i32 + 5,
        chunk_pos.y * CHUNK_SIZE as i32 + 5,
        chunk_pos.z * CHUNK_SIZE as i32 + 5,
    );
    seed_block(&mut app, pos, BlockId(1));

    // Never sent UpdatePlayer at all: reject rather than skip the check.
    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport
            .0
            .push(CLIENT, ClientToServer::Hello { name: "far".into() });
        transport.0.push(CLIENT, ClientToServer::BreakBlock { pos });
    }
    app.update();
    {
        let transport = &app.world().resource::<TransportRes<MockTransport>>().0;
        let msgs = transport.outgoing.get(&CLIENT).cloned().unwrap_or_default();
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, ServerToClient::BlockChanged { .. })),
            "a client that never sent UpdatePlayer must have edits rejected: {msgs:?}"
        );
    }

    // Far beyond SERVER_REACH: also rejected, for both Break and Place.
    let far = save_at(Vec3::new(pos.x as f32 + 1000.0, pos.y as f32, pos.z as f32));
    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(CLIENT, ClientToServer::UpdatePlayer(far));
        transport.0.push(CLIENT, ClientToServer::BreakBlock { pos });
        transport.0.push(
            CLIENT,
            ClientToServer::PlaceBlock {
                pos: IVec3::new(pos.x + 1, pos.y, pos.z),
                hotbar: CREATIVE_STONE_HOTBAR,
            },
        );
    }
    app.update();
    {
        let transport = &app.world().resource::<TransportRes<MockTransport>>().0;
        let msgs = transport.outgoing.get(&CLIENT).cloned().unwrap_or_default();
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, ServerToClient::BlockChanged { .. })),
            "edits beyond SERVER_REACH must be rejected: {msgs:?}"
        );
    }

    // Move close: the same BreakBlock now succeeds, confirming the earlier
    // rejections were about reach specifically.
    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport
            .0
            .push(CLIENT, ClientToServer::UpdatePlayer(save_near(pos)));
        transport.0.push(CLIENT, ClientToServer::BreakBlock { pos });
    }
    app.update();
    {
        let transport = &app.world().resource::<TransportRes<MockTransport>>().0;
        let msgs = transport.outgoing.get(&CLIENT).cloned().unwrap_or_default();
        assert!(
            msgs.iter()
                .any(|m| matches!(m, ServerToClient::BlockChanged { pos: p, .. } if *p == pos)),
            "the same edit must succeed once in reach: {msgs:?}"
        );
    }
}

#[test]
fn creative_mode_prefills_hotbar_and_is_free() {
    let mut app = new_test_app(MockTransport::default(), 0); // Creative
    const CLIENT: ClientId = 1;

    let chunk_pos = IVec3::new(0, 3, 0);
    let solid_pos = IVec3::new(
        chunk_pos.x * CHUNK_SIZE as i32 + 5,
        chunk_pos.y * CHUNK_SIZE as i32 + 5,
        chunk_pos.z * CHUNK_SIZE as i32 + 5,
    );
    let air_pos = IVec3::new(solid_pos.x + 1, solid_pos.y, solid_pos.z);
    seed_block(&mut app, solid_pos, BlockId(1));

    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(
            CLIENT,
            ClientToServer::Hello {
                name: "creator".into(),
            },
        );
        transport
            .0
            .push(CLIENT, ClientToServer::UpdatePlayer(save_near(solid_pos)));
    }
    app.update();
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);
        assert_eq!(
            latest_main_count(&msgs, items::STONE),
            Some(ItemRegistry::prototype().max_stack(items::STONE)),
            "creative join must prefill the hotbar with placeable items: {msgs:?}"
        );
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, ServerToClient::HealthUpdate { .. })),
            "creative mode must never send HealthUpdate: {msgs:?}"
        );
    }

    // Place into air using the prefilled hotbar: succeeds unconditionally,
    // and consumes nothing.
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            CLIENT,
            ClientToServer::PlaceBlock {
                pos: air_pos,
                hotbar: CREATIVE_STONE_HOTBAR,
            },
        );
    app.update();
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                ServerToClient::BlockChanged { pos, block } if *pos == air_pos && *block == BlockId(1)
            )),
            "creative placement must always succeed: {msgs:?}"
        );
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, ServerToClient::InventoryUpdate { .. })),
            "creative placing must never consume, so no InventoryUpdate is sent: {msgs:?}"
        );
    }

    // Break the solid block: also succeeds, no inventory credit sent.
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(CLIENT, ClientToServer::BreakBlock { pos: solid_pos });
    app.update();
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                ServerToClient::BlockChanged { pos, block } if *pos == solid_pos && block.is_air()
            )),
            "creative breaking must always succeed: {msgs:?}"
        );
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, ServerToClient::InventoryUpdate { .. })),
            "creative mode must never credit a break: {msgs:?}"
        );
    }

    // ReportDamage is ignored entirely in creative.
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            CLIENT,
            ClientToServer::ReportDamage {
                amount: MAX_HP,
                cause: DamageCause::Fall,
            },
        );
    app.update();
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);
        assert!(
            !msgs.iter().any(|m| matches!(
                m,
                ServerToClient::HealthUpdate { .. } | ServerToClient::Died { .. }
            )),
            "creative mode must ignore ReportDamage: {msgs:?}"
        );
    }
}

#[test]
fn death_drops_and_respawn() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const A: ClientId = 1;
    const B: ClientId = 2;

    // A guaranteed-air chunk with a seeded 2-block stone floor and, above
    // it, two breakable stone blocks (so A's inventory ends up with a
    // count > 1, a more meaningful drop check than a single item) plus a
    // third solid block A will try (and fail) to break while dead.
    let chunk_pos = IVec3::new(0, 3, 0);
    let floor_a = IVec3::new(
        chunk_pos.x * CHUNK_SIZE as i32 + 5,
        chunk_pos.y * CHUNK_SIZE as i32 + 5,
        chunk_pos.z * CHUNK_SIZE as i32 + 5,
    );
    let floor_b = IVec3::new(floor_a.x + 1, floor_a.y, floor_a.z);
    let break_a = IVec3::new(floor_a.x, floor_a.y + 1, floor_a.z);
    let break_b = IVec3::new(floor_b.x, floor_b.y + 1, floor_b.z);
    let while_dead_pos = IVec3::new(floor_a.x, floor_a.y + 2, floor_a.z);
    seed_block(&mut app, floor_a, BlockId(1));
    seed_block(&mut app, floor_b, BlockId(1));
    seed_block(&mut app, break_a, BlockId(1));
    seed_block(&mut app, break_b, BlockId(1));
    seed_block(&mut app, while_dead_pos, BlockId(1));

    // A's last known position at the moment of death: standing above
    // `break_a`'s column, so the dropped items rest deterministically on
    // `floor_a` (see `sim::resolve_rest_position`'s docs).
    let a_pos = Vec3::new(
        break_a.x as f32 + 0.5,
        break_a.y as f32 + 1.5,
        break_a.z as f32 + 0.5,
    );

    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(
            A,
            ClientToServer::Hello {
                name: "victim".into(),
            },
        );
        transport
            .0
            .push(A, ClientToServer::UpdatePlayer(save_at(a_pos)));
        transport
            .0
            .push(A, ClientToServer::BreakBlock { pos: break_a });
        transport
            .0
            .push(A, ClientToServer::BreakBlock { pos: break_b });
    }
    app.update();
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(A);
        assert_eq!(
            latest_main_count(&msgs, items::STONE),
            Some(2),
            "expected both breaks credited: {msgs:?}"
        );
    }

    // Kill A outright.
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            A,
            ClientToServer::ReportDamage {
                amount: MAX_HP,
                cause: DamageCause::Fall,
            },
        );
    app.update();
    let (dropped_item_id, dropped_stack, rest_pos) = {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(A);
        assert!(
            msgs.iter()
                .any(|m| matches!(m, ServerToClient::HealthUpdate { hp: 0 })),
            "expected HealthUpdate{{hp: 0}}: {msgs:?}"
        );
        assert!(
            msgs.iter()
                .any(|m| matches!(m, ServerToClient::Died { .. })),
            "expected Died: {msgs:?}"
        );
        assert_eq!(
            latest_main_count(&msgs, items::STONE),
            Some(0),
            "inventory must be cleared on death: {msgs:?}"
        );
        msgs.iter()
            .find_map(|m| match m {
                ServerToClient::ItemSpawned { id, pos, stack } if stack.item == items::STONE => {
                    Some((*id, *stack, *pos))
                }
                _ => None,
            })
            .expect("expected the dropped inventory to spawn as an item")
    };
    assert_eq!(
        dropped_stack.count, 2,
        "the dropped item must carry the player's full stone count"
    );

    // While dead: edits are ignored (target is otherwise perfectly valid
    // and in reach, isolating this as specifically the death gate).
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            A,
            ClientToServer::BreakBlock {
                pos: while_dead_pos,
            },
        );
    app.update();
    {
        let transport = &app.world().resource::<TransportRes<MockTransport>>().0;
        let msgs = transport.outgoing.get(&A).cloned().unwrap_or_default();
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, ServerToClient::BlockChanged { .. })),
            "a dead player's edits must be ignored: {msgs:?}"
        );
    }

    // Move A far away from its own drop before it comes back to life --
    // otherwise the respawned A (standing right on top of where it died)
    // would immediately pick its own item back up, before B gets a chance
    // to (harmless in general, but it would make this specific test, whose
    // point is B's pickup, depend on a race).
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            A,
            ClientToServer::UpdatePlayer(save_at(Vec3::new(a_pos.x + 1000.0, a_pos.y, a_pos.z))),
        );

    // Respawn brings health back to full.
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(A, ClientToServer::Respawn);
    app.update();
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(A);
        assert!(
            msgs.iter()
                .any(|m| matches!(m, ServerToClient::HealthUpdate { hp } if *hp == MAX_HP)),
            "expected HealthUpdate{{hp: MAX_HP}} after respawn: {msgs:?}"
        );
    }

    // A second player standing where the dropped item rests picks it up
    // (`rest_pos` was captured from the ItemSpawned message at death time,
    // rather than re-queried now, since A moving away is the only thing
    // guaranteeing the item is still there to query).
    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(
            B,
            ClientToServer::Hello {
                name: "scavenger".into(),
            },
        );
        transport
            .0
            .push(B, ClientToServer::UpdatePlayer(save_at(rest_pos)));
    }
    app.update();
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(B);
        assert!(
            msgs.iter().any(
                |m| matches!(m, ServerToClient::ItemDespawned { id } if *id == dropped_item_id)
            ),
            "B standing on the dropped item should pick it up: {msgs:?}"
        );
        assert_eq!(
            latest_main_count(&msgs, items::STONE),
            Some(2),
            "B's inventory should gain the dropped stone: {msgs:?}"
        );
    }
}

#[test]
fn pickup_delay_and_expiry() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    // A small, fixed tick interval so timing transitions (not-yet-eligible
    // vs. eligible) are observable across a handful of ticks.
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 0.1;

    const PICKER: ClientId = 1;

    let pickup_pos = Vec3::new(200.0, 64.0, 200.0);
    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(
            PICKER,
            ClientToServer::Hello {
                name: "picker".into(),
            },
        );
        transport
            .0
            .push(PICKER, ClientToServer::UpdatePlayer(save_at(pickup_pos)));
    }
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(PICKER);

    let now = app.world().resource::<SimRes>().clock.0;
    app.world_mut()
        .resource_mut::<SimRes>()
        .items
        .insert_loaded(pickup_pos, ItemStack::new(items::DIRT, 3), now);

    for _ in 0..4 {
        app.update(); // +0.1s each, up to 0.4s: still within the 0.5s delay.
    }
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(PICKER);
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, ServerToClient::ItemDespawned { .. })),
            "an item younger than the pickup delay must not be picked up yet: {msgs:?}"
        );
    }
    app.update(); // 0.5s total: past the delay.
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(PICKER);
        assert!(
            msgs.iter()
                .any(|m| matches!(m, ServerToClient::ItemDespawned { .. })),
            "an item past the pickup delay, within radius, must be picked up: {msgs:?}"
        );
        assert_eq!(
            latest_main_count(&msgs, items::DIRT),
            Some(3),
            "picked-up items must be credited to the picker's inventory: {msgs:?}"
        );
    }

    // --- Expiry: an old item despawns without being picked up. ---
    let stale_pos = Vec3::new(300.0, 64.0, 300.0); // far from any player
    let now = app.world().resource::<SimRes>().clock.0;
    app.world_mut()
        .resource_mut::<SimRes>()
        .items
        .insert_loaded(
            stale_pos,
            ItemStack::one(items::SAND),
            now - sim::ITEM_EXPIRY_SECS - 1.0,
        );
    let stale_id = {
        let sim_res = app.world().resource::<SimRes>();
        sim_res
            .items
            .items
            .iter()
            .find(|(_, it)| it.stack.item == items::SAND)
            .map(|(&id, _)| id)
            .expect("expected the stale item to be present")
    };

    app.update();
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(PICKER);
        assert!(
            msgs.iter()
                .any(|m| matches!(m, ServerToClient::ItemDespawned { id } if *id == stale_id)),
            "an item older than ITEM_EXPIRY_SECS must expire: {msgs:?}"
        );
    }
    assert!(
        !app.world()
            .resource::<SimRes>()
            .items
            .items
            .contains_key(&stale_id),
        "expired item must be removed from the live set"
    );
}

/// roadmap M5: "a full-inventory pickup leaving the item on the ground".
#[test]
fn full_inventory_pickup_leaves_item_on_ground() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const PICKER: ClientId = 1;
    let pos = Vec3::new(50.0, 64.0, 50.0);

    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(
            PICKER,
            ClientToServer::Hello {
                name: "packed".into(),
            },
        );
        transport
            .0
            .push(PICKER, ClientToServer::UpdatePlayer(save_at(pos)));
    }
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(PICKER);

    // Completely fill the main inventory with maxed-out stacks of an item
    // the dropped stack below doesn't match, so no partial merge is
    // possible either.
    let reg = ItemRegistry::prototype();
    {
        let mut state = app.world_mut().resource_mut::<ServerState>();
        let client = state.clients.get_mut(&PICKER).unwrap();
        for i in 0..tsumiki_world::MAIN_INVENTORY_SIZE {
            client.main.set_slot(
                i,
                Some(ItemStack::new(items::DIRT, reg.max_stack(items::DIRT))),
            );
        }
    }

    let now = app.world().resource::<SimRes>().clock.0;
    let dropped_id = {
        let mut sim_res = app.world_mut().resource_mut::<SimRes>();
        sim_res
            .items
            .insert_loaded(pos, ItemStack::new(items::STONE, 5), now - 10.0); // already past pickup delay
        sim_res.items.items.keys().next().copied().unwrap()
    };

    app.update();
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(PICKER);
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, ServerToClient::ItemDespawned { id } if *id == dropped_id)),
            "a full inventory must leave the dropped item on the ground: {msgs:?}"
        );
    }
    assert!(
        app.world()
            .resource::<SimRes>()
            .items
            .items
            .contains_key(&dropped_id),
        "the item must still exist in the world"
    );
}

#[test]
fn persistence_v4_roundtrip() {
    let dir = tempfile::tempdir().expect("failed to create tempdir");
    let world_dir = dir.path().to_path_buf();

    let stone_pos = IVec3::new(5, 5, 5);
    let stone_pos2 = IVec3::new(505, 5, 505);

    {
        let mut app = new_test_app_with(
            MockTransport::default(),
            0,
            Persistence::new(Some(world_dir.clone()), 9999.0),
            GameMode::Survival,
        );
        const CLIENT: ClientId = 1;
        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            transport.0.push(
                CLIENT,
                ClientToServer::Hello {
                    name: "surv".into(),
                },
            );
            transport
                .0
                .push(CLIENT, ClientToServer::UpdatePlayer(save_near(stone_pos)));
            transport
                .0
                .push(CLIENT, ClientToServer::BreakBlock { pos: stone_pos });
        }
        app.update();
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);

        // Die (dropping that one item), then respawn, then break a second
        // block so the surviving inventory isn't empty.
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(
                CLIENT,
                ClientToServer::ReportDamage {
                    amount: MAX_HP,
                    cause: DamageCause::Fall,
                },
            );
        app.update();
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);

        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            transport.0.push(CLIENT, ClientToServer::Respawn);
            transport
                .0
                .push(CLIENT, ClientToServer::UpdatePlayer(save_near(stone_pos2)));
            transport
                .0
                .push(CLIENT, ClientToServer::BreakBlock { pos: stone_pos2 });
        }
        app.update();
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);

        app.world_mut()
            .resource_mut::<SimRes>()
            .world_time
            .time_of_day = 0.37;

        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(CLIENT, ClientToServer::Goodbye);
        app.update();

        let mut reload = Persistence::new(Some(world_dir.clone()), 9999.0);
        let loaded = reload
            .load()
            .expect("load failed")
            .expect("expected a saved world");
        assert_eq!(loaded.game_mode, GameMode::Survival);
        assert!(
            (loaded.world_time_of_day - 0.37).abs() < 1e-6,
            "time of day did not survive the save: {}",
            loaded.world_time_of_day
        );
        let record = loaded.players.get("surv").expect("expected player record");
        assert_eq!(
            record.hp, MAX_HP,
            "respawn should have restored full health"
        );
        assert!(
            record
                .main
                .iter()
                .flatten()
                .any(|s| s.item == items::STONE && s.count >= 1),
            "expected the post-respawn break to survive in inventory: {:?}",
            record.main
        );
        assert_eq!(loaded.items.len(), 1, "expected the one death-dropped item");
        assert_eq!(loaded.items[0].stack, ItemStack::one(items::STONE));
        assert!(loaded.containers.is_empty());
    }

    // --- Restart via the real server entry point: the persisted Survival
    // mode must win over a config that now asks for Creative. ---
    {
        let (server_transport, mut client) = pair();
        let handle = thread::spawn({
            let world_dir = world_dir.clone();
            move || {
                run_server(
                    server_transport,
                    ServerConfig {
                        seed: 0,
                        tick_hz: 60.0,
                        world_dir: Some(world_dir),
                        autosave_interval_secs: 9999.0,
                        game_mode: Some(GameMode::Creative),
                    },
                );
            }
        });

        client.send(ClientToServer::Hello {
            name: "surv".into(),
        });
        match recv_within(&mut client, Duration::from_secs(5)) {
            Some(ServerToClient::Welcome {
                game_mode,
                time_of_day,
                ..
            }) => {
                assert_eq!(
                    game_mode,
                    GameMode::Survival,
                    "the world's persisted Survival mode must override the config's Creative"
                );
                assert!(
                    (time_of_day - 0.37).abs() < 1e-6,
                    "expected the persisted time of day back: {time_of_day}"
                );
            }
            other => panic!("expected Welcome, got {other:?}"),
        }

        let mut saw_inventory = false;
        let mut saw_health = false;
        let mut saw_item = false;
        for _ in 0..3 {
            match recv_within(&mut client, Duration::from_secs(5)) {
                Some(ServerToClient::InventoryUpdate { main, .. }) => {
                    saw_inventory = true;
                    assert!(
                        main.iter()
                            .flatten()
                            .any(|s| s.item == items::STONE && s.count >= 1),
                        "expected the restored inventory to include the surviving item: {main:?}"
                    );
                }
                Some(ServerToClient::HealthUpdate { hp }) => {
                    saw_health = true;
                    assert_eq!(hp, MAX_HP);
                }
                Some(ServerToClient::ItemSpawned { stack, .. }) => {
                    saw_item = true;
                    assert_eq!(stack, ItemStack::one(items::STONE));
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }
        assert!(
            saw_inventory && saw_health && saw_item,
            "expected all three of InventoryUpdate/HealthUpdate/ItemSpawned on join"
        );

        client.send(ClientToServer::Goodbye);
        join_within(handle, Duration::from_secs(5));
    }
}

#[test]
fn v3_meta_migrates_block_counts_to_item_stacks() {
    let v3_dir = tempfile::tempdir().expect("failed to create tempdir");

    #[derive(serde::Serialize)]
    struct LegacyPlayerRecordV3 {
        save: PlayerSave,
        hp: u16,
        inventory: Vec<(BlockId, u32)>,
    }
    #[derive(serde::Serialize)]
    struct LegacyItemRecordV3 {
        pos: Vec3,
        block: BlockId,
        count: u32,
    }
    #[derive(serde::Serialize)]
    struct LegacyMetaV3 {
        version: u32,
        seed: u64,
        game_mode: GameMode,
        world_time_of_day: f32,
        players: HashMap<String, LegacyPlayerRecordV3>,
        items: Vec<LegacyItemRecordV3>,
    }

    let mut players = HashMap::new();
    players.insert(
        "digger".to_string(),
        LegacyPlayerRecordV3 {
            save: save_at(Vec3::new(1.0, 2.0, 3.0)),
            hp: 15,
            // stone (places-mapped to items::STONE) plus water, which places
            // no item at all and must be dropped rather than invented.
            inventory: vec![(BlockId(1), 5), (blocks::WATER, 3)],
        },
    );
    let legacy = LegacyMetaV3 {
        version: 3,
        seed: 42,
        game_mode: GameMode::Survival,
        world_time_of_day: 0.5,
        players,
        items: vec![LegacyItemRecordV3 {
            pos: Vec3::new(4.0, 5.0, 6.0),
            block: BlockId(1),
            count: 2,
        }],
    };
    let bytes = postcard::to_allocvec(&legacy).expect("failed to encode legacy v3 meta.bin");
    fs::write(v3_dir.path().join("meta.bin"), bytes).expect("failed to write legacy meta.bin");

    let mut p = Persistence::new(Some(v3_dir.path().to_path_buf()), 9999.0);
    let loaded = p.load().expect("load failed").expect("expected a world");

    assert_eq!(loaded.game_mode, GameMode::Survival);
    let record = loaded
        .players
        .get("digger")
        .expect("expected migrated player");
    assert_eq!(record.hp, 15);
    assert!(
        record
            .main
            .iter()
            .flatten()
            .any(|s| s.item == items::STONE && s.count == 5),
        "expected the block-1 count to migrate to 5 stone items: {:?}",
        record.main
    );
    assert!(
        record.main.iter().flatten().all(|s| s.item != ItemId(0)),
        "no placeholder items should appear for the un-mappable water count"
    );

    assert_eq!(loaded.items.len(), 1);
    assert_eq!(loaded.items[0].stack, ItemStack::new(items::STONE, 2));
    assert!(loaded.containers.is_empty());
}

#[test]
fn v1_v2_meta_migrate_to_v4_creative_with_empty_inventory() {
    let v2_dir = tempfile::tempdir().expect("failed to create tempdir");
    #[derive(serde::Serialize)]
    struct LegacyMetaV2 {
        version: u32,
        seed: u64,
        players: HashMap<String, PlayerSave>,
    }
    let mut legacy_players = HashMap::new();
    legacy_players.insert(
        "legacy".to_string(),
        PlayerSave {
            pos: Vec3::new(1.0, 2.0, 3.0),
            yaw: 0.0,
            pitch: 0.0,
        },
    );
    let legacy = LegacyMetaV2 {
        version: 2,
        seed: 99,
        players: legacy_players,
    };
    let bytes = postcard::to_allocvec(&legacy).expect("failed to encode legacy meta.bin");
    fs::write(v2_dir.path().join("meta.bin"), bytes).expect("failed to write legacy meta.bin");

    let mut p = Persistence::new(Some(v2_dir.path().to_path_buf()), 9999.0);
    let loaded = p.load().expect("load failed").expect("expected a world");
    assert_eq!(loaded.game_mode, GameMode::Creative);
    let rec = loaded
        .players
        .get("legacy")
        .expect("expected the migrated player");
    assert_eq!(rec.hp, MAX_HP);
    assert!(rec.main.is_empty());
    assert!(loaded.containers.is_empty());
}

#[test]
fn peek_meta_of_directory_without_meta_bin_is_not_an_error() {
    let dir = tempfile::tempdir().expect("failed to create tempdir");
    let peeked = peek_meta(dir.path()).expect("peek_meta should not error on a fresh directory");
    assert!(peeked.is_none());
}

#[test]
fn peek_meta_reads_seed_and_game_mode_without_a_full_load() {
    let dir = tempfile::tempdir().expect("failed to create tempdir");
    create_world_meta(dir.path(), 12345, GameMode::Survival)
        .expect("failed to write initial meta.bin");

    // No `regions/` directory exists at all, so a peek that tried to read
    // chunks (rather than just meta.bin) would error here.
    let peeked = peek_meta(dir.path())
        .expect("peek_meta failed")
        .expect("expected a world");
    assert_eq!(peeked.seed, 12345);
    assert_eq!(peeked.game_mode, GameMode::Survival);
}

#[test]
fn peek_meta_agrees_with_persistence_load() {
    let dir = tempfile::tempdir().expect("failed to create tempdir");
    create_world_meta(dir.path(), 7, GameMode::Creative).expect("failed to write meta.bin");

    let peeked = peek_meta(dir.path())
        .expect("peek_meta failed")
        .expect("expected a world");
    let mut p = Persistence::new(Some(dir.path().to_path_buf()), 9999.0);
    let loaded = p.load().expect("load failed").expect("expected a world");

    assert_eq!(peeked.seed, loaded.seed);
    assert_eq!(peeked.game_mode, loaded.game_mode);
    assert!(loaded.players.is_empty());
    assert!(loaded.items.is_empty());
    assert!(loaded.containers.is_empty());
    assert!(loaded.chunks.is_empty());
}

#[test]
fn peek_meta_migrates_legacy_formats_like_a_full_load() {
    let dir = tempfile::tempdir().expect("failed to create tempdir");
    #[derive(serde::Serialize)]
    struct LegacyMetaV1 {
        version: u32,
        seed: u64,
        player: Option<PlayerSave>,
    }
    let legacy = LegacyMetaV1 {
        version: 1,
        seed: 55,
        player: None,
    };
    let bytes = postcard::to_allocvec(&legacy).expect("failed to encode legacy meta.bin");
    fs::write(dir.path().join("meta.bin"), bytes).expect("failed to write legacy meta.bin");

    let peeked = peek_meta(dir.path())
        .expect("peek_meta failed")
        .expect("expected a world");
    assert_eq!(peeked.seed, 55);
    // v1 predates game modes; both `peek_meta` and a full load migrate it to
    // Creative (see `decode_meta`'s docs).
    assert_eq!(peeked.game_mode, GameMode::Creative);
}

#[test]
fn time_advances_and_broadcasts() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;

    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            CLIENT,
            ClientToServer::Hello {
                name: "watcher".into(),
            },
        );
    app.update();
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);
        let welcome_time = msgs.iter().find_map(|m| match m {
            ServerToClient::Welcome { time_of_day, .. } => Some(*time_of_day),
            _ => None,
        });
        assert_eq!(
            welcome_time,
            Some(0.0),
            "a brand-new world starts at sunrise"
        );
    }

    // The harness's default tick_interval_secs is 1.0, so 5 ticks total is
    // exactly TIME_BROADCAST_INTERVAL_SECS -- the Hello-processing update
    // above already consumed the first tick, so 3 more brings the total to
    // 4 (not yet due), and one further update below is the 5th (due).
    for _ in 0..3 {
        app.update();
    }
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, ServerToClient::TimeUpdate { .. })),
            "must not broadcast before the interval elapses: {msgs:?}"
        );
    }
    app.update();
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);
        let time_of_day = msgs.iter().find_map(|m| match m {
            ServerToClient::TimeUpdate { time_of_day } => Some(*time_of_day),
            _ => None,
        });
        assert!(
            time_of_day.is_some(),
            "expected a TimeUpdate after 5 ticks (5.0s): {msgs:?}"
        );
        let expected = 5.0 / sim::DAY_LENGTH_SECS;
        assert!(
            (time_of_day.unwrap() - expected).abs() < 1e-4,
            "expected time_of_day near {expected}, got {:?}",
            time_of_day
        );
    }

    // Wraparound: pushing time_of_day just past 1.0 wraps back near 0.
    {
        let mut sim_res = app.world_mut().resource_mut::<SimRes>();
        sim_res.world_time.time_of_day = 0.9999;
    }
    app.update();
    {
        let time_of_day = app.world().resource::<SimRes>().world_time.time_of_day;
        assert!(
            (0.0..0.01).contains(&time_of_day),
            "time of day must wrap at 1.0, got {time_of_day}"
        );
    }
}

// ---------------------------------------------------------------------
// M5: items and crafting (doc/roadmap.md M5).
// ---------------------------------------------------------------------

#[test]
fn craft_a_crafting_table_from_planks() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;

    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            CLIENT,
            ClientToServer::Hello {
                name: "carpenter".into(),
            },
        );
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);

    seed_main_slot(&mut app, CLIENT, 0, ItemStack::new(items::PLANKS, 4));

    // Pick the stack up, then deposit one plank at a time into each of the
    // 2x2 hand-crafting cells -- raw indices 0, 1, 3, 4 of the always-3x3
    // grid (see `tsumiki_world::inventory::craft_grid_index`); index 2 is
    // masked out without a table open.
    let mut push = |msg: ClientToServer| {
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(CLIENT, msg);
    };
    push(ClientToServer::SlotClick {
        slot: SlotRef {
            area: SlotArea::Main,
            index: 0,
        },
        right: false,
        shift: false,
    });
    for i in [0u8, 1, 3, 4] {
        push(ClientToServer::SlotClick {
            slot: SlotRef {
                area: SlotArea::Crafting,
                index: i,
            },
            right: true,
            shift: false,
        });
    }
    app.update();
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);
        assert_eq!(
            latest_cursor(&msgs),
            Some(None),
            "all 4 planks should have been deposited, emptying the cursor: {msgs:?}"
        );
    }

    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            CLIENT,
            ClientToServer::SlotClick {
                slot: SlotRef {
                    area: SlotArea::CraftOutput,
                    index: 0,
                },
                right: false,
                shift: false,
            },
        );
    app.update();
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);
        assert_eq!(
            latest_cursor(&msgs),
            Some(Some(ItemStack::one(items::CRAFTING_TABLE))),
            "expected a crafted crafting table on the cursor: {msgs:?}"
        );
        let crafting_empty = msgs.iter().rev().find_map(|m| match m {
            ServerToClient::InventoryUpdate { crafting, .. } => {
                Some(crafting.iter().all(Option::is_none))
            }
            _ => None,
        });
        assert_eq!(
            crafting_empty,
            Some(true),
            "the 4 single-plank cells must be fully consumed: {msgs:?}"
        );
    }
}

#[test]
fn chest_recipe_needs_crafting_table() {
    // The crafting grid is always the full 9-slot 3x3 array; without a
    // table only raw indices 0, 1, 3, 4 (the top-left 2x2) are usable, per
    // `tsumiki_world::inventory::craft_grid_index`.
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;

    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            CLIENT,
            ClientToServer::Hello {
                name: "crafter".into(),
            },
        );
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);

    let load = |app: &mut App, indices: &[usize]| {
        let mut state = app.world_mut().resource_mut::<ServerState>();
        let client = state.clients.get_mut(&CLIENT).unwrap();
        for i in 0..9 {
            client.crafting.set_slot(i, None);
        }
        for &i in indices {
            client
                .crafting
                .set_slot(i, Some(ItemStack::one(items::PLANKS)));
        }
        client.cursor = None;
    };
    let set_table_open = |app: &mut App, open: bool| {
        let mut state = app.world_mut().resource_mut::<ServerState>();
        let client = state.clients.get_mut(&CLIENT).unwrap();
        client.open_container = open.then_some((IVec3::new(0, 0, 0), ContainerKind::CraftingTable));
    };
    let click_output = |app: &mut App| {
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(
                CLIENT,
                ClientToServer::SlotClick {
                    slot: SlotRef {
                        area: SlotArea::CraftOutput,
                        index: 0,
                    },
                    right: false,
                    shift: false,
                },
            );
        app.update();
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT)
    };

    // Planks at raw indices 0, 1, 3, 4 (the "square") match the 2x2
    // crafting-table recipe at *both* view sizes: at size 2 those are
    // exactly the hand-crafting cells, and at size 3 they form the same
    // 2x2 shape in the top-left corner of the identity view (with every
    // other cell empty).
    for table_open in [false, true] {
        load(&mut app, &[0, 1, 3, 4]);
        set_table_open(&mut app, table_open);
        let msgs = click_output(&mut app);
        assert_eq!(
            latest_cursor(&msgs),
            Some(Some(ItemStack::one(items::CRAFTING_TABLE))),
            "the square pattern (table_open={table_open}) should craft a table: {msgs:?}"
        );
    }

    // The chest "ring" (raw indices 0,1,2,3,5,6,7,8; the center, index 4,
    // is empty) matches the chest recipe only at size 3. At size 2, the
    // hand view maps to raw indices [0,1,3,4] -- and raw index 4 is the
    // ring's empty center, so the hand view sees only three planks and one
    // empty cell: no recipe matches, and the grid must be left untouched.
    let ring = [0usize, 1, 2, 3, 5, 6, 7, 8];
    load(&mut app, &ring);
    set_table_open(&mut app, false);
    let msgs = click_output(&mut app);
    assert_eq!(
        latest_craft_output(&msgs),
        Some(None),
        "the ring must produce no output without a table open: {msgs:?}"
    );
    assert_eq!(
        latest_cursor(&msgs),
        Some(None),
        "clicking a non-matching output must be a no-op: {msgs:?}"
    );
    {
        let state = app.world().resource::<ServerState>();
        let client = &state.clients[&CLIENT];
        assert!(
            ring.iter().all(|&i| client.crafting.slot(i).is_some()),
            "a failed craft attempt must not consume the grid"
        );
    }

    // The same ring, with a crafting table open, does yield the chest.
    load(&mut app, &ring);
    set_table_open(&mut app, true);
    let msgs = click_output(&mut app);
    assert_eq!(
        latest_cursor(&msgs),
        Some(Some(ItemStack::one(items::CHEST))),
        "expected the chest with a crafting table open: {msgs:?}"
    );
}

/// roadmap M5 (corrected contract): the crafting grid is a fixed 9-slot
/// array, masked rather than resized by view size, so opening or closing a
/// crafting table must never move (or drop) whatever is already sitting in
/// it -- including in cells only reachable with the table open. This is
/// distinct from `CloseContainer` (see `dropping_the_cursor_closes_and_
/// returns_it_to_the_world`), which deliberately empties the grid; walking
/// out of reach auto-closes the UI without touching its contents.
#[test]
fn opening_and_closing_a_table_leaves_grid_contents_in_place() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;
    let (_, table_pos) = guaranteed_air_edit(3, 3);
    seed_block(&mut app, table_pos, blocks::CRAFTING_TABLE);

    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(
            CLIENT,
            ClientToServer::Hello {
                name: "tinkerer".into(),
            },
        );
        transport
            .0
            .push(CLIENT, ClientToServer::UpdatePlayer(save_near(table_pos)));
    }
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);

    // Seed a hand-visible cell (raw index 0) and a masked-out-without-a-
    // table cell (raw index 2) directly, since there is no way to reach
    // index 2 via SlotClick before a table is open.
    {
        let mut state = app.world_mut().resource_mut::<ServerState>();
        let client = state.clients.get_mut(&CLIENT).unwrap();
        client
            .crafting
            .set_slot(0, Some(ItemStack::one(items::PLANKS)));
        client
            .crafting
            .set_slot(2, Some(ItemStack::one(items::STONE)));
    }

    // Open the table through the real protocol path.
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(CLIENT, ClientToServer::OpenContainer { pos: table_pos });
    app.update();
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                ServerToClient::ContainerOpened { kind: ContainerKind::CraftingTable, pos, .. }
                    if *pos == table_pos
            )),
            "expected ContainerOpened for the crafting table: {msgs:?}"
        );
    }
    {
        let state = app.world().resource::<ServerState>();
        let client = &state.clients[&CLIENT];
        assert_eq!(client.crafting.slot(0), Some(ItemStack::one(items::PLANKS)));
        assert_eq!(client.crafting.slot(2), Some(ItemStack::one(items::STONE)));
    }

    // Walk out of reach: the UI auto-closes (`ContainerClosed`), but --
    // unlike an explicit `CloseContainer` -- nothing is dropped.
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            CLIENT,
            ClientToServer::UpdatePlayer(save_at(Vec3::new(
                table_pos.x as f32 + 1000.0,
                table_pos.y as f32,
                table_pos.z as f32,
            ))),
        );
    app.update();
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);
        assert!(
            msgs.iter()
                .any(|m| matches!(m, ServerToClient::ContainerClosed)),
            "expected ContainerClosed when walking out of reach: {msgs:?}"
        );
    }
    {
        let state = app.world().resource::<ServerState>();
        let client = &state.clients[&CLIENT];
        assert_eq!(client.open_container, None);
        assert_eq!(
            client.crafting.slot(0),
            Some(ItemStack::one(items::PLANKS)),
            "the hand-visible cell must survive closing the table"
        );
        assert_eq!(
            client.crafting.slot(2),
            Some(ItemStack::one(items::STONE)),
            "the masked-out cell must survive closing the table -- it must not be moved or dropped"
        );
    }
    assert!(
        app.world().resource::<SimRes>().items.items.is_empty(),
        "an out-of-reach auto-close must not drop anything into the world"
    );
}

#[test]
fn chest_open_place_take_and_persist() {
    let dir = tempfile::tempdir().expect("failed to create tempdir");
    let world_dir = dir.path().to_path_buf();
    // Guaranteed air (see `guaranteed_air_edit`), so the chest place isn't
    // rejected by an already-solid destination.
    let (_, chest_pos) = guaranteed_air_edit(2, 2);

    {
        let mut app = new_test_app_with(
            MockTransport::default(),
            0,
            Persistence::new(Some(world_dir.clone()), 9999.0),
            GameMode::Survival,
        );
        const CLIENT: ClientId = 1;
        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            transport.0.push(
                CLIENT,
                ClientToServer::Hello {
                    name: "stasher".into(),
                },
            );
            transport
                .0
                .push(CLIENT, ClientToServer::UpdatePlayer(save_near(chest_pos)));
        }
        app.update();
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);

        seed_main_slot(&mut app, CLIENT, 0, ItemStack::one(items::CHEST));
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(
                CLIENT,
                ClientToServer::PlaceBlock {
                    pos: chest_pos,
                    hotbar: 0,
                },
            );
        app.update();
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);

        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(CLIENT, ClientToServer::OpenContainer { pos: chest_pos });
        app.update();
        {
            let msgs = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>()
                .0
                .take(CLIENT);
            assert!(
                msgs.iter().any(|m| matches!(
                    m,
                    ServerToClient::ContainerOpened { kind: ContainerKind::Chest, pos, .. }
                        if *pos == chest_pos
                )),
                "expected ContainerOpened for the chest: {msgs:?}"
            );
        }

        // Deposit planks from the cursor into the chest's first slot.
        {
            let mut state = app.world_mut().resource_mut::<ServerState>();
            let client = state.clients.get_mut(&CLIENT).unwrap();
            client.cursor = Some(ItemStack::new(items::PLANKS, 5));
        }
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(
                CLIENT,
                ClientToServer::SlotClick {
                    slot: SlotRef {
                        area: SlotArea::Container,
                        index: 0,
                    },
                    right: false,
                    shift: false,
                },
            );
        app.update();
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);

        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(CLIENT, ClientToServer::Goodbye);
        app.update();
    }

    let mut reload = Persistence::new(Some(world_dir), 9999.0);
    let loaded = reload.load().unwrap().expect("expected saved world");
    assert_eq!(loaded.containers.len(), 1, "expected one saved chest");
    let (pos, slots) = &loaded.containers[0];
    assert_eq!(*pos, chest_pos);
    assert_eq!(
        slots[0],
        Some(ItemStack::new(items::PLANKS, 5)),
        "chest contents did not survive the save: {slots:?}"
    );
}

#[test]
fn quick_move_between_hotbar_and_backpack() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            CLIENT,
            ClientToServer::Hello {
                name: "mover".into(),
            },
        );
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);

    seed_main_slot(&mut app, CLIENT, 0, ItemStack::new(items::DIRT, 10));
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            CLIENT,
            ClientToServer::SlotClick {
                slot: SlotRef {
                    area: SlotArea::Main,
                    index: 0,
                },
                right: false,
                shift: true,
            },
        );
    app.update();
    let msgs = app
        .world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);
    let main = msgs
        .iter()
        .rev()
        .find_map(|m| match m {
            ServerToClient::InventoryUpdate { main, .. } => Some(main.clone()),
            _ => None,
        })
        .expect("expected InventoryUpdate");
    assert_eq!(main[0], None, "the hotbar slot should have emptied");
    assert_eq!(
        main[9],
        Some(ItemStack::new(items::DIRT, 10)),
        "expected the stack to land in the first backpack slot: {main:?}"
    );
}

#[test]
fn dropping_the_cursor_closes_and_returns_it_to_the_world() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;
    let pos = Vec3::new(10.0, 64.0, 10.0);

    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(
            CLIENT,
            ClientToServer::Hello {
                name: "clumsy".into(),
            },
        );
        transport
            .0
            .push(CLIENT, ClientToServer::UpdatePlayer(save_at(pos)));
    }
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);

    // Hold something on the cursor and something in the crafting grid, as
    // if mid-drag when the player closes the screen.
    {
        let mut state = app.world_mut().resource_mut::<ServerState>();
        let client = state.clients.get_mut(&CLIENT).unwrap();
        client.cursor = Some(ItemStack::new(items::STICK, 2));
        client
            .crafting
            .set_slot(0, Some(ItemStack::one(items::PLANKS)));
        client.open_container = Some((IVec3::new(0, 0, 0), ContainerKind::CraftingTable));
    }

    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(CLIENT, ClientToServer::CloseContainer);
    app.update();
    let msgs = app
        .world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);

    assert!(
        msgs.iter()
            .any(|m| matches!(m, ServerToClient::ContainerClosed)),
        "expected ContainerClosed: {msgs:?}"
    );
    assert_eq!(
        latest_cursor(&msgs),
        Some(None),
        "the cursor must be emptied on close: {msgs:?}"
    );
    let crafting_empty = msgs.iter().rev().find_map(|m| match m {
        ServerToClient::InventoryUpdate { crafting, .. } => {
            Some(crafting.iter().all(Option::is_none))
        }
        _ => None,
    });
    assert_eq!(
        crafting_empty,
        Some(true),
        "the crafting grid must be emptied on close: {msgs:?}"
    );

    // Both the cursor stack and the crafting-grid item must have become
    // dropped items in the world.
    let dropped_items = &app.world().resource::<SimRes>().items.items;
    let stick_dropped = dropped_items
        .values()
        .any(|it| it.stack == ItemStack::new(items::STICK, 2));
    let planks_dropped = dropped_items
        .values()
        .any(|it| it.stack == ItemStack::one(items::PLANKS));
    assert!(stick_dropped, "expected the cursor's stick stack to drop");
    assert!(planks_dropped, "expected the crafting grid's plank to drop");
}

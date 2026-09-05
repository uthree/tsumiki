use super::*;
use std::fs;
use std::thread;
use std::time::Instant;

use tsumiki_protocol::ClientTransport;
use tsumiki_protocol::DamageCause;
use tsumiki_protocol::local::{LOCAL_CLIENT_ID, pair};
use tsumiki_protocol::{SlotArea, SlotRef};

use bevy_math::Vec3;
use tsumiki_world::smelting::{FURNACE_FUEL, FURNACE_INPUT, FURNACE_OUTPUT};
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
    app.init_resource::<lighting::Lighting>();
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
    let b_received = transport
        .outgoing
        .get(&CLIENT_B)
        .map(|messages| {
            messages
                .iter()
                .filter(|message| matches!(message, ServerToClient::ChunkData { .. }))
                .count()
        })
        .unwrap_or(0);
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

    // The out-of-bounds position must never arrive. Derived lighting for
    // valid chunks can arrive asynchronously after their block data.
    let deadline = Instant::now() + Duration::from_millis(200);
    while let Some(message) = recv_within(
        &mut client,
        deadline.saturating_duration_since(Instant::now()),
    ) {
        match message {
            ServerToClient::ChunkData { pos, .. } => {
                panic!("unexpected extra block chunk: {pos:?}")
            }
            ServerToClient::LightChunkData { pos, .. } => assert!(pos == valid_a || pos == valid_b),
            _ => {}
        }
        if Instant::now() >= deadline {
            break;
        }
    }

    // Re-requesting an already-sent chunk is honored again: a client
    // that despawned and forgot a chunk beyond its view distance, then
    // walked back and re-requested it, must be served from cache rather
    // than silently ignored (see `rerequest_after_forget_is_served`).
    client.send(ClientToServer::RequestChunks {
        positions: vec![valid_a],
    });
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        match recv_within(
            &mut client,
            deadline.saturating_duration_since(Instant::now()),
        ) {
            Some(ServerToClient::ChunkData { pos, .. }) => {
                assert_eq!(pos, valid_a);
                break;
            }
            Some(_) if Instant::now() < deadline => {}
            other => panic!("expected the re-requested chunk to be served again, got {other:?}"),
        }
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
    // Both message kinds name a hotbar slot (roadmap M6 gave `BreakBlock`
    // one too), so both get the same treatment: reject the whole message
    // rather than guess at a fallback.
    let bad_hotbar_place = ClientToServer::PlaceBlock {
        pos: IVec3::new(0, 10, 0),
        hotbar: 200,
    };
    let bad_hotbar_break = ClientToServer::BreakBlock {
        pos: IVec3::new(1, 10, 0),
        hotbar: 200,
    };

    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(CLIENT, below_bounds);
        transport.0.push(CLIENT, above_bounds);
        transport.0.push(CLIENT, bad_hotbar_place);
        transport.0.push(CLIENT, bad_hotbar_break);
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
        "invalid PlaceBlock/BreakBlock requests must never broadcast a change: {msgs:?}"
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
fn break_drops_item_then_delayed_pickup_allows_placing() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;
    const OBSERVER: ClientId = 2;
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 0.125;
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
    seed_block(&mut app, pos_a - IVec3::Y, stone_block);
    let far_pos = save_near(pos_a).pos + Vec3::X * 20.0;

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
        transport.0.push(
            OBSERVER,
            ClientToServer::Hello {
                name: "observer".into(),
            },
        );
        transport
            .0
            .push(OBSERVER, ClientToServer::UpdatePlayer(save_at(far_pos)));
    }
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(OBSERVER);

    // Stone is gated (roadmap M6): a wooden pickaxe in the hotbar is
    // required to get anything from it. Keep slot 0 empty for the later
    // pickup and placement.
    seed_main_slot(&mut app, CLIENT, 8, ItemStack::one(items::WOODEN_PICKAXE));
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            CLIENT,
            ClientToServer::BreakBlock {
                pos: pos_a,
                hotbar: 8,
            },
        );
    app.update();
    let dropped_id = {
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
            latest_main_count(&msgs, items::COBBLESTONE),
            Some(0),
            "the immediate inventory update must contain tool wear only: {msgs:?}"
        );
        msgs.iter()
            .find_map(|m| match m {
                ServerToClient::ItemSpawned { id, stack, .. }
                    if *stack == ItemStack::one(items::COBBLESTONE) =>
                {
                    Some(*id)
                }
                _ => None,
            })
            .expect("mining must spawn the drop despite available inventory space")
    };
    let observer_msgs = app
        .world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(OBSERVER);
    assert!(observer_msgs.iter().any(|m| matches!(
        m,
        ServerToClient::ItemSpawned { id, .. } if *id == dropped_id
    )));

    // Even standing on the drop cannot collect it before the pickup delay.
    app.update();
    app.update();
    assert!(
        app.world()
            .resource::<SimRes>()
            .items
            .items
            .contains_key(&dropped_id)
    );
    assert_eq!(
        app.world().resource::<ServerState>().clients[&CLIENT]
            .main
            .slot(0),
        None
    );

    // Once old enough, it still stays in the world while every client is
    // outside the pickup radius.
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(CLIENT, ClientToServer::UpdatePlayer(save_at(far_pos)));
    app.update();
    assert!(
        app.world()
            .resource::<SimRes>()
            .items
            .items
            .contains_key(&dropped_id)
    );
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(CLIENT, ClientToServer::UpdatePlayer(save_near(pos_a)));
    app.update();
    let pickup_msgs = app
        .world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);
    assert_eq!(latest_main_count(&pickup_msgs, items::COBBLESTONE), Some(1));
    assert!(
        !app.world()
            .resource::<SimRes>()
            .items
            .items
            .contains_key(&dropped_id)
    );
    let observer_msgs = app
        .world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(OBSERVER);
    for msgs in [&pickup_msgs, &observer_msgs] {
        assert!(msgs.iter().any(|m| matches!(
            m,
            ServerToClient::ItemDespawned { id } if *id == dropped_id
        )));
    }

    // Cobblestone landed in main slot 0 (first empty slot) -- place it back:
    // consumes the 1, inventory goes to 0.
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
                ServerToClient::BlockChanged { pos, block } if *pos == pos_a && *block == tsumiki_world::blocks::COBBLESTONE
            )),
            "expected BlockChanged to cobblestone: {msgs:?}"
        );
        assert_eq!(
            latest_main_count(&msgs, items::COBBLESTONE),
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
        transport.0.push(
            CLIENT,
            ClientToServer::BreakBlock {
                pos,
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
        transport.0.push(
            CLIENT,
            ClientToServer::BreakBlock {
                pos,
                hotbar: CREATIVE_STONE_HOTBAR,
            },
        );
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
        transport.0.push(
            CLIENT,
            ClientToServer::BreakBlock {
                pos,
                hotbar: CREATIVE_STONE_HOTBAR,
            },
        );
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

    // Break the solid block: also succeeds, with no inventory update or drop.
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            CLIENT,
            ClientToServer::BreakBlock {
                pos: solid_pos,
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
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, ServerToClient::ItemSpawned { .. })),
            "creative mining must not create drops: {msgs:?}"
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
    }
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(A);

    // Stone is gated (roadmap M6): A needs a wooden pickaxe to get anything
    // from it.
    seed_main_slot(&mut app, A, 8, ItemStack::one(items::WOODEN_PICKAXE));
    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(
            A,
            ClientToServer::BreakBlock {
                pos: break_a,
                hotbar: 8,
            },
        );
        transport.0.push(
            A,
            ClientToServer::BreakBlock {
                pos: break_b,
                hotbar: 8,
            },
        );
    }
    app.update();
    {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(A);
        assert_eq!(
            latest_main_count(&msgs, items::COBBLESTONE),
            Some(2),
            "expected both drops picked up after the one-second test tick: {msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                ServerToClient::ItemSpawned { stack, .. }
                    if *stack == ItemStack::new(items::COBBLESTONE, 2)
            )),
            "nearby mined items must merge before pickup: {msgs:?}"
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
            latest_main_count(&msgs, items::COBBLESTONE),
            Some(0),
            "inventory must be cleared on death: {msgs:?}"
        );
        msgs.iter()
            .find_map(|m| match m {
                ServerToClient::ItemSpawned { id, pos, stack }
                    if stack.item == items::COBBLESTONE =>
                {
                    Some((*id, *stack, *pos))
                }
                _ => None,
            })
            .expect("expected the dropped inventory to spawn as an item")
    };
    assert_eq!(
        dropped_stack.count, 2,
        "the dropped item must carry the player's full cobblestone count"
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
                hotbar: 8,
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
            latest_main_count(&msgs, items::COBBLESTONE),
            Some(2),
            "B's inventory should gain the dropped cobblestone: {msgs:?}"
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
    let break_pos = IVec3::new(50, 64, 50);
    seed_block(&mut app, break_pos, blocks::DIRT);
    seed_block(&mut app, break_pos - IVec3::Y, blocks::STONE);

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

    // Maxed-out matching stacks cannot absorb even one more mined item;
    // the unrelated loaded stack below cannot use an empty slot either.
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

    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            PICKER,
            ClientToServer::BreakBlock {
                pos: break_pos,
                hotbar: 0,
            },
        );
    app.update();
    let mined_id = {
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(PICKER);
        assert_eq!(latest_main_count(&msgs, items::DIRT), None);
        msgs.iter()
            .find_map(|m| match m {
                ServerToClient::ItemSpawned { id, stack, .. }
                    if *stack == ItemStack::one(items::DIRT) =>
                {
                    Some(*id)
                }
                _ => None,
            })
            .expect("a full inventory must still allow mining to create its drop")
    };
    let now = app.world().resource::<SimRes>().clock.0;
    let dropped_id = {
        let mut sim_res = app.world_mut().resource_mut::<SimRes>();
        sim_res
            .items
            .insert_loaded(pos, ItemStack::new(items::STONE, 5), now - 10.0); // already past pickup delay
        sim_res
            .items
            .items
            .iter()
            .find(|(_, item)| item.stack.item == items::STONE)
            .map(|(&id, _)| id)
            .unwrap()
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
                .any(|m| matches!(m, ServerToClient::ItemDespawned { id } if *id == dropped_id || *id == mined_id)),
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
    assert!(
        app.world()
            .resource::<SimRes>()
            .items
            .items
            .contains_key(&mined_id)
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
        }
        app.update();
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);

        // Stone is gated (roadmap M6): a wooden pickaxe is required to get
        // anything from it.
        seed_main_slot(&mut app, CLIENT, 8, ItemStack::one(items::WOODEN_PICKAXE));
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(
                CLIENT,
                ClientToServer::BreakBlock {
                    pos: stone_pos,
                    hotbar: 8,
                },
            );
        app.update();
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);
        assert!(msgs.iter().any(|m| matches!(
            m,
            ServerToClient::ItemSpawned { stack, .. }
                if *stack == ItemStack::one(items::COBBLESTONE)
        )));
        assert_eq!(
            latest_main_count(&msgs, items::COBBLESTONE),
            Some(1),
            "the one-second test tick must allow the nearby mined drop to be picked up"
        );

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
        }
        app.update();
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);

        // Death drained the pickaxe along with everything else; re-seed one
        // for the post-respawn break.
        seed_main_slot(&mut app, CLIENT, 8, ItemStack::one(items::WOODEN_PICKAXE));
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(
                CLIENT,
                ClientToServer::BreakBlock {
                    pos: stone_pos2,
                    hotbar: 8,
                },
            );
        app.update();
        let msgs = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);
        assert!(msgs.iter().any(|m| matches!(
            m,
            ServerToClient::ItemSpawned { stack, .. }
                if *stack == ItemStack::one(items::COBBLESTONE)
        )));
        assert_eq!(latest_main_count(&msgs, items::COBBLESTONE), Some(1));

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
                .any(|s| s.item == items::COBBLESTONE && s.count >= 1),
            "expected the post-respawn pickup to survive in inventory: {:?}",
            record.main
        );
        // Death drained the whole inventory, including the wooden pickaxe
        // used for the first break -- so both it and the mined cobblestone
        // ended up on the ground.
        assert_eq!(
            loaded.items.len(),
            2,
            "expected the death-dropped cobblestone and pickaxe"
        );
        assert!(
            loaded
                .items
                .iter()
                .any(|rec| rec.stack == ItemStack::one(items::COBBLESTONE)),
            "expected the death-dropped cobblestone: {:?}",
            loaded.items
        );
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
        let mut items_seen = 0;
        // InventoryUpdate + HealthUpdate + one ItemSpawned per dropped item
        // still on the ground (the death-dropped cobblestone and pickaxe).
        for _ in 0..4 {
            match recv_within(&mut client, Duration::from_secs(5)) {
                Some(ServerToClient::InventoryUpdate { main, .. }) => {
                    saw_inventory = true;
                    assert!(
                        main.iter()
                            .flatten()
                            .any(|s| s.item == items::COBBLESTONE && s.count >= 1),
                        "expected the restored inventory to include the surviving item: {main:?}"
                    );
                }
                Some(ServerToClient::HealthUpdate { hp }) => {
                    saw_health = true;
                    assert_eq!(hp, MAX_HP);
                }
                Some(ServerToClient::ItemSpawned { stack, .. }) => {
                    items_seen += 1;
                    assert!(
                        stack == ItemStack::one(items::COBBLESTONE)
                            || stack == ItemStack::one(items::WOODEN_PICKAXE),
                        "unexpected dropped item: {stack:?}"
                    );
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }
        assert!(
            saw_inventory && saw_health && items_seen == 2,
            "expected InventoryUpdate, HealthUpdate, and both dropped items on join"
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

/// Recipe ids from `RecipeRegistry::prototype()`
/// (`tsumiki_world::recipe::RecipeRegistry::prototype`), in catalog order.
/// Hardcoding these mirrors the convention the recipe table's own tests use
/// (`crates/world/src/recipe.rs`, e.g. `reg.recipes()[2]`).
const RECIPE_PLANKS: u16 = 0;
const RECIPE_STICKS: u16 = 1;
const RECIPE_CHEST: u16 = 3;

/// Sends `Hello` for a fresh client and discards the join messages
/// (`Welcome`/`InventoryUpdate`/...), so a test's own `take(CLIENT)` only
/// sees what happened after it. Shared by the `Craft` tests below.
fn join(app: &mut App, client: ClientId, name: &str) {
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            client,
            ClientToServer::Hello {
                name: name.to_string(),
            },
        );
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(client);
}

fn craft(app: &mut App, client: ClientId, recipe: u16, all: bool) -> Vec<ServerToClient> {
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(client, ClientToServer::Craft { recipe, all });
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(client)
}

#[test]
fn craft_by_id_consumes_inputs_and_yields_output() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;
    join(&mut app, CLIENT, "carpenter");
    seed_main_slot(&mut app, CLIENT, 0, ItemStack::new(items::LOG, 3));

    let msgs = craft(&mut app, CLIENT, RECIPE_PLANKS, false);

    assert_eq!(
        latest_main_count(&msgs, items::LOG),
        Some(2),
        "one log should have been consumed: {msgs:?}"
    );
    assert_eq!(
        latest_main_count(&msgs, items::PLANKS),
        Some(4),
        "expected 4 planks from one craft: {msgs:?}"
    );
}

#[test]
fn unknown_recipe_id_is_rejected() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;
    join(&mut app, CLIENT, "hopeful");
    seed_main_slot(&mut app, CLIENT, 0, ItemStack::new(items::LOG, 3));

    let msgs = craft(&mut app, CLIENT, 9999, false);

    assert!(
        msgs.is_empty(),
        "an unknown recipe id must be silently ignored, not answered: {msgs:?}"
    );
    let state = app.world().resource::<ServerState>();
    assert_eq!(
        state.clients[&CLIENT].main.count_of(items::LOG),
        3,
        "a rejected craft must not touch the inventory"
    );
}

#[test]
fn chest_recipe_needs_a_crafting_table() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;
    join(&mut app, CLIENT, "crafter");
    seed_main_slot(&mut app, CLIENT, 0, ItemStack::new(items::PLANKS, 8));

    // No crafting table open: the chest recipe's station isn't reachable, so
    // the craft is rejected outright and the materials are untouched.
    let msgs = craft(&mut app, CLIENT, RECIPE_CHEST, false);
    assert!(
        msgs.is_empty(),
        "the chest recipe must be rejected without a crafting table open: {msgs:?}"
    );
    {
        let state = app.world().resource::<ServerState>();
        assert_eq!(state.clients[&CLIENT].main.count_of(items::PLANKS), 8);
    }

    // Open a crafting table (directly, as there is no protocol message that
    // just flips this without also planting a real block) and retry.
    {
        let mut state = app.world_mut().resource_mut::<ServerState>();
        let client = state.clients.get_mut(&CLIENT).unwrap();
        client.open_container = Some((IVec3::new(0, 0, 0), ContainerKind::CraftingTable));
    }
    let msgs = craft(&mut app, CLIENT, RECIPE_CHEST, false);
    assert_eq!(
        latest_main_count(&msgs, items::CHEST),
        Some(1),
        "expected a chest once a crafting table is open: {msgs:?}"
    );
    assert_eq!(
        latest_main_count(&msgs, items::PLANKS),
        Some(0),
        "the 8 planks should have been fully consumed: {msgs:?}"
    );
}

#[test]
fn craft_all_crafts_as_many_times_as_materials_allow() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;
    join(&mut app, CLIENT, "batcher");
    // 2 planks per craft (see `RecipeRegistry::prototype`): 5 crafts from 10,
    // with none left over.
    seed_main_slot(&mut app, CLIENT, 0, ItemStack::new(items::PLANKS, 10));

    let msgs = craft(&mut app, CLIENT, RECIPE_STICKS, true);

    assert_eq!(
        latest_main_count(&msgs, items::PLANKS),
        Some(0),
        "all 10 planks should have been used: {msgs:?}"
    );
    assert_eq!(
        latest_main_count(&msgs, items::STICK),
        Some(20),
        "expected 5 crafts worth of sticks (4 each): {msgs:?}"
    );
}

#[test]
fn craft_overflow_drops_as_items() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;
    let pos = Vec3::new(5.0, 64.0, 5.0);
    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(
            CLIENT,
            ClientToServer::Hello {
                name: "packrat".into(),
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

    // A stack of planks large enough that consuming one craft's worth still
    // leaves the slot occupied (so it isn't the craft's own output that ends
    // up reusing the freed slot), plus every other slot already full of an
    // unrelated item -- so the sticks this craft produces have nowhere in
    // the inventory to land at all.
    seed_main_slot(&mut app, CLIENT, 0, ItemStack::new(items::PLANKS, 64));
    for i in 1..MAIN_INVENTORY_SIZE {
        seed_main_slot(&mut app, CLIENT, i, ItemStack::new(items::STONE, 64));
    }

    let msgs = craft(&mut app, CLIENT, RECIPE_STICKS, false);

    assert_eq!(
        latest_main_count(&msgs, items::STICK),
        Some(0),
        "the crafted sticks must not have landed in the full inventory: {msgs:?}"
    );
    assert_eq!(
        latest_main_count(&msgs, items::PLANKS),
        Some(62),
        "2 planks should have been consumed: {msgs:?}"
    );
    let dropped = &app.world().resource::<SimRes>().items.items;
    assert!(
        dropped
            .values()
            .any(|it| it.stack == ItemStack::new(items::STICK, 4)),
        "expected the overflowing sticks to drop as an item entity: {dropped:?}"
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

    // Hold something on the cursor, as if mid-drag when the player closes
    // the screen.
    {
        let mut state = app.world_mut().resource_mut::<ServerState>();
        let client = state.clients.get_mut(&CLIENT).unwrap();
        client.cursor = Some(ItemStack::new(items::STICK, 2));
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

    // The cursor stack must have become a dropped item in the world.
    let dropped_items = &app.world().resource::<SimRes>().items.items;
    let stick_dropped = dropped_items
        .values()
        .any(|it| it.stack == ItemStack::new(items::STICK, 2));
    assert!(stick_dropped, "expected the cursor's stick stack to drop");
}

// --- roadmap M6: tools, harvest gating, durability, and furnaces ---------

/// Puts `stack` on `client_id`'s cursor and left-clicks container slot
/// `index`, the standard way these tests deposit an item into a chest or
/// furnace slot without going through a full pickup-from-somewhere chain.
fn deposit_into_container(app: &mut App, client_id: ClientId, index: usize, stack: ItemStack) {
    {
        let mut state = app.world_mut().resource_mut::<ServerState>();
        let client = state.clients.get_mut(&client_id).unwrap();
        client.cursor = Some(stack);
    }
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            client_id,
            ClientToServer::SlotClick {
                slot: SlotRef {
                    area: SlotArea::Container,
                    index: index as u8,
                },
                right: false,
                shift: false,
            },
        );
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(client_id);
}

/// Opens the furnace at `pos` for `client_id`, who must already have sent
/// `Hello`/`UpdatePlayer` and be in reach.
fn open_furnace(app: &mut App, client_id: ClientId, pos: IVec3) {
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(client_id, ClientToServer::OpenContainer { pos });
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(client_id);
}

#[test]
fn mining_stone_bare_handed_breaks_the_block_but_yields_nothing() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;
    let (_, pos) = guaranteed_air_edit(1, 1);
    seed_block(&mut app, pos, blocks::STONE);

    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(
            CLIENT,
            ClientToServer::Hello {
                name: "barehanded".into(),
            },
        );
        transport
            .0
            .push(CLIENT, ClientToServer::UpdatePlayer(save_near(pos)));
    }
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);

    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(CLIENT, ClientToServer::BreakBlock { pos, hotbar: 0 });
    app.update();
    let msgs = app
        .world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);

    assert!(
        msgs.iter().any(|m| matches!(
            m,
            ServerToClient::BlockChanged { pos: p, block } if *p == pos && block.is_air()
        )),
        "the block must still break: {msgs:?}"
    );
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, ServerToClient::InventoryUpdate { .. })),
        "bare hands changed no inventory or tool durability, so no InventoryUpdate should be \
         sent at all: {msgs:?}"
    );
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, ServerToClient::ItemSpawned { .. })),
        "bare hands must not bypass the stone harvest gate: {msgs:?}"
    );
}

#[test]
fn iron_ore_gate_uses_the_named_hotbar_slot_not_a_better_tool_elsewhere() {
    // Regression test: `BreakBlock` names the hotbar slot in hand, exactly
    // like `PlaceBlock` does, rather than the server hunting the hotbar for
    // *some* matching tool. A player carrying a wooden pickaxe in slot 0 and
    // a stone pickaxe in slot 3 must be gated by whichever one they actually
    // named -- selecting slot 3 must succeed even though slot 0 holds a
    // tool of the same kind, and selecting slot 0 must still fail even
    // though a perfectly good stone pickaxe sits right next to it.
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;
    let (_, pos_a) = guaranteed_air_edit(2, 1);
    let pos_b = IVec3::new(pos_a.x + 1, pos_a.y, pos_a.z);
    seed_block(&mut app, pos_a, blocks::IRON_ORE);
    seed_block(&mut app, pos_b, blocks::IRON_ORE);

    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(
            CLIENT,
            ClientToServer::Hello {
                name: "prospector".into(),
            },
        );
        transport
            .0
            .push(CLIENT, ClientToServer::UpdatePlayer(save_near(pos_a)));
    }
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);

    // Both pickaxes present at once, in different slots, for the whole test.
    seed_main_slot(&mut app, CLIENT, 0, ItemStack::one(items::WOODEN_PICKAXE));
    seed_main_slot(&mut app, CLIENT, 3, ItemStack::one(items::STONE_PICKAXE));

    // Naming slot 0 (wooden, too low a tier): the ore breaks, nothing is
    // dropped, but the wooden pickaxe still wears -- it is the tool that
    // was actually swung, not the stone one sitting in slot 3.
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            CLIENT,
            ClientToServer::BreakBlock {
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
                ServerToClient::BlockChanged { pos, block } if *pos == pos_a && block.is_air()
            )),
            "the ore must still break: {msgs:?}"
        );
        assert_eq!(
            latest_main_count(&msgs, items::IRON_ORE),
            Some(0),
            "naming the wooden pickaxe's slot must not harvest iron ore, even with a stone \
             pickaxe sitting elsewhere in the hotbar: {msgs:?}"
        );
        assert!(!msgs.iter().any(|m| matches!(
            m,
            ServerToClient::ItemSpawned { stack, .. } if stack.item == items::IRON_ORE
        )));
        let main = msgs
            .iter()
            .rev()
            .find_map(|m| match m {
                ServerToClient::InventoryUpdate { main, .. } => Some(main.clone()),
                _ => None,
            })
            .expect("expected an InventoryUpdate from the tool wearing");
        assert_eq!(
            main[0],
            Some(ItemStack::one(items::WOODEN_PICKAXE).with_damage(1)),
            "wear must land on the named (wooden) slot: {main:?}"
        );
        assert_eq!(
            main[3],
            Some(ItemStack::one(items::STONE_PICKAXE)),
            "the un-named stone pickaxe must be untouched: {main:?}"
        );
    }

    // Naming slot 3 (stone, meets the tier) succeeds -- the very presence of
    // the wooden pickaxe in slot 0 must not deny it.
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            CLIENT,
            ClientToServer::BreakBlock {
                pos: pos_b,
                hotbar: 3,
            },
        );
    app.update();
    let msgs = app
        .world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);
    assert_eq!(
        latest_main_count(&msgs, items::IRON_ORE),
        Some(0),
        "the mined ore must stay below the floating test platform until collected: {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| matches!(
            m,
            ServerToClient::ItemSpawned { stack, .. }
                if *stack == ItemStack::one(items::IRON_ORE)
        )),
        "naming the stone pickaxe's slot should drop iron ore: {msgs:?}"
    );
    let main = msgs
        .iter()
        .rev()
        .find_map(|m| match m {
            ServerToClient::InventoryUpdate { main, .. } => Some(main.clone()),
            _ => None,
        })
        .expect("expected an InventoryUpdate");
    assert_eq!(
        main[3],
        Some(ItemStack::one(items::STONE_PICKAXE).with_damage(1)),
        "wear must land on the named (stone) slot: {main:?}"
    );
    assert_eq!(
        main[0],
        Some(ItemStack::one(items::WOODEN_PICKAXE).with_damage(1)),
        "the un-named wooden pickaxe must not wear further: {main:?}"
    );
}

#[test]
fn a_tool_breaks_once_its_durability_is_exhausted() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;
    let (_, pos) = guaranteed_air_edit(3, 1);

    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(
            CLIENT,
            ClientToServer::Hello {
                name: "grinder".into(),
            },
        );
        transport
            .0
            .push(CLIENT, ClientToServer::UpdatePlayer(save_near(pos)));
    }
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);

    let durability = {
        let reg = app.world().resource::<CraftingRes>();
        reg.items.tool(items::WOODEN_PICKAXE).unwrap().durability
    };
    seed_main_slot(&mut app, CLIENT, 0, ItemStack::one(items::WOODEN_PICKAXE));

    // Re-seed a fresh stone block at the same position before every break
    // (bypassing the protocol, same as `seed_block` is used elsewhere) so
    // durability -- not block supply -- is what's under test.
    for _ in 0..durability {
        seed_block(&mut app, pos, blocks::STONE);
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(CLIENT, ClientToServer::BreakBlock { pos, hotbar: 0 });
        app.update();
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);
    }

    let mut state = app.world_mut().resource_mut::<ServerState>();
    let client = state.clients.get_mut(&CLIENT).unwrap();
    assert_eq!(
        client.main.slot(0),
        None,
        "the pickaxe should have broken after {durability} uses"
    );
}

#[test]
fn furnace_smelts_iron_ore_into_an_ingot_and_consumes_fuel() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;
    let (_, pos) = guaranteed_air_edit(4, 1);
    seed_block(&mut app, pos, blocks::FURNACE);

    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(
            CLIENT,
            ClientToServer::Hello {
                name: "smelter".into(),
            },
        );
        transport
            .0
            .push(CLIENT, ClientToServer::UpdatePlayer(save_near(pos)));
    }
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);

    open_furnace(&mut app, CLIENT, pos);
    deposit_into_container(
        &mut app,
        CLIENT,
        FURNACE_INPUT,
        ItemStack::one(items::IRON_ORE),
    );
    deposit_into_container(
        &mut app,
        CLIENT,
        FURNACE_FUEL,
        ItemStack::new(items::COAL, 1),
    );

    // The test harness's fixed tick interval is 1 simulated second (see
    // `new_test_app_with`'s docs); the iron ore recipe needs 10.
    for _ in 0..10 {
        app.update();
    }

    let crafting = app.world().resource::<CraftingRes>();
    let state = crafting
        .furnaces
        .states
        .get(&pos)
        .expect("expected furnace state to exist");
    assert_eq!(
        state.inv.slot(FURNACE_OUTPUT),
        Some(ItemStack::one(items::IRON_INGOT)),
        "expected a finished ingot in the output slot"
    );
    assert_eq!(
        state.inv.slot(FURNACE_INPUT),
        None,
        "the ore should have been consumed"
    );
    assert!(
        state.fuel_secs_left > 0.0,
        "coal burns much longer than the 10s smelt, so it should still be lit"
    );
}

#[test]
fn a_full_output_slot_stalls_the_furnace_instead_of_voiding_the_item() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;
    let (_, pos) = guaranteed_air_edit(4, 2);
    seed_block(&mut app, pos, blocks::FURNACE);

    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(
            CLIENT,
            ClientToServer::Hello {
                name: "hoarder".into(),
            },
        );
        transport
            .0
            .push(CLIENT, ClientToServer::UpdatePlayer(save_near(pos)));
    }
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);

    open_furnace(&mut app, CLIENT, pos);
    deposit_into_container(
        &mut app,
        CLIENT,
        FURNACE_INPUT,
        ItemStack::one(items::IRON_ORE),
    );

    // Fill the output to its stack cap directly (bypassing the protocol --
    // the output slot itself rejects a deposit, see
    // `furnace_output_can_be_taken_but_never_deposited_into`) *before*
    // adding fuel, so ignition -- which happens the moment fuel lands next
    // to a valid, room-having recipe -- never gets a chance to fire first.
    {
        let mut crafting = app.world_mut().resource_mut::<CraftingRes>();
        let max = crafting.items.max_stack(items::IRON_INGOT);
        let state = crafting.furnaces.states.get_mut(&pos).unwrap();
        state
            .inv
            .set_slot(FURNACE_OUTPUT, Some(ItemStack::new(items::IRON_INGOT, max)));
    }

    deposit_into_container(
        &mut app,
        CLIENT,
        FURNACE_FUEL,
        ItemStack::new(items::COAL, 1),
    );

    for _ in 0..10 {
        app.update();
    }

    let crafting = app.world().resource::<CraftingRes>();
    let state = crafting.furnaces.states.get(&pos).unwrap();
    assert_eq!(
        state.inv.slot(FURNACE_INPUT),
        Some(ItemStack::one(items::IRON_ORE)),
        "the ore must not be consumed while the output has no room"
    );
    assert_eq!(
        state.fuel_secs_left, 0.0,
        "fuel must never light for a smelt that can't complete"
    );
}

#[test]
fn furnace_fuel_is_not_lit_with_an_empty_input() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;
    let (_, pos) = guaranteed_air_edit(4, 3);
    seed_block(&mut app, pos, blocks::FURNACE);

    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(
            CLIENT,
            ClientToServer::Hello {
                name: "impatient".into(),
            },
        );
        transport
            .0
            .push(CLIENT, ClientToServer::UpdatePlayer(save_near(pos)));
    }
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);

    open_furnace(&mut app, CLIENT, pos);
    deposit_into_container(
        &mut app,
        CLIENT,
        FURNACE_FUEL,
        ItemStack::new(items::COAL, 1),
    );

    for _ in 0..5 {
        app.update();
    }

    let crafting = app.world().resource::<CraftingRes>();
    let state = crafting.furnaces.states.get(&pos).unwrap();
    assert_eq!(
        state.fuel_secs_left, 0.0,
        "fuel must not ignite with nothing to smelt"
    );
    assert_eq!(
        state.inv.slot(FURNACE_FUEL),
        Some(ItemStack::new(items::COAL, 1)),
        "unlit fuel must not be consumed"
    );
}

#[test]
fn furnace_slot_restrictions_reject_the_wrong_kind_of_item() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;
    let (_, pos) = guaranteed_air_edit(4, 4);
    seed_block(&mut app, pos, blocks::FURNACE);

    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(
            CLIENT,
            ClientToServer::Hello {
                name: "confused".into(),
            },
        );
        transport
            .0
            .push(CLIENT, ClientToServer::UpdatePlayer(save_near(pos)));
    }
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);
    open_furnace(&mut app, CLIENT, pos);

    // Dirt doesn't smelt and isn't fuel: both deposits must be no-ops, and
    // the cursor must keep holding what it tried to deposit.
    deposit_into_container(&mut app, CLIENT, FURNACE_INPUT, ItemStack::one(items::DIRT));
    {
        let mut state = app.world_mut().resource_mut::<ServerState>();
        let client = state.clients.get_mut(&CLIENT).unwrap();
        assert_eq!(client.cursor, Some(ItemStack::one(items::DIRT)));
    }
    deposit_into_container(&mut app, CLIENT, FURNACE_FUEL, ItemStack::one(items::DIRT));
    {
        let mut state = app.world_mut().resource_mut::<ServerState>();
        let client = state.clients.get_mut(&CLIENT).unwrap();
        assert_eq!(client.cursor, Some(ItemStack::one(items::DIRT)));
        client.cursor = None;
    }

    let crafting = app.world().resource::<CraftingRes>();
    let state = crafting.furnaces.states.get(&pos).unwrap();
    assert_eq!(state.inv.slot(FURNACE_INPUT), None);
    assert_eq!(state.inv.slot(FURNACE_FUEL), None);
}

#[test]
fn furnace_output_can_be_taken_but_never_deposited_into() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;
    let (_, pos) = guaranteed_air_edit(4, 5);
    seed_block(&mut app, pos, blocks::FURNACE);

    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(
            CLIENT,
            ClientToServer::Hello {
                name: "raider".into(),
            },
        );
        transport
            .0
            .push(CLIENT, ClientToServer::UpdatePlayer(save_near(pos)));
    }
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);
    open_furnace(&mut app, CLIENT, pos);

    {
        let mut crafting = app.world_mut().resource_mut::<CraftingRes>();
        let state = crafting.furnaces.states.get_mut(&pos).unwrap();
        state
            .inv
            .set_slot(FURNACE_OUTPUT, Some(ItemStack::one(items::IRON_INGOT)));
    }

    // Attempting to deposit (with an empty cursor there is nothing to
    // deposit, so hold something first) must not merge into the slot.
    deposit_into_container(
        &mut app,
        CLIENT,
        FURNACE_OUTPUT,
        ItemStack::one(items::IRON_INGOT),
    );
    {
        let crafting = app.world().resource::<CraftingRes>();
        let state = crafting.furnaces.states.get(&pos).unwrap();
        assert_eq!(
            state.inv.slot(FURNACE_OUTPUT),
            Some(ItemStack::one(items::IRON_INGOT)),
            "the output slot must reject a deposit even of the same item"
        );
    }
    {
        let mut state = app.world_mut().resource_mut::<ServerState>();
        let client = state.clients.get_mut(&CLIENT).unwrap();
        assert_eq!(client.cursor, Some(ItemStack::one(items::IRON_INGOT)));
    }

    // Taking it back out, on the other hand, works normally.
    {
        let mut state = app.world_mut().resource_mut::<ServerState>();
        let client = state.clients.get_mut(&CLIENT).unwrap();
        client.cursor = None;
    }
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(
            CLIENT,
            ClientToServer::SlotClick {
                slot: SlotRef {
                    area: SlotArea::Container,
                    index: FURNACE_OUTPUT as u8,
                },
                right: false,
                shift: false,
            },
        );
    app.update();
    let crafting = app.world().resource::<CraftingRes>();
    let state = crafting.furnaces.states.get(&pos).unwrap();
    assert_eq!(
        state.inv.slot(FURNACE_OUTPUT),
        None,
        "the output slot should have been emptied by the take"
    );
}

#[test]
fn breaking_a_furnace_drops_its_contents() {
    let mut app = new_test_app_with(
        MockTransport::default(),
        0,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;
    let (_, pos) = guaranteed_air_edit(4, 6);
    seed_block(&mut app, pos, blocks::FURNACE);

    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport.0.push(
            CLIENT,
            ClientToServer::Hello {
                name: "wrecker".into(),
            },
        );
        transport
            .0
            .push(CLIENT, ClientToServer::UpdatePlayer(save_near(pos)));
    }
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);

    open_furnace(&mut app, CLIENT, pos);
    // Input only, deliberately with no fuel: depositing fuel next to a
    // valid input ignites it immediately (see
    // `furnace_smelts_iron_ore_into_an_ingot_and_consumes_fuel`), and a
    // burning fuel unit has no leftover item to drop -- it's already
    // "spent". This test is about slot *contents* surviving the break, so it
    // sticks to what actually stays a physical item: the ore sitting unlit
    // in the input slot.
    deposit_into_container(
        &mut app,
        CLIENT,
        FURNACE_INPUT,
        ItemStack::one(items::IRON_ORE),
    );

    // A pickaxe of high enough tier so breaking the furnace itself also
    // succeeds, keeping the test focused on the contents rather than the
    // harvest gate.
    seed_main_slot(&mut app, CLIENT, 8, ItemStack::one(items::WOODEN_PICKAXE));
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(CLIENT, ClientToServer::BreakBlock { pos, hotbar: 8 });
    app.update();
    let msgs = app
        .world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);

    assert!(
        msgs.iter().any(|m| matches!(
            m,
            ServerToClient::BlockChanged { pos: p, block } if *p == pos && block.is_air()
        )),
        "expected the furnace to break: {msgs:?}"
    );
    assert!(
        msgs.iter()
            .any(|m| matches!(m, ServerToClient::ContainerClosed)),
        "expected the open furnace UI to close: {msgs:?}"
    );

    let dropped_items = &app.world().resource::<SimRes>().items.items;
    assert!(
        dropped_items
            .values()
            .any(|it| it.stack == ItemStack::one(items::IRON_ORE)),
        "expected the furnace's input to drop"
    );

    let crafting = app.world().resource::<CraftingRes>();
    assert!(
        !crafting.furnaces.states.contains_key(&pos),
        "the broken furnace's state should be forgotten"
    );
}

#[test]
fn furnace_state_survives_save_and_load() {
    let dir = tempfile::tempdir().expect("failed to create tempdir");
    let world_dir = dir.path().to_path_buf();
    let (_, pos) = guaranteed_air_edit(4, 7);

    {
        let mut app = new_test_app_with(
            MockTransport::default(),
            0,
            Persistence::new(Some(world_dir.clone()), 9999.0),
            GameMode::Survival,
        );
        const CLIENT: ClientId = 1;
        seed_block(&mut app, pos, blocks::FURNACE);
        {
            let mut transport = app
                .world_mut()
                .resource_mut::<TransportRes<MockTransport>>();
            transport.0.push(
                CLIENT,
                ClientToServer::Hello {
                    name: "keeper".into(),
                },
            );
            transport
                .0
                .push(CLIENT, ClientToServer::UpdatePlayer(save_near(pos)));
        }
        app.update();
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(CLIENT);

        open_furnace(&mut app, CLIENT, pos);
        deposit_into_container(
            &mut app,
            CLIENT,
            FURNACE_INPUT,
            ItemStack::one(items::IRON_ORE),
        );
        deposit_into_container(
            &mut app,
            CLIENT,
            FURNACE_FUEL,
            ItemStack::new(items::COAL, 1),
        );

        // Stop halfway through the 10s smelt so there is real in-progress
        // state (not just slot contents) to verify survives a reload.
        for _ in 0..5 {
            app.update();
        }

        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(CLIENT, ClientToServer::Goodbye);
        app.update();
    }

    let mut reload = Persistence::new(Some(world_dir), 9999.0);
    let loaded = reload
        .load()
        .expect("load failed")
        .expect("expected a saved world");
    assert_eq!(loaded.furnaces.len(), 1, "expected one saved furnace");
    let (saved_pos, record) = &loaded.furnaces[0];
    assert_eq!(*saved_pos, pos);
    assert_eq!(
        record.slots[FURNACE_INPUT],
        Some(ItemStack::one(items::IRON_ORE))
    );
    assert_eq!(
        record.slots[FURNACE_FUEL], None,
        "the one fuel unit should be burning, not sitting in the slot"
    );
    assert!(
        record.cook_secs > 0.0,
        "expected partial cook progress to survive: {record:?}"
    );
    assert!(
        record.fuel_secs_left > 0.0,
        "expected the burning fuel's remaining time to survive"
    );
}

#[test]
fn torches_craft_place_persist_and_can_be_recovered_by_hand() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = new_test_app_with(
        MockTransport::default(),
        42,
        Persistence::new(Some(dir.path().to_path_buf()), 9999.0),
        GameMode::Survival,
    );
    const CLIENT: ClientId = 1;
    join(&mut app, CLIENT, "caver");
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 0.125;
    seed_main_slot(&mut app, CLIENT, 0, ItemStack::one(items::COAL));
    seed_main_slot(&mut app, CLIENT, 1, ItemStack::one(items::STICK));
    let recipe = app
        .world()
        .resource::<CraftingRes>()
        .recipes
        .recipes()
        .iter()
        .position(|r| r.output.item == items::TORCH)
        .unwrap() as u16;
    let msgs = craft(&mut app, CLIENT, recipe, false);
    assert_eq!(latest_main_count(&msgs, items::TORCH), Some(4));
    assert_eq!(latest_main_count(&msgs, items::COAL), Some(0));
    assert_eq!(latest_main_count(&msgs, items::STICK), Some(0));

    let pos = IVec3::new(16, 12, 16);
    seed_block(&mut app, pos, blocks::AIR);
    {
        let mut transport = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>();
        transport
            .0
            .push(CLIENT, ClientToServer::UpdatePlayer(save_near(pos)));
        transport
            .0
            .push(CLIENT, ClientToServer::PlaceBlock { pos, hotbar: 0 });
    }
    app.update();
    let msgs = app
        .world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);
    assert_eq!(latest_main_count(&msgs, items::TORCH), Some(3));
    let (chunk_pos, local) = split_block_pos(pos);
    assert_eq!(
        app.world().resource::<ChunkCache>().chunks[&chunk_pos].get(local.as_uvec3()),
        blocks::TORCH
    );

    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(CLIENT, ClientToServer::Goodbye);
    app.update();
    let saved = Persistence::new(Some(dir.path().to_path_buf()), 9999.0)
        .load()
        .unwrap()
        .unwrap();
    assert_eq!(
        saved
            .chunks
            .iter()
            .find(|(p, _)| *p == chunk_pos)
            .unwrap()
            .1
            .get(local.as_uvec3()),
        blocks::TORCH
    );
    assert_eq!(
        saved.players["caver"]
            .main
            .iter()
            .flatten()
            .filter(|s| s.item == items::TORCH)
            .map(|s| s.count)
            .sum::<u32>(),
        3
    );

    join(&mut app, CLIENT, "caver");
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(CLIENT, ClientToServer::UpdatePlayer(save_near(pos)));
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(CLIENT, ClientToServer::BreakBlock { pos, hotbar: 0 });
    app.update();
    let msgs = app
        .world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);
    assert_eq!(latest_main_count(&msgs, items::TORCH), None);
    assert_eq!(
        app.world().resource::<ServerState>().clients[&CLIENT]
            .main
            .slot(0),
        Some(ItemStack::new(items::TORCH, 3)),
        "breaking a torch by hand must not immediately credit it"
    );
    let (dropped_id, rest_pos) = msgs
        .iter()
        .find_map(|m| match m {
            ServerToClient::ItemSpawned { id, pos, stack }
                if *stack == ItemStack::one(items::TORCH) =>
            {
                Some((*id, *pos))
            }
            _ => None,
        })
        .expect("breaking a torch by hand must drop a recoverable item");
    assert_eq!(
        app.world().resource::<ChunkCache>().chunks[&chunk_pos].get(local.as_uvec3()),
        blocks::AIR
    );
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 0.5;
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(CLIENT, ClientToServer::UpdatePlayer(save_at(rest_pos)));
    app.update();
    let msgs = app
        .world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(CLIENT);
    assert_eq!(latest_main_count(&msgs, items::TORCH), Some(4));
    assert!(msgs.iter().any(|m| matches!(
        m,
        ServerToClient::ItemDespawned { id } if *id == dropped_id
    )));
}

/// Generates disposable saved worlds for comparing the real server/renderer
/// lighting path. Run explicitly; ordinary tests never write outside tempdirs.
#[test]
#[ignore = "writes visual verification fixtures under target/m7-qa"]
fn write_lighting_verification_worlds() {
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/m7-qa");
    for lit in [false, true] {
        let dir = base.join(if lit { "lit" } else { "dark" });
        let mut persistence = Persistence::new(Some(dir), 9999.0);
        let mut chunks = HashMap::new();
        // Solid terrain encloses a room across the x=32 chunk boundary.
        // Both variants use the same camera and room; only the torch differs.
        for cx in 0..=1 {
            let chunk_pos = IVec3::new(cx, 0, 0);
            let mut chunk = Chunk::filled(blocks::STONE);
            for y in 8..=14 {
                for z in 9..=24 {
                    for x in 23..=40 {
                        let pos = IVec3::new(x, y, z);
                        let (cp, local) = split_block_pos(pos);
                        if cp == chunk_pos {
                            chunk.set(local.as_uvec3(), blocks::AIR);
                        }
                    }
                }
            }
            // Ore patches on the far wall make brightness/color readable.
            for x in 27..=36 {
                let (cp, local) = split_block_pos(IVec3::new(x, 10, 8));
                if cp == chunk_pos {
                    chunk.set(
                        local.as_uvec3(),
                        if x < 32 {
                            blocks::IRON_ORE
                        } else {
                            blocks::COAL_ORE
                        },
                    );
                }
            }
            if lit {
                let (cp, local) = split_block_pos(IVec3::new(31, 8, 15));
                if cp == chunk_pos {
                    chunk.set(local.as_uvec3(), blocks::TORCH);
                }
            }
            chunks.insert(chunk_pos, chunk);
            persistence.mark_chunk_dirty(chunk_pos);
        }
        let mut main = vec![None; MAIN_INVENTORY_SIZE];
        main[0] = Some(ItemStack::new(items::TORCH, 16));
        let players = HashMap::from([(
            "player".to_string(),
            PlayerRecord {
                save: PlayerSave {
                    pos: Vec3::new(32.5, 8.0, 22.5),
                    yaw: 0.0,
                    pitch: -0.12,
                },
                hp: MAX_HP,
                main,
            },
        )]);
        persistence
            .save(
                42,
                GameMode::Creative,
                0.25,
                &players,
                &[],
                &[],
                &[],
                &chunks,
            )
            .unwrap();
    }
    eprintln!("Lighting verification worlds: {}", base.display());
}

/// A daylight gallery exercises every block face, item icon and greedy
/// floor repetition through the normal persisted-world rendering path.
#[test]
#[ignore = "writes a visual verification fixture under target/texture-qa"]
fn write_texture_verification_world() {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/texture-qa/gallery");
    let mut persistence = Persistence::new(Some(dir.clone()), 9999.0);
    let chunk_pos = IVec3::new(0, 2, 0);
    let mut chunk = Chunk::filled(blocks::AIR);
    for z in 0..32 {
        for x in 0..32 {
            chunk.set(UVec3::new(x, 12, z), blocks::STONE);
        }
    }
    for id in 1..=blocks::TORCH.0 {
        let index = u32::from(id - 1);
        chunk.set(
            UVec3::new(8 + index % 5 * 4, 13, 12 + index / 5 * 4),
            tsumiki_world::BlockId(id),
        );
    }
    let registry = ItemRegistry::prototype();
    let mut main = vec![None; MAIN_INVENTORY_SIZE];
    for id in 1..registry.len() as u16 {
        main[id as usize - 1] = Some(ItemStack::one(tsumiki_world::ItemId(id)));
    }
    let players = HashMap::from([(
        "player".to_string(),
        PlayerRecord {
            save: PlayerSave {
                pos: Vec3::new(16.5, 80.0, 3.5),
                yaw: std::f32::consts::PI,
                pitch: -0.30,
            },
            hp: MAX_HP,
            main,
        },
    )]);
    let drops: Vec<_> = [items::LOG, items::IRON_PICKAXE, items::IRON_INGOT]
        .into_iter()
        .enumerate()
        .map(|(i, item)| crate::persist::ItemRecord {
            pos: Vec3::new(14.0 + i as f32 * 2.0, 77.3, 9.5),
            stack: ItemStack::one(item),
        })
        .collect();
    persistence.mark_chunk_dirty(chunk_pos);
    persistence
        .save(
            42,
            GameMode::Creative,
            0.25,
            &players,
            &drops,
            &[],
            &[],
            &HashMap::from([(chunk_pos, chunk)]),
        )
        .unwrap();
    eprintln!("Texture verification world: {}", dir.display());
}

#[path = "demo_tests.rs"]
mod demo_tests;

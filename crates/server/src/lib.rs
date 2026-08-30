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

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::prelude::*;

use bevy_math::IVec3;
use tsumiki_protocol::{ClientId, ClientToServer, ServerToClient, ServerTransport};
use tsumiki_world::{Chunk, WorldGenerator, WORLD_HEIGHT_CHUNKS};

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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { seed: 0, tick_hz: 30.0 }
    }
}

/// Wraps the transport as a Bevy resource. Generic over the transport type so
/// both the in-process and (future) renet transports can drive the same
/// server systems.
#[derive(Resource)]
struct TransportRes<T: ServerTransport>(T);

#[derive(Resource)]
struct WorldGenRes(WorldGenerator);

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
pub fn run_server<T: ServerTransport>(transport: T, config: ServerConfig) {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(
        1.0 / config.tick_hz,
    ))));
    app.insert_resource(TransportRes(transport));
    app.insert_resource(WorldGenRes(WorldGenerator::new(config.seed)));
    app.init_resource::<ChunkCache>();
    app.init_resource::<ServerState>();
    app.add_systems(Update, tick_server::<T>);
    app.run();
}

fn tick_server<T: ServerTransport>(
    mut transport: ResMut<TransportRes<T>>,
    mut state: ResMut<ServerState>,
    world_gen: Res<WorldGenRes>,
    mut cache: ResMut<ChunkCache>,
) {
    let ServerState { clients, pending, pending_set, rotation } = &mut *state;

    while let Some((client_id, msg)) = transport.0.try_recv() {
        match msg {
            ClientToServer::Hello { .. } => {
                transport.0.send(client_id, ServerToClient::Welcome { client_id });
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
        transport.0.send(client_id, ServerToClient::ChunkData { pos, chunk });

        served += 1;
        if !queue.is_empty() {
            rotation.push_back(client_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Instant;

    use tsumiki_protocol::local::{pair, LOCAL_CLIENT_ID};
    use tsumiki_protocol::ClientTransport;

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

    /// Regression test for the starvation bug: a flooding client must not
    /// delay another client's very first chunk by more than one tick.
    #[test]
    fn round_robin_prevents_starvation() {
        let mut app = App::new();
        app.insert_resource(TransportRes(MockTransport::default()));
        app.insert_resource(WorldGenRes(WorldGenerator::new(0)));
        app.init_resource::<ChunkCache>();
        app.init_resource::<ServerState>();
        app.add_systems(Update, tick_server::<MockTransport>);

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
        app.world_mut().resource_mut::<TransportRes<MockTransport>>().0.push(
            CLIENT_B,
            ClientToServer::RequestChunks { positions: vec![IVec3::new(0, 0, 0)] },
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
        let mut app = App::new();
        app.insert_resource(TransportRes(MockTransport::default()));
        app.insert_resource(WorldGenRes(WorldGenerator::new(0)));
        app.init_resource::<ChunkCache>();
        app.init_resource::<ServerState>();
        app.add_systems(Update, tick_server::<MockTransport>);

        const CLIENT: ClientId = 1;
        let oversized: Vec<IVec3> = (0..10_000).map(|i| IVec3::new(i, 0, 0)).collect();
        app.world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .push(CLIENT, ClientToServer::RequestChunks { positions: oversized });
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

        // run_server never returns; leaking the thread is fine for a test.
        thread::spawn(move || {
            run_server(
                server_transport,
                ServerConfig { seed: 42, tick_hz: 60.0 },
            );
        });

        client.send(ClientToServer::Hello { name: "tester".into() });

        let welcome = recv_within(&mut client, Duration::from_secs(5))
            .expect("expected a Welcome reply");
        match welcome {
            ServerToClient::Welcome { client_id } => assert_eq!(client_id, LOCAL_CLIENT_ID),
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
        client.send(ClientToServer::RequestChunks { positions: vec![valid_a] });
        assert!(
            recv_within(&mut client, Duration::from_millis(500)).is_none(),
            "server resent an already-sent chunk"
        );
    }
}

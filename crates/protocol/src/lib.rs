//! Client/server messages and the transport abstraction (design.md §1.2).
//!
//! Game logic on either side speaks only through these types. Whether the
//! peer lives in the same process (singleplayer) or across the network
//! (multiplayer, later via renet) is invisible to it.
//!
//! Messages derive `Serialize`/`Deserialize` for the future network
//! transport; the in-process transport passes them as typed values without
//! serializing.

use bevy_math::{IVec3, Vec3};
use serde::{Deserialize, Serialize};
use tsumiki_world::{BlockId, Chunk};

/// Server-assigned identifier of a connected client.
pub type ClientId = u64;

/// Persisted player state, echoed back on reconnect.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlayerSave {
    /// Feet position, world space.
    pub pos: Vec3,
    pub yaw: f32,
    pub pitch: f32,
}

/// A world's rules, fixed per world (server setting, persisted with it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GameMode {
    /// Blocks must be mined (taking time) and placing consumes inventory;
    /// players have health and can die.
    Survival,
    /// Free instant editing, no inventory constraints, no health, flying.
    Creative,
}

/// What hurt a player. Damage is client-detected (movement is
/// client-authoritative) and server-applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DamageCause {
    Fall,
    Drown,
}

/// Maximum health.
pub const MAX_HP: u16 = 20;

/// Client-side interaction reach in blocks. The server validates edits with
/// [`SERVER_REACH`] instead — deliberately looser, since it sees the
/// player's position only through ~10 Hz `UpdatePlayer` samples.
pub const REACH: f32 = 5.0;
pub const SERVER_REACH: f32 = 7.0;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ClientToServer {
    Hello {
        name: String,
    },
    /// Chunk positions the client wants, in chunk coordinates.
    RequestChunks {
        positions: Vec<IVec3>,
    },
    /// LOD chunk positions the client wants (design.md §3). `level` is
    /// `1..=tsumiki_world::lod::MAX_LOD`; positions are in level-L chunk
    /// coordinates (one level-L chunk spans `32 * 2^level` blocks).
    RequestLodChunks {
        level: u8,
        positions: Vec<IVec3>,
    },
    /// A completed block break (in survival, the client sends this after the
    /// hold-to-mine time elapses; in creative, immediately). The server
    /// validates (reach, block exists and is breakable) and, on success,
    /// broadcasts [`ServerToClient::BlockChanged`] to air and credits the
    /// block to the miner's inventory in survival (overflow drops as an
    /// item entity).
    BreakBlock {
        pos: IVec3,
    },
    /// Requests placing `block` at `pos`. The server validates (reach,
    /// destination replaceable, valid placeable block id, and in survival:
    /// inventory holds one, which is consumed) and broadcasts
    /// [`ServerToClient::BlockChanged`] on success.
    PlaceBlock {
        pos: IVec3,
        block: BlockId,
    },
    /// Client-detected damage (see [`DamageCause`]). The server clamps the
    /// amount, ignores it in creative mode, and answers with
    /// [`ServerToClient::HealthUpdate`] (or [`ServerToClient::Died`]).
    ReportDamage {
        amount: u16,
        cause: DamageCause,
    },
    /// Request respawn after death (only meaningful while dead).
    Respawn,
    /// Periodic (~10 Hz) player state for persistence and, later,
    /// replication. Client-authoritative for now.
    UpdatePlayer(PlayerSave),
    /// Graceful disconnect; the server saves the world before dropping the
    /// client.
    Goodbye,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServerToClient {
    Welcome {
        client_id: ClientId,
        /// Saved state from a previous session, if any; `None` means the
        /// client decides its own fresh spawn.
        player: Option<PlayerSave>,
        /// The world's rules. Fixed for the session.
        game_mode: GameMode,
        /// Current time of day in `[0, 1)`: 0.0 = sunrise, 0.25 = noon,
        /// 0.5 = sunset. The client advances it locally between
        /// [`ServerToClient::TimeUpdate`]s.
        time_of_day: f32,
    },
    ChunkData {
        pos: IVec3,
        chunk: Chunk,
    },
    /// A LOD chunk (same palette-compressed representation, cells instead of
    /// blocks). Also re-sent unsolicited when a block edit invalidates a LOD
    /// chunk a client already holds.
    LodChunkData {
        level: u8,
        pos: IVec3,
        chunk: Chunk,
    },
    /// An accepted block edit. Sent to every client, including the one that
    /// requested it (which treats it as idempotent confirmation).
    BlockChanged {
        pos: IVec3,
        block: BlockId,
    },
    /// A player became visible to this client: they connected nearby, or
    /// moved into interest range. Carries everything needed to spawn them.
    PlayerJoined {
        id: ClientId,
        name: String,
        state: PlayerSave,
    },
    /// A player stopped being visible to this client: they disconnected, or
    /// moved out of interest range. The client despawns them either way.
    PlayerLeft {
        id: ClientId,
    },
    /// Movement update for a player currently visible to this client.
    PlayerMoved {
        id: ClientId,
        state: PlayerSave,
    },
    /// Full snapshot of the receiving player's inventory (small: the item
    /// catalog is tiny). Sent on join and whenever it changes.
    InventoryUpdate {
        counts: Vec<(BlockId, u32)>,
    },
    /// The receiving player's health changed.
    HealthUpdate {
        hp: u16,
    },
    /// The receiving player died (their inventory dropped at `at`); the
    /// client shows the death screen and eventually sends
    /// [`ClientToServer::Respawn`].
    Died {
        at: Vec3,
    },
    /// A dropped item appeared (already at rest; items don't move once
    /// spawned, so there is no movement sync for them).
    ItemSpawned {
        id: u64,
        pos: Vec3,
        block: BlockId,
        count: u32,
    },
    /// A dropped item was picked up, merged away, or expired.
    ItemDespawned {
        id: u64,
    },
    /// Periodic time-of-day resync (see `Welcome::time_of_day`).
    TimeUpdate {
        time_of_day: f32,
    },
}

/// Server-side endpoint: receives from any client, sends to a specific one.
///
/// The pump hooks exist for transports that need driving (UDP): the server
/// calls [`tick`](Self::tick) once at the start of every server tick and
/// [`flush`](Self::flush) once at the end. A network transport synthesizes a
/// [`ClientToServer::Goodbye`] when a client disconnects without one, so
/// game logic never needs a separate disconnect path.
pub trait ServerTransport: Send + Sync + 'static {
    fn try_recv(&mut self) -> Option<(ClientId, ClientToServer)>;
    /// Sending to a disconnected client is a no-op, not an error.
    fn send(&mut self, to: ClientId, msg: ServerToClient);
    /// Advance the transport (receive packets, timeouts). `dt` is the time
    /// since the previous tick, in seconds.
    fn tick(&mut self, dt: f32) {
        let _ = dt;
    }
    /// Push buffered outgoing messages onto the wire.
    fn flush(&mut self) {}
}

/// Client-side endpoint, connected to one server.
///
/// Pump hooks as on [`ServerTransport`]: the client calls
/// [`tick`](Self::tick) once at the start of every frame and
/// [`flush`](Self::flush) once at the end.
pub trait ClientTransport: Send + Sync + 'static {
    /// Sending after the server is gone is a no-op, not an error.
    fn send(&mut self, msg: ClientToServer);
    fn try_recv(&mut self) -> Option<ServerToClient>;
    fn tick(&mut self, dt: f32) {
        let _ = dt;
    }
    fn flush(&mut self) {}
}

pub mod local {
    //! In-process transport for singleplayer: typed messages over unbounded
    //! channels, no serialization, a single hardcoded client.

    use super::*;
    use crossbeam_channel::{Receiver, Sender, unbounded};

    /// The one client id the local transport serves.
    pub const LOCAL_CLIENT_ID: ClientId = 1;

    pub struct LocalServerTransport {
        rx: Receiver<ClientToServer>,
        tx: Sender<ServerToClient>,
    }

    pub struct LocalClientTransport {
        tx: Sender<ClientToServer>,
        rx: Receiver<ServerToClient>,
    }

    /// Creates a connected (server, client) endpoint pair.
    pub fn pair() -> (LocalServerTransport, LocalClientTransport) {
        let (c2s_tx, c2s_rx) = unbounded();
        let (s2c_tx, s2c_rx) = unbounded();
        (
            LocalServerTransport {
                rx: c2s_rx,
                tx: s2c_tx,
            },
            LocalClientTransport {
                tx: c2s_tx,
                rx: s2c_rx,
            },
        )
    }

    impl ServerTransport for LocalServerTransport {
        fn try_recv(&mut self) -> Option<(ClientId, ClientToServer)> {
            self.rx.try_recv().ok().map(|msg| (LOCAL_CLIENT_ID, msg))
        }

        fn send(&mut self, to: ClientId, msg: ServerToClient) {
            debug_assert_eq!(to, LOCAL_CLIENT_ID);
            let _ = self.tx.send(msg);
        }
    }

    impl ClientTransport for LocalClientTransport {
        fn send(&mut self, msg: ClientToServer) {
            let _ = self.tx.send(msg);
        }

        fn try_recv(&mut self) -> Option<ServerToClient> {
            self.rx.try_recv().ok()
        }
    }
}

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
    /// Requests a block edit (break = set to air). The server validates and,
    /// on success, broadcasts [`ServerToClient::BlockChanged`] to everyone
    /// (including the sender).
    SetBlock {
        pos: IVec3,
        block: BlockId,
    },
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

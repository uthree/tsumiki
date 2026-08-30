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
    /// An accepted block edit. Sent to every client, including the one that
    /// requested it (which treats it as idempotent confirmation).
    BlockChanged {
        pos: IVec3,
        block: BlockId,
    },
}

/// Server-side endpoint: receives from any client, sends to a specific one.
pub trait ServerTransport: Send + Sync + 'static {
    fn try_recv(&mut self) -> Option<(ClientId, ClientToServer)>;
    /// Sending to a disconnected client is a no-op, not an error.
    fn send(&mut self, to: ClientId, msg: ServerToClient);
}

/// Client-side endpoint, connected to one server.
pub trait ClientTransport: Send + Sync + 'static {
    /// Sending after the server is gone is a no-op, not an error.
    fn send(&mut self, msg: ClientToServer);
    fn try_recv(&mut self) -> Option<ServerToClient>;
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

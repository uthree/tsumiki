//! Network transports over UDP (renet + netcode), implementing the
//! transport traits from `tsumiki-protocol` (design.md §1.2/§1.3).
//!
//! Messages are postcard-serialized. Channel policy:
//! - Reliable-ordered: everything except movement (Hello/Welcome, chunk
//!   requests and data, block edits, player joined/left, goodbye).
//! - Unreliable: `UpdatePlayer` / `PlayerMoved` (latest-state-wins; loss is
//!   fine because a fresh state follows immediately).
//!
//! Disconnects are surfaced to game logic as a synthesized
//! [`ClientToServer::Goodbye`], per the trait contract — the server never
//! needs a separate disconnect path.
//!
//! Authentication is netcode "unsecure" (LAN play, M2). Real auth is a
//! later concern.

mod client;
mod server;

pub use client::NetClientTransport;
pub use server::NetServerTransport;

use renet::DefaultChannel;
use tsumiki_protocol::{ClientToServer, ServerToClient};

/// Shared netcode protocol id — both sides must agree. Bump when the wire
/// format or block/item catalog changes incompatibly.
pub const PROTOCOL_ID: u64 = 4;

/// Default UDP port for dedicated servers.
pub const DEFAULT_PORT: u16 = 24571;

/// Maximum simultaneous clients a [`NetServerTransport`] accepts (spec
/// requires at least 16; picked with headroom).
pub const MAX_CLIENTS: usize = 64;

/// Channel id for reliable-ordered traffic (see module docs for policy).
fn reliable_channel_id() -> u8 {
    DefaultChannel::ReliableOrdered.into()
}

/// Channel id for unreliable, latest-wins movement traffic.
fn unreliable_channel_id() -> u8 {
    DefaultChannel::Unreliable.into()
}

/// Picks the channel a [`ClientToServer`] message must travel on.
fn client_msg_channel(msg: &ClientToServer) -> u8 {
    match msg {
        ClientToServer::UpdatePlayer(_) => unreliable_channel_id(),
        _ => reliable_channel_id(),
    }
}

/// Picks the channel a [`ServerToClient`] message must travel on.
fn server_msg_channel(msg: &ServerToClient) -> u8 {
    match msg {
        ServerToClient::PlayerMoved { .. } => unreliable_channel_id(),
        _ => reliable_channel_id(),
    }
}

/// Wraps any [`std::fmt::Display`]-able error as an [`std::io::Error`], for
/// bubbling up renet/netcode construction failures through the `io::Result`
/// signatures the transport constructors use.
fn io_err(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

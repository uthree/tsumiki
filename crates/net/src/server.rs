//! Server-side UDP transport (renet + netcode).

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use renet::{ConnectionConfig, RenetServer, ServerEvent};
use renet_netcode::{NetcodeServerTransport, ServerAuthentication, ServerConfig};

use tsumiki_protocol::{ClientId, ClientToServer, ServerToClient, ServerTransport};

use crate::{
    MAX_CLIENTS, PROTOCOL_ID, reliable_channel_id, server_msg_channel, unreliable_channel_id,
};

/// Server-side UDP transport. One instance serves all clients.
pub struct NetServerTransport {
    server: RenetServer,
    transport: NetcodeServerTransport,
    recv_queue: VecDeque<(ClientId, ClientToServer)>,
}

impl NetServerTransport {
    /// Binds a UDP socket on `bind` and starts listening.
    pub fn bind(bind: SocketAddr) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(bind)?;
        let public_addr = socket.local_addr()?;

        let server = RenetServer::new(ConnectionConfig::default());
        let server_config = ServerConfig {
            current_time: Duration::ZERO,
            max_clients: MAX_CLIENTS,
            protocol_id: PROTOCOL_ID,
            public_addresses: vec![public_addr],
            authentication: ServerAuthentication::Unsecure,
        };
        let transport = NetcodeServerTransport::new(server_config, socket)?;

        Ok(Self {
            server,
            transport,
            recv_queue: VecDeque::new(),
        })
    }

    /// The address actually bound (useful when `bind` used port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.transport.addresses()[0]
    }
}

impl ServerTransport for NetServerTransport {
    fn try_recv(&mut self) -> Option<(ClientId, ClientToServer)> {
        self.recv_queue.pop_front()
    }

    fn send(&mut self, to: ClientId, msg: ServerToClient) {
        // Per the trait doc, sending to a disconnected or unknown client is a
        // no-op, not an error; `is_connected` is false for both cases since
        // a removed connection is dropped from renet's connection map.
        if !self.server.is_connected(to) {
            return;
        }
        let channel = server_msg_channel(&msg);
        match postcard::to_allocvec(&msg) {
            Ok(bytes) => self.server.send_message(to, channel, bytes),
            Err(e) => eprintln!("tsumiki-net: failed to serialize ServerToClient: {e}"),
        }
    }

    fn tick(&mut self, dt: f32) {
        let duration = Duration::from_secs_f32(dt.max(0.0));

        self.server.update(duration);
        if let Err(e) = self.transport.update(duration, &mut self.server) {
            eprintln!("tsumiki-net: server transport update error: {e}");
        }

        while let Some(event) = self.server.get_event() {
            match event {
                ServerEvent::ClientConnected { .. } => {
                    // Nothing to do yet: game logic only learns about a new
                    // client once it sends `Hello`, matching the in-process
                    // transport's behavior.
                }
                ServerEvent::ClientDisconnected { client_id, .. } => {
                    // Always synthesize Goodbye, even if the client already
                    // sent an explicit one before disconnecting (e.g. it
                    // sent Goodbye then tore down its socket immediately).
                    // crates/server/src/lib.rs's Goodbye handler is
                    // idempotent: it saves the world (a no-op save when
                    // nothing is dirty) and removes the client from maps via
                    // HashMap::remove/Vec::retain, which are no-ops when the
                    // entry is already gone. So a duplicate Goodbye is
                    // harmless, and always synthesizing is simpler and
                    // correct than tracking per-connection "already said
                    // goodbye" state.
                    self.recv_queue
                        .push_back((client_id, ClientToServer::Goodbye));
                }
            }
        }

        let reliable = reliable_channel_id();
        let unreliable = unreliable_channel_id();
        for client_id in self.server.clients_id() {
            for channel in [reliable, unreliable] {
                while let Some(bytes) = self.server.receive_message(client_id, channel) {
                    match postcard::from_bytes::<ClientToServer>(&bytes) {
                        Ok(msg) => self.recv_queue.push_back((client_id, msg)),
                        Err(e) => {
                            eprintln!(
                                "tsumiki-net: dropping malformed packet from {client_id}: {e}"
                            );
                        }
                    }
                }
            }
        }
    }

    fn flush(&mut self) {
        self.transport.send_packets(&mut self.server);
    }
}

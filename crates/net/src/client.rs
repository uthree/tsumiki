//! Client-side UDP transport (renet + netcode).

use std::collections::VecDeque;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use renet::{ConnectionConfig, RenetClient};
use renet_netcode::{ClientAuthentication, NetcodeClientTransport};

use tsumiki_protocol::{ClientToServer, ClientTransport, ServerToClient};

use crate::{PROTOCOL_ID, client_msg_channel, io_err, reliable_channel_id, unreliable_channel_id};

/// Client-side UDP transport connected to one server.
pub struct NetClientTransport {
    client: RenetClient,
    transport: NetcodeClientTransport,
    recv_queue: VecDeque<ServerToClient>,
    /// Messages sent before the netcode handshake finished. renet's
    /// `RenetClient` will happily queue messages while "connecting" (it only
    /// refuses to queue once disconnected), but `NetcodeClientTransport`
    /// refuses to encrypt/send *any* application payload until the netcode
    /// handshake reaches `Connected` (`generate_payload_packet` errors
    /// otherwise) — and by the time that error surfaces, the message has
    /// already been popped out of `RenetClient`'s internal channel and would
    /// simply be discarded. So messages sent while connecting are buffered
    /// here instead of ever touching `client`, and are only handed to
    /// `client.send_message` once the handshake completes.
    pending_before_connect: VecDeque<(u8, Vec<u8>)>,
}

/// Generates a locally-unique-enough client id without a `rand` dependency:
/// current time + process id + a per-process atomic counter, hashed
/// together. The counter matters because `SystemTime`'s resolution on
/// Windows is coarse (tens of milliseconds), so two clients created back to
/// back in the same process (as tests do) could otherwise hash to the same
/// id and collide.
fn random_client_id() -> u64 {
    use std::hash::{Hash, Hasher};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
    hasher.finish()
}

impl NetClientTransport {
    /// Starts connecting to `server` (non-blocking; messages queue until the
    /// connection completes).
    pub fn connect(server: SocketAddr) -> std::io::Result<Self> {
        let bind_addr: SocketAddr = match server {
            SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
        };
        let socket = UdpSocket::bind(bind_addr)?;

        let authentication = ClientAuthentication::Unsecure {
            protocol_id: PROTOCOL_ID,
            client_id: random_client_id(),
            server_addr: server,
            user_data: None,
        };
        let transport =
            NetcodeClientTransport::new(Duration::ZERO, authentication, socket).map_err(io_err)?;
        let client = RenetClient::new(ConnectionConfig::default());

        Ok(Self {
            client,
            transport,
            recv_queue: VecDeque::new(),
            pending_before_connect: VecDeque::new(),
        })
    }
}

impl ClientTransport for NetClientTransport {
    fn send(&mut self, msg: ClientToServer) {
        // Sending after the server is gone is a no-op, not an error.
        if self.client.is_disconnected() {
            return;
        }

        let channel = client_msg_channel(&msg);
        let bytes = match postcard::to_allocvec(&msg) {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!("tsumiki-net: failed to serialize ClientToServer: {e}");
                return;
            }
        };

        if self.client.is_connected() {
            self.client.send_message(channel, bytes);
        } else {
            // Still connecting (or the very first tick hasn't run yet) —
            // buffer so this is never lost. See `pending_before_connect`.
            self.pending_before_connect.push_back((channel, bytes));
        }
    }

    fn try_recv(&mut self) -> Option<ServerToClient> {
        self.recv_queue.pop_front()
    }

    fn tick(&mut self, dt: f32) {
        let duration = Duration::from_secs_f32(dt.max(0.0));

        self.client.update(duration);
        if let Err(e) = self.transport.update(duration, &mut self.client) {
            eprintln!("tsumiki-net: client transport update error: {e}");
        }

        if self.client.is_connected() {
            while let Some((channel, bytes)) = self.pending_before_connect.pop_front() {
                self.client.send_message(channel, bytes);
            }
        }

        let reliable = reliable_channel_id();
        let unreliable = unreliable_channel_id();
        for channel in [reliable, unreliable] {
            while let Some(bytes) = self.client.receive_message(channel) {
                match postcard::from_bytes::<ServerToClient>(&bytes) {
                    Ok(msg) => self.recv_queue.push_back(msg),
                    Err(e) => eprintln!("tsumiki-net: dropping malformed packet from server: {e}"),
                }
            }
        }
    }

    fn flush(&mut self) {
        if self.client.is_disconnected() {
            return;
        }
        if let Err(e) = self.transport.send_packets(&mut self.client) {
            eprintln!("tsumiki-net: client send_packets error: {e}");
        }
    }
}

impl Drop for NetClientTransport {
    fn drop(&mut self) {
        // Sends the netcode disconnect packet immediately so the server
        // notices well before its passive connection timeout, letting
        // `ServerTransport::tick` synthesize `Goodbye` promptly.
        self.transport.disconnect();
    }
}

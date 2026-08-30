//! Integration test 4: dropping a connected client transport makes the
//! server yield a synthesized `Goodbye` for that client within a timeout.

mod common;

use std::time::Duration;

use tsumiki_net::{NetClientTransport, NetServerTransport};
use tsumiki_protocol::{ClientToServer, ClientTransport, ServerTransport};

use common::{TICK_DT, pump_until};

#[test]
fn disconnect_synthesizes_goodbye() {
    let mut server = NetServerTransport::bind("127.0.0.1:0".parse().unwrap()).expect("bind server");
    let addr = server.local_addr();
    let mut client = Some(NetClientTransport::connect(addr).expect("connect client"));

    client.as_mut().unwrap().send(ClientToServer::Hello {
        name: "carol".into(),
    });

    let (client_id, _hello) = pump_until(Duration::from_secs(10), || {
        server.tick(TICK_DT);
        server.flush();
        if let Some(c) = client.as_mut() {
            c.tick(TICK_DT);
            c.flush();
        }
        server.try_recv()
    })
    .expect("server never received Hello");

    // Drop the client transport. Its `Drop` impl sends an explicit netcode
    // disconnect packet, so the server should notice well before any passive
    // timeout.
    #[allow(unused_assignments)]
    {
        client = None;
    }

    let (goodbye_id, goodbye) = pump_until(Duration::from_secs(10), || {
        server.tick(TICK_DT);
        server.flush();
        server.try_recv()
    })
    .expect("server never synthesized Goodbye after disconnect");

    assert_eq!(goodbye_id, client_id);
    assert!(
        matches!(goodbye, ClientToServer::Goodbye),
        "expected Goodbye, got {goodbye:?}"
    );
}

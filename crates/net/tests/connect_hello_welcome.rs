//! Integration test 1: a client connects, sends Hello, the server yields it,
//! and a Welcome reply makes it back to the client.

mod common;

use std::time::Duration;

use tsumiki_net::NetClientTransport;
use tsumiki_net::NetServerTransport;
use tsumiki_protocol::{
    ClientToServer, ClientTransport, GameMode, ServerToClient, ServerTransport,
};

use common::{TICK_DT, pump_until};

#[test]
fn connect_hello_welcome() {
    let mut server = NetServerTransport::bind("127.0.0.1:0".parse().unwrap()).expect("bind server");
    let addr = server.local_addr();
    let mut client = NetClientTransport::connect(addr).expect("connect client");

    // Hello is sent before the handshake completes; it must survive.
    client.send(ClientToServer::Hello {
        name: "alice".into(),
    });

    let (client_id, hello) = pump_until(Duration::from_secs(10), || {
        server.tick(TICK_DT);
        server.flush();
        client.tick(TICK_DT);
        client.flush();
        server.try_recv()
    })
    .expect("server never received a message from the client");

    match hello {
        ClientToServer::Hello { name } => assert_eq!(name, "alice"),
        other => panic!("expected Hello, got {other:?}"),
    }

    server.send(
        client_id,
        ServerToClient::Welcome {
            client_id,
            player: None,
            game_mode: GameMode::Creative,
            time_of_day: 0.25,
        },
    );

    let welcome = pump_until(Duration::from_secs(10), || {
        server.tick(TICK_DT);
        server.flush();
        client.tick(TICK_DT);
        client.flush();
        client.try_recv()
    })
    .expect("client never received Welcome");

    match welcome {
        ServerToClient::Welcome {
            client_id: id,
            player,
            ..
        } => {
            assert_eq!(id, client_id);
            assert!(player.is_none());
        }
        other => panic!("expected Welcome, got {other:?}"),
    }
}

//! Integration test 3: two clients connect to the same server, are seen with
//! distinct ids, and a message addressed to one never reaches the other.

mod common;

use std::collections::HashMap;
use std::time::Duration;

use tsumiki_net::{NetClientTransport, NetServerTransport};
use tsumiki_protocol::{
    ClientId, ClientToServer, ClientTransport, GameMode, ServerToClient, ServerTransport,
};

use common::{TICK_DT, pump_for, pump_until};

#[test]
fn two_clients_distinct_ids() {
    let mut server = NetServerTransport::bind("127.0.0.1:0".parse().unwrap()).expect("bind server");
    let addr = server.local_addr();
    let mut client_a = NetClientTransport::connect(addr).expect("connect client a");
    let mut client_b = NetClientTransport::connect(addr).expect("connect client b");

    client_a.send(ClientToServer::Hello { name: "a".into() });
    client_b.send(ClientToServer::Hello { name: "b".into() });

    let mut hellos: HashMap<ClientId, String> = HashMap::new();
    pump_until(Duration::from_secs(10), || {
        server.tick(TICK_DT);
        server.flush();
        client_a.tick(TICK_DT);
        client_a.flush();
        client_b.tick(TICK_DT);
        client_b.flush();

        while let Some((id, msg)) = server.try_recv() {
            if let ClientToServer::Hello { name } = msg {
                hellos.insert(id, name);
            }
        }
        (hellos.len() >= 2).then_some(())
    })
    .expect("server never saw Hello from both clients");

    let ids: Vec<ClientId> = hellos.keys().copied().collect();
    assert_eq!(
        ids.len(),
        2,
        "expected two distinct client ids, got {ids:?}"
    );
    assert_ne!(ids[0], ids[1]);

    let id_a = *hellos
        .iter()
        .find(|(_, name)| name.as_str() == "a")
        .unwrap()
        .0;

    server.send(
        id_a,
        ServerToClient::Welcome {
            client_id: id_a,
            player: None,
            game_mode: GameMode::Creative,
            time_of_day: 0.25,
        },
    );

    let welcome_a = pump_until(Duration::from_secs(10), || {
        server.tick(TICK_DT);
        server.flush();
        client_a.tick(TICK_DT);
        client_a.flush();
        client_b.tick(TICK_DT);
        client_b.flush();
        client_a.try_recv()
    })
    .expect("client a never received Welcome");

    match welcome_a {
        ServerToClient::Welcome { client_id, .. } => assert_eq!(client_id, id_a),
        other => panic!("expected Welcome, got {other:?}"),
    }

    // client_b must never receive the message addressed only to client_a.
    let mut leaked = None;
    pump_for(Duration::from_millis(500), || {
        server.tick(TICK_DT);
        server.flush();
        client_a.tick(TICK_DT);
        client_a.flush();
        client_b.tick(TICK_DT);
        client_b.flush();
        if let Some(msg) = client_b.try_recv() {
            leaked = Some(msg);
        }
    });
    assert!(
        leaked.is_none(),
        "client b received a message meant for client a: {leaked:?}"
    );
}

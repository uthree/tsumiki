//! Integration test 5: a `ServerToClient::ChunkData` carrying a real chunk
//! survives postcard-over-UDP, verified by sampling `get()`.

mod common;

use std::time::Duration;

use bevy_math::{IVec3, UVec3};
use tsumiki_net::{NetClientTransport, NetServerTransport};
use tsumiki_protocol::{ClientToServer, ClientTransport, ServerToClient, ServerTransport};
use tsumiki_world::{BlockId, Chunk};

use common::{TICK_DT, pump_until};

#[test]
fn chunk_data_roundtrip() {
    let mut server = NetServerTransport::bind("127.0.0.1:0".parse().unwrap()).expect("bind server");
    let addr = server.local_addr();
    let mut client = NetClientTransport::connect(addr).expect("connect client");

    client.send(ClientToServer::Hello {
        name: "dave".into(),
    });
    let (client_id, _hello) = pump_until(Duration::from_secs(10), || {
        server.tick(TICK_DT);
        server.flush();
        client.tick(TICK_DT);
        client.flush();
        server.try_recv()
    })
    .expect("server never received Hello");

    let mut chunk = Chunk::filled(BlockId(0));
    let samples = [
        (UVec3::new(0, 0, 0), BlockId(1)),
        (UVec3::new(31, 0, 0), BlockId(2)),
        (UVec3::new(5, 17, 9), BlockId(3)),
        (UVec3::new(31, 31, 31), BlockId(4)),
    ];
    for &(pos, block) in &samples {
        chunk.set(pos, block);
    }

    let chunk_pos = IVec3::new(2, 0, -3);
    server.send(
        client_id,
        ServerToClient::ChunkData {
            pos: chunk_pos,
            chunk: chunk.clone(),
        },
    );

    let received = pump_until(Duration::from_secs(10), || {
        server.tick(TICK_DT);
        server.flush();
        client.tick(TICK_DT);
        client.flush();
        client.try_recv()
    })
    .expect("client never received ChunkData");

    match received {
        ServerToClient::ChunkData {
            pos,
            chunk: received_chunk,
        } => {
            assert_eq!(pos, chunk_pos);
            for &(local, expected) in &samples {
                assert_eq!(received_chunk.get(local), expected, "mismatch at {local:?}");
            }
            // An untouched cell should still read back as the fill block.
            assert_eq!(received_chunk.get(UVec3::new(10, 10, 10)), BlockId(0));
        }
        other => panic!("expected ChunkData, got {other:?}"),
    }
}

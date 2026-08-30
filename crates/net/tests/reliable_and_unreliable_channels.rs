//! Integration test 2: a reliable SetBlock and a burst of unreliable
//! UpdatePlayer messages both arrive over loopback. The reliable message
//! must always arrive; at least one of the unreliable burst must arrive.

mod common;

use std::time::Duration;

use bevy_math::{IVec3, Vec3};
use tsumiki_net::{NetClientTransport, NetServerTransport};
use tsumiki_protocol::{ClientToServer, ClientTransport, PlayerSave, ServerTransport};
use tsumiki_world::BlockId;

use common::{TICK_DT, pump_until};

#[test]
fn reliable_and_unreliable_channels() {
    let mut server = NetServerTransport::bind("127.0.0.1:0".parse().unwrap()).expect("bind server");
    let addr = server.local_addr();
    let mut client = NetClientTransport::connect(addr).expect("connect client");

    // Establish a full connection first (via Hello) so the burst below goes
    // out on the real channels rather than through the pre-connect buffer.
    client.send(ClientToServer::Hello { name: "bob".into() });
    pump_until(Duration::from_secs(10), || {
        server.tick(TICK_DT);
        server.flush();
        client.tick(TICK_DT);
        client.flush();
        server.try_recv()
    })
    .expect("server never received Hello");

    let set_block_pos = IVec3::new(1, 2, 3);
    client.send(ClientToServer::SetBlock {
        pos: set_block_pos,
        block: BlockId(7),
    });

    for i in 0..50 {
        client.send(ClientToServer::UpdatePlayer(PlayerSave {
            pos: Vec3::new(i as f32, 0.0, 0.0),
            yaw: 0.0,
            pitch: 0.0,
        }));
    }

    let mut saw_set_block = false;
    let mut saw_update_player = false;

    pump_until(Duration::from_secs(10), || {
        server.tick(TICK_DT);
        server.flush();
        client.tick(TICK_DT);
        client.flush();

        while let Some((_id, msg)) = server.try_recv() {
            match msg {
                ClientToServer::SetBlock { pos, block } => {
                    assert_eq!(pos, set_block_pos);
                    assert_eq!(block, BlockId(7));
                    saw_set_block = true;
                }
                ClientToServer::UpdatePlayer(_) => saw_update_player = true,
                other => panic!("unexpected message: {other:?}"),
            }
        }
        (saw_set_block && saw_update_player).then_some(())
    })
    .expect(
        "did not see both a reliable SetBlock and at least one unreliable UpdatePlayer in time",
    );

    assert!(saw_set_block, "reliable SetBlock must always arrive");
    assert!(
        saw_update_player,
        "at least one unreliable UpdatePlayer must arrive"
    );
}

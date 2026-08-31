//! Postcard roundtrip coverage for every `ClientToServer` and `ServerToClient`
//! variant.
//!
//! The real (future) network transport serializes every message with
//! postcard; this guards network-readiness for every future protocol change
//! by failing as soon as a new field or variant is added without a matching
//! roundtrip case here.

use bevy_math::{IVec3, UVec3, Vec3};
use tsumiki_protocol::{
    ClientToServer, ContainerKind, DamageCause, GameMode, PlayerSave, ServerToClient, SlotArea,
    SlotRef,
};
use tsumiki_world::{BlockId, CHUNK_SIZE, Chunk, ItemStack, items};

fn roundtrip<T>(value: &T) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let bytes = postcard::to_allocvec(value).expect("postcard serialize");
    postcard::from_bytes(&bytes).expect("postcard deserialize")
}

/// Samples every block in linear order. `Chunk` doesn't implement
/// `PartialEq` (see crates/world/src/chunk.rs), so equality is checked by
/// comparing full block-by-block samples instead.
fn sample_chunk(chunk: &Chunk) -> Vec<BlockId> {
    let mut out = Vec::with_capacity(CHUNK_SIZE.pow(3));
    for y in 0..CHUNK_SIZE {
        for z in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                out.push(chunk.get(UVec3::new(x as u32, y as u32, z as u32)));
            }
        }
    }
    out
}

fn sample_player() -> PlayerSave {
    PlayerSave {
        pos: Vec3::new(1.5, 64.0, -2.5),
        yaw: 0.75,
        pitch: -0.2,
    }
}

#[test]
fn hello_roundtrip() {
    let original = ClientToServer::Hello {
        name: "tester".to_string(),
    };
    let decoded = roundtrip(&original);
    match (original, decoded) {
        (ClientToServer::Hello { name: a }, ClientToServer::Hello { name: b }) => {
            assert_eq!(a, b);
        }
        _ => panic!("variant mismatch after roundtrip"),
    }
}

#[test]
fn request_chunks_roundtrip() {
    let original = ClientToServer::RequestChunks {
        positions: vec![IVec3::new(1, 2, 3), IVec3::new(-4, 0, 5)],
    };
    let decoded = roundtrip(&original);
    match (original, decoded) {
        (
            ClientToServer::RequestChunks { positions: a },
            ClientToServer::RequestChunks { positions: b },
        ) => assert_eq!(a, b),
        _ => panic!("variant mismatch after roundtrip"),
    }
}

#[test]
fn break_block_roundtrip() {
    let original = ClientToServer::BreakBlock {
        pos: IVec3::new(5, 10, -5),
        hotbar: 4,
    };
    let decoded = roundtrip(&original);
    match (original, decoded) {
        (
            ClientToServer::BreakBlock {
                pos: a,
                hotbar: slot_a,
            },
            ClientToServer::BreakBlock {
                pos: b,
                hotbar: slot_b,
            },
        ) => {
            assert_eq!(a, b);
            assert_eq!(slot_a, slot_b);
        }
        _ => panic!("variant mismatch after roundtrip"),
    }
}

#[test]
fn place_block_roundtrip() {
    let original = ClientToServer::PlaceBlock {
        pos: IVec3::new(5, 10, -5),
        hotbar: 3,
    };
    let decoded = roundtrip(&original);
    match (original, decoded) {
        (
            ClientToServer::PlaceBlock {
                pos: pos_a,
                hotbar: hotbar_a,
            },
            ClientToServer::PlaceBlock {
                pos: pos_b,
                hotbar: hotbar_b,
            },
        ) => {
            assert_eq!(pos_a, pos_b);
            assert_eq!(hotbar_a, hotbar_b);
        }
        _ => panic!("variant mismatch after roundtrip"),
    }
}

#[test]
fn slot_click_roundtrip_covers_every_area() {
    for area in [SlotArea::Main, SlotArea::Container] {
        for (right, shift) in [(false, false), (true, false), (false, true), (true, true)] {
            let original = ClientToServer::SlotClick {
                slot: SlotRef { area, index: 17 },
                right,
                shift,
            };
            let decoded = roundtrip(&original);
            match decoded {
                ClientToServer::SlotClick {
                    slot,
                    right: right_b,
                    shift: shift_b,
                } => {
                    assert_eq!(slot, SlotRef { area, index: 17 });
                    assert_eq!(right_b, right);
                    assert_eq!(shift_b, shift);
                }
                _ => panic!("variant mismatch after roundtrip"),
            }
        }
    }
}

#[test]
fn container_requests_roundtrip() {
    let decoded = roundtrip(&ClientToServer::OpenContainer {
        pos: IVec3::new(3, 40, -8),
    });
    match decoded {
        ClientToServer::OpenContainer { pos } => assert_eq!(pos, IVec3::new(3, 40, -8)),
        _ => panic!("variant mismatch after roundtrip"),
    }
    assert!(matches!(
        roundtrip(&ClientToServer::CloseContainer),
        ClientToServer::CloseContainer
    ));
}

#[test]
fn drop_slot_roundtrip() {
    let original = ClientToServer::DropSlot {
        slot: SlotRef {
            area: SlotArea::Main,
            index: 0,
        },
        all: true,
    };
    let decoded = roundtrip(&original);
    match decoded {
        ClientToServer::DropSlot { slot, all } => {
            assert_eq!(slot.area, SlotArea::Main);
            assert_eq!(slot.index, 0);
            assert!(all);
        }
        _ => panic!("variant mismatch after roundtrip"),
    }
}

#[test]
fn report_damage_roundtrip() {
    for cause in [DamageCause::Fall, DamageCause::Drown] {
        let original = ClientToServer::ReportDamage { amount: 6, cause };
        let decoded = roundtrip(&original);
        match decoded {
            ClientToServer::ReportDamage {
                amount,
                cause: cause_b,
            } => {
                assert_eq!(amount, 6);
                assert_eq!(cause_b, cause);
            }
            _ => panic!("variant mismatch after roundtrip"),
        }
    }
}

#[test]
fn respawn_roundtrip() {
    let decoded = roundtrip(&ClientToServer::Respawn);
    assert!(matches!(decoded, ClientToServer::Respawn));
}

#[test]
fn inventory_update_roundtrip() {
    let mut main = vec![None; tsumiki_world::MAIN_INVENTORY_SIZE];
    main[0] = Some(ItemStack::new(items::STONE, 64));
    main[35] = Some(ItemStack::one(items::CHEST));

    let original = ServerToClient::InventoryUpdate {
        main: main.clone(),
        cursor: Some(ItemStack::new(items::LOG, 7)),
    };
    let decoded = roundtrip(&original);
    match decoded {
        ServerToClient::InventoryUpdate {
            main: main_b,
            cursor,
        } => {
            assert_eq!(main_b, main);
            assert_eq!(cursor, Some(ItemStack::new(items::LOG, 7)));
        }
        _ => panic!("variant mismatch after roundtrip"),
    }
}

#[test]
fn container_messages_roundtrip() {
    for kind in [
        ContainerKind::Chest,
        ContainerKind::CraftingTable,
        ContainerKind::Furnace,
    ] {
        let slots = match kind {
            ContainerKind::Chest => {
                let mut slots = vec![None; tsumiki_world::inventory::CHEST_SIZE];
                slots[2] = Some(ItemStack::new(items::PLANKS, 12));
                slots
            }
            ContainerKind::CraftingTable => Vec::new(),
            ContainerKind::Furnace => vec![None; tsumiki_world::smelting::FURNACE_SIZE],
        };
        let original = ServerToClient::ContainerOpened {
            kind,
            pos: IVec3::new(-2, 38, 6),
            slots: slots.clone(),
        };
        let decoded = roundtrip(&original);
        match decoded {
            ServerToClient::ContainerOpened {
                kind: kind_b,
                pos,
                slots: slots_b,
            } => {
                assert_eq!(kind_b, kind);
                assert_eq!(pos, IVec3::new(-2, 38, 6));
                assert_eq!(slots_b, slots);
            }
            _ => panic!("variant mismatch after roundtrip"),
        }
    }

    let slots = vec![Some(ItemStack::one(items::DIRT)), None];
    let decoded = roundtrip(&ServerToClient::ContainerUpdate {
        slots: slots.clone(),
    });
    match decoded {
        ServerToClient::ContainerUpdate { slots: b } => assert_eq!(b, slots),
        _ => panic!("variant mismatch after roundtrip"),
    }

    assert!(matches!(
        roundtrip(&ServerToClient::ContainerClosed),
        ServerToClient::ContainerClosed
    ));
}

#[test]
fn health_update_and_died_roundtrip() {
    let decoded = roundtrip(&ServerToClient::HealthUpdate { hp: 13 });
    match decoded {
        ServerToClient::HealthUpdate { hp } => assert_eq!(hp, 13),
        _ => panic!("variant mismatch after roundtrip"),
    }
    let decoded = roundtrip(&ServerToClient::Died {
        at: Vec3::new(1.0, 50.0, -3.0),
    });
    match decoded {
        ServerToClient::Died { at } => assert_eq!(at, Vec3::new(1.0, 50.0, -3.0)),
        _ => panic!("variant mismatch after roundtrip"),
    }
}

#[test]
fn item_spawned_despawned_roundtrip() {
    let original = ServerToClient::ItemSpawned {
        id: 99,
        pos: Vec3::new(4.5, 41.0, 7.5),
        stack: ItemStack::new(items::DIRT, 5),
    };
    let decoded = roundtrip(&original);
    match decoded {
        ServerToClient::ItemSpawned { id, pos, stack } => {
            assert_eq!(id, 99);
            assert_eq!(pos, Vec3::new(4.5, 41.0, 7.5));
            assert_eq!(stack, ItemStack::new(items::DIRT, 5));
        }
        _ => panic!("variant mismatch after roundtrip"),
    }
    let decoded = roundtrip(&ServerToClient::ItemDespawned { id: 99 });
    match decoded {
        ServerToClient::ItemDespawned { id } => assert_eq!(id, 99),
        _ => panic!("variant mismatch after roundtrip"),
    }
}

#[test]
fn time_update_roundtrip() {
    let decoded = roundtrip(&ServerToClient::TimeUpdate { time_of_day: 0.75 });
    match decoded {
        ServerToClient::TimeUpdate { time_of_day } => assert_eq!(time_of_day, 0.75),
        _ => panic!("variant mismatch after roundtrip"),
    }
}

#[test]
fn update_player_roundtrip() {
    let original = ClientToServer::UpdatePlayer(sample_player());
    let decoded = roundtrip(&original);
    match (original, decoded) {
        (ClientToServer::UpdatePlayer(a), ClientToServer::UpdatePlayer(b)) => assert_eq!(a, b),
        _ => panic!("variant mismatch after roundtrip"),
    }
}

#[test]
fn goodbye_roundtrip() {
    let original = ClientToServer::Goodbye;
    let decoded = roundtrip(&original);
    assert!(matches!(decoded, ClientToServer::Goodbye));
}

#[test]
fn welcome_roundtrip() {
    for (player, game_mode) in [
        (Some(sample_player()), GameMode::Survival),
        (None, GameMode::Creative),
    ] {
        let original = ServerToClient::Welcome {
            client_id: 42,
            player,
            game_mode,
            time_of_day: 0.25,
        };
        let decoded = roundtrip(&original);
        match decoded {
            ServerToClient::Welcome {
                client_id,
                player: player_b,
                game_mode: mode_b,
                time_of_day,
            } => {
                assert_eq!(client_id, 42);
                assert_eq!(player_b, player);
                assert_eq!(mode_b, game_mode);
                assert_eq!(time_of_day, 0.25);
            }
            _ => panic!("variant mismatch after roundtrip"),
        }
    }
}

#[test]
fn chunk_data_roundtrip() {
    let mut chunk = Chunk::filled(BlockId::AIR);
    chunk.set(UVec3::new(1, 2, 3), BlockId(5));
    chunk.set(UVec3::new(31, 0, 31), BlockId(2));
    let original = ServerToClient::ChunkData {
        pos: IVec3::new(2, 0, -1),
        chunk,
    };
    let decoded = roundtrip(&original);
    match (original, decoded) {
        (
            ServerToClient::ChunkData {
                pos: pos_a,
                chunk: chunk_a,
            },
            ServerToClient::ChunkData {
                pos: pos_b,
                chunk: chunk_b,
            },
        ) => {
            assert_eq!(pos_a, pos_b);
            assert_eq!(sample_chunk(&chunk_a), sample_chunk(&chunk_b));
        }
        _ => panic!("variant mismatch after roundtrip"),
    }
}

#[test]
fn request_lod_chunks_roundtrip() {
    let original = ClientToServer::RequestLodChunks {
        level: 2,
        positions: vec![IVec3::new(1, 0, -3)],
    };
    let decoded = roundtrip(&original);
    match (original, decoded) {
        (
            ClientToServer::RequestLodChunks {
                level: level_a,
                positions: positions_a,
            },
            ClientToServer::RequestLodChunks {
                level: level_b,
                positions: positions_b,
            },
        ) => {
            assert_eq!(level_a, level_b);
            assert_eq!(positions_a, positions_b);
        }
        _ => panic!("variant mismatch after roundtrip"),
    }
}

#[test]
fn lod_chunk_data_roundtrip() {
    let mut chunk = Chunk::filled(BlockId::AIR);
    chunk.set(UVec3::new(0, 1, 2), BlockId(1));
    let original = ServerToClient::LodChunkData {
        level: 3,
        pos: IVec3::new(-1, 0, 2),
        chunk,
    };
    let decoded = roundtrip(&original);
    match (original, decoded) {
        (
            ServerToClient::LodChunkData {
                level: level_a,
                pos: pos_a,
                chunk: chunk_a,
            },
            ServerToClient::LodChunkData {
                level: level_b,
                pos: pos_b,
                chunk: chunk_b,
            },
        ) => {
            assert_eq!(level_a, level_b);
            assert_eq!(pos_a, pos_b);
            assert_eq!(sample_chunk(&chunk_a), sample_chunk(&chunk_b));
        }
        _ => panic!("variant mismatch after roundtrip"),
    }
}

#[test]
fn player_joined_roundtrip() {
    let original = ServerToClient::PlayerJoined {
        id: 9,
        name: "friend".to_string(),
        state: sample_player(),
    };
    let decoded = roundtrip(&original);
    match (original, decoded) {
        (
            ServerToClient::PlayerJoined {
                id: id_a,
                name: name_a,
                state: state_a,
            },
            ServerToClient::PlayerJoined {
                id: id_b,
                name: name_b,
                state: state_b,
            },
        ) => {
            assert_eq!(id_a, id_b);
            assert_eq!(name_a, name_b);
            assert_eq!(state_a, state_b);
        }
        _ => panic!("variant mismatch after roundtrip"),
    }
}

#[test]
fn player_left_roundtrip() {
    let original = ServerToClient::PlayerLeft { id: 3 };
    let decoded = roundtrip(&original);
    match decoded {
        ServerToClient::PlayerLeft { id } => assert_eq!(id, 3),
        _ => panic!("variant mismatch after roundtrip"),
    }
}

#[test]
fn player_moved_roundtrip() {
    let original = ServerToClient::PlayerMoved {
        id: 11,
        state: sample_player(),
    };
    let decoded = roundtrip(&original);
    match (original, decoded) {
        (
            ServerToClient::PlayerMoved {
                id: id_a,
                state: state_a,
            },
            ServerToClient::PlayerMoved {
                id: id_b,
                state: state_b,
            },
        ) => {
            assert_eq!(id_a, id_b);
            assert_eq!(state_a, state_b);
        }
        _ => panic!("variant mismatch after roundtrip"),
    }
}

#[test]
fn block_changed_roundtrip() {
    let original = ServerToClient::BlockChanged {
        pos: IVec3::new(0, 5, 0),
        block: BlockId(1),
    };
    let decoded = roundtrip(&original);
    match (original, decoded) {
        (
            ServerToClient::BlockChanged {
                pos: pos_a,
                block: block_a,
            },
            ServerToClient::BlockChanged {
                pos: pos_b,
                block: block_b,
            },
        ) => {
            assert_eq!(pos_a, pos_b);
            assert_eq!(block_a, block_b);
        }
        _ => panic!("variant mismatch after roundtrip"),
    }
}

#[test]
fn craft_roundtrip() {
    for all in [false, true] {
        let decoded = roundtrip(&ClientToServer::Craft { recipe: 3, all });
        match decoded {
            ClientToServer::Craft { recipe, all: all_b } => {
                assert_eq!(recipe, 3);
                assert_eq!(all_b, all);
            }
            _ => panic!("variant mismatch after roundtrip"),
        }
    }
}

#[test]
fn furnace_progress_roundtrip() {
    let decoded = roundtrip(&ServerToClient::FurnaceProgress {
        cook: 0.25,
        fuel: 0.5,
    });
    match decoded {
        ServerToClient::FurnaceProgress { cook, fuel } => {
            assert_eq!(cook, 0.25);
            assert_eq!(fuel, 0.5);
        }
        _ => panic!("variant mismatch after roundtrip"),
    }
}

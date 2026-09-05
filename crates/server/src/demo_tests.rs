use super::*;

#[test]
fn demo_lamps_are_creative_only_even_if_a_survival_inventory_is_seeded() {
    for mode in [GameMode::Survival, GameMode::Creative] {
        let mut app = new_test_app_with(
            MockTransport::default(),
            42,
            Persistence::new(None, 9999.0),
            mode,
        );
        const CLIENT: ClientId = 1;
        join(&mut app, CLIENT, "lighting-demo");
        let allowed = mode == GameMode::Creative && tsumiki_world::item::DEMO_LIGHTS_ENABLED;
        let catalog = app.world().resource::<ServerState>().clients[&CLIENT]
            .main
            .slots();
        for item in [
            items::DEMO_RED_LIGHT,
            items::DEMO_GREEN_LIGHT,
            items::DEMO_BLUE_LIGHT,
        ] {
            assert_eq!(
                catalog.iter().flatten().any(|stack| stack.item == item),
                allowed
            );
        }
        for (index, (item, block)) in [
            (items::DEMO_RED_LIGHT, blocks::DEMO_RED_LIGHT),
            (items::DEMO_GREEN_LIGHT, blocks::DEMO_GREEN_LIGHT),
            (items::DEMO_BLUE_LIGHT, blocks::DEMO_BLUE_LIGHT),
        ]
        .into_iter()
        .enumerate()
        {
            let pos = IVec3::new(12 + index as i32, 10, 12);
            seed_block(&mut app, pos, blocks::AIR);
            seed_main_slot(&mut app, CLIENT, 0, ItemStack::one(item));
            {
                let mut transport = app
                    .world_mut()
                    .resource_mut::<TransportRes<MockTransport>>();
                transport
                    .0
                    .push(CLIENT, ClientToServer::UpdatePlayer(save_near(pos)));
                transport
                    .0
                    .push(CLIENT, ClientToServer::PlaceBlock { pos, hotbar: 0 });
            }
            app.update();
            let (chunk, local) = split_block_pos(pos);
            let placed = app.world().resource::<ChunkCache>().chunks[&chunk].get(local.as_uvec3());
            assert_eq!(placed, if allowed { block } else { blocks::AIR });
        }
    }
}

/// Reproducible three-source RGB mixing scene, kept outside normal saves.
#[test]
#[ignore = "writes a visual verification fixture under target/controls-qa"]
fn write_rgb_verification_world() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/controls-qa/rgb");
    let mut persistence = Persistence::new(Some(dir.clone()), 9999.0);
    let mut chunks = HashMap::new();
    for cx in 0..=1 {
        let chunk_pos = IVec3::new(cx, 0, 0);
        let mut chunk = Chunk::filled(blocks::STONE);
        for y in 8..=15 {
            for z in 9..=26 {
                for x in 22..=42 {
                    let (cp, local) = split_block_pos(IVec3::new(x, y, z));
                    if cp == chunk_pos {
                        chunk.set(local.as_uvec3(), blocks::AIR);
                    }
                }
            }
        }
        for (x, block) in [
            (27, blocks::DEMO_RED_LIGHT),
            (32, blocks::DEMO_GREEN_LIGHT),
            (37, blocks::DEMO_BLUE_LIGHT),
        ] {
            let (cp, local) = split_block_pos(IVec3::new(x, 8, 15));
            if cp == chunk_pos {
                chunk.set(local.as_uvec3(), block);
            }
        }
        chunks.insert(chunk_pos, chunk);
        persistence.mark_chunk_dirty(chunk_pos);
    }
    let players = HashMap::from([(
        "player".to_string(),
        PlayerRecord {
            save: PlayerSave {
                pos: Vec3::new(32.5, 9.0, 24.5),
                yaw: 0.0,
                pitch: -0.18,
            },
            hp: MAX_HP,
            hunger: MAX_HUNGER,
            exhaustion: 0.0,
            main: vec![None; MAIN_INVENTORY_SIZE],
        },
    )]);
    persistence
        .save(
            42,
            GameMode::Creative,
            0.25,
            &players,
            &[],
            &[],
            &[],
            &chunks,
        )
        .unwrap();
    eprintln!("RGB verification world: {}", dir.display());
}

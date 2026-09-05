//! End-to-end M8 behavior through the same transport pump as real clients.
use super::*;

const PLAYER: ClientId = 81;
const FARM: IVec3 = IVec3::new(20, 100, 20);

fn command(app: &mut App, message: ClientToServer) -> Vec<ServerToClient> {
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(PLAYER, message);
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(PLAYER)
}

fn survival() -> App {
    let mut app = new_test_app_with(
        MockTransport::default(),
        42,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 0.1;
    command(
        &mut app,
        ClientToServer::Hello {
            name: "farmer".into(),
        },
    );
    command(
        &mut app,
        ClientToServer::UpdatePlayer(save_near(FARM + IVec3::new(3, 2, 0))),
    );
    app
}

fn state(app: &App) -> &ClientState {
    &app.world().resource::<ServerState>().clients[&PLAYER]
}

fn block(app: &mut App, pos: IVec3) -> BlockId {
    let generator = WorldGenerator::new(42);
    farming::block_at(
        &mut app.world_mut().resource_mut::<ChunkCache>(),
        &generator,
        0,
        pos,
    )
}

fn planted(app: &mut App) {
    seed_block(app, FARM, blocks::DIRT);
    seed_block(app, FARM + IVec3::Y, blocks::AIR);
    seed_block(app, FARM + IVec3::new(4, 0, 0), blocks::WATER);
    seed_main_slot(app, PLAYER, 0, ItemStack::one(items::WOODEN_SHOVEL));
    seed_main_slot(app, PLAYER, 1, ItemStack::new(items::WHEAT_SEEDS, 3));
    command(
        app,
        ClientToServer::TillSoil {
            pos: FARM,
            hotbar: 0,
        },
    );
    assert_eq!(block(app, FARM), blocks::FARMLAND);
    assert_eq!(state(app).main.slot(0).unwrap().damage, 1);
    command(
        app,
        ClientToServer::PlaceBlock {
            pos: FARM + IVec3::Y,
            hotbar: 1,
        },
    );
    assert_eq!(block(app, FARM + IVec3::Y), blocks::WHEAT_YOUNG);
    assert_eq!(state(app).main.count_of(items::WHEAT_SEEDS), 2);
}

#[test]
fn food_consumes_only_the_named_valid_slot_and_never_at_fullness_or_in_creative() {
    let mut app = survival();
    seed_main_slot(&mut app, PLAYER, 3, ItemStack::new(items::BREAD, 4));
    command(&mut app, ClientToServer::Eat { hotbar: 3 });
    assert_eq!(state(&app).main.count_of(items::BREAD), 4);
    app.world_mut()
        .resource_mut::<ServerState>()
        .clients
        .get_mut(&PLAYER)
        .unwrap()
        .hunger = 13;
    for slot in [0, 9, 255] {
        command(&mut app, ClientToServer::Eat { hotbar: slot });
    }
    assert_eq!(state(&app).hunger, 13);
    let messages = command(&mut app, ClientToServer::Eat { hotbar: 3 });
    assert_eq!(state(&app).hunger, 18);
    assert_eq!(state(&app).main.count_of(items::BREAD), 3);
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, ServerToClient::HungerUpdate { hunger: 18 }))
    );
    command(&mut app, ClientToServer::Eat { hotbar: 3 });
    assert_eq!(state(&app).hunger, MAX_HUNGER);
    assert_eq!(state(&app).main.count_of(items::BREAD), 2);
    app.world_mut().resource_mut::<SimRes>().game_mode = GameMode::Creative;
    app.world_mut()
        .resource_mut::<ServerState>()
        .clients
        .get_mut(&PLAYER)
        .unwrap()
        .hunger = 0;
    command(&mut app, ClientToServer::Eat { hotbar: 3 });
    assert_eq!(state(&app).main.count_of(items::BREAD), 2);
    assert_eq!(state(&app).hp, MAX_HP);
}

#[test]
fn regeneration_requires_food_and_spends_energy_while_activity_and_idle_drain_it() {
    let mut app = survival();
    {
        let mut clients = app.world_mut().resource_mut::<ServerState>();
        let c = clients.clients.get_mut(&PLAYER).unwrap();
        c.hp = 10;
        c.hunger = 17;
    }
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 6.0;
    app.update();
    assert_eq!(state(&app).hp, 10);
    app.world_mut()
        .resource_mut::<ServerState>()
        .clients
        .get_mut(&PLAYER)
        .unwrap()
        .hunger = MAX_HUNGER;
    app.update();
    assert_eq!(state(&app).hp, 13);
    assert!(state(&app).hunger < MAX_HUNGER);
    let old_energy = state(&app).exhaustion;
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 0.1;
    command(
        &mut app,
        ClientToServer::UpdatePlayer(save_near(FARM + IVec3::new(4, 2, 0))),
    );
    assert!(state(&app).exhaustion > old_energy + 0.01);
    let before = state(&app).hunger;
    app.world_mut()
        .resource_mut::<ServerState>()
        .clients
        .get_mut(&PLAYER)
        .unwrap()
        .hp = MAX_HP;
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 120.0;
    app.update();
    assert_eq!(state(&app).hunger, before - 1);
}

#[test]
fn starvation_uses_normal_death_drops_and_respawn_resets_food() {
    let mut app = survival();
    seed_main_slot(&mut app, PLAYER, 0, ItemStack::new(items::BREAD, 2));
    {
        let mut clients = app.world_mut().resource_mut::<ServerState>();
        let c = clients.clients.get_mut(&PLAYER).unwrap();
        c.hp = 1;
        c.hunger = 0;
        c.cursor = Some(ItemStack::one(items::WHEAT));
        c.open_container = Some((FARM, ContainerKind::CraftingTable));
    }
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 4.0;
    app.update();
    let messages = app
        .world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(PLAYER);
    assert_eq!(state(&app).hp, 0);
    assert!(state(&app).main.to_vec().iter().all(Option::is_none));
    assert!(state(&app).cursor.is_none());
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, ServerToClient::Died { .. }))
    );
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, ServerToClient::ContainerClosed))
    );
    for (item, count) in [(items::BREAD, 2), (items::WHEAT, 1)] {
        assert_eq!(
            app.world()
                .resource::<SimRes>()
                .items
                .items
                .values()
                .filter(|i| i.stack.item == item)
                .map(|i| i.stack.count)
                .sum::<u32>(),
            count
        );
    }
    seed_main_slot(&mut app, PLAYER, 0, ItemStack::one(items::TOAST));
    command(&mut app, ClientToServer::Eat { hotbar: 0 });
    assert_eq!(state(&app).hunger, 0);
    assert_eq!(state(&app).main.count_of(items::TOAST), 1);
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 0.1;
    command(&mut app, ClientToServer::Respawn);
    assert_eq!(state(&app).hp, MAX_HP);
    assert_eq!(state(&app).hunger, MAX_HUNGER);
}

#[test]
fn till_plant_grow_harvest_replant_and_cook_form_a_renewable_food_loop() {
    let mut app = survival();
    planted(&mut app);
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 119.0;
    app.update();
    assert_eq!(block(&mut app, FARM + IVec3::Y), blocks::WHEAT_YOUNG);
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 1.0;
    app.update();
    assert_eq!(block(&mut app, FARM + IVec3::Y), blocks::WHEAT_MATURE);
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 0.1;
    let messages = command(
        &mut app,
        ClientToServer::BreakBlock {
            pos: FARM + IVec3::Y,
            hotbar: 1,
        },
    );
    for (item, count) in [(items::WHEAT, 1), (items::WHEAT_SEEDS, 2)] {
        assert!(messages.iter().any(|m| matches!(m, ServerToClient::ItemSpawned { stack, .. } if stack.item == item && stack.count == count)));
    }
    command(
        &mut app,
        ClientToServer::PlaceBlock {
            pos: FARM + IVec3::Y,
            hotbar: 1,
        },
    );
    assert_eq!(block(&mut app, FARM + IVec3::Y), blocks::WHEAT_YOUNG);
    assert!(app.world().resource::<Persistence>().crops.elapsed[&(FARM + IVec3::Y)] < 1.0);
    // The same harvested product is accepted by crafting and by a working furnace.
    seed_main_slot(&mut app, PLAYER, 2, ItemStack::new(items::WHEAT, 3));
    let bread_recipe = app
        .world()
        .resource::<CraftingRes>()
        .recipes
        .available(None)
        .find(|(_, recipe)| recipe.output.item == items::BREAD)
        .unwrap()
        .0;
    command(
        &mut app,
        ClientToServer::Craft {
            recipe: bread_recipe,
            all: false,
        },
    );
    assert_eq!(state(&app).main.count_of(items::BREAD), 1);
    assert_eq!(state(&app).main.count_of(items::WHEAT), 0);
    let furnace_pos = FARM + IVec3::new(0, 0, 2);
    seed_block(&mut app, furnace_pos, blocks::FURNACE);
    command(&mut app, ClientToServer::OpenContainer { pos: furnace_pos });
    {
        let mut crafting = app.world_mut().resource_mut::<CraftingRes>();
        let f = crafting.furnaces.states.get_mut(&furnace_pos).unwrap();
        f.inv
            .set_slot(FURNACE_INPUT, Some(ItemStack::one(items::BREAD)));
        f.inv
            .set_slot(FURNACE_FUEL, Some(ItemStack::one(items::COAL)));
    }
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 10.0;
    app.update();
    assert_eq!(
        app.world().resource::<CraftingRes>().furnaces.states[&furnace_pos]
            .inv
            .slot(FURNACE_OUTPUT),
        Some(ItemStack::one(items::TOAST))
    );
}

#[test]
fn dry_or_dark_crops_pause_and_torch_light_resumes_growth() {
    let mut app = survival();
    planted(&mut app);
    seed_block(&mut app, FARM + IVec3::new(4, 0, 0), blocks::DIRT);
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 120.0;
    app.update();
    assert_eq!(block(&mut app, FARM + IVec3::Y), blocks::WHEAT_YOUNG);
    seed_block(&mut app, FARM + IVec3::new(4, 0, 0), blocks::WATER);
    for x in -2_i32..=2 {
        for z in -2_i32..=2 {
            if x != 0 || z != 0 {
                seed_block(&mut app, FARM + IVec3::new(x, 0, z), blocks::STONE);
            }
            for y in 1..=4 {
                if x.abs() == 2 || z.abs() == 2 || y == 4 {
                    seed_block(&mut app, FARM + IVec3::new(x, y, z), blocks::STONE);
                }
            }
        }
    }
    app.update();
    assert_eq!(block(&mut app, FARM + IVec3::Y), blocks::WHEAT_YOUNG);
    seed_block(&mut app, FARM + IVec3::new(1, 1, 0), blocks::TORCH);
    app.update();
    assert_eq!(block(&mut app, FARM + IVec3::Y), blocks::WHEAT_MATURE);
}

#[test]
fn tilling_and_seeds_validate_tool_reach_exposure_life_and_support() {
    let mut app = survival();
    seed_block(&mut app, FARM, blocks::GRASS);
    seed_block(&mut app, FARM + IVec3::Y, blocks::AIR);
    seed_main_slot(&mut app, PLAYER, 0, ItemStack::one(items::WOODEN_PICKAXE));
    seed_main_slot(&mut app, PLAYER, 1, ItemStack::new(items::WHEAT_SEEDS, 2));
    command(
        &mut app,
        ClientToServer::TillSoil {
            pos: FARM,
            hotbar: 0,
        },
    );
    command(
        &mut app,
        ClientToServer::PlaceBlock {
            pos: FARM + IVec3::Y,
            hotbar: 1,
        },
    );
    assert_eq!(block(&mut app, FARM), blocks::GRASS);
    assert_eq!(block(&mut app, FARM + IVec3::Y), blocks::AIR);
    assert_eq!(state(&app).main.count_of(items::WHEAT_SEEDS), 2);
    seed_main_slot(&mut app, PLAYER, 0, ItemStack::one(items::WOODEN_SHOVEL));
    seed_block(&mut app, FARM + IVec3::Y, blocks::STONE);
    command(
        &mut app,
        ClientToServer::TillSoil {
            pos: FARM,
            hotbar: 0,
        },
    );
    seed_block(&mut app, FARM + IVec3::Y, blocks::AIR);
    command(
        &mut app,
        ClientToServer::TillSoil {
            pos: FARM,
            hotbar: 255,
        },
    );
    command(
        &mut app,
        ClientToServer::TillSoil {
            pos: FARM + IVec3::X * 100,
            hotbar: 0,
        },
    );
    app.world_mut()
        .resource_mut::<ServerState>()
        .clients
        .get_mut(&PLAYER)
        .unwrap()
        .hp = 0;
    command(
        &mut app,
        ClientToServer::TillSoil {
            pos: FARM,
            hotbar: 0,
        },
    );
    assert_eq!(block(&mut app, FARM), blocks::GRASS);
    assert_eq!(state(&app).main.slot(0).unwrap().damage, 0);
}

#[test]
fn grass_starts_a_farm_and_breaking_crop_support_drops_both_blocks() {
    let mut app = survival();
    seed_block(&mut app, FARM, blocks::GRASS);
    let messages = command(
        &mut app,
        ClientToServer::BreakBlock {
            pos: FARM,
            hotbar: 0,
        },
    );
    for item in [items::DIRT, items::WHEAT_SEEDS] {
        assert!(
            messages.iter().any(
                |m| matches!(m, ServerToClient::ItemSpawned { stack, .. } if stack.item == item)
            )
        );
    }
    planted(&mut app);
    let messages = command(
        &mut app,
        ClientToServer::BreakBlock {
            pos: FARM,
            hotbar: 0,
        },
    );
    assert_eq!(block(&mut app, FARM + IVec3::Y), blocks::AIR);
    assert!(
        !app.world()
            .resource::<Persistence>()
            .crops
            .elapsed
            .contains_key(&(FARM + IVec3::Y))
    );
    assert!(messages.iter().any(|m| matches!(m, ServerToClient::BlockChanged { pos, block } if *pos == FARM + IVec3::Y && block.is_air())));
}

#[test]
fn hunger_and_partial_crop_progress_survive_restart_without_offline_growth() {
    let mut app = survival();
    planted(&mut app);
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 60.0;
    app.update();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .join(format!("tsumiki-m8-persist-{}", std::process::id()));
    let mut persistence = Persistence::new(Some(dir.clone()), 10.0);
    persistence.crops =
        farming::Crops::from_records(app.world().resource::<Persistence>().crops.records());
    persistence.mark_chunk_dirty(split_block_pos(FARM).0);
    persistence
        .save(
            42,
            GameMode::Survival,
            0.2,
            &app.world().resource::<PlayersRes>().0,
            &[],
            &[],
            &[],
            &app.world().resource::<ChunkCache>().chunks,
        )
        .unwrap();
    let mut restored = Persistence::new(Some(dir.clone()), 10.0);
    let loaded = restored.load().unwrap().unwrap();
    let record = &loaded.players["farmer"];
    assert_eq!(record.hunger, state(&app).hunger);
    assert_eq!(record.exhaustion, state(&app).exhaustion);
    let age = restored.crops.elapsed[&(FARM + IVec3::Y)];
    assert!((60.0..61.0).contains(&age));
    let mut cache = ChunkCache::default();
    cache.chunks.extend(loaded.chunks);
    let edits = farming::tick(
        &mut restored.crops,
        &mut cache,
        &WorldGenerator::new(42),
        &BlockRegistry::prototype(),
        0,
        59.0,
    );
    assert!(edits.is_empty());
    let edits = farming::tick(
        &mut restored.crops,
        &mut cache,
        &WorldGenerator::new(42),
        &BlockRegistry::prototype(),
        0,
        1.0,
    );
    assert_eq!(edits, vec![FARM + IVec3::Y]);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "writes the M8 visual fixture under target/m89-qa/farm"]
fn write_survival_verification_world() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/m89-qa/farm");
    let mut persistence = Persistence::new(Some(dir.clone()), 9999.0);
    let chunk_pos = IVec3::new(0, 3, 0);
    let mut chunk = Chunk::filled(blocks::AIR);
    for z in 0..32 {
        for x in 0..32 {
            for y in 0..5 {
                chunk.set(
                    UVec3::new(x, y, z),
                    if y == 4 { blocks::GRASS } else { blocks::DIRT },
                );
            }
        }
    }
    for z in 14..24 {
        chunk.set(UVec3::new(20, 4, z), blocks::WATER);
        for x in [17, 18, 19, 21, 22, 23] {
            chunk.set(UVec3::new(x, 4, z), blocks::FARMLAND);
            let crop = if x < 20 {
                blocks::WHEAT_YOUNG
            } else {
                blocks::WHEAT_MATURE
            };
            chunk.set(UVec3::new(x, 5, z), crop);
            if crop == blocks::WHEAT_YOUNG {
                persistence
                    .crops
                    .elapsed
                    .insert(IVec3::new(x as i32, 101, z as i32), 40.0);
            }
        }
    }
    chunk.set(UVec3::new(15, 5, 21), blocks::FURNACE);
    chunk.set(UVec3::new(15, 5, 20), blocks::CRAFTING_TABLE);
    let mut main = vec![None; MAIN_INVENTORY_SIZE];
    for (index, item) in [
        items::WOODEN_SHOVEL,
        items::WHEAT_SEEDS,
        items::WHEAT,
        items::BREAD,
        items::TOAST,
        items::TORCH,
    ]
    .into_iter()
    .enumerate()
    {
        main[index] = Some(ItemStack::new(item, if index == 0 { 1 } else { 8 }));
    }
    let players = HashMap::from([(
        "player".to_string(),
        PlayerRecord {
            save: PlayerSave {
                pos: Vec3::new(20.5, 101.0, 28.5),
                yaw: 0.0,
                pitch: -0.35,
            },
            hp: 16,
            hunger: 12,
            exhaustion: 0.0,
            main,
        },
    )]);
    persistence.mark_chunk_dirty(chunk_pos);
    persistence
        .save(
            42,
            GameMode::Survival,
            0.25,
            &players,
            &[],
            &[],
            &[],
            &HashMap::from([(chunk_pos, chunk)]),
        )
        .unwrap();
    eprintln!("Survival verification world: {}", dir.display());
}

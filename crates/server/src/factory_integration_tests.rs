//! M9 integration tests exercise world edits, graph ownership and transfers.
use super::*;
use tsumiki_protocol::FactoryAction;
use tsumiki_world::factory::FactoryNodeKind;

const BUILDER: ClientId = 201;
const OBSERVER: ClientId = 202;
const MINER: IVec3 = IVec3::new(10, 101, 10);
const STORAGE: IVec3 = IVec3::new(13, 101, 10);

fn command(app: &mut App, client: ClientId, message: ClientToServer) -> Vec<ServerToClient> {
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .push(client, message);
    app.update();
    app.world_mut()
        .resource_mut::<TransportRes<MockTransport>>()
        .0
        .take(client)
}

fn action(
    app: &mut App,
    client: ClientId,
    pos: IVec3,
    action: FactoryAction,
) -> Vec<ServerToClient> {
    command(app, client, ClientToServer::FactoryAction { pos, action })
}

fn player(app: &App, id: ClientId) -> &ClientState {
    &app.world().resource::<ServerState>().clients[&id]
}

fn amount(app: &App, pos: IVec3) -> f64 {
    let factories = &app.world().resource::<Persistence>().factories;
    factories
        .graph
        .node(factories.machines[&pos].id)
        .unwrap()
        .output
        .unwrap()
        .amount
}

fn block(app: &mut App, pos: IVec3) -> BlockId {
    farming::block_at(
        &mut app.world_mut().resource_mut::<ChunkCache>(),
        &WorldGenerator::new(42),
        0,
        pos,
    )
}

fn setup() -> App {
    let mut app = new_test_app_with(
        MockTransport::default(),
        42,
        Persistence::new(None, 10.0),
        GameMode::Survival,
    );
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 0.0;
    for (id, name) in [(BUILDER, "builder"), (OBSERVER, "observer")] {
        command(&mut app, id, ClientToServer::Hello { name: name.into() });
        command(
            &mut app,
            id,
            ClientToServer::UpdatePlayer(save_near(MINER + IVec3::new(2, 2, 3))),
        );
    }
    for z in 10..18 {
        seed_block(&mut app, IVec3::new(10, 100, z), blocks::IRON_ORE);
    }
    for x in 8..16 {
        for z in 8..19 {
            seed_block(&mut app, IVec3::new(x, 99, z), blocks::STONE);
        }
    }
    for (slot, item, pos) in [
        (0, items::MINER, MINER),
        (1, items::BELT, MINER + IVec3::X),
        (2, items::POWERED_FURNACE, MINER + IVec3::X * 2),
        (3, items::FACTORY_STORAGE, STORAGE),
        (4, items::GENERATOR, MINER + IVec3::Z),
    ] {
        seed_block(&mut app, pos, blocks::AIR);
        seed_main_slot(&mut app, BUILDER, slot, ItemStack::one(item));
        let messages = command(
            &mut app,
            BUILDER,
            ClientToServer::PlaceBlock {
                pos,
                hotbar: slot as u8,
            },
        );
        assert!(messages.iter().any(
            |m| matches!(m, ServerToClient::BlockChanged { pos: changed, .. } if *changed == pos)
        ));
        assert!(
            player(&app, BUILDER).main.slot(slot).is_none(),
            "survival placement consumes its machine"
        );
    }
    assert_eq!(
        app.world()
            .resource::<Persistence>()
            .factories
            .machines
            .len(),
        5
    );
    app
}

#[test]
fn placed_factory_produces_without_chunk_requests_and_reports_to_both_viewers() {
    let mut app = setup();
    for id in [BUILDER, OBSERVER] {
        let messages = command(&mut app, id, ClientToServer::OpenContainer { pos: STORAGE });
        assert!(messages.iter().any(|m| matches!(
            m,
            ServerToClient::ContainerOpened {
                kind: ContainerKind::Factory,
                ..
            }
        )));
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, ServerToClient::FactoryStatus(view) if view.pos == STORAGE))
        );
    }
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 20.0;
    app.update();
    let first = amount(&app, STORAGE);
    assert!(
        first >= 1.0,
        "powered production must reach storage: {first}"
    );
    for id in [BUILDER, OBSERVER] {
        let messages = app
            .world_mut()
            .resource_mut::<TransportRes<MockTransport>>()
            .0
            .take(id);
        assert!(messages.iter().any(|m| matches!(m, ServerToClient::FactoryStatus(view) if view.pos == STORAGE && view.output.is_some_and(|b| b.amount >= 1.0))));
        assert!(messages.iter().any(|m| matches!(m, ServerToClient::FactoryFlows { flows } if flows.iter().any(|flow| flow.pos == MINER + IVec3::X && flow.rate > 0.0))));
        assert!(
            !messages
                .iter()
                .any(|m| matches!(m, ServerToClient::ChunkData { .. }))
        );
    }
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 0.0;
    for id in [BUILDER, OBSERVER] {
        command(
            &mut app,
            id,
            ClientToServer::UpdatePlayer(save_at(Vec3::new(3000.0, 103.0, 3000.0))),
        );
        assert!(player(&app, id).open_container.is_none());
    }
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 20.0;
    app.update();
    assert!(amount(&app, STORAGE) > first);
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 0.0;
    command(
        &mut app,
        BUILDER,
        ClientToServer::UpdatePlayer(save_near(STORAGE)),
    );
    let messages = command(
        &mut app,
        BUILDER,
        ClientToServer::OpenContainer { pos: STORAGE },
    );
    assert!(messages.iter().any(|m| matches!(m, ServerToClient::FactoryStatus(view) if view.output.is_some_and(|b| b.amount > first))));
}

#[test]
fn factory_deposit_withdraw_and_capacity_limits_conserve_cursor_and_inventory() {
    let mut app = setup();
    command(
        &mut app,
        BUILDER,
        ClientToServer::OpenContainer { pos: STORAGE },
    );
    let storage_id = app.world().resource::<Persistence>().factories.machines[&STORAGE].id;
    app.world_mut()
        .resource_mut::<Persistence>()
        .factories
        .graph
        .insert(storage_id, items::IRON_INGOT, 4094.0)
        .unwrap();
    seed_main_slot(&mut app, BUILDER, 8, ItemStack::new(items::IRON_INGOT, 5));
    command(
        &mut app,
        BUILDER,
        ClientToServer::SlotClick {
            slot: SlotRef {
                area: SlotArea::Main,
                index: 8,
            },
            right: false,
            shift: false,
        },
    );
    action(&mut app, BUILDER, STORAGE, FactoryAction::Deposit);
    assert_eq!(amount(&app, STORAGE), 4096.0);
    assert_eq!(
        player(&app, BUILDER).cursor,
        Some(ItemStack::new(items::IRON_INGOT, 3))
    );
    for slot in 0..MAIN_INVENTORY_SIZE {
        seed_main_slot(&mut app, BUILDER, slot, ItemStack::new(items::STONE, 64));
    }
    action(&mut app, BUILDER, STORAGE, FactoryAction::Withdraw);
    assert_eq!(amount(&app, STORAGE), 4096.0);
    assert_eq!(player(&app, BUILDER).main.count_of(items::IRON_INGOT), 0);
    seed_main_slot(&mut app, BUILDER, 0, ItemStack::new(items::IRON_INGOT, 63));
    action(&mut app, BUILDER, STORAGE, FactoryAction::Withdraw);
    assert_eq!(amount(&app, STORAGE), 4095.0);
    assert_eq!(player(&app, BUILDER).main.count_of(items::IRON_INGOT), 64);
    assert_eq!(
        player(&app, BUILDER).cursor,
        Some(ItemStack::new(items::IRON_INGOT, 3))
    );
    action(&mut app, BUILDER, STORAGE, FactoryAction::CycleItem);
    assert_eq!(
        amount(&app, STORAGE),
        4095.0,
        "whole contents forbid changing the filter"
    );
    command(&mut app, BUILDER, ClientToServer::CloseContainer);
    assert!(player(&app, BUILDER).cursor.is_none());
    assert_eq!(
        app.world()
            .resource::<SimRes>()
            .items
            .items
            .values()
            .filter(|i| i.stack.item == items::IRON_INGOT)
            .map(|i| i.stack.count)
            .sum::<u32>(),
        3
    );
}

#[test]
fn factory_actions_require_the_live_players_open_nearby_machine() {
    let mut app = setup();
    let direction = app.world().resource::<Persistence>().factories.machines[&STORAGE].direction;
    action(&mut app, BUILDER, STORAGE, FactoryAction::Rotate);
    assert_eq!(
        app.world().resource::<Persistence>().factories.machines[&STORAGE].direction,
        direction
    );
    command(
        &mut app,
        BUILDER,
        ClientToServer::OpenContainer { pos: STORAGE },
    );
    action(&mut app, BUILDER, STORAGE + IVec3::X, FactoryAction::Rotate);
    assert_eq!(
        app.world().resource::<Persistence>().factories.machines[&STORAGE].direction,
        direction
    );
    app.world_mut()
        .resource_mut::<ServerState>()
        .clients
        .get_mut(&BUILDER)
        .unwrap()
        .hp = 0;
    action(&mut app, BUILDER, STORAGE, FactoryAction::Rotate);
    assert_eq!(
        app.world().resource::<Persistence>().factories.machines[&STORAGE].direction,
        direction
    );
    app.world_mut()
        .resource_mut::<ServerState>()
        .clients
        .get_mut(&BUILDER)
        .unwrap()
        .hp = MAX_HP;
    command(
        &mut app,
        BUILDER,
        ClientToServer::UpdatePlayer(save_at(Vec3::new(2000.0, 103.0, 2000.0))),
    );
    action(&mut app, BUILDER, STORAGE, FactoryAction::Rotate);
    assert_eq!(
        app.world().resource::<Persistence>().factories.machines[&STORAGE].direction,
        direction
    );
    command(
        &mut app,
        BUILDER,
        ClientToServer::UpdatePlayer(save_near(STORAGE)),
    );
    command(
        &mut app,
        BUILDER,
        ClientToServer::OpenContainer { pos: STORAGE },
    );
    action(&mut app, BUILDER, STORAGE, FactoryAction::Rotate);
    assert_eq!(
        app.world().resource::<Persistence>().factories.machines[&STORAGE].direction,
        (direction + 1) % 4
    );
    action(&mut app, BUILDER, STORAGE, FactoryAction::Toggle);
    let factories = &app.world().resource::<Persistence>().factories;
    assert!(
        !factories
            .graph
            .node(factories.machines[&STORAGE].id)
            .unwrap()
            .enabled
    );
}

#[test]
fn breaking_a_factory_drops_whole_buffers_and_closes_every_viewer() {
    let mut app = setup();
    for id in [BUILDER, OBSERVER] {
        command(&mut app, id, ClientToServer::OpenContainer { pos: STORAGE });
    }
    let storage_id = app.world().resource::<Persistence>().factories.machines[&STORAGE].id;
    app.world_mut()
        .resource_mut::<Persistence>()
        .factories
        .graph
        .insert(storage_id, items::IRON_INGOT, 5.75)
        .unwrap();
    seed_main_slot(&mut app, BUILDER, 8, ItemStack::one(items::IRON_PICKAXE));
    let messages = command(
        &mut app,
        BUILDER,
        ClientToServer::BreakBlock {
            pos: STORAGE,
            hotbar: 8,
        },
    );
    assert_eq!(block(&mut app, STORAGE), blocks::AIR);
    assert!(
        !app.world()
            .resource::<Persistence>()
            .factories
            .machines
            .contains_key(&STORAGE)
    );
    assert!(messages.iter().any(|m| matches!(m, ServerToClient::ItemSpawned { stack, .. } if stack.item == items::IRON_INGOT && stack.count == 5)));
    assert!(messages.iter().any(|m| matches!(m, ServerToClient::ItemSpawned { stack, .. } if stack.item == items::FACTORY_STORAGE && stack.count == 1)));
    for id in [BUILDER, OBSERVER] {
        assert!(player(&app, id).open_container.is_none());
        let messages = if id == BUILDER {
            messages.clone()
        } else {
            app.world_mut()
                .resource_mut::<TransportRes<MockTransport>>()
                .0
                .take(id)
        };
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, ServerToClient::ContainerClosed))
        );
    }
}

#[test]
fn reserved_ore_is_reconciled_before_manual_mining_and_never_duplicated() {
    let mut app = setup();
    seed_main_slot(&mut app, BUILDER, 8, ItemStack::one(items::IRON_PICKAXE));
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 1.0;
    app.update();
    let consumed = MINER - IVec3::Y;
    assert_eq!(block(&mut app, consumed), blocks::AIR);
    app.world_mut().resource_mut::<SimRes>().tick_interval_secs = 0.0;
    let messages = command(
        &mut app,
        BUILDER,
        ClientToServer::BreakBlock {
            pos: consumed,
            hotbar: 8,
        },
    );
    assert!(!messages.iter().any(
        |m| matches!(m, ServerToClient::ItemSpawned { stack, .. } if stack.item == items::IRON_ORE)
    ));
    let manual = consumed + IVec3::Z * 7;
    command(
        &mut app,
        BUILDER,
        ClientToServer::UpdatePlayer(save_near(manual)),
    );
    command(
        &mut app,
        BUILDER,
        ClientToServer::BreakBlock {
            pos: manual,
            hotbar: 8,
        },
    );
    assert_eq!(block(&mut app, manual), blocks::AIR);
    let factories = &app.world().resource::<Persistence>().factories;
    let mut total = 0.0;
    for node in factories.graph.nodes().values() {
        if let FactoryNodeKind::Miner { remaining, .. } = node.kind {
            total += remaining;
        }
        total += node
            .input
            .iter()
            .chain(node.output.iter())
            .filter(|buffer| [items::IRON_ORE, items::IRON_INGOT].contains(&buffer.item))
            .map(|buffer| buffer.amount)
            .sum::<f64>();
    }
    total += app
        .world()
        .resource::<SimRes>()
        .items
        .items
        .values()
        .filter(|i| i.stack.item == items::IRON_ORE)
        .map(|i| f64::from(i.stack.count))
        .sum::<f64>();
    assert!(
        (total - 8.0).abs() < 1e-7,
        "all ore remains accounted for after manual mining: {total}"
    );
}

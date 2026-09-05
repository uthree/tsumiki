use super::*;

fn picker(pos: Vec3) -> ClientState {
    ClientState {
        hp: MAX_HP,
        save: Some(save_at(pos)),
        ..Default::default()
    }
}

#[test]
fn factory_storage_drop_larger_than_inventory_is_partially_recoverable() {
    const PICKER: ClientId = 1;
    const OBSERVER: ClientId = 2;
    let pos = Vec3::new(4.5, 10.5, 4.5);
    let mut factories = factory::Factories::default();
    let block_pos = IVec3::new(4, 10, 4);
    factories.place(block_pos, blocks::FACTORY_STORAGE, |_| blocks::AIR);
    assert_eq!(
        factories.deposit(block_pos, ItemStack::new(items::IRON_INGOT, 4096)),
        4096
    );
    let drops = factories.remove(block_pos);
    assert_eq!(drops, vec![ItemStack::new(items::IRON_INGOT, 4096)]);
    let mut dropped = sim::ItemsRes::default();
    dropped.insert_loaded(pos, drops[0], 50.0);
    let old_id = *dropped.items.keys().next().unwrap();
    let mut clients = HashMap::from([
        (PICKER, picker(pos)),
        (OBSERVER, picker(pos + Vec3::X * 100.0)),
    ]);
    let mut transport = MockTransport::default();
    let registry = ItemRegistry::prototype();
    assert_eq!(
        sim::tick_items(
            &mut transport,
            &mut clients,
            &mut dropped,
            &registry,
            51.0,
            true
        ),
        vec![PICKER]
    );
    let capacity = (MAIN_INVENTORY_SIZE as u32) * registry.max_stack(items::IRON_INGOT);
    assert_eq!(capacity, 2304);
    assert_eq!(clients[&PICKER].main.count_of(items::IRON_INGOT), capacity);
    assert_eq!(dropped.items.len(), 1);
    let (&new_id, remainder) = dropped.items.iter().next().unwrap();
    assert_ne!(new_id, old_id);
    assert_eq!(
        remainder.stack,
        ItemStack::new(items::IRON_INGOT, 4096 - capacity)
    );
    assert_eq!(remainder.pos, pos);
    assert_eq!(remainder.spawned_at, 50.0);
    for client in [PICKER, OBSERVER] {
        let messages = transport.take(client);
        assert!(
            matches!(messages.as_slice(), [ServerToClient::ItemDespawned { id }, ServerToClient::ItemSpawned { id: spawned, pos: rest, stack }]
            if *id == old_id && *spawned == new_id && *rest == pos && *stack == remainder.stack)
        );
    }
    clients.get_mut(&PICKER).unwrap().main = Inventory::new(MAIN_INVENTORY_SIZE);
    assert_eq!(
        sim::tick_items(
            &mut transport,
            &mut clients,
            &mut dropped,
            &registry,
            51.1,
            true
        ),
        vec![PICKER]
    );
    assert!(dropped.items.is_empty());
    assert_eq!(
        clients[&PICKER].main.count_of(items::IRON_INGOT),
        4096 - capacity
    );
}

#[test]
fn one_remaining_slot_item_fills_and_remainder_keeps_its_original_expiry() {
    const PICKER: ClientId = 1;
    const OBSERVER: ClientId = 2;
    let pos = Vec3::new(4.5, 10.5, 4.5);
    let mut player = picker(pos);
    for slot in 0..MAIN_INVENTORY_SIZE {
        player
            .main
            .set_slot(slot, Some(ItemStack::new(items::DIRT, 64)));
    }
    player
        .main
        .set_slot(0, Some(ItemStack::new(items::IRON_INGOT, 63)));
    let mut clients = HashMap::from([(PICKER, player), (OBSERVER, picker(pos + Vec3::Z * 100.0))]);
    let mut dropped = sim::ItemsRes::default();
    dropped.insert_loaded(pos, ItemStack::new(items::IRON_INGOT, 10), 123.0);
    let old_id = *dropped.items.keys().next().unwrap();
    let mut transport = MockTransport::default();
    let registry = ItemRegistry::prototype();
    assert_eq!(
        sim::tick_items(
            &mut transport,
            &mut clients,
            &mut dropped,
            &registry,
            125.0,
            true
        ),
        vec![PICKER]
    );
    assert_eq!(clients[&PICKER].main.count_of(items::IRON_INGOT), 64);
    let (&new_id, remainder) = dropped.items.iter().next().unwrap();
    assert_ne!(new_id, old_id);
    assert_eq!(remainder.stack.count, 9);
    assert_eq!(remainder.spawned_at, 123.0);
    assert_eq!(remainder.pos, pos);
    transport.take(PICKER);
    transport.take(OBSERVER);
    assert!(
        sim::tick_items(
            &mut transport,
            &mut clients,
            &mut dropped,
            &registry,
            422.9,
            true
        )
        .is_empty()
    );
    assert!(dropped.items.contains_key(&new_id));
    assert!(transport.take(PICKER).is_empty());
    assert!(transport.take(OBSERVER).is_empty());
    assert!(
        sim::tick_items(
            &mut transport,
            &mut clients,
            &mut dropped,
            &registry,
            423.0,
            true
        )
        .is_empty()
    );
    assert!(dropped.items.is_empty());
    for client in [PICKER, OBSERVER] {
        assert!(
            matches!(transport.take(client).as_slice(), [ServerToClient::ItemDespawned { id }] if *id == new_id)
        );
    }
}

#[test]
fn other_players_can_collect_the_remainder_in_the_same_tick() {
    let pos = Vec3::new(4.5, 10.5, 4.5);
    let mut clients = HashMap::from([
        (1, picker(pos)),
        (2, picker(pos)),
        (3, picker(pos + Vec3::X * 100.0)),
    ]);
    let mut dropped = sim::ItemsRes::default();
    dropped.insert_loaded(pos, ItemStack::new(items::IRON_INGOT, 4096), 123.0);
    let old_id = *dropped.items.keys().next().unwrap();
    let mut transport = MockTransport::default();
    let mut changed = sim::tick_items(
        &mut transport,
        &mut clients,
        &mut dropped,
        &ItemRegistry::prototype(),
        125.0,
        true,
    );
    changed.sort_unstable();
    assert_eq!(changed, vec![1, 2]);
    assert_eq!(
        clients[&1].main.count_of(items::IRON_INGOT) + clients[&2].main.count_of(items::IRON_INGOT),
        4096
    );
    assert!(dropped.items.is_empty());
    let observed = transport.take(3);
    assert!(
        matches!(observed.as_slice(), [ServerToClient::ItemDespawned { id }, ServerToClient::ItemSpawned { id: remainder, stack, .. }, ServerToClient::ItemDespawned { id: collected }]
        if *id == old_id && remainder != id && remainder == collected && stack.count == 1792)
    );
}

#[test]
fn completely_full_inventory_preserves_drop_id_count_age_and_emits_no_messages() {
    let pos = Vec3::new(4.5, 10.5, 4.5);
    let mut player = picker(pos);
    for slot in 0..MAIN_INVENTORY_SIZE {
        player
            .main
            .set_slot(slot, Some(ItemStack::new(items::IRON_INGOT, 64)));
    }
    let mut clients = HashMap::from([(1, player)]);
    let mut dropped = sim::ItemsRes::default();
    dropped.insert_loaded(pos, ItemStack::new(items::IRON_INGOT, 4096), 123.0);
    let old_id = *dropped.items.keys().next().unwrap();
    let mut transport = MockTransport::default();
    assert!(
        sim::tick_items(
            &mut transport,
            &mut clients,
            &mut dropped,
            &ItemRegistry::prototype(),
            125.0,
            true
        )
        .is_empty()
    );
    assert_eq!(dropped.items.len(), 1);
    assert_eq!(dropped.items[&old_id].stack.count, 4096);
    assert_eq!(dropped.items[&old_id].spawned_at, 123.0);
    assert_eq!(dropped.items[&old_id].pos, pos);
    assert!(transport.take(1).is_empty());
}

fn spawn_for_merge_test(
    dropped: &mut sim::ItemsRes,
    transport: &mut MockTransport,
    stack: ItemStack,
) {
    let mut cache = ChunkCache::default();
    let mut chunk = tsumiki_world::Chunk::filled(blocks::AIR);
    chunk.set(UVec3::new(1, 0, 1), blocks::STONE);
    cache.chunks.insert(IVec3::ZERO, chunk);
    sim::spawn_item(
        transport,
        &[1],
        dropped,
        &mut cache,
        &WorldGenerator::new(0),
        &BlockRegistry::prototype(),
        0,
        10.0,
        Vec3::new(1.5, 2.5, 1.5),
        stack,
    );
}

#[test]
fn dropped_tool_merge_preserves_wear_and_never_combines_different_damage() {
    let mut dropped = sim::ItemsRes::default();
    let mut transport = MockTransport::default();
    let worn = ItemStack::one(items::WOODEN_PICKAXE).with_damage(7);
    spawn_for_merge_test(&mut dropped, &mut transport, worn);
    spawn_for_merge_test(&mut dropped, &mut transport, worn);
    assert_eq!(dropped.items.len(), 1);
    assert_eq!(
        dropped.items.values().next().unwrap().stack,
        ItemStack { count: 2, ..worn }
    );
    spawn_for_merge_test(
        &mut dropped,
        &mut transport,
        ItemStack::one(worn.item).with_damage(8),
    );
    spawn_for_merge_test(&mut dropped, &mut transport, ItemStack::one(worn.item));
    assert_eq!(dropped.items.len(), 3);
    let mut counts: Vec<_> = dropped
        .items
        .values()
        .map(|item| (item.stack.damage, item.stack.count))
        .collect();
    counts.sort_unstable();
    assert_eq!(counts, vec![(0, 1), (7, 2), (8, 1)]);
}

#[test]
fn merge_that_would_overflow_count_keeps_both_drops() {
    let mut dropped = sim::ItemsRes::default();
    let mut transport = MockTransport::default();
    spawn_for_merge_test(
        &mut dropped,
        &mut transport,
        ItemStack::new(items::DIRT, u32::MAX),
    );
    spawn_for_merge_test(&mut dropped, &mut transport, ItemStack::one(items::DIRT));
    assert_eq!(dropped.items.len(), 2);
    assert_eq!(
        dropped
            .items
            .values()
            .map(|item| u64::from(item.stack.count))
            .sum::<u64>(),
        u64::from(u32::MAX) + 1
    );
}

use super::*;

fn close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-8, "{actual} != {expected}");
}

fn place(factories: &mut Factories, pos: IVec3, block: BlockId) {
    factories.place(pos, block, |_| blocks::AIR);
}

fn ore_line(count: i32, ore: BlockId) -> (Factories, HashMap<IVec3, BlockId>) {
    let terrain: HashMap<_, _> = (0..count).map(|x| (IVec3::new(x, 0, 0), ore)).collect();
    let mut factories = Factories::default();
    factories.place(IVec3::Y, blocks::MINER, |pos| {
        terrain.get(&pos).copied().unwrap_or(blocks::AIR)
    });
    place(&mut factories, IVec3::new(0, 1, 2), blocks::GENERATOR);
    (factories, terrain)
}

fn buffer_amount(factories: &Factories, pos: IVec3) -> f64 {
    factories
        .graph
        .node(factories.machines[&pos].id)
        .unwrap()
        .output
        .unwrap()
        .amount
}

#[test]
fn finite_vein_becomes_real_mined_terrain_without_chunk_objects() {
    let (mut factories, mut terrain) = ore_line(3, blocks::IRON_ORE);
    factories.advance(1.0);
    close(buffer_amount(&factories, IVec3::Y), 0.25);
    let first = factories.mined_blocks();
    assert_eq!(first, vec![IVec3::ZERO]);
    for pos in first {
        terrain.insert(pos, blocks::AIR);
    }
    assert!(factories.mined_blocks().is_empty());
    factories.advance(1e9);
    let rest = factories.mined_blocks();
    assert_eq!(rest, vec![IVec3::X, IVec3::X * 2]);
    for pos in rest {
        terrain.insert(pos, blocks::AIR);
    }
    assert!(terrain.values().all(|block| *block == blocks::AIR));
    close(buffer_amount(&factories, IVec3::Y), 3.0);
    close(factories.view(IVec3::Y).unwrap().reserve, 0.0);
    assert_eq!(
        factories.output(IVec3::Y),
        Some(ItemStack::new(items::IRON_ORE, 3))
    );
}

#[test]
fn manual_mining_of_unconsumed_reservation_reduces_factory_supply_by_one() {
    let (mut factories, _) = ore_line(3, blocks::IRON_ORE);
    factories.advance(1.0);
    assert_eq!(factories.mined_blocks(), vec![IVec3::ZERO]);
    factories.manual_break(IVec3::X * 2);
    close(factories.view(IVec3::Y).unwrap().reserve, 1.75);
    factories.manual_break(IVec3::X * 2);
    close(factories.view(IVec3::Y).unwrap().reserve, 1.75);
    factories.advance(100.0);
    assert_eq!(factories.mined_blocks(), vec![IVec3::X]);
    close(buffer_amount(&factories, IVec3::Y) + 1.0, 3.0);
    assert!(factories.mined_blocks().is_empty());
}

#[test]
fn miner_reservations_are_finite_connected_typed_and_exclusive() {
    let (mut factories, terrain) = ore_line(300, blocks::COAL_ORE);
    let first = &factories.machines[&IVec3::Y];
    assert_eq!(first.vein.len(), MAX_VEIN);
    assert_eq!(first.selected, items::COAL);
    let reserved: HashSet<_> = first.vein.iter().copied().collect();
    let second_pos = IVec3::new(255, 1, 0);
    factories.place(second_pos, blocks::MINER, |pos| {
        terrain.get(&pos).copied().unwrap_or(blocks::AIR)
    });
    assert!(factories.machines[&second_pos].vein.is_empty());
    let third_pos = IVec3::new(299, 1, 0);
    factories.place(third_pos, blocks::MINER, |pos| {
        terrain.get(&pos).copied().unwrap_or(blocks::AIR)
    });
    let third = &factories.machines[&third_pos];
    assert_eq!(third.vein.len(), 44);
    assert!(third.vein.iter().all(|pos| !reserved.contains(pos)));
    let empty_pos = IVec3::new(0, 20, 0);
    place(&mut factories, empty_pos, blocks::MINER);
    assert!(factories.machines[&empty_pos].vein.is_empty());
}

#[test]
fn deposits_transfer_only_exact_whole_items_and_reject_damage_or_filters() {
    let mut factories = Factories::default();
    place(&mut factories, IVec3::ZERO, blocks::BELT);
    let id = factories.machines[&IVec3::ZERO].id;
    factories
        .graph
        .insert(id, items::IRON_ORE, 3.0000000005)
        .unwrap();
    assert_eq!(
        factories.deposit(IVec3::ZERO, ItemStack::one(items::IRON_ORE)),
        0
    );
    close(buffer_amount(&factories, IVec3::ZERO), 3.0000000005);
    factories.extract(IVec3::ZERO, 3);
    assert_eq!(
        factories.deposit(IVec3::ZERO, ItemStack::new(items::IRON_ORE, 10)),
        3
    );
    close(buffer_amount(&factories, IVec3::ZERO), 3.0000000005);
    assert_eq!(
        factories.deposit(IVec3::ZERO, ItemStack::one(items::COAL)),
        0
    );
    assert_eq!(
        factories.deposit(IVec3::ZERO, ItemStack::one(items::IRON_ORE).with_damage(1)),
        0
    );
    factories.graph.extract(id, 4.0).unwrap();
    assert_eq!(
        factories.deposit(IVec3::ZERO, ItemStack::new(items::IRON_ORE, 8)),
        4
    );
    close(buffer_amount(&factories, IVec3::ZERO), 4.0);
    assert_eq!(
        factories.deposit(IVec3::ZERO, ItemStack::one(items::IRON_ORE)),
        0
    );
}

#[test]
fn output_does_not_round_fractional_production_into_free_whole_items() {
    let mut factories = Factories::default();
    place(&mut factories, IVec3::ZERO, blocks::BELT);
    let id = factories.machines[&IVec3::ZERO].id;
    factories
        .graph
        .insert(id, items::IRON_ORE, 0.9999999995)
        .unwrap();
    assert_eq!(factories.output(IVec3::ZERO), None);
    factories.graph.insert(id, items::IRON_ORE, 1.0).unwrap();
    assert_eq!(
        factories.output(IVec3::ZERO),
        Some(ItemStack::one(items::IRON_ORE))
    );
    factories.extract(IVec3::ZERO, 1);
    assert_eq!(factories.output(IVec3::ZERO), None);
    close(buffer_amount(&factories, IVec3::ZERO), 0.9999999995);
    assert!(factories.remove(IVec3::ZERO).is_empty());
}

#[test]
fn large_storage_output_is_bounded_to_an_inventory_stack_and_keeps_remainder() {
    let mut factories = Factories::default();
    place(&mut factories, IVec3::ZERO, blocks::FACTORY_STORAGE);
    assert_eq!(
        factories.deposit(IVec3::ZERO, ItemStack::new(items::IRON_INGOT, 5000)),
        4096
    );
    assert_eq!(
        factories.output(IVec3::ZERO),
        Some(ItemStack::new(items::IRON_INGOT, 64))
    );
    factories.extract(IVec3::ZERO, 64);
    close(buffer_amount(&factories, IVec3::ZERO), 4032.0);
    assert_eq!(
        factories.remove(IVec3::ZERO),
        vec![ItemStack::new(items::IRON_INGOT, 4032)]
    );
}

#[test]
fn saved_wall_time_resumes_once_and_matches_uninterrupted_production() {
    let (mut uninterrupted, _) = ore_line(20, blocks::IRON_ORE);
    uninterrupted.advance(2.25);
    uninterrupted.stamp_save(1000.0);
    let saved = postcard::to_stdvec(&uninterrupted).unwrap();
    let mut restarted: Factories = postcard::from_bytes(&saved).unwrap();
    restarted.resume(1027.5);
    uninterrupted.advance(27.5);
    close(restarted.graph.time(), uninterrupted.graph.time());
    close(
        buffer_amount(&restarted, IVec3::Y),
        buffer_amount(&uninterrupted, IVec3::Y),
    );
    assert_eq!(restarted.mined_blocks(), uninterrupted.mined_blocks());
    restarted.resume(2000.0);
    close(restarted.graph.time(), uninterrupted.graph.time());
    restarted.stamp_save(2000.0);
    restarted.resume(1900.0);
    close(restarted.graph.time(), uninterrupted.graph.time());
}

#[test]
fn line_flow_follows_direction_filter_and_enabled_state() {
    let (mut factories, _) = ore_line(20, blocks::IRON_ORE);
    let belt = IVec3::new(1, 1, 0);
    let furnace = IVec3::new(2, 1, 0);
    let storage = IVec3::new(3, 1, 0);
    place(&mut factories, belt, blocks::BELT);
    place(&mut factories, furnace, blocks::POWERED_FURNACE);
    place(&mut factories, storage, blocks::FACTORY_STORAGE);
    factories.advance(20.0);
    close(buffer_amount(&factories, storage), 2.0);
    close(
        factories
            .flows()
            .iter()
            .find(|flow| flow.pos == belt)
            .unwrap()
            .rate,
        0.25,
    );
    close(factories.view(furnace).unwrap().input.unwrap().amount, 3.0);
    factories.toggle(IVec3::new(0, 1, 2));
    factories.advance(100.0);
    close(buffer_amount(&factories, storage), 2.0);
    close(factories.view(furnace).unwrap().power_ratio, 0.0);
    factories.toggle(IVec3::new(0, 1, 2));
    factories.toggle(belt);
    factories.advance(20.0);
    close(
        factories
            .flows()
            .iter()
            .find(|flow| flow.pos == belt)
            .unwrap()
            .rate,
        0.0,
    );
    close(buffer_amount(&factories, storage), 4.0);
    factories.toggle(belt);
    factories.rotate(belt);
    factories.advance(20.0);
    close(
        factories
            .flows()
            .iter()
            .find(|flow| flow.pos == belt)
            .unwrap()
            .rate,
        0.0,
    );
    close(buffer_amount(&factories, storage), 5.0);
}

#[test]
fn reconfiguration_preserves_whole_items_and_enabled_state() {
    let mut factories = Factories::default();
    let pos = IVec3::ZERO;
    place(&mut factories, pos, blocks::BELT);
    factories.deposit(pos, ItemStack::one(items::IRON_ORE));
    factories.cycle_item(pos);
    assert_eq!(factories.machines[&pos].selected, items::IRON_ORE);
    close(buffer_amount(&factories, pos), 1.0);
    factories.extract(pos, 1);
    factories.toggle(pos);
    factories.cycle_item(pos);
    assert_eq!(factories.machines[&pos].selected, items::IRON_INGOT);
    assert!(!factories.view(pos).unwrap().enabled);
    assert_eq!(factories.deposit(pos, ItemStack::one(items::IRON_ORE)), 0);
    assert_eq!(factories.deposit(pos, ItemStack::one(items::IRON_INGOT)), 1);
}

#[test]
#[ignore = "manual timing probe for graph setup and actual boundary work"]
fn benchmark_factory_graph_boundaries() {
    use std::time::Instant;
    for count in [100_u64, 250, 500] {
        let setup = Instant::now();
        let mut graph = FactoryGraph::default();
        graph
            .add_node(0, FactoryNode::miner(items::IRON_ORE, 1e6, 0.25, 64.0, 0.0))
            .unwrap();
        for id in 1..count {
            graph
                .add_node(
                    id,
                    FactoryNode::storage(items::IRON_ORE, if id + 1 == count { 1.0 } else { 4.0 }),
                )
                .unwrap();
            graph.connect(id, id - 1, id, 2.0).unwrap();
        }
        let setup = setup.elapsed();
        let solve = Instant::now();
        let initial = graph.advance_to(0.0).unwrap();
        let solve = solve.elapsed();
        let steady = Instant::now();
        for step in 1..=100_000 {
            graph.advance_to(f64::from(step) / 100_000.0).unwrap();
        }
        let steady = steady.elapsed();
        let boundary = Instant::now();
        let report = graph.advance_to(4.1).unwrap();
        let boundary = boundary.elapsed();
        assert_eq!(initial.rate_solves, 1);
        assert_eq!(report.events, 1);
        eprintln!(
            "factory {count} nodes: setup {setup:?}, initial solve {solve:?}, 100k steady advances {steady:?}, boundary solve {boundary:?}"
        );
    }
}

#[test]
fn loaded_adapter_validation_rejects_bad_directions_ids_and_missing_nodes() {
    let (mut factories, _) = ore_line(3, blocks::IRON_ORE);
    assert_eq!(factories.validate(), Ok(()));
    factories.machines.get_mut(&IVec3::Y).unwrap().direction = 4;
    assert!(factories.validate().is_err());

    let (mut factories, _) = ore_line(3, blocks::IRON_ORE);
    let miner_id = factories.machines[&IVec3::Y].id;
    factories.machines.get_mut(&IVec3::new(0, 1, 2)).unwrap().id = miner_id;
    assert!(factories.validate().is_err());

    let (mut factories, _) = ore_line(3, blocks::IRON_ORE);
    let id = factories.machines[&IVec3::Y].id;
    let node = factories.graph.remove_node(id).unwrap();
    factories.graph.add_node(999, node).unwrap();
    assert!(factories.validate().is_err());

    let (mut factories, _) = ore_line(3, blocks::IRON_ORE);
    factories.next_id = 0;
    assert!(factories.validate().is_err());
    factories.next_id = u64::MAX;
    assert!(factories.validate().is_err());
}

#[test]
fn loaded_adapter_validation_rejects_invalid_or_duplicate_ore_reservations() {
    let (mut factories, _) = ore_line(3, blocks::IRON_ORE);
    factories.machines.get_mut(&IVec3::Y).unwrap().consumed = 4;
    assert!(factories.validate().is_err());

    let (mut factories, _) = ore_line(3, blocks::IRON_ORE);
    factories.machines.get_mut(&IVec3::Y).unwrap().vein[0].y = -1;
    assert!(factories.validate().is_err());

    let (mut factories, _) = ore_line(3, blocks::IRON_ORE);
    factories.machines.get_mut(&IVec3::Y).unwrap().vein[1] = IVec3::ZERO;
    assert!(factories.validate().is_err());

    let (mut factories, _) = ore_line(3, blocks::IRON_ORE);
    let id = factories.machines[&IVec3::Y].id;
    factories.graph.set_miner_remaining(id, 4.0).unwrap();
    assert!(factories.validate().is_err());
}

#[test]
fn loaded_adapter_validation_rejects_invalid_save_timestamps() {
    for invalid in [-1.0, f64::NAN, f64::INFINITY] {
        let (mut factories, _) = ore_line(3, blocks::IRON_ORE);
        factories.stamp_save(invalid);
        assert!(factories.validate().is_err());
    }
    let (mut factories, _) = ore_line(3, blocks::IRON_ORE);
    factories.advance(0.5);
    factories.stamp_save(1000.0);
    assert_eq!(factories.validate(), Ok(()));
    factories.mined_blocks();
    assert_eq!(factories.validate(), Ok(()));
}

#[test]
fn loaded_adapter_validation_rejects_catalog_mismatch_and_premature_reconciliation() {
    let (mut factories, _) = ore_line(3, blocks::IRON_ORE);
    factories.machines.get_mut(&IVec3::Y).unwrap().consumed = 1;
    assert!(factories.validate().is_err());

    let mut factories = Factories::default();
    place(&mut factories, IVec3::ZERO, blocks::BELT);
    let id = factories.machines[&IVec3::ZERO].id;
    factories.graph.remove_node(id);
    factories
        .graph
        .add_node(id, FactoryNode::storage(items::COAL, 4.0))
        .unwrap();
    assert!(factories.validate().is_err());

    let mut factories = Factories::default();
    place(&mut factories, IVec3::ZERO, blocks::POWERED_FURNACE);
    let id = factories.machines[&IVec3::ZERO].id;
    let mut node = factories.graph.remove_node(id).unwrap();
    let FactoryNodeKind::Smelter { recipe } = &mut node.kind else {
        panic!("expected smelter");
    };
    recipe.output_per_cycle = 999.0;
    factories.graph.add_node(id, node).unwrap();
    assert!(factories.validate().is_err());
}

#[test]
fn incompatible_item_filters_disconnect_and_matching_filters_restore_links() {
    let mut factories = Factories::default();
    place(&mut factories, IVec3::ZERO, blocks::BELT);
    place(&mut factories, IVec3::X, blocks::POWERED_FURNACE);
    assert_eq!(factories.graph.links().len(), 1);
    factories.cycle_item(IVec3::ZERO);
    assert_eq!(factories.machines[&IVec3::ZERO].selected, items::IRON_INGOT);
    assert!(factories.graph.links().is_empty());
    factories.cycle_item(IVec3::X);
    assert_eq!(factories.machines[&IVec3::X].selected, items::BREAD);
    factories.cycle_item(IVec3::ZERO);
    factories.cycle_item(IVec3::ZERO);
    assert_eq!(factories.machines[&IVec3::ZERO].selected, items::BREAD);
    assert_eq!(factories.graph.links().len(), 1);
    assert_eq!(factories.validate(), Ok(()));
}

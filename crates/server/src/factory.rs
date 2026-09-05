//! The voxel-facing factory adapter. The graph owns all production; blocks
//! only locate nodes and edit links. Ore reservations are reconciled when
//! the world is observed or edited, without keeping source chunks loaded.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use bevy_math::IVec3;
use serde::{Deserialize, Serialize};
use tsumiki_protocol::{BeltFlow, FactoryBufferView, FactoryView};
use tsumiki_world::factory::{
    FactoryBuffer, FactoryGraph, FactoryNode, FactoryNodeKind, FactoryRecipe,
};
use tsumiki_world::{BlockId, ItemId, ItemStack, SmeltingRegistry, blocks, items};

const MAX_VEIN: usize = 256;
const DIRECTIONS: [IVec3; 4] = [IVec3::X, IVec3::Z, IVec3::NEG_X, IVec3::NEG_Z];
const SELECTABLE: [ItemId; 6] = [
    items::IRON_ORE,
    items::IRON_INGOT,
    items::COAL,
    items::BREAD,
    items::TOAST,
    items::WHEAT,
];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Machine {
    pub id: u64,
    pub block: BlockId,
    pub direction: u8,
    pub selected: ItemId,
    vein: Vec<IVec3>,
    consumed: usize,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Factories {
    pub graph: FactoryGraph,
    pub machines: HashMap<IVec3, Machine>,
    next_id: u64,
    /// Wall time is used only across a stopped server. Active simulation uses
    /// the same monotonic elapsed clock as the graph and tests.
    saved_at: Option<f64>,
    #[serde(skip)]
    pub broadcast_accum: f64,
}

pub fn unix_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

pub fn is_machine(block: BlockId) -> bool {
    matches!(
        block,
        blocks::MINER
            | blocks::BELT
            | blocks::POWERED_FURNACE
            | blocks::FACTORY_STORAGE
            | blocks::GENERATOR
    )
}

impl Factories {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.graph.validate().map_err(|_| "invalid factory graph")?;
        let nodes = self.graph.nodes();
        let registry = tsumiki_world::ItemRegistry::prototype();
        let mut ids = HashSet::new();
        let mut reserved = HashSet::new();
        if nodes.len() != self.machines.len() || self.next_id == u64::MAX {
            return Err("factory node count or next id is invalid");
        }
        for (pos, machine) in &self.machines {
            if !(0..tsumiki_world::WORLD_HEIGHT_BLOCKS).contains(&pos.y)
                || !is_machine(machine.block)
                || machine.direction >= 4
                || !registry.is_valid(machine.selected)
                || machine.id >= self.next_id
                || !ids.insert(machine.id)
                || machine.consumed > machine.vein.len()
                || machine.vein.len() > MAX_VEIN
            {
                return Err("invalid machine metadata");
            }
            let node = nodes.get(&machine.id).ok_or("missing machine node")?;
            let valid_kind = match &node.kind {
                FactoryNodeKind::Miner { remaining, .. } => {
                    machine.block == blocks::MINER && *remaining <= machine.vein.len() as f64
                }
                FactoryNodeKind::Smelter { recipe } => {
                    machine.block == blocks::POWERED_FURNACE
                        && recipe.input == machine.selected
                        && SmeltingRegistry::prototype().find(recipe.input).is_some()
                }
                FactoryNodeKind::Storage => {
                    matches!(machine.block, blocks::BELT | blocks::FACTORY_STORAGE)
                }
                FactoryNodeKind::Generator { .. } => machine.block == blocks::GENERATOR,
            };
            if !valid_kind
                || node
                    .input
                    .iter()
                    .chain(node.output.iter())
                    .any(|buffer| !registry.is_valid(buffer.item))
            {
                return Err("machine and recipe catalog disagree");
            }
            let mut expected = node_for(machine);
            if let FactoryNodeKind::Miner { remaining, .. } = node.kind {
                let started = (machine.vein.len() as f64 - remaining - 1e-9)
                    .ceil()
                    .max(0.0) as usize;
                if machine.consumed > started {
                    return Err("ore reconciliation is ahead of production");
                }
                if let FactoryNodeKind::Miner {
                    remaining: value, ..
                } = &mut expected.kind
                {
                    *value = remaining;
                }
            }
            let buffer_matches =
                |actual: &Option<FactoryBuffer>, expected: &Option<FactoryBuffer>| match (
                    actual, expected,
                ) {
                    (None, None) => true,
                    (Some(a), Some(b)) => a.item == b.item && a.capacity == b.capacity,
                    _ => false,
                };
            if node.kind != expected.kind
                || !buffer_matches(&node.input, &expected.input)
                || !buffer_matches(&node.output, &expected.output)
            {
                return Err("saved machine rates or buffers disagree with catalog");
            }
            for ore in machine.vein.iter().skip(machine.consumed) {
                if !(0..tsumiki_world::WORLD_HEIGHT_BLOCKS).contains(&ore.y)
                    || !reserved.insert(*ore)
                {
                    return Err("invalid or duplicate ore reservation");
                }
            }
        }
        if self
            .saved_at
            .is_some_and(|time| !time.is_finite() || time < 0.0)
        {
            return Err("invalid factory save timestamp");
        }
        Ok(())
    }
    pub fn stamp_save(&mut self, wall_time: f64) {
        self.saved_at = Some(wall_time);
    }

    pub fn resume(&mut self, wall_time: f64) {
        if let Some(saved) = self.saved_at.take() {
            let elapsed = (wall_time - saved).max(0.0);
            if elapsed.is_finite() {
                self.advance(elapsed);
            }
        }
    }

    pub fn advance(&mut self, elapsed: f64) {
        self.graph
            .advance_to(self.graph.time() + elapsed)
            .expect("valid factory elapsed time");
    }

    /// A miner reserves a connected vein directly below it, excluding ore
    /// already claimed by another miner. Reserved ore remains real terrain
    /// until its first fraction is consumed, and can still be mined by hand.
    pub fn place(&mut self, pos: IVec3, block: BlockId, mut read: impl FnMut(IVec3) -> BlockId) {
        if !is_machine(block) || self.machines.contains_key(&pos) {
            return;
        }
        let mut vein = Vec::new();
        let mut selected = items::IRON_ORE;
        if block == blocks::MINER {
            let start = pos - IVec3::Y;
            let ore = read(start);
            if matches!(ore, blocks::IRON_ORE | blocks::COAL_ORE) {
                selected = if ore == blocks::IRON_ORE {
                    items::IRON_ORE
                } else {
                    items::COAL
                };
                let claimed: HashSet<_> = self
                    .machines
                    .values()
                    .flat_map(|machine| machine.vein.iter().skip(machine.consumed).copied())
                    .collect();
                let mut seen = HashSet::from([start]);
                let mut queue = VecDeque::from([start]);
                while let Some(candidate) = queue.pop_front() {
                    if claimed.contains(&candidate) || read(candidate) != ore {
                        continue;
                    }
                    vein.push(candidate);
                    if vein.len() == MAX_VEIN {
                        break;
                    }
                    for delta in [
                        IVec3::X,
                        IVec3::NEG_X,
                        IVec3::Y,
                        IVec3::NEG_Y,
                        IVec3::Z,
                        IVec3::NEG_Z,
                    ] {
                        let next = candidate + delta;
                        if seen.insert(next) {
                            queue.push_back(next);
                        }
                    }
                }
            }
        } else if block == blocks::FACTORY_STORAGE {
            selected = items::IRON_INGOT;
        }
        let id = self.next_id;
        self.next_id += 1;
        let machine = Machine {
            id,
            block,
            direction: 0,
            selected,
            vein,
            consumed: 0,
        };
        self.graph
            .add_node(id, node_for(&machine))
            .expect("valid machine definition");
        self.machines.insert(pos, machine);
        self.relink();
    }

    /// Each block sends toward its selected horizontal neighbour. Belts are
    /// finite transport buffers, so bends and loops use the same graph rules.
    fn relink(&mut self) {
        let links: Vec<_> = self.graph.links().keys().copied().collect();
        for id in links {
            self.graph.disconnect(id);
        }
        let mut nodes: Vec<_> = self.machines.iter().collect();
        nodes.sort_by_key(|(_, machine)| machine.id);
        for (pos, machine) in nodes {
            let target = *pos + DIRECTIONS[machine.direction as usize];
            if let Some(next) = self.machines.get(&target) {
                // Incompatible item filters intentionally leave a gap. The
                // panel can change them once the machine's buffers are empty.
                let _ = self.graph.connect(machine.id, machine.id, next.id, 2.0);
            }
        }
    }

    /// World edits to apply before serving terrain or accepting a manual
    /// edit. ceil reserves the current partly mined voxel too, preventing a
    /// player from collecting that same ore while its fraction is in flight.
    pub fn mined_blocks(&mut self) -> Vec<IVec3> {
        let mut mined = Vec::new();
        for machine in self.machines.values_mut() {
            let Some(FactoryNode {
                kind: FactoryNodeKind::Miner { remaining, .. },
                ..
            }) = self.graph.node(machine.id)
            else {
                continue;
            };
            let completed = ((machine.vein.len() as f64 - remaining - 1e-9)
                .ceil()
                .max(0.0) as usize)
                .min(machine.vein.len());
            while machine.consumed < completed {
                mined.push(machine.vein[machine.consumed]);
                machine.consumed += 1;
            }
        }
        mined.sort_by_key(|pos| (pos.x, pos.y, pos.z));
        mined
    }

    /// Manual mining removes only unconsumed reservations. The graph's
    /// remaining amount decreases by exactly the manually collected unit.
    pub fn manual_break(&mut self, pos: IVec3) {
        for machine in self.machines.values_mut() {
            if let Some(index) = machine
                .vein
                .iter()
                .position(|p| *p == pos)
                .filter(|index| *index >= machine.consumed)
            {
                machine.vein.remove(index);
                if let Some(FactoryNode {
                    kind: FactoryNodeKind::Miner { remaining, .. },
                    ..
                }) = self.graph.node(machine.id)
                {
                    self.graph
                        .set_miner_remaining(machine.id, (remaining - 1.0).max(0.0))
                        .expect("miner reservation");
                }
            }
        }
    }

    pub fn remove(&mut self, pos: IVec3) -> Vec<ItemStack> {
        let Some(machine) = self.machines.remove(&pos) else {
            return Vec::new();
        };
        let mut drops = Vec::new();
        if let Some(node) = self.graph.remove_node(machine.id) {
            for buffer in node.input.into_iter().chain(node.output) {
                let amount = buffer.amount.floor() as u32;
                if amount > 0 {
                    drops.push(ItemStack::new(buffer.item, amount));
                }
            }
        }
        self.relink();
        drops
    }

    pub fn rotate(&mut self, pos: IVec3) {
        if let Some(machine) = self.machines.get_mut(&pos) {
            machine.direction = (machine.direction + 1) % 4;
            self.relink();
        }
    }

    pub fn toggle(&mut self, pos: IVec3) {
        if let Some(machine) = self.machines.get(&pos)
            && let Some(node) = self.graph.node(machine.id)
        {
            self.graph
                .set_enabled(machine.id, !node.enabled)
                .expect("known node");
        }
    }

    pub fn cycle_item(&mut self, pos: IVec3) {
        let Some(machine) = self.machines.get_mut(&pos) else {
            return;
        };
        if matches!(machine.block, blocks::MINER | blocks::GENERATOR) {
            return;
        }
        let Some(node) = self.graph.node(machine.id) else {
            return;
        };
        if node
            .input
            .iter()
            .chain(node.output.iter())
            .any(|buffer| buffer.amount >= 1.0 - 1e-9)
        {
            return;
        }
        let options = if machine.block == blocks::POWERED_FURNACE {
            vec![items::IRON_ORE, items::BREAD]
        } else {
            SELECTABLE.to_vec()
        };
        let index = options
            .iter()
            .position(|item| *item == machine.selected)
            .unwrap_or(0);
        machine.selected = options[(index + 1) % options.len()];
        self.graph.remove_node(machine.id);
        let mut replacement = node_for(machine);
        replacement.enabled = node.enabled;
        self.graph
            .add_node(machine.id, replacement)
            .expect("valid replacement");
        self.relink();
    }

    pub fn deposit(&mut self, pos: IVec3, stack: ItemStack) -> u32 {
        let Some(machine) = self.machines.get(&pos) else {
            return 0;
        };
        if stack.damage != 0 {
            return 0;
        }
        let Some(node) = self.graph.node(machine.id) else {
            return 0;
        };
        let buffer = if matches!(node.kind, FactoryNodeKind::Storage) {
            node.output
        } else {
            node.input
        };
        let Some(buffer) = buffer.filter(|buffer| buffer.item == stack.item) else {
            return 0;
        };
        let count = stack
            .count
            .min((buffer.capacity - buffer.amount).floor().max(0.0) as u32);
        if count == 0 {
            return 0;
        }
        self.graph
            .insert(machine.id, stack.item, count as f64)
            .expect("matching input")
            .round() as u32
    }

    pub fn output(&self, pos: IVec3) -> Option<ItemStack> {
        let machine = self.machines.get(&pos)?;
        let buffer = self.graph.node(machine.id)?.output?;
        let count = (buffer.amount.floor() as u32).min(64);
        (count > 0).then(|| ItemStack::new(buffer.item, count))
    }

    pub fn extract(&mut self, pos: IVec3, count: u32) {
        if let Some(machine) = self.machines.get(&pos) {
            self.graph
                .extract(machine.id, count as f64)
                .expect("known output");
        }
    }

    pub fn view(&mut self, pos: IVec3) -> Option<FactoryView> {
        let machine = self.machines.get(&pos)?;
        let node = self.graph.node(machine.id)?;
        let rates = self.graph.rates();
        let incoming: f64 = self
            .graph
            .links()
            .iter()
            .filter(|(_, link)| link.to == machine.id)
            .map(|(id, _)| rates.links.get(id).copied().unwrap_or(0.0))
            .sum();
        let outgoing: f64 = self
            .graph
            .links()
            .iter()
            .filter(|(_, link)| link.from == machine.id)
            .map(|(id, _)| rates.links.get(id).copied().unwrap_or(0.0))
            .sum();
        let speed = rates.machines.get(&machine.id).copied().unwrap_or(0.0);
        let (input_rate, output_rate, reserve) = match &node.kind {
            FactoryNodeKind::Miner { remaining, .. } => (0.0, speed - outgoing, *remaining),
            FactoryNodeKind::Smelter { recipe } => (
                incoming - speed * recipe.input_per_cycle,
                speed * recipe.output_per_cycle - outgoing,
                0.0,
            ),
            FactoryNodeKind::Storage => (0.0, incoming - outgoing, 0.0),
            FactoryNodeKind::Generator { .. } => (0.0, 0.0, 0.0),
        };
        let buffer_view = |buffer: FactoryBuffer, rate| FactoryBufferView {
            item: buffer.item,
            amount: buffer.amount,
            capacity: buffer.capacity,
            rate,
        };
        Some(FactoryView {
            pos,
            block: machine.block,
            direction: machine.direction,
            enabled: node.enabled,
            input: node.input.map(|b| buffer_view(b, input_rate)),
            output: node.output.map(|b| buffer_view(b, output_rate)),
            reserve,
            power_ratio: rates.power_fraction,
        })
    }

    pub fn flows(&mut self) -> Vec<BeltFlow> {
        let rates = self.graph.rates();
        let mut flows: Vec<_> = self
            .machines
            .iter()
            .filter(|(_, machine)| machine.block == blocks::BELT)
            .map(|(pos, machine)| BeltFlow {
                pos: *pos,
                direction: machine.direction,
                item: machine.selected,
                rate: rates.links.get(&machine.id).copied().unwrap_or(0.0),
            })
            .collect();
        flows.sort_by_key(|flow| (flow.pos.x, flow.pos.y, flow.pos.z));
        flows
    }
}

fn node_for(machine: &Machine) -> FactoryNode {
    match machine.block {
        blocks::MINER => {
            FactoryNode::miner(machine.selected, machine.vein.len() as f64, 0.25, 64.0, 1.0)
        }
        blocks::POWERED_FURNACE => FactoryNode::smelter(
            FactoryRecipe::from_smelting(
                SmeltingRegistry::prototype()
                    .find(machine.selected)
                    .expect("supported smelting selection"),
                2.0,
            ),
            64.0,
            64.0,
        ),
        blocks::BELT => FactoryNode::storage(machine.selected, 4.0),
        blocks::FACTORY_STORAGE => FactoryNode::storage(machine.selected, 4096.0),
        blocks::GENERATOR => FactoryNode::generator(4.0),
        _ => unreachable!("only machine blocks create nodes"),
    }
}

#[cfg(test)]
#[path = "factory_tests.rs"]
mod tests;

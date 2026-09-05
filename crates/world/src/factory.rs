//! Chunk-independent, continuous factory production (design.md section 4).
//!
//! Buffers are anchored at a simulation time and carry constant rates until a
//! buffer boundary or exhausted deposit changes the feasible flows. Advancing
//! through a steady interval changes only the clock, regardless of its length.
//! Queries and serialization project the anchored amounts to that clock.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{ItemId, SmeltRecipe};

pub type NodeId = u64;
pub type LinkId = u64;
const EPSILON: f64 = 1e-9;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FactoryBuffer {
    pub item: ItemId,
    pub amount: f64,
    pub capacity: f64,
}

impl FactoryBuffer {
    fn empty(item: ItemId, capacity: f64) -> Self {
        Self {
            item,
            amount: 0.0,
            capacity,
        }
    }
}

/// One declarative conversion. Power replaces the hand furnace's fuel slot;
/// its item conversion and duration come directly from the smelting table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FactoryRecipe {
    pub input: ItemId,
    pub output: ItemId,
    pub input_per_cycle: f64,
    pub output_per_cycle: f64,
    pub cycles_per_second: f64,
    pub power: f64,
}

impl FactoryRecipe {
    pub fn from_smelting(recipe: &SmeltRecipe, power: f64) -> Self {
        Self {
            input: recipe.input,
            output: recipe.output.item,
            input_per_cycle: 1.0,
            output_per_cycle: f64::from(recipe.output.count),
            cycles_per_second: 1.0 / f64::from(recipe.secs_per_item),
            power,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FactoryNodeKind {
    Miner {
        remaining: f64,
        items_per_second: f64,
        power: f64,
    },
    Smelter {
        recipe: FactoryRecipe,
    },
    Storage,
    Generator {
        supply: f64,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FactoryNode {
    pub kind: FactoryNodeKind,
    pub input: Option<FactoryBuffer>,
    /// Storage uses this one buffer as both its inlet and outlet.
    pub output: Option<FactoryBuffer>,
    pub enabled: bool,
}

impl FactoryNode {
    pub fn miner(
        item: ItemId,
        reserve: f64,
        items_per_second: f64,
        capacity: f64,
        power: f64,
    ) -> Self {
        Self {
            kind: FactoryNodeKind::Miner {
                remaining: reserve,
                items_per_second,
                power,
            },
            input: None,
            output: Some(FactoryBuffer::empty(item, capacity)),
            enabled: true,
        }
    }

    pub fn smelter(recipe: FactoryRecipe, input_capacity: f64, output_capacity: f64) -> Self {
        Self {
            input: Some(FactoryBuffer::empty(recipe.input, input_capacity)),
            output: Some(FactoryBuffer::empty(recipe.output, output_capacity)),
            kind: FactoryNodeKind::Smelter { recipe },
            enabled: true,
        }
    }

    pub fn storage(item: ItemId, capacity: f64) -> Self {
        Self {
            kind: FactoryNodeKind::Storage,
            input: None,
            output: Some(FactoryBuffer::empty(item, capacity)),
            enabled: true,
        }
    }

    pub fn generator(supply: f64) -> Self {
        Self {
            kind: FactoryNodeKind::Generator { supply },
            input: None,
            output: None,
            enabled: true,
        }
    }

    fn inlet(&self) -> Option<&FactoryBuffer> {
        if matches!(self.kind, FactoryNodeKind::Storage) {
            self.output.as_ref()
        } else {
            self.input.as_ref()
        }
    }

    fn inlet_mut(&mut self) -> Option<&mut FactoryBuffer> {
        if matches!(self.kind, FactoryNodeKind::Storage) {
            self.output.as_mut()
        } else {
            self.input.as_mut()
        }
    }

    fn valid(&self) -> bool {
        let valid_buffer = |buffer: &FactoryBuffer| {
            buffer.item.0 != 0
                && positive(buffer.capacity)
                && nonnegative(buffer.amount)
                && buffer.amount <= buffer.capacity
        };
        if !self
            .input
            .iter()
            .chain(self.output.iter())
            .all(valid_buffer)
        {
            return false;
        }
        match &self.kind {
            FactoryNodeKind::Miner {
                remaining,
                items_per_second,
                power,
            } => {
                self.input.is_none()
                    && self.output.is_some()
                    && nonnegative(*remaining)
                    && positive(*items_per_second)
                    && nonnegative(*power)
            }
            FactoryNodeKind::Smelter { recipe } => {
                self.input.as_ref().is_some_and(|b| b.item == recipe.input)
                    && self
                        .output
                        .as_ref()
                        .is_some_and(|b| b.item == recipe.output)
                    && recipe.input != recipe.output
                    && positive(recipe.input_per_cycle)
                    && positive(recipe.output_per_cycle)
                    && positive(recipe.cycles_per_second)
                    && nonnegative(recipe.power)
            }
            FactoryNodeKind::Storage => self.input.is_none() && self.output.is_some(),
            FactoryNodeKind::Generator { supply } => {
                self.input.is_none() && self.output.is_none() && nonnegative(*supply)
            }
        }
    }
}

fn positive(value: f64) -> bool {
    value.is_finite() && value > 0.0
}
fn nonnegative(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FactoryLink {
    pub from: NodeId,
    pub to: NodeId,
    pub throughput: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FactoryError {
    InvalidTime,
    InvalidNode,
    DuplicateNode,
    MissingNode,
    DuplicateLink,
    InvalidLink,
    WrongItem,
    InvalidAmount,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FactoryRates {
    /// Items per second for miners; recipe cycles per second for smelters.
    pub machines: BTreeMap<NodeId, f64>,
    pub links: BTreeMap<LinkId, f64>,
    pub power_supply: f64,
    pub power_demand: f64,
    pub power_fraction: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AdvanceReport {
    pub events: usize,
    pub rate_solves: usize,
}

#[derive(Clone, Debug, Default)]
struct NodeRate {
    input: f64,
    output: f64,
    reserve: f64,
}

#[derive(Clone, Debug)]
struct RatePlan {
    nodes: BTreeMap<NodeId, NodeRate>,
    public: FactoryRates,
    next_event: f64,
}

/// Mutations take effect at `time()`. Call `advance_to` before editing or
/// observing a factory when the authoritative server clock has moved on.
#[derive(Clone, Debug, Default)]
pub struct FactoryGraph {
    time: f64,
    anchor: f64,
    nodes: BTreeMap<NodeId, FactoryNode>,
    links: BTreeMap<LinkId, FactoryLink>,
    plan: Option<RatePlan>,
}

#[derive(Serialize, Deserialize)]
struct SavedGraph {
    time: f64,
    nodes: BTreeMap<NodeId, FactoryNode>,
    links: BTreeMap<LinkId, FactoryLink>,
}

impl Serialize for FactoryGraph {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        SavedGraph {
            time: self.time,
            nodes: self.nodes(),
            links: self.links.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for FactoryGraph {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let saved = SavedGraph::deserialize(deserializer)?;
        let graph = Self {
            time: saved.time,
            anchor: saved.time,
            nodes: saved.nodes,
            links: saved.links,
            plan: None,
        };
        graph.validate().map_err(|error| {
            serde::de::Error::custom(format!("invalid factory graph: {error:?}"))
        })?;
        Ok(graph)
    }
}

impl FactoryGraph {
    pub fn time(&self) -> f64 {
        self.time
    }

    pub fn node(&self, id: NodeId) -> Option<FactoryNode> {
        let mut node = self.nodes.get(&id)?.clone();
        if let Some(rate) = self.plan.as_ref().and_then(|plan| plan.nodes.get(&id)) {
            project(&mut node, rate, self.time - self.anchor);
        }
        Some(node)
    }

    pub fn nodes(&self) -> BTreeMap<NodeId, FactoryNode> {
        self.nodes
            .keys()
            .map(|&id| (id, self.node(id).expect("existing node")))
            .collect()
    }

    pub fn links(&self) -> &BTreeMap<LinkId, FactoryLink> {
        &self.links
    }

    pub fn add_node(&mut self, id: NodeId, node: FactoryNode) -> Result<(), FactoryError> {
        if self.nodes.contains_key(&id) {
            return Err(FactoryError::DuplicateNode);
        }
        if !node.valid() {
            return Err(FactoryError::InvalidNode);
        }
        self.materialize();
        self.nodes.insert(id, node);
        self.plan = None;
        Ok(())
    }

    /// Removing a machine also removes its transport links. The caller owns
    /// the returned contents and can drop or transfer whole items explicitly.
    pub fn remove_node(&mut self, id: NodeId) -> Option<FactoryNode> {
        self.materialize();
        let node = self.nodes.remove(&id)?;
        self.links
            .retain(|_, link| link.from != id && link.to != id);
        self.plan = None;
        Some(node)
    }

    pub fn connect(
        &mut self,
        id: LinkId,
        from: NodeId,
        to: NodeId,
        throughput: f64,
    ) -> Result<(), FactoryError> {
        if self.links.contains_key(&id) {
            return Err(FactoryError::DuplicateLink);
        }
        let link = FactoryLink {
            from,
            to,
            throughput,
        };
        if !self.valid_link(&link) {
            return Err(FactoryError::InvalidLink);
        }
        self.materialize();
        self.links.insert(id, link);
        self.plan = None;
        Ok(())
    }

    pub fn disconnect(&mut self, id: LinkId) -> Option<FactoryLink> {
        self.materialize();
        let result = self.links.remove(&id);
        if result.is_some() {
            self.plan = None;
        }
        result
    }

    pub fn set_enabled(&mut self, id: NodeId, enabled: bool) -> Result<(), FactoryError> {
        self.materialize();
        let node = self.nodes.get_mut(&id).ok_or(FactoryError::MissingNode)?;
        node.enabled = enabled;
        self.plan = None;
        Ok(())
    }

    /// Reconciles the finite deposit with authoritative world edits, after
    /// advancing to the edit time. Already produced output is preserved.
    pub fn set_miner_remaining(&mut self, id: NodeId, amount: f64) -> Result<(), FactoryError> {
        if !nonnegative(amount) {
            return Err(FactoryError::InvalidAmount);
        }
        self.materialize();
        let node = self.nodes.get_mut(&id).ok_or(FactoryError::MissingNode)?;
        let FactoryNodeKind::Miner { remaining, .. } = &mut node.kind else {
            return Err(FactoryError::InvalidNode);
        };
        *remaining = amount;
        self.plan = None;
        Ok(())
    }

    /// Inserts into a smelter input or a storage buffer. Returns the amount
    /// accepted; fractional quantities remain internal to the rate graph.
    pub fn insert(&mut self, id: NodeId, item: ItemId, amount: f64) -> Result<f64, FactoryError> {
        if !nonnegative(amount) {
            return Err(FactoryError::InvalidAmount);
        }
        self.materialize();
        let buffer = self
            .nodes
            .get_mut(&id)
            .ok_or(FactoryError::MissingNode)?
            .inlet_mut()
            .ok_or(FactoryError::WrongItem)?;
        if buffer.item != item {
            return Err(FactoryError::WrongItem);
        }
        let accepted = amount.min((buffer.capacity - buffer.amount).max(0.0));
        buffer.amount += accepted;
        self.plan = None;
        Ok(accepted)
    }

    /// Extracts up to `amount` from the output. Server inventory operations
    /// should request whole quantities, leaving fractional production here.
    pub fn extract(
        &mut self,
        id: NodeId,
        amount: f64,
    ) -> Result<Option<(ItemId, f64)>, FactoryError> {
        if !nonnegative(amount) {
            return Err(FactoryError::InvalidAmount);
        }
        self.materialize();
        let node = self.nodes.get_mut(&id).ok_or(FactoryError::MissingNode)?;
        let Some(buffer) = node.output.as_mut() else {
            return Ok(None);
        };
        let taken = amount.min(buffer.amount);
        buffer.amount -= taken;
        self.plan = None;
        Ok((taken > 0.0).then_some((buffer.item, taken)))
    }

    pub fn rates(&mut self) -> FactoryRates {
        self.ensure_plan();
        self.plan.as_ref().expect("computed plan").public.clone()
    }

    pub fn advance_to(&mut self, time: f64) -> Result<AdvanceReport, FactoryError> {
        if !nonnegative(time) || time < self.time {
            return Err(FactoryError::InvalidTime);
        }
        let mut report = AdvanceReport::default();
        if self.plan.is_none() {
            self.ensure_plan();
            report.rate_solves += 1;
        }
        while self.plan.as_ref().expect("computed plan").next_event <= time {
            self.time = self.plan.as_ref().expect("computed plan").next_event;
            self.materialize();
            self.plan = None;
            self.ensure_plan();
            report.events += 1;
            report.rate_solves += 1;
        }
        self.time = time;
        Ok(report)
    }

    pub fn validate(&self) -> Result<(), FactoryError> {
        if !nonnegative(self.time) {
            return Err(FactoryError::InvalidTime);
        }
        if self.nodes.values().any(|node| !node.valid()) {
            return Err(FactoryError::InvalidNode);
        }
        if self.links.values().any(|link| !self.valid_link(link)) {
            return Err(FactoryError::InvalidLink);
        }
        Ok(())
    }

    fn valid_link(&self, link: &FactoryLink) -> bool {
        if link.from == link.to || !positive(link.throughput) {
            return false;
        }
        let Some(from) = self
            .nodes
            .get(&link.from)
            .and_then(|node| node.output.as_ref())
        else {
            return false;
        };
        let Some(to) = self.nodes.get(&link.to).and_then(FactoryNode::inlet) else {
            return false;
        };
        from.item == to.item
    }

    fn materialize(&mut self) {
        if let Some(plan) = &self.plan {
            let elapsed = self.time - self.anchor;
            if elapsed > 0.0 {
                for (&id, node) in &mut self.nodes {
                    if let Some(rate) = plan.nodes.get(&id) {
                        project(node, rate, elapsed);
                    }
                }
            }
        }
        self.anchor = self.time;
    }

    fn ensure_plan(&mut self) {
        if self.plan.is_none() {
            self.materialize();
            self.plan = Some(solve_rates(&self.nodes, &self.links, self.anchor));
        }
    }
}

fn project(node: &mut FactoryNode, rate: &NodeRate, elapsed: f64) {
    if let Some(input) = &mut node.input {
        input.amount = (input.amount + rate.input * elapsed).clamp(0.0, input.capacity);
    }
    if let Some(output) = &mut node.output {
        output.amount = (output.amount + rate.output * elapsed).clamp(0.0, output.capacity);
    }
    if let FactoryNodeKind::Miner { remaining, .. } = &mut node.kind {
        *remaining = (*remaining + rate.reserve * elapsed).max(0.0);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Port {
    Input(NodeId),
    Output(NodeId),
    Reserve(NodeId),
}

struct Operation {
    machine: Option<NodeId>,
    link: Option<LinkId>,
    capacity: f64,
    changes: Vec<(Port, f64)>,
}

fn inlet_port(id: NodeId, node: &FactoryNode) -> Port {
    if matches!(node.kind, FactoryNodeKind::Storage) {
        Port::Output(id)
    } else {
        Port::Input(id)
    }
}

fn solve_rates(
    nodes: &BTreeMap<NodeId, FactoryNode>,
    links: &BTreeMap<LinkId, FactoryLink>,
    time: f64,
) -> RatePlan {
    let mut public = FactoryRates {
        power_fraction: 1.0,
        ..Default::default()
    };
    let mut buffers = BTreeMap::new();
    for (&id, node) in nodes {
        if let Some(buffer) = &node.input {
            buffers.insert(Port::Input(id), (buffer.amount, buffer.capacity));
        }
        if let Some(buffer) = &node.output {
            buffers.insert(Port::Output(id), (buffer.amount, buffer.capacity));
        }
        if let FactoryNodeKind::Miner { remaining, .. } = node.kind {
            buffers.insert(Port::Reserve(id), (remaining, f64::INFINITY));
        }
        if node.enabled {
            match &node.kind {
                FactoryNodeKind::Miner {
                    remaining, power, ..
                } if *remaining > EPSILON => public.power_demand += power,
                FactoryNodeKind::Smelter { recipe } => public.power_demand += recipe.power,
                FactoryNodeKind::Generator { supply } => public.power_supply += supply,
                _ => {}
            }
        }
    }
    if public.power_demand > 0.0 {
        public.power_fraction = (public.power_supply / public.power_demand).min(1.0);
    }
    let mut operations = Vec::new();
    for (&id, node) in nodes {
        if !node.enabled {
            continue;
        }
        let power_factor = |power| {
            if power == 0.0 {
                1.0
            } else {
                public.power_fraction
            }
        };
        match &node.kind {
            FactoryNodeKind::Miner {
                remaining,
                items_per_second,
                power,
            } if *remaining > EPSILON => operations.push(Operation {
                machine: Some(id),
                link: None,
                capacity: items_per_second * power_factor(*power),
                changes: vec![(Port::Reserve(id), -1.0), (Port::Output(id), 1.0)],
            }),
            FactoryNodeKind::Smelter { recipe } => operations.push(Operation {
                machine: Some(id),
                link: None,
                capacity: recipe.cycles_per_second * power_factor(recipe.power),
                changes: vec![
                    (Port::Input(id), -recipe.input_per_cycle),
                    (Port::Output(id), recipe.output_per_cycle),
                ],
            }),
            _ => {}
        }
    }
    for (&id, link) in links {
        if !nodes[&link.from].enabled || !nodes[&link.to].enabled {
            continue;
        }
        operations.push(Operation {
            machine: None,
            link: Some(id),
            capacity: link.throughput,
            changes: vec![
                (Port::Output(link.from), -1.0),
                (inlet_port(link.to, &nodes[&link.to]), 1.0),
            ],
        });
    }

    // An entirely empty directed cycle cannot start circulating imaginary
    // material. Reachability starts with actual contents (including deposits)
    // and follows powered transformations and directed transport only.
    let mut reachable: BTreeSet<_> = buffers
        .iter()
        .filter(|(_, (amount, _))| *amount > EPSILON)
        .map(|(&port, _)| port)
        .collect();
    loop {
        let before = reachable.len();
        for operation in &operations {
            if operation.capacity > EPSILON
                && operation
                    .changes
                    .iter()
                    .filter(|(_, change)| *change < 0.0)
                    .all(|(port, _)| reachable.contains(port))
            {
                reachable.extend(
                    operation
                        .changes
                        .iter()
                        .filter(|(_, change)| *change > 0.0)
                        .map(|(port, _)| *port),
                );
            }
        }
        if before == reachable.len() {
            break;
        }
    }
    operations.retain(|operation| {
        operation.capacity > EPSILON
            && operation
                .changes
                .iter()
                .filter(|(_, change)| *change < 0.0)
                .all(|(port, _)| reachable.contains(port))
    });

    let count = operations.len();
    let mut constraints = Vec::new();
    let mut bounds = Vec::new();
    for (index, operation) in operations.iter().enumerate() {
        let mut row = vec![0.0; count];
        row[index] = 1.0;
        constraints.push(row);
        bounds.push(operation.capacity);
    }
    for (&port, &(amount, capacity)) in &buffers {
        let direction = if amount <= EPSILON {
            -1.0
        } else if amount >= capacity - EPSILON {
            1.0
        } else {
            continue;
        };
        let row = operations
            .iter()
            .map(|operation| {
                direction
                    * operation
                        .changes
                        .iter()
                        .filter(|(changed, _)| *changed == port)
                        .map(|(_, coefficient)| coefficient)
                        .sum::<f64>()
            })
            .collect();
        constraints.push(row);
        bounds.push(0.0);
    }
    // Every variable is bounded and zero is feasible. A deterministic simplex
    // solves the simultaneous boundary constraints, including full-buffer
    // pass-through and cycles, without iterative rate attenuation or chatter.
    let objectives: Vec<_> = operations
        .iter()
        .map(|operation| {
            if operation.machine.is_some() {
                1.0
            } else {
                0.25
            }
        })
        .collect();
    let values = maximize(&constraints, &bounds, &objectives);
    let mut rates: BTreeMap<_, NodeRate> =
        nodes.keys().map(|&id| (id, NodeRate::default())).collect();
    for (operation, value) in operations.iter().zip(values) {
        if let Some(id) = operation.machine {
            public.machines.insert(id, value);
        }
        if let Some(id) = operation.link {
            public.links.insert(id, value);
        }
        for &(port, coefficient) in &operation.changes {
            match port {
                Port::Input(id) => rates.get_mut(&id).expect("node").input += value * coefficient,
                Port::Output(id) => rates.get_mut(&id).expect("node").output += value * coefficient,
                Port::Reserve(id) => {
                    rates.get_mut(&id).expect("node").reserve += value * coefficient
                }
            }
        }
    }
    let mut next_event = f64::INFINITY;
    for (&port, &(amount, capacity)) in &buffers {
        let rate = match port {
            Port::Input(id) => &mut rates.get_mut(&id).expect("node").input,
            Port::Output(id) => &mut rates.get_mut(&id).expect("node").output,
            Port::Reserve(id) => &mut rates.get_mut(&id).expect("node").reserve,
        };
        if rate.abs() < EPSILON {
            *rate = 0.0;
        }
        let until = if *rate > 0.0 && capacity.is_finite() {
            (capacity - amount) / *rate
        } else if *rate < 0.0 {
            -amount / *rate
        } else {
            f64::INFINITY
        };
        if until > EPSILON {
            next_event = next_event.min(time + until);
        }
    }
    RatePlan {
        nodes: rates,
        public,
        next_event,
    }
}

/// Primal simplex with Bland's entering/leaving rule. All constraints have
/// nonnegative right-hand sides, so the initial slack basis is feasible; the
/// variable caps guarantee boundedness. Bland's rule terminates even on the
/// degenerate zero-capacity boundaries common in empty production lines.
fn maximize(a: &[Vec<f64>], b: &[f64], objective: &[f64]) -> Vec<f64> {
    let n = objective.len();
    let m = b.len();
    if n == 0 {
        return Vec::new();
    }
    let last = n + m;
    let mut table = vec![vec![0.0; last + 1]; m + 1];
    let mut basis: Vec<_> = (n..n + m).collect();
    for row in 0..m {
        table[row][..n].copy_from_slice(&a[row]);
        table[row][n + row] = 1.0;
        table[row][last] = b[row];
    }
    for (column, value) in objective.iter().enumerate() {
        table[m][column] = -value;
    }
    while let Some(column) = (0..last).find(|&column| table[m][column] < -EPSILON) {
        let row = (0..m)
            .filter(|&row| table[row][column] > EPSILON)
            .min_by(|&left, &right| {
                let l = table[left][last] / table[left][column];
                let r = table[right][last] / table[right][column];
                if (l - r).abs() <= EPSILON {
                    basis[left].cmp(&basis[right])
                } else {
                    l.total_cmp(&r)
                }
            })
            .expect("bounded factory operation");
        let divisor = table[row][column];
        for value in &mut table[row] {
            *value /= divisor;
        }
        for other in 0..=m {
            if other == row {
                continue;
            }
            let scale = table[other][column];
            if scale == 0.0 {
                continue;
            }
            for col in 0..=last {
                table[other][col] -= scale * table[row][col];
            }
        }
        basis[row] = column;
    }
    let mut values = vec![0.0; n];
    for row in 0..m {
        if basis[row] < n {
            values[basis[row]] = table[row][last].max(0.0);
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SmeltingRegistry, items};

    fn close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-7, "{actual} != {expected}");
    }

    fn output(graph: &FactoryGraph, id: NodeId) -> f64 {
        graph.node(id).unwrap().output.unwrap().amount
    }

    fn remaining(graph: &FactoryGraph, id: NodeId) -> f64 {
        match graph.node(id).unwrap().kind {
            FactoryNodeKind::Miner { remaining, .. } => remaining,
            _ => panic!("expected miner"),
        }
    }

    fn miner_line(
        reserve: f64,
        capacity: f64,
        storage_capacity: f64,
        throughput: f64,
    ) -> FactoryGraph {
        let mut graph = FactoryGraph::default();
        graph
            .add_node(
                1,
                FactoryNode::miner(items::IRON_ORE, reserve, 2.0, capacity, 0.0),
            )
            .unwrap();
        graph
            .add_node(2, FactoryNode::storage(items::IRON_ORE, storage_capacity))
            .unwrap();
        graph.connect(1, 1, 2, throughput).unwrap();
        graph
    }

    #[test]
    fn steady_time_advance_is_lazy_and_does_not_recompute_rates() {
        let mut graph = miner_line(1e12, 1e12, 1e12, 1.0);
        let first = graph.advance_to(1.0).unwrap();
        assert_eq!(
            first,
            AdvanceReport {
                events: 0,
                rate_solves: 1
            }
        );
        let second = graph.advance_to(1e8).unwrap();
        assert_eq!(second, AdvanceReport::default());
        close(graph.nodes[&1].output.as_ref().unwrap().amount, 0.0);
        close(output(&graph, 1), 1e8);
        close(output(&graph, 2), 1e8);
        close(remaining(&graph, 1), 1e12 - 2e8);
    }

    #[test]
    fn throughput_backpressure_and_extraction_preserve_every_item() {
        let mut graph = miner_line(100.0, 4.0, 10.0, 0.5);
        graph.advance_to(2.0).unwrap();
        close(output(&graph, 1), 3.0);
        close(output(&graph, 2), 1.0);
        close(graph.rates().links[&1], 0.5);
        let report = graph.advance_to(1000.0).unwrap();
        assert!(report.events <= 3);
        close(output(&graph, 1), 4.0);
        close(output(&graph, 2), 10.0);
        close(remaining(&graph, 1), 86.0);
        close(graph.rates().machines[&1], 0.0);
        assert_eq!(graph.extract(2, 5.0).unwrap(), Some((items::IRON_ORE, 5.0)));
        graph.advance_to(1010.0).unwrap();
        close(output(&graph, 1), 4.0);
        close(output(&graph, 2), 10.0);
        close(remaining(&graph, 1), 81.0);
    }

    #[test]
    fn finite_deposit_exhausts_then_buffers_finish_draining() {
        let mut graph = miner_line(10.0, 64.0, 64.0, 0.5);
        let report = graph.advance_to(1e12).unwrap();
        assert!(report.events <= 3);
        close(remaining(&graph, 1), 0.0);
        close(output(&graph, 1), 0.0);
        close(output(&graph, 2), 10.0);
        assert!(graph.rates().links.values().all(|rate| *rate == 0.0));
    }

    #[test]
    fn very_long_offline_interval_stops_at_finite_capacity_with_few_events() {
        let mut graph = miner_line(1e9, 64.0, 128.0, 1.0);
        let report = graph.advance_to(1e12).unwrap();
        assert!(report.events <= 4, "{report:?}");
        close(
            output(&graph, 1) + output(&graph, 2) + remaining(&graph, 1),
            1e9,
        );
        close(output(&graph, 2), 128.0);
    }

    fn smelting_line() -> FactoryGraph {
        let mut graph = FactoryGraph::default();
        graph
            .add_node(
                1,
                FactoryNode::miner(items::IRON_ORE, 100.0, 0.25, 8.0, 1.0),
            )
            .unwrap();
        let registry = SmeltingRegistry::prototype();
        let recipe = FactoryRecipe::from_smelting(registry.find(items::IRON_ORE).unwrap(), 2.0);
        graph
            .add_node(2, FactoryNode::smelter(recipe, 8.0, 8.0))
            .unwrap();
        graph
            .add_node(3, FactoryNode::storage(items::IRON_INGOT, 64.0))
            .unwrap();
        graph.add_node(4, FactoryNode::generator(2.0)).unwrap();
        graph.connect(1, 1, 2, 2.0).unwrap();
        graph.connect(2, 2, 3, 2.0).unwrap();
        graph
    }

    #[test]
    fn shared_smelting_recipe_and_power_deficit_scale_both_machines() {
        let mut graph = smelting_line();
        let rates = graph.rates();
        close(rates.power_supply, 2.0);
        close(rates.power_demand, 3.0);
        close(rates.power_fraction, 2.0 / 3.0);
        close(rates.machines[&1], 1.0 / 6.0);
        close(rates.machines[&2], 1.0 / 15.0);
        graph.advance_to(30.0).unwrap();
        close(remaining(&graph, 1), 95.0);
        close(graph.node(2).unwrap().input.unwrap().amount, 3.0);
        close(output(&graph, 3), 2.0);
        close(output(&graph, 1), 0.0);
        close(output(&graph, 2), 0.0);
    }

    #[test]
    fn single_offline_advance_matches_arbitrarily_segmented_advances() {
        let mut once = smelting_line();
        let mut segmented = once.clone();
        once.advance_to(10_000.0).unwrap();
        for time in [
            0.1, 0.3, 1.0, 100.25, 123.0, 999.99, 1200.0, 2000.0, 9999.0, 10_000.0,
        ] {
            segmented.advance_to(time).unwrap();
        }
        for (id, node) in once.nodes() {
            let other = segmented.node(id).unwrap();
            for (a, b) in node
                .input
                .iter()
                .chain(node.output.iter())
                .zip(other.input.iter().chain(other.output.iter()))
            {
                close(a.amount, b.amount);
            }
            if matches!(node.kind, FactoryNodeKind::Miner { .. }) {
                close(remaining(&once, id), remaining(&segmented, id));
            }
        }
        close(
            remaining(&once, 1)
                + output(&once, 1)
                + once.node(2).unwrap().input.unwrap().amount
                + output(&once, 2)
                + output(&once, 3),
            100.0,
        );
    }

    #[test]
    fn power_off_and_recipe_toggle_take_effect_at_the_current_clock() {
        let mut graph = smelting_line();
        graph.advance_to(30.0).unwrap();
        graph.set_enabled(4, false).unwrap();
        graph.advance_to(300.0).unwrap();
        close(remaining(&graph, 1), 95.0);
        close(output(&graph, 3), 2.0);
        graph.set_enabled(4, true).unwrap();
        graph.set_enabled(1, false).unwrap();
        close(graph.rates().power_fraction, 1.0);
        graph.advance_to(330.0).unwrap();
        close(output(&graph, 3), 5.0);
        close(remaining(&graph, 1), 95.0);
    }

    #[test]
    fn starvation_stops_conversion_without_negative_buffers() {
        let mut graph = smelting_line();
        graph.remove_node(1).unwrap();
        graph.advance_to(1000.0).unwrap();
        close(output(&graph, 3), 0.0);
        assert_eq!(graph.insert(2, items::IRON_ORE, 3.0).unwrap(), 3.0);
        graph.advance_to(2000.0).unwrap();
        close(output(&graph, 3), 3.0);
        close(graph.node(2).unwrap().input.unwrap().amount, 0.0);
    }

    #[test]
    fn branches_share_a_finite_source_and_blocked_branch_does_not_stop_others() {
        let mut graph = miner_line(30.0, 4.0, 2.0, 1.0);
        graph
            .add_node(3, FactoryNode::storage(items::IRON_ORE, 64.0))
            .unwrap();
        graph.connect(2, 1, 3, 1.0).unwrap();
        graph.advance_to(1000.0).unwrap();
        close(output(&graph, 2), 2.0);
        close(output(&graph, 3), 28.0);
        close(remaining(&graph, 1), 0.0);
        close(output(&graph, 1), 0.0);
    }

    #[test]
    fn empty_cycles_have_no_phantom_flow_and_seeded_cycles_conserve_items() {
        let mut graph = FactoryGraph::default();
        for id in 1..=3 {
            graph
                .add_node(id, FactoryNode::storage(items::IRON_ORE, 8.0))
                .unwrap();
        }
        graph.connect(1, 1, 2, 2.0).unwrap();
        graph.connect(2, 2, 3, 1.0).unwrap();
        graph.connect(3, 3, 1, 3.0).unwrap();
        graph.advance_to(1000.0).unwrap();
        assert!(graph.rates().links.values().all(|rate| *rate == 0.0));
        graph.insert(1, items::IRON_ORE, 5.0).unwrap();
        let report = graph.advance_to(1e9).unwrap();
        assert!(report.events < 10, "{report:?}");
        close((1..=3).map(|id| output(&graph, id)).sum(), 5.0);
        for id in 1..=3 {
            assert!((0.0..=8.0).contains(&output(&graph, id)));
        }
    }

    #[test]
    fn full_intermediate_buffer_passes_through_at_the_slowest_link_rate() {
        let mut graph = miner_line(100.0, 4.0, 4.0, 2.0);
        graph.insert(2, items::IRON_ORE, 4.0).unwrap();
        graph
            .add_node(3, FactoryNode::storage(items::IRON_ORE, 64.0))
            .unwrap();
        graph.connect(2, 2, 3, 0.5).unwrap();
        graph.advance_to(10.0).unwrap();
        close(output(&graph, 2), 4.0);
        close(output(&graph, 3), 5.0);
        close(graph.rates().links[&1], 0.5);
        close(graph.rates().links[&2], 0.5);
        close(
            remaining(&graph, 1) + (1..=3).map(|id| output(&graph, id)).sum::<f64>(),
            104.0,
        );
    }

    #[test]
    fn serialization_materializes_lazy_buffers_and_restarts_the_same_future() {
        let mut graph = smelting_line();
        graph.advance_to(37.25).unwrap();
        let json = serde_json::to_string(&graph).unwrap();
        let mut restored: FactoryGraph = serde_json::from_str(&json).unwrap();
        close(restored.time(), 37.25);
        close(output(&restored, 3), output(&graph, 3));
        graph.advance_to(1000.0).unwrap();
        restored.advance_to(1000.0).unwrap();
        close(output(&restored, 3), output(&graph, 3));
        close(remaining(&restored, 1), remaining(&graph, 1));
        close(
            restored.node(2).unwrap().input.unwrap().amount,
            graph.node(2).unwrap().input.unwrap().amount,
        );
    }

    #[test]
    fn mutation_removes_links_without_losing_projected_contents() {
        let mut graph = miner_line(100.0, 64.0, 64.0, 1.0);
        graph.advance_to(2.5).unwrap();
        let removed = graph.remove_node(1).unwrap();
        close(removed.output.unwrap().amount, 2.5);
        assert!(graph.links().is_empty());
        graph.advance_to(1e9).unwrap();
        close(output(&graph, 2), 2.5);
    }

    #[test]
    fn external_mining_reconciles_only_the_remaining_deposit() {
        let mut graph = miner_line(10.0, 64.0, 64.0, 1.0);
        graph.advance_to(2.0).unwrap();
        graph.set_miner_remaining(1, 1.0).unwrap();
        graph.advance_to(100.0).unwrap();
        close(output(&graph, 2), 5.0);
        close(remaining(&graph, 1), 0.0);
    }

    #[test]
    fn invalid_numeric_inputs_item_mismatches_and_time_reversal_are_rejected() {
        let mut graph = miner_line(10.0, 4.0, 4.0, 1.0);
        assert_eq!(graph.advance_to(f64::NAN), Err(FactoryError::InvalidTime));
        assert_eq!(
            graph.advance_to(f64::INFINITY),
            Err(FactoryError::InvalidTime)
        );
        graph.advance_to(1.0).unwrap();
        assert_eq!(graph.advance_to(0.5), Err(FactoryError::InvalidTime));
        assert_eq!(
            graph.add_node(3, FactoryNode::generator(f64::INFINITY)),
            Err(FactoryError::InvalidNode)
        );
        assert_eq!(
            graph.insert(2, items::COAL, 1.0),
            Err(FactoryError::WrongItem)
        );
        assert_eq!(graph.extract(2, -1.0), Err(FactoryError::InvalidAmount));
        assert_eq!(graph.connect(2, 2, 2, 1.0), Err(FactoryError::InvalidLink));
        graph
            .add_node(3, FactoryNode::storage(items::IRON_INGOT, 4.0))
            .unwrap();
        assert_eq!(graph.connect(2, 2, 3, 1.0), Err(FactoryError::InvalidLink));
    }

    #[test]
    fn corrupt_serialized_buffers_and_dangling_links_are_rejected() {
        let graph = miner_line(10.0, 4.0, 4.0, 1.0);
        let mut value = serde_json::to_value(&graph).unwrap();
        value["nodes"]["2"]["output"]["amount"] = serde_json::json!(5.0);
        assert!(serde_json::from_value::<FactoryGraph>(value).is_err());
        let mut value = serde_json::to_value(&graph).unwrap();
        value["links"]["1"]["to"] = serde_json::json!(999);
        assert!(serde_json::from_value::<FactoryGraph>(value).is_err());
    }

    #[test]
    fn multi_item_recipe_preserves_stoichiometry_across_starvation() {
        let mut graph = FactoryGraph::default();
        graph
            .add_node(
                1,
                FactoryNode::smelter(
                    FactoryRecipe {
                        input: items::IRON_ORE,
                        output: items::IRON_INGOT,
                        input_per_cycle: 2.0,
                        output_per_cycle: 3.0,
                        cycles_per_second: 0.25,
                        power: 0.0,
                    },
                    64.0,
                    64.0,
                ),
            )
            .unwrap();
        graph.insert(1, items::IRON_ORE, 7.0).unwrap();
        graph.advance_to(1.0).unwrap();
        close(output(&graph, 1), 0.75);
        close(graph.node(1).unwrap().input.unwrap().amount, 6.5);
        graph.advance_to(1e9).unwrap();
        close(output(&graph, 1), 10.5);
        close(graph.node(1).unwrap().input.unwrap().amount, 0.0);
    }

    #[test]
    fn randomized_branched_cycles_remain_bounded_conservative_and_segment_independent() {
        let mut seed = 139_u64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 32) as u32
        };
        for _case in 0..40 {
            let mut graph = FactoryGraph::default();
            graph
                .add_node(0, FactoryNode::miner(items::IRON_ORE, 25.0, 0.4, 4.0, 0.0))
                .unwrap();
            let mut initial = 25.0;
            for id in 1..=7 {
                let capacity = f64::from(next() % 7 + 1);
                graph
                    .add_node(id, FactoryNode::storage(items::IRON_ORE, capacity))
                    .unwrap();
                let amount = graph
                    .insert(id, items::IRON_ORE, f64::from(next() % 4))
                    .unwrap();
                initial += amount;
            }
            for id in 0..20 {
                let from = u64::from(next() % 8);
                let to = u64::from(next() % 7 + 1);
                if from != to {
                    graph
                        .connect(id, from, to, f64::from(next() % 5 + 1) / 10.0)
                        .unwrap();
                }
            }
            let mut segmented = graph.clone();
            let report = graph.advance_to(1e6).unwrap();
            assert!(report.events < 200, "unexpected event churn: {report:?}");
            for time in [1.1, 3.7, 12.5, 32.0, 100.0, 511.0, 1e6] {
                segmented.advance_to(time).unwrap();
            }
            close(
                remaining(&graph, 0) + (0..=7).map(|id| output(&graph, id)).sum::<f64>(),
                initial,
            );
            for id in 0..=7 {
                close(output(&graph, id), output(&segmented, id));
                let buffer = graph.node(id).unwrap().output.unwrap();
                assert!((0.0..=buffer.capacity).contains(&buffer.amount));
            }
        }
    }
}

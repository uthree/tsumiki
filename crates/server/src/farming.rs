//! Server-authoritative planting and active-time crop growth (M8).

use std::collections::{HashMap, VecDeque};

use bevy_math::{IVec3, UVec3};
use tsumiki_world::{
    BlockId, BlockRegistry, ItemRegistry, ItemStack, WORLD_HEIGHT_BLOCKS, WorldGenerator, blocks,
    items, split_block_pos, tool::ToolKind,
};

use crate::ChunkCache;

pub const GROWTH_SECONDS: f32 = 120.0;

#[derive(Default)]
pub struct Crops {
    pub elapsed: HashMap<IVec3, f32>,
    check_accum: f32,
}

impl Crops {
    pub fn from_records(records: Vec<(IVec3, f32)>) -> Self {
        Self {
            elapsed: records
                .into_iter()
                .filter_map(|(pos, elapsed)| {
                    (pos.y > 0 && pos.y < WORLD_HEIGHT_BLOCKS && elapsed.is_finite())
                        .then_some((pos, elapsed.clamp(0.0, GROWTH_SECONDS)))
                })
                .collect(),
            check_accum: 0.0,
        }
    }

    pub fn records(&self) -> Vec<(IVec3, f32)> {
        self.elapsed
            .iter()
            .map(|(&pos, &elapsed)| (pos, elapsed))
            .collect()
    }

    pub fn planted(&mut self, pos: IVec3) {
        self.elapsed.insert(pos, 0.0);
    }
}

pub fn is_crop(block: BlockId) -> bool {
    block == blocks::WHEAT_YOUNG || block == blocks::WHEAT_MATURE
}

pub fn held_shovel(stack: ItemStack, registry: &ItemRegistry) -> bool {
    registry
        .tool(stack.item)
        .is_some_and(|tool| tool.kind == ToolKind::Shovel)
}

/// Crop yields replace the catalog's single-drop entry. Grass gives a seed
/// alongside its ordinary dirt, making the first farm reachable without loot.
pub fn extra_drops(block: BlockId) -> Vec<ItemStack> {
    if block == blocks::GRASS {
        vec![ItemStack::one(items::WHEAT_SEEDS)]
    } else if block == blocks::WHEAT_MATURE {
        vec![ItemStack::new(items::WHEAT_SEEDS, 2)]
    } else {
        Vec::new()
    }
}

pub fn block_at(
    cache: &mut ChunkCache,
    world_gen: &WorldGenerator,
    tick: u64,
    pos: IVec3,
) -> BlockId {
    if pos.y < 0 || pos.y >= WORLD_HEIGHT_BLOCKS {
        return BlockId::AIR;
    }
    let (chunk_pos, local) = split_block_pos(pos);
    let chunk = cache
        .chunks
        .entry(chunk_pos)
        .or_insert_with(|| world_gen.generate_chunk(chunk_pos));
    cache.last_access.insert(chunk_pos, tick);
    chunk.get(UVec3::new(local.x as u32, local.y as u32, local.z as u32))
}

fn hydrated(cache: &mut ChunkCache, world_gen: &WorldGenerator, tick: u64, crop: IVec3) -> bool {
    (-4..=4).any(|dx| {
        (-4..=4).any(|dz| {
            block_at(cache, world_gen, tick, crop + IVec3::new(dx, -1, dz)) == blocks::WATER
        })
    })
}

/// Direct sky is sufficient at any time of day, matching stored skylight.
/// Enclosed farms also grow where a nearby lamp or torch supplies level 9.
fn illuminated(
    cache: &mut ChunkCache,
    world_gen: &WorldGenerator,
    registry: &BlockRegistry,
    tick: u64,
    crop: IVec3,
) -> bool {
    if skylight(cache, world_gen, registry, tick, crop) >= 9 {
        return true;
    }
    let mut visited = HashMap::from([(crop, 0_u8)]);
    let mut queue = VecDeque::from([(crop, 0_u8)]);
    while let Some((pos, attenuation)) = queue.pop_front() {
        for offset in [
            IVec3::X,
            IVec3::NEG_X,
            IVec3::Y,
            IVec3::NEG_Y,
            IVec3::Z,
            IVec3::NEG_Z,
        ] {
            let next = pos + offset;
            let def = registry.get(block_at(cache, world_gen, tick, next));
            if def.light_emission.iter().copied().max().unwrap_or(0) >= 10 + attenuation {
                return true;
            }
            let next_attenuation = attenuation + def.light_opacity.max(1);
            if next_attenuation > 6
                || visited
                    .get(&next)
                    .is_some_and(|&old| old <= next_attenuation)
            {
                continue;
            }
            visited.insert(next, next_attenuation);
            if skylight(cache, world_gen, registry, tick, next) >= 9 + next_attenuation {
                return true;
            }
            queue.push_back((next, next_attenuation));
        }
    }
    false
}

fn skylight(
    cache: &mut ChunkCache,
    world_gen: &WorldGenerator,
    registry: &BlockRegistry,
    tick: u64,
    pos: IVec3,
) -> u8 {
    let mut light = 15_u8;
    for y in pos.y..WORLD_HEIGHT_BLOCKS {
        let opacity = registry
            .get(block_at(
                cache,
                world_gen,
                tick,
                IVec3::new(pos.x, y, pos.z),
            ))
            .light_opacity;
        light = light.saturating_sub(opacity);
        if light < 9 {
            break;
        }
    }
    light
}

/// Checks crops once a simulated second; returns mature block edits for the
/// caller's normal chunk/lighting/LOD replication path. Offline time is absent.
pub fn tick(
    crops: &mut Crops,
    cache: &mut ChunkCache,
    world_gen: &WorldGenerator,
    registry: &BlockRegistry,
    tick: u64,
    dt: f32,
) -> Vec<IVec3> {
    crops.check_accum += dt;
    if crops.check_accum < 1.0 {
        return Vec::new();
    }
    let elapsed = std::mem::take(&mut crops.check_accum);
    let mut mature = Vec::new();
    crops.elapsed.retain(|&pos, age| {
        if block_at(cache, world_gen, tick, pos) != blocks::WHEAT_YOUNG {
            return false;
        }
        if block_at(cache, world_gen, tick, pos - IVec3::Y) == blocks::FARMLAND
            && hydrated(cache, world_gen, tick, pos)
            && illuminated(cache, world_gen, registry, tick, pos)
        {
            *age += elapsed;
            if *age >= GROWTH_SECONDS {
                mature.push(pos);
                return false;
            }
        }
        true
    });
    mature
}

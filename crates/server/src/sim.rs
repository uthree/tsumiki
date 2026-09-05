//! Survival-mode simulation: dropped items, health regen, and the day/night
//! clock (doc/roadmap.md M4). M5 upgrades dropped items to carry a real
//! [`ItemStack`] instead of a raw block + count, and adds
//! [`drop_ui_leftovers`] for the "items can never be parked in a closed UI"
//! rule shared by `CloseContainer`, death, and disconnect.
//!
//! Kept as a separate module from `lib.rs` (which is already large) because
//! this is pure world-state mutation plus outgoing broadcasts, called from
//! `tick_server`. It reaches into `lib.rs`'s private `ClientState` and
//! `ChunkCache` types directly -- sound because a private item is visible to
//! every descendant module of the one that defines it, and this module is a
//! child of the crate root.

use std::collections::HashMap;

use bevy_math::{IVec3, UVec3, Vec3};

use tsumiki_protocol::{ClientId, ServerToClient, ServerTransport};
use tsumiki_world::{
    BlockRegistry, ItemRegistry, ItemStack, WORLD_HEIGHT_BLOCKS, WorldGenerator, split_block_pos,
};

use crate::{ChunkCache, ClientState};

/// A dropped item is eligible for pickup only this long after it (last)
/// spawned/merged, so an item that appears right under a player's feet
/// (e.g. a block broken while standing on it) doesn't vanish instantly.
pub const ITEM_PICKUP_DELAY_SECS: f32 = 0.5;
/// Distance within which an alive survival player picks up a dropped item.
pub const ITEM_PICKUP_RADIUS: f32 = 1.5;
/// Distance within which two same-item dropped stacks merge into one.
pub const ITEM_MERGE_RADIUS: f32 = 1.0;
/// Dropped items older than this despawn unpicked.
pub const ITEM_EXPIRY_SECS: f32 = 300.0;

/// One in-game day, in real seconds (doc/roadmap.md M4, "day/night cycle").
pub const DAY_LENGTH_SECS: f32 = 600.0;
/// How often the server resyncs every client's time of day.
pub const TIME_BROADCAST_INTERVAL_SECS: f32 = 5.0;

/// A dropped item entity, server-authoritative and static once spawned (it
/// never moves after coming to rest, so there is no movement sync).
#[derive(Clone, Copy, Debug)]
pub struct DroppedItem {
    pub pos: Vec3,
    pub stack: ItemStack,
    /// [`GameClock`] value at spawn (or last merge); drives pickup delay and
    /// expiry.
    pub spawned_at: f32,
}

/// Live dropped-item state. Not persisted directly -- see
/// `persist::ItemRecord`, which drops the id and timestamp: ids are
/// re-assigned and ages reset on load, since neither affects correctness
/// (a freshly-loaded item just gets a fresh pickup-delay and expiry window).
///
/// Plain struct rather than a `Resource` itself -- it lives as a field of
/// `lib.rs`'s `SimRes` wrapper, which keeps `tick_server`'s Bevy system
/// parameter count under the tuple-impl limit (16).
#[derive(Default)]
pub struct ItemsRes {
    pub items: HashMap<u64, DroppedItem>,
    next_id: u64,
}

impl ItemsRes {
    /// Allocates an id and inserts a freshly-spawned item, for items
    /// restored from persistence (which carries no id or age -- see
    /// `persist::ItemRecord`).
    pub fn insert_loaded(&mut self, pos: Vec3, stack: ItemStack, clock: f32) {
        let id = self.alloc_id();
        self.items.insert(
            id,
            DroppedItem {
                pos,
                stack,
                spawned_at: clock,
            },
        );
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

/// A monotonic game-time clock, advanced by the server's fixed tick interval
/// rather than wall-clock time (see `TickInterval` in `lib.rs`), so tests can
/// simulate arbitrarily large elapsed time deterministically in a single
/// tick. Used only for dropped items' pickup-delay/expiry timestamps. Also a
/// plain struct for the same reason as [`ItemsRes`].
#[derive(Default, Clone, Copy)]
pub struct GameClock(pub f32);

/// The world's day/night clock (doc/roadmap.md M4). Also a plain struct for
/// the same reason as [`ItemsRes`].
pub struct WorldTimeRes {
    /// `[0, 1)` fraction of a day; 0.0 = sunrise (see
    /// `protocol::ServerToClient::Welcome` docs).
    pub time_of_day: f32,
    /// Seconds accumulated since the last `TimeUpdate` broadcast.
    broadcast_accum: f32,
}

impl WorldTimeRes {
    pub fn new(time_of_day: f32) -> Self {
        Self {
            time_of_day: time_of_day.rem_euclid(1.0),
            broadcast_accum: 0.0,
        }
    }
}

/// Scans straight down from `spawn_pos` through loaded/generated chunks
/// (generating any missing chunk in the column via the normal cache path,
/// same as `RequestChunks`/edits do) to the first solid block, and returns a
/// resting position 0.5 blocks above its top. Falls back to resting on the
/// world floor if the column has no solid block at all (shouldn't happen
/// with the prototype worldgen, but keeps this total).
fn resolve_rest_position(
    cache: &mut ChunkCache,
    world_gen: &WorldGenerator,
    registry: &BlockRegistry,
    tick: u64,
    spawn_pos: Vec3,
) -> Vec3 {
    let mut y = (spawn_pos.y.floor() as i32).clamp(0, WORLD_HEIGHT_BLOCKS - 1);
    loop {
        let block_pos = IVec3::new(spawn_pos.x.floor() as i32, y, spawn_pos.z.floor() as i32);
        let (chunk_pos, local) = split_block_pos(block_pos);
        let local = UVec3::new(local.x as u32, local.y as u32, local.z as u32);
        let chunk = cache
            .chunks
            .entry(chunk_pos)
            .or_insert_with(|| world_gen.generate_chunk(chunk_pos));
        cache.last_access.insert(chunk_pos, tick);
        if registry.get(chunk.get(local)).solid {
            // The block at `y` occupies world-height [y, y+1); rest 0.5
            // above its top surface (y + 1).
            return Vec3::new(spawn_pos.x, y as f32 + 1.5, spawn_pos.z);
        }
        if y == 0 {
            return Vec3::new(spawn_pos.x, 0.5, spawn_pos.z);
        }
        y -= 1;
    }
}

/// Spawns (or merges into) a dropped `stack`, resting it on the ground below
/// `spawn_pos`, and broadcasts the result to `recipients` (every known
/// client -- interest management for items is deferred, same as the roadmap
/// leaves finer replication for later).
///
/// Merge policy (within [`ITEM_MERGE_RADIUS`] of the same item and wear): despawn the
/// old entity and spawn a fresh one with the combined count, rather than
/// mutating the existing id's count in place. This keeps the spawn/despawn
/// id lifecycle strictly 1:1, simpler for clients to reason about than an id
/// whose count can silently change underneath them.
#[allow(clippy::too_many_arguments)]
pub fn spawn_item<T: ServerTransport>(
    transport: &mut T,
    recipients: &[ClientId],
    items: &mut ItemsRes,
    cache: &mut ChunkCache,
    world_gen: &WorldGenerator,
    registry: &BlockRegistry,
    tick: u64,
    clock: f32,
    spawn_pos: Vec3,
    stack: ItemStack,
) {
    if stack.count == 0 {
        return;
    }
    let rest_pos = resolve_rest_position(cache, world_gen, registry, tick, spawn_pos);

    let merge_target = items
        .items
        .iter()
        .find(|(_, it)| {
            it.stack.mergeable_with(stack)
                && it.stack.count.checked_add(stack.count).is_some()
                && it.pos.distance(rest_pos) <= ITEM_MERGE_RADIUS
        })
        .map(|(&id, it)| (id, it.stack.count));

    let total_count = if let Some((merge_id, existing_count)) = merge_target {
        items.items.remove(&merge_id);
        for &cid in recipients {
            transport.send(cid, ServerToClient::ItemDespawned { id: merge_id });
        }
        existing_count + stack.count
    } else {
        stack.count
    };

    let merged_stack = ItemStack {
        count: total_count,
        ..stack
    };
    let id = items.alloc_id();
    items.items.insert(
        id,
        DroppedItem {
            pos: rest_pos,
            stack: merged_stack,
            spawned_at: clock,
        },
    );
    for &cid in recipients {
        transport.send(
            cid,
            ServerToClient::ItemSpawned {
                id,
                pos: rest_pos,
                stack: merged_stack,
            },
        );
    }
}

/// Drops `client`'s cursor stack into the world near its last known position
/// (falling back to the origin if none is known yet), clearing it -- items
/// can never be parked in a closed UI (roadmap M5). Shared by
/// `CloseContainer`, death, and disconnect.
///
/// Crafting no longer has a per-player grid to drain (recipes are crafted by
/// id straight out of the main inventory), so the cursor is the only thing
/// that can be left holding items when a UI closes.
#[allow(clippy::too_many_arguments)]
pub fn drop_ui_leftovers<T: ServerTransport>(
    transport: &mut T,
    recipients: &[ClientId],
    items: &mut ItemsRes,
    cache: &mut ChunkCache,
    world_gen: &WorldGenerator,
    block_reg: &BlockRegistry,
    tick: u64,
    clock: f32,
    client: &mut ClientState,
) {
    let drop_pos = client.save.map(|s| s.pos).unwrap_or(Vec3::ZERO);
    if let Some(cursor) = client.cursor.take() {
        spawn_item(
            transport, recipients, items, cache, world_gen, block_reg, tick, clock, drop_pos,
            cursor,
        );
    }
}

/// Expires items older than [`ITEM_EXPIRY_SECS`] (any mode) and, when
/// `survival` is set, lets alive players with a known position pick up
/// nearby, non-fresh items into their inventory (creative has no inventory
/// concept exposed to the client, so pickup is skipped there -- only expiry
/// runs, in case a migrated/mode-switched world has leftover items).
///
/// Pickup fills available inventory space. A partial pickup replaces the
/// old entity with a new id carrying the remainder, preserving its resting
/// position and original expiry time. Spawn/despawn messages keep observers
/// synchronized. An entirely full inventory leaves the item unchanged.
///
/// Returns the client ids whose inventory changed, so the caller can send
/// `InventoryUpdate` and mark persistence dirty for each.
pub fn tick_items<T: ServerTransport>(
    transport: &mut T,
    clients: &mut HashMap<ClientId, ClientState>,
    items: &mut ItemsRes,
    item_reg: &ItemRegistry,
    clock: f32,
    survival: bool,
) -> Vec<ClientId> {
    let client_ids: Vec<ClientId> = clients.keys().copied().collect();

    let expired: Vec<u64> = items
        .items
        .iter()
        .filter(|(_, it)| clock - it.spawned_at >= ITEM_EXPIRY_SECS)
        .map(|(&id, _)| id)
        .collect();
    for id in &expired {
        items.items.remove(id);
    }
    for &id in &expired {
        for &cid in &client_ids {
            transport.send(cid, ServerToClient::ItemDespawned { id });
        }
    }

    if !survival {
        return Vec::new();
    }

    let mut changed = Vec::new();
    for &client_id in &client_ids {
        let Some(client) = clients.get_mut(&client_id) else {
            continue;
        };
        if client.hp == 0 {
            continue;
        }
        let Some(save) = client.save else {
            continue;
        };

        let nearby: Vec<u64> = items
            .items
            .iter()
            .filter(|(_, it)| {
                clock - it.spawned_at >= ITEM_PICKUP_DELAY_SECS
                    && it.pos.distance(save.pos) <= ITEM_PICKUP_RADIUS
            })
            .map(|(&id, _)| id)
            .collect();
        if nearby.is_empty() {
            continue;
        }

        let mut inventory_changed = false;
        for id in nearby {
            let dropped = items.items[&id];
            let remainder = client.main.insert(dropped.stack, item_reg);
            if remainder.is_some_and(|stack| stack.count == dropped.stack.count) {
                continue;
            }
            inventory_changed = true;
            items.items.remove(&id);
            for &cid in &client_ids {
                transport.send(cid, ServerToClient::ItemDespawned { id });
            }
            if let Some(stack) = remainder {
                let new_id = items.alloc_id();
                items.items.insert(new_id, DroppedItem { stack, ..dropped });
                for &cid in &client_ids {
                    transport.send(
                        cid,
                        ServerToClient::ItemSpawned {
                            id: new_id,
                            pos: dropped.pos,
                            stack,
                        },
                    );
                }
            }
        }
        if inventory_changed {
            changed.push(client_id);
        }
    }

    changed
}

/// Advances the day/night clock by `dt` and, every
/// [`TIME_BROADCAST_INTERVAL_SECS`], broadcasts a `TimeUpdate` to every known
/// client.
pub fn tick_world_time<T: ServerTransport>(
    transport: &mut T,
    clients: &HashMap<ClientId, ClientState>,
    world_time: &mut WorldTimeRes,
    dt: f32,
) {
    world_time.time_of_day = (world_time.time_of_day + dt / DAY_LENGTH_SECS).rem_euclid(1.0);
    world_time.broadcast_accum += dt;
    while world_time.broadcast_accum >= TIME_BROADCAST_INTERVAL_SECS {
        world_time.broadcast_accum -= TIME_BROADCAST_INTERVAL_SECS;
        for &cid in clients.keys() {
            transport.send(
                cid,
                ServerToClient::TimeUpdate {
                    time_of_day: world_time.time_of_day,
                },
            );
        }
    }
}

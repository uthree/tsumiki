//! World persistence: chunks + player state saved to disk (doc/roadmap.md
//! M1, "World persistence"; M2 upgrades the player slot to per-name; M4 adds
//! game mode, health, inventory, dropped items, and time of day; M5 replaces
//! the block-count inventory with real items, and adds chest containers).
//!
//! Format contract:
//! - `<world_dir>/meta.bin`: postcard-serialized world metadata (format
//!   version, world seed, and everything else that isn't a chunk). Format v1
//!   (M1) had a single global player slot; v2 (M2) keyed player saves by
//!   name; v3 (M4) added `game_mode`, `world_time_of_day`, per-player health
//!   + inventory, and dropped items; v4 (M5) replaces the block-count
//!     inventory with a real slotted `ItemStack` inventory and adds persisted
//!     chest containers. See [`decode_meta`] for how versions are told apart
//!     and migrated.
//! - `<world_dir>/regions/r.<rx>.<rz>.bin`: postcard-serialized
//!   `Vec<(IVec3, Chunk)>` holding every chunk (at any Y level) that has ever
//!   been modified and whose region is `(x.div_euclid(REGION_SIZE),
//!   z.div_euclid(REGION_SIZE))`. Unmodified chunks regenerate deterministically
//!   from the seed and are deliberately never written.
//!
//! Writes are atomic enough for a game save: each file is written to a
//! sibling `.tmp` path first, then renamed into place, so a crash mid-write
//! never leaves a half-written file at the real path.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use bevy::prelude::Resource;
use bevy_math::{IVec3, Vec3};
use serde::{Deserialize, Serialize};

use tsumiki_protocol::{GameMode, MAX_HP, PlayerSave};
use tsumiki_world::{
    BlockId, Chunk, Inventory, ItemId, ItemRegistry, ItemStack, MAIN_INVENTORY_SIZE,
};

/// Chunk columns per region edge; a region file covers all Y levels of an
/// 8x8 chunk-column footprint.
pub const REGION_SIZE: i32 = 8;

const META_FORMAT_VERSION: u32 = 4;

/// Key a migrated v1 (single global slot) player save is filed under in the
/// per-name map.
const LEGACY_PLAYER_NAME: &str = "player";

/// Per-player persisted state beyond position: health and the main inventory
/// (M5 replaces M4's flat block-count list with real slotted item stacks).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlayerRecord {
    pub save: PlayerSave,
    pub hp: u16,
    /// [`tsumiki_world::MAIN_INVENTORY_SIZE`] entries; `0..9` is the hotbar.
    /// The crafting grid and cursor are deliberately not persisted here --
    /// they are always emptied into the world on `CloseContainer`,
    /// disconnect, or death (roadmap M5), so a live session never has
    /// anything in them worth saving.
    pub main: Vec<Option<ItemStack>>,
}

/// A dropped item entity as persisted (M5: carries a real [`ItemStack`]
/// rather than M4's raw block + count). Unlike the live server-side
/// representation, this carries no id or spawn timestamp: ids are
/// re-assigned and age resets on load, since neither matters for
/// correctness (a freshly-loaded item just gets a fresh pickup-delay and
/// expiry window).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ItemRecord {
    pub pos: Vec3,
    pub stack: ItemStack,
}

/// Just enough of `meta.bin`'s layout to read the leading `version` field
/// without committing to a full struct shape. Used by [`decode_meta`] to
/// dispatch to the right versioned struct.
#[derive(Deserialize)]
struct VersionHeader {
    version: u32,
}

/// On-disk contents of `meta.bin`, format v1 (M1): a single global player
/// slot, no game mode (the game was de-facto creative). Kept only so
/// [`decode_meta`] can migrate old saves.
#[derive(Serialize, Deserialize)]
struct WorldMetaV1 {
    version: u32,
    seed: u64,
    player: Option<PlayerSave>,
}

/// On-disk contents of `meta.bin`, format v2 (M2): player saves keyed by
/// name, still no game mode. Kept only so [`decode_meta`] can migrate old
/// saves.
#[derive(Serialize, Deserialize)]
struct WorldMetaV2 {
    version: u32,
    seed: u64,
    players: HashMap<String, PlayerSave>,
}

/// `PlayerRecord`'s shape at format v3 (M4): an inventory was a flat list of
/// `(BlockId, count)` pairs, since items didn't exist yet -- holding a block
/// meant holding the block itself. Kept only so [`decode_meta`] can migrate
/// v3 saves (see [`migrate_v3_main_inventory`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PlayerRecordV3 {
    save: PlayerSave,
    hp: u16,
    inventory: Vec<(BlockId, u32)>,
}

/// A dropped item entity's shape at format v3: a raw block + count, before
/// the item/block split existed.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ItemRecordV3 {
    pos: Vec3,
    block: BlockId,
    count: u32,
}

/// On-disk contents of `meta.bin`, format v3 (M4): adds game mode, time of
/// day, per-player health/inventory, and dropped items. Kept only so
/// [`decode_meta`] can migrate v3 saves.
#[derive(Serialize, Deserialize)]
struct WorldMetaV3 {
    version: u32,
    seed: u64,
    game_mode: GameMode,
    world_time_of_day: f32,
    players: HashMap<String, PlayerRecordV3>,
    items: Vec<ItemRecordV3>,
}

/// On-disk contents of `meta.bin`, format v4 (M5): the block-count inventory
/// becomes a real slotted `ItemStack` inventory, and chest containers are
/// persisted alongside players and dropped items.
#[derive(Serialize, Deserialize)]
struct WorldMetaV4 {
    version: u32,
    seed: u64,
    game_mode: GameMode,
    world_time_of_day: f32,
    players: HashMap<String, PlayerRecord>,
    items: Vec<ItemRecord>,
    /// Chest contents, keyed by block position. Crafting tables hold no
    /// items and are therefore never listed here.
    containers: Vec<(IVec3, Vec<Option<ItemStack>>)>,
}

/// `(seed, game_mode, world_time_of_day, players, items, containers)`, as
/// decoded by [`decode_meta`] regardless of which on-disk format version it
/// read.
type DecodedMeta = (
    u64,
    GameMode,
    f32,
    HashMap<String, PlayerRecord>,
    Vec<ItemRecord>,
    Vec<(IVec3, Vec<Option<ItemStack>>)>,
);

/// Finds the item that places `block`, by searching
/// [`ItemRegistry::placeable`] for a match (M5's block/item split postdates
/// v3, when an inventory or dropped item literally held a block). `None`
/// means no current item places that block -- shouldn't happen for the M4
/// prototype catalog, since every v3 block count originated from placing an
/// item that (by construction) placed it, but a hand-authored or
/// hand-edited save could still hit it.
fn item_for_block(block: BlockId, item_reg: &ItemRegistry) -> Option<ItemId> {
    item_reg
        .placeable()
        .find(|&id| item_reg.places(id) == Some(block))
}

/// Migrates a v3 flat block-count inventory into a v4 slotted item
/// inventory. A block with no corresponding item (see [`item_for_block`])
/// is dropped with a note; so is anything that doesn't fit in
/// [`MAIN_INVENTORY_SIZE`] slots once split into stacks (a v3 count could
/// exceed the M5 stack cap, e.g. from M4's much larger per-block cap).
fn migrate_v3_main_inventory(
    old: Vec<(BlockId, u32)>,
    item_reg: &ItemRegistry,
) -> Vec<Option<ItemStack>> {
    let mut inv = Inventory::new(MAIN_INVENTORY_SIZE);
    for (block, count) in old {
        if count == 0 {
            continue;
        }
        let Some(item) = item_for_block(block, item_reg) else {
            eprintln!(
                "tsumiki-server: v3->v4 migration: block {block:?} has no placeable item; \
                 dropping {count} from a player's inventory"
            );
            continue;
        };
        if let Some(leftover) = inv.insert(ItemStack::new(item, count), item_reg) {
            eprintln!(
                "tsumiki-server: v3->v4 migration: inventory full; dropping {} of item {:?}",
                leftover.count, leftover.item
            );
        }
    }
    inv.to_vec()
}

/// Migrates v3 dropped-item records (raw block + count) to v4 (`ItemStack`).
/// An item is uncapped on the ground (unlike inventory slots), so no
/// splitting is needed -- only the block/item lookup can fail.
fn migrate_v3_items(old: Vec<ItemRecordV3>, item_reg: &ItemRegistry) -> Vec<ItemRecord> {
    old.into_iter()
        .filter_map(|rec| {
            let Some(item) = item_for_block(rec.block, item_reg) else {
                eprintln!(
                    "tsumiki-server: v3->v4 migration: dropped-item block {:?} has no \
                     placeable item; removing {} from the ground",
                    rec.block, rec.count
                );
                return None;
            };
            Some(ItemRecord {
                pos: rec.pos,
                stack: ItemStack::new(item, rec.count),
            })
        })
        .collect()
}

/// Decodes `meta.bin`, migrating v1/v2/v3 files transparently.
///
/// postcard has no self-describing format, so there is no generic "peek the
/// tag" operation. Instead we decode only the leading `version` field via
/// [`postcard::take_from_bytes`] (which, unlike `from_bytes`, tolerates and
/// returns the unconsumed remainder rather than erroring on it), then decode
/// the *whole* buffer again from the start using whichever full struct that
/// version corresponds to. This is sound because postcard serializes struct
/// fields in declaration order and every version declares `version` first,
/// so the header decode and the full decode agree on where `version` lives.
///
/// v1 and v2 predate game modes entirely -- worlds saved under them had free
/// building and no health, i.e. they were de-facto creative. Migrating them
/// to [`GameMode::Creative`] (rather than defaulting to Survival, which would
/// suddenly demand mining and introduce death for existing worlds) preserves
/// that behavior. Migrated players get full health and an empty inventory,
/// neither of which is meaningful in creative mode. v3 predates the
/// item/block split (roadmap M5); see [`migrate_v3_main_inventory`] and
/// [`migrate_v3_items`] for how its block counts become item stacks.
fn decode_meta(bytes: &[u8]) -> io::Result<DecodedMeta> {
    let (header, _) = postcard::take_from_bytes::<VersionHeader>(bytes).map_err(postcard_err)?;
    match header.version {
        4 => {
            let meta: WorldMetaV4 = postcard::from_bytes(bytes).map_err(postcard_err)?;
            Ok((
                meta.seed,
                meta.game_mode,
                meta.world_time_of_day,
                meta.players,
                meta.items,
                meta.containers,
            ))
        }
        3 => {
            let meta: WorldMetaV3 = postcard::from_bytes(bytes).map_err(postcard_err)?;
            eprintln!(
                "tsumiki-server: migrating world meta v3 (predates the item/block split) to v4"
            );
            let item_reg = ItemRegistry::prototype();
            let players = meta
                .players
                .into_iter()
                .map(|(name, rec)| {
                    (
                        name,
                        PlayerRecord {
                            save: rec.save,
                            hp: rec.hp,
                            main: migrate_v3_main_inventory(rec.inventory, &item_reg),
                        },
                    )
                })
                .collect();
            let items = migrate_v3_items(meta.items, &item_reg);
            Ok((
                meta.seed,
                meta.game_mode,
                meta.world_time_of_day,
                players,
                items,
                Vec::new(),
            ))
        }
        2 => {
            let meta: WorldMetaV2 = postcard::from_bytes(bytes).map_err(postcard_err)?;
            eprintln!(
                "tsumiki-server: migrating world meta v2 (predates game modes) to v4; \
                 world becomes Creative, players get full health and empty inventory"
            );
            let players = meta
                .players
                .into_iter()
                .map(|(name, save)| {
                    (
                        name,
                        PlayerRecord {
                            save,
                            hp: MAX_HP,
                            main: Vec::new(),
                        },
                    )
                })
                .collect();
            Ok((
                meta.seed,
                GameMode::Creative,
                0.0,
                players,
                Vec::new(),
                Vec::new(),
            ))
        }
        1 => {
            let meta: WorldMetaV1 = postcard::from_bytes(bytes).map_err(postcard_err)?;
            eprintln!(
                "tsumiki-server: migrating world meta v1 (predates game modes) to v4; \
                 world becomes Creative, players get full health and empty inventory"
            );
            let mut players = HashMap::new();
            if let Some(player) = meta.player {
                players.insert(
                    LEGACY_PLAYER_NAME.to_string(),
                    PlayerRecord {
                        save: player,
                        hp: MAX_HP,
                        main: Vec::new(),
                    },
                );
            }
            Ok((
                meta.seed,
                GameMode::Creative,
                0.0,
                players,
                Vec::new(),
                Vec::new(),
            ))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("meta.bin has unsupported format version {other}"),
        )),
    }
}

/// The chunks and metadata read back from disk at startup.
pub struct LoadedWorld {
    pub seed: u64,
    /// The world's game mode as saved, or migrated (see [`decode_meta`]).
    pub game_mode: GameMode,
    pub world_time_of_day: f32,
    /// Persisted player records, keyed by player name.
    pub players: HashMap<String, PlayerRecord>,
    pub items: Vec<ItemRecord>,
    /// Chest contents, keyed by block position (see [`WorldMetaV4::containers`]).
    pub containers: Vec<(IVec3, Vec<Option<ItemStack>>)>,
    pub chunks: Vec<(IVec3, Chunk)>,
}

/// Region coordinate a chunk position falls into.
fn region_of(chunk_pos: IVec3) -> (i32, i32) {
    (
        chunk_pos.x.div_euclid(REGION_SIZE),
        chunk_pos.z.div_euclid(REGION_SIZE),
    )
}

fn meta_path(world_dir: &Path) -> PathBuf {
    world_dir.join("meta.bin")
}

fn regions_dir(world_dir: &Path) -> PathBuf {
    world_dir.join("regions")
}

fn region_path(world_dir: &Path, region: (i32, i32)) -> PathBuf {
    regions_dir(world_dir).join(format!("r.{}.{}.bin", region.0, region.1))
}

fn postcard_err(e: postcard::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

/// Serializes `value` and writes it to `path` atomically: the bytes land in
/// a sibling `.tmp` file first, which is then renamed over `path`. A crash or
/// power loss between the write and the rename leaves the original file (or
/// no file) intact, never a truncated one.
fn write_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = postcard::to_allocvec(value).map_err(postcard_err)?;
    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, &bytes)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Server-side persistence bookkeeping. Owns no chunk data itself (that
/// lives in the server's `ChunkCache`); tracks which chunks are persisted and
/// which have changed since the last save, and performs the actual disk I/O
/// on request.
#[derive(Resource)]
pub struct Persistence {
    world_dir: Option<PathBuf>,
    autosave_interval_secs: f64,
    /// Seconds accumulated since the last autosave check.
    accumulator: f64,
    /// Every chunk position that is persisted: loaded from disk at startup,
    /// or edited this session. Only these are ever written to region files;
    /// everything else regenerates deterministically from the seed.
    modified: HashSet<IVec3>,
    /// Chunk positions edited since the last save. Drives which region files
    /// get rewritten; cleared after a successful save.
    dirty_chunks: HashSet<IVec3>,
    /// Set when any player's save/health/inventory changes since the last
    /// save.
    player_dirty: bool,
    /// Set when the dropped-item set changes (spawn, pickup, merge, expiry)
    /// since the last save.
    items_dirty: bool,
    /// Set when any chest's contents change since the last save (roadmap M5).
    containers_dirty: bool,
}

impl Persistence {
    pub fn new(world_dir: Option<PathBuf>, autosave_interval_secs: f64) -> Self {
        Self {
            world_dir,
            autosave_interval_secs,
            accumulator: 0.0,
            modified: HashSet::new(),
            dirty_chunks: HashSet::new(),
            player_dirty: false,
            items_dirty: false,
            containers_dirty: false,
        }
    }

    /// Loads persisted state from `world_dir`, if any. Returns `Ok(None)`
    /// for an ephemeral server (`world_dir` is `None`) or a fresh world
    /// directory (no `meta.bin` yet). Every loaded chunk is recorded as
    /// `modified` so later autosaves keep persisting it.
    pub fn load(&mut self) -> io::Result<Option<LoadedWorld>> {
        let Some(dir) = self.world_dir.clone() else {
            return Ok(None);
        };
        let meta_file = meta_path(&dir);
        if !meta_file.exists() {
            return Ok(None);
        }

        let bytes = fs::read(&meta_file)?;
        let (seed, game_mode, world_time_of_day, players, items, containers) = decode_meta(&bytes)?;

        let mut chunks = Vec::new();
        let regions = regions_dir(&dir);
        if regions.is_dir() {
            for entry in fs::read_dir(&regions)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let bytes = fs::read(entry.path())?;
                let region_chunks: Vec<(IVec3, Chunk)> =
                    postcard::from_bytes(&bytes).map_err(postcard_err)?;
                for (pos, chunk) in region_chunks {
                    self.modified.insert(pos);
                    chunks.push((pos, chunk));
                }
            }
        }

        Ok(Some(LoadedWorld {
            seed,
            game_mode,
            world_time_of_day,
            players,
            items,
            containers,
            chunks,
        }))
    }

    /// Marks a chunk as edited: it must be persisted (and its region
    /// rewritten) on the next save.
    pub fn mark_chunk_dirty(&mut self, chunk_pos: IVec3) {
        self.modified.insert(chunk_pos);
        self.dirty_chunks.insert(chunk_pos);
    }

    /// Marks player state (position, health, or inventory) as changed since
    /// the last save.
    pub fn mark_player_dirty(&mut self) {
        self.player_dirty = true;
    }

    /// Marks the dropped-item set as changed since the last save.
    pub fn mark_items_dirty(&mut self) {
        self.items_dirty = true;
    }

    /// Marks a chest's contents as changed since the last save (roadmap M5).
    pub fn mark_containers_dirty(&mut self) {
        self.containers_dirty = true;
    }

    /// Whether `chunk_pos` is tracked as persisted: loaded from disk, or
    /// edited this session. Used by the server's bounded-memory eviction
    /// (doc/roadmap.md M3) to tell apart evictable pristine chunks (they
    /// regenerate deterministically from the seed) from modified chunks
    /// (the only copy of a player's edit, so never evicted).
    pub fn is_modified(&self, chunk_pos: IVec3) -> bool {
        self.modified.contains(&chunk_pos)
    }

    /// `true` if a save would write anything new.
    pub fn has_dirty(&self) -> bool {
        !self.dirty_chunks.is_empty()
            || self.player_dirty
            || self.items_dirty
            || self.containers_dirty
    }

    /// Advances the autosave clock by `dt` seconds. Returns `true` at most
    /// once per `autosave_interval_secs` crossed, regardless of whether
    /// anything is actually dirty (callers check [`Self::has_dirty`]
    /// themselves so a quiet world doesn't touch disk).
    pub fn autosave_due(&mut self, dt: f64) -> bool {
        self.accumulator += dt;
        if self.accumulator >= self.autosave_interval_secs {
            self.accumulator -= self.autosave_interval_secs;
            true
        } else {
            false
        }
    }

    /// Writes every region file touched by `dirty_chunks` (in full, from
    /// `cache`) plus `meta.bin` (always, since it's small and its contents
    /// -- game mode, time of day, players, items, containers -- are simplest
    /// to keep as one consistent snapshot rather than tracking granular
    /// dirtiness for each), then clears dirty tracking. A no-op for an
    /// ephemeral server. Used both for periodic autosave and for the final
    /// save on `Goodbye`.
    #[allow(clippy::too_many_arguments)]
    pub fn save(
        &mut self,
        seed: u64,
        game_mode: GameMode,
        world_time_of_day: f32,
        players: &HashMap<String, PlayerRecord>,
        items: &[ItemRecord],
        containers: &[(IVec3, Vec<Option<ItemStack>>)],
        chunks: &HashMap<IVec3, Chunk>,
    ) -> io::Result<()> {
        let Some(dir) = self.world_dir.clone() else {
            return Ok(());
        };

        let mut affected_regions: HashSet<(i32, i32)> = HashSet::new();
        for &pos in &self.dirty_chunks {
            affected_regions.insert(region_of(pos));
        }

        for region in affected_regions {
            let region_chunks: Vec<(IVec3, Chunk)> = self
                .modified
                .iter()
                .filter(|&&pos| region_of(pos) == region)
                .filter_map(|&pos| chunks.get(&pos).map(|chunk| (pos, chunk.clone())))
                .collect();
            write_atomic(&region_path(&dir, region), &region_chunks)?;
        }

        let meta = WorldMetaV4 {
            version: META_FORMAT_VERSION,
            seed,
            game_mode,
            world_time_of_day,
            players: players.clone(),
            items: items.to_vec(),
            containers: containers.to_vec(),
        };
        write_atomic(&meta_path(&dir), &meta)?;

        self.dirty_chunks.clear();
        self.player_dirty = false;
        self.items_dirty = false;
        self.containers_dirty = false;
        Ok(())
    }
}

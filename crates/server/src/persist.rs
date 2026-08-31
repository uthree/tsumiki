//! World persistence: chunks + player state saved to disk (doc/roadmap.md
//! M1, "World persistence"; M2 upgrades the player slot to per-name; M4 adds
//! game mode, health, inventory, dropped items, and time of day).
//!
//! Format contract:
//! - `<world_dir>/meta.bin`: postcard-serialized world metadata (format
//!   version, world seed, and everything else that isn't a chunk). Format v1
//!   (M1) had a single global player slot; v2 (M2) keyed player saves by
//!   name; v3 (M4) adds `game_mode`, `world_time_of_day`, per-player health +
//!   inventory, and dropped items. See [`decode_meta`] for how versions are
//!   told apart and migrated.
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
use tsumiki_world::{BlockId, Chunk};

/// Chunk columns per region edge; a region file covers all Y levels of an
/// 8x8 chunk-column footprint.
pub const REGION_SIZE: i32 = 8;

const META_FORMAT_VERSION: u32 = 3;

/// Key a migrated v1 (single global slot) player save is filed under in the
/// per-name map.
const LEGACY_PLAYER_NAME: &str = "player";

/// Per-player persisted state beyond position: health and inventory (M4).
/// `inventory` is a flat list rather than a map because postcard has no
/// native map-with-non-string-key support as convenient as a vec of pairs,
/// and the counts are small (item catalog is small, see design.md).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlayerRecord {
    pub save: PlayerSave,
    pub hp: u16,
    pub inventory: Vec<(BlockId, u32)>,
}

/// A dropped item entity as persisted (M4). Unlike the live server-side
/// representation, this carries no id or spawn timestamp: ids are
/// re-assigned and age resets on load, since neither matters for
/// correctness (a freshly-loaded item just gets a fresh pickup-delay and
/// expiry window).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ItemRecord {
    pub pos: Vec3,
    pub block: BlockId,
    pub count: u32,
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

/// On-disk contents of `meta.bin`, format v3 (M4): adds game mode, time of
/// day, per-player health/inventory, and dropped items.
#[derive(Serialize, Deserialize)]
struct WorldMetaV3 {
    version: u32,
    seed: u64,
    game_mode: GameMode,
    world_time_of_day: f32,
    players: HashMap<String, PlayerRecord>,
    items: Vec<ItemRecord>,
}

/// `(seed, game_mode, world_time_of_day, players, items)`, as decoded by
/// [`decode_meta`] regardless of which on-disk format version it read.
type DecodedMeta = (
    u64,
    GameMode,
    f32,
    HashMap<String, PlayerRecord>,
    Vec<ItemRecord>,
);

/// Decodes `meta.bin`, migrating v1/v2 files transparently.
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
/// neither of which is meaningful in creative mode.
fn decode_meta(bytes: &[u8]) -> io::Result<DecodedMeta> {
    let (header, _) = postcard::take_from_bytes::<VersionHeader>(bytes).map_err(postcard_err)?;
    match header.version {
        3 => {
            let meta: WorldMetaV3 = postcard::from_bytes(bytes).map_err(postcard_err)?;
            Ok((
                meta.seed,
                meta.game_mode,
                meta.world_time_of_day,
                meta.players,
                meta.items,
            ))
        }
        2 => {
            let meta: WorldMetaV2 = postcard::from_bytes(bytes).map_err(postcard_err)?;
            eprintln!(
                "tsumiki-server: migrating world meta v2 (predates game modes) to v3; \
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
                            inventory: Vec::new(),
                        },
                    )
                })
                .collect();
            Ok((meta.seed, GameMode::Creative, 0.0, players, Vec::new()))
        }
        1 => {
            let meta: WorldMetaV1 = postcard::from_bytes(bytes).map_err(postcard_err)?;
            eprintln!(
                "tsumiki-server: migrating world meta v1 (predates game modes) to v3; \
                 world becomes Creative, players get full health and empty inventory"
            );
            let mut players = HashMap::new();
            if let Some(player) = meta.player {
                players.insert(
                    LEGACY_PLAYER_NAME.to_string(),
                    PlayerRecord {
                        save: player,
                        hp: MAX_HP,
                        inventory: Vec::new(),
                    },
                );
            }
            Ok((meta.seed, GameMode::Creative, 0.0, players, Vec::new()))
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
        let (seed, game_mode, world_time_of_day, players, items) = decode_meta(&bytes)?;

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
        !self.dirty_chunks.is_empty() || self.player_dirty || self.items_dirty
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
    /// -- game mode, time of day, players, items -- are simplest to keep as
    /// one consistent snapshot rather than tracking granular dirtiness for
    /// each), then clears dirty tracking. A no-op for an ephemeral server.
    /// Used both for periodic autosave and for the final save on `Goodbye`.
    #[allow(clippy::too_many_arguments)]
    pub fn save(
        &mut self,
        seed: u64,
        game_mode: GameMode,
        world_time_of_day: f32,
        players: &HashMap<String, PlayerRecord>,
        items: &[ItemRecord],
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

        let meta = WorldMetaV3 {
            version: META_FORMAT_VERSION,
            seed,
            game_mode,
            world_time_of_day,
            players: players.clone(),
            items: items.to_vec(),
        };
        write_atomic(&meta_path(&dir), &meta)?;

        self.dirty_chunks.clear();
        self.player_dirty = false;
        self.items_dirty = false;
        Ok(())
    }
}

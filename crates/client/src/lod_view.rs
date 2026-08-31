//! LOD ring requesting, meshing and lifecycle (design.md §3, doc/roadmap.md M3).
//!
//! - Pure band math ([`inner_bound`], [`outer_bound`], [`is_wanted`],
//!   [`wanted_lod_chunks`]): which level-`L` LOD chunks a camera wants, given
//!   [`Settings::view_distance_chunks`]. Bands double per level, with one
//!   level-`L` chunk span of overlap between consecutive bands so a point
//!   anywhere past the level-0 view distance is always covered by at least
//!   one level: level `L`'s band is `[inner_L, outer_L]` where
//!   `outer_L = vd_blocks << L` and
//!   `inner_L = (vd_blocks << (L - 1)) - chunk_span(L)` (algebraically,
//!   `outer_{L-1} - chunk_span(L)`). A level-`L` chunk is wanted iff its
//!   horizontal *center* distance to the camera lies in that band, for every
//!   `y` in `0..world_height_lod_chunks(L)`.
//! - [`LodStore`] resource: received LOD chunks (keyed by `(level, pos)`),
//!   the requested set, the meshed set and spawned entities. [`insert_lod_chunk`]
//!   is [`crate::net`]'s entrypoint for `LodChunkData`: an unsolicited
//!   re-send for a `(level, pos)` already held replaces the data and marks it
//!   (and any already-meshed same-level neighbor) for re-mesh, since a
//!   neighbor's mesh may have sampled this position as absent (air) before.
//! - Per frame, only in [`AppState::InGame`]: request not-yet-requested
//!   wanted chunks nearest-first, batched like [`crate::net::request_chunks`];
//!   mesh unmeshed chunks nearest-first (no wait for all 6 neighbors — a
//!   missing same-level neighbor is `None` = air, which is what closes ring
//!   boundaries with wall faces; intended), spending only the
//!   [`view::MeshFrameBudget`] level-0 meshing left over this frame, so LOD
//!   work never starves it; despawn (and forget) chunks that leave their band
//!   with a hysteresis margin of one level-`L` chunk span, so camera jitter
//!   near a boundary doesn't thrash.
//! - Spawn transform: `translation = pos * chunk_span(L)` with a small
//!   downward offset (hides z-fighting at the LOD0 seam), `scale =
//!   cell_size(L)` (mesh-local units are cells, not blocks). Shares
//!   [`view::ChunkMaterial`] — no second material.
//! - Full teardown in `OnExit(AppState::InGame)`, mirroring [`view`].

use std::collections::{HashMap, HashSet};

use bevy::prelude::*;
use tsumiki_world::lod::{MAX_LOD, chunk_span, world_height_lod_chunks};
use tsumiki_world::{CHUNK_SIZE, Chunk};

use crate::AppState;
use crate::camera::Player;
use crate::mesh::build_chunk_mesh;
use crate::net::Transport;
use crate::settings::Settings;
use crate::view;

/// Upper bound on `(level, position)` pairs requested in a single frame's
/// messages, across all levels combined. Mirrors
/// [`net::MAX_CHUNK_REQUESTS_PER_FRAME`] (also doubled alongside the raised
/// view distance range, see that constant's doc comment).
const MAX_LOD_CHUNK_REQUESTS_PER_FRAME: usize = 128;

/// Vertical downward offset (blocks) applied to every LOD chunk's spawn
/// translation, so it renders fractionally below coincident level-0 terrain
/// at the seam instead of z-fighting with it.
const SEAM_Y_OFFSET: f32 = -0.25;

/// Same-level neighbor offsets, in level-`L` chunk coordinates. Order must
/// match [`build_chunk_mesh`]'s `neighbors` parameter: `[-X, +X, -Y, +Y, -Z, +Z]`.
const NEIGHBOR_OFFSETS: [IVec3; 6] = [
    IVec3::NEG_X,
    IVec3::X,
    IVec3::NEG_Y,
    IVec3::Y,
    IVec3::NEG_Z,
    IVec3::Z,
];

/// Inner bound of level `level`'s band, in blocks. `level` must be in
/// `1..=MAX_LOD` (debug-asserted).
#[inline]
pub fn inner_bound(level: u8, vd_blocks: i32) -> i32 {
    debug_assert!(level >= 1);
    (vd_blocks << (level - 1)) - chunk_span(level)
}

/// Outer bound of level `level`'s band, in blocks.
#[inline]
pub fn outer_bound(level: u8, vd_blocks: i32) -> i32 {
    vd_blocks << level
}

/// Horizontal distance from `camera_xz` (`.x`/`.y` mapping world X/Z, as
/// elsewhere in this crate) to the center of the level-`level` chunk at
/// `pos` (level-`level` chunk coordinates), in blocks.
pub fn center_distance(level: u8, pos: IVec3, camera_xz: Vec2) -> f32 {
    let span = chunk_span(level) as f32;
    let center = Vec2::new((pos.x as f32 + 0.5) * span, (pos.z as f32 + 0.5) * span);
    camera_xz.distance(center)
}

/// Whether the level-`level` chunk at `pos` belongs in the wanted set for a
/// camera at `camera_xz` with view distance `vd_blocks` blocks (see module
/// docs for the band formula).
pub fn is_wanted(level: u8, pos: IVec3, camera_xz: Vec2, vd_blocks: i32) -> bool {
    if pos.y < 0 || pos.y >= world_height_lod_chunks(level) {
        return false;
    }
    let dist = center_distance(level, pos, camera_xz);
    dist >= inner_bound(level, vd_blocks) as f32 && dist <= outer_bound(level, vd_blocks) as f32
}

/// Like [`is_wanted`], but with an extra hysteresis margin of one level-`L`
/// chunk span on both bounds, so a chunk sitting right at the band boundary
/// doesn't despawn and re-request every time the camera jitters across it.
fn is_wanted_with_hysteresis(level: u8, pos: IVec3, camera_xz: Vec2, vd_blocks: i32) -> bool {
    if pos.y < 0 || pos.y >= world_height_lod_chunks(level) {
        return false;
    }
    let margin = chunk_span(level) as f32;
    let dist = center_distance(level, pos, camera_xz);
    let inner = (inner_bound(level, vd_blocks) as f32 - margin).max(0.0);
    let outer = outer_bound(level, vd_blocks) as f32 + margin;
    dist >= inner && dist <= outer
}

/// Every `(level, pos)` pair currently wanted, across every LOD level.
/// Unordered (callers that need nearest-first sort the result themselves);
/// deterministic and a pure function of its inputs.
pub fn wanted_lod_chunks(camera_xz: Vec2, vd_blocks: i32) -> Vec<(u8, IVec3)> {
    let mut result = Vec::new();
    for level in 1..=MAX_LOD {
        let span = chunk_span(level) as f32;
        let outer = outer_bound(level, vd_blocks) as f32;
        // +1 chunk of slack past the ceiling division so a chunk whose
        // footprint only partially overlaps the outer radius isn't missed.
        let radius_chunks = (outer / span).ceil() as i32 + 1;
        let cam_cx = (camera_xz.x / span).floor() as i32;
        let cam_cz = (camera_xz.y / span).floor() as i32;
        let y_count = world_height_lod_chunks(level);
        for dx in -radius_chunks..=radius_chunks {
            for dz in -radius_chunks..=radius_chunks {
                for y in 0..y_count {
                    let pos = IVec3::new(cam_cx + dx, y, cam_cz + dz);
                    if is_wanted(level, pos, camera_xz, vd_blocks) {
                        result.push((level, pos));
                    }
                }
            }
        }
    }
    result
}

/// Client-side LOD chunk cache and mesh bookkeeping, mirroring
/// [`view::ChunkStore`] one level of indirection up (keyed by `(level, pos)`
/// instead of just `pos`).
#[derive(Resource, Default)]
pub struct LodStore {
    pub chunks: HashMap<(u8, IVec3), Chunk>,
    pub requested: HashSet<(u8, IVec3)>,
    pub meshed: HashSet<(u8, IVec3)>,
    pub entities: HashMap<(u8, IVec3), Entity>,
}

/// [`crate::net`]'s entrypoint for a received `LodChunkData`: inserts (or
/// replaces) the chunk, clears `requested`, and marks it — and any
/// already-meshed same-level neighbor — for re-mesh. See module docs.
pub fn insert_lod_chunk(store: &mut LodStore, level: u8, pos: IVec3, chunk: Chunk) {
    let key = (level, pos);
    store.chunks.insert(key, chunk);
    store.requested.remove(&key);
    store.meshed.remove(&key);

    for offset in NEIGHBOR_OFFSETS {
        let neighbor = (level, pos + offset);
        store.meshed.remove(&neighbor);
    }
}

/// Wires the LOD requesting/meshing/despawn systems into `app`.
pub fn install(app: &mut App) {
    app.init_resource::<LodStore>()
        .add_systems(OnExit(AppState::InGame), teardown_lod_chunks)
        .add_systems(
            Update,
            (
                request_lod_chunks,
                despawn_stale_lod_chunks,
                mesh_lod_chunks.after(view::mesh_ready_chunks),
            )
                .run_if(in_state(AppState::InGame)),
        );
}

fn request_lod_chunks(
    mut transport: ResMut<Transport>,
    mut store: ResMut<LodStore>,
    settings: Res<Settings>,
    cameras: Query<&Transform, With<Player>>,
) {
    let Ok(transform) = cameras.single() else {
        return;
    };
    let camera_xz = Vec2::new(transform.translation.x, transform.translation.z);
    let vd_blocks = settings.view_distance_chunks * CHUNK_SIZE as i32;

    let mut candidates: Vec<(f32, u8, IVec3)> = wanted_lod_chunks(camera_xz, vd_blocks)
        .into_iter()
        .filter(|&(level, pos)| {
            let key = (level, pos);
            !store.chunks.contains_key(&key) && !store.requested.contains(&key)
        })
        .map(|(level, pos)| (center_distance(level, pos, camera_xz), level, pos))
        .collect();
    if candidates.is_empty() {
        return;
    }

    candidates.sort_by(|a, b| a.0.total_cmp(&b.0));
    candidates.truncate(MAX_LOD_CHUNK_REQUESTS_PER_FRAME);

    let mut by_level: HashMap<u8, Vec<IVec3>> = HashMap::new();
    for &(_, level, pos) in &candidates {
        store.requested.insert((level, pos));
        by_level.entry(level).or_default().push(pos);
    }
    for (level, positions) in by_level {
        transport.send(tsumiki_protocol::ClientToServer::RequestLodChunks { level, positions });
    }
}

/// Mesh data for one LOD chunk, built while `store` is only borrowed
/// immutably (see [`mesh_lod_chunks`] for why the split matters).
fn build_lod_chunk(
    store: &LodStore,
    level: u8,
    pos: IVec3,
    registry: &tsumiki_world::BlockRegistry,
) -> crate::mesh::MeshBuild {
    let chunk = &store.chunks[&(level, pos)];
    if chunk.is_all_air() {
        return crate::mesh::MeshBuild::default();
    }
    let neighbors = [
        store.chunks.get(&(level, pos + NEIGHBOR_OFFSETS[0])),
        store.chunks.get(&(level, pos + NEIGHBOR_OFFSETS[1])),
        store.chunks.get(&(level, pos + NEIGHBOR_OFFSETS[2])),
        store.chunks.get(&(level, pos + NEIGHBOR_OFFSETS[3])),
        store.chunks.get(&(level, pos + NEIGHBOR_OFFSETS[4])),
        store.chunks.get(&(level, pos + NEIGHBOR_OFFSETS[5])),
    ];
    build_chunk_mesh(chunk, neighbors, registry)
}

/// Meshes unmeshed LOD chunks nearest-first, spending only what
/// [`view::MeshFrameBudget`] has left after level-0 meshing this frame (lower
/// priority; see module docs).
#[allow(clippy::too_many_arguments)]
fn mesh_lod_chunks(
    mut commands: Commands,
    mut store: ResMut<LodStore>,
    mut meshes: ResMut<Assets<Mesh>>,
    material: Option<Res<view::ChunkMaterial>>,
    registry: Res<view::Registry>,
    mut frame_budget: ResMut<view::MeshFrameBudget>,
    cameras: Query<&Transform, With<Player>>,
    mesh_handles: Query<&Mesh3d>,
) {
    let Some(material) = material else {
        return;
    };
    if frame_budget.0 == 0 {
        return;
    }

    let camera_xz = cameras
        .single()
        .map(|t| Vec2::new(t.translation.x, t.translation.z))
        .unwrap_or(Vec2::ZERO);

    let mut candidates: Vec<(u8, IVec3)> = store
        .chunks
        .keys()
        .copied()
        .filter(|key| !store.meshed.contains(key))
        .collect();
    candidates.sort_by(|a, b| {
        center_distance(a.0, a.1, camera_xz).total_cmp(&center_distance(b.0, b.1, camera_xz))
    });
    candidates.truncate(frame_budget.0);

    // Build mesh data up front (only reads `store.chunks`) so the borrow
    // never overlaps the `store.meshed`/`store.entities` mutation below.
    let builds: Vec<(u8, IVec3, crate::mesh::MeshBuild)> = candidates
        .iter()
        .map(|&(level, pos)| (level, pos, build_lod_chunk(&store, level, pos, &registry.0)))
        .collect();
    frame_budget.0 -= builds.len();

    for (level, pos, build) in builds {
        let key = (level, pos);
        store.meshed.insert(key);
        let existing = store.entities.get(&key).copied();

        if build.is_empty() {
            if let Some(entity) = existing {
                if let Ok(mesh3d) = mesh_handles.get(entity) {
                    meshes.remove(&mesh3d.0);
                }
                commands.entity(entity).despawn();
                store.entities.remove(&key);
            }
            continue;
        }

        match existing {
            Some(entity) => {
                let existing_handle = mesh_handles.get(entity).ok().map(|mesh3d| mesh3d.0.clone());
                let can_reuse = existing_handle
                    .as_ref()
                    .is_some_and(|handle| meshes.contains(handle));
                if can_reuse {
                    let handle = existing_handle.expect("checked by can_reuse");
                    if let Some(mut mesh) = meshes.get_mut(&handle) {
                        *mesh = view::to_bevy_mesh(build);
                    }
                } else {
                    let handle = meshes.add(view::to_bevy_mesh(build));
                    commands.entity(entity).insert(Mesh3d(handle));
                }
            }
            None => {
                let translation =
                    pos.as_vec3() * chunk_span(level) as f32 + Vec3::new(0.0, SEAM_Y_OFFSET, 0.0);
                let scale = Vec3::splat(tsumiki_world::lod::cell_size(level) as f32);
                let handle = meshes.add(view::to_bevy_mesh(build));
                let entity = commands
                    .spawn((
                        Mesh3d(handle),
                        MeshMaterial3d(material.0.clone()),
                        Transform::from_translation(translation).with_scale(scale),
                    ))
                    .id();
                store.entities.insert(key, entity);
            }
        }
    }
}

fn despawn_stale_lod_chunks(
    mut commands: Commands,
    mut store: ResMut<LodStore>,
    mut meshes: ResMut<Assets<Mesh>>,
    mesh_handles: Query<&Mesh3d>,
    settings: Res<Settings>,
    cameras: Query<&Transform, With<Player>>,
) {
    let Ok(transform) = cameras.single() else {
        return;
    };
    let camera_xz = Vec2::new(transform.translation.x, transform.translation.z);
    let vd_blocks = settings.view_distance_chunks * CHUNK_SIZE as i32;

    let stale: Vec<(u8, IVec3)> = store
        .chunks
        .keys()
        .copied()
        .filter(|&(level, pos)| !is_wanted_with_hysteresis(level, pos, camera_xz, vd_blocks))
        .collect();

    for key in stale {
        if let Some(entity) = store.entities.remove(&key) {
            if let Ok(mesh3d) = mesh_handles.get(entity) {
                meshes.remove(&mesh3d.0);
            }
            commands.entity(entity).despawn();
        }
        store.chunks.remove(&key);
        store.requested.remove(&key);
        store.meshed.remove(&key);
    }
}

/// Part of the `OnExit(AppState::InGame)` "despawn everything in-game"
/// contract (see `pause` module docs; mirrors [`view::teardown_chunks`]):
/// despawns every LOD chunk mesh entity, frees their `Mesh` assets, and
/// fully clears [`LodStore`]. Does not touch [`view::ChunkMaterial`] — it is
/// shared, and [`view::teardown_chunks`] owns its lifecycle.
fn teardown_lod_chunks(
    mut commands: Commands,
    mut store: ResMut<LodStore>,
    mut meshes: ResMut<Assets<Mesh>>,
    mesh_handles: Query<&Mesh3d>,
) {
    for (_, entity) in store.entities.drain() {
        if let Ok(mesh3d) = mesh_handles.get(entity) {
            meshes.remove(&mesh3d.0);
        }
        commands.entity(entity).despawn();
    }
    store.chunks.clear();
    store.requested.clear();
    store.meshed.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net;

    const DEFAULT_VD_CHUNKS: i32 = net::VIEW_DISTANCE_CHUNKS;

    fn default_vd_blocks() -> i32 {
        DEFAULT_VD_CHUNKS * CHUNK_SIZE as i32
    }

    #[test]
    fn bands_cover_the_full_radial_range_without_gaps() {
        for vd_chunks in [4, 8, 12, 24] {
            let vd_blocks = vd_chunks * CHUNK_SIZE as i32;
            let horizon = outer_bound(MAX_LOD, vd_blocks);
            let samples = 500;
            for i in 0..=samples {
                let dist =
                    vd_blocks as f32 + (horizon - vd_blocks) as f32 * (i as f32 / samples as f32);
                let covered = (1..=MAX_LOD).any(|level| {
                    dist >= inner_bound(level, vd_blocks) as f32
                        && dist <= outer_bound(level, vd_blocks) as f32
                });
                assert!(
                    covered,
                    "distance {dist} not covered by any band (vd_blocks={vd_blocks})"
                );
            }
        }
    }

    #[test]
    fn consecutive_bands_overlap_by_exactly_one_chunk_span() {
        let vd_blocks = default_vd_blocks();
        for level in 2..=MAX_LOD {
            let overlap = outer_bound(level - 1, vd_blocks) - inner_bound(level, vd_blocks);
            assert_eq!(overlap, chunk_span(level));
        }
        // Level 1's inner bound reaches back from the level-0 view distance
        // (`vd_blocks`, i.e. `outer_0` if level 0 had a band) by one
        // level-1 chunk span: exactly the "for L=1" case in the spec, which
        // the general formula already produces.
        assert_eq!(vd_blocks - inner_bound(1, vd_blocks), chunk_span(1));
    }

    #[test]
    fn wanted_counts_are_bounded_per_level() {
        let vd_blocks = default_vd_blocks();
        let wanted = wanted_lod_chunks(Vec2::ZERO, vd_blocks);

        for level in 1..=MAX_LOD {
            let count = wanted.iter().filter(|&&(l, _)| l == level).count();
            assert!(count > 0, "level {level} wants nothing");

            let span = chunk_span(level);
            let outer = outer_bound(level, vd_blocks);
            // Generous sanity bound: a square covering the outer radius plus
            // slack, times the vertical chunk count.
            let side_chunks = (2 * outer / span + 4) as i64;
            let bound = side_chunks * side_chunks * world_height_lod_chunks(level) as i64;
            assert!(
                (count as i64) <= bound,
                "level {level}: {count} exceeds sanity bound {bound}"
            );
        }
    }

    #[test]
    fn wanted_lod_chunks_is_deterministic() {
        let vd_blocks = default_vd_blocks();
        let a = wanted_lod_chunks(Vec2::new(123.0, -45.0), vd_blocks);
        let b = wanted_lod_chunks(Vec2::new(123.0, -45.0), vd_blocks);
        assert_eq!(a, b);
    }

    #[test]
    fn moving_camera_by_one_chunk_changes_the_set_incrementally() {
        let vd_blocks = default_vd_blocks();
        let before: HashSet<(u8, IVec3)> = wanted_lod_chunks(Vec2::new(500.0, 500.0), vd_blocks)
            .into_iter()
            .collect();
        // Moved by one level-0 chunk span (32 blocks).
        let after: HashSet<(u8, IVec3)> =
            wanted_lod_chunks(Vec2::new(500.0 + CHUNK_SIZE as f32, 500.0), vd_blocks)
                .into_iter()
                .collect();

        let intersection = before.intersection(&after).count();
        let union = before.union(&after).count();
        assert!(
            (intersection as f32 / union as f32) > 0.8,
            "moving one chunk should mostly preserve the wanted set: \
             intersection={intersection} union={union}"
        );
    }

    #[test]
    fn chunk_outside_world_height_is_never_wanted() {
        let vd_blocks = default_vd_blocks();
        let above = IVec3::new(0, world_height_lod_chunks(1), 0);
        assert!(!is_wanted(1, above, Vec2::ZERO, vd_blocks));
    }

    #[test]
    fn chunk_at_camera_center_is_not_wanted_at_any_level() {
        // The camera's own position is deep inside level 0's territory, well
        // inside every level's inner bound.
        let vd_blocks = default_vd_blocks();
        for level in 1..=MAX_LOD {
            assert!(!is_wanted(level, IVec3::ZERO, Vec2::ZERO, vd_blocks));
        }
    }

    #[test]
    fn insert_lod_chunk_replaces_data_and_marks_for_remesh() {
        let mut store = LodStore::default();
        let key = (1_u8, IVec3::new(0, 0, 0));
        insert_lod_chunk(
            &mut store,
            key.0,
            key.1,
            Chunk::filled(tsumiki_world::blocks::AIR),
        );
        store.meshed.insert(key);

        // Unsolicited re-send: must clear `meshed` so it gets re-meshed.
        insert_lod_chunk(
            &mut store,
            key.0,
            key.1,
            Chunk::filled(tsumiki_world::blocks::STONE),
        );
        assert!(!store.meshed.contains(&key));
        assert_eq!(
            store.chunks[&key].get(UVec3::ZERO),
            tsumiki_world::blocks::STONE
        );
    }

    #[test]
    fn insert_lod_chunk_dirties_already_meshed_same_level_neighbor() {
        let mut store = LodStore::default();
        let level = 1_u8;
        let center = IVec3::new(0, 0, 0);
        let neighbor = center + IVec3::X;

        insert_lod_chunk(
            &mut store,
            level,
            neighbor,
            Chunk::filled(tsumiki_world::blocks::AIR),
        );
        store.meshed.insert((level, neighbor));

        insert_lod_chunk(
            &mut store,
            level,
            center,
            Chunk::filled(tsumiki_world::blocks::STONE),
        );

        assert!(
            !store.meshed.contains(&(level, neighbor)),
            "a newly-arrived chunk must dirty an already-meshed same-level neighbor"
        );
    }
}

//! Client-side chunk cache and mesh lifecycle.
//!
//! - `ChunkStore` resource: received chunks, the requested set, the mapping
//!   from chunk position to spawned mesh entity, and the dirty set of edited
//!   chunks awaiting remesh.
//! - Per frame, mesh up to a small budget of chunks, dirty (edited) chunks
//!   first so edits feel instant, then newly-arrived chunks nearest first. A
//!   chunk is ready to mesh when all six neighbors are available; a neighbor
//!   position outside the vertical world bounds counts as available (air).
//!   All-air chunks and empty mesh results spawn nothing but are marked
//!   meshed.
//! - Spawn mesh entities at `chunk_pos * CHUNK_SIZE` sharing a single
//!   white `StandardMaterial` (vertex colors carry the block colors). Dirty
//!   remeshes reuse the existing `Mesh` asset and entity where possible.
//! - [`set_block`] is the single edit entrypoint used by both local
//!   prediction ([`crate::interact`]) and server echoes ([`crate::net`]).
//! - Despawn (and forget) chunks that fall well outside the view distance.

use std::collections::{HashMap, HashSet};

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use tsumiki_world::{BlockId, BlockRegistry, CHUNK_SIZE, Chunk, WORLD_HEIGHT_CHUNKS};

use crate::AppState;
use crate::camera::Player;
use crate::mesh::{MeshBuild, build_chunk_mesh};
use crate::net::VIEW_DISTANCE_CHUNKS;

/// Chunks meshed per frame. Kept small so a burst of newly-arrived chunks
/// doesn't spike a single frame's cost.
const MESH_BUDGET_PER_FRAME: usize = 6;

/// Chunks farther than `VIEW_DISTANCE_CHUNKS + this` (horizontally) are
/// despawned and forgotten. The margin avoids despawn/reload thrashing for
/// chunks sitting right at the request boundary.
const DESPAWN_MARGIN_CHUNKS: i32 = 2;

/// Neighbor offsets in the order `build_chunk_mesh` expects:
/// `[-X, +X, -Y, +Y, -Z, +Z]`.
const NEIGHBOR_OFFSETS: [IVec3; 6] = [
    IVec3::NEG_X,
    IVec3::X,
    IVec3::NEG_Y,
    IVec3::Y,
    IVec3::NEG_Z,
    IVec3::Z,
];

/// The world's block registry, wrapped as a Bevy resource (the `world` crate
/// itself stays free of ECS dependencies).
///
/// Public so [`crate::camera`] and [`crate::interact`] can query block
/// solidity for physics and raycasting.
#[derive(Resource)]
pub struct Registry(pub BlockRegistry);

/// The single white, vertex-color-respecting material shared by every chunk
/// mesh.
#[derive(Resource, Clone)]
struct ChunkMaterial(Handle<StandardMaterial>);

/// Client-side chunk cache and mesh bookkeeping.
///
/// - `chunks`: every chunk the server has sent, keyed by chunk position.
/// - `requested`: positions already asked for, so [`crate::net`] does not
///   re-request them every frame while the reply is in flight.
/// - `meshed`: positions already processed by the mesher, whether or not
///   they produced a visible entity (an all-air chunk is meshed to nothing).
/// - `entities`: chunk position -> spawned mesh entity, for positions that
///   produced a visible mesh.
/// - `dirty`: chunks edited since they were last meshed (via [`set_block`]
///   or a `BlockChanged` echo); served by the mesher before newly-arrived
///   chunks so edits feel instant.
#[derive(Resource, Default)]
pub struct ChunkStore {
    pub chunks: HashMap<IVec3, Chunk>,
    pub requested: HashSet<IVec3>,
    pub meshed: HashSet<IVec3>,
    pub entities: HashMap<IVec3, Entity>,
    pub dirty: HashSet<IVec3>,
}

/// Wires the chunk cache, mesher and despawn systems into `app`, and inserts
/// `registry` as the (wrapped) block registry resource.
///
/// `Registry` and `ChunkStore` are inserted unconditionally (not gated to
/// [`AppState::InGame`]) since [`crate::screenshot`] reads `ChunkStore`
/// regardless of app state; only the per-frame meshing/despawn work and the
/// chunk-material setup are in-game-only.
pub fn install(app: &mut App, registry: BlockRegistry) {
    app.insert_resource(Registry(registry))
        .init_resource::<ChunkStore>()
        .add_systems(OnEnter(AppState::InGame), setup_chunk_material)
        .add_systems(
            Update,
            (mesh_ready_chunks, despawn_far_chunks).run_if(in_state(AppState::InGame)),
        );
}

fn setup_chunk_material(mut commands: Commands, mut materials: ResMut<Assets<StandardMaterial>>) {
    let handle = materials.add(StandardMaterial {
        base_color: Color::WHITE,
        perceptual_roughness: 1.0,
        ..default()
    });
    commands.insert_resource(ChunkMaterial(handle));
}

/// Converts a world-space translation into a chunk position.
pub fn world_pos_to_chunk(pos: Vec3) -> IVec3 {
    IVec3::new(
        (pos.x / CHUNK_SIZE as f32).floor() as i32,
        (pos.y / CHUNK_SIZE as f32).floor() as i32,
        (pos.z / CHUNK_SIZE as f32).floor() as i32,
    )
}

fn chunk_distance_sq(a: IVec3, b: IVec3) -> i32 {
    let d = a - b;
    d.x * d.x + d.y * d.y + d.z * d.z
}

fn is_vertically_in_bounds(y: i32) -> bool {
    (0..WORLD_HEIGHT_CHUNKS).contains(&y)
}

/// The block at world-space `pos`.
///
/// Positions outside the vertical world bounds are always air (there is
/// never a chunk there to load). Otherwise `None` means the chunk containing
/// `pos` has not been loaded yet.
pub fn block_at(store: &ChunkStore, pos: IVec3) -> Option<BlockId> {
    let (chunk_pos, local) = tsumiki_world::split_block_pos(pos);
    if !is_vertically_in_bounds(chunk_pos.y) {
        return Some(BlockId::AIR);
    }
    store
        .chunks
        .get(&chunk_pos)
        .map(|chunk| chunk.get(UVec3::new(local.x as u32, local.y as u32, local.z as u32)))
}

/// `true` if the chunk containing `pos` is available for solidity queries:
/// either actually loaded, or outside the vertical world bounds (where
/// [`block_at`] always reports air without needing a chunk).
pub fn is_chunk_loaded(store: &ChunkStore, pos: IVec3) -> bool {
    let (chunk_pos, _) = tsumiki_world::split_block_pos(pos);
    !is_vertically_in_bounds(chunk_pos.y) || store.chunks.contains_key(&chunk_pos)
}

/// Edits the block at world-space `pos` in its containing chunk, if loaded,
/// and marks that chunk dirty for remeshing.
///
/// When `pos` sits on a chunk border, the neighbor(s) whose mesh depends on
/// this block (their greedy meshing samples one block across the border) are
/// marked dirty too — up to three for a corner edit.
///
/// Returns the chunk positions that became dirty; empty if the chunk was not
/// loaded (the edit is silently dropped, matching the case where a
/// `BlockChanged` or local prediction targets a not-yet-loaded chunk).
pub fn set_block(store: &mut ChunkStore, pos: IVec3, block: BlockId) -> Vec<IVec3> {
    let (chunk_pos, local) = tsumiki_world::split_block_pos(pos);
    let Some(chunk) = store.chunks.get_mut(&chunk_pos) else {
        return Vec::new();
    };
    chunk.set(
        UVec3::new(local.x as u32, local.y as u32, local.z as u32),
        block,
    );

    let size = CHUNK_SIZE as i32;
    let mut dirtied = vec![chunk_pos];
    if local.x == 0 {
        dirtied.push(chunk_pos + IVec3::NEG_X);
    }
    if local.x == size - 1 {
        dirtied.push(chunk_pos + IVec3::X);
    }
    if local.y == 0 {
        dirtied.push(chunk_pos + IVec3::NEG_Y);
    }
    if local.y == size - 1 {
        dirtied.push(chunk_pos + IVec3::Y);
    }
    if local.z == 0 {
        dirtied.push(chunk_pos + IVec3::NEG_Z);
    }
    if local.z == size - 1 {
        dirtied.push(chunk_pos + IVec3::Z);
    }

    for &d in &dirtied {
        store.dirty.insert(d);
    }
    dirtied
}

/// A neighbor position outside the vertical world bounds counts as present
/// (air) for readiness purposes, matching the mesh contract where `None`
/// means air.
fn neighbor_present(store: &ChunkStore, pos: IVec3) -> bool {
    !is_vertically_in_bounds(pos.y) || store.chunks.contains_key(&pos)
}

/// The neighbor chunk to pass to `build_chunk_mesh`: `None` both for
/// out-of-bounds (air) and, defensively, for a missing loaded chunk.
fn neighbor_chunk(store: &ChunkStore, pos: IVec3) -> Option<&Chunk> {
    if is_vertically_in_bounds(pos.y) {
        store.chunks.get(&pos)
    } else {
        None
    }
}

fn is_ready_to_mesh(store: &ChunkStore, pos: IVec3) -> bool {
    store.chunks.contains_key(&pos)
        && NEIGHBOR_OFFSETS
            .iter()
            .all(|&offset| neighbor_present(store, pos + offset))
}

/// True when some cached chunk is ready to mesh but hasn't been yet. Used by
/// [`crate::screenshot`] to detect that the initial view has settled.
pub fn any_chunk_ready(store: &ChunkStore) -> bool {
    store
        .chunks
        .keys()
        .any(|&pos| !store.meshed.contains(&pos) && is_ready_to_mesh(store, pos))
}

/// Builds mesh data for a ready chunk, or `None` if it vanished from the
/// store between the readiness check and now (should not normally happen).
/// An all-air chunk short-circuits to an empty build without touching the
/// mesher.
fn build_ready_chunk(
    store: &ChunkStore,
    pos: IVec3,
    registry: &BlockRegistry,
) -> Option<MeshBuild> {
    let chunk = store.chunks.get(&pos)?;
    if chunk.is_all_air() {
        return Some(MeshBuild::default());
    }
    let neighbors = [
        neighbor_chunk(store, pos + IVec3::NEG_X),
        neighbor_chunk(store, pos + IVec3::X),
        neighbor_chunk(store, pos + IVec3::NEG_Y),
        neighbor_chunk(store, pos + IVec3::Y),
        neighbor_chunk(store, pos + IVec3::NEG_Z),
        neighbor_chunk(store, pos + IVec3::Z),
    ];
    Some(build_chunk_mesh(chunk, neighbors, registry))
}

fn to_bevy_mesh(build: MeshBuild) -> Mesh {
    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::RENDER_WORLD,
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, build.positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, build.normals);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, build.colors);
    mesh.insert_indices(Indices::U32(build.indices));
    mesh
}

/// Rebuilds the mesh for one already-loaded chunk in place: reuses the
/// existing `Mesh` asset (and entity) when possible, so remeshing a dirty
/// chunk never leaks a stale `Mesh` into `Assets<Mesh>`.
fn remesh_chunk(
    commands: &mut Commands,
    store: &mut ChunkStore,
    meshes: &mut Assets<Mesh>,
    mesh_handles: &Query<&Mesh3d>,
    material: &ChunkMaterial,
    registry: &BlockRegistry,
    pos: IVec3,
) {
    let Some(build) = build_ready_chunk(store, pos, registry) else {
        return;
    };
    store.meshed.insert(pos);

    let existing = store.entities.get(&pos).copied();
    match existing {
        Some(entity) if build.is_empty() => {
            if let Ok(mesh3d) = mesh_handles.get(entity) {
                meshes.remove(&mesh3d.0);
            }
            commands.entity(entity).despawn();
            store.entities.remove(&pos);
        }
        Some(entity) => {
            // Overwrite the mesh data behind the existing handle in place
            // (still change-tracked, so the renderer re-extracts it) rather
            // than allocating a new asset slot.
            let existing_handle = mesh_handles.get(entity).ok().map(|mesh3d| mesh3d.0.clone());
            let can_reuse = existing_handle
                .as_ref()
                .is_some_and(|handle| meshes.contains(handle));

            if can_reuse {
                let handle = existing_handle.expect("checked by can_reuse");
                if let Some(mut mesh) = meshes.get_mut(&handle) {
                    *mesh = to_bevy_mesh(build);
                }
            } else {
                let handle = meshes.add(to_bevy_mesh(build));
                commands.entity(entity).insert(Mesh3d(handle));
            }
        }
        None if build.is_empty() => {
            // Still nothing to show.
        }
        None => {
            let mesh_handle = meshes.add(to_bevy_mesh(build));
            let entity = commands
                .spawn((
                    Mesh3d(mesh_handle),
                    MeshMaterial3d(material.0.clone()),
                    Transform::from_translation(pos.as_vec3() * CHUNK_SIZE as f32),
                ))
                .id();
            store.entities.insert(pos, entity);
        }
    }
}

fn mesh_ready_chunks(
    mut commands: Commands,
    mut store: ResMut<ChunkStore>,
    mut meshes: ResMut<Assets<Mesh>>,
    material: Option<Res<ChunkMaterial>>,
    registry: Res<Registry>,
    cameras: Query<&Transform, With<Player>>,
    mesh_handles: Query<&Mesh3d>,
) {
    // The material is inserted by a Startup system; skip the very first
    // frame or two if it hasn't landed yet.
    let Some(material) = material else {
        return;
    };

    let cam_chunk = cameras
        .single()
        .map(|transform| world_pos_to_chunk(transform.translation))
        .unwrap_or(IVec3::ZERO);

    // Dirty (edited) chunks are served first, out of the same per-frame
    // budget, so edits feel instant even under a burst of newly-arrived
    // chunks.
    let mut dirty: Vec<IVec3> = store
        .dirty
        .iter()
        .copied()
        .filter(|&pos| is_ready_to_mesh(&store, pos))
        .collect();
    dirty.sort_by_key(|&pos| chunk_distance_sq(pos, cam_chunk));
    dirty.truncate(MESH_BUDGET_PER_FRAME);

    for pos in &dirty {
        store.dirty.remove(pos);
        remesh_chunk(
            &mut commands,
            &mut store,
            &mut meshes,
            &mesh_handles,
            &material,
            &registry.0,
            *pos,
        );
    }

    let remaining_budget = MESH_BUDGET_PER_FRAME.saturating_sub(dirty.len());
    if remaining_budget == 0 {
        return;
    }

    let mut candidates: Vec<IVec3> = store
        .chunks
        .keys()
        .copied()
        .filter(|&pos| !store.meshed.contains(&pos) && is_ready_to_mesh(&store, pos))
        .collect();
    candidates.sort_by_key(|&pos| chunk_distance_sq(pos, cam_chunk));
    candidates.truncate(remaining_budget);

    for pos in candidates {
        let build = build_ready_chunk(&store, pos, &registry.0);
        store.meshed.insert(pos);

        let Some(build) = build else { continue };
        if build.is_empty() {
            continue;
        }

        let mesh_handle = meshes.add(to_bevy_mesh(build));
        let entity = commands
            .spawn((
                Mesh3d(mesh_handle),
                MeshMaterial3d(material.0.clone()),
                Transform::from_translation(pos.as_vec3() * CHUNK_SIZE as f32),
            ))
            .id();
        store.entities.insert(pos, entity);
    }
}

fn despawn_far_chunks(
    mut commands: Commands,
    mut store: ResMut<ChunkStore>,
    mut meshes: ResMut<Assets<Mesh>>,
    mesh_handles: Query<&Mesh3d>,
    cameras: Query<&Transform, With<Player>>,
) {
    let Ok(transform) = cameras.single() else {
        return;
    };
    let cam_chunk = world_pos_to_chunk(transform.translation);
    let max_radius = VIEW_DISTANCE_CHUNKS + DESPAWN_MARGIN_CHUNKS;
    let max_radius_sq = max_radius * max_radius;

    let stale: Vec<IVec3> = store
        .chunks
        .keys()
        .copied()
        .filter(|pos| {
            let dx = pos.x - cam_chunk.x;
            let dz = pos.z - cam_chunk.z;
            dx * dx + dz * dz > max_radius_sq
        })
        .collect();

    for pos in stale {
        if let Some(entity) = store.entities.remove(&pos) {
            if let Ok(mesh3d) = mesh_handles.get(entity) {
                meshes.remove(&mesh3d.0);
            }
            commands.entity(entity).despawn();
        }
        forget_chunk(&mut store, pos);
    }
}

/// Removes `pos` from every position-keyed bookkeeping set except
/// `entities` (the caller despawns/removes the associated mesh entity and
/// asset first, since that needs `Commands`/`Assets<Mesh>`).
///
/// Clearing `requested` here matters: [`crate::net::request_chunks`] never
/// re-requests a position still marked `requested`, so forgetting a chunk
/// without also un-marking it would leave a permanent hole in the world for
/// a chunk the player walks away from and back to.
fn forget_chunk(store: &mut ChunkStore, pos: IVec3) {
    store.chunks.remove(&pos);
    store.requested.remove(&pos);
    store.meshed.remove(&pos);
    store.dirty.remove(&pos);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsumiki_world::blocks;

    fn store_with_chunk_at(pos: IVec3) -> ChunkStore {
        let mut store = ChunkStore::default();
        store.chunks.insert(pos, Chunk::filled(blocks::AIR));
        store
    }

    #[test]
    fn interior_edit_dirties_only_its_own_chunk() {
        let mut store = store_with_chunk_at(IVec3::ZERO);

        let dirtied = set_block(&mut store, IVec3::new(5, 5, 5), blocks::STONE);

        assert_eq!(dirtied, vec![IVec3::ZERO]);
        assert_eq!(store.dirty, HashSet::from([IVec3::ZERO]));
        assert_eq!(block_at(&store, IVec3::new(5, 5, 5)), Some(blocks::STONE));
    }

    #[test]
    fn edit_at_min_x_border_also_dirties_neg_x_neighbor() {
        let mut store = store_with_chunk_at(IVec3::ZERO);

        let dirtied = set_block(&mut store, IVec3::new(0, 5, 5), blocks::STONE);

        assert_eq!(dirtied.len(), 2);
        assert!(dirtied.contains(&IVec3::ZERO));
        assert!(dirtied.contains(&IVec3::NEG_X));
    }

    #[test]
    fn edit_at_max_x_border_also_dirties_pos_x_neighbor() {
        let mut store = store_with_chunk_at(IVec3::ZERO);
        let size = CHUNK_SIZE as i32;

        let dirtied = set_block(&mut store, IVec3::new(size - 1, 5, 5), blocks::STONE);

        assert_eq!(dirtied.len(), 2);
        assert!(dirtied.contains(&IVec3::ZERO));
        assert!(dirtied.contains(&IVec3::X));
    }

    #[test]
    fn corner_edit_dirties_all_three_adjacent_chunks() {
        let mut store = store_with_chunk_at(IVec3::ZERO);
        let size = CHUNK_SIZE as i32;

        // (x=0, y=0, z=size-1): a corner on the -X, -Y, +Z faces at once.
        let dirtied = set_block(&mut store, IVec3::new(0, 0, size - 1), blocks::STONE);

        assert_eq!(dirtied.len(), 4, "self + 3 adjacent chunks: {dirtied:?}");
        assert!(dirtied.contains(&IVec3::ZERO));
        assert!(dirtied.contains(&IVec3::NEG_X));
        assert!(dirtied.contains(&IVec3::NEG_Y));
        assert!(dirtied.contains(&IVec3::Z));
    }

    #[test]
    fn edit_on_unloaded_chunk_is_a_no_op() {
        let mut store = ChunkStore::default();

        let dirtied = set_block(&mut store, IVec3::new(5, 5, 5), blocks::STONE);

        assert!(dirtied.is_empty());
        assert!(store.dirty.is_empty());
    }

    #[test]
    fn block_at_outside_vertical_bounds_is_always_air() {
        let store = ChunkStore::default();
        let above_world = IVec3::new(0, tsumiki_world::WORLD_HEIGHT_BLOCKS + 10, 0);
        assert_eq!(block_at(&store, above_world), Some(blocks::AIR));
        assert!(is_chunk_loaded(&store, above_world));
    }

    #[test]
    fn block_at_in_unloaded_chunk_is_none() {
        let store = ChunkStore::default();
        assert_eq!(block_at(&store, IVec3::new(5, 5, 5)), None);
        assert!(!is_chunk_loaded(&store, IVec3::new(5, 5, 5)));
    }

    #[test]
    fn forgetting_a_chunk_clears_the_requested_set_so_it_can_be_re_requested() {
        // Regression test for a latent bug: despawning a far chunk without
        // also clearing `requested` would make `request_chunks` treat it as
        // still in flight forever, so walking back into range would never
        // re-request it.
        let pos = IVec3::new(3, 0, 3);
        let mut store = store_with_chunk_at(pos);
        store.requested.insert(pos);
        store.meshed.insert(pos);
        store.dirty.insert(pos);

        forget_chunk(&mut store, pos);

        assert!(!store.chunks.contains_key(&pos));
        assert!(
            !store.requested.contains(&pos),
            "a chunk walked back into view must be re-requestable"
        );
        assert!(!store.meshed.contains(&pos));
        assert!(!store.dirty.contains(&pos));
    }
}

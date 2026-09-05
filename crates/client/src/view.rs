//! Client-side chunk cache and mesh lifecycle.
//!
//! - `ChunkStore` resource: received chunks, the requested set, the mapping
//!   from chunk position to spawned mesh entity, and the dirty set of edited
//!   chunks awaiting remesh.
//! - Per frame, mesh up to a small budget of chunks, dirty (edited) chunks
//!   first so edits feel instant, then newly-arrived chunks nearest first. A
//!   chunk is ready to mesh when it and all six neighbors have blocks and
//!   lighting; a neighbor
//!   position outside the vertical world bounds counts as available (air).
//!   All-air chunks and empty mesh results spawn nothing but are marked
//!   meshed.
//! - Spawn mesh entities at `chunk_pos * CHUNK_SIZE` sharing a single
//!   voxel material (atlas tiles near, representative colors far). Dirty
//!   remeshes reuse the existing `Mesh` asset and entity where possible.
//! - [`set_block`] is the single edit entrypoint used by both local
//!   prediction ([`crate::interact`]) and server echoes ([`crate::net`]).
//! - Despawn (and forget) chunks that fall well outside the view distance.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use tsumiki_world::light::{LightChunk, LightValue};
use tsumiki_world::{BlockId, BlockRegistry, CHUNK_SIZE, Chunk, WORLD_HEIGHT_CHUNKS};

use crate::AppState;
use crate::camera::Player;
use crate::mesh::{MeshBuild, build_chunk_mesh_lit};
use crate::settings::Settings;
use crate::voxel_material::{VoxelLighting, VoxelMaterial};

/// Chunks meshed per frame. Kept small so a burst of newly-arrived chunks
/// doesn't spike a single frame's cost; doubled from 6 (matching the view
/// distance range's `4..=12` -> `4..=24` and `MAX_LOD` 3 -> 5 raises --
/// `crate::settings::VIEW_DISTANCE_RANGE`/`tsumiki_world::lod::MAX_LOD`'s doc
/// comments) so a much larger view distance still meshes in on the order of
/// seconds, not minutes, while staying small enough not to spike a frame.
const MESH_BUDGET_PER_FRAME: usize = 12;

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

static OPEN_SKY: LazyLock<LightChunk> = LazyLock::new(|| LightChunk::filled(LightValue::SKY));

/// The world's block registry, wrapped as a Bevy resource (the `world` crate
/// itself stays free of ECS dependencies).
///
/// Public so [`crate::camera`] and [`crate::interact`] can query block
/// solidity for physics and raycasting.
#[derive(Resource)]
pub struct Registry(pub BlockRegistry);

/// The single white, vertex-color-respecting material shared by every chunk
/// mesh. `pub(crate)` (with a `pub(crate)` field) so [`crate::lod_view`] can
/// share it for LOD chunk meshes instead of allocating a second material.
#[derive(Resource, Clone)]
pub(crate) struct ChunkMaterial(pub(crate) Handle<VoxelMaterial>);

/// Shared per-frame mesh budget (design.md M3): [`mesh_ready_chunks`]
/// (level-0, higher priority) always spends from a full [`MESH_BUDGET_PER_FRAME`]
/// first; whatever it doesn't use is what [`crate::lod_view`]'s mesher gets
/// to spend in the same frame, so a backlog of LOD work can never delay
/// level-0 meshing. Reset to the full budget at the start of every
/// `InGame` frame by [`reset_mesh_frame_budget`].
#[derive(Resource)]
pub(crate) struct MeshFrameBudget(pub(crate) usize);

impl Default for MeshFrameBudget {
    fn default() -> Self {
        Self(MESH_BUDGET_PER_FRAME)
    }
}

fn reset_mesh_frame_budget(mut budget: ResMut<MeshFrameBudget>) {
    budget.0 = MESH_BUDGET_PER_FRAME;
}

/// Client-side chunk cache and mesh bookkeeping.
///
/// - `chunks`: every chunk the server has sent, keyed by chunk position.
/// - `light`: compressed propagated RGB/skylight, received independently.
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
    pub light: HashMap<IVec3, LightChunk>,
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
    crate::voxel_material::install(app);
    app.insert_resource(Registry(registry))
        .init_resource::<ChunkStore>()
        .init_resource::<MeshFrameBudget>()
        .add_systems(OnEnter(AppState::InGame), setup_chunk_material)
        .add_systems(OnExit(AppState::InGame), teardown_chunks)
        .add_systems(
            Update,
            (
                reset_mesh_frame_budget,
                mesh_ready_chunks,
                despawn_far_chunks,
            )
                .chain()
                .run_if(in_state(AppState::InGame)),
        );
}

fn setup_chunk_material(
    mut commands: Commands,
    mut materials: ResMut<Assets<VoxelMaterial>>,
    asset_server: Res<AssetServer>,
) {
    let handle = materials.add(VoxelMaterial {
        base: StandardMaterial {
            base_color: Color::WHITE,
            unlit: true,
            perceptual_roughness: 1.0,
            ..default()
        },
        extension: VoxelLighting {
            sunlight: Vec4::ONE,
            atlas: asset_server
                .load_builder()
                .with_settings(|settings: &mut bevy::image::ImageLoaderSettings| {
                    settings.sampler = bevy::image::ImageSampler::nearest();
                })
                .load("atlas.png"),
        },
    });
    commands.insert_resource(ChunkMaterial(handle));
}

/// New lighting can alter faces in this chunk and all six neighbors. Dirty
/// chunks share the normal remesh budget, so a server batch stays bounded.
pub fn insert_light_chunk(store: &mut ChunkStore, pos: IVec3, light: LightChunk) {
    if (!store.chunks.contains_key(&pos) && !store.requested.contains(&pos))
        || store.light.get(&pos) == Some(&light)
    {
        return;
    }
    store.light.insert(pos, light);
    store.dirty.insert(pos);
    for offset in NEIGHBOR_OFFSETS {
        if store.chunks.contains_key(&(pos + offset)) {
            store.dirty.insert(pos + offset);
        }
    }
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
    !is_vertically_in_bounds(pos.y)
        || (store.chunks.contains_key(&pos) && store.light.contains_key(&pos))
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
        && store.light.contains_key(&pos)
        && NEIGHBOR_OFFSETS
            .iter()
            .all(|&offset| neighbor_present(store, pos + offset))
}

/// True when some cached chunk is ready to mesh but hasn't been yet. Used by
/// [`crate::screenshot`] to detect that the initial view has settled.
pub fn any_chunk_ready(store: &ChunkStore) -> bool {
    store.chunks.keys().any(|&pos| {
        (store.dirty.contains(&pos) || !store.meshed.contains(&pos)) && is_ready_to_mesh(store, pos)
    })
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
    let light_neighbors = NEIGHBOR_OFFSETS.map(|offset| {
        let neighbor = pos + offset;
        if neighbor.y >= WORLD_HEIGHT_CHUNKS {
            Some(&*OPEN_SKY)
        } else {
            store.light.get(&neighbor)
        }
    });
    Some(build_chunk_mesh_lit(
        chunk,
        neighbors,
        registry,
        store.light.get(&pos),
        light_neighbors,
    ))
}

/// `pub(crate)` so [`crate::lod_view`] can reuse it for LOD chunk meshes.
pub(crate) fn to_bevy_mesh(build: MeshBuild) -> Mesh {
    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::RENDER_WORLD,
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, build.positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, build.normals);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, build.colors);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, build.light_uvs);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_1, build.texture_uvs);
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

/// `pub(crate)` so [`crate::lod_view`] can order its own (lower-priority)
/// mesher after this one and read what's left of [`MeshFrameBudget`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn mesh_ready_chunks(
    mut commands: Commands,
    mut store: ResMut<ChunkStore>,
    mut meshes: ResMut<Assets<Mesh>>,
    material: Option<Res<ChunkMaterial>>,
    registry: Res<Registry>,
    mut frame_budget: ResMut<MeshFrameBudget>,
    cameras: Query<&Transform, With<Player>>,
    mesh_handles: Query<&Mesh3d>,
) {
    // The material is inserted by a Startup system; skip the very first
    // frame or two if it hasn't landed yet. The full budget stays available
    // for LOD meshing in this case (level-0 didn't spend any of it).
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
    let candidates: Vec<IVec3> = if remaining_budget == 0 {
        Vec::new()
    } else {
        let mut candidates: Vec<IVec3> = store
            .chunks
            .keys()
            .copied()
            .filter(|&pos| !store.meshed.contains(&pos) && is_ready_to_mesh(&store, pos))
            .collect();
        candidates.sort_by_key(|&pos| chunk_distance_sq(pos, cam_chunk));
        candidates.truncate(remaining_budget);
        candidates
    };

    for &pos in &candidates {
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

    // Whatever this frame's fixed budget didn't use is what LOD meshing
    // (lower priority, see `crate::lod_view`) gets to spend, in the same
    // frame.
    frame_budget.0 = remaining_budget.saturating_sub(candidates.len());
}

fn despawn_far_chunks(
    mut commands: Commands,
    mut store: ResMut<ChunkStore>,
    mut meshes: ResMut<Assets<Mesh>>,
    mesh_handles: Query<&Mesh3d>,
    settings: Res<Settings>,
    cameras: Query<&Transform, With<Player>>,
) {
    let Ok(transform) = cameras.single() else {
        return;
    };
    let cam_chunk = world_pos_to_chunk(transform.translation);
    let max_radius = settings.view_distance_chunks + DESPAWN_MARGIN_CHUNKS;
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

/// Part of the `OnExit(AppState::InGame)` "despawn everything in-game"
/// contract (see `pause` module docs): despawns every chunk mesh entity,
/// frees their `Mesh` assets and the shared chunk material, and fully clears
/// [`ChunkStore`] so a fresh session starts from nothing (fresh chunks
/// re-requested, nothing stale left meshed/dirty/requested).
fn teardown_chunks(
    mut commands: Commands,
    mut store: ResMut<ChunkStore>,
    mut meshes: ResMut<Assets<Mesh>>,
    mesh_handles: Query<&Mesh3d>,
    material: Option<Res<ChunkMaterial>>,
    mut materials: ResMut<Assets<VoxelMaterial>>,
) {
    for (_, entity) in store.entities.drain() {
        if let Ok(mesh3d) = mesh_handles.get(entity) {
            meshes.remove(&mesh3d.0);
        }
        commands.entity(entity).despawn();
    }
    store.chunks.clear();
    store.light.clear();
    store.requested.clear();
    store.meshed.clear();
    store.dirty.clear();

    if let Some(material) = material {
        materials.remove(&material.0);
        commands.remove_resource::<ChunkMaterial>();
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
    store.light.remove(&pos);
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
        store.light.insert(
            pos,
            LightChunk::filled(tsumiki_world::light::LightValue::SKY),
        );

        forget_chunk(&mut store, pos);

        assert!(!store.chunks.contains_key(&pos));
        assert!(
            !store.requested.contains(&pos),
            "a chunk walked back into view must be re-requestable"
        );
        assert!(!store.meshed.contains(&pos));
        assert!(!store.dirty.contains(&pos));
        assert!(!store.light.contains_key(&pos));
    }

    #[test]
    fn arriving_light_invalidates_center_and_loaded_neighbor_meshes() {
        let mut store = store_with_chunk_at(IVec3::ZERO);
        store.chunks.insert(IVec3::X, Chunk::filled(blocks::STONE));
        store.meshed.extend([IVec3::ZERO, IVec3::X]);
        insert_light_chunk(
            &mut store,
            IVec3::ZERO,
            LightChunk::filled(tsumiki_world::light::LightValue::SKY),
        );
        assert_eq!(store.dirty, HashSet::from([IVec3::ZERO, IVec3::X]));
    }

    #[test]
    fn ready_chunk_waits_for_light_in_all_loaded_neighbors() {
        let mut store = store_with_chunk_at(IVec3::ZERO);
        for offset in NEIGHBOR_OFFSETS {
            if is_vertically_in_bounds(offset.y) {
                store.chunks.insert(offset, Chunk::filled(blocks::AIR));
                store.light.insert(
                    offset,
                    LightChunk::filled(tsumiki_world::light::LightValue::SKY),
                );
            }
        }
        assert!(!is_ready_to_mesh(&store, IVec3::ZERO));
        store.light.insert(
            IVec3::ZERO,
            LightChunk::filled(tsumiki_world::light::LightValue::SKY),
        );
        assert!(is_ready_to_mesh(&store, IVec3::ZERO));
        store.light.remove(&IVec3::X);
        assert!(!is_ready_to_mesh(&store, IVec3::ZERO));
    }

    #[test]
    fn highest_world_layer_receives_open_sky_on_its_upper_face() {
        let pos = IVec3::new(0, WORLD_HEIGHT_CHUNKS - 1, 0);
        let mut store = store_with_chunk_at(pos);
        store
            .chunks
            .get_mut(&pos)
            .unwrap()
            .set(UVec3::new(16, CHUNK_SIZE as u32 - 1, 16), blocks::STONE);
        store
            .light
            .insert(pos, LightChunk::filled(LightValue::DARK));
        let build = build_ready_chunk(&store, pos, &BlockRegistry::prototype()).unwrap();
        for (normal, uv) in build.normals.iter().zip(&build.light_uvs) {
            if *normal == [0.0, 1.0, 0.0] {
                assert_eq!(*uv, [0.0, 1.0]);
            }
        }
    }
}

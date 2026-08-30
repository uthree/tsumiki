//! Client-side chunk cache and mesh lifecycle.
//!
//! Responsibilities (implemented by the client agent):
//! - `ChunkStore` resource: received chunks, the requested set, and the
//!   mapping from chunk position to spawned mesh entity.
//! - Per frame, mesh up to a small budget of chunks (nearest first). A chunk
//!   is ready to mesh when all six neighbors are available; a neighbor
//!   position outside the vertical world bounds counts as available (air).
//!   All-air chunks and empty mesh results spawn nothing but are marked
//!   meshed.
//! - Spawn mesh entities at `chunk_pos * CHUNK_SIZE` sharing a single
//!   white `StandardMaterial` (vertex colors carry the block colors).
//! - Despawn (and forget) chunks that fall well outside the view distance.

use std::collections::{HashMap, HashSet};

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use tsumiki_world::{BlockRegistry, CHUNK_SIZE, Chunk, WORLD_HEIGHT_CHUNKS};

use crate::camera::FlyCam;
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
#[derive(Resource)]
struct Registry(BlockRegistry);

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
#[derive(Resource, Default)]
pub struct ChunkStore {
    pub chunks: HashMap<IVec3, Chunk>,
    pub requested: HashSet<IVec3>,
    pub meshed: HashSet<IVec3>,
    pub entities: HashMap<IVec3, Entity>,
}

/// Wires the chunk cache, mesher and despawn systems into `app`, and inserts
/// `registry` as the (wrapped) block registry resource.
pub fn install(app: &mut App, registry: BlockRegistry) {
    app.insert_resource(Registry(registry))
        .init_resource::<ChunkStore>()
        .add_systems(Startup, setup_chunk_material)
        .add_systems(Update, (mesh_ready_chunks, despawn_far_chunks));
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
fn build_ready_chunk(store: &ChunkStore, pos: IVec3, registry: &BlockRegistry) -> Option<MeshBuild> {
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
    let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD);
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, build.positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, build.normals);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, build.colors);
    mesh.insert_indices(Indices::U32(build.indices));
    mesh
}

fn mesh_ready_chunks(
    mut commands: Commands,
    mut store: ResMut<ChunkStore>,
    mut meshes: ResMut<Assets<Mesh>>,
    material: Option<Res<ChunkMaterial>>,
    registry: Res<Registry>,
    cameras: Query<&Transform, With<FlyCam>>,
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

    let mut candidates: Vec<IVec3> = store
        .chunks
        .keys()
        .copied()
        .filter(|&pos| !store.meshed.contains(&pos) && is_ready_to_mesh(&store, pos))
        .collect();
    candidates.sort_by_key(|&pos| chunk_distance_sq(pos, cam_chunk));
    candidates.truncate(MESH_BUDGET_PER_FRAME);

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
    cameras: Query<&Transform, With<FlyCam>>,
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
            commands.entity(entity).despawn();
        }
        store.chunks.remove(&pos);
        store.requested.remove(&pos);
        store.meshed.remove(&pos);
    }
}

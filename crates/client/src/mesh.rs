//! Greedy meshing of chunks into render-ready buffers.
//!
//! Pure function of the input data — no Bevy ECS types here except mesh
//! building primitives, so it stays unit-testable.

use bevy::math::{IVec3, UVec3};
use bevy::prelude::Color;
use tsumiki_world::{BlockDef, BlockId, BlockRegistry, CHUNK_SIZE, Chunk};

/// Face directions, also the order of the `neighbors` parameter of
/// [`build_chunk_mesh`]: `[-X, +X, -Y, +Y, -Z, +Z]`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Face {
    NegX,
    PosX,
    NegY,
    PosY,
    NegZ,
    PosZ,
}

impl Face {
    const ALL: [Face; 6] = [
        Face::NegX,
        Face::PosX,
        Face::NegY,
        Face::PosY,
        Face::NegZ,
        Face::PosZ,
    ];

    fn normal(self) -> [f32; 3] {
        match self {
            Face::NegX => [-1.0, 0.0, 0.0],
            Face::PosX => [1.0, 0.0, 0.0],
            Face::NegY => [0.0, -1.0, 0.0],
            Face::PosY => [0.0, 1.0, 0.0],
            Face::NegZ => [0.0, 0.0, -1.0],
            Face::PosZ => [0.0, 0.0, 1.0],
        }
    }

    /// Index into `build_chunk_mesh`'s `neighbors` array for the chunk lying
    /// beyond this face.
    fn neighbor_index(self) -> usize {
        match self {
            Face::NegX => 0,
            Face::PosX => 1,
            Face::NegY => 2,
            Face::PosY => 3,
            Face::NegZ => 4,
            Face::PosZ => 5,
        }
    }
}

/// CPU-side mesh buffers for one chunk.
#[derive(Default, Debug)]
pub struct MeshBuild {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    /// Linear RGBA vertex colors.
    pub colors: Vec<[f32; 4]>,
    pub indices: Vec<u32>,
}

impl MeshBuild {
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }
}

/// Builds a greedy-meshed chunk mesh.
///
/// - `neighbors` order is `[-X, +X, -Y, +Y, -Z, +Z]`; `None` is treated as
///   all-air (faces on that border are emitted).
/// - A face is emitted when the block is opaque and the adjacent block is
///   not opaque. Same-type adjacent faces within a plane are merged into
///   maximal rectangles (greedy meshing).
/// - Positions are in chunk-local space (`0.0..=32.0`); the caller places
///   the chunk entity at `chunk_pos * 32`.
/// - Vertex colors come from the registry's per-face placeholder colors
///   (sRGB u8), converted to linear RGBA.
/// - Winding: counter-clockwise seen from outside the block (Bevy front
///   face), normals axis-aligned per face.
pub fn build_chunk_mesh(
    chunk: &Chunk,
    neighbors: [Option<&Chunk>; 6],
    registry: &BlockRegistry,
) -> MeshBuild {
    let mut build = MeshBuild::default();

    if chunk.is_all_air() {
        return build;
    }

    let size = CHUNK_SIZE as i32;
    let mut mask: Vec<Option<BlockId>> = vec![None; (size * size) as usize];

    for face in Face::ALL {
        for layer in 0..size {
            mask.iter_mut().for_each(|slot| *slot = None);

            for j in 0..size {
                for i in 0..size {
                    let (cur, adj) = face_sample_positions(face, layer, i, j);
                    let cur_block = sample_block(chunk, &neighbors, cur);
                    if !is_opaque(registry, cur_block) {
                        continue;
                    }
                    let adj_block = sample_block(chunk, &neighbors, adj);
                    if is_opaque(registry, adj_block) {
                        continue;
                    }
                    mask[(j * size + i) as usize] = Some(cur_block);
                }
            }

            merge_mask_into(&mut mask, size, face, layer, registry, &mut build);
        }
    }

    build
}

/// Returns `(current, adjacent)` chunk-local positions (in the extended
/// `-1..=size` range) for the block owning a candidate face at `layer` and
/// mask coordinates `(i, j)`, per [`Face`]'s `(i, j)` axis convention:
/// X faces use `(y, z)`, Y faces use `(x, z)`, Z faces use `(x, y)`.
fn face_sample_positions(face: Face, layer: i32, i: i32, j: i32) -> (IVec3, IVec3) {
    match face {
        Face::NegX => (IVec3::new(layer, i, j), IVec3::new(layer - 1, i, j)),
        Face::PosX => (IVec3::new(layer, i, j), IVec3::new(layer + 1, i, j)),
        Face::NegY => (IVec3::new(i, layer, j), IVec3::new(i, layer - 1, j)),
        Face::PosY => (IVec3::new(i, layer, j), IVec3::new(i, layer + 1, j)),
        Face::NegZ => (IVec3::new(i, j, layer), IVec3::new(i, j, layer - 1)),
        Face::PosZ => (IVec3::new(i, j, layer), IVec3::new(i, j, layer + 1)),
    }
}

/// Samples a block at a possibly out-of-range (by exactly one axis, by
/// exactly one step) chunk-local position, falling back to the appropriate
/// neighbor chunk's opposite border, or air when that neighbor is absent.
fn sample_block(chunk: &Chunk, neighbors: &[Option<&Chunk>; 6], pos: IVec3) -> BlockId {
    let size = CHUNK_SIZE as i32;
    let in_range = |v: i32| (0..size).contains(&v);

    if in_range(pos.x) && in_range(pos.y) && in_range(pos.z) {
        return chunk.get(UVec3::new(pos.x as u32, pos.y as u32, pos.z as u32));
    }

    let (idx, local) = if pos.x < 0 {
        (Face::NegX, IVec3::new(size - 1, pos.y, pos.z))
    } else if pos.x >= size {
        (Face::PosX, IVec3::new(0, pos.y, pos.z))
    } else if pos.y < 0 {
        (Face::NegY, IVec3::new(pos.x, size - 1, pos.z))
    } else if pos.y >= size {
        (Face::PosY, IVec3::new(pos.x, 0, pos.z))
    } else if pos.z < 0 {
        (Face::NegZ, IVec3::new(pos.x, pos.y, size - 1))
    } else {
        (Face::PosZ, IVec3::new(pos.x, pos.y, 0))
    };

    match neighbors[idx.neighbor_index()] {
        Some(neighbor) => neighbor.get(UVec3::new(local.x as u32, local.y as u32, local.z as u32)),
        None => BlockId::AIR,
    }
}

fn is_opaque(registry: &BlockRegistry, id: BlockId) -> bool {
    registry.get(id).opaque
}

fn face_color(def: &BlockDef, face: Face) -> [f32; 4] {
    let rgb = match face {
        Face::PosY => def.color_top,
        Face::NegY => def.color_bottom,
        Face::NegX | Face::PosX | Face::NegZ | Face::PosZ => def.color_side,
    };
    let linear = Color::srgb_u8(rgb[0], rgb[1], rgb[2]).to_linear();
    [linear.red, linear.green, linear.blue, linear.alpha]
}

/// Greedily merges a `size x size` visibility mask (indexed `j * size + i`,
/// consumed in place) into maximal same-block rectangles and emits one quad
/// per rectangle.
fn merge_mask_into(
    mask: &mut [Option<BlockId>],
    size: i32,
    face: Face,
    layer: i32,
    registry: &BlockRegistry,
    build: &mut MeshBuild,
) {
    let idx = |i: i32, j: i32| (j * size + i) as usize;

    for j in 0..size {
        let mut i = 0;
        while i < size {
            let Some(block) = mask[idx(i, j)] else {
                i += 1;
                continue;
            };

            let mut w = 1;
            while i + w < size && mask[idx(i + w, j)] == Some(block) {
                w += 1;
            }

            let mut h = 1;
            'grow: while j + h < size {
                for k in 0..w {
                    if mask[idx(i + k, j + h)] != Some(block) {
                        break 'grow;
                    }
                }
                h += 1;
            }

            for dj in 0..h {
                for di in 0..w {
                    mask[idx(i + di, j + dj)] = None;
                }
            }

            emit_quad(build, registry, face, layer, i, i + w, j, j + h, block);
            i += w;
        }
    }
}

/// Emits one quad spanning mask rectangle `[i0, i1) x [j0, j1)` at `layer`.
#[allow(clippy::too_many_arguments)]
fn emit_quad(
    build: &mut MeshBuild,
    registry: &BlockRegistry,
    face: Face,
    layer: i32,
    i0: i32,
    i1: i32,
    j0: i32,
    j1: i32,
    block: BlockId,
) {
    let def = registry.get(block);
    let color = face_color(def, face);
    let normal = face.normal();
    let corners = face_corners(face, layer, i0, i1, j0, j1);

    let base = build.positions.len() as u32;
    for corner in corners {
        build.positions.push(corner);
        build.normals.push(normal);
        build.colors.push(color);
    }
    build
        .indices
        .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

/// Returns the 4 quad corners in winding order such that
/// `(v1 - v0) x (v2 - v0)` points along the face normal (CCW seen from
/// outside the block).
fn face_corners(face: Face, layer: i32, i0: i32, i1: i32, j0: i32, j1: i32) -> [[f32; 3]; 4] {
    let (i0, i1, j0, j1) = (i0 as f32, i1 as f32, j0 as f32, j1 as f32);
    match face {
        Face::NegX => {
            let x = layer as f32;
            [[x, i0, j0], [x, i0, j1], [x, i1, j1], [x, i1, j0]]
        }
        Face::PosX => {
            let x = (layer + 1) as f32;
            [[x, i0, j0], [x, i1, j0], [x, i1, j1], [x, i0, j1]]
        }
        Face::NegY => {
            let y = layer as f32;
            [[i0, y, j0], [i1, y, j0], [i1, y, j1], [i0, y, j1]]
        }
        Face::PosY => {
            let y = (layer + 1) as f32;
            [[i0, y, j0], [i0, y, j1], [i1, y, j1], [i1, y, j0]]
        }
        Face::NegZ => {
            let z = layer as f32;
            [[i0, j0, z], [i0, j1, z], [i1, j1, z], [i1, j0, z]]
        }
        Face::PosZ => {
            let z = (layer + 1) as f32;
            [[i0, j0, z], [i1, j0, z], [i1, j1, z], [i0, j1, z]]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::math::Vec3;
    use tsumiki_world::blocks;

    fn empty_neighbors() -> [Option<&'static Chunk>; 6] {
        [None, None, None, None, None, None]
    }

    #[test]
    fn single_block_emits_six_unmerged_quads() {
        let registry = BlockRegistry::prototype();
        let mut chunk = Chunk::filled(blocks::AIR);
        chunk.set(UVec3::new(16, 16, 16), blocks::STONE);

        let mesh = build_chunk_mesh(&chunk, empty_neighbors(), &registry);

        assert_eq!(mesh.positions.len(), 24);
        assert_eq!(mesh.indices.len(), 36);

        let normal_sum = mesh
            .normals
            .iter()
            .fold(Vec3::ZERO, |acc, n| acc + Vec3::from(*n));
        assert_eq!(normal_sum, Vec3::ZERO);

        for expected in [
            Face::NegX.normal(),
            Face::PosX.normal(),
            Face::NegY.normal(),
            Face::PosY.normal(),
            Face::NegZ.normal(),
            Face::PosZ.normal(),
        ] {
            assert!(
                mesh.normals.contains(&expected),
                "missing face with normal {expected:?}"
            );
        }
    }

    #[test]
    fn fully_filled_chunk_merges_each_face_into_one_quad() {
        let registry = BlockRegistry::prototype();
        let chunk = Chunk::filled(blocks::STONE);

        let mesh = build_chunk_mesh(&chunk, empty_neighbors(), &registry);

        // 6 faces, each merged to a single 32x32 quad: 6*4 verts, 6*6 indices.
        assert_eq!(mesh.positions.len(), 24);
        assert_eq!(mesh.indices.len(), 36);
    }

    #[test]
    fn fully_enclosed_chunk_produces_empty_mesh() {
        let registry = BlockRegistry::prototype();
        let chunk = Chunk::filled(blocks::STONE);
        let neighbor = Chunk::filled(blocks::STONE);
        let neighbors = [
            Some(&neighbor),
            Some(&neighbor),
            Some(&neighbor),
            Some(&neighbor),
            Some(&neighbor),
            Some(&neighbor),
        ];

        let mesh = build_chunk_mesh(&chunk, neighbors, &registry);

        assert!(mesh.is_empty());
        assert!(mesh.positions.is_empty());
    }

    #[test]
    fn adjacent_same_type_blocks_cull_shared_face_and_merge_outer_faces() {
        // Two touching stone blocks form a solid 2x1x1 box: full greedy
        // meshing (required for the slab test below) merges each of the box's
        // 6 outer faces into one quad, in addition to culling the 2 internal
        // faces where the blocks touch. Naive per-block face culling alone
        // (12 faces - 2 shared = 10) undercounts this merging; 6 is correct
        // for a mesher that does full 2D rectangle merging per face.
        let registry = BlockRegistry::prototype();
        let mut chunk = Chunk::filled(blocks::AIR);
        chunk.set(UVec3::new(16, 16, 16), blocks::STONE);
        chunk.set(UVec3::new(17, 16, 16), blocks::STONE);

        let mesh = build_chunk_mesh(&chunk, empty_neighbors(), &registry);

        assert_eq!(mesh.positions.len(), 24, "expected 6 merged quads");
        assert_eq!(mesh.indices.len(), 36);

        // The shared internal faces (PosX of the first block, NegX of the
        // second) must not appear: exactly one NegX and one PosX quad exist.
        let neg_x_count = mesh
            .normals
            .iter()
            .filter(|n| **n == Face::NegX.normal())
            .count();
        let pos_x_count = mesh
            .normals
            .iter()
            .filter(|n| **n == Face::PosX.normal())
            .count();
        assert_eq!(neg_x_count, 4, "one merged NegX quad (4 vertices)");
        assert_eq!(pos_x_count, 4, "one merged PosX quad (4 vertices)");
    }

    #[test]
    fn slab_top_face_is_a_single_quad() {
        let registry = BlockRegistry::prototype();
        let mut chunk = Chunk::filled(blocks::AIR);
        for x in 0..32u32 {
            for z in 0..32u32 {
                chunk.set(UVec3::new(x, 0, z), blocks::STONE);
            }
        }

        let mesh = build_chunk_mesh(&chunk, empty_neighbors(), &registry);

        let top_quads = mesh
            .normals
            .iter()
            .filter(|n| **n == Face::PosY.normal())
            .count()
            / 4;
        assert_eq!(top_quads, 1, "the 32x32 top face must merge to one quad");

        let top_positions: Vec<Vec3> = mesh
            .positions
            .iter()
            .zip(mesh.normals.iter())
            .filter(|(_, n)| **n == Face::PosY.normal())
            .map(|(p, _)| Vec3::from(*p))
            .collect();
        for expected in [
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(32.0, 1.0, 0.0),
            Vec3::new(32.0, 1.0, 32.0),
            Vec3::new(0.0, 1.0, 32.0),
        ] {
            assert!(
                top_positions.contains(&expected),
                "missing corner {expected:?} in {top_positions:?}"
            );
        }
    }

    #[test]
    fn quad_winding_matches_normal_via_right_hand_rule() {
        let registry = BlockRegistry::prototype();
        let mut chunk = Chunk::filled(blocks::AIR);
        chunk.set(UVec3::new(16, 16, 16), blocks::STONE);

        let mesh = build_chunk_mesh(&chunk, empty_neighbors(), &registry);

        for (quad_idx, verts) in mesh.positions.chunks(4).enumerate() {
            let v0 = Vec3::from(verts[0]);
            let v1 = Vec3::from(verts[1]);
            let v2 = Vec3::from(verts[2]);
            let normal = Vec3::from(mesh.normals[quad_idx * 4]);

            let cross = (v1 - v0).cross(v2 - v0);
            assert!(
                cross.normalize().dot(normal) > 0.99,
                "quad {quad_idx}: winding cross {cross:?} does not match normal {normal:?}"
            );
        }
    }

    #[test]
    fn different_block_types_do_not_merge() {
        let registry = BlockRegistry::prototype();
        let mut chunk = Chunk::filled(blocks::AIR);
        chunk.set(UVec3::new(16, 16, 16), blocks::STONE);
        chunk.set(UVec3::new(17, 16, 16), blocks::DIRT);

        let mesh = build_chunk_mesh(&chunk, empty_neighbors(), &registry);

        // Top faces of the two blocks are coplanar and adjacent but must
        // stay as two separate 1x1 quads since the block ids differ.
        let top_quads = mesh
            .normals
            .iter()
            .filter(|n| **n == Face::PosY.normal())
            .count()
            / 4;
        assert_eq!(top_quads, 2, "different block ids must not merge");
    }
}

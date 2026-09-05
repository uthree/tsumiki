//! Greedy meshing of chunks into render-ready buffers.
//!
//! Pure function of the input data — no Bevy ECS types here except mesh
//! building primitives, so it stays unit-testable.

use bevy::color::ColorToComponents;
use bevy::math::{IVec3, UVec3};
use bevy::prelude::Color;
use tsumiki_world::light::{LightChunk, LightValue};
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
    /// RGB packed into UV.x, normalized skylight in UV.y.
    pub light_uvs: Vec<[f32; 2]>,
    /// Atlas tile index and shape mapping mode. A negative tile keeps LOD flat.
    pub texture_uvs: Vec<[f32; 2]>,
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
///   not opaque. Adjacent faces with matching block and light are merged into
///   maximal rectangles (greedy meshing).
/// - Positions are in chunk-local space (`0.0..=32.0`); the caller places
///   the chunk entity at `chunk_pos * 32`.
/// - This LOD entrypoint uses registry average face colors, converted from
///   sRGB to linear RGBA. Near terrain uses [`build_chunk_mesh_lit`].
/// - Winding: counter-clockwise seen from outside the block (Bevy front
///   face), normals axis-aligned per face.
pub fn build_chunk_mesh(
    chunk: &Chunk,
    neighbors: [Option<&Chunk>; 6],
    registry: &BlockRegistry,
) -> MeshBuild {
    build_chunk_mesh_lit(chunk, neighbors, registry, None, [None; 6])
}

/// Like [`build_chunk_mesh`], with propagated light sampled on the air side
/// of every face. Lit near terrain selects an atlas tile per face; the shader
/// repeats it in world space independently of greedy quad dimensions.
/// `None` uses full daylight and flat average colors for distant LOD meshes.
pub fn build_chunk_mesh_lit(
    chunk: &Chunk,
    neighbors: [Option<&Chunk>; 6],
    registry: &BlockRegistry,
    light: Option<&LightChunk>,
    light_neighbors: [Option<&LightChunk>; 6],
) -> MeshBuild {
    let mut build = MeshBuild::default();

    if chunk.is_all_air() {
        return build;
    }

    let size = CHUNK_SIZE as i32;
    let mut mask: Vec<Option<FaceKey>> = vec![None; (size * size) as usize];

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
                    mask[(j * size + i) as usize] = Some(FaceKey {
                        block: cur_block,
                        light: sample_light(light, &light_neighbors, adj),
                        textured: light.is_some(),
                    });
                }
            }

            merge_mask_into(&mut mask, size, face, layer, registry, &mut build);
        }
    }

    // Torches are nonopaque and need their own narrow geometry, rather than
    // entering the greedy cube mask or occluding their neighbors.
    for z in 0..CHUNK_SIZE as u32 {
        for y in 0..CHUNK_SIZE as u32 {
            for x in 0..CHUNK_SIZE as u32 {
                let pos = UVec3::new(x, y, z);
                if chunk.get(pos) == tsumiki_world::blocks::TORCH {
                    emit_torch(&mut build, pos, registry, light.is_some());
                }
            }
        }
    }

    build
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FaceKey {
    block: BlockId,
    light: LightValue,
    textured: bool,
}

/// Atlas generation and meshing share the face order documented on [`Face`].
fn face_tile(block: BlockId, face: Face) -> f32 {
    (usize::from(block.0) * Face::ALL.len() + face.neighbor_index()) as f32
}

fn light_uv(value: LightValue) -> [f32; 2] {
    [
        f32::from(value.rgb[0]) + f32::from(value.rgb[1]) * 16.0 + f32::from(value.rgb[2]) * 256.0,
        f32::from(value.sky) / 15.0,
    ]
}

/// The mesher only samples one face across a chunk boundary.
fn sample_light(
    light: Option<&LightChunk>,
    neighbors: &[Option<&LightChunk>; 6],
    pos: IVec3,
) -> LightValue {
    let Some(light) = light else {
        return LightValue::new([0; 3], 15);
    };
    let size = CHUNK_SIZE as i32;
    let local = pos.rem_euclid(IVec3::splat(size)).as_uvec3();
    let neighbor = if pos.x < 0 {
        Some(0)
    } else if pos.x >= size {
        Some(1)
    } else if pos.y < 0 {
        Some(2)
    } else if pos.y >= size {
        Some(3)
    } else if pos.z < 0 {
        Some(4)
    } else if pos.z >= size {
        Some(5)
    } else {
        None
    };
    match neighbor {
        Some(index) => neighbors[index].map_or(LightValue::default(), |chunk| chunk.get(local)),
        None => light.get(local),
    }
}

fn emit_torch(build: &mut MeshBuild, pos: UVec3, registry: &BlockRegistry, textured: bool) {
    let emission = registry.get(tsumiki_world::blocks::TORCH).light_emission;
    let uv = light_uv(LightValue::new(emission, 0));
    let offset = pos.as_vec3();
    for (min, max, color, texture_block, mapping_mode) in [
        (
            [0.43, 0.0, 0.43],
            [0.57, 0.62, 0.57],
            [112, 72, 34],
            tsumiki_world::blocks::LOG,
            1.0,
        ),
        (
            [0.40, 0.58, 0.40],
            [0.60, 0.82, 0.60],
            [255, 222, 122],
            tsumiki_world::blocks::TORCH,
            2.0,
        ),
    ] {
        let color = if textured {
            [1.0; 4]
        } else {
            Color::srgb_u8(color[0], color[1], color[2])
                .to_linear()
                .to_f32_array()
        };
        for face in Face::ALL {
            let base = build.positions.len() as u32;
            for unit in face_corners(face, 0, 0, 1, 0, 1) {
                build.positions.push([
                    offset.x + min[0] + unit[0] * (max[0] - min[0]),
                    offset.y + min[1] + unit[1] * (max[1] - min[1]),
                    offset.z + min[2] + unit[2] * (max[2] - min[2]),
                ]);
                build.normals.push(face.normal());
                build.colors.push(color);
                build.light_uvs.push(uv);
                build.texture_uvs.push(if textured {
                    [face_tile(texture_block, face), mapping_mode]
                } else {
                    [-1.0, 0.0]
                });
            }
            build
                .indices
                .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }
    }
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
/// consumed in place) into rectangles of matching block/light and emits one quad
/// per rectangle.
fn merge_mask_into(
    mask: &mut [Option<FaceKey>],
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
    key: FaceKey,
) {
    let def = registry.get(key.block);
    let color = if key.textured {
        [1.0; 4]
    } else {
        face_color(def, face)
    };
    let normal = face.normal();
    let corners = face_corners(face, layer, i0, i1, j0, j1);

    let base = build.positions.len() as u32;
    for corner in corners {
        build.positions.push(corner);
        build.normals.push(normal);
        build.colors.push(color);
        build.light_uvs.push(light_uv(key.light));
        build.texture_uvs.push(if key.textured {
            [face_tile(key.block, face), 0.0]
        } else {
            [-1.0, 0.0]
        });
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

    #[test]
    fn light_boundaries_split_greedy_faces_without_fragmenting_uniform_light() {
        let registry = BlockRegistry::prototype();
        let mut chunk = Chunk::filled(blocks::AIR);
        chunk.set(UVec3::new(16, 16, 16), blocks::STONE);
        chunk.set(UVec3::new(17, 16, 16), blocks::STONE);
        let mut light = LightChunk::filled(LightValue::SKY);
        let uniform = build_chunk_mesh_lit(
            &chunk,
            empty_neighbors(),
            &registry,
            Some(&light),
            [None; 6],
        );
        assert_eq!(uniform.indices.len() / 6, 6);
        light.set(UVec3::new(17, 17, 16), LightValue::new([12, 9, 5], 0));
        let split = build_chunk_mesh_lit(
            &chunk,
            empty_neighbors(),
            &registry,
            Some(&light),
            [None; 6],
        );
        assert_eq!(
            split.indices.len() / 6,
            7,
            "only the changed top face splits"
        );
        let top_light: Vec<_> = split
            .normals
            .iter()
            .zip(&split.light_uvs)
            .filter(|(normal, _)| **normal == Face::PosY.normal())
            .map(|(_, uv)| *uv)
            .collect();
        assert!(top_light.contains(&light_uv(LightValue::SKY)));
        assert!(top_light.contains(&light_uv(LightValue::new([12, 9, 5], 0))));
    }

    #[test]
    fn unlit_cave_faces_have_no_sky_or_rgb_light() {
        let registry = BlockRegistry::prototype();
        let mut chunk = Chunk::filled(blocks::AIR);
        chunk.set(UVec3::splat(16), blocks::STONE);
        let dark = LightChunk::filled(LightValue::DARK);
        let mesh =
            build_chunk_mesh_lit(&chunk, empty_neighbors(), &registry, Some(&dark), [None; 6]);
        assert!(!mesh.is_empty());
        assert!(mesh.light_uvs.iter().all(|uv| *uv == [0.0; 2]));
    }

    #[test]
    fn face_light_samples_the_adjacent_chunk_across_a_border() {
        let registry = BlockRegistry::prototype();
        let mut chunk = Chunk::filled(blocks::AIR);
        chunk.set(UVec3::new(31, 16, 16), blocks::STONE);
        let dark = LightChunk::filled(LightValue::DARK);
        let mut neighbor = LightChunk::filled(LightValue::DARK);
        let torch = LightValue::new([14, 11, 7], 0);
        neighbor.set(UVec3::new(0, 16, 16), torch);
        let mesh = build_chunk_mesh_lit(
            &chunk,
            empty_neighbors(),
            &registry,
            Some(&dark),
            [None, Some(&neighbor), None, None, None, None],
        );
        for (normal, uv) in mesh.normals.iter().zip(&mesh.light_uvs) {
            assert_eq!(
                *uv,
                if *normal == Face::PosX.normal() {
                    light_uv(torch)
                } else {
                    [0.0; 2]
                }
            );
        }
    }

    #[test]
    fn torch_geometry_is_narrow_upright_and_self_lit() {
        let registry = BlockRegistry::prototype();
        let mut chunk = Chunk::filled(blocks::AIR);
        chunk.set(UVec3::new(16, 16, 16), blocks::TORCH);
        let dark = LightChunk::filled(LightValue::DARK);
        let mesh =
            build_chunk_mesh_lit(&chunk, empty_neighbors(), &registry, Some(&dark), [None; 6]);
        assert_eq!(mesh.indices.len() / 6, 12, "shaft and glowing head");
        for pos in &mesh.positions {
            assert!((16.39..16.61).contains(&pos[0]));
            assert!((16.0..16.83).contains(&pos[1]));
            assert!((16.39..16.61).contains(&pos[2]));
        }
        let expected = light_uv(LightValue::new(
            registry.get(blocks::TORCH).light_emission,
            0,
        ));
        assert!(mesh.light_uvs.iter().all(|uv| *uv == expected));
    }

    #[test]
    fn textured_faces_select_the_matching_atlas_face_without_double_tinting() {
        let registry = BlockRegistry::prototype();
        let mut chunk = Chunk::filled(blocks::AIR);
        chunk.set(UVec3::splat(16), blocks::FURNACE);
        let light = LightChunk::filled(LightValue::SKY);
        let mesh = build_chunk_mesh_lit(
            &chunk,
            empty_neighbors(),
            &registry,
            Some(&light),
            [None; 6],
        );
        for (index, face) in Face::ALL.into_iter().enumerate() {
            assert_eq!(mesh.normals[index * 4], face.normal());
            let expected_tile = f32::from(blocks::FURNACE.0) * 6.0 + index as f32;
            for uv in &mesh.texture_uvs[index * 4..index * 4 + 4] {
                assert_eq!(*uv, [expected_tile, 0.0]);
            }
        }
        assert!(mesh.colors.iter().all(|color| *color == [1.0; 4]));
    }

    #[test]
    fn textured_greedy_slab_keeps_one_tile_id_across_its_full_block_span() {
        let registry = BlockRegistry::prototype();
        let mut chunk = Chunk::filled(blocks::AIR);
        for x in 0..CHUNK_SIZE as u32 {
            for z in 0..CHUNK_SIZE as u32 {
                chunk.set(UVec3::new(x, 0, z), blocks::PLANKS);
            }
        }
        let light = LightChunk::filled(LightValue::SKY);
        let mesh = build_chunk_mesh_lit(
            &chunk,
            empty_neighbors(),
            &registry,
            Some(&light),
            [None; 6],
        );
        let top: Vec<_> = mesh
            .normals
            .iter()
            .enumerate()
            .filter(|(_, normal)| **normal == Face::PosY.normal())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            top.len(),
            4,
            "a textured 32x32 surface stays one greedy quad"
        );
        for i in top {
            // The GPU projects each world position; metadata is a tile id,
            // rather than 0..1 texture corners that would stretch the tile.
            assert_eq!(
                mesh.texture_uvs[i],
                [face_tile(blocks::PLANKS, Face::PosY), 0.0]
            );
            assert!([0.0, CHUNK_SIZE as f32].contains(&mesh.positions[i][0]));
            assert!([0.0, CHUNK_SIZE as f32].contains(&mesh.positions[i][2]));
        }
        assert_eq!(mesh.indices.len(), 36);
    }

    #[test]
    fn distant_lod_meshes_keep_average_colors_with_atlas_sampling_disabled() {
        let registry = BlockRegistry::prototype();
        let mut chunk = Chunk::filled(blocks::AIR);
        chunk.set(UVec3::splat(16), blocks::GRASS);
        let mesh = build_chunk_mesh(&chunk, empty_neighbors(), &registry);
        assert!(mesh.texture_uvs.iter().all(|uv| *uv == [-1.0, 0.0]));
        for (index, face) in Face::ALL.into_iter().enumerate() {
            assert_eq!(
                mesh.colors[index * 4],
                face_color(registry.get(blocks::GRASS), face)
            );
        }
    }

    #[test]
    fn torch_uses_separate_wood_and_flame_tiles_with_shape_mapping() {
        let registry = BlockRegistry::prototype();
        let mut chunk = Chunk::filled(blocks::AIR);
        chunk.set(UVec3::splat(16), blocks::TORCH);
        let light = LightChunk::filled(LightValue::DARK);
        let mesh = build_chunk_mesh_lit(
            &chunk,
            empty_neighbors(),
            &registry,
            Some(&light),
            [None; 6],
        );
        assert_eq!(mesh.texture_uvs.len(), 48);
        for (index, face) in Face::ALL.into_iter().enumerate() {
            assert_eq!(
                mesh.texture_uvs[index * 4],
                [face_tile(blocks::LOG, face), 1.0]
            );
            assert_eq!(
                mesh.texture_uvs[24 + index * 4],
                [face_tile(blocks::TORCH, face), 2.0]
            );
        }
        assert!(mesh.colors.iter().all(|color| *color == [1.0; 4]));
    }

    #[test]
    #[ignore = "manual lighting fragmentation and meshing throughput probe"]
    fn lighting_mesh_probe() {
        use std::hint::black_box;
        use std::time::Instant;
        use tsumiki_world::light::{LightMaterial, solve_region};

        let registry = BlockRegistry::prototype();
        let generator = tsumiki_world::worldgen::WorldGenerator::new(42);
        for (label, pos) in [("surface", IVec3::new(0, 1, 0)), ("cave", IVec3::ZERO)] {
            let chunk = generator.generate_chunk(pos);
            let neighbors = [
                IVec3::NEG_X,
                IVec3::X,
                IVec3::NEG_Y,
                IVec3::Y,
                IVec3::NEG_Z,
                IVec3::Z,
            ]
            .map(|offset| generator.generate_chunk(pos + offset));
            let neighbors = neighbors.each_ref().map(Some);
            let values = solve_region(UVec3::splat(CHUNK_SIZE as u32), |local| {
                let def = registry.get(chunk.get(local));
                LightMaterial {
                    opacity: def.light_opacity,
                    emission: def.light_emission,
                }
            });
            let light = LightChunk::from_packed(&values);
            let unlit = build_chunk_mesh(&chunk, neighbors, &registry);
            let lit = build_chunk_mesh_lit(&chunk, neighbors, &registry, Some(&light), [None; 6]);
            let started = Instant::now();
            for _ in 0..100 {
                black_box(build_chunk_mesh_lit(
                    &chunk,
                    neighbors,
                    &registry,
                    Some(&light),
                    [None; 6],
                ));
            }
            eprintln!(
                "{label}: uniform_quads={} lit_quads={} mesh_us={:.0}",
                unlit.indices.len() / 6,
                lit.indices.len() / 6,
                started.elapsed().as_micros() as f64 / 100.0
            );
        }
    }
}

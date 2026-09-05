//! Derived voxel lighting, independent of the persisted block representation.
//!
//! Block light has three four-bit channels; skylight has one. Direct sky
//! travels down a column without decaying through air. All flood propagation
//! loses at least one level per step, so a 15-block horizontal halo is enough
//! to solve a full-height column without depending on loaded neighbors.

use std::collections::{HashMap, VecDeque};

use bevy_math::UVec3;
use serde::{Deserialize, Serialize};

use crate::chunk::{CHUNK_SIZE, CHUNK_VOLUME};

/// Unpacked channels, each in `0..=15`. Storage uses [`Self::packed`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct LightValue {
    pub rgb: [u8; 3],
    pub sky: u8,
}

impl LightValue {
    pub const DARK: Self = Self::new([0; 3], 0);
    pub const SKY: Self = Self::new([0; 3], 15);

    pub const fn new(rgb: [u8; 3], sky: u8) -> Self {
        Self { rgb, sky }
    }

    pub const fn packed(self) -> u16 {
        (self.rgb[0] as u16 & 15)
            | ((self.rgb[1] as u16 & 15) << 4)
            | ((self.rgb[2] as u16 & 15) << 8)
            | ((self.sky as u16 & 15) << 12)
    }

    pub const fn from_packed(value: u16) -> Self {
        Self {
            rgb: [
                (value & 15) as u8,
                ((value >> 4) & 15) as u8,
                ((value >> 8) & 15) as u8,
            ],
            sky: ((value >> 12) & 15) as u8,
        }
    }
}

/// Palette/RLE-compressed 32³ lighting. Runs store exclusive end indices and
/// palette indices; uniform air or underground chunks occupy one run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LightChunk {
    palette: Vec<u16>,
    runs: Vec<(u32, u16)>,
}

impl LightChunk {
    pub fn filled(value: LightValue) -> Self {
        Self {
            palette: vec![value.packed()],
            runs: vec![(CHUNK_VOLUME as u32, 0)],
        }
    }

    /// Packs values in `(y * 32 + z) * 32 + x` order.
    pub fn from_packed(values: &[u16]) -> Self {
        assert_eq!(values.len(), CHUNK_VOLUME);
        let mut palette = Vec::new();
        let mut indices = HashMap::new();
        let mut runs: Vec<(u32, u16)> = Vec::new();
        for (i, &value) in values.iter().enumerate() {
            let palette_index = *indices.entry(value).or_insert_with(|| {
                let index = palette.len() as u16;
                palette.push(value);
                index
            });
            if let Some(last) = runs.last_mut()
                && last.1 == palette_index
            {
                last.0 = i as u32 + 1;
            } else {
                runs.push((i as u32 + 1, palette_index));
            }
        }
        Self { palette, runs }
    }

    pub fn get(&self, local: UVec3) -> LightValue {
        debug_assert!(local.cmplt(UVec3::splat(CHUNK_SIZE as u32)).all());
        let index = (local.y * CHUNK_SIZE as u32 + local.z) * CHUNK_SIZE as u32 + local.x;
        let run = self.runs.partition_point(|&(end, _)| end <= index);
        LightValue::from_packed(self.palette[self.runs[run].1 as usize])
    }

    /// Expands storage for repeated bulk sampling or editing.
    pub fn unpack(&self) -> Vec<LightValue> {
        let mut result = Vec::with_capacity(CHUNK_VOLUME);
        for &(end, palette_index) in &self.runs {
            result.resize(
                end as usize,
                LightValue::from_packed(self.palette[palette_index as usize]),
            );
        }
        result
    }

    /// Rebuilds compression after one edit. Bulk producers should instead
    /// use [`Self::from_packed`] once after solving their dense buffer.
    pub fn set(&mut self, local: UVec3, value: LightValue) {
        let mut values: Vec<u16> = self.unpack().into_iter().map(LightValue::packed).collect();
        let index =
            (local.y as usize * CHUNK_SIZE + local.z as usize) * CHUNK_SIZE + local.x as usize;
        values[index] = value.packed();
        *self = Self::from_packed(&values);
    }
}

/// The only block properties the propagation engine consumes.
#[derive(Clone, Copy, Debug, Default)]
pub struct LightMaterial {
    pub opacity: u8,
    pub emission: [u8; 3],
}

/// Solves a rectangular region with open sky above it and opaque side/bottom
/// boundaries. Callers retaining an interior column provide a 15-block halo
/// on each horizontal side. Light is rebuilt from sources, so removing a
/// source or closing a shaft cannot leave stale light behind.
pub fn solve_region(size: UVec3, mut material_at: impl FnMut(UVec3) -> LightMaterial) -> Vec<u16> {
    let width = size.x as usize;
    let depth = size.z as usize;
    let height = size.y as usize;
    let layer = width * depth;
    let volume = layer * height;
    let mut opacity = vec![0u8; volume];
    let mut light = vec![0u16; volume];
    let mut queue = VecDeque::new();
    for z in 0..depth {
        for x in 0..width {
            let mut sky = 15u8;
            for y in (0..height).rev() {
                let i = (y * depth + z) * width + x;
                let material = material_at(UVec3::new(x as u32, y as u32, z as u32));
                opacity[i] = material.opacity.min(15);
                sky = sky.saturating_sub(opacity[i]);
                light[i] = LightValue::new(material.emission, sky).packed();
                if material.emission != [0; 3] {
                    queue.push_back(i);
                }
            }
        }
    }

    // Direct vertical sky is already at equilibrium. Only boundaries
    // between columns need seeding: queueing every sunlit air voxel would
    // spend most of the flood on a uniform empty sky.
    for y in 0..height {
        for z in 0..depth {
            for x in 0..width {
                let i = (y * depth + z) * width + x;
                let sky = (light[i] >> 12) as u8;
                if sky == 0 {
                    continue;
                }
                let mut can_spread = false;
                for neighbor in [
                    (x > 0).then(|| i - 1),
                    (x + 1 < width).then_some(i + 1),
                    (z > 0).then(|| i - width),
                    (z + 1 < depth).then_some(i + width),
                ]
                .into_iter()
                .flatten()
                {
                    if sky.saturating_sub(opacity[neighbor].max(1)) > (light[neighbor] >> 12) as u8
                    {
                        can_spread = true;
                        break;
                    }
                }
                if can_spread {
                    queue.push_back(i);
                }
            }
        }
    }

    while let Some(i) = queue.pop_front() {
        let x = i % width;
        let z = i / width % depth;
        let y = i / layer;
        let source = LightValue::from_packed(light[i]);
        for neighbor in [
            (x > 0).then(|| i - 1),
            (x + 1 < width).then_some(i + 1),
            (z > 0).then(|| i - width),
            (z + 1 < depth).then_some(i + width),
            (y > 0).then(|| i - layer),
            (y + 1 < height).then_some(i + layer),
        ]
        .into_iter()
        .flatten()
        {
            let attenuation = opacity[neighbor].max(1);
            if attenuation == 15 {
                continue;
            }
            let previous = light[neighbor];
            let mut target = LightValue::from_packed(previous);
            for channel in 0..3 {
                target.rgb[channel] =
                    target.rgb[channel].max(source.rgb[channel].saturating_sub(attenuation));
            }
            target.sky = target.sky.max(source.sky.saturating_sub(attenuation));
            light[neighbor] = target.packed();
            if light[neighbor] != previous {
                queue.push_back(neighbor);
            }
        }
    }
    light
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(values: &[u16], size: UVec3, pos: UVec3) -> LightValue {
        LightValue::from_packed(values[((pos.y * size.z + pos.z) * size.x + pos.x) as usize])
    }

    #[test]
    fn compressed_chunks_preserve_all_four_channels() {
        let values: Vec<_> = (0..CHUNK_VOLUME)
            .map(|i| ((i / 17) % 65536) as u16)
            .collect();
        let chunk = LightChunk::from_packed(&values);
        assert_eq!(
            chunk
                .unpack()
                .into_iter()
                .map(LightValue::packed)
                .collect::<Vec<_>>(),
            values
        );
        for i in [0, 31, 32, 1023, CHUNK_VOLUME - 1] {
            let local = UVec3::new((i % 32) as u32, (i / 1024) as u32, (i / 32 % 32) as u32);
            assert_eq!(chunk.get(local).packed(), values[i]);
        }
        assert_eq!(LightChunk::filled(LightValue::SKY).runs.len(), 1);
    }

    #[test]
    fn colored_sources_mix_across_a_chunk_boundary_and_disappear_when_removed() {
        let size = UVec3::new(64, 5, 5);
        let solve = |red_present| {
            solve_region(size, |p| {
                if p.y != 2 || p.z != 2 {
                    LightMaterial {
                        opacity: 15,
                        emission: [0; 3],
                    }
                } else {
                    LightMaterial {
                        opacity: 0,
                        emission: if p.x == 30 && red_present {
                            [15, 0, 0]
                        } else if p.x == 34 {
                            [0, 0, 15]
                        } else {
                            [0; 3]
                        },
                    }
                }
            })
        };
        assert_eq!(
            at(&solve(true), size, UVec3::new(32, 2, 2)),
            LightValue::new([13, 0, 13], 0)
        );
        assert_eq!(
            at(&solve(false), size, UVec3::new(32, 2, 2)),
            LightValue::new([0, 0, 13], 0)
        );
    }

    #[test]
    fn a_shaft_carries_sky_across_vertical_chunks_and_a_roof_removes_it() {
        let size = UVec3::new(5, 96, 5);
        let solve = |roof| {
            solve_region(size, |p| LightMaterial {
                opacity: if p.x == 2 && p.z == 2 && !(roof && p.y == 70) {
                    0
                } else {
                    15
                },
                emission: [0; 3],
            })
        };
        let open = solve(false);
        for y in [0, 31, 32, 63, 64, 95] {
            assert_eq!(at(&open, size, UVec3::new(2, y, 2)).sky, 15);
        }
        let closed = solve(true);
        assert_eq!(at(&closed, size, UVec3::new(2, 69, 2)).sky, 0);
        assert_eq!(at(&closed, size, UVec3::new(2, 71, 2)).sky, 15);
    }

    #[test]
    fn water_attenuates_direct_sky_and_opaque_walls_stop_rgb() {
        let size = UVec3::new(35, 5, 5);
        let values = solve_region(size, |p| LightMaterial {
            opacity: if p.x == 17 {
                15
            } else if p.y == 4 {
                2
            } else {
                0
            },
            emission: if p == UVec3::new(16, 2, 2) {
                [15, 12, 8]
            } else {
                [0; 3]
            },
        });
        assert_eq!(at(&values, size, UVec3::new(0, 0, 0)).sky, 13);
        assert_eq!(at(&values, size, UVec3::new(18, 2, 2)).rgb, [0; 3]);
    }
}

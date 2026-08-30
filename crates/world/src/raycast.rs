//! Voxel ray traversal (Amanatides & Woo DDA), as a pure function.
//!
//! Used by the client for block targeting/highlighting; reusable by the
//! server for authoritative reach validation later.

use bevy_math::{IVec3, Vec3};

/// A ray/voxel intersection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RayHit {
    /// The block that was hit.
    pub block: IVec3,
    /// Unit axis normal of the face that was entered (points toward the ray
    /// origin). `IVec3::ZERO` when the ray origin already lies inside a
    /// target block — callers must treat that case specially (e.g. refuse
    /// placement).
    pub face_normal: IVec3,
    /// Distance from the origin to the entry point, in blocks.
    pub distance: f32,
}

/// Below this squared length, `dir` is treated as having no direction.
const MIN_DIR_LENGTH_SQUARED: f32 = 1e-12;

/// Walks the voxel grid from `origin` along `dir` for up to `max_distance`
/// and returns the first block for which `is_target` is true.
///
/// - `dir` does not need to be normalized; a near-zero direction returns
///   `None`.
/// - If the origin's own block is a target, returns it with `distance` 0 and
///   `face_normal` `IVec3::ZERO`.
/// - Correct for negative coordinates and axis-aligned rays (the classic DDA
///   pitfalls; covered by tests).
pub fn raycast_voxels(
    origin: Vec3,
    dir: Vec3,
    max_distance: f32,
    is_target: impl Fn(IVec3) -> bool,
) -> Option<RayHit> {
    if dir.length_squared() < MIN_DIR_LENGTH_SQUARED {
        return None;
    }
    let dir = dir.normalize();

    let origin_a = [origin.x, origin.y, origin.z];
    let dir_a = [dir.x, dir.y, dir.z];

    let mut block = [
        origin.x.floor() as i32,
        origin.y.floor() as i32,
        origin.z.floor() as i32,
    ];

    if is_target(IVec3::new(block[0], block[1], block[2])) {
        return Some(RayHit {
            block: IVec3::new(block[0], block[1], block[2]),
            face_normal: IVec3::ZERO,
            distance: 0.0,
        });
    }

    // Amanatides & Woo DDA setup: `step` is the block-index direction to
    // move on each axis, `t_delta` is the parametric distance to cross one
    // full block along that axis, and `t_max` is the distance to the first
    // boundary crossing from `origin`. Axes with zero direction never cross
    // a boundary, so they get an infinite `t_delta`/`t_max` and are simply
    // never chosen as the stepping axis.
    let mut step = [0i32; 3];
    let mut t_delta = [f32::INFINITY; 3];
    let mut t_max = [f32::INFINITY; 3];

    for i in 0..3 {
        if dir_a[i] > 0.0 {
            step[i] = 1;
            t_delta[i] = 1.0 / dir_a[i];
            t_max[i] = ((block[i] as f32 + 1.0) - origin_a[i]) / dir_a[i];
        } else if dir_a[i] < 0.0 {
            step[i] = -1;
            t_delta[i] = 1.0 / -dir_a[i];
            t_max[i] = (block[i] as f32 - origin_a[i]) / dir_a[i];
        }
        // dir_a[i] == 0.0: leave step 0, t_delta/t_max at infinity.
    }

    loop {
        let axis = if t_max[0] <= t_max[1] && t_max[0] <= t_max[2] {
            0
        } else if t_max[1] <= t_max[2] {
            1
        } else {
            2
        };

        let t = t_max[axis];
        if t > max_distance {
            return None;
        }

        block[axis] += step[axis];
        t_max[axis] += t_delta[axis];

        let pos = IVec3::new(block[0], block[1], block[2]);
        if is_target(pos) {
            let mut normal = [0i32; 3];
            // The face we entered through faces back toward the origin,
            // i.e. opposite the direction we just stepped.
            normal[axis] = -step[axis];
            return Some(RayHit {
                block: pos,
                face_normal: IVec3::new(normal[0], normal[1], normal[2]),
                distance: t,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Minimal deterministic LCG so tests need no `rand` dependency (same
    /// pattern as `chunk.rs`'s tests).
    struct Lcg(u64);

    impl Lcg {
        fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next_u64(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }

        fn next_range(&mut self, bound: usize) -> usize {
            (self.next_u64() % bound as u64) as usize
        }

        fn next_f32(&mut self, lo: f32, hi: f32) -> f32 {
            let frac = (self.next_u64() % 1_000_000) as f32 / 1_000_000.0;
            lo + frac * (hi - lo)
        }
    }

    #[test]
    fn hits_each_axis_direction_from_adjacent_air() {
        let target = IVec3::new(0, 0, 0);
        let is_target = |p: IVec3| p == target;

        // (origin, dir, expected face_normal)
        let cases = [
            (
                Vec3::new(-0.5, 0.5, 0.5),
                Vec3::new(1.0, 0.0, 0.0),
                IVec3::new(-1, 0, 0),
            ),
            (
                Vec3::new(1.5, 0.5, 0.5),
                Vec3::new(-1.0, 0.0, 0.0),
                IVec3::new(1, 0, 0),
            ),
            (
                Vec3::new(0.5, -0.5, 0.5),
                Vec3::new(0.0, 1.0, 0.0),
                IVec3::new(0, -1, 0),
            ),
            (
                Vec3::new(0.5, 1.5, 0.5),
                Vec3::new(0.0, -1.0, 0.0),
                IVec3::new(0, 1, 0),
            ),
            (
                Vec3::new(0.5, 0.5, -0.5),
                Vec3::new(0.0, 0.0, 1.0),
                IVec3::new(0, 0, -1),
            ),
            (
                Vec3::new(0.5, 0.5, 1.5),
                Vec3::new(0.0, 0.0, -1.0),
                IVec3::new(0, 0, 1),
            ),
        ];

        for (origin, dir, expected_normal) in cases {
            let hit = raycast_voxels(origin, dir, 5.0, is_target)
                .unwrap_or_else(|| panic!("expected a hit for origin={origin:?} dir={dir:?}"));
            assert_eq!(hit.block, target);
            assert_eq!(hit.face_normal, expected_normal);
            assert!(
                (hit.distance - 0.5).abs() < 1e-4,
                "distance = {}",
                hit.distance
            );
        }
    }

    #[test]
    fn diagonal_ray_hits_correct_first_target() {
        // An infinite wall at x = 5 (any y, z); everything else is not a
        // target, so the ray must thread past several non-target cells
        // before finally crossing into the wall's x slab.
        let is_target = |p: IVec3| p.x == 5;

        let origin = Vec3::new(0.2, 0.3, 0.4);
        let dir = Vec3::new(1.0, 0.5, 0.3);
        let dir_n = dir.normalize();

        let hit = raycast_voxels(origin, dir, 20.0, is_target).expect("expected a hit");
        assert_eq!(hit.block.x, 5);
        assert_eq!(hit.face_normal, IVec3::new(-1, 0, 0));

        let expected_distance = (5.0 - origin.x) / dir_n.x;
        assert!(
            (hit.distance - expected_distance).abs() < 1e-3,
            "distance = {}, expected {}",
            hit.distance,
            expected_distance
        );
    }

    #[test]
    fn negative_coordinate_target() {
        let target = IVec3::new(-3, -2, -1);
        let is_target = |p: IVec3| p == target;

        let origin = Vec3::new(-3.5, -1.5, -0.5);
        let dir = Vec3::new(1.0, 0.0, 0.0);

        let hit = raycast_voxels(origin, dir, 5.0, is_target).expect("expected a hit");
        assert_eq!(hit.block, target);
        assert_eq!(hit.face_normal, IVec3::new(-1, 0, 0));
        assert!((hit.distance - 0.5).abs() < 1e-4);
    }

    #[test]
    fn origin_inside_target_returns_zero_distance() {
        let target = IVec3::new(2, 2, 2);
        let is_target = |p: IVec3| p == target;

        let origin = Vec3::new(2.3, 2.7, 2.1);
        let hit = raycast_voxels(origin, Vec3::new(1.0, 0.0, 0.0), 5.0, is_target)
            .expect("expected an immediate hit");
        assert_eq!(hit.block, target);
        assert_eq!(hit.face_normal, IVec3::ZERO);
        assert_eq!(hit.distance, 0.0);
    }

    #[test]
    fn max_distance_cutoff() {
        let target = IVec3::new(0, 0, 0);
        let is_target = |p: IVec3| p == target;

        // True hit distance is 0.5; cap the ray well short of that.
        let hit = raycast_voxels(
            Vec3::new(-0.5, 0.5, 0.5),
            Vec3::new(1.0, 0.0, 0.0),
            0.3,
            is_target,
        );
        assert_eq!(hit, None);
    }

    #[test]
    fn near_zero_direction_returns_none() {
        // Even though the origin sits inside a target block, a degenerate
        // direction is refused unconditionally.
        let hit = raycast_voxels(Vec3::new(0.5, 0.5, 0.5), Vec3::ZERO, 10.0, |_| true);
        assert_eq!(hit, None);

        let hit = raycast_voxels(Vec3::new(0.5, 0.5, 0.5), Vec3::splat(1e-8), 10.0, |_| true);
        assert_eq!(hit, None);
    }

    #[test]
    fn random_rays_hit_only_land_in_target_after_sampling_before_it() {
        let mut rng = Lcg::new(0x5EED_u64);

        const BOUND: i32 = 6;
        let mut targets: HashSet<IVec3> = HashSet::new();
        for x in -BOUND..=BOUND {
            for y in -BOUND..=BOUND {
                for z in -BOUND..=BOUND {
                    if rng.next_range(100) < 10 {
                        targets.insert(IVec3::new(x, y, z));
                    }
                }
            }
        }
        let is_target = |p: IVec3| targets.contains(&p);

        let mut checked = 0usize;
        for _ in 0..3000 {
            let origin = Vec3::new(
                rng.next_f32(-5.0, 5.0),
                rng.next_f32(-5.0, 5.0),
                rng.next_f32(-5.0, 5.0),
            );
            let dir = Vec3::new(
                rng.next_f32(-1.0, 1.0),
                rng.next_f32(-1.0, 1.0),
                rng.next_f32(-1.0, 1.0),
            );
            if dir.length_squared() < 1e-4 {
                continue;
            }

            let Some(hit) = raycast_voxels(origin, dir, 12.0, is_target) else {
                continue;
            };
            assert!(is_target(hit.block), "returned block must be a target");

            let dir_n = dir.normalize();
            const EPS: f32 = 0.02;
            for frac in [0.1_f32, 0.3, 0.5, 0.7, 0.9] {
                let d = hit.distance * frac;
                if hit.distance - d < EPS {
                    continue;
                }
                let p = origin + dir_n * d;
                let sampled =
                    IVec3::new(p.x.floor() as i32, p.y.floor() as i32, p.z.floor() as i32);
                assert!(
                    !is_target(sampled),
                    "point strictly before the hit distance landed in a target block \
                     (origin={origin:?} dir={dir:?} hit={hit:?} frac={frac} sampled={sampled:?})"
                );
            }
            checked += 1;
        }
        assert!(
            checked > 500,
            "only {checked} rays produced a hit to verify"
        );
    }
}

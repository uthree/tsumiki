//! Voxel AABB collision physics, as pure functions.
//!
//! Lives in the world crate (not the client) so it is unit-testable without
//! Bevy and reusable by the server for authoritative validation later.
//! Callers supply an `is_solid` sampler over world-space block positions;
//! what "solid" means (and how unloaded chunks are treated) is the caller's
//! policy.

use bevy_math::{IVec3, Vec3};

pub const PLAYER_WIDTH: f32 = 0.6;
pub const PLAYER_HEIGHT: f32 = 1.8;
/// Eye height above the feet.
pub const PLAYER_EYE_HEIGHT: f32 = 1.62;

/// Gravity acceleration (blocks/s², negative = down).
pub const GRAVITY: f32 = -28.0;
/// Initial vertical speed of a jump (≈ 1.25 blocks of height).
pub const JUMP_SPEED: f32 = 8.5;
/// Horizontal walking speed, blocks/s.
pub const WALK_SPEED: f32 = 4.5;

/// Axis-aligned box; `min` componentwise below `max`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aabb {
    pub min: Vec3,
    pub max: Vec3,
}

impl Aabb {
    /// The player's collision box: `feet` is the bottom-center point.
    pub fn player(feet: Vec3) -> Self {
        let half = PLAYER_WIDTH / 2.0;
        Self {
            min: Vec3::new(feet.x - half, feet.y, feet.z - half),
            max: Vec3::new(feet.x + half, feet.y + PLAYER_HEIGHT, feet.z + half),
        }
    }

    /// `true` if this box overlaps the unit cube of `block` (exclusive of
    /// exactly-touching faces).
    pub fn intersects_block(&self, block: IVec3) -> bool {
        let bmin = block.as_vec3();
        let bmax = bmin + Vec3::ONE;
        self.min.x < bmax.x
            && self.max.x > bmin.x
            && self.min.y < bmax.y
            && self.max.y > bmin.y
            && self.min.z < bmax.z
            && self.max.z > bmin.z
    }
}

/// Outcome of [`move_aabb`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MoveResult {
    /// The delta actually applied (componentwise clamped by collisions).
    pub moved: Vec3,
    /// Whether movement was blocked on each axis.
    pub hit_x: bool,
    pub hit_y: bool,
    pub hit_z: bool,
    /// `true` when downward movement was blocked (standing on ground).
    pub on_ground: bool,
}

/// Moves `aabb` by `delta` through the voxel grid, resolving collisions
/// axis-by-axis in the order Y, X, Z (collide-and-slide).
///
/// Per axis: advance up to the first solid block face along the movement
/// direction, leaving a small skin gap (~1e-4) so the box never ends up
/// exactly flush inside a block face. Movement on later axes uses the
/// already-resolved position of earlier axes. `is_solid` is consulted only
/// for blocks the swept box could overlap.
///
/// Robustness requirements (tested):
/// - No tunneling for `|delta|` up to at least one block per call (callers
///   substep larger deltas).
/// - An AABB already overlapping a solid block must not get stuck launched;
///   the overlapping axis simply reports a hit with zero movement.
pub fn move_aabb(aabb: Aabb, delta: Vec3, is_solid: impl Fn(IVec3) -> bool) -> MoveResult {
    let mut current = aabb;
    let mut moved = Vec3::ZERO;

    // Y axis: v = X, w = Z (both still at their original, unresolved extents).
    let y = sweep_axis(
        current.min.y,
        current.max.y,
        delta.y,
        current.min.x,
        current.max.x,
        current.min.z,
        current.max.z,
        |by, bx, bz| IVec3::new(bx, by, bz),
        &is_solid,
    );
    current.min.y += y.moved;
    current.max.y += y.moved;
    moved.y = y.moved;

    // X axis: v = Y (already resolved above), w = Z (still original).
    let x = sweep_axis(
        current.min.x,
        current.max.x,
        delta.x,
        current.min.y,
        current.max.y,
        current.min.z,
        current.max.z,
        IVec3::new,
        &is_solid,
    );
    current.min.x += x.moved;
    current.max.x += x.moved;
    moved.x = x.moved;

    // Z axis: v = X, w = Y (both already resolved above).
    let z = sweep_axis(
        current.min.z,
        current.max.z,
        delta.z,
        current.min.x,
        current.max.x,
        current.min.y,
        current.max.y,
        |bz, bx, by| IVec3::new(bx, by, bz),
        &is_solid,
    );
    // (No further axis is resolved after Z, so `current` need not be
    // updated with `z.moved`.)
    moved.z = z.moved;

    MoveResult {
        moved,
        hit_x: x.hit,
        hit_y: y.hit,
        hit_z: z.hit,
        // Grounded when downward (or stationary-while-embedded) motion on Y
        // was blocked; a blocked upward move is a ceiling hit, not ground.
        on_ground: y.hit && delta.y <= 0.0,
    }
}

/// Skin gap left between a stopped box face and the block face that stopped
/// it, so the box never ends up exactly flush (which would be ambiguous for
/// the next frame's overlap test).
const SKIN: f32 = 1e-4;

/// Epsilon used only to decide whether a box's face that lands exactly on an
/// integer block boundary should count as touching the block beyond that
/// boundary. Kept well below [`SKIN`] so a box stopped with the skin gap is
/// never misclassified as still overlapping the block it stopped against.
const BOUNDARY_EPS: f32 = 1e-5;

/// Smallest block index whose cube can overlap a box extent starting at
/// `min_val` on some axis.
fn range_lo(min_val: f32) -> i32 {
    min_val.floor() as i32
}

/// Largest block index whose cube can overlap a box extent ending at
/// `max_val` on some axis (exclusive: a box edge sitting exactly on a block
/// boundary does not overlap the block beyond it).
fn range_hi(max_val: f32) -> i32 {
    (max_val - BOUNDARY_EPS).floor() as i32
}

/// Outcome of sweeping a single axis.
struct AxisResult {
    /// Actual signed displacement applied along the axis.
    moved: f32,
    /// Whether movement was blocked (either by a collision found during the
    /// sweep, or because the box already overlapped a solid block on this
    /// axis before moving).
    hit: bool,
}

/// Sweeps one axis of a box from `u0..u1` by `d`, given the box's (fixed,
/// already-resolved-or-original) extent on the other two axes `v0..v1` and
/// `w0..w1`. `make_pos(bu, bv, bw)` reassembles a block coordinate from the
/// swept axis index and the two fixed-axis indices in whatever order the
/// caller's axis assignment requires.
#[allow(clippy::too_many_arguments)]
fn sweep_axis(
    u0: f32,
    u1: f32,
    d: f32,
    v0: f32,
    v1: f32,
    w0: f32,
    w1: f32,
    make_pos: impl Fn(i32, i32, i32) -> IVec3,
    is_solid: &impl Fn(IVec3) -> bool,
) -> AxisResult {
    let v_lo = range_lo(v0);
    let v_hi = range_hi(v1);
    let w_lo = range_lo(w0);
    let w_hi = range_hi(w1);

    let any_solid_at = |bu: i32| -> bool {
        for bv in v_lo..=v_hi {
            for bw in w_lo..=w_hi {
                if is_solid(make_pos(bu, bv, bw)) {
                    return true;
                }
            }
        }
        false
    };

    let cur_lo = range_lo(u0);
    let cur_hi = range_hi(u1);

    // Already-overlapping guard: if the box's own (unmoved) extent already
    // overlaps a solid block, refuse to move on this axis at all rather than
    // computing a "correcting" displacement that would launch it out.
    for bu in cur_lo..=cur_hi {
        if any_solid_at(bu) {
            return AxisResult {
                moved: 0.0,
                hit: true,
            };
        }
    }

    if d == 0.0 {
        return AxisResult {
            moved: 0.0,
            hit: false,
        };
    }

    if d > 0.0 {
        let start = cur_hi + 1;
        let end = range_hi(u1 + d);
        let mut hit_bu = None;
        if end >= start {
            for bu in start..=end {
                if any_solid_at(bu) {
                    hit_bu = Some(bu);
                    break;
                }
            }
        }
        match hit_bu {
            Some(bu) => {
                let allowed = (bu as f32 - u1 - SKIN).clamp(0.0, d);
                AxisResult {
                    moved: allowed,
                    hit: true,
                }
            }
            None => AxisResult {
                moved: d,
                hit: false,
            },
        }
    } else {
        let start = cur_lo - 1;
        let end = range_lo(u0 + d);
        let mut hit_bu = None;
        if start >= end {
            let mut bu = start;
            while bu >= end {
                if any_solid_at(bu) {
                    hit_bu = Some(bu);
                    break;
                }
                bu -= 1;
            }
        }
        match hit_bu {
            Some(bu) => {
                let allowed = ((bu + 1) as f32 - u0 + SKIN).clamp(d, 0.0);
                AxisResult {
                    moved: allowed,
                    hit: true,
                }
            }
            None => AxisResult {
                moved: d,
                hit: false,
            },
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

    /// Every block in a set overlapping `aabb` over the range it could
    /// possibly touch.
    fn intersects_any(aabb: &Aabb, is_solid: &impl Fn(IVec3) -> bool) -> bool {
        let lo = IVec3::new(
            aabb.min.x.floor() as i32,
            aabb.min.y.floor() as i32,
            aabb.min.z.floor() as i32,
        );
        let hi = IVec3::new(
            (aabb.max.x.ceil() as i32) - 1,
            (aabb.max.y.ceil() as i32) - 1,
            (aabb.max.z.ceil() as i32) - 1,
        );
        for x in lo.x..=hi.x {
            for y in lo.y..=hi.y {
                for z in lo.z..=hi.z {
                    let p = IVec3::new(x, y, z);
                    if is_solid(p) && aabb.intersects_block(p) {
                        return true;
                    }
                }
            }
        }
        false
    }

    #[test]
    fn zero_delta_yields_zero_result() {
        let aabb = Aabb::player(Vec3::new(0.0, 0.0, 0.0));
        let result = move_aabb(aabb, Vec3::ZERO, |_| false);
        assert_eq!(result, MoveResult::default());
    }

    #[test]
    fn falling_lands_exactly_on_floor_surface() {
        // Floor occupies y = 0 (top surface at world y = 1) under a wide
        // patch of x/z.
        let is_solid = |p: IVec3| p.y == 0 && p.x.abs() <= 5 && p.z.abs() <= 5;

        let mut aabb = Aabb::player(Vec3::new(0.0, 5.0, 0.0));
        let mut landed = None;
        for _ in 0..20 {
            let result = move_aabb(aabb, Vec3::new(0.0, -1.0, 0.0), is_solid);
            aabb.min += result.moved;
            aabb.max += result.moved;
            if result.on_ground {
                landed = Some(result);
                break;
            }
        }
        let result = landed.expect("box should have landed on the floor");
        assert!(result.hit_y);
        assert!(result.on_ground);
        // Box's feet (min.y) should rest just above the floor's top surface
        // (y = 1), within a couple of skin gaps.
        assert!(
            (aabb.min.y - 1.0).abs() < 4.0 * SKIN,
            "min.y = {}, expected ~1.0",
            aabb.min.y
        );
    }

    #[test]
    fn walking_into_wall_stops_with_skin_gap() {
        // Solid wall filling x = 5 for all y, z.
        let is_solid = |p: IVec3| p.x == 5;

        let mut aabb = Aabb::player(Vec3::new(0.0, 0.0, 0.0));
        let mut hit = false;
        for _ in 0..20 {
            let result = move_aabb(aabb, Vec3::new(1.0, 0.0, 0.0), is_solid);
            aabb.min += result.moved;
            aabb.max += result.moved;
            if result.hit_x {
                hit = true;
                break;
            }
        }
        assert!(hit, "box should have hit the wall");
        assert!(
            (aabb.max.x - (5.0 - SKIN)).abs() < 1e-5,
            "max.x = {}, expected ~{}",
            aabb.max.x,
            5.0 - SKIN
        );
    }

    #[test]
    fn sliding_along_wall_keeps_free_axis_full_speed() {
        // Solid wall filling x = 5 for all y, z; z is open.
        let is_solid = |p: IVec3| p.x == 5;

        // Positioned close enough that a delta of 1.0 on x would tunnel past
        // the wall face if unclamped.
        let aabb = Aabb::player(Vec3::new(4.4, 0.0, 0.0));
        let result = move_aabb(aabb, Vec3::new(1.0, 0.0, 1.0), is_solid);

        assert!(result.hit_x);
        assert!(!result.hit_z);
        assert_eq!(result.moved.z, 1.0);
        assert!(result.moved.x < 1.0);
        let new_max_x = aabb.max.x + result.moved.x;
        assert!(
            (new_max_x - (5.0 - SKIN)).abs() < 1e-5,
            "max.x after slide = {new_max_x}"
        );
    }

    #[test]
    fn jumping_into_ceiling_stops_without_on_ground() {
        // Solid ceiling filling y = 5 for all x, z.
        let is_solid = |p: IVec3| p.y == 5;

        let mut aabb = Aabb::player(Vec3::new(0.0, 0.0, 0.0));
        let mut hit = false;
        for _ in 0..20 {
            let result = move_aabb(aabb, Vec3::new(0.0, 1.0, 0.0), is_solid);
            aabb.min += result.moved;
            aabb.max += result.moved;
            if result.hit_y {
                hit = true;
                assert!(
                    !result.on_ground,
                    "hitting a ceiling is not standing on ground"
                );
                break;
            }
        }
        assert!(hit, "box should have hit the ceiling");
        assert!(
            (aabb.max.y - (5.0 - SKIN)).abs() < 1e-5,
            "max.y = {}, expected ~{}",
            aabb.max.y,
            5.0 - SKIN
        );
    }

    #[test]
    fn walking_through_narrow_corridor() {
        // Walls at x = -1 and x = 1, corridor open at x in [0, 1). No
        // ceiling: player height 1.8 fits under an implicit "roof" that
        // simply doesn't exist here.
        let is_solid = |p: IVec3| p.x == -1 || p.x == 1;

        let mut aabb = Aabb::player(Vec3::new(0.5, 0.0, 0.0));
        for _ in 0..10 {
            let result = move_aabb(aabb, Vec3::new(0.0, 0.0, 1.0), is_solid);
            assert!(
                !result.hit_x,
                "corridor walls should not obstruct forward motion"
            );
            assert!(!result.hit_z, "corridor should be unobstructed lengthwise");
            assert_eq!(result.moved.z, 1.0);
            aabb.min += result.moved;
            aabb.max += result.moved;
        }
    }

    #[test]
    fn stepping_off_edge_falls() {
        // Floor only exists for x in [0, 5).
        let is_solid = |p: IVec3| p.y == 0 && (0..5).contains(&p.x) && p.z.abs() <= 5;

        // Start resting on the floor.
        let mut aabb = Aabb::player(Vec3::new(2.0, 1.0 + SKIN, 0.0));

        // Walk past the edge (x = 5 onward has no floor).
        for _ in 0..6 {
            let result = move_aabb(aabb, Vec3::new(1.0, 0.0, 0.0), is_solid);
            aabb.min += result.moved;
            aabb.max += result.moved;
        }
        assert!(
            aabb.min.x >= 5.0,
            "should have walked past the floor's edge"
        );

        // Now falling should be unobstructed.
        let result = move_aabb(aabb, Vec3::new(0.0, -1.0, 0.0), is_solid);
        assert!(!result.hit_y, "there is no floor beyond the edge");
        assert!(!result.on_ground);
        assert_eq!(result.moved.y, -1.0);
    }

    #[test]
    fn overlapping_box_reports_hit_with_zero_movement_never_launched() {
        // A single solid block at the origin.
        let is_solid = |p: IVec3| p == IVec3::new(0, 0, 0);

        // A small box fully embedded inside that block on all three axes.
        let aabb = Aabb {
            min: Vec3::new(0.2, 0.2, 0.2),
            max: Vec3::new(0.8, 0.8, 0.8),
        };

        let result = move_aabb(aabb, Vec3::new(0.5, 0.5, 0.5), is_solid);
        assert_eq!(result.moved, Vec3::ZERO);
        assert!(result.hit_x && result.hit_y && result.hit_z);

        // Same, moving the other way: still refused, never "launched" out.
        let result = move_aabb(aabb, Vec3::new(-0.5, -0.5, -0.5), is_solid);
        assert_eq!(result.moved, Vec3::ZERO);
        assert!(result.hit_x && result.hit_y && result.hit_z);
    }

    #[test]
    fn random_moves_never_end_intersecting_solid_blocks() {
        let mut rng = Lcg::new(0xB0BACAFE_u64);

        const BOUND: i32 = 6;
        let mut field: HashSet<IVec3> = HashSet::new();
        for x in -BOUND..=BOUND {
            for y in -BOUND..=BOUND {
                for z in -BOUND..=BOUND {
                    if rng.next_range(100) < 20 {
                        field.insert(IVec3::new(x, y, z));
                    }
                }
            }
        }
        let is_solid = |p: IVec3| field.contains(&p);

        let half = Vec3::new(0.3, 0.4, 0.3);
        let mut tested = 0usize;
        let mut attempts = 0usize;
        while tested < 3000 && attempts < 500_000 {
            attempts += 1;
            let center = Vec3::new(
                rng.next_f32(-4.0, 4.0),
                rng.next_f32(-4.0, 4.0),
                rng.next_f32(-4.0, 4.0),
            );
            let aabb = Aabb {
                min: center - half,
                max: center + half,
            };
            if intersects_any(&aabb, &is_solid) {
                continue;
            }

            let delta = Vec3::new(
                rng.next_f32(-1.0, 1.0),
                rng.next_f32(-1.0, 1.0),
                rng.next_f32(-1.0, 1.0),
            );
            let result = move_aabb(aabb, delta, is_solid);

            // Never move further than requested on any axis.
            assert!(result.moved.x.abs() <= delta.x.abs() + 1e-6);
            assert!(result.moved.y.abs() <= delta.y.abs() + 1e-6);
            assert!(result.moved.z.abs() <= delta.z.abs() + 1e-6);

            let moved_box = Aabb {
                min: aabb.min + result.moved,
                max: aabb.max + result.moved,
            };
            assert!(
                !intersects_any(&moved_box, &is_solid),
                "moved box ended up intersecting a solid block: start={aabb:?} delta={delta:?} result={result:?}"
            );
            tested += 1;
        }
        assert!(
            tested >= 3000,
            "only managed {tested} valid trials in {attempts} attempts"
        );
    }
}

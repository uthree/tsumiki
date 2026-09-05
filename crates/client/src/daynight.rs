//! Day/night cycle (roadmap.md M4): local time advancement and the sun's
//! direction/illuminance, ambient brightness, and sky color, all driven by
//! [`GameState::time_of_day`].
//!
//! The `time -> lighting` mapping ([`lighting_for_time`]) is a pure
//! function, unit-tested below without touching Bevy; the systems below just
//! advance the clock and apply its output to the relevant resources/entity.

use bevy::prelude::*;

use crate::AppState;
use crate::state::GameState;
use crate::view::ChunkMaterial;
use crate::voxel_material::VoxelMaterial;

/// Seconds for a full day/night cycle: `time_of_day` (`[0, 1)`) advances by
/// `1.0 / DAY_LENGTH_SECS` per second locally, resynced periodically by
/// `ServerToClient::TimeUpdate`.
const DAY_LENGTH_SECS: f32 = 600.0;

/// Peak sun elevation above the horizon, in radians (~80 degrees).
const MAX_ELEVATION_RAD: f32 = 1.396_263_4;

/// Half-width, in `sin(2π·t)` units, of the smooth transition band around
/// each horizon crossing (sunrise/sunset) used to interpolate
/// illuminance/ambient/sky color instead of snapping exactly at the
/// horizon.
const TRANSITION_EDGE: f32 = 0.15;

const DAY_ILLUMINANCE: f32 = 10_000.0;
const NIGHT_ILLUMINANCE: f32 = 5.0;

const DAY_SKY: Color = Color::srgb(0.55, 0.78, 0.95);
const NIGHT_SKY: Color = Color::srgb(0.05, 0.06, 0.14);

/// The lighting state for a given time of day: everything [`apply_lighting`]
/// needs to drive the sun/ambient/sky. See [`lighting_for_time`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lighting {
    /// Radians above (positive) or below (negative) the horizon.
    pub sun_elevation: f32,
    pub illuminance: f32,
    pub ambient_brightness: f32,
    pub sky_color: Color,
    /// RGB tint plus intensity, applied only where skylight has propagated.
    pub voxel_sunlight: Vec4,
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Pure `time_of_day -> lighting` mapping. `t` wraps: any real value is
/// accepted, only `t.rem_euclid(1.0)` matters (`0` = sunrise, `0.25` = noon,
/// `0.5` = sunset, `0.75` = midnight, matching `Welcome::time_of_day`'s
/// convention).
pub fn lighting_for_time(t: f32) -> Lighting {
    let t = t.rem_euclid(1.0);
    let sun_sin = (t * std::f32::consts::TAU).sin();
    let sun_elevation = sun_sin * MAX_ELEVATION_RAD;

    // `sun_sin` is already 0 at both horizon crossings and ±1 at
    // noon/midnight, so it doubles as a ready-made interpolation parameter
    // for a smooth day/night blend around those crossings.
    let day_factor = smoothstep(-TRANSITION_EDGE, TRANSITION_EDGE, sun_sin);
    let horizon_factor = 1.0 - smoothstep(0.0, 0.25, sun_sin.abs());
    let sunlight_color = Vec3::new(0.40, 0.52, 0.82)
        .lerp(Vec3::new(1.0, 0.98, 0.90), day_factor)
        .lerp(Vec3::new(1.0, 0.57, 0.30), horizon_factor * 0.65);

    Lighting {
        sun_elevation,
        illuminance: NIGHT_ILLUMINANCE + (DAY_ILLUMINANCE - NIGHT_ILLUMINANCE) * day_factor,
        ambient_brightness: 0.0,
        sky_color: NIGHT_SKY.mix(&DAY_SKY, day_factor),
        voxel_sunlight: sunlight_color.extend(0.045 + 1.25 * day_factor),
    }
}

/// Tags the sun so [`despawn_sun`] can find it again on the way out.
#[derive(Component)]
struct SunLight;

fn spawn_sun(mut commands: Commands) {
    commands.spawn((
        DirectionalLight {
            color: Color::srgb(1.0, 0.98, 0.9),
            // Voxel sky occlusion supplies terrain shadows; avatars and
            // items sample that same field. Shadow-map passes would draw
            // the entire world again without affecting these materials.
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::default(),
        SunLight,
    ));
}

/// Part of the `OnExit(AppState::InGame)` "despawn everything in-game"
/// contract (see `pause` module docs).
fn despawn_sun(mut commands: Commands, suns: Query<Entity, With<SunLight>>) {
    for entity in &suns {
        commands.entity(entity).despawn();
    }
}

fn advance_time(time: Res<Time>, mut state: ResMut<GameState>) {
    state.time_of_day = (state.time_of_day + time.delta_secs() / DAY_LENGTH_SECS).rem_euclid(1.0);
}

/// Rotates the sun, sets its illuminance, the global ambient brightness and
/// the sky clear color, all as a function of [`GameState::time_of_day`].
fn apply_lighting(
    state: Res<GameState>,
    mut suns: Query<(&mut Transform, &mut DirectionalLight), With<SunLight>>,
    mut ambient: ResMut<GlobalAmbientLight>,
    mut clear_color: ResMut<ClearColor>,
    material: Option<Res<ChunkMaterial>>,
    mut materials: ResMut<Assets<VoxelMaterial>>,
) {
    let lighting = lighting_for_time(state.time_of_day);
    for (mut transform, mut light) in &mut suns {
        // Fixed azimuth (matches the old fixed sun's -30deg yaw); only the
        // elevation (pitch) sweeps with time of day.
        transform.rotation = Quat::from_rotation_y((-30f32).to_radians())
            * Quat::from_rotation_x(-lighting.sun_elevation);
        light.illuminance = lighting.illuminance;
    }
    ambient.brightness = lighting.ambient_brightness;
    clear_color.0 = lighting.sky_color;
    if let Some(material) = material
        && let Some(mut voxel) = materials.get_mut(&material.0)
    {
        voxel.extension.sunlight = lighting.voxel_sunlight;
    }
}

/// Wires the sun entity's lifecycle and the time-advance/lighting-apply
/// systems into `app`.
pub fn install(app: &mut App) {
    app.add_systems(OnEnter(AppState::InGame), spawn_sun)
        .add_systems(OnExit(AppState::InGame), despawn_sun)
        .add_systems(
            Update,
            (advance_time, apply_lighting)
                .chain()
                .run_if(in_state(AppState::InGame)),
        );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noon_is_brighter_than_sunrise_and_sunset() {
        let noon = lighting_for_time(0.25);
        let sunrise = lighting_for_time(0.0);
        let sunset = lighting_for_time(0.5);
        assert!(noon.illuminance > sunrise.illuminance);
        assert!(noon.illuminance > sunset.illuminance);
    }

    #[test]
    fn midnight_is_darkest() {
        let midnight = lighting_for_time(0.75);
        let noon = lighting_for_time(0.25);
        let sunrise = lighting_for_time(0.0);
        assert!(midnight.illuminance < noon.illuminance);
        assert!(midnight.illuminance < sunrise.illuminance);
    }

    #[test]
    fn day_night_ordering_is_monotonic_through_a_full_cycle() {
        let midnight = lighting_for_time(0.75).illuminance;
        let dawn = lighting_for_time(0.0).illuminance;
        let noon = lighting_for_time(0.25).illuminance;
        let dusk = lighting_for_time(0.5).illuminance;
        assert!(midnight < dawn, "midnight={midnight} dawn={dawn}");
        assert!(dawn < noon, "dawn={dawn} noon={noon}");
        assert!(noon > dusk, "noon={noon} dusk={dusk}");
        assert!(dusk > midnight, "dusk={dusk} midnight={midnight}");
    }

    #[test]
    fn global_ambient_does_not_illuminate_unlit_caves() {
        let samples = 1000;
        for i in 0..=samples {
            let t = i as f32 / samples as f32;
            let ambient = lighting_for_time(t).ambient_brightness;
            assert_eq!(ambient, 0.0, "t={t}");
        }
    }

    #[test]
    fn voxel_skylight_dimms_and_turns_blue_at_night() {
        let noon = lighting_for_time(0.25).voxel_sunlight;
        let midnight = lighting_for_time(0.75).voxel_sunlight;
        assert!(midnight.w < noon.w * 0.05);
        assert!(midnight.z > midnight.x);
        assert!(noon.x > noon.z);
    }

    #[test]
    fn sunrise_and_sunset_tint_sky_lit_terrain_warm() {
        let noon = lighting_for_time(0.25).voxel_sunlight;
        for time in [0.0, 0.5] {
            let horizon = lighting_for_time(time).voxel_sunlight;
            assert!(horizon.x / horizon.z > noon.x / noon.z * 1.5);
            assert!(horizon.w < noon.w);
            assert!(horizon.w > lighting_for_time(0.75).voxel_sunlight.w);
        }
    }

    #[test]
    fn wraps_around_seamlessly() {
        assert_eq!(lighting_for_time(1.0), lighting_for_time(0.0));
        assert_eq!(lighting_for_time(1.25), lighting_for_time(0.25));
        assert_eq!(lighting_for_time(-0.25), lighting_for_time(0.75));
    }

    #[test]
    fn sun_is_above_horizon_at_noon_and_below_at_midnight() {
        assert!(lighting_for_time(0.25).sun_elevation > 0.0);
        assert!(lighting_for_time(0.75).sun_elevation < 0.0);
    }

    #[test]
    fn sun_is_at_the_horizon_on_sunrise_and_sunset() {
        assert!((lighting_for_time(0.0).sun_elevation).abs() < 1e-4);
        assert!((lighting_for_time(0.5).sun_elevation).abs() < 1e-4);
    }
}

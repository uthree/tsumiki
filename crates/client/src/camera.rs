//! Free-flying debug camera.
//!
//! Responsibilities (implemented by the client agent):
//! - Component + plugin/systems for a fly camera: mouse look (cursor grabbed
//!   on left click, released on Escape), WASD horizontal movement relative to
//!   look direction, Space/Ctrl for up/down, Shift to boost speed.
//! - Spawned at a vantage point above the terrain looking toward it.

use std::f32::consts::FRAC_PI_2;

use bevy::input::mouse::AccumulatedMouseMotion;
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

/// Marker + per-entity state for the free-flying debug camera.
#[derive(Component)]
pub struct FlyCam {
    pub yaw: f32,
    pub pitch: f32,
    pub speed: f32,
    pub sensitivity: f32,
    pub boost_multiplier: f32,
}

impl Default for FlyCam {
    fn default() -> Self {
        Self {
            yaw: 0.0,
            pitch: 0.0,
            speed: 24.0,
            sensitivity: 0.0025,
            boost_multiplier: 4.0,
        }
    }
}

/// Keep just shy of the poles to avoid the look quaternion degenerating.
const PITCH_LIMIT: f32 = FRAC_PI_2 - 0.01;

/// Wires the fly-camera systems into `app` and spawns the camera entity.
pub fn install(app: &mut App) {
    app.add_systems(Startup, spawn_camera)
        .add_systems(Update, (grab_cursor, look, movement));
}

fn spawn_camera(mut commands: Commands) {
    let yaw = (-135f32).to_radians();
    let pitch = (-20f32).to_radians();
    commands.spawn((
        Camera3d::default(),
        Projection::Perspective(PerspectiveProjection {
            near: 0.1,
            far: 2000.0,
            ..default()
        }),
        Transform::from_xyz(8.0, 80.0, 8.0)
            .with_rotation(Quat::from_euler(EulerRot::YXZ, yaw, pitch, 0.0)),
        FlyCam {
            yaw,
            pitch,
            ..default()
        },
    ));
}

fn grab_cursor(
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    mut windows: Query<&mut CursorOptions, With<PrimaryWindow>>,
) {
    let Ok(mut cursor) = windows.single_mut() else {
        return;
    };
    if mouse_buttons.just_pressed(MouseButton::Left) {
        cursor.grab_mode = CursorGrabMode::Locked;
        cursor.visible = false;
    }
    if keys.just_pressed(KeyCode::Escape) {
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    }
}

fn look(
    mouse_motion: Res<AccumulatedMouseMotion>,
    windows: Query<&CursorOptions, With<PrimaryWindow>>,
    mut cameras: Query<(&mut FlyCam, &mut Transform)>,
) {
    let Ok(cursor) = windows.single() else {
        return;
    };
    if cursor.grab_mode == CursorGrabMode::None || mouse_motion.delta == Vec2::ZERO {
        return;
    }
    for (mut cam, mut transform) in &mut cameras {
        cam.yaw -= mouse_motion.delta.x * cam.sensitivity;
        cam.pitch =
            (cam.pitch - mouse_motion.delta.y * cam.sensitivity).clamp(-PITCH_LIMIT, PITCH_LIMIT);
        transform.rotation = Quat::from_euler(EulerRot::YXZ, cam.yaw, cam.pitch, 0.0);
    }
}

fn movement(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    mut cameras: Query<(&FlyCam, &mut Transform)>,
) {
    for (cam, mut transform) in &mut cameras {
        // WASD is relative to yaw only, so looking up/down doesn't slow
        // horizontal movement or make W climb/dive.
        let yaw_rotation = Quat::from_rotation_y(cam.yaw);
        let forward = yaw_rotation * Vec3::NEG_Z;
        let right = yaw_rotation * Vec3::X;

        let mut direction = Vec3::ZERO;
        if keys.pressed(KeyCode::KeyW) {
            direction += forward;
        }
        if keys.pressed(KeyCode::KeyS) {
            direction -= forward;
        }
        if keys.pressed(KeyCode::KeyD) {
            direction += right;
        }
        if keys.pressed(KeyCode::KeyA) {
            direction -= right;
        }
        if keys.pressed(KeyCode::Space) {
            direction += Vec3::Y;
        }
        if keys.pressed(KeyCode::ControlLeft) {
            direction -= Vec3::Y;
        }
        if direction == Vec3::ZERO {
            continue;
        }

        let mut speed = cam.speed;
        if keys.pressed(KeyCode::ShiftLeft) {
            speed *= cam.boost_multiplier;
        }
        transform.translation += direction.normalize() * speed * time.delta_secs();
    }
}

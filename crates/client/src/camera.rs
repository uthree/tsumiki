//! Player controller: first-person camera driven by a feet position, with a
//! walking mode (voxel AABB collision + gravity) and a flying debug mode.
//!
//! - Mouse look and cursor grab/release behavior match the old fly camera.
//! - `F` toggles between [`PlayerMode::Walk`] and [`PlayerMode::Fly`].
//! - The camera `Transform` is a pure function of `feet` + eye height and
//!   yaw/pitch; it is written once per frame by [`sync_camera_transform`],
//!   after input/physics have updated the [`Player`] component.
//! - Until [`crate::net`] resolves the spawn point, `Player::spawned` is
//!   `false` and movement/physics are skipped entirely (the camera just
//!   sits at a vantage point above the default spawn column).

use bevy::input::mouse::AccumulatedMouseMotion;
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use std::f32::consts::FRAC_PI_2;
use tsumiki_world::physics::{
    self, Aabb, GRAVITY, JUMP_SPEED, MoveResult, PLAYER_EYE_HEIGHT, WALK_SPEED,
};

use crate::view::{self, ChunkStore};
use crate::{AppState, ClientConfig};

/// Default spawn column (world-space X/Z), used both for the initial
/// "waiting for spawn" vantage point and, by [`crate::net`], to find the
/// ground when no saved player state exists, unless overridden by
/// [`ClientConfig::spawn_xz`] (see [`spawn_xz`]).
pub const DEFAULT_SPAWN_X: f32 = 8.0;
pub const DEFAULT_SPAWN_Z: f32 = 8.0;

/// The world-space X/Z spawn column to use for a fresh player: the
/// client-configured override if set (`.x`/`.y` map to world X/Z), else the
/// fixed default. Shared by [`spawn_player`]'s placeholder position and
/// [`crate::net`]'s resolved-spawn column so both agree.
pub fn spawn_xz(config: &ClientConfig) -> Vec2 {
    config
        .spawn_xz
        .unwrap_or(Vec2::new(DEFAULT_SPAWN_X, DEFAULT_SPAWN_Z))
}

/// Placeholder feet height used only until [`crate::net`] resolves the real
/// spawn point.
const WAITING_FEET_Y: f32 = 80.0;

const FLY_SPEED: f32 = 24.0;
const FLY_BOOST_MULTIPLIER: f32 = 4.0;
const MOUSE_SENSITIVITY: f32 = 0.0025;

/// Keep just shy of the poles to avoid the look quaternion degenerating.
const PITCH_LIMIT: f32 = FRAC_PI_2 - 0.01;

/// Largest delta component handed to a single [`physics::move_aabb`] call;
/// larger frame deltas (e.g. after a hitch, or fast fly-to-walk transitions)
/// are substepped so the sweep never risks tunneling.
const MAX_SUBSTEP: f32 = 0.5;

/// How the player's movement is currently being simulated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlayerMode {
    Walk,
    Fly,
}

/// The player: feet position (bottom-center, matching
/// [`tsumiki_world::physics::Aabb::player`]), velocity (used in `Walk` mode
/// only), look angles, and simulation mode.
#[derive(Component)]
pub struct Player {
    pub feet: Vec3,
    pub velocity: Vec3,
    pub yaw: f32,
    pub pitch: f32,
    pub mode: PlayerMode,
    /// `true` once [`crate::net`] has resolved a real spawn point. Movement
    /// and physics are skipped while this is `false`.
    pub spawned: bool,
    /// Result of the previous `Walk`-mode move; used to gate jumping.
    pub on_ground: bool,
}

/// Wires the player-controller systems into `app`. The player entity (in its
/// "waiting for spawn" state) is spawned on entering [`AppState::InGame`];
/// none of these systems (including cursor-grab-on-click) run in the menu.
pub fn install(app: &mut App) {
    app.add_systems(OnEnter(AppState::InGame), spawn_player)
        .add_systems(
            Update,
            (
                grab_cursor,
                look,
                toggle_mode,
                movement,
                sync_camera_transform,
            )
                .chain()
                .run_if(in_state(AppState::InGame)),
        );
}

fn spawn_player(mut commands: Commands, config: Res<ClientConfig>) {
    let yaw = (-135f32).to_radians();
    let pitch = (-20f32).to_radians();
    let xz = spawn_xz(&config);
    let feet = Vec3::new(xz.x, WAITING_FEET_Y, xz.y);
    commands.spawn((
        Camera3d::default(),
        Projection::Perspective(PerspectiveProjection {
            near: 0.1,
            far: 2000.0,
            ..default()
        }),
        Transform::from_translation(feet + Vec3::Y * PLAYER_EYE_HEIGHT)
            .with_rotation(Quat::from_euler(EulerRot::YXZ, yaw, pitch, 0.0)),
        Player {
            feet,
            velocity: Vec3::ZERO,
            yaw,
            pitch,
            // Fly until net.rs resolves a real spawn; harmless since
            // `spawned` is false and movement is skipped either way.
            mode: PlayerMode::Fly,
            spawned: false,
            on_ground: false,
        },
    ));
}

/// Grabs the cursor on left click, releases it on Escape.
///
/// `pub(crate)` so [`crate::interact`] can order its click handling relative
/// to this system: it must run *before* this system so that the very click
/// that grabs the cursor is not also seen as a "grabbed" click by
/// break/place handling.
pub(crate) fn grab_cursor(
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
    mut players: Query<&mut Player>,
) {
    let Ok(cursor) = windows.single() else {
        return;
    };
    if cursor.grab_mode == CursorGrabMode::None || mouse_motion.delta == Vec2::ZERO {
        return;
    }
    for mut player in &mut players {
        player.yaw -= mouse_motion.delta.x * MOUSE_SENSITIVITY;
        player.pitch = (player.pitch - mouse_motion.delta.y * MOUSE_SENSITIVITY)
            .clamp(-PITCH_LIMIT, PITCH_LIMIT);
    }
}

fn toggle_mode(keys: Res<ButtonInput<KeyCode>>, mut players: Query<&mut Player>) {
    if !keys.just_pressed(KeyCode::KeyF) {
        return;
    }
    for mut player in &mut players {
        if !player.spawned {
            continue;
        }
        player.mode = match player.mode {
            PlayerMode::Walk => PlayerMode::Fly,
            PlayerMode::Fly => PlayerMode::Walk,
        };
        if player.mode == PlayerMode::Fly {
            // Dropping into Fly with leftover fall/jump velocity would jolt
            // the camera on the next Walk re-entry.
            player.velocity = Vec3::ZERO;
        }
    }
}

fn movement(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    store: Res<ChunkStore>,
    registry: Res<view::Registry>,
    mut players: Query<&mut Player>,
) {
    let dt = time.delta_secs();
    for mut player in &mut players {
        if !player.spawned {
            continue;
        }
        match player.mode {
            PlayerMode::Fly => fly_step(&mut player, &keys, dt),
            PlayerMode::Walk => walk_step(&mut player, &keys, &store, &registry.0, dt),
        }
    }
}

fn fly_step(player: &mut Player, keys: &ButtonInput<KeyCode>, dt: f32) {
    let yaw_rotation = Quat::from_rotation_y(player.yaw);
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
        return;
    }

    let mut speed = FLY_SPEED;
    if keys.pressed(KeyCode::ShiftLeft) {
        speed *= FLY_BOOST_MULTIPLIER;
    }
    player.feet += direction.normalize() * speed * dt;
}

fn walk_step(
    player: &mut Player,
    keys: &ButtonInput<KeyCode>,
    store: &ChunkStore,
    registry: &tsumiki_world::BlockRegistry,
    dt: f32,
) {
    let feet_block = IVec3::new(
        player.feet.x.floor() as i32,
        player.feet.y.floor() as i32,
        player.feet.z.floor() as i32,
    );
    if !view::is_chunk_loaded(store, feet_block) {
        // Freeze entirely: no gravity accumulation, no movement, until the
        // ground beneath the player is actually known.
        return;
    }

    // Jump uses last frame's grounded state, applied before this frame's
    // gravity so the initial jump velocity isn't immediately eaten by it.
    if keys.just_pressed(KeyCode::Space) && player.on_ground {
        player.velocity.y = JUMP_SPEED;
    }
    player.velocity.y += GRAVITY * dt;

    let yaw_rotation = Quat::from_rotation_y(player.yaw);
    let forward = yaw_rotation * Vec3::NEG_Z;
    let right = yaw_rotation * Vec3::X;
    let mut wish = Vec3::ZERO;
    if keys.pressed(KeyCode::KeyW) {
        wish += forward;
    }
    if keys.pressed(KeyCode::KeyS) {
        wish -= forward;
    }
    if keys.pressed(KeyCode::KeyD) {
        wish += right;
    }
    if keys.pressed(KeyCode::KeyA) {
        wish -= right;
    }
    let horizontal = if wish != Vec3::ZERO {
        wish.normalize() * WALK_SPEED
    } else {
        Vec3::ZERO
    };
    player.velocity.x = horizontal.x;
    player.velocity.z = horizontal.z;

    let delta = player.velocity * dt;
    let result = substep_move(player.feet, delta, |pos| {
        view::block_at(store, pos)
            .map(|block| registry.get(block).solid)
            .unwrap_or(false)
    });
    player.feet += result.moved;
    if result.hit_y {
        player.velocity.y = 0.0;
    }
    player.on_ground = result.on_ground;
}

/// Moves `feet` by `delta`, splitting it into substeps of at most
/// [`MAX_SUBSTEP`] per axis so `physics::move_aabb`'s no-tunneling guarantee
/// (valid for `|delta| <= 1` block) holds with margin even for a hitching
/// frame or a fast fly-to-walk transition.
fn substep_move(feet: Vec3, delta: Vec3, is_solid: impl Fn(IVec3) -> bool) -> MoveResult {
    let max_component = delta.x.abs().max(delta.y.abs()).max(delta.z.abs());
    let steps = ((max_component / MAX_SUBSTEP).ceil() as u32).max(1);
    let step_delta = delta / steps as f32;

    let mut current_feet = feet;
    let mut total = MoveResult::default();
    for _ in 0..steps {
        let step = physics::move_aabb(Aabb::player(current_feet), step_delta, &is_solid);
        current_feet += step.moved;
        total.moved += step.moved;
        total.hit_x |= step.hit_x;
        total.hit_y |= step.hit_y;
        total.hit_z |= step.hit_z;
        total.on_ground = step.on_ground;
    }
    total
}

/// Derives the camera's `Transform` from `feet` + eye height and yaw/pitch.
/// Runs every frame, after look/movement, regardless of `spawned` (so the
/// camera still exists at a sane vantage point before spawn resolves).
fn sync_camera_transform(mut players: Query<(&Player, &mut Transform)>) {
    for (player, mut transform) in &mut players {
        transform.translation = player.feet + Vec3::Y * PLAYER_EYE_HEIGHT;
        transform.rotation = Quat::from_euler(EulerRot::YXZ, player.yaw, player.pitch, 0.0);
    }
}

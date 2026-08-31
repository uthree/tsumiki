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
use tsumiki_world::blocks;
use tsumiki_world::physics::{
    self, Aabb, GRAVITY, JUMP_SPEED, MoveResult, PLAYER_EYE_HEIGHT, WALK_SPEED,
};

use crate::pause;
use crate::settings::Settings;
use crate::state;
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

/// Gravity multiplier while swimming (roadmap.md M4): falling/sinking is
/// slowed to about a quarter speed.
const SWIM_GRAVITY_SCALE: f32 = 0.25;
/// Per-frame-equivalent (60fps) velocity retention while swimming, applied
/// dt-scaled so it stays frame-rate independent; gives swimming a floaty
/// inertia instead of walking's instant snap-to-target-speed.
const SWIM_DAMPING: f32 = 0.85;
/// Vertical speed while holding Space underwater (not at the surface).
const SWIM_UP_SPEED: f32 = 5.0;

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
    /// `true` when the block at the player's feet/eye is water, updated once
    /// per frame by [`update_water_flags`] regardless of `mode`. Read by
    /// [`crate::damage`] (drowning), [`crate::health`] (air HUD) and
    /// [`crate::underwater`] (screen tint).
    pub feet_in_water: bool,
    pub eye_in_water: bool,
    /// Downward impact speed (blocks/s, positive magnitude) on the exact
    /// frame the player lands from a fall in `Walk` mode; `None` every other
    /// frame. Set by [`walk_step`]; consumed by [`crate::damage`].
    pub landed_this_frame: Option<f32>,
}

/// Wires the player-controller systems into `app`. The player entity (in its
/// "waiting for spawn" state) is spawned on entering [`AppState::InGame`];
/// none of these systems (including cursor-grab-on-click) run in the menu.
pub fn install(app: &mut App) {
    app.add_systems(OnEnter(AppState::InGame), spawn_player)
        .add_systems(OnExit(AppState::InGame), despawn_player)
        .add_systems(
            Update,
            (
                grab_cursor,
                look,
                toggle_mode,
                update_water_flags,
                movement,
                sync_camera_transform,
            )
                .chain()
                .run_if(in_state(AppState::InGame))
                .run_if(pause::is_playing)
                .run_if(state::is_alive),
        )
        // FOV applies live even while paused/settings is open, and every
        // frame (not just on `Settings` change) so a freshly spawned camera
        // picks up the configured value immediately.
        .add_systems(Update, apply_fov.run_if(in_state(AppState::InGame)));
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
            feet_in_water: false,
            eye_in_water: false,
            landed_this_frame: None,
        },
    ));
}

/// Grabs the cursor on left click. Releasing it is [`crate::pause`]'s job
/// now (Escape opens the pause menu, which releases the cursor as part of
/// that transition) — this system only ever grabs, and only runs while
/// [`crate::pause::is_playing`], so a click on a pause/settings button can
/// never be mistaken for a grab click.
///
/// `pub(crate)` so [`crate::interact`] can order its click handling relative
/// to this system: it must run *before* this system so that the very click
/// that grabs the cursor is not also seen as a "grabbed" click by
/// break/place handling.
pub(crate) fn grab_cursor(
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    mut windows: Query<&mut CursorOptions, With<PrimaryWindow>>,
) {
    let Ok(mut cursor) = windows.single_mut() else {
        return;
    };
    if mouse_buttons.just_pressed(MouseButton::Left) {
        cursor.grab_mode = CursorGrabMode::Locked;
        cursor.visible = false;
    }
}

fn look(
    mouse_motion: Res<AccumulatedMouseMotion>,
    windows: Query<&CursorOptions, With<PrimaryWindow>>,
    settings: Res<Settings>,
    mut players: Query<&mut Player>,
) {
    let Ok(cursor) = windows.single() else {
        return;
    };
    if cursor.grab_mode == CursorGrabMode::None || mouse_motion.delta == Vec2::ZERO {
        return;
    }
    let sensitivity = MOUSE_SENSITIVITY * settings.mouse_sensitivity;
    for mut player in &mut players {
        player.yaw -= mouse_motion.delta.x * sensitivity;
        player.pitch =
            (player.pitch - mouse_motion.delta.y * sensitivity).clamp(-PITCH_LIMIT, PITCH_LIMIT);
    }
}

/// Part of the `OnExit(AppState::InGame)` "despawn everything in-game"
/// contract (see `pause` module docs): despawns the player entity (camera +
/// controller state) so re-entry spawns a fresh one.
fn despawn_player(mut commands: Commands, players: Query<Entity, With<Player>>) {
    for entity in &players {
        commands.entity(entity).despawn();
    }
}

/// Applies [`Settings::fov_degrees`] to the player camera's projection every
/// frame (see [`install`] for why this isn't change-gated).
fn apply_fov(settings: Res<Settings>, mut projections: Query<&mut Projection, With<Player>>) {
    for mut projection in &mut projections {
        if let Projection::Perspective(perspective) = projection.as_mut() {
            perspective.fov = settings.fov_degrees.to_radians();
        }
    }
}

/// `F` only toggles Walk/Fly in creative (roadmap.md M4); survival ignores
/// it. The screenshot orchestrator's direct `player.mode = Fly` override
/// (`crate::screenshot::position_camera_for_capture`) bypasses this
/// entirely, unaffected.
fn toggle_mode(
    keys: Res<ButtonInput<KeyCode>>,
    mode: Res<state::GameMode>,
    mut players: Query<&mut Player>,
) {
    if !keys.just_pressed(KeyCode::KeyF) || mode.is_survival() {
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

/// Floors a world-space position into its containing block position.
fn block_pos(p: Vec3) -> IVec3 {
    IVec3::new(p.x.floor() as i32, p.y.floor() as i32, p.z.floor() as i32)
}

/// Recomputes [`Player::feet_in_water`]/[`Player::eye_in_water`] every frame,
/// in every mode (not just `Walk`), so HUD/tint/damage systems relying on
/// them stay correct even while flying through water.
fn update_water_flags(store: Res<ChunkStore>, mut players: Query<&mut Player>) {
    for mut player in &mut players {
        if !player.spawned {
            player.feet_in_water = false;
            player.eye_in_water = false;
            continue;
        }
        let feet_block = block_pos(player.feet);
        let eye_block = block_pos(player.feet + Vec3::Y * PLAYER_EYE_HEIGHT);
        player.feet_in_water = view::block_at(&store, feet_block) == Some(blocks::WATER);
        player.eye_in_water = view::block_at(&store, eye_block) == Some(blocks::WATER);
    }
}

fn walk_step(
    player: &mut Player,
    keys: &ButtonInput<KeyCode>,
    store: &ChunkStore,
    registry: &tsumiki_world::BlockRegistry,
    dt: f32,
) {
    let feet_block = block_pos(player.feet);
    if !view::is_chunk_loaded(store, feet_block) {
        // Freeze entirely: no gravity accumulation, no movement, until the
        // ground beneath the player is actually known.
        return;
    }

    let previous_on_ground = player.on_ground;
    // Swimming only applies while actually afloat; once standing on solid
    // ground beneath water, treat it as ordinary ground movement (this is
    // also what makes "on_ground... allow normal jump" fall out naturally).
    let swimming = (player.feet_in_water || player.eye_in_water) && !previous_on_ground;

    if swimming {
        // Chest-deep with the head above water: a normal jump pops the
        // player out onto the bank/boat, instead of the smaller swim-up nudge.
        let near_surface = player.feet_in_water && !player.eye_in_water;
        if keys.just_pressed(KeyCode::Space) && near_surface {
            player.velocity.y = JUMP_SPEED;
        } else if keys.pressed(KeyCode::Space) {
            player.velocity.y = SWIM_UP_SPEED;
        } else {
            player.velocity.y += GRAVITY * SWIM_GRAVITY_SCALE * dt;
        }
    } else {
        // Jump uses last frame's grounded state, applied before this frame's
        // gravity so the initial jump velocity isn't immediately eaten by it.
        if keys.just_pressed(KeyCode::Space) && previous_on_ground {
            player.velocity.y = JUMP_SPEED;
        }
        player.velocity.y += GRAVITY * dt;
    }

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

    if swimming {
        // dt-scaled damping toward the wish velocity rather than walking's
        // instant snap, so swimming carries a bit of floaty inertia.
        let damp = SWIM_DAMPING.powf(dt * 60.0);
        player.velocity.x = player.velocity.x * damp + horizontal.x * (1.0 - damp);
        player.velocity.z = player.velocity.z * damp + horizontal.z * (1.0 - damp);
        if !keys.pressed(KeyCode::Space) {
            player.velocity.y *= damp;
        }
    } else {
        player.velocity.x = horizontal.x;
        player.velocity.z = horizontal.z;
    }

    let delta = player.velocity * dt;
    let result = substep_move(player.feet, delta, |pos| {
        view::block_at(store, pos)
            .map(|block| registry.get(block).solid)
            .unwrap_or(false)
    });
    player.feet += result.moved;

    // Captured before this frame's collision response zeroes it, so it's the
    // actual impact speed (see `crate::damage::fall_damage`'s docs for why
    // that alone is enough to recover the total fall height).
    let pre_collision_vy = player.velocity.y;
    if result.hit_y {
        player.velocity.y = 0.0;
    }
    player.on_ground = result.on_ground;

    player.landed_this_frame = if !previous_on_ground && result.on_ground && pre_collision_vy < 0.0
    {
        Some(-pre_collision_vy)
    } else {
        None
    };
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

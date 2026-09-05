//! Player controller: first-person camera driven by a feet position, with a
//! walking mode (voxel AABB collision + gravity) and creative flight.
//!
//! - Mouse look and cursor grab/release behavior match the old fly camera.
//! - `F` or a double tap of Space toggles creative flight. Space ascends,
//!   Shift descends, and Ctrl boosts flight speed.
//! - Holding `C` gives 4x optical zoom and reduced mouse sensitivity.
//! - The camera `Transform` is a pure function of `feet` + eye height and
//!   yaw/pitch; it is written once per frame by [`sync_camera_transform`],
//!   after input/physics have updated the [`Player`] component.
//! - Until [`crate::net`] resolves the spawn point, `Player::spawned` is
//!   `false` and movement/physics are skipped entirely (the camera just
//!   sits at a vantage point above the default spawn column).

use bevy::input::mouse::AccumulatedMouseMotion;
use bevy::prelude::*;
use bevy::time::Real;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use std::f32::consts::FRAC_PI_2;
use std::time::Duration;
use tsumiki_world::blocks;
use tsumiki_world::lod::{MAX_LOD, chunk_span};
use tsumiki_world::physics::{
    self, Aabb, GRAVITY, JUMP_SPEED, MoveResult, PLAYER_EYE_HEIGHT, WALK_SPEED,
};

use crate::lod_view;
use crate::pause;
use crate::settings::{self, Settings};
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

/// Perspective far-plane distance, derived from the deepest LOD ring's outer
/// bound at the *maximum* configurable view distance rather than picked
/// arbitrarily.
///
/// It has to cover the maximum ([`settings::VIEW_DISTANCE_RANGE`]'s upper
/// end), not just the default: `Settings::view_distance_chunks` can change at
/// runtime, and a far plane sized only for the default would start clipping
/// the horizon the moment a player raises the slider.
/// [`lod_view::outer_bound`] at [`MAX_LOD`] is where the outermost LOD
/// ring -- the farthest terrain the client ever streams -- ends; one more
/// [`chunk_span`] is added on top because [`lod_view`]'s despawn hysteresis
/// keeps a chunk alive up to one chunk span past that band, and a chunk's own
/// footprint extends roughly another chunk span beyond its center, so
/// geometry can legitimately render a little past the nominal band edge.
///
/// This costs nothing in depth precision: Bevy's `PerspectiveProjection`
/// (`bevy_camera::projection::CameraProjection for PerspectiveProjection`)
/// always builds an infinite reversed-Z matrix
/// (`Mat4::perspective_infinite_reverse_rh(fov, aspect, near)`) regardless of
/// `far` -- `far` only bounds the culling frustum
/// (`CameraProjection::far`/`compute_frustum`), never the projection or the
/// depth buffer -- so `near` does not need to move to compensate.
fn far_plane_distance() -> f32 {
    let vd_blocks_max = *settings::VIEW_DISTANCE_RANGE.end() * tsumiki_world::CHUNK_SIZE as i32;
    let horizon = lod_view::outer_bound(MAX_LOD, vd_blocks_max);
    (horizon + chunk_span(MAX_LOD)) as f32
}

const FLY_SPEED: f32 = 24.0;
const FLY_BOOST_MULTIPLIER: f32 = 4.0;
const MOUSE_SENSITIVITY: f32 = 0.0025;
const FLIGHT_DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(300);
const ZOOM_MAGNIFICATION: f32 = 4.0;

#[derive(Default)]
struct FlightTap {
    last_press: Option<Duration>,
}

impl FlightTap {
    fn toggle_requested(
        &mut self,
        now: Duration,
        allowed: bool,
        shortcut: bool,
        jump: bool,
    ) -> bool {
        if !allowed {
            self.last_press = None;
            return false;
        }
        if shortcut {
            self.last_press = None;
            return true;
        }
        if !jump {
            return false;
        }
        if self.last_press.is_some_and(|previous| {
            now.checked_sub(previous)
                .is_some_and(|elapsed| elapsed <= FLIGHT_DOUBLE_TAP_WINDOW)
        }) {
            self.last_press = None;
            true
        } else {
            self.last_press = Some(now);
            false
        }
    }
}

/// Transient input state must not survive UI transitions or world sessions.
#[derive(Resource, Default)]
struct CameraInput {
    active: bool,
    zoomed: bool,
    flight_tap: FlightTap,
}

fn reset_camera_input(mut input: ResMut<CameraInput>) {
    *input = CameraInput::default();
}

fn zoom_fov(normal_fov: f32, zoomed: bool) -> f32 {
    if zoomed {
        2.0 * ((normal_fov * 0.5).tan() / ZOOM_MAGNIFICATION).atan()
    } else {
        normal_fov
    }
}

fn zoom_sensitivity(zoomed: bool) -> f32 {
    if zoomed {
        ZOOM_MAGNIFICATION.recip()
    } else {
        1.0
    }
}

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
    app.init_resource::<CameraInput>()
        .add_systems(
            OnEnter(AppState::InGame),
            (reset_camera_input, spawn_player),
        )
        .add_systems(
            OnExit(AppState::InGame),
            (reset_camera_input, despawn_player),
        )
        .add_systems(
            Update,
            (
                grab_cursor,
                look,
                update_water_flags,
                movement,
                sync_camera_transform,
            )
                .chain()
                .run_if(in_state(AppState::InGame))
                .run_if(pause::is_playing)
                .run_if(state::is_alive),
        )
        // This also runs while paused/dead, so stale taps and zoom are
        // cleared even when gameplay input systems are gated off.
        .add_systems(
            Update,
            update_camera_input
                .after(grab_cursor)
                .before(look)
                .run_if(in_state(AppState::InGame)),
        )
        // FOV applies live even while paused/settings is open, and every
        // frame (not just on `Settings` change) so a freshly spawned camera
        // picks up the configured value immediately.
        .add_systems(
            Update,
            apply_fov
                .after(update_camera_input)
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
            far: far_plane_distance(),
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
    input: Res<CameraInput>,
    settings: Res<Settings>,
    mut players: Query<&mut Player>,
) {
    if !input.active || mouse_motion.delta == Vec2::ZERO {
        return;
    }
    let sensitivity =
        MOUSE_SENSITIVITY * settings.mouse_sensitivity * zoom_sensitivity(input.zoomed);
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
fn apply_fov(
    settings: Res<Settings>,
    input: Res<CameraInput>,
    mut projections: Query<&mut Projection, With<Player>>,
) {
    for mut projection in &mut projections {
        if let Projection::Perspective(perspective) = projection.as_mut() {
            perspective.fov = zoom_fov(settings.fov_degrees.to_radians(), input.zoomed);
        }
    }
}

/// Checks real elapsed time rather than frame count, and consumes successful
/// tap pairs so a third tap cannot immediately toggle flight back off.
#[allow(clippy::too_many_arguments)]
fn update_camera_input(
    time: Res<Time<Real>>,
    keys: Res<ButtonInput<KeyCode>>,
    mode: Res<state::GameMode>,
    game: Res<state::GameState>,
    pause: Res<State<pause::PauseState>>,
    windows: Query<&CursorOptions, With<PrimaryWindow>>,
    mut input: ResMut<CameraInput>,
    mut players: Query<&mut Player>,
) {
    let Ok(mut player) = players.single_mut() else {
        *input = CameraInput::default();
        return;
    };
    let active = player.spawned && !game.dead
        && *pause.get() == pause::PauseState::Playing
        && windows.single().is_ok_and(|cursor| cursor.grab_mode == CursorGrabMode::Locked)
        // Suppress input on the opening frame too, before the UI state
        // transition is applied by Bevy's next frame.
        && !keys.just_pressed(KeyCode::Escape) && !keys.just_pressed(KeyCode::KeyE);
    if !active {
        *input = CameraInput::default();
        return;
    }
    input.active = true;
    input.zoomed = keys.pressed(KeyCode::KeyC);
    if input.flight_tap.toggle_requested(
        time.elapsed(),
        !mode.is_survival(),
        keys.just_pressed(KeyCode::KeyF),
        keys.just_pressed(KeyCode::Space),
    ) {
        player.mode = match player.mode {
            PlayerMode::Walk => PlayerMode::Fly,
            PlayerMode::Fly => PlayerMode::Walk,
        };
        player.velocity = Vec3::ZERO;
        player.on_ground = false;
        player.landed_this_frame = None;
    }
}

fn movement(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    store: Res<ChunkStore>,
    registry: Res<view::Registry>,
    input: Res<CameraInput>,
    mode: Res<state::GameMode>,
    mut players: Query<&mut Player>,
) {
    let dt = time.delta_secs();
    for mut player in &mut players {
        if !player.spawned {
            continue;
        }
        match player.mode {
            PlayerMode::Fly if input.active && !mode.is_survival() => {
                fly_step(&mut player, &keys, dt)
            }
            PlayerMode::Fly => {}
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
    if keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]) {
        direction -= Vec3::Y;
    }
    if direction == Vec3::ZERO {
        return;
    }

    let mut speed = FLY_SPEED;
    if keys.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight]) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::system::RunSystemOnce;

    fn test_player() -> Player {
        Player {
            feet: Vec3::ZERO,
            velocity: Vec3::ZERO,
            yaw: 0.0,
            pitch: 0.0,
            mode: PlayerMode::Walk,
            spawned: true,
            on_ground: true,
            feet_in_water: false,
            eye_in_water: false,
            landed_this_frame: None,
        }
    }

    fn control_app() -> (App, Entity, Entity) {
        let mut app = App::new();
        app.init_resource::<CameraInput>()
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<AccumulatedMouseMotion>()
            .init_resource::<Time<Real>>()
            .init_resource::<state::GameMode>()
            .init_resource::<state::GameState>()
            .init_resource::<Settings>()
            .insert_resource(State::new(pause::PauseState::Playing))
            .add_systems(Update, (update_camera_input, look, apply_fov).chain());
        let player = app
            .world_mut()
            .spawn((
                test_player(),
                Projection::Perspective(PerspectiveProjection::default()),
            ))
            .id();
        let window = app
            .world_mut()
            .spawn((
                PrimaryWindow,
                CursorOptions {
                    grab_mode: CursorGrabMode::Locked,
                    ..default()
                },
            ))
            .id();
        (app, player, window)
    }

    fn step(app: &mut App, milliseconds: u64) {
        app.world_mut()
            .resource_mut::<Time<Real>>()
            .advance_by(Duration::from_millis(milliseconds));
        app.update();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .clear();
    }

    fn press(app: &mut App, key: KeyCode) {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(key);
    }

    fn release(app: &mut App, key: KeyCode) {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .release(key);
    }

    fn fov(app: &App, player: Entity) -> f32 {
        let Projection::Perspective(perspective) = app.world().get::<Projection>(player).unwrap()
        else {
            panic!("perspective camera expected");
        };
        perspective.fov
    }

    #[test]
    fn double_space_toggles_flight_once_and_consumes_the_pair() {
        let (mut app, player, _) = control_app();
        press(&mut app, KeyCode::Space);
        step(&mut app, 0);
        assert_eq!(
            app.world().get::<Player>(player).unwrap().mode,
            PlayerMode::Walk
        );
        release(&mut app, KeyCode::Space);
        step(&mut app, 100);
        press(&mut app, KeyCode::Space);
        step(&mut app, 100);
        assert_eq!(
            app.world().get::<Player>(player).unwrap().mode,
            PlayerMode::Fly
        );
        step(&mut app, 50);
        assert_eq!(
            app.world().get::<Player>(player).unwrap().mode,
            PlayerMode::Fly,
            "holding Space must not count as more taps"
        );
        release(&mut app, KeyCode::Space);
        step(&mut app, 20);
        press(&mut app, KeyCode::Space);
        step(&mut app, 20);
        assert_eq!(
            app.world().get::<Player>(player).unwrap().mode,
            PlayerMode::Fly,
            "a third tap starts a new pair"
        );
    }

    #[test]
    fn double_tap_window_is_inclusive_and_expired_taps_restart_it() {
        let mut taps = FlightTap::default();
        assert!(!taps.toggle_requested(Duration::ZERO, true, false, true));
        assert!(taps.toggle_requested(Duration::from_millis(300), true, false, true));
        assert!(!taps.toggle_requested(Duration::from_millis(400), true, false, true));
        assert!(!taps.toggle_requested(Duration::from_millis(701), true, false, true));
        assert!(taps.toggle_requested(Duration::from_millis(800), true, false, true));
    }

    #[test]
    fn flight_shortcut_requires_creative_spawn_alive_playing_and_locked_cursor() {
        for blocked in 0..6 {
            let (mut app, player, window) = control_app();
            match blocked {
                0 => {
                    app.world_mut().resource_mut::<state::GameMode>().0 =
                        tsumiki_protocol::GameMode::Survival
                }
                1 => app.world_mut().get_mut::<Player>(player).unwrap().spawned = false,
                2 => app.world_mut().resource_mut::<state::GameState>().dead = true,
                3 => app
                    .world_mut()
                    .insert_resource(State::new(pause::PauseState::Paused)),
                4 => {
                    app.world_mut()
                        .get_mut::<CursorOptions>(window)
                        .unwrap()
                        .grab_mode = CursorGrabMode::None
                }
                _ => {
                    app.world_mut()
                        .get_mut::<CursorOptions>(window)
                        .unwrap()
                        .grab_mode = CursorGrabMode::Confined
                }
            }
            press(&mut app, KeyCode::KeyF);
            step(&mut app, 0);
            assert_eq!(
                app.world().get::<Player>(player).unwrap().mode,
                PlayerMode::Walk,
                "gate {blocked}"
            );
        }
        let (mut app, player, _) = control_app();
        app.world_mut().get_mut::<Player>(player).unwrap().velocity = Vec3::new(1.0, -10.0, 2.0);
        press(&mut app, KeyCode::KeyF);
        step(&mut app, 0);
        assert_eq!(
            app.world().get::<Player>(player).unwrap().mode,
            PlayerMode::Fly
        );
        assert_eq!(
            app.world().get::<Player>(player).unwrap().velocity,
            Vec3::ZERO
        );
    }

    #[test]
    fn ui_cursor_and_death_cancel_pending_taps_and_zoom() {
        for blocked in 0..5 {
            let (mut app, player, window) = control_app();
            press(&mut app, KeyCode::Space);
            press(&mut app, KeyCode::KeyC);
            step(&mut app, 0);
            assert!(app.world().resource::<CameraInput>().zoomed);
            release(&mut app, KeyCode::Space);
            match blocked {
                0 => app
                    .world_mut()
                    .insert_resource(State::new(pause::PauseState::Paused)),
                1 => app
                    .world_mut()
                    .insert_resource(State::new(pause::PauseState::Settings)),
                2 => app
                    .world_mut()
                    .insert_resource(State::new(pause::PauseState::Inventory)),
                3 => {
                    app.world_mut()
                        .get_mut::<CursorOptions>(window)
                        .unwrap()
                        .grab_mode = CursorGrabMode::None
                }
                _ => app.world_mut().resource_mut::<state::GameState>().dead = true,
            }
            step(&mut app, 50);
            assert!(!app.world().resource::<CameraInput>().zoomed);
            assert!(
                app.world()
                    .resource::<CameraInput>()
                    .flight_tap
                    .last_press
                    .is_none()
            );
            app.world_mut()
                .insert_resource(State::new(pause::PauseState::Playing));
            app.world_mut()
                .get_mut::<CursorOptions>(window)
                .unwrap()
                .grab_mode = CursorGrabMode::Locked;
            app.world_mut().resource_mut::<state::GameState>().dead = false;
            press(&mut app, KeyCode::Space);
            step(&mut app, 50);
            assert_eq!(
                app.world().get::<Player>(player).unwrap().mode,
                PlayerMode::Walk,
                "tap must not bridge inactive gate {blocked}"
            );
        }
    }

    #[test]
    fn session_reset_clears_all_transient_camera_input() {
        let (mut app, _, _) = control_app();
        press(&mut app, KeyCode::Space);
        press(&mut app, KeyCode::KeyC);
        step(&mut app, 0);
        app.world_mut().run_system_once(reset_camera_input).unwrap();
        let input = app.world().resource::<CameraInput>();
        assert!(!input.active && !input.zoomed && input.flight_tap.last_press.is_none());
    }

    #[test]
    fn space_ascends_shift_descends_and_control_boosts_flight() {
        for (keys, expected) in [
            (vec![KeyCode::Space], Vec3::Y * 12.0),
            (vec![KeyCode::ShiftLeft], Vec3::NEG_Y * 12.0),
            (vec![KeyCode::ShiftRight], Vec3::NEG_Y * 12.0),
            (vec![KeyCode::Space, KeyCode::ControlLeft], Vec3::Y * 48.0),
            (
                vec![KeyCode::KeyW, KeyCode::ControlRight],
                Vec3::NEG_Z * 48.0,
            ),
            (vec![KeyCode::Space, KeyCode::ShiftLeft], Vec3::ZERO),
            (vec![KeyCode::ControlLeft], Vec3::ZERO),
        ] {
            let mut player = test_player();
            let mut input = ButtonInput::default();
            for key in keys {
                input.press(key);
            }
            fly_step(&mut player, &input, 0.5);
            assert!(player.feet.abs_diff_eq(expected, 1e-5));
        }
    }

    #[test]
    fn zoom_has_fourfold_optical_magnification_at_every_supported_fov() {
        for degrees in [50.0_f32, 70.0, 90.0, 110.0] {
            let normal = degrees.to_radians();
            let zoomed = zoom_fov(normal, true);
            assert!(((normal * 0.5).tan() / (zoomed * 0.5).tan() - 4.0).abs() < 1e-5);
            assert_eq!(zoom_fov(normal, false), normal);
        }
    }

    #[test]
    fn zoom_release_and_settings_screen_restore_latest_fov_without_mutating_settings() {
        let (mut app, player, _) = control_app();
        let original = *app.world().resource::<Settings>();
        press(&mut app, KeyCode::KeyC);
        step(&mut app, 0);
        assert_eq!(
            fov(&app, player),
            zoom_fov(original.fov_degrees.to_radians(), true)
        );
        assert_eq!(*app.world().resource::<Settings>(), original);
        release(&mut app, KeyCode::KeyC);
        step(&mut app, 10);
        assert_eq!(fov(&app, player), original.fov_degrees.to_radians());
        press(&mut app, KeyCode::KeyC);
        step(&mut app, 10);
        app.world_mut()
            .insert_resource(State::new(pause::PauseState::Settings));
        app.world_mut().resource_mut::<Settings>().fov_degrees = 100.0;
        step(&mut app, 10);
        assert_eq!(fov(&app, player), 100.0_f32.to_radians());
        release(&mut app, KeyCode::KeyC);
        app.world_mut()
            .insert_resource(State::new(pause::PauseState::Playing));
        step(&mut app, 10);
        assert_eq!(fov(&app, player), 100.0_f32.to_radians());
    }

    #[test]
    fn zoom_reduces_actual_mouse_rotation_by_the_same_magnification() {
        let (mut normal, normal_player, _) = control_app();
        let (mut zoomed, zoomed_player, _) = control_app();
        for app in [&mut normal, &mut zoomed] {
            app.world_mut()
                .resource_mut::<AccumulatedMouseMotion>()
                .delta = Vec2::new(8.0, 4.0);
        }
        press(&mut zoomed, KeyCode::KeyC);
        step(&mut normal, 0);
        step(&mut zoomed, 0);
        let normal = normal.world().get::<Player>(normal_player).unwrap();
        let zoomed = zoomed.world().get::<Player>(zoomed_player).unwrap();
        assert!((normal.yaw / zoomed.yaw - 4.0).abs() < 1e-5);
        assert!((normal.pitch / zoomed.pitch - 4.0).abs() < 1e-5);
    }

    #[test]
    fn far_plane_matches_the_outermost_lod_ring_plus_one_chunk_span_at_max_view_distance() {
        let vd_blocks_max = *settings::VIEW_DISTANCE_RANGE.end() * tsumiki_world::CHUNK_SIZE as i32;
        let expected = (lod_view::outer_bound(MAX_LOD, vd_blocks_max) + chunk_span(MAX_LOD)) as f32;
        assert_eq!(far_plane_distance(), expected);
    }

    #[test]
    fn far_plane_covers_the_full_configurable_view_distance_range() {
        // Every view distance the player can dial in via Settings must fall
        // well within the far plane, not just the default.
        for vd_chunks in [
            *settings::VIEW_DISTANCE_RANGE.start(),
            *settings::VIEW_DISTANCE_RANGE.end(),
        ] {
            let vd_blocks = vd_chunks * tsumiki_world::CHUNK_SIZE as i32;
            let horizon = lod_view::outer_bound(MAX_LOD, vd_blocks) as f32;
            assert!(
                far_plane_distance() > horizon,
                "far plane must clear the LOD horizon at view distance {vd_chunks} chunks"
            );
        }
    }
}

//! Remote player rendering: toy-like avatars for other connected clients,
//! screen-space name tags, and network interpolation (design.md §1.4,
//! roadmap.md M2).
//!
//! - [`InterpBuffer`] (pure, Bevy-free, unit-tested below): buffers
//!   [`PlayerSave`] samples by local receive time and renders a smoothed
//!   pose delayed slightly behind the newest sample.
//! - [`RemotePlayers`] resource: per-client avatar/name-tag entities and
//!   their buffers. Fed by [`crate::net`] on `PlayerJoined`/`PlayerMoved`/
//!   `PlayerLeft` via [`spawn_remote_player`]/[`push_sample`]/
//!   [`despawn_remote_player`].
//! - Per-frame systems: sample each buffer into the avatar's `Transform`
//!   ([`interpolate_remote_players`]), then project the avatar's head into
//!   screen space to position its name tag, hiding it when the projection
//!   fails (off-screen/behind the camera) or the avatar is too far
//!   ([`position_name_tags`]).

use std::collections::{HashMap, VecDeque};

use bevy::prelude::*;
use tsumiki_protocol::{ClientId, PlayerSave};
use tsumiki_world::physics::{PLAYER_HEIGHT, PLAYER_WIDTH};

use crate::AppState;
use crate::UiFont;
use crate::camera::Player;

/// Render delay behind the newest received sample. Smooths out jitter and
/// packet reordering at the cost of a small, fixed latency.
pub const INTERP_DELAY: f64 = 0.15;

/// A gap between consecutive samples larger than this is treated as a
/// teleport, reconnect, or long stall: interpolating across it would look
/// like the avatar sliding across the world, so old samples are dropped
/// instead ("snapping" straight to the new state once it's due).
const SNAP_GAP_SECS: f64 = 1.0;

/// Samples older than this (relative to the most recently pushed sample) are
/// pruned so a buffer fed indefinitely doesn't grow without bound.
const SAMPLE_RETENTION_SECS: f64 = 5.0;

/// Name tags for avatars farther than this from the camera are hidden.
const NAME_TAG_MAX_DISTANCE: f32 = 60.0;

/// Font size, in logical pixels, for a remote player's name tag.
const NAME_TAG_FONT_SIZE: f32 = 16.0;

/// A buffered, interpolated stream of one remote player's replicated state.
///
/// Pure and Bevy-free (only depends on [`PlayerSave`] and [`bevy::math`]
/// vector types) so it is unit-testable in isolation; see the tests below
/// for the exact interpolation/clamping/snap contract.
#[derive(Debug, Clone, Default)]
pub(crate) struct InterpBuffer {
    /// Ascending by time; enforced by [`push`](Self::push) rejecting
    /// out-of-order/duplicate timestamps.
    samples: VecDeque<(f64, PlayerSave)>,
}

impl InterpBuffer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Records a sample received at local time `t`.
    ///
    /// - Rejects (no-ops) a sample at or before the last recorded time: the
    ///   network can reorder or duplicate packets, and the buffer must never
    ///   go backward.
    /// - Snaps when the gap since the previous sample exceeds
    ///   [`SNAP_GAP_SECS`]: older samples are dropped so playback jumps
    ///   straight to the new state once it's due, rather than interpolating
    ///   across a teleport-looking gap.
    /// - Prunes samples older than [`SAMPLE_RETENTION_SECS`] relative to
    ///   `t`, always keeping at least the newest one.
    pub(crate) fn push(&mut self, t: f64, state: PlayerSave) {
        if let Some(&(last_t, _)) = self.samples.back() {
            if t <= last_t {
                return;
            }
            if t - last_t > SNAP_GAP_SECS {
                self.samples.clear();
            }
        }
        self.samples.push_back((t, state));
        while self.samples.len() > 1 {
            let &(oldest_t, _) = self.samples.front().expect("just checked len > 1");
            if t - oldest_t > SAMPLE_RETENTION_SECS {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    /// Renders the buffer at `t_render` (normally `now - INTERP_DELAY`).
    ///
    /// - `None` if nothing has ever been pushed.
    /// - Linearly interpolates position and takes the shortest-arc lerp of
    ///   yaw between the two samples bracketing `t_render`.
    /// - Clamps to the newest sample when `t_render` is at or past it (never
    ///   extrapolates/overshoots), and to the oldest when `t_render`
    ///   predates the whole buffer (e.g. right after a fresh `PlayerJoined`,
    ///   before `INTERP_DELAY` worth of history has arrived).
    pub(crate) fn sample(&self, t_render: f64) -> Option<(Vec3, f32)> {
        let &(front_t, front_state) = self.samples.front()?;
        if t_render <= front_t {
            return Some((front_state.pos, front_state.yaw));
        }
        let &(back_t, back_state) = self.samples.back().expect("front exists, so back does too");
        if t_render >= back_t {
            return Some((back_state.pos, back_state.yaw));
        }
        for i in 0..self.samples.len() - 1 {
            let (t0, s0) = &self.samples[i];
            let (t1, s1) = &self.samples[i + 1];
            if t_render >= *t0 && t_render <= *t1 {
                let span = t1 - t0;
                let alpha = if span > f64::EPSILON {
                    ((t_render - t0) / span) as f32
                } else {
                    0.0
                };
                return Some((
                    s0.pos.lerp(s1.pos, alpha),
                    lerp_angle(s0.yaw, s1.yaw, alpha),
                ));
            }
        }
        unreachable!("t_render is bounded by the front/back checks above")
    }
}

/// Shortest-arc lerp between two angles in radians.
fn lerp_angle(a: f32, b: f32, t: f32) -> f32 {
    a + shortest_delta(a, b) * t
}

/// Signed shortest angular distance from `a` to `b`, in `(-PI, PI]`.
fn shortest_delta(a: f32, b: f32) -> f32 {
    let tau = std::f32::consts::TAU;
    let mut d = (b - a) % tau;
    if d > std::f32::consts::PI {
        d -= tau;
    } else if d < -std::f32::consts::PI {
        d += tau;
    }
    d
}

/// Deterministic hue in `[0, 360)` for a client id, well-distributed even
/// for small sequential ids (a plain `id % 360` would cluster consecutive
/// players' hues a few degrees apart).
fn hue_for_id(id: ClientId) -> f32 {
    // SplitMix64-style bit mixing.
    let mut x = id.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    (x % 360) as f32
}

/// Bright, saturated, toy-like avatar color (design.md §7: "pop / toy-like
/// tone"), deterministic per client id.
fn color_for_id(id: ClientId) -> Color {
    Color::hsl(hue_for_id(id), 0.85, 0.55)
}

/// The shared avatar mesh: every remote player is the same box shape (a
/// [`PLAYER_WIDTH`]×[`PLAYER_HEIGHT`]×[`PLAYER_WIDTH`] cuboid); only the
/// material color differs per player.
#[derive(Resource)]
pub(crate) struct AvatarMesh(Handle<Mesh>);

fn setup_avatar_mesh(mut commands: Commands, mut meshes: ResMut<Assets<Mesh>>) {
    let mesh = meshes.add(Mesh::from(Cuboid::new(
        PLAYER_WIDTH,
        PLAYER_HEIGHT,
        PLAYER_WIDTH,
    )));
    commands.insert_resource(AvatarMesh(mesh));
}

/// Marks a name tag `Text` UI node with the avatar entity it should track.
#[derive(Component)]
struct NameTagTarget(Entity);

struct RemoteEntry {
    avatar: Entity,
    name_tag: Entity,
    material: Handle<StandardMaterial>,
    buffer: InterpBuffer,
}

/// Live remote players: client id -> avatar/name-tag entities and
/// interpolation state. Populated/drained by [`crate::net`] as
/// `PlayerJoined`/`PlayerMoved`/`PlayerLeft` arrive.
#[derive(Resource, Default)]
pub(crate) struct RemotePlayers(HashMap<ClientId, RemoteEntry>);

/// Wires the remote-player resources and per-frame systems into `app`.
pub fn install(app: &mut App) {
    app.init_resource::<RemotePlayers>()
        .add_systems(Startup, setup_avatar_mesh)
        .add_systems(OnExit(AppState::InGame), teardown_remote_players)
        .add_systems(
            Update,
            (interpolate_remote_players, position_name_tags).chain(),
        );
}

/// Part of the `OnExit(AppState::InGame)` "despawn everything in-game"
/// contract (see `pause` module docs): despawns every remote avatar + name
/// tag, frees their materials, and clears [`RemotePlayers`] so re-entry
/// starts with none (fresh `PlayerJoined`s repopulate it).
fn teardown_remote_players(
    mut commands: Commands,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut remote_players: ResMut<RemotePlayers>,
) {
    for (_, entry) in remote_players.0.drain() {
        materials.remove(&entry.material);
        commands.entity(entry.avatar).despawn();
        commands.entity(entry.name_tag).despawn();
    }
}

/// The avatar's `Transform` (center of the box) for a given feet position and
/// yaw. Mirrors the box's placement relative to [`tsumiki_world::physics::Aabb::player`]'s
/// feet-anchored convention.
fn avatar_transform(state: PlayerSave) -> Transform {
    Transform::from_translation(state.pos + Vec3::Y * (PLAYER_HEIGHT / 2.0))
        .with_rotation(Quat::from_rotation_y(state.yaw))
}

/// Spawns a remote player's avatar + name tag. Called by [`crate::net`] on
/// `ServerToClient::PlayerJoined`.
// Takes its ECS dependencies as parameters because it is called from inside
// another system; the count is inherent.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_remote_player(
    commands: &mut Commands,
    avatar_mesh: &AvatarMesh,
    materials: &mut Assets<StandardMaterial>,
    remote_players: &mut RemotePlayers,
    now: f64,
    id: ClientId,
    name: &str,
    state: PlayerSave,
    font: &UiFont,
) {
    // Defensive: the protocol doesn't strictly guarantee a `PlayerLeft`
    // always precedes a re-`PlayerJoined` for the same id, so replace rather
    // than leak a stale entry.
    if remote_players.0.contains_key(&id) {
        despawn_remote_player(commands, materials, remote_players, id);
    }

    let material = materials.add(StandardMaterial {
        base_color: color_for_id(id),
        perceptual_roughness: 1.0,
        ..default()
    });

    let mut buffer = InterpBuffer::new();
    buffer.push(now, state);

    let avatar = commands
        .spawn((
            Mesh3d(avatar_mesh.0.clone()),
            MeshMaterial3d(material.clone()),
            avatar_transform(state),
        ))
        .id();

    let name_tag = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(0.0),
                top: Val::Px(0.0),
                ..default()
            },
            // Positioned/shown by `position_name_tags` once the camera
            // projection is known; stays hidden until then.
            Visibility::Hidden,
            Text::new(name),
            font.text(NAME_TAG_FONT_SIZE),
            TextColor(Color::WHITE),
            NameTagTarget(avatar),
        ))
        .id();

    remote_players.0.insert(
        id,
        RemoteEntry {
            avatar,
            name_tag,
            material,
            buffer,
        },
    );
}

/// Pushes a movement sample into `id`'s interpolation buffer. Called by
/// [`crate::net`] on `ServerToClient::PlayerMoved`. A sample for an id with
/// no active entry (shouldn't happen given the protocol always `PlayerJoined`s
/// first) is silently dropped.
pub(crate) fn push_sample(
    remote_players: &mut RemotePlayers,
    now: f64,
    id: ClientId,
    state: PlayerSave,
) {
    if let Some(entry) = remote_players.0.get_mut(&id) {
        entry.buffer.push(now, state);
    }
}

/// Despawns a remote player's avatar + name tag and frees its material.
/// Called by [`crate::net`] on `ServerToClient::PlayerLeft`.
pub(crate) fn despawn_remote_player(
    commands: &mut Commands,
    materials: &mut Assets<StandardMaterial>,
    remote_players: &mut RemotePlayers,
    id: ClientId,
) {
    if let Some(entry) = remote_players.0.remove(&id) {
        materials.remove(&entry.material);
        commands.entity(entry.avatar).despawn();
        commands.entity(entry.name_tag).despawn();
    }
}

/// Samples each remote player's buffer and writes the interpolated pose into
/// its avatar's `Transform`.
fn interpolate_remote_players(
    time: Res<Time>,
    remote_players: Res<RemotePlayers>,
    mut transforms: Query<&mut Transform>,
) {
    let t_render = time.elapsed_secs_f64() - INTERP_DELAY;
    for entry in remote_players.0.values() {
        let Some((pos, yaw)) = entry.buffer.sample(t_render) else {
            continue;
        };
        if let Ok(mut transform) = transforms.get_mut(entry.avatar) {
            transform.translation = pos + Vec3::Y * (PLAYER_HEIGHT / 2.0);
            transform.rotation = Quat::from_rotation_y(yaw);
        }
    }
}

/// Projects each avatar's head into screen space and positions its name tag
/// there, hiding it when the projection fails (behind the camera / outside
/// the frustum) or the avatar is beyond [`NAME_TAG_MAX_DISTANCE`].
fn position_name_tags(
    cameras: Query<(&Camera, &Transform), With<Player>>,
    avatar_transforms: Query<&Transform, Without<Node>>,
    mut tags: Query<(&mut Node, &mut Visibility, &NameTagTarget)>,
) {
    let Ok((camera, camera_transform)) = cameras.single() else {
        return;
    };
    // Built directly from this frame's just-written camera `Transform`
    // (`Player` is unparented, so this is exactly its `GlobalTransform`)
    // rather than reading `GlobalTransform`, which would still hold last
    // frame's value until the next `PostUpdate` propagation.
    let camera_global = GlobalTransform::from(*camera_transform);

    for (mut node, mut visibility, target) in &mut tags {
        let Ok(avatar_transform) = avatar_transforms.get(target.0) else {
            *visibility = Visibility::Hidden;
            continue;
        };
        let head = avatar_transform.translation + Vec3::Y * (PLAYER_HEIGHT / 2.0);
        let distance = camera_transform.translation.distance(head);
        match camera.world_to_viewport(&camera_global, head) {
            Ok(viewport_pos) if distance <= NAME_TAG_MAX_DISTANCE => {
                *visibility = Visibility::Inherited;
                node.left = Val::Px(viewport_pos.x);
                node.top = Val::Px(viewport_pos.y);
            }
            _ => {
                *visibility = Visibility::Hidden;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    fn state(x: f32, yaw: f32) -> PlayerSave {
        PlayerSave {
            pos: Vec3::new(x, 0.0, 0.0),
            yaw,
            pitch: 0.0,
        }
    }

    #[test]
    fn empty_buffer_samples_to_none() {
        let buffer = InterpBuffer::new();
        assert_eq!(buffer.sample(0.0), None);
    }

    #[test]
    fn single_sample_clamps_regardless_of_render_time() {
        let mut buffer = InterpBuffer::new();
        buffer.push(1.0, state(5.0, 0.0));
        assert_eq!(buffer.sample(0.0), Some((Vec3::new(5.0, 0.0, 0.0), 0.0)));
        assert_eq!(buffer.sample(1.0), Some((Vec3::new(5.0, 0.0, 0.0), 0.0)));
        assert_eq!(buffer.sample(100.0), Some((Vec3::new(5.0, 0.0, 0.0), 0.0)));
    }

    #[test]
    fn exact_midpoint_lerps_position() {
        let mut buffer = InterpBuffer::new();
        buffer.push(0.0, state(0.0, 0.0));
        buffer.push(1.0, state(10.0, 0.0));
        let (pos, _) = buffer.sample(0.5).unwrap();
        assert_eq!(pos, Vec3::new(5.0, 0.0, 0.0));
    }

    #[test]
    fn yaw_wraps_the_short_way_across_plus_minus_pi() {
        let mut buffer = InterpBuffer::new();
        // 170deg and -170deg are 20deg apart the short way (through 180deg),
        // not 340deg apart the long way through 0.
        let a = 170f32.to_radians();
        let b = (-170f32).to_radians();
        buffer.push(0.0, state(0.0, a));
        buffer.push(1.0, state(0.0, b));
        let (_, yaw) = buffer.sample(0.5).unwrap();
        // Midpoint of the short arc is 180deg (== -180deg): check equality
        // up to the wrap, since either sign is an equally correct answer.
        let expected = 180f32.to_radians();
        let diff = (yaw - expected).rem_euclid(2.0 * PI);
        let diff = diff.min(2.0 * PI - diff);
        assert!(
            diff < 1e-4,
            "yaw = {yaw}, expected ~{expected} (or -{expected})"
        );
    }

    #[test]
    fn extrapolation_past_newest_sample_is_clamped_not_overshot() {
        let mut buffer = InterpBuffer::new();
        buffer.push(0.0, state(0.0, 0.0));
        buffer.push(1.0, state(10.0, 0.0));
        let (pos, _) = buffer.sample(5.0).unwrap();
        assert_eq!(pos, Vec3::new(10.0, 0.0, 0.0));
    }

    #[test]
    fn render_time_before_oldest_sample_clamps_to_oldest() {
        let mut buffer = InterpBuffer::new();
        buffer.push(2.0, state(7.0, 0.0));
        buffer.push(3.0, state(9.0, 0.0));
        let (pos, _) = buffer.sample(0.0).unwrap();
        assert_eq!(pos, Vec3::new(7.0, 0.0, 0.0));
    }

    #[test]
    fn out_of_order_sample_is_rejected() {
        let mut buffer = InterpBuffer::new();
        buffer.push(1.0, state(10.0, 0.0));
        buffer.push(0.5, state(999.0, 0.0)); // stale/reordered packet
        assert_eq!(buffer.sample(0.0), Some((Vec3::new(10.0, 0.0, 0.0), 0.0)));
    }

    #[test]
    fn duplicate_timestamp_is_rejected() {
        let mut buffer = InterpBuffer::new();
        buffer.push(1.0, state(10.0, 0.0));
        buffer.push(1.0, state(999.0, 0.0));
        assert_eq!(buffer.sample(1.0), Some((Vec3::new(10.0, 0.0, 0.0), 0.0)));
    }

    #[test]
    fn large_gap_snaps_instead_of_interpolating() {
        let mut buffer = InterpBuffer::new();
        buffer.push(0.0, state(0.0, 0.0));
        buffer.push(0.1, state(1.0, 0.0));
        // A >1s gap (stall/reconnect): old samples are dropped so playback
        // doesn't crawl across the gap once it's due.
        buffer.push(5.0, state(100.0, 0.0));
        let (pos, _) = buffer.sample(0.05).unwrap();
        assert_eq!(
            pos,
            Vec3::new(100.0, 0.0, 0.0),
            "should snap to the post-gap sample, not interpolate pre-gap history"
        );
    }

    #[test]
    fn old_samples_are_pruned() {
        let mut buffer = InterpBuffer::new();
        let mut t = 0.0f64;
        while t <= 6.0 {
            buffer.push(t, state(t as f32, 0.0));
            t += 0.1;
        }
        assert!(
            buffer.samples.len() < 55,
            "expected samples older than SAMPLE_RETENTION_SECS pruned, got {} samples",
            buffer.samples.len()
        );
    }

    #[test]
    fn hue_for_id_is_deterministic() {
        assert_eq!(hue_for_id(42), hue_for_id(42));
    }

    #[test]
    fn hue_for_id_scatters_sequential_ids() {
        // A naive `id % 360` would put ids 1..=5 within a few degrees of
        // each other; the mixed hash should scatter them.
        let hues: Vec<f32> = (1..=5).map(hue_for_id).collect();
        let mut sorted = hues.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        sorted.dedup();
        assert_eq!(sorted.len(), hues.len(), "expected distinct hues: {hues:?}");
    }
}

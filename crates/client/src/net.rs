//! Client networking systems.
//!
//! - Resource wrapping the `ClientTransport`.
//! - Startup: send `Hello`.
//! - Per frame: compute the camera's chunk position; request not-yet-requested
//!   chunks within the view distance (horizontal radius in chunks, all
//!   vertical chunks `0..WORLD_HEIGHT_CHUNKS`), nearest first, in bounded
//!   batches; mark them as requested in the `view::ChunkStore`.
//! - Per frame: drain received messages; insert `ChunkData` into the store,
//!   apply `BlockChanged` idempotently (marking the affected chunk(s)
//!   dirty), and resolve `Welcome`'s spawn (see below).
//! - Spawn resolution: a `Welcome { player: Some(save), .. }` places the
//!   player immediately. `Welcome { player: None, .. }` waits until the
//!   default spawn column has fully loaded, then snaps the player's feet to
//!   one block above the highest solid block there and starts them in Walk
//!   mode.
//! - Periodically (~10 Hz) sends `UpdatePlayer` once the player has spawned.
//! - `PlayerJoined`/`PlayerLeft`/`PlayerMoved` are forwarded to [`crate::remote`],
//!   which owns spawning/despawning/interpolating other clients' avatars.
//! - On `AppExit` (in the `Last` schedule), sends `Goodbye`.
//! - Transport pump: [`ClientTransport::tick`] once per frame in `First`,
//!   [`ClientTransport::flush`] once per frame in `Last` (after `Goodbye` on
//!   exit, so a final send still reaches the wire before the app closes).

use bevy::prelude::*;
use bevy::time::TimeSystems;
use tsumiki_protocol::{ClientToServer, ClientTransport, PlayerSave, ServerToClient};
use tsumiki_world::{CHUNK_SIZE, WORLD_HEIGHT_CHUNKS, split_block_pos};

use crate::ClientConfig;
use crate::camera::{self, Player, PlayerMode};
use crate::remote;
use crate::view::{self, ChunkStore, world_pos_to_chunk};

/// Horizontal view distance, in chunks. Chunks are meshed only when all
/// loaded neighbors are present, so the meshed radius is effectively one
/// less than this.
pub const VIEW_DISTANCE_CHUNKS: i32 = 8;

/// Upper bound on chunk positions requested in a single frame's message.
const MAX_CHUNK_REQUESTS_PER_FRAME: usize = 64;

/// Rate at which `UpdatePlayer` is sent, once spawned.
const UPDATE_PLAYER_INTERVAL_SECS: f32 = 0.1;

/// Wraps the client's transport as a Bevy resource.
///
/// `pub(crate)` (with a `send` method) so [`crate::interact`] can send
/// `SetBlock` without net.rs needing to know about block editing.
#[derive(Resource)]
pub(crate) struct Transport<T: ClientTransport>(T);

impl<T: ClientTransport> Transport<T> {
    pub(crate) fn send(&mut self, msg: ClientToServer) {
        self.0.send(msg);
    }

    fn tick(&mut self, dt: f32) {
        self.0.tick(dt);
    }

    fn flush(&mut self) {
        self.0.flush();
    }
}

/// Where the client is in figuring out where the player should spawn.
#[derive(Resource, Default, Clone, Copy, PartialEq, Eq, Debug)]
enum SpawnState {
    /// Nothing heard from the server yet.
    #[default]
    AwaitingWelcome,
    /// `Welcome` arrived with no saved player; waiting for the default spawn
    /// column to finish loading so the ground height can be measured.
    AwaitingColumn,
    /// The player has been placed.
    Resolved,
}

#[derive(Resource)]
struct UpdatePlayerTimer(Timer);

impl Default for UpdatePlayerTimer {
    fn default() -> Self {
        Self(Timer::from_seconds(
            UPDATE_PLAYER_INTERVAL_SECS,
            TimerMode::Repeating,
        ))
    }
}

/// Wires the networking systems into `app`, taking ownership of `transport`.
pub fn install<T: ClientTransport>(app: &mut App, transport: T) {
    app.insert_resource(Transport(transport))
        .init_resource::<SpawnState>()
        .init_resource::<UpdatePlayerTimer>()
        .add_systems(Startup, send_hello::<T>)
        .add_systems(First, tick_transport::<T>.after(TimeSystems))
        .add_systems(
            Update,
            (
                request_chunks::<T>,
                receive_messages::<T>,
                resolve_spawn,
                send_update_player::<T>,
            )
                .chain(),
        )
        .add_systems(
            Last,
            (send_goodbye_on_exit::<T>, flush_transport::<T>).chain(),
        );
}

fn send_hello<T: ClientTransport>(mut transport: ResMut<Transport<T>>, config: Res<ClientConfig>) {
    transport.send(ClientToServer::Hello {
        name: config.name.clone(),
    });
}

fn tick_transport<T: ClientTransport>(time: Res<Time>, mut transport: ResMut<Transport<T>>) {
    transport.tick(time.delta_secs());
}

fn flush_transport<T: ClientTransport>(mut transport: ResMut<Transport<T>>) {
    transport.flush();
}

fn request_chunks<T: ClientTransport>(
    mut transport: ResMut<Transport<T>>,
    mut store: ResMut<ChunkStore>,
    cameras: Query<&Transform, With<Player>>,
) {
    let Ok(transform) = cameras.single() else {
        return;
    };
    let cam_chunk = world_pos_to_chunk(transform.translation);

    let radius = VIEW_DISTANCE_CHUNKS;
    let radius_sq = radius * radius;
    let mut candidates: Vec<IVec3> = Vec::new();
    for dx in -radius..=radius {
        for dz in -radius..=radius {
            if dx * dx + dz * dz > radius_sq {
                continue;
            }
            for y in 0..WORLD_HEIGHT_CHUNKS {
                let pos = IVec3::new(cam_chunk.x + dx, y, cam_chunk.z + dz);
                if !store.chunks.contains_key(&pos) && !store.requested.contains(&pos) {
                    candidates.push(pos);
                }
            }
        }
    }
    if candidates.is_empty() {
        return;
    }

    candidates.sort_by_key(|&pos| {
        let d = pos - cam_chunk;
        d.x * d.x + d.y * d.y + d.z * d.z
    });
    candidates.truncate(MAX_CHUNK_REQUESTS_PER_FRAME);

    for &pos in &candidates {
        store.requested.insert(pos);
    }
    transport.send(ClientToServer::RequestChunks {
        positions: candidates,
    });
}

#[allow(clippy::too_many_arguments)]
fn receive_messages<T: ClientTransport>(
    mut commands: Commands,
    time: Res<Time>,
    mut transport: ResMut<Transport<T>>,
    mut store: ResMut<ChunkStore>,
    mut spawn_state: ResMut<SpawnState>,
    mut players: Query<&mut Player>,
    avatar_mesh: Res<remote::AvatarMesh>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut remote_players: ResMut<remote::RemotePlayers>,
) {
    while let Some(msg) = transport.0.try_recv() {
        match msg {
            ServerToClient::Welcome {
                client_id: _,
                player,
            } => match player {
                Some(save) => {
                    if let Ok(mut player) = players.single_mut() {
                        player.feet = save.pos;
                        player.yaw = save.yaw;
                        player.pitch = save.pitch;
                        player.mode = PlayerMode::Walk;
                        player.spawned = true;
                    }
                    *spawn_state = SpawnState::Resolved;
                }
                None => {
                    *spawn_state = SpawnState::AwaitingColumn;
                }
            },
            ServerToClient::ChunkData { pos, chunk } => {
                store.chunks.insert(pos, chunk);
            }
            ServerToClient::BlockChanged { pos, block } => {
                // Idempotent: a block we already predicted locally (and
                // marked dirty then) must not be re-applied/re-dirtied.
                if view::block_at(&store, pos) != Some(block) {
                    view::set_block(&mut store, pos, block);
                }
            }
            ServerToClient::PlayerJoined { id, name, state } => {
                remote::spawn_remote_player(
                    &mut commands,
                    &avatar_mesh,
                    &mut materials,
                    &mut remote_players,
                    time.elapsed_secs_f64(),
                    id,
                    &name,
                    state,
                );
            }
            ServerToClient::PlayerLeft { id } => {
                remote::despawn_remote_player(
                    &mut commands,
                    &mut materials,
                    &mut remote_players,
                    id,
                );
            }
            ServerToClient::PlayerMoved { id, state } => {
                remote::push_sample(&mut remote_players, time.elapsed_secs_f64(), id, state);
            }
        }
    }
}

/// Once the default spawn column has fully loaded, snaps the player to one
/// block above its highest solid block and starts them walking.
fn resolve_spawn(
    mut spawn_state: ResMut<SpawnState>,
    store: Res<ChunkStore>,
    registry: Res<view::Registry>,
    config: Res<ClientConfig>,
    mut players: Query<&mut Player>,
) {
    if *spawn_state != SpawnState::AwaitingColumn {
        return;
    }

    let xz = camera::spawn_xz(&config);
    let (column, local) = split_block_pos(IVec3::new(xz.x as i32, 0, xz.y as i32));

    for cy in 0..WORLD_HEIGHT_CHUNKS {
        if !store
            .chunks
            .contains_key(&IVec3::new(column.x, cy, column.z))
        {
            return; // Column not fully loaded yet.
        }
    }

    let size = CHUNK_SIZE as i32;
    let mut ground_y = None;
    'search: for cy in (0..WORLD_HEIGHT_CHUNKS).rev() {
        let chunk = &store.chunks[&IVec3::new(column.x, cy, column.z)];
        for ly in (0..size).rev() {
            let block = chunk.get(UVec3::new(local.x as u32, ly as u32, local.z as u32));
            if registry.0.get(block).solid {
                ground_y = Some(cy * size + ly);
                break 'search;
            }
        }
    }

    // If the column turned out to be all-air (shouldn't happen with the
    // current world generator), stay `AwaitingColumn` and retry next frame
    // in case more accurate data arrives; there is nothing better to do.
    let Some(ground_y) = ground_y else {
        return;
    };

    if let Ok(mut player) = players.single_mut() {
        player.feet = Vec3::new(xz.x, (ground_y + 1) as f32, xz.y);
        player.mode = PlayerMode::Walk;
        player.spawned = true;
    }
    *spawn_state = SpawnState::Resolved;
}

fn send_update_player<T: ClientTransport>(
    time: Res<Time>,
    mut timer: ResMut<UpdatePlayerTimer>,
    mut transport: ResMut<Transport<T>>,
    players: Query<&Player>,
) {
    if !timer.0.tick(time.delta()).just_finished() {
        return;
    }
    let Ok(player) = players.single() else {
        return;
    };
    if !player.spawned {
        return;
    }
    transport.send(ClientToServer::UpdatePlayer(PlayerSave {
        pos: player.feet,
        yaw: player.yaw,
        pitch: player.pitch,
    }));
}

/// Sends a graceful `Goodbye` when the app is exiting. Runs in `Last` so it
/// observes an `AppExit` written earlier in the same frame's `Update`.
///
/// Flushes immediately after sending: the app exits at the end of this same
/// frame, so getting the final message onto the wire cannot rely on
/// [`flush_transport`] (scheduled right after this system) alone — that
/// ordering is still correct and kept as the normal per-frame flush, but a
/// real (non-local) transport should not depend on it for the one message
/// that matters most.
fn send_goodbye_on_exit<T: ClientTransport>(
    mut transport: ResMut<Transport<T>>,
    mut exit_events: MessageReader<AppExit>,
) {
    if exit_events.read().next().is_some() {
        transport.send(ClientToServer::Goodbye);
        transport.flush();
    }
}

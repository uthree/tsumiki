//! Client networking systems.
//!
//! - Resource wrapping the `ClientTransport` (a boxed trait object: nothing
//!   here is generic over the concrete transport). Absent while the app is
//!   in [`AppState::Menu`]; inserted either at startup ([`StartMode::Direct`])
//!   or by [`crate::menu`] once Singleplayer/Multiplayer succeeds.
//! - On entering [`AppState::InGame`]: send `Hello`.
//! - Per frame, only in [`AppState::InGame`]: compute the camera's chunk
//!   position; request not-yet-requested chunks within the view distance
//!   (horizontal radius in chunks, all vertical chunks
//!   `0..WORLD_HEIGHT_CHUNKS`), nearest first, in bounded batches; mark them
//!   as requested in the `view::ChunkStore`.
//! - Per frame, only in [`AppState::InGame`]: drain received messages;
//!   insert `ChunkData` into the store, apply `BlockChanged` idempotently
//!   (marking the affected chunk(s) dirty), and resolve `Welcome`'s spawn
//!   (see below).
//! - Spawn resolution: a `Welcome { player: Some(save), .. }` places the
//!   player immediately. `Welcome { player: None, .. }` waits until the
//!   default spawn column has fully loaded, then snaps the player's feet to
//!   one block above the highest solid block there and starts them in Walk
//!   mode. [`crate::death`]'s Respawn handler reuses this same path by
//!   resetting [`SpawnState`] to [`SpawnState::AwaitingColumn`].
//! - `Welcome` also carries the session's fixed `game_mode` and starting
//!   `time_of_day`, stored into [`crate::state::GameMode`]/
//!   [`crate::state::GameState`]; `InventoryUpdate`/`HealthUpdate`/`Died`/
//!   `TimeUpdate` update the same [`crate::state::GameState`], and
//!   `ItemSpawned`/`ItemDespawned` are forwarded to [`crate::items`].
//! - `ContainerOpened` stores the container into
//!   [`crate::state::ContainerState`] and flips [`crate::pause::PauseState`]
//!   to `Inventory` (the screen only opens once the server actually grants
//!   it, never predicted -- see [`crate::interact`]); `ContainerUpdate`
//!   refreshes its slots; `ContainerClosed` clears it and flips back to
//!   `Playing` (whether the player asked for it, or the server closed it
//!   unsolicited: broken block, moved out of reach).
//! - Periodically (~10 Hz) sends `UpdatePlayer` once the player has spawned.
//! - `PlayerJoined`/`PlayerLeft`/`PlayerMoved` are forwarded to [`crate::remote`],
//!   which owns spawning/despawning/interpolating other clients' avatars.
//! - On `AppExit` (in the `Last` schedule), sends `Goodbye`.
//! - Transport pump: [`ClientTransport::tick`] once per frame in `First`,
//!   [`ClientTransport::flush`] once per frame in `Last` (after `Goodbye` on
//!   exit, so a final send still reaches the wire before the app closes).
//!   These pump systems run unconditionally (not gated to `InGame`) but
//!   tolerate the transport resource not existing yet, i.e. while still in
//!   the menu.

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy::time::TimeSystems;
use tsumiki_protocol::{ClientToServer, ClientTransport, PlayerSave, ServerToClient};
use tsumiki_world::{CHUNK_SIZE, WORLD_HEIGHT_CHUNKS, split_block_pos};

use crate::camera::{self, Player, PlayerMode};
use crate::items;
use crate::lod_view::{self, LodStore};
use crate::pause::PauseState;
use crate::remote;
use crate::settings::Settings;
use crate::state::{ContainerState, GameMode, GameState, ItemReg, OpenContainer};
use crate::view::{self, ChunkStore, world_pos_to_chunk};
use crate::{AppState, ClientConfig};

/// Default horizontal view distance, in chunks, used as
/// [`Settings::view_distance_chunks`]'s default value (the live value is
/// read from [`Settings`], not this constant, so it can change at runtime).
/// Chunks are meshed only when all loaded neighbors are present, so the
/// meshed radius is effectively one less than the configured value.
pub const VIEW_DISTANCE_CHUNKS: i32 = 8;

/// Upper bound on chunk positions requested in a single frame's message.
const MAX_CHUNK_REQUESTS_PER_FRAME: usize = 64;

/// Rate at which `UpdatePlayer` is sent, once spawned.
const UPDATE_PLAYER_INTERVAL_SECS: f32 = 0.1;

/// Wraps the client's transport as a Bevy resource.
///
/// `pub(crate)` (with a `send` method) so [`crate::interact`] and
/// [`crate::menu`] can use it without net.rs needing to know about block
/// editing or the menu.
#[derive(Resource)]
pub(crate) struct Transport(Box<dyn ClientTransport>);

impl Transport {
    pub(crate) fn new(inner: Box<dyn ClientTransport>) -> Self {
        Self(inner)
    }

    pub(crate) fn send(&mut self, msg: ClientToServer) {
        self.0.send(msg);
    }

    fn tick(&mut self, dt: f32) {
        self.0.tick(dt);
    }

    /// `pub(crate)` (like [`Self::send`]) so [`crate::pause`]'s "Back to
    /// Title" handler can flush a final `Goodbye` immediately, the same way
    /// [`send_goodbye_on_exit`] does for a full app exit.
    pub(crate) fn flush(&mut self) {
        self.0.flush();
    }
}

/// Where the client is in figuring out where the player should spawn.
///
/// `pub(crate)` so [`crate::death`]'s Respawn handler can drop this back to
/// [`SpawnState::AwaitingColumn`], reusing [`resolve_spawn`]'s ground-snap
/// logic instead of duplicating it.
#[derive(Resource, Default, Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SpawnState {
    /// Nothing heard from the server yet.
    #[default]
    AwaitingWelcome,
    /// `Welcome` arrived with no saved player; waiting for the default spawn
    /// column to finish loading so the ground height can be measured.
    AwaitingColumn,
    /// The player has been placed.
    Resolved,
}

/// Bundles the roadmap-M5 inventory/container resources into one
/// [`SystemParam`] purely to keep [`receive_messages`] under Bevy's 16-param
/// system-function limit (`bevy_ecs`'s `impl_system_function` is only
/// generated up to arity 16).
#[derive(SystemParam)]
struct InventorySync<'w> {
    game_state: ResMut<'w, GameState>,
    container: ResMut<'w, ContainerState>,
    next_pause: ResMut<'w, NextState<PauseState>>,
    item_reg: Res<'w, ItemReg>,
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

/// Wires the networking systems into `app`. Does not insert the transport
/// resource itself: [`crate::run_client`] does that for [`crate::StartMode::Direct`],
/// and [`crate::menu`] does it once a connection succeeds.
pub fn install(app: &mut App) {
    app.init_resource::<SpawnState>()
        .init_resource::<UpdatePlayerTimer>()
        .add_systems(OnEnter(AppState::InGame), send_hello)
        .add_systems(OnExit(AppState::InGame), reset_spawn_state)
        .add_systems(First, tick_transport.after(TimeSystems))
        .add_systems(
            Update,
            (
                request_chunks,
                receive_messages,
                resolve_spawn,
                send_update_player,
            )
                .chain()
                .run_if(in_state(AppState::InGame)),
        )
        .add_systems(Last, (send_goodbye_on_exit, flush_transport).chain());
}

fn send_hello(mut transport: ResMut<Transport>, config: Res<ClientConfig>) {
    transport.send(ClientToServer::Hello {
        name: config.name.clone(),
    });
}

/// Part of the `OnExit(AppState::InGame)` "despawn/reset everything in-game"
/// contract (see `pause` module docs): resets spawn resolution and the
/// player-update timer so a fresh session gets a fresh `Hello`/`Welcome`
/// round trip and spawn resolution, not leftover state from the last one.
fn reset_spawn_state(mut spawn_state: ResMut<SpawnState>, mut timer: ResMut<UpdatePlayerTimer>) {
    *spawn_state = SpawnState::default();
    *timer = UpdatePlayerTimer::default();
}

/// Tolerates the transport resource not existing yet (still in the menu).
fn tick_transport(time: Res<Time>, transport: Option<ResMut<Transport>>) {
    let Some(mut transport) = transport else {
        return;
    };
    transport.tick(time.delta_secs());
}

/// Tolerates the transport resource not existing yet (still in the menu).
fn flush_transport(transport: Option<ResMut<Transport>>) {
    let Some(mut transport) = transport else {
        return;
    };
    transport.flush();
}

fn request_chunks(
    mut transport: ResMut<Transport>,
    mut store: ResMut<ChunkStore>,
    settings: Res<Settings>,
    cameras: Query<&Transform, With<Player>>,
) {
    let Ok(transform) = cameras.single() else {
        return;
    };
    let cam_chunk = world_pos_to_chunk(transform.translation);

    let radius = settings.view_distance_chunks;
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
fn receive_messages(
    mut commands: Commands,
    time: Res<Time>,
    mut transport: ResMut<Transport>,
    mut store: ResMut<ChunkStore>,
    mut lod_store: ResMut<LodStore>,
    mut spawn_state: ResMut<SpawnState>,
    mut players: Query<&mut Player>,
    avatar_mesh: Res<remote::AvatarMesh>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut remote_players: ResMut<remote::RemotePlayers>,
    ui_font: Res<crate::UiFont>,
    mut mode: ResMut<GameMode>,
    mut inv: InventorySync,
    item_mesh: Res<items::ItemMesh>,
    mut dropped_items: ResMut<items::DroppedItems>,
) {
    while let Some(msg) = transport.0.try_recv() {
        match msg {
            ServerToClient::Welcome {
                client_id: _,
                player,
                game_mode,
                time_of_day,
            } => {
                *mode = GameMode(game_mode);
                inv.game_state.time_of_day = time_of_day;
                match player {
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
                }
            }
            ServerToClient::ChunkData { pos, chunk } => {
                store.chunks.insert(pos, chunk);
            }
            ServerToClient::LodChunkData { level, pos, chunk } => {
                lod_view::insert_lod_chunk(&mut lod_store, level, pos, chunk);
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
                    &ui_font,
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
            ServerToClient::InventoryUpdate {
                main,
                crafting,
                craft_output,
                cursor,
            } => {
                inv.game_state.main = main;
                inv.game_state.crafting = crafting;
                inv.game_state.craft_output = craft_output;
                inv.game_state.cursor = cursor;
            }
            ServerToClient::ContainerOpened { kind, pos, slots } => {
                inv.container.open = Some(OpenContainer { kind, pos, slots });
                inv.next_pause.set(PauseState::Inventory);
            }
            ServerToClient::ContainerUpdate { slots } => {
                if let Some(open) = inv.container.open.as_mut() {
                    open.slots = slots;
                }
            }
            ServerToClient::ContainerClosed => {
                inv.container.open = None;
                inv.next_pause.set(PauseState::Playing);
            }
            ServerToClient::HealthUpdate { hp } => {
                inv.game_state.hp = hp;
                inv.game_state.dead = hp == 0;
            }
            ServerToClient::Died { at: _ } => {
                inv.game_state.dead = true;
            }
            ServerToClient::ItemSpawned { id, pos, stack } => {
                items::spawn_item(
                    &mut commands,
                    &item_mesh,
                    &mut materials,
                    &mut dropped_items,
                    &inv.item_reg.0,
                    time.elapsed_secs(),
                    id,
                    pos,
                    stack,
                );
            }
            ServerToClient::ItemDespawned { id } => {
                items::despawn_item(&mut commands, &mut materials, &mut dropped_items, id);
            }
            ServerToClient::TimeUpdate { time_of_day } => {
                inv.game_state.time_of_day = time_of_day;
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

fn send_update_player(
    time: Res<Time>,
    mut timer: ResMut<UpdatePlayerTimer>,
    mut transport: ResMut<Transport>,
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
///
/// Tolerates the transport resource not existing (still in the menu, or
/// quitting from the menu): there is nothing to say goodbye to.
fn send_goodbye_on_exit(
    transport: Option<ResMut<Transport>>,
    mut exit_events: MessageReader<AppExit>,
) {
    if exit_events.read().next().is_some()
        && let Some(mut transport) = transport
    {
        transport.send(ClientToServer::Goodbye);
        transport.flush();
    }
}

//! Block targeting, mining, placing and container interaction (roadmap M5
//! rework: `PlaceBlock` now names a hotbar slot rather than a block, so the
//! server decides what item is actually placed; right-clicking a block with
//! an interaction opens it instead).
//!
//! - Per frame, raycasts from the player's eye (via `tsumiki_world::raycast`,
//!   targeting solid blocks, reach `tsumiki_protocol::REACH`) and draws a
//!   highlight around the targeted block (a slightly inflated gizmo cuboid).
//!   Targeting is cleared while dead, but keeps running (rather than being
//!   gated off) so it can *become* `None` the instant death happens, instead
//!   of freezing on a stale target.
//! - Mining, while the cursor is grabbed:
//!   - Survival: holding LEFT on a targeted block accrues progress against
//!     `registry.get(block).break_time_secs`; switching targets or
//!     releasing the button resets it. Progress is shown as a small bar
//!     below screen center, tinted with the target block's color. On
//!     completion, sends `BreakBlock` -- no local edit; the server's
//!     `BlockChanged` is the only thing that actually removes the block, one
//!     server tick later (the point of server-authoritative mining).
//!   - Creative: left click sends `BreakBlock` immediately and *also*
//!     applies the local prediction edit, exactly like the old `SetBlock`
//!     path.
//! - Right click on the targeted block:
//!   - If it has a `BlockInteraction` (chest, crafting table): sends
//!     `OpenContainer { pos }` instead of placing. The screen itself only
//!     opens once the server answers `ContainerOpened` (handled in `net.rs`,
//!     which flips `PauseState` to `Inventory`) -- not predicted here, since
//!     the server can refuse (out of reach, wrong block by the time it
//!     processes the message).
//!   - Otherwise, places whatever item sits in the selected hotbar slot
//!     (`hotbar: u8`, not a block id: a client cannot ask to place something
//!     it does not have selected). Rejected locally when the face normal is
//!     zero, the destination is outside vertical world bounds or not
//!     air/water, it would intersect the player's AABB, or the held item
//!     does not place a block at all (`ItemRegistry::places`). Creative
//!     additionally predicts the edit locally, resolving the block from the
//!     held item; survival never applies a local edit (waits for
//!     `BlockChanged`).
//! - Dead players get no targeting/highlight/clicks (mining/placing/opening
//!   is gated off; see above for why targeting itself stays live).
//! - Click handling runs *before* [`crate::camera::grab_cursor`] each frame,
//!   so the very click that grabs the cursor is seen as "not yet grabbed"
//!   and never also breaks/places/opens.

use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use tsumiki_protocol::ClientToServer;
use tsumiki_world::physics::{Aabb, PLAYER_EYE_HEIGHT};
use tsumiki_world::raycast::{RayHit, raycast_voxels};
use tsumiki_world::{BlockId, ItemRegistry, WORLD_HEIGHT_BLOCKS, blocks};

use crate::AppState;
use crate::camera::{self, Player};
use crate::hotbar::Hotbar;
use crate::net;
use crate::pause;
use crate::state::{self, GameState, ItemReg};
use crate::view::{self, ChunkStore};

/// Half the highlight cuboid's inflation over the unit block, per axis.
const HIGHLIGHT_INFLATION: f32 = 1.02;

const BAR_WIDTH: f32 = 120.0;
const BAR_HEIGHT: f32 = 14.0;
/// How far down from the very top of the screen the bar's track sits;
/// "just below the screen center".
const BAR_TOP_PADDING_PERCENT: f32 = 54.0;
const BAR_BORDER_COLOR: Color = Color::srgba(0.0, 0.0, 0.0, 0.6);
const BAR_TRACK_COLOR: Color = Color::srgba(0.08, 0.08, 0.1, 0.55);

/// The block currently under the crosshair, if any. Recomputed every frame.
#[derive(Resource, Default)]
struct TargetedBlock(Option<RayHit>);

/// Survival hold-to-mine progress: which block is being mined and how long
/// it's been held. Reset whenever the target changes or the left button is
/// released.
#[derive(Resource, Default)]
struct MiningProgress {
    target: Option<IVec3>,
    elapsed: f32,
}

#[derive(Component)]
struct ProgressBarRoot;
#[derive(Component)]
struct ProgressBarFill;

/// Wires the targeting/highlight/mining/placing systems into `app`.
pub fn install(app: &mut App) {
    app.init_resource::<TargetedBlock>()
        .init_resource::<MiningProgress>()
        .add_systems(OnEnter(AppState::InGame), spawn_progress_bar)
        .add_systems(OnExit(AppState::InGame), teardown_progress_bar)
        .add_systems(
            Update,
            compute_target
                .run_if(in_state(AppState::InGame))
                .run_if(pause::is_playing),
        )
        .add_systems(
            Update,
            (
                draw_highlight,
                update_progress_bar,
                handle_mining_and_placing.before(camera::grab_cursor),
            )
                .run_if(in_state(AppState::InGame))
                .run_if(pause::is_playing)
                .run_if(state::is_alive),
        );
}

fn compute_target(
    mut target: ResMut<TargetedBlock>,
    store: Res<ChunkStore>,
    registry: Res<view::Registry>,
    state: Res<GameState>,
    players: Query<&Player>,
) {
    let Ok(player) = players.single() else {
        target.0 = None;
        return;
    };
    if state.dead {
        target.0 = None;
        return;
    }
    let eye = player.feet + Vec3::Y * PLAYER_EYE_HEIGHT;
    let dir = Quat::from_euler(EulerRot::YXZ, player.yaw, player.pitch, 0.0) * Vec3::NEG_Z;
    let is_target = |pos: IVec3| {
        view::block_at(&store, pos)
            .map(|block| registry.0.get(block).solid)
            .unwrap_or(false)
    };
    target.0 = raycast_voxels(eye, dir, tsumiki_protocol::REACH, is_target);
}

fn draw_highlight(target: Res<TargetedBlock>, mut gizmos: Gizmos) {
    let Some(hit) = target.0 else {
        return;
    };
    let center = hit.block.as_vec3() + Vec3::splat(0.5);
    let size = Vec3::splat(HIGHLIGHT_INFLATION);
    gizmos.primitive_3d(&Cuboid::from_size(size), center, Color::BLACK);
}

#[allow(clippy::too_many_arguments)]
fn handle_mining_and_placing(
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    windows: Query<&CursorOptions, With<PrimaryWindow>>,
    target: Res<TargetedBlock>,
    mut store: ResMut<ChunkStore>,
    hotbar: Res<Hotbar>,
    players: Query<&Player>,
    mut transport: ResMut<net::Transport>,
    mode: Res<state::GameMode>,
    game_state: Res<GameState>,
    item_reg: Res<ItemReg>,
    registry: Res<view::Registry>,
    time: Res<Time>,
    mut mining: ResMut<MiningProgress>,
) {
    let Ok(cursor) = windows.single() else {
        return;
    };
    // Not grabbed yet: this click (if any) is the one `grab_cursor` is about
    // to consume to grab the cursor, not an edit.
    if cursor.grab_mode == CursorGrabMode::None {
        *mining = MiningProgress::default();
        return;
    }

    let current_target = target.0.map(|hit| hit.block);
    if mining.target != current_target || !mouse_buttons.pressed(MouseButton::Left) {
        mining.target = None;
        mining.elapsed = 0.0;
    }

    let Some(hit) = target.0 else {
        return;
    };

    if mouse_buttons.pressed(MouseButton::Left) {
        if mode.is_survival() {
            mining.target = Some(hit.block);
            mining.elapsed += time.delta_secs();
            let block = view::block_at(&store, hit.block).unwrap_or(BlockId::AIR);
            let required = registry.0.get(block).break_time_secs;
            if required <= 0.0 || mining.elapsed >= required {
                transport.send(ClientToServer::BreakBlock { pos: hit.block });
                mining.target = None;
                mining.elapsed = 0.0;
            }
        } else if mouse_buttons.just_pressed(MouseButton::Left) {
            view::set_block(&mut store, hit.block, blocks::AIR);
            transport.send(ClientToServer::BreakBlock { pos: hit.block });
        }
    }

    if mouse_buttons.just_pressed(MouseButton::Right) {
        let looked_at = view::block_at(&store, hit.block).unwrap_or(BlockId::AIR);
        if registry.0.get(looked_at).interaction.is_some() {
            transport.send(ClientToServer::OpenContainer { pos: hit.block });
        } else {
            try_place(
                &mut store,
                &mut transport,
                hit,
                &hotbar,
                &players,
                mode.0,
                &game_state,
                &item_reg.0,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn try_place(
    store: &mut ChunkStore,
    transport: &mut net::Transport,
    hit: RayHit,
    hotbar: &Hotbar,
    players: &Query<&Player>,
    mode: tsumiki_protocol::GameMode,
    game_state: &GameState,
    item_reg: &ItemRegistry,
) {
    if hit.face_normal == IVec3::ZERO {
        return;
    }
    let dest = hit.block + hit.face_normal;
    if !(0..WORLD_HEIGHT_BLOCKS).contains(&dest.y) {
        return;
    }
    let Some(existing) = view::block_at(store, dest) else {
        return;
    };
    if !(existing.is_air() || existing == blocks::WATER) {
        return;
    }
    let Ok(player) = players.single() else {
        return;
    };
    if Aabb::player(player.feet).intersects_block(dest) {
        return;
    }

    let Some(stack) = hotbar.selected_stack(&game_state.main) else {
        return;
    };
    let Some(block) = item_reg.places(stack.item) else {
        return;
    };

    match mode {
        tsumiki_protocol::GameMode::Creative => {
            view::set_block(store, dest, block);
            transport.send(ClientToServer::PlaceBlock {
                pos: dest,
                hotbar: hotbar.selected as u8,
            });
        }
        tsumiki_protocol::GameMode::Survival => {
            transport.send(ClientToServer::PlaceBlock {
                pos: dest,
                hotbar: hotbar.selected as u8,
            });
        }
    }
}

fn spawn_progress_bar(mut commands: Commands) {
    commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                position_type: PositionType::Absolute,
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                padding: UiRect::top(Val::Percent(BAR_TOP_PADDING_PERCENT)),
                ..default()
            },
            Visibility::Hidden,
            ProgressBarRoot,
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    width: Val::Px(BAR_WIDTH),
                    height: Val::Px(BAR_HEIGHT),
                    border: UiRect::all(Val::Px(2.0)),
                    padding: UiRect::all(Val::Px(2.0)),
                    ..default()
                },
                BackgroundColor(BAR_TRACK_COLOR),
                BorderColor::all(BAR_BORDER_COLOR),
            ))
            .with_children(|track| {
                track.spawn((
                    Node {
                        width: Val::Percent(0.0),
                        height: Val::Percent(100.0),
                        ..default()
                    },
                    BackgroundColor(Color::WHITE),
                    ProgressBarFill,
                ));
            });
        });
}

fn teardown_progress_bar(mut commands: Commands, roots: Query<Entity, With<ProgressBarRoot>>) {
    for entity in &roots {
        commands.entity(entity).despawn();
    }
}

fn update_progress_bar(
    mining: Res<MiningProgress>,
    mode: Res<state::GameMode>,
    store: Res<ChunkStore>,
    registry: Res<view::Registry>,
    mut roots: Query<&mut Visibility, With<ProgressBarRoot>>,
    mut fills: Query<(&mut Node, &mut BackgroundColor), With<ProgressBarFill>>,
) {
    let active = mode.is_survival() && mining.target.is_some();
    for mut vis in &mut roots {
        *vis = if active {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
    let Some(target) = mining.target.filter(|_| active) else {
        return;
    };

    let block = view::block_at(&store, target).unwrap_or(BlockId::AIR);
    let def = registry.0.get(block);
    let required = def.break_time_secs.max(0.0001);
    let fraction = (mining.elapsed / required).clamp(0.0, 1.0);
    let color = Color::srgb_u8(def.color_top[0], def.color_top[1], def.color_top[2]);

    for (mut node, mut bg) in &mut fills {
        node.width = Val::Percent(fraction * 100.0);
        *bg = BackgroundColor(color);
    }
}

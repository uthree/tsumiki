//! Block targeting and editing.
//!
//! - Per frame, raycasts from the player's eye (via `tsumiki_world::raycast`,
//!   targeting solid blocks, reach 5 blocks) and draws a highlight around the
//!   targeted block (a slightly inflated gizmo cuboid).
//! - While the cursor is grabbed: left click breaks the targeted block
//!   (`SetBlock` air), right click places the hotbar's selected block at
//!   `hit.block + hit.face_normal` — rejected when the face normal is zero,
//!   the destination is outside vertical world bounds or not air/water, or
//!   the placed block would intersect the player's AABB.
//! - Edits are applied locally right away (via `view::set_block`, which
//!   marks the affected chunk(s) dirty for the instant-remesh path) AND sent
//!   as `SetBlock`; the server's `BlockChanged` echo is applied idempotently
//!   by `net.rs`.
//! - Click handling runs *before* [`crate::camera::grab_cursor`] each frame,
//!   so the very click that grabs the cursor is seen as "not yet grabbed"
//!   and never also breaks/places a block.

use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use tsumiki_protocol::ClientToServer;
use tsumiki_world::physics::{Aabb, PLAYER_EYE_HEIGHT};
use tsumiki_world::raycast::{RayHit, raycast_voxels};
use tsumiki_world::{WORLD_HEIGHT_BLOCKS, blocks};

use crate::AppState;
use crate::camera::{self, Player};
use crate::hotbar::Hotbar;
use crate::net;
use crate::pause;
use crate::view::{self, ChunkStore};

/// Maximum distance a block can be targeted/edited from.
const REACH: f32 = 5.0;

/// Half the highlight cuboid's inflation over the unit block, per axis.
const HIGHLIGHT_INFLATION: f32 = 1.02;

/// The block currently under the crosshair, if any. Recomputed every frame.
#[derive(Resource, Default)]
struct TargetedBlock(Option<RayHit>);

/// Wires the targeting/highlight/edit systems into `app`.
pub fn install(app: &mut App) {
    app.init_resource::<TargetedBlock>().add_systems(
        Update,
        (
            compute_target,
            draw_highlight,
            handle_click.before(camera::grab_cursor),
        )
            .run_if(in_state(AppState::InGame))
            .run_if(pause::is_playing),
    );
}

fn compute_target(
    mut target: ResMut<TargetedBlock>,
    store: Res<ChunkStore>,
    registry: Res<view::Registry>,
    players: Query<&Player>,
) {
    let Ok(player) = players.single() else {
        target.0 = None;
        return;
    };
    let eye = player.feet + Vec3::Y * PLAYER_EYE_HEIGHT;
    let dir = Quat::from_euler(EulerRot::YXZ, player.yaw, player.pitch, 0.0) * Vec3::NEG_Z;
    let is_target = |pos: IVec3| {
        view::block_at(&store, pos)
            .map(|block| registry.0.get(block).solid)
            .unwrap_or(false)
    };
    target.0 = raycast_voxels(eye, dir, REACH, is_target);
}

fn draw_highlight(target: Res<TargetedBlock>, mut gizmos: Gizmos) {
    let Some(hit) = target.0 else {
        return;
    };
    let center = hit.block.as_vec3() + Vec3::splat(0.5);
    let size = Vec3::splat(HIGHLIGHT_INFLATION);
    gizmos.primitive_3d(&Cuboid::from_size(size), center, Color::BLACK);
}

fn handle_click(
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    windows: Query<&CursorOptions, With<PrimaryWindow>>,
    target: Res<TargetedBlock>,
    mut store: ResMut<ChunkStore>,
    hotbar: Res<Hotbar>,
    players: Query<&Player>,
    mut transport: ResMut<net::Transport>,
) {
    let Ok(cursor) = windows.single() else {
        return;
    };
    // Not grabbed yet: this click (if any) is the one `grab_cursor` is about
    // to consume to grab the cursor, not an edit.
    if cursor.grab_mode == CursorGrabMode::None {
        return;
    }

    let Some(hit) = target.0 else {
        return;
    };

    if mouse_buttons.just_pressed(MouseButton::Left) {
        view::set_block(&mut store, hit.block, blocks::AIR);
        transport.send(ClientToServer::SetBlock {
            pos: hit.block,
            block: blocks::AIR,
        });
        return;
    }

    if mouse_buttons.just_pressed(MouseButton::Right) {
        if hit.face_normal == IVec3::ZERO {
            return;
        }
        let dest = hit.block + hit.face_normal;
        if !(0..WORLD_HEIGHT_BLOCKS).contains(&dest.y) {
            return;
        }
        let Some(existing) = view::block_at(&store, dest) else {
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

        let block = hotbar.selected_block();
        view::set_block(&mut store, dest, block);
        transport.send(ClientToServer::SetBlock { pos: dest, block });
    }
}

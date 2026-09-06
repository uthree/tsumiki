//! Death overlay and respawn (roadmap.md M4).
//!
//! - Shown reactively (mirroring [`crate::pause`]'s sync-UI-to-state
//!   pattern) whenever [`GameState::dead`] is true: "You died" plus a
//!   Respawn button, semi-transparent dark red tint, styled like
//!   [`crate::menu`]/[`crate::ui`]'s panels.
//! - Movement/interact/hotbar are disabled while dead via the
//!   [`crate::state::is_alive`] run condition those modules apply; this
//!   module additionally releases the cursor so the Respawn button is
//!   clickable, the same way [`crate::pause`] does while paused.
//! - Respawn sends `ClientToServer::Respawn`, clears the player's velocity,
//!   and reuses the exact fresh-spawn ground-snap logic
//!   ([`crate::net::resolve_spawn`]) by handing spawn resolution back to
//!   [`crate::net::SpawnState::AwaitingColumn`] — the same state a
//!   save-less first spawn starts from. The overlay itself closes once the
//!   post-respawn `HealthUpdate` sets `dead` back to `false` (see
//!   `state.rs`/`net.rs`), not from any local respawn bookkeeping here.

use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use tsumiki_protocol::ClientToServer;

use crate::camera::Player;
use crate::i18n::LocalizedText;
use crate::net;
use crate::state::GameState;
use crate::{AppState, UiFont, ui};

const OVERLAY_BG: Color = Color::srgba(0.35, 0.05, 0.05, 0.55);
const TITLE_COLOR: Color = Color::srgb(0.95, 0.85, 0.82);
const RESPAWN_COLOR: Color = Color::srgb(0.43, 0.78, 0.36);
const TITLE_FONT_SIZE: f32 = 48.0;

#[derive(Component, Clone, Copy)]
struct RespawnButton;

/// The currently-spawned overlay's root entity, if any. A plain cache of
/// "what did we last build", compared against [`GameState::dead`] each
/// frame by [`sync_death_ui`] — mirrors [`crate::pause::PauseUi`]'s pattern.
#[derive(Resource, Default)]
struct DeathUi(Option<Entity>);

pub fn install(app: &mut App) {
    app.init_resource::<DeathUi>()
        .add_systems(OnExit(AppState::InGame), teardown_on_exit)
        .add_systems(
            Update,
            (
                sync_death_ui,
                release_cursor_while_dead,
                handle_respawn_button,
            )
                .chain()
                .run_if(in_state(AppState::InGame)),
        );
}

fn spawn_overlay(commands: &mut Commands, font: &UiFont) -> Entity {
    let root = commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                position_type: PositionType::Absolute,
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                row_gap: Val::Px(20.0),
                ..default()
            },
            BackgroundColor(OVERLAY_BG),
        ))
        .id();
    commands.entity(root).with_children(|parent| {
        parent.spawn((
            Text::default(),
            LocalizedText::new("death.title"),
            font.text(TITLE_FONT_SIZE),
            TextColor(TITLE_COLOR),
        ));
        parent
            .spawn(Node {
                width: Val::Px(ui::PANEL_WIDTH * 0.6),
                flex_direction: FlexDirection::Column,
                ..default()
            })
            .with_children(|panel| {
                ui::spawn_button(panel, RespawnButton, "death.respawn", RESPAWN_COLOR, font);
            });
    });
    root
}

fn sync_death_ui(
    state: Res<GameState>,
    mut ui_state: ResMut<DeathUi>,
    mut commands: Commands,
    font: Res<UiFont>,
) {
    let want = state.dead;
    let have = ui_state.0.is_some();
    if want == have {
        return;
    }
    if want {
        ui_state.0 = Some(spawn_overlay(&mut commands, &font));
    } else if let Some(root) = ui_state.0.take() {
        commands.entity(root).despawn();
    }
}

/// Releases the cursor every frame while dead, mirroring
/// [`crate::pause`]'s pause-state cursor handling, so the Respawn button is
/// clickable. Idempotent (no-op once already released), so running
/// unconditionally rather than only on a dead-transition is harmless.
fn release_cursor_while_dead(
    state: Res<GameState>,
    mut windows: Query<&mut CursorOptions, With<PrimaryWindow>>,
) {
    if !state.dead {
        return;
    }
    if let Ok(mut cursor) = windows.single_mut() {
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    }
}

fn handle_respawn_button(
    buttons: Query<&Interaction, (Changed<Interaction>, With<RespawnButton>)>,
    mut transport: ResMut<net::Transport>,
    mut spawn_state: ResMut<net::SpawnState>,
    mut players: Query<&mut Player>,
) {
    for interaction in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        transport.send(ClientToServer::Respawn);
        if let Ok(mut player) = players.single_mut() {
            player.velocity = Vec3::ZERO;
        }
        // Hands positioning back to the same ground-snap logic a save-less
        // fresh spawn uses (`net::resolve_spawn`), rather than duplicating
        // it here.
        *spawn_state = net::SpawnState::AwaitingColumn;
    }
}

/// Part of the `OnExit(AppState::InGame)` "despawn everything in-game"
/// contract (see `pause` module docs): drops any live death overlay and
/// resets the tracked UI state for the next session.
fn teardown_on_exit(mut commands: Commands, mut ui_state: ResMut<DeathUi>) {
    if let Some(root) = ui_state.0.take() {
        commands.entity(root).despawn();
    }
}

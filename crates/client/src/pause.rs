//! In-game pause menu (design.md art direction; reuses [`crate::ui`]/
//! [`crate::settings`]'s panel style).
//!
//! - [`PauseState`] is a plain `States` enum layered over
//!   [`AppState::InGame`] rather than a `SubStates` (a `SubStates` needs its
//!   own always-present source-state value to key off; a plain enum reset to
//!   `Playing` on `OnEnter(AppState::InGame)` is simpler and was explicitly
//!   allowed as the fallback). It stays `Playing` while in the menu; nothing
//!   reads it there.
//! - Escape while playing opens the pause menu and releases the cursor
//!   (this replaces Escape's old release-only role in [`crate::camera`]:
//!   the click-to-grab behavior when unpaused is unchanged). Escape or
//!   "Resume" closes it; Escape from the Settings sub-panel goes back to the
//!   main pause panel, same as its Back button.
//! - The pause UI (dark overlay + buttons, or the shared settings panel) is
//!   spawned/despawned reactively by [`sync_pause_ui`], purely as a function
//!   of the current [`PauseState`] — every other system in this module only
//!   ever sets `NextState<PauseState>`, never touches UI entities. This is
//!   also what lets [`crate::screenshot`]'s pause-capture mode trigger the
//!   real pause UI just by setting the state, with no need to simulate an
//!   Escape keypress.
//! - While paused ([`PauseState::Paused`] or [`PauseState::Settings`]),
//!   player look/movement/jump ([`crate::camera`]), interact
//!   ([`crate::interact`]) and hotbar input ([`crate::hotbar`]) are gated
//!   off via [`is_playing`] as a run condition in their own modules; chunk
//!   streaming, remote-player interpolation and the transport pump
//!   ([`crate::net`], [`crate::view`], [`crate::remote`]) keep running
//!   unconditionally — the world stays server-authoritative and alive, as in
//!   multiplayer Minecraft.
//! - `OnExit(AppState::InGame)`: this module's slice of the "despawn
//!   everything in-game" contract is tearing down any live pause UI and
//!   resetting [`PauseState`] back to `Playing` for the next session.

use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use tsumiki_protocol::ClientToServer;

use crate::net;
use crate::settings::{self, Settings};
use crate::ui;
use crate::{AppState, UiFont};

const OVERLAY_BG: Color = Color::srgba(0.05, 0.04, 0.08, 0.55);
const RESUME_COLOR: Color = Color::srgb(0.43, 0.78, 0.36);
const SETTINGS_COLOR: Color = Color::srgb(0.45, 0.55, 0.68);
const BACK_TO_TITLE_COLOR: Color = Color::srgb(0.62, 0.54, 0.30);
const QUIT_COLOR: Color = Color::srgb(0.75, 0.32, 0.30);
const BACK_COLOR: Color = Color::srgb(0.62, 0.54, 0.30);

/// The pause sub-state machine. See the module docs.
#[derive(States, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum PauseState {
    #[default]
    Playing,
    Paused,
    Settings,
}

/// Run condition used by other modules (`camera`, `interact`, `hotbar`) to
/// gate player-control systems off while paused.
pub fn is_playing(state: Res<State<PauseState>>) -> bool {
    *state.get() == PauseState::Playing
}

/// A pause-UI button's click-handler tag.
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
enum PauseButtonAction {
    Resume,
    Settings,
    /// Only present on the settings sub-panel; returns to the main panel.
    Back,
    BackToTitle,
    Quit,
}

/// Which pause UI, if any, is currently spawned. Purely a cache of "what did
/// we last build", compared against the current [`PauseState`] by
/// [`sync_pause_ui`].
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum PauseUiKind {
    #[default]
    None,
    Main,
    Settings,
}

#[derive(Resource, Default)]
struct PauseUi {
    kind: PauseUiKind,
    root: Option<Entity>,
}

/// Wires the pause state, its reactive UI, and the systems that gate
/// gameplay input off while paused into `app`.
pub fn install(app: &mut App) {
    app.insert_state(PauseState::Playing)
        .init_resource::<PauseUi>()
        .add_systems(OnEnter(AppState::InGame), reset_pause_state)
        .add_systems(OnExit(AppState::InGame), teardown_pause_ui)
        .add_systems(
            Update,
            (
                handle_escape,
                sync_cursor_for_pause,
                sync_pause_ui,
                handle_pause_buttons,
            )
                .chain()
                .run_if(in_state(AppState::InGame)),
        );
}

fn reset_pause_state(mut next: ResMut<NextState<PauseState>>) {
    next.set(PauseState::Playing);
}

fn teardown_pause_ui(mut commands: Commands, mut ui_state: ResMut<PauseUi>) {
    if let Some(root) = ui_state.root.take() {
        commands.entity(root).despawn();
    }
    ui_state.kind = PauseUiKind::None;
}

/// Escape: `Playing` -> `Paused`, `Paused` -> `Playing` (same as "Resume"),
/// `Settings` -> `Paused` (same as "Back").
fn handle_escape(
    keys: Res<ButtonInput<KeyCode>>,
    state: Res<State<PauseState>>,
    mut next: ResMut<NextState<PauseState>>,
) {
    if !keys.just_pressed(KeyCode::Escape) {
        return;
    }
    match state.get() {
        PauseState::Playing => next.set(PauseState::Paused),
        PauseState::Paused => next.set(PauseState::Playing),
        PauseState::Settings => next.set(PauseState::Paused),
    }
}

/// Releases the cursor the moment the game leaves `Playing` (Escape's old
/// release-only role, now driven by the pause state instead of the raw key
/// event). Resuming does not re-grab automatically: the existing
/// click-to-grab behavior in [`crate::camera::grab_cursor`] handles that,
/// unchanged.
fn sync_cursor_for_pause(
    state: Res<State<PauseState>>,
    mut windows: Query<&mut CursorOptions, With<PrimaryWindow>>,
) {
    if !state.is_changed() || *state.get() == PauseState::Playing {
        return;
    }
    if let Ok(mut cursor) = windows.single_mut() {
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    }
}

/// Spawns/despawns the pause UI as a pure function of the current
/// [`PauseState`] — the only system in this module that touches UI entities.
fn sync_pause_ui(
    state: Res<State<PauseState>>,
    mut ui_state: ResMut<PauseUi>,
    mut commands: Commands,
    settings: Res<Settings>,
    font: Res<UiFont>,
) {
    let desired = match state.get() {
        PauseState::Playing => PauseUiKind::None,
        PauseState::Paused => PauseUiKind::Main,
        PauseState::Settings => PauseUiKind::Settings,
    };
    if ui_state.kind == desired {
        return;
    }
    if let Some(root) = ui_state.root.take() {
        commands.entity(root).despawn();
    }
    ui_state.root = match desired {
        PauseUiKind::None => None,
        PauseUiKind::Main => Some(spawn_main_panel(&mut commands, &font)),
        PauseUiKind::Settings => Some(spawn_settings_panel(&mut commands, &settings, &font)),
    };
    ui_state.kind = desired;
}

fn spawn_overlay_root(commands: &mut Commands) -> Entity {
    commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                position_type: PositionType::Absolute,
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                ..default()
            },
            BackgroundColor(OVERLAY_BG),
        ))
        .id()
}

fn spawn_panel_container(commands: &mut Commands) -> Entity {
    commands
        .spawn((
            Node {
                width: Val::Px(ui::PANEL_WIDTH),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Stretch,
                row_gap: Val::Px(16.0),
                padding: UiRect::all(Val::Px(28.0)),
                border_radius: BorderRadius::all(Val::Px(18.0)),
                ..default()
            },
            BackgroundColor(ui::PANEL_BG),
        ))
        .id()
}

fn spawn_main_panel(commands: &mut Commands, font: &UiFont) -> Entity {
    let root = spawn_overlay_root(commands);
    let panel = spawn_panel_container(commands);
    commands.entity(panel).with_children(|parent| {
        ui::spawn_button(
            parent,
            PauseButtonAction::Resume,
            "Resume",
            RESUME_COLOR,
            font,
        );
        ui::spawn_button(
            parent,
            PauseButtonAction::Settings,
            "Settings",
            SETTINGS_COLOR,
            font,
        );
        ui::spawn_button(
            parent,
            PauseButtonAction::BackToTitle,
            "Back to Title",
            BACK_TO_TITLE_COLOR,
            font,
        );
        ui::spawn_button(parent, PauseButtonAction::Quit, "Quit", QUIT_COLOR, font);
    });
    commands.entity(root).add_child(panel);
    root
}

fn spawn_settings_panel(commands: &mut Commands, settings: &Settings, font: &UiFont) -> Entity {
    let root = spawn_overlay_root(commands);
    let panel = spawn_panel_container(commands);
    commands.entity(panel).with_children(|parent| {
        settings::spawn_settings_rows(parent, settings, font);
        ui::spawn_button(parent, PauseButtonAction::Back, "Back", BACK_COLOR, font);
    });
    commands.entity(root).add_child(panel);
    root
}

fn handle_pause_buttons(
    buttons: Query<(&Interaction, &PauseButtonAction), Changed<Interaction>>,
    mut next_pause: ResMut<NextState<PauseState>>,
    mut next_app: ResMut<NextState<AppState>>,
    mut exit: MessageWriter<AppExit>,
    mut transport: Option<ResMut<net::Transport>>,
    mut commands: Commands,
) {
    for (interaction, action) in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        match action {
            PauseButtonAction::Resume => next_pause.set(PauseState::Playing),
            PauseButtonAction::Settings => next_pause.set(PauseState::Settings),
            PauseButtonAction::Back => next_pause.set(PauseState::Paused),
            PauseButtonAction::BackToTitle => {
                if let Some(transport) = transport.as_mut() {
                    transport.send(ClientToServer::Goodbye);
                    transport.flush();
                }
                commands.remove_resource::<net::Transport>();
                next_app.set(AppState::Menu);
            }
            PauseButtonAction::Quit => {
                exit.write(AppExit::Success);
            }
        }
    }
}

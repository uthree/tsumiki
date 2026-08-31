//! Title menu (design.md art direction: pop/toy-like, no pure black/white;
//! design.md §1 decoupling: the menu only knows [`MenuHooks`], never the
//! server/net crates).
//!
//! - `OnEnter(AppState::Menu)` ([`setup_menu`]) spawns a decorative camera +
//!   slowly rotating toy-block cluster + light (the "backdrop"), and the UI:
//!   a big "tsumiki" title over an underlined bar, and a panel holding
//!   whichever [`MenuPanel`] is current:
//!   - [`MenuPanel::Main`][] -- Singleplayer/Multiplayer/Settings/Quit.
//!   - [`MenuPanel::Connect`][] -- the multiplayer connect form (server
//!     address + name fields, Connect/Back).
//!   - [`MenuPanel::Settings`][] -- the shared settings panel
//!     ([`crate::settings`]) — the same panel the in-game pause menu
//!     ([`crate::pause`]) opens, so tweaking a setting looks and behaves
//!     identically from either screen.
//!   - [`MenuPanel::WorldSelect`][] -- the singleplayer world list (name /
//!     game mode / relative last-played, newest first, scrollable), reached
//!     from Singleplayer. Selecting a row highlights it; Play starts it,
//!     double-clicking a row plays it directly, and Delete asks for
//!     confirmation (a small inline "Delete <name>? Yes / Cancel" step --
//!     see [`WorldSelectFlow`]) before calling the `delete` hook and
//!     refreshing the list. Create New World goes to
//!     [`MenuPanel::CreateWorld`][]; Back returns to [`MenuPanel::Main`][].
//!   - [`MenuPanel::CreateWorld`][] -- the new-world form (name, seed, a
//!     Survival/Creative toggle with a one-line explanation). The Create
//!     button greys out and explains why while the name is invalid or
//!     already taken ([`create_disabled_reason`]); on success it calls
//!     `create` then `start`, dropping the player straight into the new
//!     world, exactly as Minecraft does.
//! - `OnExit(AppState::Menu)` ([`teardown_menu`]) despawns everything tagged
//!   [`MenuEntity`] and drops the menu-only resources.
//! - Text entry goes through [`apply_key_to_field`], a pure helper (append
//!   printable text, backspace, ignore control/unprintable input) that is
//!   unit-tested below without needing a Bevy `App`.
//! - Successfully obtaining a transport (any of the connect/start/create
//!   hooks) inserts it as [`crate::net::Transport`] and transitions to
//!   [`AppState::InGame`]; a failure shows the error in the panel and stays
//!   put.
//! - [`MenuScreenshotNav`] lets [`crate::screenshot`] drive straight to
//!   [`MenuPanel::WorldSelect`]/[`MenuPanel::CreateWorld`] for automated
//!   verification, exactly as if the corresponding button(s) had been
//!   clicked.
//! - The cursor is never grabbed here (that's [`crate::camera::grab_cursor`],
//!   which only runs in [`AppState::InGame`]).

use std::time::{Duration, Instant};

use bevy::input::ButtonState;
use bevy::input::keyboard::KeyboardInput;
use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::prelude::*;
use tsumiki_protocol::GameMode;
use tsumiki_world::{BlockId, blocks};

use crate::net;
use crate::settings::{self, Settings};
use crate::ui;
use crate::view::Registry;
use crate::{
    AppState, ClientConfig, MAX_WORLD_NAME_CHARS, MenuHooks, NewWorld, ScreenshotTarget, UiFont,
    WorldEntry, world_name_is_valid,
};

/// Rotation speed of the decorative toy-block cluster, radians/sec.
const CLUSTER_SPIN_RATE: f32 = 0.4;

// Font sizes: multiples of 8 (doc/assets.md §1.1 — Misaki Gothic is an 8×8
// bitmap font, and only stays crisp at multiples of its grid).
const TITLE_FONT_SIZE: f32 = 96.0;
const LABEL_FONT_SIZE: f32 = 16.0;
const FIELD_FONT_SIZE: f32 = 24.0;
const ERROR_FONT_SIZE: f32 = 16.0;

const FIELD_WIDTH: f32 = 320.0;
const FIELD_HEIGHT: f32 = 40.0;

/// Tallest the world-select list gets before it scrolls instead of growing.
const WORLD_LIST_MAX_HEIGHT: f32 = 224.0;
const WORLD_ROW_GAP: f32 = 8.0;

/// Two clicks on the same row within this long count as a double-click.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// Pixels-per-line for `MouseScrollUnit::Line` wheel events, matching the
/// constant Bevy's own `scroll_and_overflow` example uses.
const SCROLL_LINE_HEIGHT: f32 = 21.0;

// Colors (design.md §8: no pure black, no pure white — a warm dark navy and
// a warm off-white bracket the ramp instead). The panel/button base colors
// live in `ui` now (shared with the pause menu and settings panel).
const TITLE_COLOR: Color = Color::srgb(0.97, 0.93, 0.83);
const UNDERLINE_COLOR: Color = Color::srgb(0.95, 0.55, 0.35);
const ERROR_TEXT_COLOR: Color = Color::srgb(0.92, 0.42, 0.38);
const SINGLEPLAYER_COLOR: Color = Color::srgb(0.43, 0.78, 0.36);
const MULTIPLAYER_COLOR: Color = Color::srgb(0.62, 0.61, 0.67);
const SETTINGS_COLOR: Color = Color::srgb(0.45, 0.55, 0.68);
const CONNECT_COLOR: Color = Color::srgb(0.43, 0.78, 0.36);
const BACK_QUIT_COLOR: Color = Color::srgb(0.62, 0.54, 0.30);
const FIELD_BG: Color = Color::srgb(0.20, 0.19, 0.26);
const FIELD_BG_FOCUSED: Color = Color::srgb(0.27, 0.26, 0.36);
const FIELD_BORDER: Color = Color::srgb(0.42, 0.40, 0.48);
const FIELD_BORDER_FOCUSED: Color = Color::srgb(0.93, 0.75, 0.38);

const PLAY_COLOR: Color = SINGLEPLAYER_COLOR;
const CREATE_COLOR: Color = CONNECT_COLOR;
const DELETE_COLOR: Color = Color::srgb(0.78, 0.35, 0.32);
const MODE_BUTTON_COLOR: Color = SETTINGS_COLOR;

const WORLD_ROW_BG: Color = FIELD_BG;
const WORLD_ROW_BG_SELECTED: Color = Color::srgb(0.30, 0.46, 0.30);
const WORLD_ROW_SUB_COLOR: Color = Color::srgb(0.72, 0.69, 0.64);

/// The decorative toy-block cluster: offset from the pivot, uniform scale,
/// and which registry block's top color to use.
const CLUSTER_BLOCKS: [(Vec3, f32, BlockId); 6] = [
    (Vec3::new(-0.55, -0.5, -0.55), 1.0, blocks::GRASS),
    (Vec3::new(0.55, -0.5, -0.55), 1.0, blocks::DIRT),
    (Vec3::new(-0.55, -0.5, 0.55), 1.0, blocks::STONE),
    (Vec3::new(0.55, -0.5, 0.55), 1.0, blocks::SAND),
    (Vec3::new(0.0, 0.55, 0.0), 1.0, blocks::LOG),
    (Vec3::new(1.7, -0.55, 0.5), 0.7, blocks::LEAVES),
];

/// Tags every top-level entity spawned for the menu (camera, light, cluster
/// pivot, UI root) so [`teardown_menu`] can despawn them (recursively, taking
/// their children — button/text/cluster-block entities — with them).
#[derive(Component)]
struct MenuEntity;

/// The decorative cluster's pivot, spun by [`spin_cluster`].
#[derive(Component)]
struct MenuCluster;

/// Tags a world-select row so [`handle_world_row_interactions`] can tell
/// which world it represents without a separate lookup table.
#[derive(Component)]
struct WorldRow(String);

/// Tags the world-select list's scrollable container so
/// [`scroll_world_list`] can find it without walking the whole panel.
#[derive(Component)]
struct WorldListScroll;

/// Which panel the menu is currently showing.
#[derive(Resource, Clone, Copy, PartialEq, Eq, Debug)]
enum MenuPanel {
    Main,
    Connect,
    Settings,
    WorldSelect,
    CreateWorld,
}

/// Entities that make up the persistent menu chrome (title bar) and the
/// currently-swapped-in panel, so panel switches can despawn/respawn just the
/// panel without touching the title or backdrop.
#[derive(Resource)]
struct MenuUi {
    panel_container: Entity,
    current_panel: Entity,
}

/// The text field entity currently receiving keyboard input, if any.
#[derive(Resource, Default)]
struct FocusedField(Option<Entity>);

/// The connect form's field/error-text entities, so the Connect button
/// handler and the error display can find them without a generic query.
/// Only present while [`MenuPanel::Connect`] is showing.
#[derive(Resource)]
struct ConnectFields {
    address: Entity,
    name: Entity,
    error_text: Entity,
}

/// Pure state machine for the world-select screen's selection / delete-
/// confirm flow, factored out so its transitions are unit-testable without a
/// Bevy `App`. [`WorldSelectState`]'s owning systems only ever mutate it
/// through these methods.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct WorldSelectFlow {
    selected: Option<String>,
    pending_delete: Option<String>,
}

impl WorldSelectFlow {
    /// Selects `name`. Cancels any pending delete confirmation -- picking a
    /// different row is an implicit "never mind" on whichever one was being
    /// confirmed.
    fn select(&mut self, name: &str) {
        self.pending_delete = None;
        self.selected = Some(name.to_string());
    }

    /// Starts a delete confirmation for the current selection. Returns
    /// `false` (a no-op) if nothing is selected.
    fn request_delete(&mut self) -> bool {
        let Some(selected) = self.selected.clone() else {
            return false;
        };
        self.pending_delete = Some(selected);
        true
    }

    /// Backs out of a delete confirmation without deleting anything.
    fn cancel_delete(&mut self) {
        self.pending_delete = None;
    }

    /// Called once the delete hook has actually run, success or failure:
    /// clears the confirmation either way. A failed delete isn't retried
    /// silently -- the user has to press Delete again.
    fn confirm_delete_resolved(&mut self) {
        self.pending_delete = None;
        self.selected = None;
    }
}

/// World-select screen's list + selection/delete-confirm state. Only
/// present while [`MenuPanel::WorldSelect`] is showing. Every action that
/// changes it triggers a full respawn of the panel's content (see the
/// `spawn_world_select_panel` call sites below), so nothing here needs a
/// per-frame redraw system -- contrast [`CreateWorldFields`], whose Create
/// button reacts to live typing and does need one.
#[derive(Resource)]
struct WorldSelectState {
    entries: Vec<WorldEntry>,
    flow: WorldSelectFlow,
    error: Option<String>,
    /// Vertical scroll offset (logical px), preserved across rebuilds so
    /// selecting a row doesn't snap the list back to the top.
    scroll: f32,
    /// The most recent row click (world name + when), for double-click
    /// detection.
    last_click: Option<(String, Instant)>,
}

impl WorldSelectState {
    fn new(entries: Vec<WorldEntry>) -> Self {
        Self {
            entries,
            flow: WorldSelectFlow::default(),
            error: None,
            scroll: 0.0,
            last_click: None,
        }
    }
}

/// The create-world form's field/error/button entities. Only present while
/// [`MenuPanel::CreateWorld`] is showing.
#[derive(Resource)]
struct CreateWorldFields {
    name_field: Entity,
    seed_field: Entity,
    mode_label_text: Entity,
    mode_explanation: Entity,
    error_text: Entity,
    create_button: Entity,
    create_reason: Entity,
    /// Snapshot of the world list taken when the form was opened, so the
    /// Create button's validity check doesn't need to re-call the `list`
    /// hook on every keystroke.
    existing: Vec<WorldEntry>,
}

/// The create-world form's current game-mode choice, separate from
/// [`CreateWorldFields`] so toggling it doesn't need write access to the
/// (otherwise read-only after spawn) field entities. Only present while
/// [`MenuPanel::CreateWorld`] is showing.
#[derive(Resource, Clone, Copy)]
struct SelectedGameMode(GameMode);

/// Requests that the menu jump straight to [`MenuPanel::WorldSelect`] or
/// [`MenuPanel::CreateWorld`], exactly as if the corresponding button(s) had
/// been clicked. Inserted by [`crate::screenshot`] for automated
/// verification of those two screens; consumed (removed) the first time
/// [`apply_screenshot_navigation`] sees it.
#[derive(Resource, Clone, Copy)]
pub struct MenuScreenshotNav(pub ScreenshotTarget);

/// A clickable menu action, attached to every button entity.
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
enum MenuButtonAction {
    Singleplayer,
    Multiplayer,
    Settings,
    Quit,
    Connect,
    /// Returns to [`MenuPanel::Main`] — shared by the connect form, the
    /// settings panel, the world-select screen and the create-world form.
    Back,
    /// World-select screen: opens [`MenuPanel::CreateWorld`].
    CreateNewWorld,
    /// World-select screen: starts the selected world. A no-op with nothing
    /// selected.
    PlaySelectedWorld,
    /// World-select screen: asks for confirmation before deleting the
    /// selected world. A no-op with nothing selected.
    DeleteSelectedWorld,
    /// World-select screen's delete-confirm step: actually deletes.
    ConfirmDelete,
    /// World-select screen's delete-confirm step: backs out without
    /// deleting.
    CancelDelete,
    /// Create-world form: cycles [`SelectedGameMode`] between Survival and
    /// Creative.
    ToggleGameMode,
    /// Create-world form: creates the world and starts it, dropping the
    /// player straight in (as Minecraft does). A no-op while the name is
    /// invalid or already taken.
    CreateWorld,
}

/// A text-entry box: its edit buffer, kept in sync with the `Text` on the
/// same entity by [`handle_text_input`].
#[derive(Component)]
struct TextFieldBox {
    buffer: String,
}

/// Wires the menu's setup/teardown and per-frame systems into `app`.
pub fn install(app: &mut App) {
    app.add_systems(OnEnter(AppState::Menu), setup_menu)
        .add_systems(OnExit(AppState::Menu), teardown_menu)
        .add_systems(
            Update,
            (
                spin_cluster,
                handle_field_click,
                draw_field_focus,
                handle_text_input,
                handle_button_actions,
                handle_world_select_actions,
                handle_world_row_interactions,
                handle_create_world_actions,
                update_create_world_button,
                scroll_world_list,
                apply_screenshot_navigation,
                handle_escape,
            )
                .run_if(in_state(AppState::Menu)),
        );
}

fn setup_menu(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    registry: Res<Registry>,
    hooks: Res<MenuHooks>,
    ui_font: Res<UiFont>,
) {
    spawn_backdrop(&mut commands, &mut meshes, &mut materials, &registry);

    let has_singleplayer = hooks.singleplayer.is_some();

    let panel_container = commands
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
        .id();
    let current_panel = spawn_main_panel(&mut commands, has_singleplayer, &ui_font);
    commands.entity(panel_container).add_child(current_panel);

    let root = commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                position_type: PositionType::Absolute,
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                row_gap: Val::Px(28.0),
                ..default()
            },
            MenuEntity,
        ))
        .with_children(|root| {
            root.spawn((
                Text::new("tsumiki"),
                ui_font.text(TITLE_FONT_SIZE),
                TextColor(TITLE_COLOR),
            ));
            root.spawn((
                Node {
                    width: Val::Px(220.0),
                    height: Val::Px(6.0),
                    border_radius: BorderRadius::all(Val::Px(3.0)),
                    ..default()
                },
                BackgroundColor(UNDERLINE_COLOR),
            ));
        })
        .id();
    commands.entity(root).add_child(panel_container);

    commands.insert_resource(MenuUi {
        panel_container,
        current_panel,
    });
    commands.insert_resource(MenuPanel::Main);
    commands.insert_resource(FocusedField::default());
}

/// Despawns every menu entity (recursively — this takes buttons, text and
/// cluster blocks with it) and drops the menu-only resources, including
/// [`MenuHooks`] itself (its hooks may already be partially consumed, and
/// none are needed once in-game).
fn teardown_menu(mut commands: Commands, entities: Query<Entity, With<MenuEntity>>) {
    for entity in &entities {
        commands.entity(entity).despawn();
    }
    commands.remove_resource::<MenuUi>();
    commands.remove_resource::<MenuPanel>();
    commands.remove_resource::<FocusedField>();
    commands.remove_resource::<ConnectFields>();
    commands.remove_resource::<WorldSelectState>();
    commands.remove_resource::<CreateWorldFields>();
    commands.remove_resource::<SelectedGameMode>();
    commands.remove_resource::<MenuScreenshotNav>();
    commands.remove_resource::<MenuHooks>();
}

/// Spawns the purely decorative camera, light and slowly rotating toy-block
/// cluster that sit behind the UI.
fn spawn_backdrop(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    registry: &Registry,
) {
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(0.0, 1.6, 6.5).looking_at(Vec3::new(0.0, 0.6, 0.0), Vec3::Y),
        MenuEntity,
    ));

    commands.spawn((
        DirectionalLight {
            color: Color::srgb(1.0, 0.97, 0.9),
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::default().with_rotation(
            Quat::from_rotation_y(35f32.to_radians())
                * Quat::from_rotation_x((-55f32).to_radians()),
        ),
        MenuEntity,
    ));

    let pivot = commands
        .spawn((
            Transform::default(),
            Visibility::Inherited,
            MenuCluster,
            MenuEntity,
        ))
        .id();
    for (offset, scale, block) in CLUSTER_BLOCKS {
        let def = registry.0.get(block);
        let color = Color::srgb_u8(def.color_top[0], def.color_top[1], def.color_top[2]);
        let material = materials.add(StandardMaterial {
            base_color: color,
            perceptual_roughness: 1.0,
            ..default()
        });
        let mesh = meshes.add(Mesh::from(Cuboid::new(scale, scale, scale)));
        let child = commands
            .spawn((
                Mesh3d(mesh),
                MeshMaterial3d(material),
                Transform::from_translation(offset)
                    .with_rotation(Quat::from_rotation_y(offset.x + offset.z)),
            ))
            .id();
        commands.entity(pivot).add_child(child);
    }
}

fn spin_cluster(time: Res<Time>, mut pivots: Query<&mut Transform, With<MenuCluster>>) {
    for mut transform in &mut pivots {
        transform.rotate_y(CLUSTER_SPIN_RATE * time.delta_secs());
    }
}

/// Despawns the current panel's content and swaps in `new_panel`, keeping
/// the persistent chrome (title bar) untouched. Every panel transition in
/// this module goes through this one function so "how do I switch panels"
/// has a single answer.
fn swap_panel(commands: &mut Commands, menu_ui: &mut MenuUi, new_panel: Entity) {
    commands.entity(menu_ui.current_panel).despawn();
    commands
        .entity(menu_ui.panel_container)
        .add_child(new_panel);
    menu_ui.current_panel = new_panel;
}

/// Builds the main panel's content (Singleplayer/Multiplayer/Settings/Quit)
/// and returns its root entity. Singleplayer is omitted when the hook isn't
/// available (never provided by the launcher).
fn spawn_main_panel(commands: &mut Commands, has_singleplayer: bool, font: &UiFont) -> Entity {
    commands
        .spawn(Node {
            width: Val::Percent(100.0),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Stretch,
            row_gap: Val::Px(12.0),
            ..default()
        })
        .with_children(|parent| {
            if has_singleplayer {
                ui::spawn_button(
                    parent,
                    MenuButtonAction::Singleplayer,
                    "Singleplayer",
                    SINGLEPLAYER_COLOR,
                    font,
                );
            }
            ui::spawn_button(
                parent,
                MenuButtonAction::Multiplayer,
                "Multiplayer",
                MULTIPLAYER_COLOR,
                font,
            );
            ui::spawn_button(
                parent,
                MenuButtonAction::Settings,
                "Settings",
                SETTINGS_COLOR,
                font,
            );
            ui::spawn_button(
                parent,
                MenuButtonAction::Quit,
                "Quit",
                BACK_QUIT_COLOR,
                font,
            );
        })
        .id()
}

/// Builds the shared settings panel's content (the four setting rows plus a
/// Back button) and returns its root entity. Reused, unmodified, by the
/// pause menu's own settings sub-panel ([`crate::pause`]).
fn spawn_settings_panel(commands: &mut Commands, settings: &Settings, font: &UiFont) -> Entity {
    commands
        .spawn(Node {
            width: Val::Percent(100.0),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Stretch,
            row_gap: Val::Px(12.0),
            ..default()
        })
        .with_children(|parent| {
            settings::spawn_settings_rows(parent, settings, font);
            ui::spawn_button(
                parent,
                MenuButtonAction::Back,
                "Back",
                BACK_QUIT_COLOR,
                font,
            );
        })
        .id()
}

/// Builds the connect form's content (address + name fields, error text,
/// Connect/Back) and returns its root entity plus the entities the button
/// handler needs.
fn spawn_connect_panel(
    commands: &mut Commands,
    default_name: &str,
    font: &UiFont,
) -> (Entity, ConnectFields) {
    let mut address_field = Entity::PLACEHOLDER;
    let mut name_field = Entity::PLACEHOLDER;
    let mut error_text = Entity::PLACEHOLDER;

    let panel = commands
        .spawn(Node {
            width: Val::Percent(100.0),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Stretch,
            row_gap: Val::Px(8.0),
            ..default()
        })
        .with_children(|parent| {
            spawn_field_label(parent, "Server address", font);
            address_field = spawn_text_field(parent, "", font);
            spawn_field_label(parent, "Name", font);
            name_field = spawn_text_field(parent, default_name, font);
            error_text = parent
                .spawn((
                    Text::new(""),
                    font.text(ERROR_FONT_SIZE),
                    TextColor(ERROR_TEXT_COLOR),
                ))
                .id();
            ui::spawn_button(
                parent,
                MenuButtonAction::Connect,
                "Connect",
                CONNECT_COLOR,
                font,
            );
            ui::spawn_button(
                parent,
                MenuButtonAction::Back,
                "Back",
                BACK_QUIT_COLOR,
                font,
            );
        })
        .id();

    (
        panel,
        ConnectFields {
            address: address_field,
            name: name_field,
            error_text,
        },
    )
}

/// Builds the world-select screen's content: the (possibly empty) world
/// list, an error line, and either the delete-confirm pair or the normal
/// Play/Delete/Create/Back buttons, depending on `state.flow`. Called both
/// on first entry and after every action that changes `state` -- see the
/// module docs on why a full rebuild, rather than incremental updates, is
/// this panel's redraw strategy.
fn spawn_world_select_panel(
    commands: &mut Commands,
    state: &WorldSelectState,
    font: &UiFont,
) -> Entity {
    commands
        .spawn(Node {
            width: Val::Percent(100.0),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Stretch,
            row_gap: Val::Px(12.0),
            ..default()
        })
        .with_children(|parent| {
            if state.entries.is_empty() {
                parent.spawn((
                    Text::new("No worlds yet."),
                    font.text(LABEL_FONT_SIZE),
                    TextColor(ui::PANEL_TEXT_COLOR),
                ));
                parent.spawn((
                    Text::new("Click Create New World below to make one."),
                    font.text(LABEL_FONT_SIZE),
                    TextColor(WORLD_ROW_SUB_COLOR),
                ));
            } else {
                spawn_world_list(parent, state, font);
            }

            if let Some(error) = &state.error {
                parent.spawn((
                    Text::new(error.clone()),
                    font.text(ERROR_FONT_SIZE),
                    TextColor(ERROR_TEXT_COLOR),
                ));
            }

            if let Some(pending) = &state.flow.pending_delete {
                parent.spawn((
                    Text::new(format!("Delete \"{pending}\"? This cannot be undone.")),
                    font.text(LABEL_FONT_SIZE),
                    TextColor(ERROR_TEXT_COLOR),
                ));
                ui::spawn_button(
                    parent,
                    MenuButtonAction::ConfirmDelete,
                    "Yes, delete",
                    DELETE_COLOR,
                    font,
                );
                ui::spawn_button(
                    parent,
                    MenuButtonAction::CancelDelete,
                    "Cancel",
                    BACK_QUIT_COLOR,
                    font,
                );
            } else {
                let has_selection = state.flow.selected.is_some();
                let play_color = if has_selection {
                    PLAY_COLOR
                } else {
                    ui::darken(PLAY_COLOR, 0.3)
                };
                let delete_color = if has_selection {
                    DELETE_COLOR
                } else {
                    ui::darken(DELETE_COLOR, 0.3)
                };
                ui::spawn_button(
                    parent,
                    MenuButtonAction::PlaySelectedWorld,
                    "Play",
                    play_color,
                    font,
                );
                ui::spawn_button(
                    parent,
                    MenuButtonAction::DeleteSelectedWorld,
                    "Delete",
                    delete_color,
                    font,
                );
                ui::spawn_button(
                    parent,
                    MenuButtonAction::CreateNewWorld,
                    "Create New World",
                    CREATE_COLOR,
                    font,
                );
                ui::spawn_button(
                    parent,
                    MenuButtonAction::Back,
                    "Back",
                    BACK_QUIT_COLOR,
                    font,
                );
            }
        })
        .id()
}

/// Builds the scrollable world list itself (only called when `state.entries`
/// is non-empty).
fn spawn_world_list(
    parent: &mut ChildSpawnerCommands<'_>,
    state: &WorldSelectState,
    font: &UiFont,
) {
    let now = current_unix_secs();
    parent
        .spawn((
            Node {
                flex_direction: FlexDirection::Column,
                max_height: Val::Px(WORLD_LIST_MAX_HEIGHT),
                overflow: Overflow::scroll_y(),
                row_gap: Val::Px(WORLD_ROW_GAP),
                ..default()
            },
            ScrollPosition(Vec2::new(0.0, state.scroll)),
            WorldListScroll,
        ))
        .with_children(|list| {
            for entry in &state.entries {
                let selected = state.flow.selected.as_deref() == Some(entry.name.as_str());
                spawn_world_row(list, entry, selected, now, font);
            }
        });
}

/// One row of the world list: the name, then a smaller line with the game
/// mode and relative last-played time.
fn spawn_world_row(
    parent: &mut ChildSpawnerCommands<'_>,
    entry: &WorldEntry,
    selected: bool,
    now_secs: u64,
    font: &UiFont,
) {
    parent
        .spawn((
            Node {
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(Val::Px(8.0)),
                border_radius: BorderRadius::all(Val::Px(8.0)),
                ..default()
            },
            BackgroundColor(if selected {
                WORLD_ROW_BG_SELECTED
            } else {
                WORLD_ROW_BG
            }),
            Interaction::None,
            WorldRow(entry.name.clone()),
        ))
        .with_children(|row| {
            row.spawn((
                Text::new(entry.name.clone()),
                font.text(FIELD_FONT_SIZE),
                TextColor(ui::PANEL_TEXT_COLOR),
            ));
            row.spawn((
                Text::new(format!(
                    "{} - {}",
                    game_mode_label(entry.game_mode),
                    format_last_played(now_secs, entry.last_played)
                )),
                font.text(LABEL_FONT_SIZE),
                TextColor(WORLD_ROW_SUB_COLOR),
            ));
        });
}

/// Builds the create-world form's content and returns its root entity plus
/// the entities its button handlers need.
fn spawn_create_world_panel(
    commands: &mut Commands,
    existing: &[WorldEntry],
    font: &UiFont,
) -> (Entity, CreateWorldFields) {
    let mut name_field = Entity::PLACEHOLDER;
    let mut seed_field = Entity::PLACEHOLDER;
    let mut mode_label_text = Entity::PLACEHOLDER;
    let mut mode_explanation = Entity::PLACEHOLDER;
    let mut error_text = Entity::PLACEHOLDER;
    let mut create_button = Entity::PLACEHOLDER;
    let mut create_reason = Entity::PLACEHOLDER;

    let default_mode = GameMode::Survival;
    let default_name = default_new_world_name(existing);
    let reason = create_disabled_reason(&default_name, existing);
    let create_color = if reason.is_none() {
        CREATE_COLOR
    } else {
        ui::darken(CREATE_COLOR, 0.3)
    };

    let panel = commands
        .spawn(Node {
            width: Val::Percent(100.0),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Stretch,
            row_gap: Val::Px(8.0),
            ..default()
        })
        .with_children(|parent| {
            spawn_field_label(parent, "World name", font);
            name_field = spawn_text_field(parent, &default_name, font);
            spawn_field_label(parent, "Seed (blank = random)", font);
            seed_field = spawn_text_field(parent, "", font);

            spawn_field_label(parent, "Game mode", font);
            mode_label_text = spawn_mode_toggle_button(parent, default_mode, font);
            mode_explanation = parent
                .spawn((
                    Text::new(game_mode_explanation(default_mode)),
                    font.text(LABEL_FONT_SIZE),
                    TextColor(WORLD_ROW_SUB_COLOR),
                ))
                .id();

            error_text = parent
                .spawn((
                    Text::new(""),
                    font.text(ERROR_FONT_SIZE),
                    TextColor(ERROR_TEXT_COLOR),
                ))
                .id();

            create_button = ui::spawn_button(
                parent,
                MenuButtonAction::CreateWorld,
                "Create World",
                create_color,
                font,
            );
            create_reason = parent
                .spawn((
                    Text::new(reason.clone().unwrap_or_default()),
                    font.text(ERROR_FONT_SIZE),
                    TextColor(WORLD_ROW_SUB_COLOR),
                ))
                .id();

            ui::spawn_button(
                parent,
                MenuButtonAction::Back,
                "Back",
                BACK_QUIT_COLOR,
                font,
            );
        })
        .id();

    (
        panel,
        CreateWorldFields {
            name_field,
            seed_field,
            mode_label_text,
            mode_explanation,
            error_text,
            create_button,
            create_reason,
            existing: existing.to_vec(),
        },
    )
}

/// Spawns the game-mode toggle button and returns its label's `Text` entity
/// (a child of the button itself), so [`handle_create_world_actions`] can
/// update it in place when the mode cycles.
fn spawn_mode_toggle_button(
    parent: &mut ChildSpawnerCommands<'_>,
    mode: GameMode,
    font: &UiFont,
) -> Entity {
    let mut label = Entity::PLACEHOLDER;
    parent
        .spawn((
            Button,
            Node {
                width: Val::Percent(100.0),
                height: Val::Px(ui::BUTTON_HEIGHT),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border_radius: BorderRadius::all(Val::Px(10.0)),
                ..default()
            },
            BackgroundColor(MODE_BUTTON_COLOR),
            ui::ButtonBase(MODE_BUTTON_COLOR),
            MenuButtonAction::ToggleGameMode,
        ))
        .with_children(|button| {
            label = button
                .spawn((
                    Text::new(game_mode_label(mode)),
                    font.text(ui::BUTTON_FONT_SIZE),
                    TextColor(ui::PANEL_TEXT_COLOR),
                ))
                .id();
        });
    label
}

fn spawn_field_label(parent: &mut ChildSpawnerCommands<'_>, label: &str, font: &UiFont) {
    parent.spawn((
        Text::new(label),
        font.text(LABEL_FONT_SIZE),
        TextColor(ui::PANEL_TEXT_COLOR),
    ));
}

/// A clickable, focusable text-entry box. `Text` lives on the same entity as
/// the `Node`/background/border, so its displayed content is just kept equal
/// to `TextFieldBox::buffer`.
fn spawn_text_field(parent: &mut ChildSpawnerCommands<'_>, initial: &str, font: &UiFont) -> Entity {
    parent
        .spawn((
            Node {
                width: Val::Px(FIELD_WIDTH),
                height: Val::Px(FIELD_HEIGHT),
                padding: UiRect::horizontal(Val::Px(10.0)),
                align_items: AlignItems::Center,
                border: UiRect::all(Val::Px(2.0)),
                border_radius: BorderRadius::all(Val::Px(8.0)),
                ..default()
            },
            BackgroundColor(FIELD_BG),
            BorderColor::all(FIELD_BORDER),
            Interaction::None,
            TextFieldBox {
                buffer: initial.to_string(),
            },
            Text::new(initial),
            font.text(FIELD_FONT_SIZE),
            TextColor(ui::PANEL_TEXT_COLOR),
        ))
        .id()
}

/// Clicking a text field focuses it.
#[allow(clippy::type_complexity)]
fn handle_field_click(
    mut focus: ResMut<FocusedField>,
    fields: Query<(Entity, &Interaction), (Changed<Interaction>, With<TextFieldBox>)>,
) {
    for (entity, interaction) in &fields {
        if *interaction == Interaction::Pressed {
            focus.0 = Some(entity);
        }
    }
}

/// Redraws every field's background/border to reflect which one (if any) is
/// focused. Runs unconditionally (cheap: at most two fields) rather than
/// tracking per-field change, since focus changing doesn't touch the field
/// entities themselves.
fn draw_field_focus(
    focus: Res<FocusedField>,
    mut fields: Query<(Entity, &mut BackgroundColor, &mut BorderColor), With<TextFieldBox>>,
) {
    for (entity, mut background, mut border) in &mut fields {
        let focused = focus.0 == Some(entity);
        *background = BackgroundColor(if focused { FIELD_BG_FOCUSED } else { FIELD_BG });
        *border = BorderColor::all(if focused {
            FIELD_BORDER_FOCUSED
        } else {
            FIELD_BORDER
        });
    }
}

/// Pure text-editing step for a menu text field: given the buffer's current
/// content and one keyboard event's key code + typed text, returns the new
/// buffer content.
///
/// - `Backspace` removes the last character (any text the platform happens
///   to attach to it is ignored — it's a control action, not text entry).
/// - Otherwise, printable characters from `text` are appended; control
///   characters (e.g. Enter's `\r`/`\n`) are dropped.
/// - Any other key with no resolved text (arrows, function keys, a dead key
///   awaiting composition, ...) leaves the buffer unchanged.
fn apply_key_to_field(buffer: &str, key_code: KeyCode, text: Option<&str>) -> String {
    if key_code == KeyCode::Backspace {
        let mut next = buffer.to_string();
        next.pop();
        return next;
    }
    let Some(text) = text else {
        return buffer.to_string();
    };
    let mut next = buffer.to_string();
    next.extend(text.chars().filter(|ch| !ch.is_control()));
    next
}

/// Applies keyboard input to the focused field, if any. Always drains the
/// event reader (even with nothing focused) so keystrokes typed before a
/// field gains focus never pile up and land on whatever gets focused later.
fn handle_text_input(
    mut events: MessageReader<KeyboardInput>,
    focus: Res<FocusedField>,
    mut fields: Query<(&mut TextFieldBox, &mut Text)>,
) {
    for event in events.read() {
        if event.state != ButtonState::Pressed {
            continue;
        }
        let Some(focused) = focus.0 else { continue };
        let Ok((mut field, mut text)) = fields.get_mut(focused) else {
            continue;
        };
        field.buffer = apply_key_to_field(&field.buffer, event.key_code, event.text.as_deref());
        text.0 = field.buffer.clone();
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_button_actions(
    mut commands: Commands,
    buttons: Query<(&Interaction, &MenuButtonAction), Changed<Interaction>>,
    fields: Query<&TextFieldBox>,
    hooks: Res<MenuHooks>,
    mut menu_ui: ResMut<MenuUi>,
    mut panel: ResMut<MenuPanel>,
    mut focus: ResMut<FocusedField>,
    mut config: ResMut<ClientConfig>,
    mut next_state: ResMut<NextState<AppState>>,
    mut exit: MessageWriter<AppExit>,
    connect_fields: Option<Res<ConnectFields>>,
    mut texts: Query<&mut Text, Without<TextFieldBox>>,
    ui_font: Res<UiFont>,
    settings: Res<Settings>,
) {
    for (interaction, action) in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        match action {
            MenuButtonAction::Singleplayer => {
                // Goes to the world-select screen rather than starting a
                // world directly. `Fn`, not `FnOnce` (see `MenuHooks`
                // docs): callable again on every visit, including after a
                // "Back to Title" round trip.
                if let Some(sp) = hooks.singleplayer.as_ref() {
                    let state = WorldSelectState::new(sorted_by_last_played((sp.list)()));
                    let new_panel = spawn_world_select_panel(&mut commands, &state, &ui_font);
                    swap_panel(&mut commands, &mut menu_ui, new_panel);
                    commands.insert_resource(state);
                    *panel = MenuPanel::WorldSelect;
                    focus.0 = None;
                }
            }
            MenuButtonAction::Multiplayer => {
                let (new_panel, connect_fields) =
                    spawn_connect_panel(&mut commands, &config.name, &ui_font);
                swap_panel(&mut commands, &mut menu_ui, new_panel);
                commands.insert_resource(connect_fields);
                *panel = MenuPanel::Connect;
                focus.0 = None;
            }
            MenuButtonAction::Settings => {
                let new_panel = spawn_settings_panel(&mut commands, &settings, &ui_font);
                swap_panel(&mut commands, &mut menu_ui, new_panel);
                *panel = MenuPanel::Settings;
                focus.0 = None;
            }
            MenuButtonAction::Quit => {
                exit.write(AppExit::Success);
            }
            MenuButtonAction::Connect => {
                let Some(connect_fields) = &connect_fields else {
                    continue;
                };
                let address = fields
                    .get(connect_fields.address)
                    .map(|f| f.buffer.clone())
                    .unwrap_or_default();
                let name = fields
                    .get(connect_fields.name)
                    .map(|f| f.buffer.clone())
                    .unwrap_or_default();
                match (hooks.connect)(&address) {
                    Ok(transport) => {
                        if !name.trim().is_empty() {
                            config.name = name;
                        }
                        commands.insert_resource(net::Transport::new(transport));
                        next_state.set(AppState::InGame);
                    }
                    Err(err) => {
                        if let Ok(mut text) = texts.get_mut(connect_fields.error_text) {
                            text.0 = err.to_string();
                        }
                    }
                }
            }
            MenuButtonAction::Back => {
                let has_singleplayer = hooks.singleplayer.is_some();
                let new_panel = spawn_main_panel(&mut commands, has_singleplayer, &ui_font);
                swap_panel(&mut commands, &mut menu_ui, new_panel);
                commands.remove_resource::<ConnectFields>();
                commands.remove_resource::<WorldSelectState>();
                commands.remove_resource::<CreateWorldFields>();
                commands.remove_resource::<SelectedGameMode>();
                *panel = MenuPanel::Main;
                focus.0 = None;
            }
            // Handled by `handle_world_select_actions`/
            // `handle_create_world_actions` respectively -- kept in the same
            // enum (rather than two) since they're all still "a menu button
            // was clicked", just routed to a system that has the specific
            // state each needs.
            MenuButtonAction::CreateNewWorld
            | MenuButtonAction::PlaySelectedWorld
            | MenuButtonAction::DeleteSelectedWorld
            | MenuButtonAction::ConfirmDelete
            | MenuButtonAction::CancelDelete
            | MenuButtonAction::ToggleGameMode
            | MenuButtonAction::CreateWorld => {}
        }
    }
}

/// Handles the world-select screen's own buttons (Create New World, Play,
/// Delete, and the delete-confirm pair). Split out from
/// [`handle_button_actions`] purely to stay under Bevy's per-system
/// parameter limit -- there's nothing conceptually different about these
/// button presses.
#[allow(clippy::too_many_arguments)]
fn handle_world_select_actions(
    mut commands: Commands,
    buttons: Query<(&Interaction, &MenuButtonAction), Changed<Interaction>>,
    state: Option<ResMut<WorldSelectState>>,
    mut menu_ui: ResMut<MenuUi>,
    mut panel: ResMut<MenuPanel>,
    ui_font: Res<UiFont>,
    hooks: Res<MenuHooks>,
    mut next_state: ResMut<NextState<AppState>>,
) {
    let Some(mut state) = state else { return };
    for (interaction, action) in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        match action {
            MenuButtonAction::CreateNewWorld => {
                let Some(sp) = hooks.singleplayer.as_ref() else {
                    continue;
                };
                let existing = (sp.list)();
                let (new_panel, fields) =
                    spawn_create_world_panel(&mut commands, &existing, &ui_font);
                swap_panel(&mut commands, &mut menu_ui, new_panel);
                commands.insert_resource(fields);
                commands.insert_resource(SelectedGameMode(GameMode::Survival));
                commands.remove_resource::<WorldSelectState>();
                *panel = MenuPanel::CreateWorld;
                return;
            }
            MenuButtonAction::PlaySelectedWorld => {
                let Some(name) = state.flow.selected.clone() else {
                    continue;
                };
                if !play_world(&mut commands, &hooks, &name, &mut state, &mut next_state) {
                    let new_panel = spawn_world_select_panel(&mut commands, &state, &ui_font);
                    swap_panel(&mut commands, &mut menu_ui, new_panel);
                }
            }
            MenuButtonAction::DeleteSelectedWorld => {
                if state.flow.request_delete() {
                    let new_panel = spawn_world_select_panel(&mut commands, &state, &ui_font);
                    swap_panel(&mut commands, &mut menu_ui, new_panel);
                }
            }
            MenuButtonAction::ConfirmDelete => {
                let Some(name) = state.flow.pending_delete.clone() else {
                    continue;
                };
                if let Some(sp) = hooks.singleplayer.as_ref() {
                    match (sp.delete)(&name) {
                        Ok(()) => {
                            state.entries = sorted_by_last_played((sp.list)());
                            state.error = None;
                        }
                        Err(err) => state.error = Some(err.to_string()),
                    }
                }
                state.flow.confirm_delete_resolved();
                let new_panel = spawn_world_select_panel(&mut commands, &state, &ui_font);
                swap_panel(&mut commands, &mut menu_ui, new_panel);
            }
            MenuButtonAction::CancelDelete => {
                state.flow.cancel_delete();
                let new_panel = spawn_world_select_panel(&mut commands, &state, &ui_font);
                swap_panel(&mut commands, &mut menu_ui, new_panel);
            }
            _ => {}
        }
    }
}

/// Handles clicks on world-list rows: single-click selects, double-click
/// (within [`DOUBLE_CLICK_WINDOW`]) plays the world directly.
fn handle_world_row_interactions(
    mut commands: Commands,
    rows: Query<(&Interaction, &WorldRow), Changed<Interaction>>,
    state: Option<ResMut<WorldSelectState>>,
    mut menu_ui: ResMut<MenuUi>,
    ui_font: Res<UiFont>,
    hooks: Res<MenuHooks>,
    mut next_state: ResMut<NextState<AppState>>,
) {
    let Some(mut state) = state else { return };
    if state.flow.pending_delete.is_some() {
        // Only Confirm/Cancel are live while a delete is pending.
        return;
    }
    let mut clicked = None;
    for (interaction, row) in &rows {
        if *interaction == Interaction::Pressed {
            clicked = Some(row.0.clone());
        }
    }
    let Some(name) = clicked else { return };

    let now = Instant::now();
    let double_clicked =
        is_double_click(state.last_click.as_ref(), &name, now, DOUBLE_CLICK_WINDOW);
    state.last_click = Some((name.clone(), now));
    state.flow.select(&name);

    if double_clicked && play_world(&mut commands, &hooks, &name, &mut state, &mut next_state) {
        return;
    }
    let new_panel = spawn_world_select_panel(&mut commands, &state, &ui_font);
    swap_panel(&mut commands, &mut menu_ui, new_panel);
}

/// Starts `name` via the `start` hook and, on success, inserts the
/// transport and transitions into the world -- shared by the row
/// double-click and the Play button. Returns whether it succeeded; on
/// failure the caller is responsible for redrawing the panel so
/// `state.error` becomes visible.
fn play_world(
    commands: &mut Commands,
    hooks: &MenuHooks,
    name: &str,
    state: &mut WorldSelectState,
    next_state: &mut NextState<AppState>,
) -> bool {
    let Some(sp) = hooks.singleplayer.as_ref() else {
        return false;
    };
    match (sp.start)(name) {
        Ok(transport) => {
            commands.insert_resource(net::Transport::new(transport));
            next_state.set(AppState::InGame);
            true
        }
        Err(err) => {
            state.error = Some(err.to_string());
            false
        }
    }
}

/// Handles the create-world form's own buttons: the game-mode toggle and
/// Create itself. Split out from [`handle_button_actions`] for the same
/// reason as [`handle_world_select_actions`].
#[allow(clippy::too_many_arguments)]
fn handle_create_world_actions(
    mut commands: Commands,
    buttons: Query<(&Interaction, &MenuButtonAction), Changed<Interaction>>,
    create_fields: Option<Res<CreateWorldFields>>,
    game_mode: Option<ResMut<SelectedGameMode>>,
    fields: Query<&TextFieldBox>,
    mut texts: Query<&mut Text, Without<TextFieldBox>>,
    hooks: Res<MenuHooks>,
    mut next_state: ResMut<NextState<AppState>>,
) {
    let Some(create_fields) = create_fields else {
        return;
    };
    let Some(mut game_mode) = game_mode else {
        return;
    };
    for (interaction, action) in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        match action {
            MenuButtonAction::ToggleGameMode => {
                game_mode.0 = toggled_game_mode(game_mode.0);
                if let Ok(mut text) = texts.get_mut(create_fields.mode_label_text) {
                    text.0 = game_mode_label(game_mode.0).to_string();
                }
                if let Ok(mut text) = texts.get_mut(create_fields.mode_explanation) {
                    text.0 = game_mode_explanation(game_mode.0).to_string();
                }
            }
            MenuButtonAction::CreateWorld => {
                let name = fields
                    .get(create_fields.name_field)
                    .map(|f| f.buffer.clone())
                    .unwrap_or_default();
                if create_disabled_reason(&name, &create_fields.existing).is_some() {
                    continue; // Inert while invalid -- nothing to do.
                }
                let seed_text = fields
                    .get(create_fields.seed_field)
                    .map(|f| f.buffer.clone())
                    .unwrap_or_default();
                let seed = match parse_seed(&seed_text) {
                    Ok(seed) => seed,
                    Err(err) => {
                        if let Ok(mut text) = texts.get_mut(create_fields.error_text) {
                            text.0 = err;
                        }
                        continue;
                    }
                };
                let Some(sp) = hooks.singleplayer.as_ref() else {
                    continue;
                };
                let new_world = NewWorld {
                    name: name.trim().to_string(),
                    seed,
                    game_mode: game_mode.0,
                };
                let result = (sp.create)(&new_world).and_then(|()| (sp.start)(&new_world.name));
                match result {
                    Ok(transport) => {
                        commands.insert_resource(net::Transport::new(transport));
                        next_state.set(AppState::InGame);
                    }
                    Err(err) => {
                        if let Ok(mut text) = texts.get_mut(create_fields.error_text) {
                            text.0 = err.to_string();
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Keeps the Create button's enabled/disabled look (and the reason text next
/// to it) in sync with live typing in the name field. The only per-frame
/// redraw system in this module -- see [`WorldSelectState`]'s docs for why
/// everything else gets away with rebuild-on-action instead.
fn update_create_world_button(
    panel: Res<MenuPanel>,
    create_fields: Option<Res<CreateWorldFields>>,
    fields: Query<&TextFieldBox>,
    mut buttons: Query<(&Interaction, &mut BackgroundColor, &mut ui::ButtonBase)>,
    mut texts: Query<&mut Text, Without<TextFieldBox>>,
) {
    if *panel != MenuPanel::CreateWorld {
        return;
    }
    let Some(create_fields) = create_fields else {
        return;
    };
    let name = fields
        .get(create_fields.name_field)
        .map(|f| f.buffer.clone())
        .unwrap_or_default();
    let reason = create_disabled_reason(&name, &create_fields.existing);
    let resting = if reason.is_none() {
        CREATE_COLOR
    } else {
        ui::darken(CREATE_COLOR, 0.3)
    };
    if let Ok((interaction, mut background, mut base)) =
        buttons.get_mut(create_fields.create_button)
    {
        base.0 = resting;
        // Only snap the visible color when not actively hovered/pressed, so
        // this doesn't fight `ui::update_button_visuals`'s hover feedback.
        if *interaction == Interaction::None {
            *background = BackgroundColor(resting);
        }
    }
    if let Ok(mut text) = texts.get_mut(create_fields.create_reason) {
        text.0 = reason.unwrap_or_default();
    }
}

/// Scrolls the world-select list with the mouse wheel while it's showing,
/// clamped to its actual scrollable range and persisted into
/// [`WorldSelectState::scroll`] so a rebuild (e.g. selecting a row) doesn't
/// snap it back to the top.
fn scroll_world_list(
    mut wheel: MessageReader<MouseWheel>,
    panel: Res<MenuPanel>,
    state: Option<ResMut<WorldSelectState>>,
    mut lists: Query<(&mut ScrollPosition, &ComputedNode), With<WorldListScroll>>,
) {
    if *panel != MenuPanel::WorldSelect {
        wheel.clear();
        return;
    }
    let delta_y: f32 = wheel
        .read()
        .map(|ev| match ev.unit {
            MouseScrollUnit::Line => ev.y * SCROLL_LINE_HEIGHT,
            MouseScrollUnit::Pixel => ev.y,
        })
        .sum();
    if delta_y == 0.0 {
        return;
    }
    let mut state = state;
    for (mut scroll, computed) in &mut lists {
        let max_offset = ((computed.content_size().y - computed.size().y)
            * computed.inverse_scale_factor())
        .max(0.0);
        scroll.y = (scroll.y - delta_y).clamp(0.0, max_offset);
        if let Some(state) = state.as_mut() {
            state.scroll = scroll.y;
        }
    }
}

/// Drives the menu into [`MenuPanel::WorldSelect`]/[`MenuPanel::CreateWorld`]
/// on request from [`crate::screenshot`], for automated verification of
/// those two screens. See [`MenuScreenshotNav`].
fn apply_screenshot_navigation(
    mut commands: Commands,
    nav: Option<Res<MenuScreenshotNav>>,
    hooks: Res<MenuHooks>,
    mut menu_ui: ResMut<MenuUi>,
    mut panel: ResMut<MenuPanel>,
    ui_font: Res<UiFont>,
) {
    let Some(nav) = nav else { return };
    let target = nav.0;
    commands.remove_resource::<MenuScreenshotNav>();
    let Some(sp) = hooks.singleplayer.as_ref() else {
        return;
    };
    let existing = (sp.list)();
    match target {
        ScreenshotTarget::WorldSelect => {
            let state = WorldSelectState::new(sorted_by_last_played(existing));
            let new_panel = spawn_world_select_panel(&mut commands, &state, &ui_font);
            swap_panel(&mut commands, &mut menu_ui, new_panel);
            commands.insert_resource(state);
            *panel = MenuPanel::WorldSelect;
        }
        ScreenshotTarget::CreateWorld => {
            let (new_panel, fields) = spawn_create_world_panel(&mut commands, &existing, &ui_font);
            swap_panel(&mut commands, &mut menu_ui, new_panel);
            commands.insert_resource(fields);
            commands.insert_resource(SelectedGameMode(GameMode::Survival));
            *panel = MenuPanel::CreateWorld;
        }
        ScreenshotTarget::Menu
        | ScreenshotTarget::World
        | ScreenshotTarget::Pause
        | ScreenshotTarget::Inventory => {}
    }
}

/// Escape while any sub-panel is open goes back to the main panel (in-game
/// pause has its own, separate Escape handling in [`crate::pause`]).
#[allow(clippy::too_many_arguments)]
fn handle_escape(
    keys: Res<ButtonInput<KeyCode>>,
    mut commands: Commands,
    mut menu_ui: ResMut<MenuUi>,
    mut panel: ResMut<MenuPanel>,
    mut focus: ResMut<FocusedField>,
    hooks: Res<MenuHooks>,
    ui_font: Res<UiFont>,
) {
    let on_sub_panel = !matches!(*panel, MenuPanel::Main);
    if !on_sub_panel || !keys.just_pressed(KeyCode::Escape) {
        return;
    }
    let has_singleplayer = hooks.singleplayer.is_some();
    let new_panel = spawn_main_panel(&mut commands, has_singleplayer, &ui_font);
    swap_panel(&mut commands, &mut menu_ui, new_panel);
    commands.remove_resource::<ConnectFields>();
    commands.remove_resource::<WorldSelectState>();
    commands.remove_resource::<CreateWorldFields>();
    commands.remove_resource::<SelectedGameMode>();
    *panel = MenuPanel::Main;
    focus.0 = None;
}

/// Current wall-clock time as Unix seconds, for the "last played" line.
/// `0` in the vanishingly unlikely case the system clock predates the epoch,
/// so a row simply reads as very stale rather than panicking.
fn current_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Newest-`last_played`-first ordering for the world list. Worlds that have
/// never been played (`None`) sort after every played world, in their
/// original relative order (this is a stable sort).
fn sorted_by_last_played(mut entries: Vec<WorldEntry>) -> Vec<WorldEntry> {
    entries.sort_by(|a, b| match (a.last_played, b.last_played) {
        (Some(a), Some(b)) => b.cmp(&a),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    entries
}

const MINUTE_SECS: u64 = 60;
const HOUR_SECS: u64 = 60 * MINUTE_SECS;
const DAY_SECS: u64 = 24 * HOUR_SECS;
const MONTH_SECS: u64 = 30 * DAY_SECS;
const YEAR_SECS: u64 = 365 * DAY_SECS;

/// One row's "last played" line: "never played" if the world has no
/// timestamp, otherwise a relative description of `last_played` as seen
/// from `now_secs`.
fn format_last_played(now_secs: u64, last_played: Option<u64>) -> String {
    match last_played {
        None => "never played".to_string(),
        Some(then) => relative_time(now_secs, then),
    }
}

/// Relative description of `then_secs` as seen from `now_secs` (both Unix
/// seconds). A pure function of two integers, not the wall clock, so it's
/// exhaustively unit-testable. `then_secs` in the future (clock skew) reads
/// as "just now" rather than underflowing.
fn relative_time(now_secs: u64, then_secs: u64) -> String {
    let diff = now_secs.saturating_sub(then_secs);
    if diff < MINUTE_SECS {
        "just now".to_string()
    } else if diff < HOUR_SECS {
        plural_ago(diff / MINUTE_SECS, "minute")
    } else if diff < DAY_SECS {
        plural_ago(diff / HOUR_SECS, "hour")
    } else if diff < MONTH_SECS {
        plural_ago(diff / DAY_SECS, "day")
    } else if diff < YEAR_SECS {
        plural_ago(diff / MONTH_SECS, "month")
    } else {
        plural_ago(diff / YEAR_SECS, "year")
    }
}

fn plural_ago(n: u64, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{n} {unit}s ago")
    }
}

fn game_mode_label(mode: GameMode) -> &'static str {
    match mode {
        GameMode::Survival => "Survival",
        GameMode::Creative => "Creative",
    }
}

/// Kept honest about what the game actually does today: hunger arrives in
/// M8, so promising it here would be a lie in the one place a player is
/// choosing between the modes.
fn game_mode_explanation(mode: GameMode) -> &'static str {
    match mode {
        GameMode::Survival => {
            "Mine for every block you place, take falling and drowning damage, and drop your items when you die."
        }
        GameMode::Creative => "Fly freely with unlimited blocks and no health -- pure building.",
    }
}

fn toggled_game_mode(mode: GameMode) -> GameMode {
    match mode {
        GameMode::Survival => GameMode::Creative,
        GameMode::Creative => GameMode::Survival,
    }
}

/// Whether `name` (case-insensitively, trimmed) already names a world in
/// `existing`.
fn world_name_conflicts(name: &str, existing: &[WorldEntry]) -> bool {
    let trimmed = name.trim().to_lowercase();
    existing.iter().any(|w| w.name.to_lowercase() == trimmed)
}

/// The first unused "New World" / "New World 2" / ... name, so the
/// create-world form starts with something valid and clickable rather than
/// an empty field.
fn default_new_world_name(existing: &[WorldEntry]) -> String {
    const BASE: &str = "New World";
    if !world_name_conflicts(BASE, existing) {
        return BASE.to_string();
    }
    let mut n = 2u32;
    loop {
        let candidate = format!("{BASE} {n}");
        if !world_name_conflicts(&candidate, existing) {
            return candidate;
        }
        n += 1;
    }
}

/// Why the Create button is disabled for `name`, or `None` if it's fine.
/// Shown next to the button so "why can't I click this" is never a mystery.
fn create_disabled_reason(name: &str, existing: &[WorldEntry]) -> Option<String> {
    if name.trim().is_empty() {
        return Some("Enter a world name.".to_string());
    }
    if !world_name_is_valid(name) {
        return Some(format!(
            "Name must be 1-{MAX_WORLD_NAME_CHARS} characters and can't be \".\", \"..\", or contain / \\ : * ? \" < > | or control characters."
        ));
    }
    if world_name_conflicts(name, existing) {
        return Some("A world with this name already exists.".to_string());
    }
    None
}

/// Parses the seed field: blank means "pick a random seed"
/// ([`NewWorld::seed`] `None`); anything else must be a whole number.
fn parse_seed(text: &str) -> Result<Option<u64>, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed.parse::<u64>().map(Some).map_err(|_| {
        format!("Seed must be a whole number, or blank for random (got \"{trimmed}\").")
    })
}

/// Whether a click on `name` should count as a double-click on the previous
/// one, given the last click (world name + when) and the current time. Pure
/// so it's testable with manually constructed `Instant`s rather than real
/// wall-clock sleeps.
fn is_double_click(
    last: Option<&(String, Instant)>,
    name: &str,
    now: Instant,
    window: Duration,
) -> bool {
    match last {
        Some((last_name, at)) => last_name == name && now.saturating_duration_since(*at) <= window,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, last_played: Option<u64>) -> WorldEntry {
        WorldEntry {
            name: name.to_string(),
            game_mode: GameMode::Survival,
            last_played,
        }
    }

    #[test]
    fn appends_printable_text() {
        assert_eq!(apply_key_to_field("ab", KeyCode::KeyC, Some("c")), "abc");
    }

    #[test]
    fn backspace_removes_last_char() {
        assert_eq!(apply_key_to_field("abc", KeyCode::Backspace, None), "ab");
    }

    #[test]
    fn backspace_on_empty_buffer_is_a_no_op() {
        assert_eq!(apply_key_to_field("", KeyCode::Backspace, None), "");
    }

    #[test]
    fn backspace_ignores_any_accompanying_text() {
        // Backspace is control logic, not text entry: even if the platform
        // attaches text to the event, it must not be appended.
        assert_eq!(apply_key_to_field("ab", KeyCode::Backspace, Some("x")), "a");
    }

    #[test]
    fn control_characters_are_dropped() {
        assert_eq!(apply_key_to_field("ab", KeyCode::Enter, Some("\r")), "ab");
    }

    #[test]
    fn no_text_leaves_buffer_unchanged() {
        assert_eq!(apply_key_to_field("ab", KeyCode::ArrowLeft, None), "ab");
    }

    #[test]
    fn mixed_control_and_printable_keeps_only_printable() {
        assert_eq!(
            apply_key_to_field("a", KeyCode::KeyB, Some("b\u{7f}")),
            "ab"
        );
    }

    #[test]
    fn empty_buffer_accepts_first_character() {
        assert_eq!(apply_key_to_field("", KeyCode::KeyA, Some("a")), "a");
    }

    // -- relative time / last-played formatting --------------------------

    #[test]
    fn just_now_for_sub_minute() {
        assert_eq!(relative_time(1_000, 950), "just now");
    }

    #[test]
    fn singular_minute() {
        assert_eq!(relative_time(1_000, 1_000 - 60), "1 minute ago");
    }

    #[test]
    fn plural_minutes() {
        assert_eq!(relative_time(1_000, 1_000 - 180), "3 minutes ago");
    }

    #[test]
    fn singular_hour() {
        assert_eq!(relative_time(100_000, 100_000 - HOUR_SECS), "1 hour ago");
    }

    #[test]
    fn plural_hours() {
        assert_eq!(
            relative_time(100_000, 100_000 - 3 * HOUR_SECS),
            "3 hours ago"
        );
    }

    #[test]
    fn plural_days() {
        assert_eq!(
            relative_time(10_000_000, 10_000_000 - 2 * DAY_SECS),
            "2 days ago"
        );
    }

    #[test]
    fn plural_months() {
        assert_eq!(
            relative_time(100_000_000, 100_000_000 - 2 * MONTH_SECS),
            "2 months ago"
        );
    }

    #[test]
    fn plural_years() {
        assert_eq!(
            relative_time(1_000_000_000, 1_000_000_000 - 2 * YEAR_SECS),
            "2 years ago"
        );
    }

    #[test]
    fn future_timestamp_is_just_now() {
        // Clock skew shouldn't underflow/panic.
        assert_eq!(relative_time(100, 200), "just now");
    }

    #[test]
    fn never_played_when_none() {
        assert_eq!(format_last_played(1_000, None), "never played");
    }

    #[test]
    fn played_when_some() {
        assert_eq!(format_last_played(1_000, Some(940)), "1 minute ago");
    }

    // -- list ordering -----------------------------------------------------

    #[test]
    fn sorts_newest_played_first() {
        let entries = vec![entry("Old", Some(100)), entry("New", Some(200))];
        let sorted = sorted_by_last_played(entries);
        assert_eq!(
            sorted.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["New", "Old"]
        );
    }

    #[test]
    fn never_played_sorts_after_every_played_world() {
        let entries = vec![
            entry("Never", None),
            entry("Recent", Some(500)),
            entry("Older", Some(100)),
        ];
        let sorted = sorted_by_last_played(entries);
        assert_eq!(
            sorted.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["Recent", "Older", "Never"]
        );
    }

    #[test]
    fn never_played_worlds_keep_relative_order() {
        let entries = vec![entry("A", None), entry("B", None)];
        let sorted = sorted_by_last_played(entries);
        assert_eq!(
            sorted.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["A", "B"]
        );
    }

    // -- world-name conflict / default naming -------------------------------

    #[test]
    fn no_conflict_on_empty_list() {
        assert!(!world_name_conflicts("Foo", &[]));
    }

    #[test]
    fn conflict_is_case_insensitive_and_trims() {
        let existing = vec![entry("My World", None)];
        assert!(world_name_conflicts("my world", &existing));
        assert!(world_name_conflicts("  MY WORLD  ", &existing));
        assert!(!world_name_conflicts("My World 2", &existing));
    }

    #[test]
    fn default_name_is_new_world_when_unused() {
        assert_eq!(default_new_world_name(&[]), "New World");
    }

    #[test]
    fn default_name_increments_on_conflict() {
        let existing = vec![entry("New World", None)];
        assert_eq!(default_new_world_name(&existing), "New World 2");
        let existing = vec![entry("New World", None), entry("New World 2", None)];
        assert_eq!(default_new_world_name(&existing), "New World 3");
    }

    // -- create-button validity ---------------------------------------------

    #[test]
    fn empty_name_is_disabled_with_reason() {
        assert!(create_disabled_reason("", &[]).is_some());
        assert!(create_disabled_reason("   ", &[]).is_some());
    }

    #[test]
    fn invalid_name_is_disabled() {
        assert!(create_disabled_reason("a/b", &[]).is_some());
    }

    #[test]
    fn conflicting_name_is_disabled() {
        let existing = vec![entry("Taken", None)];
        assert!(create_disabled_reason("Taken", &existing).is_some());
    }

    #[test]
    fn valid_unused_name_is_enabled() {
        assert_eq!(create_disabled_reason("Brand New", &[]), None);
    }

    // -- seed parsing ---------------------------------------------------------

    #[test]
    fn blank_seed_is_random() {
        assert_eq!(parse_seed(""), Ok(None));
        assert_eq!(parse_seed("   "), Ok(None));
    }

    #[test]
    fn numeric_seed_parses() {
        assert_eq!(parse_seed("42"), Ok(Some(42)));
    }

    #[test]
    fn non_numeric_seed_is_an_error_not_a_panic() {
        assert!(parse_seed("banana").is_err());
        assert!(parse_seed("12.5").is_err());
        assert!(parse_seed("-1").is_err());
    }

    // -- game mode toggle -----------------------------------------------------

    #[test]
    fn toggle_is_its_own_inverse() {
        assert_eq!(toggled_game_mode(GameMode::Survival), GameMode::Creative);
        assert_eq!(toggled_game_mode(GameMode::Creative), GameMode::Survival);
    }

    // -- double-click detection -----------------------------------------------

    #[test]
    fn double_click_within_window_on_same_row() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(200);
        assert!(is_double_click(
            Some(&("Foo".to_string(), t0)),
            "Foo",
            t1,
            Duration::from_millis(400)
        ));
    }

    #[test]
    fn click_outside_window_is_not_double() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(500);
        assert!(!is_double_click(
            Some(&("Foo".to_string(), t0)),
            "Foo",
            t1,
            Duration::from_millis(400)
        ));
    }

    #[test]
    fn click_on_different_row_is_not_double() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(50);
        assert!(!is_double_click(
            Some(&("Foo".to_string(), t0)),
            "Bar",
            t1,
            Duration::from_millis(400)
        ));
    }

    #[test]
    fn no_previous_click_is_not_double() {
        assert!(!is_double_click(
            None,
            "Foo",
            Instant::now(),
            Duration::from_millis(400)
        ));
    }

    // -- world-select flow state machine --------------------------------------

    #[test]
    fn selecting_sets_selected() {
        let mut flow = WorldSelectFlow::default();
        flow.select("Foo");
        assert_eq!(flow.selected.as_deref(), Some("Foo"));
        assert_eq!(flow.pending_delete, None);
    }

    #[test]
    fn request_delete_without_selection_is_a_no_op() {
        let mut flow = WorldSelectFlow::default();
        assert!(!flow.request_delete());
        assert_eq!(flow.pending_delete, None);
    }

    #[test]
    fn request_delete_with_selection_sets_pending() {
        let mut flow = WorldSelectFlow::default();
        flow.select("Foo");
        assert!(flow.request_delete());
        assert_eq!(flow.pending_delete.as_deref(), Some("Foo"));
    }

    #[test]
    fn cancel_delete_clears_pending_but_keeps_selection() {
        let mut flow = WorldSelectFlow::default();
        flow.select("Foo");
        flow.request_delete();
        flow.cancel_delete();
        assert_eq!(flow.pending_delete, None);
        assert_eq!(flow.selected.as_deref(), Some("Foo"));
    }

    #[test]
    fn confirm_delete_resolved_clears_both() {
        let mut flow = WorldSelectFlow::default();
        flow.select("Foo");
        flow.request_delete();
        flow.confirm_delete_resolved();
        assert_eq!(flow.pending_delete, None);
        assert_eq!(flow.selected, None);
    }

    #[test]
    fn selecting_a_different_row_cancels_a_pending_delete() {
        let mut flow = WorldSelectFlow::default();
        flow.select("Foo");
        flow.request_delete();
        flow.select("Bar");
        assert_eq!(flow.pending_delete, None);
        assert_eq!(flow.selected.as_deref(), Some("Bar"));
    }
}

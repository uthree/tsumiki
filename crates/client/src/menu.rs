//! Title menu (design.md art direction: pop/toy-like, no pure black/white;
//! design.md §1 decoupling: the menu only knows [`MenuHooks`], never the
//! server/net crates).
//!
//! - `OnEnter(AppState::Menu)` ([`setup_menu`]) spawns a decorative camera +
//!   slowly rotating toy-block cluster + light (the "backdrop"), and the UI:
//!   a big "tsumiki" title over an underlined bar, and a panel holding either
//!   the main buttons (Singleplayer/Multiplayer/Settings/Quit), a connect
//!   form (server address + name fields, Connect/Back) once Multiplayer is
//!   picked, or the shared settings panel ([`crate::settings`]) once
//!   Settings is picked — the same panel the in-game pause menu
//!   ([`crate::pause`]) opens, so tweaking a setting looks and behaves
//!   identically from either screen.
//! - `OnExit(AppState::Menu)` ([`teardown_menu`]) despawns everything tagged
//!   [`MenuEntity`] and drops the menu-only resources.
//! - Text entry goes through [`apply_key_to_field`], a pure helper (append
//!   printable text, backspace, ignore control/unprintable input) that is
//!   unit-tested below without needing a Bevy `App`.
//! - Successfully obtaining a transport (either hook) inserts it as
//!   [`crate::net::Transport`] and transitions to [`AppState::InGame`]; a
//!   failed connect attempt shows the error in the panel and stays put.
//! - The cursor is never grabbed here (that's [`crate::camera::grab_cursor`],
//!   which only runs in [`AppState::InGame`]).

use bevy::input::ButtonState;
use bevy::input::keyboard::KeyboardInput;
use bevy::prelude::*;
use tsumiki_world::{BlockId, blocks};

use crate::net;
use crate::settings::{self, Settings};
use crate::ui;
use crate::view::Registry;
use crate::{AppState, ClientConfig, MenuHooks, UiFont};

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

/// Which panel the menu is currently showing.
#[derive(Resource, Clone, Copy, PartialEq, Eq, Debug)]
enum MenuPanel {
    Main,
    Connect,
    Settings,
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

/// A clickable menu action, attached to every button entity.
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
enum MenuButtonAction {
    Singleplayer,
    Multiplayer,
    Settings,
    Quit,
    Connect,
    /// Returns to [`MenuPanel::Main`] — shared by both the connect form and
    /// the settings panel.
    Back,
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

    let has_singleplayer = hooks.start_singleplayer.is_some();

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
/// [`MenuHooks`] itself (its `start_singleplayer` may already be consumed,
/// and neither hook is needed once in-game).
fn teardown_menu(mut commands: Commands, entities: Query<Entity, With<MenuEntity>>) {
    for entity in &entities {
        commands.entity(entity).despawn();
    }
    commands.remove_resource::<MenuUi>();
    commands.remove_resource::<MenuPanel>();
    commands.remove_resource::<FocusedField>();
    commands.remove_resource::<ConnectFields>();
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
                // `Fn`, not `FnOnce` (see `MenuHooks` docs): callable again
                // on every visit, including after a "Back to Title" round
                // trip, so the button stays available rather than
                // disappearing after first use.
                if let Some(start) = hooks.start_singleplayer.as_ref() {
                    let transport = start();
                    commands.insert_resource(net::Transport::new(transport));
                    next_state.set(AppState::InGame);
                }
            }
            MenuButtonAction::Multiplayer => {
                commands.entity(menu_ui.current_panel).despawn();
                let (new_panel, connect_fields) =
                    spawn_connect_panel(&mut commands, &config.name, &ui_font);
                commands
                    .entity(menu_ui.panel_container)
                    .add_child(new_panel);
                menu_ui.current_panel = new_panel;
                commands.insert_resource(connect_fields);
                *panel = MenuPanel::Connect;
                focus.0 = None;
            }
            MenuButtonAction::Settings => {
                commands.entity(menu_ui.current_panel).despawn();
                let new_panel = spawn_settings_panel(&mut commands, &settings, &ui_font);
                commands
                    .entity(menu_ui.panel_container)
                    .add_child(new_panel);
                menu_ui.current_panel = new_panel;
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
                let has_singleplayer = hooks.start_singleplayer.is_some();
                commands.entity(menu_ui.current_panel).despawn();
                let new_panel = spawn_main_panel(&mut commands, has_singleplayer, &ui_font);
                commands
                    .entity(menu_ui.panel_container)
                    .add_child(new_panel);
                menu_ui.current_panel = new_panel;
                commands.remove_resource::<ConnectFields>();
                *panel = MenuPanel::Main;
                focus.0 = None;
            }
        }
    }
}

/// Escape while the connect form or the settings panel is open goes back to
/// the main panel (in-game pause has its own, separate Escape handling in
/// [`crate::pause`]).
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
    let on_sub_panel = *panel == MenuPanel::Connect || *panel == MenuPanel::Settings;
    if !on_sub_panel || !keys.just_pressed(KeyCode::Escape) {
        return;
    }
    let has_singleplayer = hooks.start_singleplayer.is_some();
    commands.entity(menu_ui.current_panel).despawn();
    let new_panel = spawn_main_panel(&mut commands, has_singleplayer, &ui_font);
    commands
        .entity(menu_ui.panel_container)
        .add_child(new_panel);
    menu_ui.current_panel = new_panel;
    commands.remove_resource::<ConnectFields>();
    *panel = MenuPanel::Main;
    focus.0 = None;
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

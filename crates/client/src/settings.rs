//! Persisted, live-applied client settings and the settings panel widget.
//!
//! - [`Settings`]: mouse sensitivity, FOV, view distance, fullscreen.
//!   Persisted as pretty JSON at `settings.json` in the working directory
//!   ([`load_settings`]/[`save_settings`]); loaded once at startup, saved on
//!   every applied change (the file is tiny, so this is cheap). A missing or
//!   corrupt file falls back to defaults (corrupt: `eprintln!` first); values
//!   are clamped to their valid range on load. `#[serde(default)]` at the
//!   struct level defaults any field missing from an older `settings.json`
//!   to [`Settings::default`]'s value for it, so the format can grow new
//!   fields without breaking old saves.
//! - The pure load/parse/clamp/step logic ([`settings_from_json`],
//!   [`clamp_settings`], [`adjust_f32`], [`adjust_i32`]) is unit-tested below
//!   without touching the filesystem.
//! - [`spawn_settings_rows`]: the shared row widget (one row per setting,
//!   `[-] value [+]` or an On/Off toggle — no sliders, per design.md's
//!   pop/toy-like, chunky/pixel-y direction), reused by both
//!   [`crate::menu`] (a "Settings" panel off the title screen) and
//!   [`crate::pause`] (the in-game pause menu's settings sub-panel). Each
//!   caller wraps the rows in its own panel + Back button and owns what
//!   "Back" navigates to; this module only ever mutates [`Settings`] and
//!   applies it live, never touches navigation state.
//! - Live apply: [`apply_fullscreen`] reacts to `fov`/`fullscreen` changes
//!   here; mouse sensitivity is read directly by [`crate::camera::look`] and
//!   view distance by [`crate::net::request_chunks`]/
//!   [`crate::view::despawn_far_chunks`]; FOV is applied every frame by
//!   [`crate::camera::apply_fov`] (so a freshly spawned camera picks it up
//!   immediately, not just on change).

use std::ops::RangeInclusive;
use std::path::Path;

use bevy::prelude::*;
use bevy::window::{MonitorSelection, PrimaryWindow, WindowMode};
use serde::{Deserialize, Serialize};

use crate::UiFont;
use crate::ui;

/// Where settings are persisted, relative to the working directory.
const SETTINGS_PATH: &str = "settings.json";

pub const MOUSE_SENSITIVITY_RANGE: RangeInclusive<f32> = 0.2..=3.0;
pub const MOUSE_SENSITIVITY_STEP: f32 = 0.1;
pub const FOV_RANGE: RangeInclusive<f32> = 50.0..=110.0;
pub const FOV_STEP: f32 = 5.0;
// The GPU has plenty of headroom for a much deeper view: raised from 4..=12
// alongside `tsumiki_world::lod::MAX_LOD` going from 3 to 5 (see that
// constant's doc comment), and `net::VIEW_DISTANCE_CHUNKS`'s default from 8
// to 12 below.
pub const VIEW_DISTANCE_RANGE: RangeInclusive<i32> = 4..=24;
pub const VIEW_DISTANCE_STEP: i32 = 1;

/// Persisted, live-applied client settings. See the module docs for the
/// persistence/apply contract.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Multiplier on the base look sensitivity.
    pub mouse_sensitivity: f32,
    pub fov_degrees: f32,
    pub view_distance_chunks: i32,
    pub fullscreen: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mouse_sensitivity: 1.0,
            fov_degrees: 70.0,
            // Keeps `net::VIEW_DISTANCE_CHUNKS` as the single source of
            // truth for the default (design.md-adjacent doc note: the
            // meshed radius is effectively one less than this).
            view_distance_chunks: crate::net::VIEW_DISTANCE_CHUNKS,
            fullscreen: false,
        }
    }
}

/// Clamps every field to its valid range (see the `*_RANGE` constants).
pub fn clamp_settings(settings: &mut Settings) {
    settings.mouse_sensitivity = settings.mouse_sensitivity.clamp(
        *MOUSE_SENSITIVITY_RANGE.start(),
        *MOUSE_SENSITIVITY_RANGE.end(),
    );
    settings.fov_degrees = settings
        .fov_degrees
        .clamp(*FOV_RANGE.start(), *FOV_RANGE.end());
    settings.view_distance_chunks = settings
        .view_distance_chunks
        .clamp(*VIEW_DISTANCE_RANGE.start(), *VIEW_DISTANCE_RANGE.end());
}

/// One `[-]`/`[+]` press's new value: `value + delta`, rounded to avoid
/// float drift across repeated 0.1-sized steps, then clamped to `range`.
pub fn adjust_f32(value: f32, delta: f32, range: RangeInclusive<f32>) -> f32 {
    let stepped = ((value + delta) * 100.0).round() / 100.0;
    stepped.clamp(*range.start(), *range.end())
}

/// One `[-]`/`[+]` press's new value: `value + delta`, clamped to `range`.
pub fn adjust_i32(value: i32, delta: i32, range: RangeInclusive<i32>) -> i32 {
    (value + delta).clamp(*range.start(), *range.end())
}

/// Parses `settings.json`'s content, clamping the result. Corrupt JSON is
/// reported via `eprintln!` and falls back to [`Settings::default`]; missing
/// fields (including "the whole file is `{}`") default per-field via
/// `#[serde(default)]` on [`Settings`].
pub fn settings_from_json(text: &str) -> Settings {
    match serde_json::from_str::<Settings>(text) {
        Ok(mut settings) => {
            clamp_settings(&mut settings);
            settings
        }
        Err(err) => {
            eprintln!("settings.json is corrupt ({err}); using defaults");
            Settings::default()
        }
    }
}

fn load_settings_from_path(path: &Path) -> Settings {
    match std::fs::read_to_string(path) {
        // A missing file is the ordinary first-run case, not an error: no
        // `eprintln!`, just defaults.
        Err(_) => Settings::default(),
        Ok(text) => settings_from_json(&text),
    }
}

/// Loads settings from [`SETTINGS_PATH`], falling back to defaults if the
/// file is missing or corrupt (see [`settings_from_json`]).
pub fn load_settings() -> Settings {
    load_settings_from_path(Path::new(SETTINGS_PATH))
}

/// Persists `settings` as pretty JSON to [`SETTINGS_PATH`]. Called after
/// every applied change; failures are reported via `eprintln!` rather than
/// panicking (a stale settings file is not worth crashing the game over).
pub fn save_settings(settings: &Settings) {
    match serde_json::to_string_pretty(settings) {
        Ok(json) => {
            if let Err(err) = std::fs::write(SETTINGS_PATH, json) {
                eprintln!("failed to write {SETTINGS_PATH}: {err}");
            }
        }
        Err(err) => eprintln!("failed to serialize settings: {err}"),
    }
}

/// A settings row's `[-]`/`[+]`/toggle button, attached to the button
/// entity. [`handle_settings_actions`] is the only system that reads it.
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettingsAction {
    DecreaseMouseSensitivity,
    IncreaseMouseSensitivity,
    DecreaseFov,
    IncreaseFov,
    DecreaseViewDistance,
    IncreaseViewDistance,
    ToggleFullscreen,
}

/// Which setting a [`SettingsValueText`] entity displays.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SettingsField {
    MouseSensitivity,
    Fov,
    ViewDistance,
    Fullscreen,
}

/// Tags a row's value `Text` entity with which field it displays, so
/// [`update_settings_value_texts`] can find it without knowing the panel's
/// layout.
#[derive(Component)]
struct SettingsValueText(SettingsField);

const STEP_BUTTON_SIZE: f32 = 32.0;
const TOGGLE_BUTTON_WIDTH: f32 = 72.0;
const VALUE_WIDTH: f32 = 64.0;
const ROW_LABEL_FONT_SIZE: f32 = 16.0;
const ROW_VALUE_FONT_SIZE: f32 = 16.0;
const STEP_BUTTON_FONT_SIZE: f32 = 16.0;

const STEP_BUTTON_COLOR: Color = Color::srgb(0.45, 0.44, 0.52);
const TOGGLE_ON_COLOR: Color = Color::srgb(0.43, 0.78, 0.36);
const TOGGLE_OFF_COLOR: Color = Color::srgb(0.45, 0.44, 0.52);

fn format_sensitivity(v: f32) -> String {
    format!("{v:.1}x")
}

fn format_fov(v: f32) -> String {
    format!("{v:.0}")
}

fn format_view_distance(v: i32) -> String {
    format!("{v}")
}

fn format_toggle(on: bool) -> &'static str {
    if on { "On" } else { "Off" }
}

/// Spawns the four setting rows (Mouse sensitivity / FOV / View distance /
/// Fullscreen) as children of `parent`. The caller supplies the surrounding
/// panel and its own Back button; see the module docs.
pub fn spawn_settings_rows(
    parent: &mut ChildSpawnerCommands<'_>,
    settings: &Settings,
    font: &UiFont,
) {
    spawn_stepper_row(
        parent,
        "Mouse sensitivity",
        &format_sensitivity(settings.mouse_sensitivity),
        SettingsField::MouseSensitivity,
        SettingsAction::DecreaseMouseSensitivity,
        SettingsAction::IncreaseMouseSensitivity,
        font,
    );
    spawn_stepper_row(
        parent,
        "Field of view",
        &format_fov(settings.fov_degrees),
        SettingsField::Fov,
        SettingsAction::DecreaseFov,
        SettingsAction::IncreaseFov,
        font,
    );
    spawn_stepper_row(
        parent,
        "View distance",
        &format_view_distance(settings.view_distance_chunks),
        SettingsField::ViewDistance,
        SettingsAction::DecreaseViewDistance,
        SettingsAction::IncreaseViewDistance,
        font,
    );
    spawn_toggle_row(parent, "Fullscreen", settings.fullscreen, font);
}

fn spawn_row_label(parent: &mut ChildSpawnerCommands<'_>, label: &str, font: &UiFont) {
    parent.spawn((
        Text::new(label),
        font.text(ROW_LABEL_FONT_SIZE),
        TextColor(ui::PANEL_TEXT_COLOR),
    ));
}

#[allow(clippy::too_many_arguments)]
fn spawn_stepper_row(
    parent: &mut ChildSpawnerCommands<'_>,
    label: &str,
    value: &str,
    field: SettingsField,
    decrease: SettingsAction,
    increase: SettingsAction,
    font: &UiFont,
) {
    parent
        .spawn(Node {
            width: Val::Percent(100.0),
            flex_direction: FlexDirection::Row,
            justify_content: JustifyContent::SpaceBetween,
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|row| {
            spawn_row_label(row, label, font);
            row.spawn(Node {
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                column_gap: Val::Px(8.0),
                ..default()
            })
            .with_children(|controls| {
                spawn_step_button(controls, decrease, "-", font);
                controls
                    .spawn(Node {
                        width: Val::Px(VALUE_WIDTH),
                        justify_content: JustifyContent::Center,
                        ..default()
                    })
                    .with_children(|value_box| {
                        value_box.spawn((
                            Text::new(value),
                            font.text(ROW_VALUE_FONT_SIZE),
                            TextColor(ui::PANEL_TEXT_COLOR),
                            SettingsValueText(field),
                        ));
                    });
                spawn_step_button(controls, increase, "+", font);
            });
        });
}

fn spawn_step_button(
    parent: &mut ChildSpawnerCommands<'_>,
    action: SettingsAction,
    label: &str,
    font: &UiFont,
) {
    parent
        .spawn((
            Button,
            Node {
                width: Val::Px(STEP_BUTTON_SIZE),
                height: Val::Px(STEP_BUTTON_SIZE),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(STEP_BUTTON_COLOR),
            ui::ButtonBase(STEP_BUTTON_COLOR),
            action,
        ))
        .with_children(|button| {
            button.spawn((
                Text::new(label),
                font.text(STEP_BUTTON_FONT_SIZE),
                TextColor(ui::PANEL_TEXT_COLOR),
            ));
        });
}

fn spawn_toggle_row(parent: &mut ChildSpawnerCommands<'_>, label: &str, on: bool, font: &UiFont) {
    parent
        .spawn(Node {
            width: Val::Percent(100.0),
            flex_direction: FlexDirection::Row,
            justify_content: JustifyContent::SpaceBetween,
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|row| {
            spawn_row_label(row, label, font);
            let color = if on {
                TOGGLE_ON_COLOR
            } else {
                TOGGLE_OFF_COLOR
            };
            row.spawn((
                Button,
                Node {
                    width: Val::Px(TOGGLE_BUTTON_WIDTH),
                    height: Val::Px(STEP_BUTTON_SIZE),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    ..default()
                },
                BackgroundColor(color),
                ui::ButtonBase(color),
                SettingsAction::ToggleFullscreen,
            ))
            .with_children(|button| {
                button.spawn((
                    Text::new(format_toggle(on)),
                    font.text(STEP_BUTTON_FONT_SIZE),
                    TextColor(ui::PANEL_TEXT_COLOR),
                    SettingsValueText(SettingsField::Fullscreen),
                ));
            });
        });
}

/// Applies a `[-]`/`[+]`/toggle press to [`Settings`] and persists the
/// result. The toggle button's own resting color (on/off tint) is updated
/// here too, since it (unlike the numeric rows' buttons) encodes state.
fn handle_settings_actions(
    buttons: Query<(&Interaction, &SettingsAction), Changed<Interaction>>,
    mut toggle_buttons: Query<(&SettingsAction, &mut ui::ButtonBase, &mut BackgroundColor)>,
    mut settings: ResMut<Settings>,
) {
    let mut changed = false;
    for (interaction, action) in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        match action {
            SettingsAction::DecreaseMouseSensitivity => {
                settings.mouse_sensitivity = adjust_f32(
                    settings.mouse_sensitivity,
                    -MOUSE_SENSITIVITY_STEP,
                    MOUSE_SENSITIVITY_RANGE,
                );
            }
            SettingsAction::IncreaseMouseSensitivity => {
                settings.mouse_sensitivity = adjust_f32(
                    settings.mouse_sensitivity,
                    MOUSE_SENSITIVITY_STEP,
                    MOUSE_SENSITIVITY_RANGE,
                );
            }
            SettingsAction::DecreaseFov => {
                settings.fov_degrees = adjust_f32(settings.fov_degrees, -FOV_STEP, FOV_RANGE);
            }
            SettingsAction::IncreaseFov => {
                settings.fov_degrees = adjust_f32(settings.fov_degrees, FOV_STEP, FOV_RANGE);
            }
            SettingsAction::DecreaseViewDistance => {
                settings.view_distance_chunks = adjust_i32(
                    settings.view_distance_chunks,
                    -VIEW_DISTANCE_STEP,
                    VIEW_DISTANCE_RANGE,
                );
            }
            SettingsAction::IncreaseViewDistance => {
                settings.view_distance_chunks = adjust_i32(
                    settings.view_distance_chunks,
                    VIEW_DISTANCE_STEP,
                    VIEW_DISTANCE_RANGE,
                );
            }
            SettingsAction::ToggleFullscreen => {
                settings.fullscreen = !settings.fullscreen;
            }
        }
        changed = true;
    }
    if changed {
        save_settings(&settings);
        let on_color = if settings.fullscreen {
            TOGGLE_ON_COLOR
        } else {
            TOGGLE_OFF_COLOR
        };
        for (action, mut base, mut background) in &mut toggle_buttons {
            if *action == SettingsAction::ToggleFullscreen {
                base.0 = on_color;
                *background = BackgroundColor(on_color);
            }
        }
    }
}

/// Keeps every row's displayed value text in sync with [`Settings`],
/// whichever panel (menu or pause) it's currently shown in.
fn update_settings_value_texts(
    settings: Res<Settings>,
    mut texts: Query<(&SettingsValueText, &mut Text)>,
) {
    if !settings.is_changed() {
        return;
    }
    for (value_text, mut text) in &mut texts {
        text.0 = match value_text.0 {
            SettingsField::MouseSensitivity => format_sensitivity(settings.mouse_sensitivity),
            SettingsField::Fov => format_fov(settings.fov_degrees),
            SettingsField::ViewDistance => format_view_distance(settings.view_distance_chunks),
            SettingsField::Fullscreen => format_toggle(settings.fullscreen).to_string(),
        };
    }
}

/// Toggles the OS window between borderless-fullscreen and windowed to
/// match [`Settings::fullscreen`]. Runs unconditionally (menu or in-game).
fn apply_fullscreen(settings: Res<Settings>, mut windows: Query<&mut Window, With<PrimaryWindow>>) {
    if !settings.is_changed() {
        return;
    }
    let Ok(mut window) = windows.single_mut() else {
        return;
    };
    window.mode = if settings.fullscreen {
        WindowMode::BorderlessFullscreen(MonitorSelection::Current)
    } else {
        WindowMode::Windowed
    };
}

/// Loads [`Settings`] and wires its live-apply/UI systems into `app`.
pub fn install(app: &mut App) {
    app.insert_resource(load_settings()).add_systems(
        Update,
        (
            handle_settings_actions,
            update_settings_value_texts,
            apply_fullscreen,
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_through_json_preserves_in_range_values() {
        let settings = Settings {
            mouse_sensitivity: 1.5,
            fov_degrees: 90.0,
            view_distance_chunks: 10,
            fullscreen: true,
        };
        let json = serde_json::to_string_pretty(&settings).unwrap();
        assert_eq!(settings_from_json(&json), settings);
    }

    #[test]
    fn corrupt_json_falls_back_to_defaults() {
        assert_eq!(settings_from_json("not json at all {"), Settings::default());
    }

    #[test]
    fn missing_fields_default_individually() {
        assert_eq!(settings_from_json("{}"), Settings::default());
        let partial = settings_from_json(r#"{"fov_degrees": 100.0}"#);
        assert_eq!(partial.fov_degrees, 100.0);
        assert_eq!(
            partial.mouse_sensitivity,
            Settings::default().mouse_sensitivity
        );
        assert_eq!(
            partial.view_distance_chunks,
            Settings::default().view_distance_chunks
        );
        assert_eq!(partial.fullscreen, Settings::default().fullscreen);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let json = r#"{"mouse_sensitivity": 2.0, "future_field": "surprise"}"#;
        assert_eq!(settings_from_json(json).mouse_sensitivity, 2.0);
    }

    #[test]
    fn out_of_range_values_are_clamped_on_load() {
        let json =
            r#"{"mouse_sensitivity": 99.0, "fov_degrees": -5.0, "view_distance_chunks": 999}"#;
        let settings = settings_from_json(json);
        assert_eq!(settings.mouse_sensitivity, *MOUSE_SENSITIVITY_RANGE.end());
        assert_eq!(settings.fov_degrees, *FOV_RANGE.start());
        assert_eq!(settings.view_distance_chunks, *VIEW_DISTANCE_RANGE.end());
    }

    #[test]
    fn missing_file_defaults_without_eprintln_path() {
        // Exercises the "missing file" branch distinctly from "corrupt
        // JSON": both end at defaults, but only the latter should print.
        let dir =
            std::env::temp_dir().join(format!("tsumiki-settings-test-{}", std::process::id()));
        let path = dir.join("does-not-exist.json");
        assert_eq!(load_settings_from_path(&path), Settings::default());
    }

    #[test]
    fn adjust_f32_steps_and_clamps() {
        assert_eq!(adjust_f32(1.0, 0.1, MOUSE_SENSITIVITY_RANGE), 1.1);
        assert_eq!(adjust_f32(0.2, -0.1, MOUSE_SENSITIVITY_RANGE), 0.2);
        assert_eq!(adjust_f32(3.0, 0.1, MOUSE_SENSITIVITY_RANGE), 3.0);
    }

    #[test]
    fn adjust_f32_avoids_float_drift_across_repeated_steps() {
        let mut value = 1.0;
        for _ in 0..7 {
            value = adjust_f32(value, MOUSE_SENSITIVITY_STEP, MOUSE_SENSITIVITY_RANGE);
        }
        assert_eq!(value, 1.7);
    }

    #[test]
    fn adjust_i32_steps_and_clamps() {
        assert_eq!(adjust_i32(8, 1, VIEW_DISTANCE_RANGE), 9);
        assert_eq!(adjust_i32(4, -1, VIEW_DISTANCE_RANGE), 4);
        assert_eq!(adjust_i32(24, 1, VIEW_DISTANCE_RANGE), 24);
    }

    #[test]
    fn clamp_settings_fixes_every_out_of_range_field() {
        let mut settings = Settings {
            mouse_sensitivity: -1.0,
            fov_degrees: 500.0,
            view_distance_chunks: -3,
            fullscreen: false,
        };
        clamp_settings(&mut settings);
        assert_eq!(settings.mouse_sensitivity, *MOUSE_SENSITIVITY_RANGE.start());
        assert_eq!(settings.fov_degrees, *FOV_RANGE.end());
        assert_eq!(settings.view_distance_chunks, *VIEW_DISTANCE_RANGE.start());
    }
}

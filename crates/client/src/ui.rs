//! Small widgets shared by the title menu ([`crate::menu`]), the pause menu
//! ([`crate::pause`]) and the settings panel ([`crate::settings`]) — one
//! button style and one hover/press feedback system for all three, so they
//! read as one visual system (design.md art direction: square panels
//! (no rounded corners), block-palette buttons, Misaki bitmap font;
//! doc/assets.md §1.1: sizes multiples of 8). Also holds [`spawn_gauge`], a
//! small horizontal bar-plus-label-plus-icon widget shared by every HUD stat
//! gauge (health now, hunger from M8) so they read as one family instead of
//! separate copies of the same bar code.

use bevy::prelude::*;

use crate::UiFont;
use crate::i18n::LocalizedText;

/// Full-screen overlay backdrop, shared by every full-screen panel (design.md
/// §8: no pure black/white).
pub const OVERLAY_BG: Color = Color::srgba(0.05, 0.04, 0.08, 0.55);
/// Panel background (design.md §8: no pure black/white).
pub const PANEL_BG: Color = Color::srgba(0.14, 0.12, 0.18, 0.72);
pub const PANEL_TEXT_COLOR: Color = Color::srgb(0.95, 0.92, 0.86);
pub const PANEL_WIDTH: f32 = 360.0;
pub const BUTTON_FONT_SIZE: f32 = 24.0;
pub const BUTTON_HEIGHT: f32 = 52.0;

/// A button's resting background color, so hover/press feedback can tint
/// relative to it and restore it on release.
#[derive(Component)]
pub struct ButtonBase(pub Color);

/// Spawns a full-width, square, block-palette button carrying `action` as
/// its click-handler tag. Every panel (menu/pause/settings) uses this same
/// spawner so they all look and feel the same. Returns the button's own
/// entity (not its label child) so a caller that needs to recolor it later
/// (e.g. a Create/Play/Delete button that greys out while its precondition
/// doesn't hold) doesn't have to re-derive it from a query.
/// `label` is a translation key, updated live by [`LocalizedText`].
pub fn spawn_button<A: Component + Clone>(
    parent: &mut ChildSpawnerCommands<'_>,
    action: A,
    label: &str,
    color: Color,
    font: &UiFont,
) -> Entity {
    parent
        .spawn((
            Button,
            Node {
                width: Val::Percent(100.0),
                height: Val::Px(BUTTON_HEIGHT),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(color),
            ButtonBase(color),
            action,
        ))
        .with_children(|button| {
            button.spawn((
                Text::new(label),
                LocalizedText::new(label),
                font.text(BUTTON_FONT_SIZE),
                TextColor(PANEL_TEXT_COLOR),
            ));
        })
        .id()
}

/// Hover/press tint feedback for every [`ButtonBase`]-tagged button
/// (full-size panel buttons and the settings panel's small stepper
/// buttons alike), relative to its resting color.
pub fn update_button_visuals(
    mut buttons: Query<(&Interaction, &ButtonBase, &mut BackgroundColor), Changed<Interaction>>,
) {
    for (interaction, base, mut background) in &mut buttons {
        *background = BackgroundColor(match interaction {
            Interaction::Pressed => darken(base.0, 0.15),
            Interaction::Hovered => lighten(base.0, 0.10),
            Interaction::None => base.0,
        });
    }
}

pub fn lighten(color: Color, amount: f32) -> Color {
    let c = color.to_srgba();
    Color::srgba(
        (c.red + amount).min(1.0),
        (c.green + amount).min(1.0),
        (c.blue + amount).min(1.0),
        c.alpha,
    )
}

pub fn darken(color: Color, amount: f32) -> Color {
    let c = color.to_srgba();
    Color::srgba(
        (c.red - amount).max(0.0),
        (c.green - amount).max(0.0),
        (c.blue - amount).max(0.0),
        c.alpha,
    )
}

/// Wires the shared visual-feedback system into `app`. Runs unconditionally
/// (cheap: `Changed<Interaction>`-gated) so it applies across every state
/// that spawns [`ButtonBase`]-tagged buttons (menu, pause, settings).
pub fn install(app: &mut App) {
    app.add_systems(Update, update_button_visuals);
}

/// Spawns a full-screen, centered, semi-transparent overlay root -- the
/// common backdrop for every full-screen panel ([`crate::pause`]'s pause/
/// settings panels, [`crate::inventory`]'s screen). Callers add their own
/// panel as a child and despawn this root (which despawns the panel with
/// it) when done.
pub fn spawn_overlay_root(commands: &mut Commands) -> Entity {
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

// ---- stat gauge (bar + centered label + trailing icon) ----

/// Default width/height of a [`spawn_gauge`] bar.
pub const GAUGE_WIDTH: f32 = 160.0;
pub const GAUGE_HEIGHT: f32 = 24.0;
/// Gap between the bar and its trailing icon glyph.
pub const GAUGE_ICON_GAP: f32 = 8.0;
const GAUGE_LABEL_FONT_SIZE: f32 = 16.0;
const GAUGE_ICON_FONT_SIZE: f32 = 24.0;

/// The entities [`spawn_gauge`] creates that a caller needs to keep updating
/// live (the fill bar's `Node` and the label's `Text`), so it can tag them
/// with its own marker components right after spawning -- mirrors the
/// `Entity::PLACEHOLDER`-capture pattern used for menu form fields.
pub struct GaugeEntities {
    pub fill: Entity,
    pub label: Entity,
}

/// Sets a gauge's fill-bar width from `fraction` (clamped to `[0, 1]`) -- the
/// single place this mapping happens, so every gauge (health now, hunger
/// from M8) fills identically.
pub fn set_gauge_fill(node: &mut Node, fraction: f32) {
    node.width = Val::Percent(fraction.clamp(0.0, 1.0) * 100.0);
}

/// Spawns one HUD stat gauge: a horizontal bar (`track_color` background
/// behind a `fill_color` foreground sized to `fraction`), its value centered
/// on top as text, and a trailing icon glyph naming what the gauge measures
/// (e.g. `♥` for health) -- kept as a plain [`UiFont`] text glyph rather than
/// an image so it costs no assets. Square, per design.md's no-rounded-corners
/// direction, like every other panel in this crate.
///
/// This is the one place a stat gauge's structure is built, so a second
/// gauge (M8's hunger, right next to this one) is a second call to this
/// function instead of a second copy of the layout code. The caller updates
/// the fill/label live via the returned [`GaugeEntities`] as the underlying
/// value changes ([`set_gauge_fill`] for the bar, a direct `Text` write for
/// the label).
pub fn spawn_gauge(
    parent: &mut ChildSpawnerCommands<'_>,
    font: &UiFont,
    fraction: f32,
    label: &str,
    icon: &str,
    track_color: Color,
    fill_color: Color,
) -> GaugeEntities {
    let mut fill = Entity::PLACEHOLDER;
    let mut label_entity = Entity::PLACEHOLDER;
    parent
        .spawn(Node {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: Val::Px(GAUGE_ICON_GAP),
            ..default()
        })
        .with_children(|row| {
            row.spawn((
                Node {
                    width: Val::Px(GAUGE_WIDTH),
                    height: Val::Px(GAUGE_HEIGHT),
                    ..default()
                },
                BackgroundColor(track_color),
            ))
            .with_children(|track| {
                let mut fill_node = Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(0.0),
                    top: Val::Px(0.0),
                    bottom: Val::Px(0.0),
                    ..default()
                };
                set_gauge_fill(&mut fill_node, fraction);
                fill = track.spawn((fill_node, BackgroundColor(fill_color))).id();

                track
                    .spawn(Node {
                        position_type: PositionType::Absolute,
                        width: Val::Percent(100.0),
                        height: Val::Percent(100.0),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        ..default()
                    })
                    .with_children(|center| {
                        label_entity = center
                            .spawn((
                                Text::new(label),
                                font.text(GAUGE_LABEL_FONT_SIZE),
                                TextColor(PANEL_TEXT_COLOR),
                            ))
                            .id();
                    });
            });
            row.spawn((
                Text::new(icon),
                font.text(GAUGE_ICON_FONT_SIZE),
                TextColor(fill_color),
            ));
        });
    GaugeEntities {
        fill,
        label: label_entity,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_gauge_fill_maps_fraction_to_percent_width() {
        let mut node = Node::default();
        set_gauge_fill(&mut node, 0.5);
        assert_eq!(node.width, Val::Percent(50.0));
    }

    #[test]
    fn set_gauge_fill_clamps_out_of_range_fractions() {
        let mut over = Node::default();
        set_gauge_fill(&mut over, 1.5);
        assert_eq!(over.width, Val::Percent(100.0));

        let mut under = Node::default();
        set_gauge_fill(&mut under, -0.5);
        assert_eq!(under.width, Val::Percent(0.0));
    }
}

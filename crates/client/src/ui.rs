//! Small widgets shared by the title menu ([`crate::menu`]), the pause menu
//! ([`crate::pause`]) and the settings panel ([`crate::settings`]) — one
//! button style and one hover/press feedback system for all three, so they
//! read as one visual system (design.md art direction: rounded panel,
//! block-palette buttons, Misaki bitmap font; doc/assets.md §1.1: sizes
//! multiples of 8).

use bevy::prelude::*;

use crate::UiFont;

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

/// Spawns a full-width, rounded, block-palette button carrying `action` as
/// its click-handler tag. Every panel (menu/pause/settings) uses this same
/// spawner so they all look and feel the same.
pub fn spawn_button<A: Component + Clone>(
    parent: &mut ChildSpawnerCommands<'_>,
    action: A,
    label: &str,
    color: Color,
    font: &UiFont,
) {
    parent
        .spawn((
            Button,
            Node {
                width: Val::Percent(100.0),
                height: Val::Px(BUTTON_HEIGHT),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border_radius: BorderRadius::all(Val::Px(10.0)),
                ..default()
            },
            BackgroundColor(color),
            ButtonBase(color),
            action,
        ))
        .with_children(|button| {
            button.spawn((
                Text::new(label),
                font.text(BUTTON_FONT_SIZE),
                TextColor(PANEL_TEXT_COLOR),
            ));
        });
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

//! Health HUD (roadmap.md M4): a row of hearts above the hotbar, survival
//! only, plus an air-bubble row while submerged.
//!
//! - Hearts: 10 glyphs (doc/assets.md §1.1's Misaki font includes `♥`), 2 hp
//!   each, rounded up (no half-heart glyph) — see [`full_heart_count`].
//! - Air bubbles: 10 glyphs (`○`), counting down the 10s reserve tracked by
//!   [`crate::damage::Submersion`] — see [`filled_bubble_count`]. Shown only
//!   while actually submerged (reserve below max).

use bevy::prelude::*;

use crate::damage::{self, Submersion};
use crate::state::GameMode;
use crate::{AppState, UiFont};

const GLYPH_COUNT: usize = 10;
const HEART_FONT_SIZE: f32 = 24.0;
const BUBBLE_FONT_SIZE: f32 = 24.0;
const GLYPH_GAP: f32 = 2.0;
const ROW_GAP: f32 = 2.0;
/// Hotbar margin-bottom (24) + slot height (48) + a small gap, so this HUD
/// sits directly above the hotbar (`crate::hotbar`).
const HUD_BOTTOM_MARGIN: f32 = 24.0 + 48.0 + 10.0;

const FULL_HEART_COLOR: Color = Color::srgb(0.86, 0.27, 0.27);
const EMPTY_HEART_COLOR: Color = Color::srgb(0.32, 0.29, 0.34);
const FULL_BUBBLE_COLOR: Color = Color::srgb(0.55, 0.80, 0.95);
const EMPTY_BUBBLE_COLOR: Color = Color::srgb(0.32, 0.29, 0.34);

/// Full hearts to draw for `hp` (2 hp/heart, rounded up). Pure and
/// unit-tested.
pub fn full_heart_count(hp: u16) -> usize {
    (hp.div_ceil(2) as usize).min(GLYPH_COUNT)
}

/// Filled air bubbles to draw for `air_remaining` seconds of the reserve (1
/// bubble/second, rounded up). Pure and unit-tested.
pub fn filled_bubble_count(air_remaining: f32) -> usize {
    (air_remaining.ceil().clamp(0.0, GLYPH_COUNT as f32)) as usize
}

#[derive(Component)]
struct HealthHudRoot;
#[derive(Component)]
struct BubbleRow;
#[derive(Component)]
struct HeartGlyph(usize);
#[derive(Component)]
struct BubbleGlyph(usize);

pub fn install(app: &mut App) {
    app.add_systems(OnEnter(AppState::InGame), spawn_hud)
        .add_systems(OnExit(AppState::InGame), teardown_hud)
        .add_systems(Update, update_hud.run_if(in_state(AppState::InGame)));
}

fn spawn_hud(mut commands: Commands, font: Res<UiFont>) {
    commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                position_type: PositionType::Absolute,
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::FlexEnd,
                row_gap: Val::Px(ROW_GAP),
                padding: UiRect::bottom(Val::Px(HUD_BOTTOM_MARGIN)),
                ..default()
            },
            HealthHudRoot,
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    flex_direction: FlexDirection::Row,
                    column_gap: Val::Px(GLYPH_GAP),
                    ..default()
                },
                BubbleRow,
            ))
            .with_children(|row| {
                for i in 0..GLYPH_COUNT {
                    row.spawn((
                        Text::new("○"),
                        font.text(BUBBLE_FONT_SIZE),
                        TextColor(EMPTY_BUBBLE_COLOR),
                        BubbleGlyph(i),
                    ));
                }
            });
            root.spawn(Node {
                flex_direction: FlexDirection::Row,
                column_gap: Val::Px(GLYPH_GAP),
                ..default()
            })
            .with_children(|row| {
                for i in 0..GLYPH_COUNT {
                    row.spawn((
                        Text::new("♥"),
                        font.text(HEART_FONT_SIZE),
                        TextColor(EMPTY_HEART_COLOR),
                        HeartGlyph(i),
                    ));
                }
            });
        });
}

fn teardown_hud(mut commands: Commands, roots: Query<Entity, With<HealthHudRoot>>) {
    for entity in &roots {
        commands.entity(entity).despawn();
    }
}

#[allow(clippy::too_many_arguments)]
fn update_hud(
    mode: Res<GameMode>,
    state: Res<crate::state::GameState>,
    submersion: Res<Submersion>,
    mut roots: Query<&mut Visibility, (With<HealthHudRoot>, Without<BubbleRow>)>,
    mut bubble_rows: Query<&mut Visibility, With<BubbleRow>>,
    mut hearts: Query<(&HeartGlyph, &mut TextColor)>,
    mut bubbles: Query<(&BubbleGlyph, &mut TextColor), Without<HeartGlyph>>,
) {
    let visible = mode.is_survival();
    for mut vis in &mut roots {
        *vis = if visible {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
    if !visible {
        return;
    }

    let full = full_heart_count(state.hp);
    for (glyph, mut color) in &mut hearts {
        color.0 = if glyph.0 < full {
            FULL_HEART_COLOR
        } else {
            EMPTY_HEART_COLOR
        };
    }

    let submerged = submersion.air_remaining < damage::AIR_MAX;
    for mut vis in &mut bubble_rows {
        *vis = if submerged {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
    if submerged {
        let filled = filled_bubble_count(submersion.air_remaining);
        for (glyph, mut color) in &mut bubbles {
            color.0 = if glyph.0 < filled {
                FULL_BUBBLE_COLOR
            } else {
                EMPTY_BUBBLE_COLOR
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_health_shows_ten_hearts() {
        assert_eq!(full_heart_count(20), 10);
    }

    #[test]
    fn zero_hp_shows_no_hearts() {
        assert_eq!(full_heart_count(0), 0);
    }

    #[test]
    fn odd_hp_rounds_up_to_the_next_heart() {
        assert_eq!(full_heart_count(1), 1);
        assert_eq!(full_heart_count(3), 2);
        assert_eq!(full_heart_count(19), 10);
    }

    #[test]
    fn heart_count_never_exceeds_ten_even_if_hp_somehow_overshoots() {
        assert_eq!(full_heart_count(u16::MAX), 10);
    }

    #[test]
    fn full_air_shows_ten_bubbles() {
        assert_eq!(filled_bubble_count(10.0), 10);
    }

    #[test]
    fn zero_air_shows_no_bubbles() {
        assert_eq!(filled_bubble_count(0.0), 0);
    }

    #[test]
    fn partial_air_rounds_up_to_the_next_bubble() {
        assert_eq!(filled_bubble_count(0.4), 1);
        assert_eq!(filled_bubble_count(9.999), 10);
        assert_eq!(filled_bubble_count(3.0), 3);
    }
}

//! Survival HUD: health and hunger gauges above the hotbar, plus an
//! air-bubble row while submerged.
//!
//! - Health: [`ui::spawn_gauge`]'s shared bar-plus-label-plus-icon widget, so
//!   a hunger gauge (M8) can sit right next to this one without duplicating
//!   any layout code. The bar's fill fraction is [`health_fraction`], its
//!   centered label is [`health_label`] (always `hp/MAX_HP`, `MAX_HP` read
//!   from [`tsumiki_protocol`] rather than hardcoded), and its trailing icon
//!   is a `♥` glyph in the existing Misaki font.
//! - Air bubbles: 10 glyphs (`○`), counting down the 10s reserve tracked by
//!   [`crate::damage::Submersion`] — see [`filled_bubble_count`]. Shown only
//!   while actually submerged (reserve below max).

use bevy::prelude::*;
use tsumiki_protocol::MAX_HP;
use tsumiki_world::food::MAX_HUNGER;

use crate::damage::{self, Submersion};
use crate::state::GameMode;
use crate::{AppState, UiFont, ui};

const GLYPH_COUNT: usize = 10;
const BUBBLE_FONT_SIZE: f32 = 24.0;
const GLYPH_GAP: f32 = 2.0;
const ROW_GAP: f32 = 4.0;
/// Hotbar margin-bottom (24) + slot height (48) + a small gap, so this HUD
/// sits directly above the hotbar (`crate::hotbar`).
const HUD_BOTTOM_MARGIN: f32 = 24.0 + 48.0 + 10.0;

/// Health gauge colors (design.md §8: no pure black/white). The fill is a
/// warm coral rather than a saturated alarm-red, so a full bar still reads
/// as "health" without shouting "danger" -- the palette convention every
/// other panel in this crate follows (see `ui.rs`).
const HEALTH_FILL_COLOR: Color = Color::srgb(0.80, 0.40, 0.32);
/// Track color: a darker tint of the fill, not pure black.
const HEALTH_TRACK_COLOR: Color = Color::srgb(0.20, 0.14, 0.15);
const HEALTH_ICON: &str = "♥";
const HUNGER_FILL_COLOR: Color = Color::srgb(0.77, 0.62, 0.30);
const HUNGER_TRACK_COLOR: Color = Color::srgb(0.22, 0.18, 0.12);
const HUNGER_ICON: &str = "食";

const FULL_BUBBLE_COLOR: Color = Color::srgb(0.55, 0.80, 0.95);
const EMPTY_BUBBLE_COLOR: Color = Color::srgb(0.32, 0.29, 0.34);

/// Fraction of the health gauge that should render filled, in `[0, 1]`. Pure
/// and unit-tested.
pub fn health_fraction(hp: u16) -> f32 {
    hp.min(MAX_HP) as f32 / MAX_HP as f32
}

/// The gauge's centered label, e.g. `"14/20"`. Always derives the
/// denominator from [`MAX_HP`] rather than hardcoding it, so the label can
/// never drift out of sync with the bar. Pure and unit-tested.
pub fn health_label(hp: u16) -> String {
    format!("{}/{MAX_HP}", hp.min(MAX_HP))
}

fn hunger_fraction(hunger: u16) -> f32 {
    hunger.min(MAX_HUNGER) as f32 / MAX_HUNGER as f32
}

fn hunger_label(hunger: u16) -> String {
    format!("{}/{MAX_HUNGER}", hunger.min(MAX_HUNGER))
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
struct HealthGaugeFill;
#[derive(Component)]
struct HealthGaugeLabel;
#[derive(Component)]
struct HungerGaugeFill;
#[derive(Component)]
struct HungerGaugeLabel;
#[derive(Component)]
struct BubbleGlyph(usize);

pub fn install(app: &mut App) {
    app.add_systems(OnEnter(AppState::InGame), spawn_hud)
        .add_systems(OnExit(AppState::InGame), teardown_hud)
        .add_systems(Update, update_hud.run_if(in_state(AppState::InGame)));
}

fn spawn_hud(mut commands: Commands, font: Res<UiFont>) {
    let mut gauge_fill = Entity::PLACEHOLDER;
    let mut gauge_label = Entity::PLACEHOLDER;
    let mut hunger_fill = Entity::PLACEHOLDER;
    let mut hunger_label_entity = Entity::PLACEHOLDER;

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
                column_gap: Val::Px(24.0),
                ..default()
            })
            .with_children(|row| {
                let gauge = ui::spawn_gauge(
                    row,
                    &font,
                    health_fraction(MAX_HP),
                    &health_label(MAX_HP),
                    HEALTH_ICON,
                    HEALTH_TRACK_COLOR,
                    HEALTH_FILL_COLOR,
                );
                gauge_fill = gauge.fill;
                gauge_label = gauge.label;
                let hunger = ui::spawn_gauge(
                    row,
                    &font,
                    hunger_fraction(MAX_HUNGER),
                    &hunger_label(MAX_HUNGER),
                    HUNGER_ICON,
                    HUNGER_TRACK_COLOR,
                    HUNGER_FILL_COLOR,
                );
                hunger_fill = hunger.fill;
                hunger_label_entity = hunger.label;
            });
        });

    commands.entity(gauge_fill).insert(HealthGaugeFill);
    commands.entity(gauge_label).insert(HealthGaugeLabel);
    commands.entity(hunger_fill).insert(HungerGaugeFill);
    commands
        .entity(hunger_label_entity)
        .insert(HungerGaugeLabel);
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
    mut gauge_fills: Query<&mut Node, (With<HealthGaugeFill>, Without<HungerGaugeFill>)>,
    mut gauge_labels: Query<&mut Text, (With<HealthGaugeLabel>, Without<HungerGaugeLabel>)>,
    mut hunger_fills: Query<&mut Node, With<HungerGaugeFill>>,
    mut hunger_labels: Query<&mut Text, With<HungerGaugeLabel>>,
    mut bubbles: Query<(&BubbleGlyph, &mut TextColor)>,
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

    let fraction = health_fraction(state.hp);
    for mut node in &mut gauge_fills {
        ui::set_gauge_fill(&mut node, fraction);
    }
    let label = health_label(state.hp);
    for mut text in &mut gauge_labels {
        text.0 = label.clone();
    }
    for mut node in &mut hunger_fills {
        ui::set_gauge_fill(&mut node, hunger_fraction(state.hunger));
    }
    for mut text in &mut hunger_labels {
        text.0 = hunger_label(state.hunger);
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
    fn survival_hud_renders_both_snapshots_and_hides_in_creative() {
        let mut app = App::new();
        app.insert_resource(UiFont(Handle::default()))
            .insert_resource(GameMode(tsumiki_protocol::GameMode::Survival))
            .insert_resource(crate::state::GameState {
                hp: 14,
                hunger: 7,
                ..default()
            })
            .init_resource::<Submersion>()
            .add_systems(Startup, spawn_hud)
            .add_systems(Update, update_hud);
        app.update();
        let world = app.world_mut();
        assert_eq!(
            world
                .query_filtered::<&Text, With<HealthGaugeLabel>>()
                .single(world)
                .unwrap()
                .0,
            "14/20"
        );
        assert_eq!(
            world
                .query_filtered::<&Text, With<HungerGaugeLabel>>()
                .single(world)
                .unwrap()
                .0,
            "7/20"
        );
        assert_eq!(
            world
                .query_filtered::<&Visibility, With<HealthHudRoot>>()
                .single(world)
                .unwrap(),
            &Visibility::Inherited
        );
        world.resource_mut::<GameMode>().0 = tsumiki_protocol::GameMode::Creative;
        app.update();
        let world = app.world_mut();
        assert_eq!(
            world
                .query_filtered::<&Visibility, With<HealthHudRoot>>()
                .single(world)
                .unwrap(),
            &Visibility::Hidden
        );
    }

    #[test]
    fn hunger_gauge_tracks_partial_empty_full_and_invalid_snapshots() {
        for (value, fraction, label) in [
            (0, 0.0, "0/20"),
            (7, 0.35, "7/20"),
            (MAX_HUNGER, 1.0, "20/20"),
            (u16::MAX, 1.0, "20/20"),
        ] {
            assert_eq!(hunger_fraction(value), fraction);
            assert_eq!(hunger_label(value), label);
        }
    }

    #[test]
    fn zero_hp_is_an_empty_gauge() {
        assert_eq!(health_fraction(0), 0.0);
        assert_eq!(health_label(0), "0/20");
    }

    #[test]
    fn max_hp_is_a_full_gauge() {
        assert_eq!(health_fraction(MAX_HP), 1.0);
        assert_eq!(health_label(MAX_HP), "20/20");
    }

    #[test]
    fn partial_hp_is_a_proportional_fraction() {
        assert_eq!(health_fraction(10), 0.5);
        assert_eq!(health_label(10), "10/20");
    }

    #[test]
    fn odd_hp_produces_an_exact_fraction_not_rounded() {
        assert_eq!(health_fraction(1), 1.0 / MAX_HP as f32);
        assert_eq!(health_label(1), "1/20");
    }

    #[test]
    fn hp_beyond_max_is_clamped() {
        assert_eq!(health_fraction(u16::MAX), 1.0);
        assert_eq!(health_label(u16::MAX), format!("{MAX_HP}/{MAX_HP}"));
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

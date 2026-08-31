//! Hotbar UI and block selection.
//!
//! - [`Hotbar`] resource holds the placeable block list (every solid
//!   prototype block plus water) and the selected index.
//! - Selection via number keys `1..=7` and the mouse wheel (wraps around).
//! - A bottom-center Bevy UI row of slots, each tinted with its block's top
//!   color, with a white border on the selected slot.
//! - Survival (roadmap.md M4): each slot additionally shows its inventory
//!   count (bottom-right, hidden at 0) and dims when empty — see
//!   [`count_label`]/[`is_dimmed`]. Creative shows neither, unchanged from
//!   before M4.

use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;
use tsumiki_world::{BlockId, blocks};

use crate::pause;
use crate::state;
use crate::ui;
use crate::view;
use crate::{AppState, UiFont};

/// Placeable blocks, in hotbar order: every solid prototype block plus
/// water.
pub const PLACEABLE_BLOCKS: [BlockId; 7] = [
    blocks::STONE,
    blocks::DIRT,
    blocks::GRASS,
    blocks::SAND,
    blocks::WATER,
    blocks::LOG,
    blocks::LEAVES,
];

const SLOT_SIZE_PX: f32 = 48.0;
const SLOT_GAP_PX: f32 = 6.0;
const SLOT_BORDER_PX: f32 = 3.0;
const SELECTED_BORDER: Color = Color::WHITE;
const UNSELECTED_BORDER: Color = Color::srgba(0.0, 0.0, 0.0, 0.35);
const COUNT_FONT_SIZE: f32 = 16.0;
const COUNT_TEXT_COLOR: Color = Color::WHITE;

/// Text to show for a slot's inventory count: hidden in creative (no
/// scarcity to track) and hidden at a zero count in survival. Pure and
/// unit-tested.
pub fn count_label(mode: tsumiki_protocol::GameMode, count: u32) -> Option<String> {
    if mode == tsumiki_protocol::GameMode::Creative || count == 0 {
        None
    } else {
        Some(count.to_string())
    }
}

/// Whether a slot should render dimmed/desaturated: survival with an empty
/// count. Pure and unit-tested.
pub fn is_dimmed(mode: tsumiki_protocol::GameMode, count: u32) -> bool {
    mode == tsumiki_protocol::GameMode::Survival && count == 0
}

/// Desaturates and slightly darkens `color`, used for empty survival slots.
fn dim_color(color: Color) -> Color {
    let gray = Color::srgb(0.5, 0.5, 0.5);
    ui::darken(color.mix(&gray, 0.6), 0.12)
}

/// The currently selected hotbar slot.
#[derive(Resource, Default)]
pub struct Hotbar {
    pub selected: usize,
}

impl Hotbar {
    pub fn selected_block(&self) -> BlockId {
        PLACEABLE_BLOCKS[self.selected]
    }
}

/// Marks a spawned hotbar slot UI node with its index into
/// [`PLACEABLE_BLOCKS`].
#[derive(Component)]
struct HotbarSlot(usize);

/// Marks a slot's inventory-count text node with its index into
/// [`PLACEABLE_BLOCKS`].
#[derive(Component)]
struct HotbarCountText(usize);

/// Tags the hotbar UI's root node so `OnExit(AppState::InGame)` can despawn
/// it (see `pause` module docs).
#[derive(Component)]
struct HotbarRoot;

/// Wires the hotbar resource, input, UI and highlight systems into `app`.
pub fn install(app: &mut App) {
    app.init_resource::<Hotbar>()
        .add_systems(OnEnter(AppState::InGame), spawn_hotbar_ui)
        .add_systems(OnExit(AppState::InGame), teardown_hotbar_ui)
        .add_systems(
            Update,
            (
                handle_selection
                    .run_if(pause::is_playing)
                    .run_if(state::is_alive),
                update_selection_highlight,
                update_hotbar_counts,
            )
                .chain()
                .run_if(in_state(AppState::InGame)),
        );
}

fn teardown_hotbar_ui(mut commands: Commands, roots: Query<Entity, With<HotbarRoot>>) {
    for entity in &roots {
        commands.entity(entity).despawn();
    }
}

fn spawn_hotbar_ui(mut commands: Commands, registry: Res<view::Registry>, font: Res<UiFont>) {
    commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                position_type: PositionType::Absolute,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::FlexEnd,
                ..default()
            },
            HotbarRoot,
        ))
        .with_children(|root| {
            root.spawn(Node {
                flex_direction: FlexDirection::Row,
                margin: UiRect::bottom(Val::Px(24.0)),
                column_gap: Val::Px(SLOT_GAP_PX),
                ..default()
            })
            .with_children(|row| {
                for (i, &block) in PLACEABLE_BLOCKS.iter().enumerate() {
                    let def = registry.0.get(block);
                    let color =
                        Color::srgb_u8(def.color_top[0], def.color_top[1], def.color_top[2]);
                    row.spawn((
                        Node {
                            width: Val::Px(SLOT_SIZE_PX),
                            height: Val::Px(SLOT_SIZE_PX),
                            border: UiRect::all(Val::Px(SLOT_BORDER_PX)),
                            ..default()
                        },
                        BackgroundColor(color),
                        BorderColor::all(if i == 0 {
                            SELECTED_BORDER
                        } else {
                            UNSELECTED_BORDER
                        }),
                        HotbarSlot(i),
                    ))
                    .with_children(|slot| {
                        slot.spawn((
                            Node {
                                position_type: PositionType::Absolute,
                                right: Val::Px(2.0),
                                bottom: Val::Px(0.0),
                                ..default()
                            },
                            Text::new(""),
                            font.text(COUNT_FONT_SIZE),
                            TextColor(COUNT_TEXT_COLOR),
                            HotbarCountText(i),
                        ));
                    });
                }
            });
        });
}

/// Updates every slot's count text and dimming from [`state::GameState`]/
/// [`state::GameMode`]. Runs unconditionally each frame (cheap: a handful of
/// slots) rather than change-gated, since `GameState` changes continuously
/// anyway (time of day advances every frame).
fn update_hotbar_counts(
    mode: Res<state::GameMode>,
    game_state: Res<state::GameState>,
    registry: Res<view::Registry>,
    mut counts: Query<(&HotbarCountText, &mut Text)>,
    mut slots: Query<(&HotbarSlot, &mut BackgroundColor)>,
) {
    for (tag, mut text) in &mut counts {
        let block = PLACEABLE_BLOCKS[tag.0];
        let count = game_state.inventory_count(block);
        text.0 = count_label(mode.0, count).unwrap_or_default();
    }
    for (slot, mut bg) in &mut slots {
        let block = PLACEABLE_BLOCKS[slot.0];
        let count = game_state.inventory_count(block);
        let def = registry.0.get(block);
        let base = Color::srgb_u8(def.color_top[0], def.color_top[1], def.color_top[2]);
        *bg = BackgroundColor(if is_dimmed(mode.0, count) {
            dim_color(base)
        } else {
            base
        });
    }
}

/// Number keys `1..=7` select a slot directly; the mouse wheel steps through
/// slots, wrapping around at either end.
fn handle_selection(
    keys: Res<ButtonInput<KeyCode>>,
    mut wheel: MessageReader<MouseWheel>,
    mut hotbar: ResMut<Hotbar>,
) {
    const DIGIT_KEYS: [KeyCode; 7] = [
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
    ];
    for (i, key) in DIGIT_KEYS.iter().enumerate() {
        if keys.just_pressed(*key) {
            hotbar.selected = i;
        }
    }

    let scroll: f32 = wheel.read().map(|ev| ev.y).sum();
    let len = PLACEABLE_BLOCKS.len();
    if scroll > 0.0 {
        hotbar.selected = (hotbar.selected + len - 1) % len;
    } else if scroll < 0.0 {
        hotbar.selected = (hotbar.selected + 1) % len;
    }
}

fn update_selection_highlight(
    hotbar: Res<Hotbar>,
    mut slots: Query<(&HotbarSlot, &mut BorderColor)>,
) {
    if !hotbar.is_changed() {
        return;
    }
    for (slot, mut border) in &mut slots {
        *border = BorderColor::all(if slot.0 == hotbar.selected {
            SELECTED_BORDER
        } else {
            UNSELECTED_BORDER
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creative_never_shows_a_count() {
        assert_eq!(count_label(tsumiki_protocol::GameMode::Creative, 5), None);
        assert_eq!(count_label(tsumiki_protocol::GameMode::Creative, 0), None);
    }

    #[test]
    fn survival_hides_a_zero_count() {
        assert_eq!(count_label(tsumiki_protocol::GameMode::Survival, 0), None);
    }

    #[test]
    fn survival_shows_a_nonzero_count() {
        assert_eq!(
            count_label(tsumiki_protocol::GameMode::Survival, 3),
            Some("3".to_string())
        );
    }

    #[test]
    fn only_survival_with_zero_count_is_dimmed() {
        assert!(is_dimmed(tsumiki_protocol::GameMode::Survival, 0));
        assert!(!is_dimmed(tsumiki_protocol::GameMode::Survival, 1));
        assert!(!is_dimmed(tsumiki_protocol::GameMode::Creative, 0));
    }
}

//! Hotbar UI and slot selection (roadmap M5 rework: item-backed, not a fixed
//! block palette).
//!
//! - [`Hotbar`] resource holds only the selected slot index
//!   (`0..HOTBAR_SIZE`); slot contents come from
//!   [`state::GameState::main`]`[0..HOTBAR_SIZE]`, the same server snapshot
//!   [`crate::inventory`] renders the rest of.
//! - Selection via number keys `1..=9` and the mouse wheel (wraps around).
//! - A bottom-center Bevy UI row of slots, each tinted with the held item's
//!   placeholder color (or left neutral when empty) and showing its count
//!   (hidden at 1, roadmap M5's "only when count > 1" convention -- see
//!   [`crate::inventory::slot_visual`], shared with the inventory screen so
//!   both read identically), with a white border on the selected slot.

use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;
use tsumiki_world::{HOTBAR_SIZE, ItemStack};

use crate::inventory::slot_visual;
use crate::pause;
use crate::state;
use crate::{AppState, UiFont};

const SLOT_SIZE_PX: f32 = 48.0;
const SLOT_GAP_PX: f32 = 6.0;
const SLOT_BORDER_PX: f32 = 3.0;
const SELECTED_BORDER: Color = Color::WHITE;
const UNSELECTED_BORDER: Color = Color::srgba(0.0, 0.0, 0.0, 0.35);
const COUNT_FONT_SIZE: f32 = 16.0;
const COUNT_TEXT_COLOR: Color = Color::WHITE;

/// The currently selected hotbar slot, an index into
/// [`state::GameState::main`]'s first [`HOTBAR_SIZE`] entries.
#[derive(Resource, Default)]
pub struct Hotbar {
    pub selected: usize,
}

impl Hotbar {
    /// The item stack currently selected, if any. Pure and unit-tested; used
    /// by [`crate::interact`] to decide what a right-click places.
    pub fn selected_stack(&self, main: &[Option<ItemStack>]) -> Option<ItemStack> {
        main.get(self.selected).copied().flatten()
    }
}

/// Marks a spawned hotbar slot UI node with its index into `main`
/// (`0..HOTBAR_SIZE`).
#[derive(Component)]
struct HotbarSlot(usize);

/// Marks a slot's count text node with the same index.
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
                update_hotbar_slots,
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

fn spawn_hotbar_ui(mut commands: Commands, font: Res<UiFont>) {
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
                for i in 0..HOTBAR_SIZE {
                    row.spawn((
                        Node {
                            width: Val::Px(SLOT_SIZE_PX),
                            height: Val::Px(SLOT_SIZE_PX),
                            border: UiRect::all(Val::Px(SLOT_BORDER_PX)),
                            border_radius: BorderRadius::all(Val::Px(6.0)),
                            ..default()
                        },
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

/// Updates every slot's background color and count text from
/// [`state::GameState::main`]. Runs unconditionally each frame (cheap: a
/// handful of slots) rather than change-gated, since `GameState` changes
/// continuously anyway (time of day advances every frame).
fn update_hotbar_slots(
    game_state: Res<state::GameState>,
    item_reg: Res<state::ItemReg>,
    mut counts: Query<(&HotbarCountText, &mut Text)>,
    mut slots: Query<(&HotbarSlot, &mut BackgroundColor)>,
) {
    for (tag, mut text) in &mut counts {
        let stack = game_state.main.get(tag.0).copied().flatten();
        text.0 = slot_visual(stack, &item_reg.0).count_text;
    }
    for (slot, mut bg) in &mut slots {
        let stack = game_state.main.get(slot.0).copied().flatten();
        *bg = BackgroundColor(slot_visual(stack, &item_reg.0).color);
    }
}

/// Number keys `1..=9` select a slot directly; the mouse wheel steps through
/// slots, wrapping around at either end.
fn handle_selection(
    keys: Res<ButtonInput<KeyCode>>,
    mut wheel: MessageReader<MouseWheel>,
    mut hotbar: ResMut<Hotbar>,
) {
    const DIGIT_KEYS: [KeyCode; HOTBAR_SIZE] = [
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
        KeyCode::Digit8,
        KeyCode::Digit9,
    ];
    for (i, key) in DIGIT_KEYS.iter().enumerate() {
        if keys.just_pressed(*key) {
            hotbar.selected = i;
        }
    }

    let scroll: f32 = wheel.read().map(|ev| ev.y).sum();
    if scroll > 0.0 {
        hotbar.selected = (hotbar.selected + HOTBAR_SIZE - 1) % HOTBAR_SIZE;
    } else if scroll < 0.0 {
        hotbar.selected = (hotbar.selected + 1) % HOTBAR_SIZE;
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
    use tsumiki_world::{ItemRegistry, items};

    #[test]
    fn selected_stack_reads_the_chosen_hotbar_index() {
        let hotbar = Hotbar { selected: 2 };
        let main = vec![None, None, Some(ItemStack::new(items::STICK, 3)), None];
        assert_eq!(
            hotbar.selected_stack(&main),
            Some(ItemStack::new(items::STICK, 3))
        );
    }

    #[test]
    fn selected_stack_is_none_past_the_end_of_main() {
        let hotbar = Hotbar { selected: 8 };
        assert_eq!(hotbar.selected_stack(&[]), None);
    }

    #[test]
    fn selected_stack_is_none_for_an_empty_slot() {
        let hotbar = Hotbar { selected: 0 };
        assert_eq!(hotbar.selected_stack(&[None]), None);
    }

    #[test]
    fn hotbar_slots_use_the_shared_slot_visual_rendering() {
        let reg = ItemRegistry::prototype();
        let visual = slot_visual(Some(ItemStack::one(items::LOG)), &reg);
        assert_eq!(visual.color, Color::srgb_u8(140, 106, 70));
        assert_eq!(visual.count_text, "", "a lone item shows no count");

        let stacked = slot_visual(Some(ItemStack::new(items::LOG, 12)), &reg);
        assert_eq!(stacked.count_text, "12");
    }
}

//! Hotbar UI and slot selection (roadmap M5 rework: item-backed, not a fixed
//! block palette).
//!
//! - [`Hotbar`] resource holds only the selected slot index
//!   (`0..HOTBAR_SIZE`); slot contents come from
//!   [`state::GameState::main`]`[0..HOTBAR_SIZE]`, the same server snapshot
//!   [`crate::inventory`] renders the rest of.
//! - Selection via number keys `1..=9` and the mouse wheel (wraps around).
//! - A bottom-center Bevy UI row of framed pixel-art icons and their counts
//!   (hidden at 1, roadmap M5's "only when count > 1" convention -- see
//!   [`crate::inventory::slot_visual`], shared with the inventory screen so
//!   both read identically), with a white border on the selected slot. A
//!   tool with wear on it also gets a thin durability bar along the slot's
//!   bottom edge (roadmap M6, [`crate::inventory::SlotVisual::wear`]).

use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;
use tsumiki_world::{HOTBAR_SIZE, ItemStack};

use crate::i18n::item_name;
use crate::inventory::{WEAR_BAR_COLOR, WEAR_BAR_HEIGHT_PX, slot_visual};
use crate::item_icons::{self, ItemIcons};
use crate::pause;
use crate::settings::Settings;
use crate::state;
use crate::{AppState, UiFont, ui};

const SLOT_SIZE_PX: f32 = 48.0;
const SLOT_GAP_PX: f32 = 6.0;
const SLOT_BORDER_PX: f32 = 3.0;
const ICON_SIZE_PX: f32 = 32.0;
const SLOT_BACKGROUND: Color = Color::srgba(0.14, 0.12, 0.18, 0.78);
const SELECTED_BORDER: Color = Color::WHITE;
const UNSELECTED_BORDER: Color = Color::srgba(0.0, 0.0, 0.0, 0.35);
const COUNT_FONT_SIZE: f32 = 16.0;
const COUNT_TEXT_COLOR: Color = Color::WHITE;
const ITEM_NAME_SECONDS: f32 = 2.5;
const ITEM_NAME_FADE_SECONDS: f32 = 0.5;

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
#[derive(Component)]
struct HotbarIcon(usize);

/// Marks a slot's durability wear-bar node with the same index (roadmap M6;
/// see [`crate::inventory::SlotVisual::wear`]).
#[derive(Component)]
struct HotbarWearBar(usize);

/// Tags the hotbar UI's root node so `OnExit(AppState::InGame)` can despawn
/// it (see `pause` module docs).
#[derive(Component)]
struct HotbarRoot;

#[derive(Component)]
struct SelectedItemName;

#[derive(Resource, Default)]
struct SelectedItemNotice {
    selection: Option<(usize, tsumiki_world::ItemId)>,
    remaining: f32,
}

/// Wires the hotbar resource, input, UI and highlight systems into `app`.
pub fn install(app: &mut App) {
    app.init_resource::<Hotbar>()
        .init_resource::<SelectedItemNotice>()
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
                update_selected_item_name,
            )
                .chain()
                .run_if(in_state(AppState::InGame)),
        );
}

fn teardown_hotbar_ui(
    mut commands: Commands,
    roots: Query<Entity, With<HotbarRoot>>,
    mut notice: ResMut<SelectedItemNotice>,
) {
    for entity in &roots {
        commands.entity(entity).despawn();
    }
    *notice = SelectedItemNotice::default();
}

fn spawn_hotbar_ui(mut commands: Commands, font: Res<UiFont>, icons: Res<ItemIcons>) {
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                bottom: Val::Px(148.0),
                justify_content: JustifyContent::Center,
                ..default()
            },
            Pickable::IGNORE,
            HotbarRoot,
        ))
        .with_children(|parent| {
            parent.spawn((
                Node {
                    padding: UiRect::axes(Val::Px(10.0), Val::Px(4.0)),
                    ..default()
                },
                Text::new(""),
                font.text(24.0),
                TextColor(ui::PANEL_TEXT_COLOR),
                BackgroundColor(SLOT_BACKGROUND),
                Visibility::Hidden,
                Pickable::IGNORE,
                SelectedItemName,
            ));
        });
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
                            ..default()
                        },
                        BorderColor::all(if i == 0 {
                            SELECTED_BORDER
                        } else {
                            UNSELECTED_BORDER
                        }),
                        BackgroundColor(SLOT_BACKGROUND),
                        HotbarSlot(i),
                    ))
                    .with_children(|slot| {
                        slot.spawn((
                            Node {
                                position_type: PositionType::Absolute,
                                left: Val::Px(
                                    (SLOT_SIZE_PX - SLOT_BORDER_PX * 2.0 - ICON_SIZE_PX) / 2.0,
                                ),
                                top: Val::Px(
                                    (SLOT_SIZE_PX - SLOT_BORDER_PX * 2.0 - ICON_SIZE_PX) / 2.0,
                                ),
                                width: Val::Px(ICON_SIZE_PX),
                                height: Val::Px(ICON_SIZE_PX),
                                ..default()
                            },
                            icons.node(tsumiki_world::ItemId(0)),
                            HotbarIcon(i),
                        ));
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
                        slot.spawn((
                            Node {
                                position_type: PositionType::Absolute,
                                left: Val::Px(0.0),
                                bottom: Val::Px(0.0),
                                width: Val::Percent(0.0),
                                height: Val::Px(WEAR_BAR_HEIGHT_PX),
                                ..default()
                            },
                            BackgroundColor(WEAR_BAR_COLOR),
                            Visibility::Hidden,
                            HotbarWearBar(i),
                        ));
                    });
                }
            });
        });
}

/// Count and durability updates keep the current timer; a different slot or
/// item starts it again, including when an empty selected slot is filled.
#[allow(clippy::too_many_arguments)]
fn update_selected_item_name(
    time: Res<Time>,
    hotbar: Res<Hotbar>,
    game_state: Res<state::GameState>,
    registry: Res<state::ItemReg>,
    settings: Res<Settings>,
    pause: Res<State<pause::PauseState>>,
    mut notice: ResMut<SelectedItemNotice>,
    mut labels: Query<
        (
            &mut Text,
            &mut TextColor,
            &mut BackgroundColor,
            &mut Visibility,
        ),
        With<SelectedItemName>,
    >,
) {
    let selection = hotbar
        .selected_stack(&game_state.main)
        .filter(|_| !game_state.dead)
        .map(|stack| (hotbar.selected, stack.item));
    let playing = *pause.get() == pause::PauseState::Playing;
    if notice.selection != selection {
        notice.selection = selection;
        notice.remaining = if selection.is_some() {
            ITEM_NAME_SECONDS
        } else {
            0.0
        };
    } else if playing {
        notice.remaining = (notice.remaining - time.delta_secs()).max(0.0);
    }
    let alpha = (notice.remaining / ITEM_NAME_FADE_SECONDS).clamp(0.0, 1.0);
    for (mut text, mut color, mut background, mut visibility) in &mut labels {
        *visibility = if playing && selection.is_some() && alpha > 0.0 {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
        let label = selection.map_or_else(String::new, |(_, item)| {
            item_name(settings.language, registry.0.get(item).name)
        });
        if text.0 != label {
            text.0 = label;
        }
        color.0 = ui::PANEL_TEXT_COLOR.with_alpha(alpha);
        background.0 = SLOT_BACKGROUND.with_alpha(0.78 * alpha);
    }
}

/// Updates every slot's icon, count text, and wear bar from
/// [`state::GameState::main`]. Runs unconditionally each frame (cheap: a
/// handful of slots) rather than change-gated, since `GameState` changes
/// continuously anyway (time of day advances every frame).
fn update_hotbar_slots(
    game_state: Res<state::GameState>,
    item_reg: Res<state::ItemReg>,
    mut counts: Query<(&HotbarCountText, &mut Text)>,
    mut icons: Query<(&HotbarIcon, &mut ImageNode)>,
    mut wear_bars: Query<(&HotbarWearBar, &mut Node, &mut Visibility)>,
) {
    for (tag, mut text) in &mut counts {
        let stack = game_state.main.get(tag.0).copied().flatten();
        text.0 = slot_visual(stack, &item_reg.0).count_text;
    }
    for (slot, mut image) in &mut icons {
        let stack = game_state.main.get(slot.0).copied().flatten();
        image.rect = Some(item_icons::rect(
            stack.map_or(tsumiki_world::ItemId(0), |s| s.item),
        ));
    }
    for (tag, mut node, mut vis) in &mut wear_bars {
        let stack = game_state.main.get(tag.0).copied().flatten();
        match slot_visual(stack, &item_reg.0).wear {
            Some(fraction) => {
                ui::set_gauge_fill(&mut node, fraction);
                *vis = Visibility::Inherited;
            }
            None => *vis = Visibility::Hidden,
        }
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
    use crate::i18n::Language;
    use std::time::Duration;
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
        assert_eq!(visual.color, slot_visual(None, &reg).color);
        assert_eq!(visual.count_text, "", "a lone item shows no count");

        let stacked = slot_visual(Some(ItemStack::new(items::LOG, 12)), &reg);
        assert_eq!(stacked.count_text, "12");
    }

    fn notice_app() -> (App, Entity) {
        let mut app = App::new();
        let mut game_state = state::GameState::default();
        game_state.main[0] = Some(ItemStack::one(items::STONE));
        game_state.main[1] = Some(ItemStack::one(items::STONE));
        game_state.main[2] = Some(ItemStack::one(items::DIRT));
        app.init_resource::<Time>()
            .init_resource::<Hotbar>()
            .init_resource::<SelectedItemNotice>()
            .init_resource::<Settings>()
            .insert_resource(State::new(pause::PauseState::Playing))
            .insert_resource(game_state)
            .insert_resource(state::ItemReg(ItemRegistry::prototype()))
            .add_systems(Update, update_selected_item_name);
        let label = app
            .world_mut()
            .spawn((
                Text::new(""),
                TextColor(Color::WHITE),
                BackgroundColor(SLOT_BACKGROUND),
                Visibility::Hidden,
                SelectedItemName,
            ))
            .id();
        (app, label)
    }

    fn advance_notice(app: &mut App, seconds: f32) {
        app.world_mut()
            .resource_mut::<Time>()
            .advance_by(Duration::from_secs_f32(seconds));
        app.update();
    }

    #[test]
    fn notice_refreshes_on_slot_or_item_changes_but_not_stack_count_or_wear() {
        let (mut app, label) = notice_app();
        app.update();
        assert_eq!(app.world().get::<Text>(label).unwrap().0, "Stone");
        assert_eq!(
            *app.world().get::<Visibility>(label).unwrap(),
            Visibility::Inherited
        );
        advance_notice(&mut app, 2.25);
        assert!((app.world().get::<TextColor>(label).unwrap().0.alpha() - 0.5).abs() < 0.001);
        app.world_mut().resource_mut::<state::GameState>().main[0] =
            Some(ItemStack::new(items::STONE, 2));
        advance_notice(&mut app, 0.25);
        assert_eq!(
            *app.world().get::<Visibility>(label).unwrap(),
            Visibility::Hidden
        );
        app.world_mut().resource_mut::<Hotbar>().selected = 1;
        advance_notice(&mut app, 0.1);
        assert_eq!(
            app.world().resource::<SelectedItemNotice>().remaining,
            ITEM_NAME_SECONDS
        );
        app.world_mut().resource_mut::<state::GameState>().main[1] =
            Some(ItemStack::one(items::WOODEN_PICKAXE));
        advance_notice(&mut app, 0.1);
        assert_eq!(app.world().get::<Text>(label).unwrap().0, "Wooden Pickaxe");
        app.world_mut().resource_mut::<state::GameState>().main[1]
            .as_mut()
            .unwrap()
            .damage = 1;
        advance_notice(&mut app, 0.1);
        assert!(app.world().resource::<SelectedItemNotice>().remaining < ITEM_NAME_SECONDS);
    }

    #[test]
    fn notice_localizes_live_hides_for_empty_or_dead_and_preserves_time_in_menus() {
        let (mut app, label) = notice_app();
        app.update();
        app.world_mut().resource_mut::<Settings>().language = Language::Japanese;
        advance_notice(&mut app, 0.25);
        assert_eq!(app.world().get::<Text>(label).unwrap().0, "石");
        app.insert_resource(State::new(pause::PauseState::Inventory));
        let remaining = app.world().resource::<SelectedItemNotice>().remaining;
        advance_notice(&mut app, 3.0);
        assert_eq!(
            app.world().resource::<SelectedItemNotice>().remaining,
            remaining
        );
        assert_eq!(
            *app.world().get::<Visibility>(label).unwrap(),
            Visibility::Hidden
        );
        app.insert_resource(State::new(pause::PauseState::Playing));
        advance_notice(&mut app, 0.1);
        assert_eq!(
            *app.world().get::<Visibility>(label).unwrap(),
            Visibility::Inherited
        );
        app.world_mut().resource_mut::<Hotbar>().selected = 8;
        advance_notice(&mut app, 0.1);
        assert_eq!(app.world().get::<Text>(label).unwrap().0, "");
        assert_eq!(
            *app.world().get::<Visibility>(label).unwrap(),
            Visibility::Hidden
        );
        app.world_mut().resource_mut::<state::GameState>().main[8] =
            Some(ItemStack::one(items::STONE));
        advance_notice(&mut app, 0.1);
        assert_eq!(
            *app.world().get::<Visibility>(label).unwrap(),
            Visibility::Inherited
        );
        app.world_mut().resource_mut::<state::GameState>().dead = true;
        advance_notice(&mut app, 0.1);
        assert_eq!(
            *app.world().get::<Visibility>(label).unwrap(),
            Visibility::Hidden
        );
    }

    #[test]
    fn keyboard_and_wheel_selection_drive_the_notice() {
        use bevy::input::mouse::MouseScrollUnit;
        let (mut app, label) = notice_app();
        app.init_resource::<ButtonInput<KeyCode>>()
            .add_message::<MouseWheel>()
            .add_systems(Update, handle_selection.before(update_selected_item_name));
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Digit3);
        app.update();
        assert_eq!(app.world().get::<Text>(label).unwrap().0, "Dirt");
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .clear();
        app.world_mut().write_message(MouseWheel {
            unit: MouseScrollUnit::Line,
            x: 0.0,
            y: 1.0,
            window: Entity::PLACEHOLDER,
            phase: bevy::input::touch::TouchPhase::Moved,
        });
        app.update();
        assert_eq!(app.world().get::<Text>(label).unwrap().0, "Stone");
        assert_eq!(app.world().resource::<Hotbar>().selected, 1);
    }
}

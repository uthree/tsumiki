//! Hotbar UI and block selection.
//!
//! - [`Hotbar`] resource holds the placeable block list (every solid
//!   prototype block plus water) and the selected index.
//! - Selection via number keys `1..=7` and the mouse wheel (wraps around).
//! - A bottom-center Bevy UI row of slots, each tinted with its block's top
//!   color, with a white border on the selected slot.

use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;
use tsumiki_world::{BlockId, blocks};

use crate::AppState;
use crate::view;

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

/// Wires the hotbar resource, input, UI and highlight systems into `app`.
pub fn install(app: &mut App) {
    app.init_resource::<Hotbar>()
        .add_systems(OnEnter(AppState::InGame), spawn_hotbar_ui)
        .add_systems(
            Update,
            (handle_selection, update_selection_highlight)
                .chain()
                .run_if(in_state(AppState::InGame)),
        );
}

fn spawn_hotbar_ui(mut commands: Commands, registry: Res<view::Registry>) {
    commands
        .spawn(Node {
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            position_type: PositionType::Absolute,
            justify_content: JustifyContent::Center,
            align_items: AlignItems::FlexEnd,
            ..default()
        })
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
                    ));
                }
            });
        });
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

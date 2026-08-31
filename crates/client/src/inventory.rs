//! The inventory screen (roadmap M5, design.md §7): backpack + hotbar, the
//! crafting grid (2x2, 3x3 while a crafting table is open) and its output
//! slot, and a chest's slots when one is open.
//!
//! - Toggled with `E` (from [`PauseState::Playing`]); `Escape` also closes
//!   it. Reuses [`PauseState`] (a new [`PauseState::Inventory`] variant)
//!   rather than a second pause-like mechanism, purely to get its existing
//!   "release the cursor and gate player-control systems off" behavior for
//!   free -- see `pause.rs`'s module docs. Opening a container
//!   ([`crate::interact`]'s `OpenContainer`) drives the same transition once
//!   the server confirms it (`ServerToClient::ContainerOpened`, handled in
//!   `net.rs`), so the screen never shows a container the server hasn't
//!   actually granted.
//! - What the screen looks like is a pure function of [`PauseState`] and
//!   whether a container is open ([`desired_screen`]): [`ScreenKind::Plain`]
//!   (2x2 hand-craft grid), [`ScreenKind::CraftingTable`] (3x3, no slots of
//!   its own) or [`ScreenKind::Chest`] (2x2 grid plus the chest's 27 slots).
//!   [`sync_inventory_ui`] spawns/despawns reactively from this, mirroring
//!   [`crate::pause`]'s `sync_pause_ui` pattern.
//! - The crafting grid is *always* a 9-slot 3x3 array on the wire, masked
//!   rather than resized (`SlotArea::Crafting`'s doc comment in
//!   `tsumiki_protocol`): the 2x2 hand-craft view is its top-left square,
//!   indices 0, 1, 3 and 4. [`crafting_slot`] maps a view cell to its
//!   backing index via [`tsumiki_world::inventory::craft_grid_index`] for
//!   clicks, and [`update_crafting_slots`] reads content back through
//!   [`craft_view`] for rendering -- both delegate the mapping to the
//!   `world` crate rather than re-deriving it here.
//! - The client never mutates its own inventory: every slot square just
//!   renders the last [`state::GameState`]/[`state::ContainerState`]
//!   snapshot the server sent ([`read_slot`]/[`slot_visual`]); a click sends
//!   `SlotClick` and waits for the next snapshot. This is deliberate -- no
//!   local prediction, unlike block placement.
//! - Left/right click on a slot sends `SlotClick { slot, right, shift }`
//!   (shift held -> `shift: true`); `Q` sends `DropSlot` for whichever slot
//!   the mouse is over (`Ctrl+Q` drops the whole stack). Both are detected
//!   via [`bevy::ui::RelativeCursorPosition`] rather than `Interaction`,
//!   since `Interaction` (see `bevy_ui::focus::ui_focus_system`) only ever
//!   reacts to the left mouse button.
//! - The cursor stack (the stack picked up mid-drag) is drawn as a small
//!   icon that follows the mouse, a child of the screen's overlay root so
//!   its absolute position matches the window cursor 1:1.
//! - Items are drawn as flat colored squares ([`ItemDef::color`]) with the
//!   count in the corner, shown only above 1 -- [`slot_visual`], shared with
//!   [`crate::hotbar`] so both render identically.

use bevy::prelude::*;
use bevy::ui::RelativeCursorPosition;
use bevy::window::PrimaryWindow;
use tsumiki_protocol::{ClientToServer, ContainerKind, SlotArea, SlotRef};
use tsumiki_world::inventory::{CHEST_SIZE, craft_grid_index, craft_view};
use tsumiki_world::{HOTBAR_SIZE, ItemRegistry, ItemStack};

use crate::net;
use crate::pause::PauseState;
use crate::state::{self, ContainerState, GameState};
use crate::{AppState, UiFont, ui};

const SLOT_SIZE_PX: f32 = 40.0;
const SLOT_GAP_PX: f32 = 4.0;
const SECTION_GAP_PX: f32 = 14.0;
/// Empty-slot background: a faint lift off the panel, never pure black
/// (design.md §8).
const EMPTY_SLOT_COLOR: Color = Color::srgba(1.0, 1.0, 1.0, 0.10);

/// The inventory panel is far larger than the pause panel, so it uses a
/// more opaque background than the shared [`ui::PANEL_BG`]: at that panel's
/// alpha, terrain showing through this much area turns the slot grid into
/// noise and empty slots stop reading as slots.
const INVENTORY_PANEL_BG: Color = Color::srgba(0.14, 0.12, 0.18, 0.94);
const COUNT_FONT_SIZE: f32 = 16.0;
const TITLE_FONT_SIZE: f32 = 24.0;
const ARROW_FONT_SIZE: f32 = 24.0;

/// Backpack/chest grids are both laid out 9 columns x 3 rows (the backpack's
/// 27 slots follow the hotbar's 9 in `Main`; a chest's 27 slots are its own
/// `Container` area).
const GRID_COLS: usize = 9;
const GRID_ROWS: usize = CHEST_SIZE / GRID_COLS;

/// What the inventory screen currently shows, as a pure function of
/// [`PauseState`] and whatever container (if any) is open -- see
/// [`desired_screen`]. Drives both [`sync_inventory_ui`] (spawn/despawn) and
/// [`crafting_grid_size`] (2x2 vs 3x3).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum ScreenKind {
    #[default]
    None,
    Plain,
    CraftingTable,
    Chest,
}

/// The screen [`sync_inventory_ui`] should show right now. Pure and
/// unit-tested: the screen is closed unless [`PauseState::Inventory`] is
/// active, and otherwise reflects whatever container kind (if any) is open.
fn desired_screen(pause: PauseState, container: Option<ContainerKind>) -> ScreenKind {
    if pause != PauseState::Inventory {
        return ScreenKind::None;
    }
    match container {
        None => ScreenKind::Plain,
        Some(ContainerKind::CraftingTable) => ScreenKind::CraftingTable,
        Some(ContainerKind::Chest) => ScreenKind::Chest,
    }
}

/// The crafting grid's width/height: 3x3 only while a crafting table is
/// open, 2x2 (hand-crafting) otherwise -- including while a chest is open,
/// which does not widen it.
fn crafting_grid_size(screen: ScreenKind) -> usize {
    if screen == ScreenKind::CraftingTable {
        3
    } else {
        2
    }
}

fn title_for(screen: ScreenKind) -> &'static str {
    match screen {
        ScreenKind::None => "",
        ScreenKind::Plain => "Inventory",
        ScreenKind::CraftingTable => "Crafting Table",
        ScreenKind::Chest => "Chest",
    }
}

/// `E`'s target state: opens from [`PauseState::Playing`], closes from
/// [`PauseState::Inventory`] (either way, whether or not a container is
/// open), and does nothing from [`PauseState::Paused`]/[`PauseState::Settings`].
/// Pure and unit-tested.
fn e_key_target(current: PauseState) -> Option<PauseState> {
    match current {
        PauseState::Playing => Some(PauseState::Inventory),
        PauseState::Inventory => Some(PauseState::Playing),
        PauseState::Paused | PauseState::Settings => None,
    }
}

// ---- slot-ref <-> screen-layout mapping ----

/// The backpack's row-major `(row, col)` cell (3 rows of 9, `col`/`row` both
/// 0-based) as an index into [`SlotArea::Main`] -- offset past the hotbar's
/// [`HOTBAR_SIZE`] slots.
fn backpack_index(row: usize, col: usize) -> usize {
    HOTBAR_SIZE + row * GRID_COLS + col
}

fn main_slot(index: usize) -> SlotRef {
    SlotRef {
        area: SlotArea::Main,
        index: index as u8,
    }
}

/// A crafting grid cell (row-major, `size`-wide *view*) as a
/// [`SlotArea::Crafting`] ref. `size` is 2 (hand-craft) or 3 (table); see
/// [`crafting_grid_size`].
///
/// The crafting grid is *always* a 9-slot 3x3 array on the wire (masked, not
/// resized, so opening/closing a table never moves what's already in it);
/// the 2x2 hand-craft view sits at indices 0, 1, 3 and 4 of that array, not
/// contiguous 0..4. [`tsumiki_world::inventory::craft_grid_index`] is the
/// single source of truth for that mapping -- it is deliberately not
/// re-derived here (see `SlotArea::Crafting`'s doc comment in
/// `tsumiki_protocol`).
fn crafting_slot(row: usize, col: usize, size: usize) -> SlotRef {
    let cell = row * size + col;
    SlotRef {
        area: SlotArea::Crafting,
        index: craft_grid_index(size, cell).expect("cell is within the size x size view") as u8,
    }
}

fn craft_output_slot() -> SlotRef {
    SlotRef {
        area: SlotArea::CraftOutput,
        index: 0,
    }
}

/// A chest cell (row-major, 9-wide) as a [`SlotArea::Container`] ref.
fn container_slot(row: usize, col: usize) -> SlotRef {
    SlotRef {
        area: SlotArea::Container,
        index: (row * GRID_COLS + col) as u8,
    }
}

/// Reads what `slot` currently holds from the last server snapshot. Pure and
/// unit-tested: this is the single place that knows how a [`SlotRef`] maps
/// onto [`GameState`]/[`ContainerState`], shared by every slot square's
/// rendering.
fn read_slot(state: &GameState, container: &ContainerState, slot: SlotRef) -> Option<ItemStack> {
    match slot.area {
        SlotArea::Main => state.main.get(slot.index as usize).copied().flatten(),
        SlotArea::Crafting => state.crafting.get(slot.index as usize).copied().flatten(),
        SlotArea::CraftOutput => state.craft_output,
        SlotArea::Container => container
            .open
            .as_ref()
            .and_then(|open| open.slots.get(slot.index as usize).copied().flatten()),
    }
}

/// One slot square's rendered appearance: background color and count label
/// (empty unless `count > 1`, design.md's pop/toy-like minimal-clutter
/// convention). Pure and unit-tested; shared by [`crate::hotbar`] so both
/// render identically.
pub struct SlotVisual {
    pub color: Color,
    pub count_text: String,
}

pub fn slot_visual(stack: Option<ItemStack>, reg: &ItemRegistry) -> SlotVisual {
    match stack {
        None => SlotVisual {
            color: EMPTY_SLOT_COLOR,
            count_text: String::new(),
        },
        Some(stack) => {
            let c = reg.get(stack.item).color;
            SlotVisual {
                color: Color::srgb_u8(c[0], c[1], c[2]),
                count_text: if stack.count > 1 {
                    stack.count.to_string()
                } else {
                    String::new()
                },
            }
        }
    }
}

// ---- ECS wiring ----

/// A spawned slot square, carrying the [`SlotRef`] it addresses. One
/// component type for every section (main/crafting/output/container): click
/// handling and rendering only ever need the ref, never which section it
/// came from.
#[derive(Component, Clone, Copy)]
struct SlotWidget(SlotRef);

/// The count-text child of a [`SlotWidget`].
#[derive(Component)]
struct SlotCountText;

/// Tags a crafting-grid slot square with its position in the active
/// `size`x`size` *view* (row-major), as opposed to [`SlotWidget`]'s already-
/// masked backing index. [`update_crafting_slots`] reads content through
/// [`craft_view`] via this, rather than the generic index on [`SlotWidget`]
/// -- both agree (`craft_view` is built from the same
/// [`craft_grid_index`] this module's [`crafting_slot`] uses), but routing
/// rendering through `craft_view` keeps "what does view cell N currently
/// show" answered in exactly one place.
#[derive(Component)]
struct CraftingCell(usize);

/// The floating icon that follows the mouse while it holds
/// [`GameState::cursor`].
#[derive(Component)]
struct CursorStackIcon;
#[derive(Component)]
struct CursorStackCountText;

/// Cache of "what screen did we last build", compared against
/// [`desired_screen`] each frame by [`sync_inventory_ui`] -- mirrors
/// [`crate::pause::PauseUi`]'s pattern.
#[derive(Resource, Default)]
struct InventoryUi {
    kind: ScreenKind,
    root: Option<Entity>,
}

/// Wires the inventory screen into `app`.
pub fn install(app: &mut App) {
    app.init_resource::<InventoryUi>()
        .add_systems(OnExit(AppState::InGame), teardown)
        .add_systems(OnExit(PauseState::Inventory), send_close_container)
        .add_systems(
            Update,
            toggle_inventory_key
                .run_if(in_state(AppState::InGame))
                .run_if(state::is_alive),
        )
        .add_systems(
            Update,
            (
                sync_inventory_ui,
                update_slots,
                update_crafting_slots,
                update_cursor_stack,
            )
                .chain()
                .run_if(in_state(AppState::InGame)),
        )
        .add_systems(
            Update,
            (handle_slot_clicks, handle_drop_key).run_if(in_state(PauseState::Inventory)),
        );
}

fn toggle_inventory_key(
    keys: Res<ButtonInput<KeyCode>>,
    state: Res<State<PauseState>>,
    mut next: ResMut<NextState<PauseState>>,
) {
    if !keys.just_pressed(KeyCode::KeyE) {
        return;
    }
    if let Some(target) = e_key_target(*state.get()) {
        next.set(target);
    }
}

/// Leaving the screen (`E` or `Escape`, from either side) always tells the
/// server: dropping the cursor stack and any crafting-grid contents into the
/// world is the server's job (protocol docs on `CloseContainer`), not
/// something the client can skip just because no container happened to be
/// open. Tolerates the transport not existing (should not happen while this
/// state is reachable, but mirrors the defensive pattern used elsewhere).
fn send_close_container(transport: Option<ResMut<net::Transport>>) {
    if let Some(mut transport) = transport {
        transport.send(ClientToServer::CloseContainer);
    }
}

fn teardown(mut commands: Commands, mut ui_state: ResMut<InventoryUi>) {
    if let Some(root) = ui_state.root.take() {
        commands.entity(root).despawn();
    }
    *ui_state = InventoryUi::default();
}

/// Spawns/despawns the inventory screen as a pure function of
/// [`desired_screen`] -- the only system in this module that touches UI
/// entities (mirrors [`crate::pause::sync_pause_ui`]).
fn sync_inventory_ui(
    state: Res<State<PauseState>>,
    container: Res<ContainerState>,
    mut ui_state: ResMut<InventoryUi>,
    mut commands: Commands,
    font: Res<UiFont>,
) {
    let desired = desired_screen(*state.get(), container.open.as_ref().map(|open| open.kind));
    if ui_state.kind == desired {
        return;
    }
    if let Some(root) = ui_state.root.take() {
        commands.entity(root).despawn();
    }
    ui_state.root = spawn_screen(&mut commands, &font, desired);
    ui_state.kind = desired;
}

fn spawn_screen(commands: &mut Commands, font: &UiFont, screen: ScreenKind) -> Option<Entity> {
    if screen == ScreenKind::None {
        return None;
    }
    let root = ui::spawn_overlay_root(commands);

    let panel = commands
        .spawn((
            Node {
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: Val::Px(SECTION_GAP_PX),
                padding: UiRect::all(Val::Px(24.0)),
                border_radius: BorderRadius::all(Val::Px(18.0)),
                ..default()
            },
            BackgroundColor(INVENTORY_PANEL_BG),
        ))
        .id();

    commands.entity(panel).with_children(|parent| {
        parent.spawn((
            Text::new(title_for(screen)),
            font.text(TITLE_FONT_SIZE),
            TextColor(ui::PANEL_TEXT_COLOR),
        ));

        let size = crafting_grid_size(screen);
        parent
            .spawn(Node {
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                column_gap: Val::Px(SECTION_GAP_PX),
                ..default()
            })
            .with_children(|row| {
                spawn_crafting_grid(row, font, size);
                row.spawn((
                    Text::new(">"),
                    font.text(ARROW_FONT_SIZE),
                    TextColor(ui::PANEL_TEXT_COLOR),
                ));
                spawn_slot(row, font, craft_output_slot());
            });

        if screen == ScreenKind::Chest {
            spawn_grid(parent, font, GRID_COLS, GRID_ROWS, container_slot);
        }

        spawn_grid(parent, font, GRID_COLS, GRID_ROWS, |r, c| {
            main_slot(backpack_index(r, c))
        });
        spawn_grid(parent, font, HOTBAR_SIZE, 1, |_, c| main_slot(c));
    });

    commands.entity(root).add_child(panel);
    spawn_cursor_stack_icon(commands, root, font);
    Some(root)
}

fn spawn_grid(
    parent: &mut ChildSpawnerCommands<'_>,
    font: &UiFont,
    cols: usize,
    rows: usize,
    slot_ref: impl Fn(usize, usize) -> SlotRef,
) {
    parent
        .spawn(Node {
            flex_direction: FlexDirection::Column,
            row_gap: Val::Px(SLOT_GAP_PX),
            ..default()
        })
        .with_children(|grid| {
            for r in 0..rows {
                grid.spawn(Node {
                    flex_direction: FlexDirection::Row,
                    column_gap: Val::Px(SLOT_GAP_PX),
                    ..default()
                })
                .with_children(|row_node| {
                    for c in 0..cols {
                        spawn_slot(row_node, font, slot_ref(r, c));
                    }
                });
            }
        });
}

/// Like [`spawn_grid`], but for the crafting section specifically: each cell
/// additionally carries a [`CraftingCell`] (its position in the `size`x`size`
/// view) alongside the masked [`SlotWidget`] ref, so [`update_crafting_slots`]
/// can paint it through [`craft_view`].
fn spawn_crafting_grid(parent: &mut ChildSpawnerCommands<'_>, font: &UiFont, size: usize) {
    parent
        .spawn(Node {
            flex_direction: FlexDirection::Column,
            row_gap: Val::Px(SLOT_GAP_PX),
            ..default()
        })
        .with_children(|grid| {
            for row in 0..size {
                grid.spawn(Node {
                    flex_direction: FlexDirection::Row,
                    column_gap: Val::Px(SLOT_GAP_PX),
                    ..default()
                })
                .with_children(|row_node| {
                    for col in 0..size {
                        spawn_crafting_slot(row_node, font, size, row, col);
                    }
                });
            }
        });
}

fn spawn_crafting_slot(
    parent: &mut ChildSpawnerCommands<'_>,
    font: &UiFont,
    size: usize,
    row: usize,
    col: usize,
) {
    parent
        .spawn((
            Node {
                width: Val::Px(SLOT_SIZE_PX),
                height: Val::Px(SLOT_SIZE_PX),
                border_radius: BorderRadius::all(Val::Px(6.0)),
                ..default()
            },
            BackgroundColor(EMPTY_SLOT_COLOR),
            RelativeCursorPosition::default(),
            SlotWidget(crafting_slot(row, col, size)),
            CraftingCell(row * size + col),
        ))
        .with_children(|s| {
            s.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    right: Val::Px(2.0),
                    bottom: Val::Px(0.0),
                    ..default()
                },
                Text::new(""),
                font.text(COUNT_FONT_SIZE),
                TextColor(Color::WHITE),
                SlotCountText,
            ));
        });
}

fn spawn_slot(parent: &mut ChildSpawnerCommands<'_>, font: &UiFont, slot: SlotRef) {
    parent
        .spawn((
            Node {
                width: Val::Px(SLOT_SIZE_PX),
                height: Val::Px(SLOT_SIZE_PX),
                border_radius: BorderRadius::all(Val::Px(6.0)),
                ..default()
            },
            BackgroundColor(EMPTY_SLOT_COLOR),
            RelativeCursorPosition::default(),
            SlotWidget(slot),
        ))
        .with_children(|s| {
            s.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    right: Val::Px(2.0),
                    bottom: Val::Px(0.0),
                    ..default()
                },
                Text::new(""),
                font.text(COUNT_FONT_SIZE),
                TextColor(Color::WHITE),
                SlotCountText,
            ));
        });
}

fn spawn_cursor_stack_icon(commands: &mut Commands, root: Entity, font: &UiFont) {
    let icon = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                width: Val::Px(SLOT_SIZE_PX * 0.7),
                height: Val::Px(SLOT_SIZE_PX * 0.7),
                border_radius: BorderRadius::all(Val::Px(4.0)),
                ..default()
            },
            BackgroundColor(EMPTY_SLOT_COLOR),
            Visibility::Hidden,
            CursorStackIcon,
        ))
        .with_children(|s| {
            s.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    right: Val::Px(1.0),
                    bottom: Val::Px(0.0),
                    ..default()
                },
                Text::new(""),
                font.text(COUNT_FONT_SIZE * 0.75),
                TextColor(Color::WHITE),
                CursorStackCountText,
            ));
        })
        .id();
    commands.entity(root).add_child(icon);
}

/// Repaints every non-crafting slot square from the last server snapshot
/// ([`update_crafting_slots`] handles the crafting grid, through
/// [`craft_view`] instead of this generic per-[`SlotRef`] path). Runs
/// unconditionally in [`AppState::InGame`] (cheap, and harmless when no
/// screen is spawned: the queries simply match nothing).
fn update_slots(
    game_state: Res<GameState>,
    container: Res<ContainerState>,
    item_reg: Res<state::ItemReg>,
    mut slots: Query<(&SlotWidget, &mut BackgroundColor, &Children), Without<CraftingCell>>,
    mut texts: Query<&mut Text, With<SlotCountText>>,
) {
    for (widget, mut bg, children) in &mut slots {
        let visual = slot_visual(read_slot(&game_state, &container, widget.0), &item_reg.0);
        *bg = BackgroundColor(visual.color);
        for &child in children {
            if let Ok(mut text) = texts.get_mut(child) {
                text.0 = visual.count_text.clone();
            }
        }
    }
}

/// Repaints the crafting grid through
/// [`tsumiki_world::inventory::craft_view`] at the active view size, rather
/// than the generic per-[`SlotRef`] path [`update_slots`] uses for every
/// other section -- this is the "render through `craft_view`" contract
/// (`SlotArea::Crafting`'s doc comment in `tsumiki_protocol`): the 2x2 view
/// always shows the top-left square of the always-9-slot backing array.
fn update_crafting_slots(
    pause_state: Res<State<PauseState>>,
    game_state: Res<GameState>,
    container: Res<ContainerState>,
    item_reg: Res<state::ItemReg>,
    mut cells: Query<(&CraftingCell, &mut BackgroundColor, &Children)>,
    mut texts: Query<&mut Text, With<SlotCountText>>,
) {
    if cells.is_empty() {
        return;
    }
    let screen = desired_screen(
        *pause_state.get(),
        container.open.as_ref().map(|open| open.kind),
    );
    let size = crafting_grid_size(screen);
    let view = craft_view(&game_state.crafting, size);

    for (cell, mut bg, children) in &mut cells {
        let visual = slot_visual(view.get(cell.0).copied().flatten(), &item_reg.0);
        *bg = BackgroundColor(visual.color);
        for &child in children {
            if let Ok(mut text) = texts.get_mut(child) {
                text.0 = visual.count_text.clone();
            }
        }
    }
}

/// Moves the cursor-stack icon to the window cursor position and repaints it
/// from [`GameState::cursor`]; hidden whenever nothing is held.
fn update_cursor_stack(
    game_state: Res<GameState>,
    item_reg: Res<state::ItemReg>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut icons: Query<(&mut Node, &mut BackgroundColor, &mut Visibility), With<CursorStackIcon>>,
    mut texts: Query<&mut Text, With<CursorStackCountText>>,
) {
    let Ok(window) = windows.single() else {
        return;
    };
    let cursor_pos = window.cursor_position();
    let visual = slot_visual(game_state.cursor, &item_reg.0);
    let visible = game_state.cursor.is_some() && cursor_pos.is_some();

    for (mut node, mut bg, mut vis) in &mut icons {
        *vis = if visible {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
        if let Some(pos) = cursor_pos {
            node.left = Val::Px(pos.x - SLOT_SIZE_PX * 0.35);
            node.top = Val::Px(pos.y - SLOT_SIZE_PX * 0.35);
        }
        *bg = BackgroundColor(visual.color);
    }
    for mut text in &mut texts {
        text.0 = visual.count_text.clone();
    }
}

/// The [`SlotWidget`] the mouse is currently over, if any.
fn hovered_slot(slots: &Query<(&SlotWidget, &RelativeCursorPosition)>) -> Option<SlotRef> {
    slots
        .iter()
        .find(|(_, rel)| rel.cursor_over())
        .map(|(widget, _)| widget.0)
}

fn handle_slot_clicks(
    mouse: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    mut transport: ResMut<net::Transport>,
    slots: Query<(&SlotWidget, &RelativeCursorPosition)>,
) {
    let right = mouse.just_pressed(MouseButton::Right);
    let left = mouse.just_pressed(MouseButton::Left);
    if !left && !right {
        return;
    }
    let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
    if let Some(slot) = hovered_slot(&slots) {
        transport.send(ClientToServer::SlotClick { slot, right, shift });
    }
}

/// `Q` drops one of the hovered slot's stack; `Ctrl+Q` drops all of it
/// (vanilla Minecraft's convention -- the protocol leaves the modifier up to
/// the client).
fn handle_drop_key(
    keys: Res<ButtonInput<KeyCode>>,
    mut transport: ResMut<net::Transport>,
    slots: Query<(&SlotWidget, &RelativeCursorPosition)>,
) {
    if !keys.just_pressed(KeyCode::KeyQ) {
        return;
    }
    let all = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    if let Some(slot) = hovered_slot(&slots) {
        transport.send(ClientToServer::DropSlot { slot, all });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsumiki_world::ItemRegistry;
    use tsumiki_world::items;

    #[test]
    fn screen_is_closed_unless_pause_state_is_inventory() {
        assert_eq!(desired_screen(PauseState::Playing, None), ScreenKind::None);
        assert_eq!(
            desired_screen(PauseState::Paused, Some(ContainerKind::Chest)),
            ScreenKind::None
        );
        assert_eq!(desired_screen(PauseState::Settings, None), ScreenKind::None);
    }

    #[test]
    fn open_screen_reflects_the_container_kind() {
        assert_eq!(
            desired_screen(PauseState::Inventory, None),
            ScreenKind::Plain
        );
        assert_eq!(
            desired_screen(PauseState::Inventory, Some(ContainerKind::Chest)),
            ScreenKind::Chest
        );
        assert_eq!(
            desired_screen(PauseState::Inventory, Some(ContainerKind::CraftingTable)),
            ScreenKind::CraftingTable
        );
    }

    #[test]
    fn crafting_grid_widens_only_at_a_table() {
        assert_eq!(crafting_grid_size(ScreenKind::Plain), 2);
        assert_eq!(crafting_grid_size(ScreenKind::Chest), 2);
        assert_eq!(crafting_grid_size(ScreenKind::CraftingTable), 3);
    }

    #[test]
    fn e_key_toggles_between_playing_and_inventory() {
        assert_eq!(
            e_key_target(PauseState::Playing),
            Some(PauseState::Inventory)
        );
        assert_eq!(
            e_key_target(PauseState::Inventory),
            Some(PauseState::Playing)
        );
        assert_eq!(e_key_target(PauseState::Paused), None);
        assert_eq!(e_key_target(PauseState::Settings), None);
    }

    #[test]
    fn backpack_index_offsets_past_the_hotbar() {
        assert_eq!(backpack_index(0, 0), HOTBAR_SIZE);
        assert_eq!(backpack_index(2, 8), HOTBAR_SIZE + 26);
    }

    #[test]
    fn hand_craft_view_maps_onto_the_masked_top_left_square() {
        // The 2x2 hand-craft view sits at indices 0, 1, 3, 4 of the always-
        // 9-slot backing array -- not contiguous 0..4.
        assert_eq!(
            crafting_slot(0, 0, 2),
            SlotRef {
                area: SlotArea::Crafting,
                index: 0
            }
        );
        assert_eq!(
            crafting_slot(0, 1, 2),
            SlotRef {
                area: SlotArea::Crafting,
                index: 1
            }
        );
        assert_eq!(
            crafting_slot(1, 0, 2),
            SlotRef {
                area: SlotArea::Crafting,
                index: 3
            },
            "bottom-left cell of the 2x2 view"
        );
        assert_eq!(
            crafting_slot(1, 1, 2),
            SlotRef {
                area: SlotArea::Crafting,
                index: 4
            }
        );
    }

    #[test]
    fn table_craft_view_is_the_identity_mapping() {
        for row in 0..3 {
            for col in 0..3 {
                assert_eq!(
                    crafting_slot(row, col, 3),
                    SlotRef {
                        area: SlotArea::Crafting,
                        index: (row * 3 + col) as u8
                    }
                );
            }
        }
    }

    #[test]
    fn container_slot_ref_is_row_major() {
        assert_eq!(
            container_slot(0, 0),
            SlotRef {
                area: SlotArea::Container,
                index: 0
            }
        );
        assert_eq!(
            container_slot(2, 8),
            SlotRef {
                area: SlotArea::Container,
                index: 26
            }
        );
    }

    #[test]
    fn read_slot_reads_the_matching_snapshot_field() {
        let mut state = GameState::default();
        state.main[3] = Some(ItemStack::one(items::STICK));
        state.crafting[0] = Some(ItemStack::new(items::PLANKS, 2));
        state.craft_output = Some(ItemStack::one(items::CRAFTING_TABLE));
        let no_container = ContainerState::default();

        assert_eq!(
            read_slot(&state, &no_container, main_slot(3)),
            Some(ItemStack::one(items::STICK))
        );
        assert_eq!(
            read_slot(&state, &no_container, crafting_slot(0, 0, 2)),
            Some(ItemStack::new(items::PLANKS, 2))
        );
        assert_eq!(
            read_slot(&state, &no_container, craft_output_slot()),
            Some(ItemStack::one(items::CRAFTING_TABLE))
        );
    }

    #[test]
    fn container_slot_reads_from_the_open_container() {
        let state = GameState::default();
        let mut slots = vec![None; CHEST_SIZE];
        slots[0] = Some(ItemStack::one(items::CHEST));
        let container = ContainerState {
            open: Some(state::OpenContainer {
                kind: ContainerKind::Chest,
                pos: IVec3::ZERO,
                slots,
            }),
        };

        assert_eq!(
            read_slot(&state, &container, container_slot(0, 0)),
            Some(ItemStack::one(items::CHEST))
        );
    }

    #[test]
    fn container_slot_is_none_when_nothing_is_open() {
        let state = GameState::default();
        assert_eq!(
            read_slot(&state, &ContainerState::default(), container_slot(0, 0)),
            None
        );
    }

    #[test]
    fn slot_visual_hides_the_count_for_a_lone_item() {
        let reg = ItemRegistry::prototype();
        let visual = slot_visual(Some(ItemStack::one(items::STICK)), &reg);
        assert_eq!(visual.count_text, "");
    }

    #[test]
    fn slot_visual_shows_the_count_above_one() {
        let reg = ItemRegistry::prototype();
        let visual = slot_visual(Some(ItemStack::new(items::STICK, 5)), &reg);
        assert_eq!(visual.count_text, "5");
    }

    #[test]
    fn slot_visual_empty_slot_is_the_empty_color_with_no_count() {
        let reg = ItemRegistry::prototype();
        let visual = slot_visual(None, &reg);
        assert_eq!(visual.color, EMPTY_SLOT_COLOR);
        assert_eq!(visual.count_text, "");
    }
}

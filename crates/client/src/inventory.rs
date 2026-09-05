//! The inventory screen (roadmap M5, design.md §7): backpack + hotbar, a
//! recipe list, and a chest's or furnace's slots when one is open.
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
//!   (hand recipes only), [`ScreenKind::CraftingTable`] (hand recipes plus
//!   whatever a crafting table unlocks), [`ScreenKind::Chest`] (hand recipes
//!   plus the chest's 27 slots), or [`ScreenKind::Furnace`] (roadmap M6: hand
//!   recipes plus the furnace's input/fuel/output slots and its cook/fuel
//!   gauges, [`spawn_furnace`]/[`update_furnace_bars`]). A furnace grants no
//!   [`tsumiki_world::recipe::CraftingStation`] any more than a chest does --
//!   smelting is a separate recipe format (`tsumiki_world::smelting`) with
//!   its own server-side clock, not something the hand-crafting list drives.
//!   [`sync_inventory_ui`] spawns/despawns reactively from this, mirroring
//!   [`crate::pause`]'s `sync_pause_ui` pattern.
//! - Crafting is a scrollable list, not a grid (design.md §7,
//!   `tsumiki_world::recipe`'s module docs on why the grid was removed): one
//!   row per [`tsumiki_world::RecipeRegistry::available`] entry for
//!   [`station_for`]'s station, showing the output icon+count, the output's
//!   name, and a "Needs ..." line spelling out what it consumes -- names
//!   rather than icons alone, since identifying items by color is exactly
//!   the memorisation this list exists to remove. A row the player cannot
//!   currently afford is
//!   dimmed, not hidden -- [`update_recipe_affordability`] recomputes this
//!   live from the inventory snapshot via
//!   [`tsumiki_world::recipe::can_craft`], entirely client-side (the server
//!   deliberately never sends craftability, see
//!   `ServerToClient::InventoryUpdate`'s doc comment). Left click on a row
//!   sends `Craft { recipe, all: false }`; shift-click sends `all: true`
//!   ([`handle_recipe_clicks`]).
//! - The client never mutates its own inventory: every slot square and
//!   recipe row just renders the last [`state::GameState`]/
//!   [`state::ContainerState`]/[`state::RecipeReg`] snapshot
//!   ([`read_slot`]/[`slot_visual`]/[`recipe_is_affordable`]); a slot click
//!   sends `SlotClick` and a row click sends `Craft`, and both wait for the
//!   next snapshot. This is deliberate -- no local prediction, unlike block
//!   placement.
//! - Left/right click on a slot sends `SlotClick { slot, right, shift }`
//!   (shift held -> `shift: true`); `Q` sends `DropSlot` for whichever slot
//!   the mouse is over (`Ctrl+Q` drops the whole stack). Both are detected
//!   via [`bevy::ui::RelativeCursorPosition`] rather than `Interaction`,
//!   since `Interaction` (see `bevy_ui::focus::ui_focus_system`) only ever
//!   reacts to the left mouse button -- which is also why a recipe row (an
//!   ordinary `Button`) can rely on plain `Interaction` for its own left-only
//!   click handling.
//! - The cursor stack (the stack picked up mid-drag) is drawn as a small
//!   icon that follows the mouse, a child of the screen's overlay root so
//!   its absolute position matches the window cursor 1:1.
//! - Items use the shared pixel-art atlas with the count in the corner,
//!   shown only above 1 -- [`slot_visual`], shared with
//!   [`crate::hotbar`] so both render identically. A tool that has taken
//!   damage also gets a thin wear bar along the icon's bottom edge
//!   (roadmap M6, [`SlotVisual::wear`]/[`wear_fraction`]) -- the same shared
//!   function, so the hotbar and the backpack never disagree about how worn
//!   a tool looks.

use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::prelude::*;
use bevy::ui::RelativeCursorPosition;
use bevy::window::PrimaryWindow;
use tsumiki_protocol::{ClientToServer, ContainerKind, SlotArea, SlotRef};
use tsumiki_world::inventory::CHEST_SIZE;
use tsumiki_world::recipe::{CraftingStation, Recipe, can_craft};
use tsumiki_world::smelting::{FURNACE_FUEL, FURNACE_INPUT, FURNACE_OUTPUT};
use tsumiki_world::{HOTBAR_SIZE, Inventory, ItemRegistry, ItemStack, RecipeId, RecipeRegistry};

use crate::item_icons::{self, ItemIcons};
use crate::net;
use crate::pause::PauseState;
use crate::state::{self, ContainerState, GameState};
use crate::{AppState, UiFont, ui};

const SLOT_SIZE_PX: f32 = 40.0;
const ITEM_ICON_SIZE_PX: f32 = 32.0;
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

/// Backpack/chest grids are both laid out 9 columns x 3 rows (the backpack's
/// 27 slots follow the hotbar's 9 in `Main`; a chest's 27 slots are its own
/// `Container` area).
const GRID_COLS: usize = 9;
const GRID_ROWS: usize = CHEST_SIZE / GRID_COLS;

/// Height of the durability wear bar drawn along an icon's bottom edge
/// (roadmap M6), shared with [`crate::hotbar`]. A sliver, not a second gauge:
/// it only needs to be legible at a glance.
pub const WEAR_BAR_HEIGHT_PX: f32 = 4.0;
/// A muted amber -- reads as "worn", not as a health/danger warning (which
/// already owns red, see `crate::health::HEALTH_FILL_COLOR`).
pub const WEAR_BAR_COLOR: Color = Color::srgb(0.82, 0.58, 0.22);

// ---- furnace screen (roadmap M6) ----

const FURNACE_ROW_GAP_PX: f32 = 8.0;
const FURNACE_TRACK_COLOR: Color = Color::srgb(0.20, 0.14, 0.12);
/// The cook bar: an ember orange, since it tracks the item currently
/// smelting.
const FURNACE_COOK_COLOR: Color = Color::srgb(0.85, 0.45, 0.20);
/// The fuel bar: a duller amber than the cook bar, so the two never get
/// mixed up at a glance even though both live in the same panel.
const FURNACE_FUEL_COLOR: Color = Color::srgb(0.80, 0.62, 0.22);

// ---- recipe list ----

const RECIPE_ICON_SIZE_PX: f32 = 32.0;
const RECIPE_ROW_GAP_PX: f32 = 8.0;
const RECIPE_ICON_GAP_PX: f32 = 6.0;
/// Tallest the recipe list gets before it scrolls instead of growing --
/// roughly three rows, matching `crate::menu`'s world-select list's approach
/// to "grows until it doesn't".
const RECIPE_LIST_MAX_HEIGHT_PX: f32 = 176.0;
const RECIPE_NAME_FONT_SIZE: f32 = 16.0;
const RECIPE_NEEDS_FONT_SIZE: f32 = 16.0;
/// The requirements line is quieter than the recipe's name: it is reference,
/// not the thing being chosen.
const RECIPE_NEEDS_COLOR: Color = Color::srgb(0.72, 0.70, 0.66);
const RECIPE_COUNT_FONT_SIZE: f32 = 16.0;
const RECIPE_ROW_BG: Color = Color::srgba(1.0, 1.0, 1.0, 0.06);
/// Alpha multiplier applied to a recipe row's icon/text colors while it
/// cannot currently be crafted -- dimmed, not hidden: seeing what you cannot
/// yet make is how a player learns what to gather (see module docs).
const UNAFFORDABLE_ALPHA: f32 = 0.35;
/// Pixels-per-line for `MouseScrollUnit::Line` wheel events, matching the
/// constant Bevy's own `scroll_and_overflow` example uses (mirrors
/// `crate::menu`'s identical constant for its own scrollable list).
const SCROLL_LINE_HEIGHT: f32 = 21.0;

/// What the inventory screen currently shows, as a pure function of
/// [`PauseState`] and whatever container (if any) is open -- see
/// [`desired_screen`]. Drives both [`sync_inventory_ui`] (spawn/despawn) and
/// [`station_for`] (which recipes the list shows).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum ScreenKind {
    #[default]
    None,
    Plain,
    CraftingTable,
    Chest,
    /// Input/fuel/output slots plus a smelting progress bar (roadmap M6).
    Furnace,
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
        Some(ContainerKind::Furnace) => ScreenKind::Furnace,
    }
}

/// Which crafting station (if any) the recipe list draws from for `screen`:
/// a crafting table only while [`ScreenKind::CraftingTable`] is showing --
/// hand recipes are available everywhere else, including
/// [`ScreenKind::Chest`]/[`ScreenKind::Furnace`], neither of which grants a
/// station any more than the plain screen does (a furnace has its own
/// separate recipe format, `tsumiki_world::smelting`, not
/// `CraftingStation`). Pure and unit-tested.
fn station_for(screen: ScreenKind) -> Option<CraftingStation> {
    match screen {
        ScreenKind::CraftingTable => Some(CraftingStation::CraftingTable),
        ScreenKind::None | ScreenKind::Plain | ScreenKind::Chest | ScreenKind::Furnace => None,
    }
}

fn title_for(screen: ScreenKind) -> &'static str {
    match screen {
        ScreenKind::None => "",
        ScreenKind::Plain => "Inventory",
        ScreenKind::CraftingTable => "Crafting Table",
        ScreenKind::Chest => "Chest",
        ScreenKind::Furnace => "Furnace",
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

/// A chest cell (row-major, 9-wide) as a [`SlotArea::Container`] ref.
fn container_slot(row: usize, col: usize) -> SlotRef {
    SlotRef {
        area: SlotArea::Container,
        index: (row * GRID_COLS + col) as u8,
    }
}

/// One of a furnace's three named slots (`tsumiki_world::smelting::FURNACE_*`)
/// as a [`SlotArea::Container`] ref -- a furnace has no grid, just fixed
/// indices, so this is `container_slot`'s flat counterpart. Pure and
/// unit-tested: this is the one place the smelting crate's slot indices get
/// turned into screen positions.
fn furnace_slot(index: usize) -> SlotRef {
    SlotRef {
        area: SlotArea::Container,
        index: index as u8,
    }
}

/// Reads what `slot` currently holds from the last server snapshot. Pure and
/// unit-tested: this is the single place that knows how a [`SlotRef`] maps
/// onto [`GameState`]/[`ContainerState`], shared by every slot square's
/// rendering.
fn read_slot(state: &GameState, container: &ContainerState, slot: SlotRef) -> Option<ItemStack> {
    match slot.area {
        SlotArea::Main => state.main.get(slot.index as usize).copied().flatten(),
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
    /// Durability wear-bar fill fraction (roadmap M6), if this stack should
    /// show one at all -- see [`wear_fraction`].
    pub wear: Option<f32>,
}

pub fn slot_visual(stack: Option<ItemStack>, reg: &ItemRegistry) -> SlotVisual {
    match stack {
        None => SlotVisual {
            color: EMPTY_SLOT_COLOR,
            count_text: String::new(),
            wear: None,
        },
        Some(stack) => SlotVisual {
            color: EMPTY_SLOT_COLOR,
            count_text: if stack.count > 1 {
                stack.count.to_string()
            } else {
                String::new()
            },
            wear: wear_fraction(stack, reg),
        },
    }
}

/// Fraction of a tool's durability already spent (`damage / durability`), for
/// the wear bar drawn along an icon's bottom edge. `None` for anything that
/// either isn't a tool or hasn't been used yet -- a fresh tool or an ordinary
/// item draws no bar at all, the same "only show it when it says something"
/// rule [`slot_visual`]'s count text already follows. Pure and unit-tested.
pub fn wear_fraction(stack: ItemStack, reg: &ItemRegistry) -> Option<f32> {
    let tool = reg.tool(stack.item)?;
    if stack.damage == 0 || tool.durability == 0 {
        return None;
    }
    Some((stack.damage as f32 / tool.durability as f32).clamp(0.0, 1.0))
}

// ---- recipe list helpers ----

/// The recipe ids `station` makes available, in catalog order. A thin pure
/// wrapper around [`RecipeRegistry::available`] so "what does this screen
/// list" is unit-tested without spinning up the inventory screen's ECS.
fn available_recipe_ids(reg: &RecipeRegistry, station: Option<CraftingStation>) -> Vec<RecipeId> {
    reg.available(station).map(|(id, _)| id).collect()
}

/// Whether `recipe` can be crafted at least once from `main`, the player's
/// own slot snapshot. This is the exact client-side computation
/// [`update_recipe_affordability`] uses to dim a row -- the server
/// deliberately never sends craftability
/// (`ServerToClient::InventoryUpdate`'s doc comment), since the client holds
/// the same recipe registry and can derive it. Pure and unit-tested.
fn recipe_is_affordable(recipe: &Recipe, main: &[Option<ItemStack>]) -> bool {
    can_craft(recipe, &Inventory::from_slots(main.to_vec()))
}

/// Dims `color`'s alpha by `factor` (`1.0` = unchanged) -- the single place a
/// recipe row's unaffordable look is computed, so icons and labels agree.
/// Pure and unit-tested.
fn dim(color: Color, factor: f32) -> Color {
    let c = color.to_srgba();
    Color::srgba(c.red, c.green, c.blue, c.alpha * factor)
}

// ---- ECS wiring ----

/// A spawned slot square, carrying the [`SlotRef`] it addresses. One
/// component type for every section (main/container): click handling and
/// rendering only ever need the ref, never which section it came from.
#[derive(Component, Clone, Copy)]
struct SlotWidget(SlotRef);

/// The count-text child of a [`SlotWidget`].
#[derive(Component)]
struct SlotCountText;

/// The durability wear-bar child of a [`SlotWidget`] (roadmap M6). Hidden
/// unless [`SlotVisual::wear`] is `Some`.
#[derive(Component)]
struct SlotWearBar;
#[derive(Component)]
struct SlotImage;

/// The floating icon that follows the mouse while it holds
/// [`GameState::cursor`].
#[derive(Component)]
struct CursorStackIcon;
#[derive(Component)]
struct CursorStackCountText;

/// The furnace panel's cook/fuel gauge fill bars (roadmap M6), tagged onto
/// the entities [`ui::spawn_gauge`] returns so [`update_furnace_bars`] can
/// find them without walking the whole panel.
#[derive(Component)]
struct FurnaceCookFill;
#[derive(Component)]
struct FurnaceFuelFill;

/// A recipe row's click target, tagging the row (a [`Button`]) with which
/// recipe it crafts. Left click sends `Craft { all: false }`; shift-click
/// sends `all: true` -- see [`handle_recipe_clicks`].
#[derive(Component, Clone, Copy)]
struct RecipeRow(RecipeId);

/// One recipe row's image, dimmed when its ingredients are unavailable.
#[derive(Component)]
struct RecipeIcon {
    row: RecipeId,
}

/// A recipe row's count label, mirroring [`RecipeIcon`] for text color.
#[derive(Component)]
struct RecipeLabel {
    row: RecipeId,
    base_color: Color,
}

/// The recipe list's scrollable container, so [`scroll_recipe_list`] can
/// find it without walking the whole panel (mirrors [`crate::menu`]'s
/// `WorldListScroll`).
#[derive(Component)]
struct RecipeListScroll;

/// Cache of "what screen did we last build", compared against
/// [`desired_screen`] each frame by [`sync_inventory_ui`] -- mirrors
/// [`crate::pause::PauseUi`]'s pattern. Also remembers the recipe list's
/// scroll offset across rebuilds (e.g. opening/closing a chest), so it
/// doesn't always snap back to the top.
#[derive(Resource, Default)]
struct InventoryUi {
    kind: ScreenKind,
    root: Option<Entity>,
    recipe_scroll: f32,
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
                update_recipe_affordability,
                update_cursor_stack,
                update_furnace_bars,
            )
                .chain()
                .run_if(in_state(AppState::InGame)),
        )
        .add_systems(
            Update,
            (
                handle_slot_clicks,
                handle_recipe_clicks,
                handle_drop_key,
                scroll_recipe_list,
            )
                .run_if(in_state(PauseState::Inventory)),
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
/// server: dropping the cursor stack into the world is the server's job
/// (protocol docs on `CloseContainer`), not something the client can skip
/// just because no container happened to be open. Tolerates the transport
/// not existing (should not happen while this state is reachable, but
/// mirrors the defensive pattern used elsewhere).
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
#[allow(clippy::too_many_arguments)]
fn sync_inventory_ui(
    state: Res<State<PauseState>>,
    container: Res<ContainerState>,
    mut ui_state: ResMut<InventoryUi>,
    mut commands: Commands,
    font: Res<UiFont>,
    item_reg: Res<state::ItemReg>,
    recipe_reg: Res<state::RecipeReg>,
    icons: Res<ItemIcons>,
) {
    let desired = desired_screen(*state.get(), container.open.as_ref().map(|open| open.kind));
    if ui_state.kind == desired {
        return;
    }
    if let Some(root) = ui_state.root.take() {
        commands.entity(root).despawn();
    }
    let scroll = ui_state.recipe_scroll;
    ui_state.root = spawn_screen(
        &mut commands,
        &font,
        desired,
        &item_reg.0,
        &recipe_reg.0,
        &icons,
        scroll,
    );
    ui_state.kind = desired;
}

#[allow(clippy::too_many_arguments)]
fn spawn_screen(
    commands: &mut Commands,
    font: &UiFont,
    screen: ScreenKind,
    item_reg: &ItemRegistry,
    recipe_reg: &RecipeRegistry,
    icons: &ItemIcons,
    recipe_scroll: f32,
) -> Option<Entity> {
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
                ..default()
            },
            BackgroundColor(INVENTORY_PANEL_BG),
        ))
        .id();

    // Captured from inside `with_children` below (mirrors
    // `crate::health::spawn_hud`'s pattern for tagging a shared widget's
    // entities after the fact): the furnace's two gauges only exist for
    // `ScreenKind::Furnace`, so these stay `PLACEHOLDER` otherwise and the
    // tagging below is skipped.
    let mut furnace_cook_fill = Entity::PLACEHOLDER;
    let mut furnace_fuel_fill = Entity::PLACEHOLDER;

    commands.entity(panel).with_children(|parent| {
        parent.spawn((
            Text::new(title_for(screen)),
            font.text(TITLE_FONT_SIZE),
            TextColor(ui::PANEL_TEXT_COLOR),
        ));

        spawn_recipe_list(
            parent,
            font,
            item_reg,
            recipe_reg,
            icons,
            station_for(screen),
            recipe_scroll,
        );

        if screen == ScreenKind::Chest {
            spawn_grid(parent, font, icons, GRID_COLS, GRID_ROWS, container_slot);
        }
        if screen == ScreenKind::Furnace {
            let gauges = spawn_furnace(parent, font, icons);
            furnace_cook_fill = gauges.0;
            furnace_fuel_fill = gauges.1;
        }

        spawn_grid(parent, font, icons, GRID_COLS, GRID_ROWS, |r, c| {
            main_slot(backpack_index(r, c))
        });
        spawn_grid(parent, font, icons, HOTBAR_SIZE, 1, |_, c| main_slot(c));
    });

    if screen == ScreenKind::Furnace {
        commands.entity(furnace_cook_fill).insert(FurnaceCookFill);
        commands.entity(furnace_fuel_fill).insert(FurnaceFuelFill);
    }

    commands.entity(root).add_child(panel);
    spawn_cursor_stack_icon(commands, root, font, icons);
    Some(root)
}

/// Builds the furnace panel: input above fuel on the left, a cook gauge and a
/// fuel gauge in the middle (each fed live by [`update_furnace_bars`] from
/// [`state::OpenContainer::cook`]/`fuel`), and the output slot on the right --
/// so the layout reads left-to-right as input -> output, with fuel
/// underneath the input feeding it (roadmap M6). Returns the two gauges'
/// fill-bar entities so the caller can tag them for [`update_furnace_bars`]
/// to find (mirrors [`ui::spawn_gauge`]'s own `GaugeEntities` capture
/// pattern, one level up).
fn spawn_furnace(
    parent: &mut ChildSpawnerCommands<'_>,
    font: &UiFont,
    icons: &ItemIcons,
) -> (Entity, Entity) {
    let mut cook_fill = Entity::PLACEHOLDER;
    let mut fuel_fill = Entity::PLACEHOLDER;
    parent
        .spawn(Node {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: Val::Px(FURNACE_ROW_GAP_PX),
            ..default()
        })
        .with_children(|row| {
            row.spawn(Node {
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(SLOT_GAP_PX),
                ..default()
            })
            .with_children(|col| {
                spawn_slot(col, font, icons, furnace_slot(FURNACE_INPUT));
                spawn_slot(col, font, icons, furnace_slot(FURNACE_FUEL));
            });

            row.spawn(Node {
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(SLOT_GAP_PX),
                ..default()
            })
            .with_children(|col| {
                let cook = ui::spawn_gauge(
                    col,
                    font,
                    0.0,
                    "Cook",
                    "",
                    FURNACE_TRACK_COLOR,
                    FURNACE_COOK_COLOR,
                );
                cook_fill = cook.fill;
                let fuel = ui::spawn_gauge(
                    col,
                    font,
                    0.0,
                    "Fuel",
                    "",
                    FURNACE_TRACK_COLOR,
                    FURNACE_FUEL_COLOR,
                );
                fuel_fill = fuel.fill;
            });

            spawn_slot(row, font, icons, furnace_slot(FURNACE_OUTPUT));
        });
    (cook_fill, fuel_fill)
}

fn spawn_grid(
    parent: &mut ChildSpawnerCommands<'_>,
    font: &UiFont,
    icons: &ItemIcons,
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
                        spawn_slot(row_node, font, icons, slot_ref(r, c));
                    }
                });
            }
        });
}

fn spawn_slot(
    parent: &mut ChildSpawnerCommands<'_>,
    font: &UiFont,
    icons: &ItemIcons,
    slot: SlotRef,
) {
    parent
        .spawn((
            Node {
                width: Val::Px(SLOT_SIZE_PX),
                height: Val::Px(SLOT_SIZE_PX),
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
                    left: Val::Px((SLOT_SIZE_PX - ITEM_ICON_SIZE_PX) / 2.0),
                    top: Val::Px((SLOT_SIZE_PX - ITEM_ICON_SIZE_PX) / 2.0),
                    width: Val::Px(ITEM_ICON_SIZE_PX),
                    height: Val::Px(ITEM_ICON_SIZE_PX),
                    ..default()
                },
                icons.node(tsumiki_world::ItemId(0)),
                SlotImage,
            ));
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
            s.spawn((
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
                SlotWearBar,
            ));
        });
}

fn spawn_cursor_stack_icon(
    commands: &mut Commands,
    root: Entity,
    font: &UiFont,
    icons: &ItemIcons,
) {
    let icon = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                width: Val::Px(ITEM_ICON_SIZE_PX),
                height: Val::Px(ITEM_ICON_SIZE_PX),
                ..default()
            },
            icons.node(tsumiki_world::ItemId(0)),
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

/// Builds the recipe list: one row per [`RecipeRegistry::available`] entry
/// for `station`, scrollable once it outgrows [`RECIPE_LIST_MAX_HEIGHT_PX`]
/// (design.md §7: a list, not a grid -- see the module docs). Rows are
/// static once spawned (a recipe's inputs/output never change); only their
/// affordability dimming is re-evaluated live, by
/// [`update_recipe_affordability`].
fn spawn_recipe_list(
    parent: &mut ChildSpawnerCommands<'_>,
    font: &UiFont,
    item_reg: &ItemRegistry,
    recipe_reg: &RecipeRegistry,
    icons: &ItemIcons,
    station: Option<CraftingStation>,
    scroll: f32,
) {
    parent
        .spawn((
            Node {
                flex_direction: FlexDirection::Column,
                max_height: Val::Px(RECIPE_LIST_MAX_HEIGHT_PX),
                overflow: Overflow::scroll_y(),
                row_gap: Val::Px(RECIPE_ROW_GAP_PX),
                ..default()
            },
            ScrollPosition(Vec2::new(0.0, scroll)),
            RecipeListScroll,
        ))
        .with_children(|list| {
            for id in available_recipe_ids(recipe_reg, station) {
                if let Some(recipe) = recipe_reg.get(id) {
                    spawn_recipe_row(list, font, item_reg, icons, id, recipe);
                }
            }
        });
}

fn spawn_recipe_row(
    parent: &mut ChildSpawnerCommands<'_>,
    font: &UiFont,
    item_reg: &ItemRegistry,
    icons: &ItemIcons,
    id: RecipeId,
    recipe: &Recipe,
) {
    parent
        .spawn((
            Button,
            Node {
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                column_gap: Val::Px(RECIPE_ICON_GAP_PX),
                padding: UiRect::all(Val::Px(4.0)),
                ..default()
            },
            BackgroundColor(RECIPE_ROW_BG),
            RecipeRow(id),
        ))
        .with_children(|row| {
            spawn_recipe_icon(row, font, icons, id, recipe.output);
            row.spawn(Node {
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(2.0),
                ..default()
            })
            .with_children(|text| {
                text.spawn((
                    Text::new(display_name(item_reg.get(recipe.output.item).name)),
                    font.text(RECIPE_NAME_FONT_SIZE),
                    TextColor(ui::PANEL_TEXT_COLOR),
                    RecipeLabel {
                        row: id,
                        base_color: ui::PANEL_TEXT_COLOR,
                    },
                ));
                text.spawn((
                    Text::new(needs_line(item_reg, recipe)),
                    font.text(RECIPE_NEEDS_FONT_SIZE),
                    TextColor(RECIPE_NEEDS_COLOR),
                    RecipeLabel {
                        row: id,
                        base_color: RECIPE_NEEDS_COLOR,
                    },
                ));
            });
        });
}

/// Turns a registry name (`"crafting_table"`) into something to show a
/// player (`"Crafting Table"`).
///
/// Names accompany the artwork so identifying a recipe never depends on
/// memorizing icons or distinguishing their colors.
fn display_name(name: &str) -> String {
    name.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The "what this costs" line under a recipe's name, e.g. `"Needs 2 Planks"`.
fn needs_line(item_reg: &ItemRegistry, recipe: &Recipe) -> String {
    let inputs = recipe
        .inputs
        .iter()
        .map(|input| {
            format!(
                "{} {}",
                input.count,
                display_name(item_reg.get(input.item).name)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("Needs {inputs}")
}

fn spawn_recipe_icon(
    parent: &mut ChildSpawnerCommands<'_>,
    font: &UiFont,
    icons: &ItemIcons,
    row: RecipeId,
    stack: ItemStack,
) {
    parent
        .spawn((
            Node {
                width: Val::Px(RECIPE_ICON_SIZE_PX),
                height: Val::Px(RECIPE_ICON_SIZE_PX),
                ..default()
            },
            icons.node(stack.item),
            RecipeIcon { row },
        ))
        .with_children(|s| {
            s.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    right: Val::Px(2.0),
                    bottom: Val::Px(0.0),
                    ..default()
                },
                Text::new(stack.count.to_string()),
                font.text(RECIPE_COUNT_FONT_SIZE),
                TextColor(ui::PANEL_TEXT_COLOR),
                RecipeLabel {
                    row,
                    base_color: ui::PANEL_TEXT_COLOR,
                },
            ));
        });
}

/// Repaints every slot square from the last server snapshot. Runs
/// unconditionally in [`AppState::InGame`] (cheap, and harmless when no
/// screen is spawned: the query simply matches nothing).
fn update_slots(
    game_state: Res<GameState>,
    container: Res<ContainerState>,
    item_reg: Res<state::ItemReg>,
    mut slots: Query<(&SlotWidget, &mut BackgroundColor, &Children)>,
    mut texts: Query<&mut Text, With<SlotCountText>>,
    mut wear_bars: Query<(&mut Node, &mut Visibility), With<SlotWearBar>>,
    mut images: Query<&mut ImageNode, With<SlotImage>>,
) {
    for (widget, mut bg, children) in &mut slots {
        let stack = read_slot(&game_state, &container, widget.0);
        let visual = slot_visual(stack, &item_reg.0);
        *bg = BackgroundColor(visual.color);
        for &child in children {
            if let Ok(mut image) = images.get_mut(child) {
                image.rect = Some(item_icons::rect(
                    stack.map_or(tsumiki_world::ItemId(0), |s| s.item),
                ));
            }
            if let Ok(mut text) = texts.get_mut(child) {
                text.0 = visual.count_text.clone();
            }
            if let Ok((mut node, mut vis)) = wear_bars.get_mut(child) {
                match visual.wear {
                    Some(fraction) => {
                        ui::set_gauge_fill(&mut node, fraction);
                        *vis = Visibility::Inherited;
                    }
                    None => *vis = Visibility::Hidden,
                }
            }
        }
    }
}

/// Re-dims every recipe row's icons/labels from the last inventory snapshot,
/// entirely client-side ([`recipe_is_affordable`]'s doc comment). Runs
/// unconditionally in [`AppState::InGame`]; short-circuits when no recipe row
/// is spawned (menu closed, or a screen with none -- never happens today, but
/// cheap to guard).
fn update_recipe_affordability(
    game_state: Res<GameState>,
    recipe_reg: Res<state::RecipeReg>,
    mut icons: Query<(&RecipeIcon, &mut ImageNode)>,
    mut labels: Query<(&RecipeLabel, &mut TextColor)>,
) {
    if icons.is_empty() {
        return;
    }
    let affordable: Vec<bool> = recipe_reg
        .0
        .recipes()
        .iter()
        .map(|recipe| recipe_is_affordable(recipe, &game_state.main))
        .collect();
    let is_affordable = |id: RecipeId| affordable.get(id as usize).copied().unwrap_or(false);

    for (icon, mut image) in &mut icons {
        let alpha = if is_affordable(icon.row) {
            1.0
        } else {
            UNAFFORDABLE_ALPHA
        };
        image.color = dim(Color::WHITE, alpha);
    }
    for (label, mut color) in &mut labels {
        let alpha = if is_affordable(label.row) {
            1.0
        } else {
            UNAFFORDABLE_ALPHA
        };
        *color = TextColor(dim(label.base_color, alpha));
    }
}

/// Moves the cursor-stack icon to the window cursor position and repaints it
/// from [`GameState::cursor`]; hidden whenever nothing is held.
fn update_cursor_stack(
    game_state: Res<GameState>,
    item_reg: Res<state::ItemReg>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut icons: Query<(&mut Node, &mut ImageNode, &mut Visibility), With<CursorStackIcon>>,
    mut texts: Query<&mut Text, With<CursorStackCountText>>,
) {
    let Ok(window) = windows.single() else {
        return;
    };
    let cursor_pos = window.cursor_position();
    let visual = slot_visual(game_state.cursor, &item_reg.0);
    let visible = game_state.cursor.is_some() && cursor_pos.is_some();

    for (mut node, mut image, mut vis) in &mut icons {
        *vis = if visible {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
        if let Some(pos) = cursor_pos {
            node.left = Val::Px(pos.x - ITEM_ICON_SIZE_PX / 2.0);
            node.top = Val::Px(pos.y - ITEM_ICON_SIZE_PX / 2.0);
        }
        image.rect = Some(item_icons::rect(
            game_state
                .cursor
                .map_or(tsumiki_world::ItemId(0), |s| s.item),
        ));
    }
    for mut text in &mut texts {
        text.0 = visual.count_text.clone();
    }
}

/// Repaints the furnace panel's cook/fuel gauges from
/// [`state::OpenContainer::cook`]/`fuel` (roadmap M6). Runs unconditionally
/// in [`AppState::InGame`], like [`update_slots`]; harmless when no furnace
/// screen is spawned (the queries simply match nothing). Never extrapolates
/// between `FurnaceProgress` messages -- it just re-paints whatever
/// [`crate::net`] last stored, exactly like every other server-owned value
/// this screen renders.
fn update_furnace_bars(
    container: Res<ContainerState>,
    mut cook_fills: Query<&mut Node, (With<FurnaceCookFill>, Without<FurnaceFuelFill>)>,
    mut fuel_fills: Query<&mut Node, (With<FurnaceFuelFill>, Without<FurnaceCookFill>)>,
) {
    let Some(open) = container.open.as_ref() else {
        return;
    };
    for mut node in &mut cook_fills {
        ui::set_gauge_fill(&mut node, open.cook);
    }
    for mut node in &mut fuel_fills {
        ui::set_gauge_fill(&mut node, open.fuel);
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

/// Left click on a recipe row crafts it once; shift-click crafts as many as
/// materials allow (`Craft { all: true }`). Relies on `Interaction` reacting
/// only to the left mouse button (see module docs), so there is no separate
/// "which button" check here, unlike [`handle_slot_clicks`] (which also
/// handles right-click and so needs raw button state).
fn handle_recipe_clicks(
    keys: Res<ButtonInput<KeyCode>>,
    mut transport: ResMut<net::Transport>,
    rows: Query<(&Interaction, &RecipeRow), Changed<Interaction>>,
) {
    let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
    for (interaction, row) in &rows {
        if *interaction == Interaction::Pressed {
            transport.send(ClientToServer::Craft {
                recipe: row.0,
                all: shift,
            });
        }
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

/// Scrolls the recipe list with the mouse wheel, clamped to its actual
/// scrollable range and persisted into [`InventoryUi::recipe_scroll`] so
/// switching screens (e.g. opening a chest) doesn't always snap back to the
/// top -- mirrors [`crate::menu`]'s `scroll_world_list`.
fn scroll_recipe_list(
    mut wheel: MessageReader<MouseWheel>,
    mut ui_state: ResMut<InventoryUi>,
    mut lists: Query<(&mut ScrollPosition, &ComputedNode), With<RecipeListScroll>>,
) {
    let delta_y: f32 = wheel
        .read()
        .map(|ev| match ev.unit {
            MouseScrollUnit::Line => ev.y * SCROLL_LINE_HEIGHT,
            MouseScrollUnit::Pixel => ev.y,
        })
        .sum();
    if delta_y == 0.0 {
        return;
    }
    for (mut scroll, computed) in &mut lists {
        let max_offset = ((computed.content_size().y - computed.size().y)
            * computed.inverse_scale_factor())
        .max(0.0);
        scroll.y = (scroll.y - delta_y).clamp(0.0, max_offset);
        ui_state.recipe_scroll = scroll.y;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsumiki_world::MAIN_INVENTORY_SIZE;
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
        assert_eq!(
            desired_screen(PauseState::Inventory, Some(ContainerKind::Furnace)),
            ScreenKind::Furnace
        );
    }

    #[test]
    fn only_a_crafting_table_screen_grants_a_station() {
        assert_eq!(station_for(ScreenKind::Plain), None);
        assert_eq!(station_for(ScreenKind::Chest), None);
        assert_eq!(station_for(ScreenKind::Furnace), None);
        assert_eq!(
            station_for(ScreenKind::CraftingTable),
            Some(CraftingStation::CraftingTable)
        );
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
    fn furnace_slots_map_to_the_smelting_indices() {
        assert_eq!(
            furnace_slot(FURNACE_INPUT),
            SlotRef {
                area: SlotArea::Container,
                index: FURNACE_INPUT as u8
            }
        );
        assert_eq!(
            furnace_slot(FURNACE_FUEL),
            SlotRef {
                area: SlotArea::Container,
                index: FURNACE_FUEL as u8
            }
        );
        assert_eq!(
            furnace_slot(FURNACE_OUTPUT),
            SlotRef {
                area: SlotArea::Container,
                index: FURNACE_OUTPUT as u8
            }
        );
        // Three distinct slots -- a regression here would mean two of the
        // furnace's slots silently aliased the same server-side index.
        let indices: std::collections::HashSet<u8> = [FURNACE_INPUT, FURNACE_FUEL, FURNACE_OUTPUT]
            .map(|i| furnace_slot(i).index)
            .into_iter()
            .collect();
        assert_eq!(indices.len(), 3);
    }

    #[test]
    fn read_slot_reads_the_matching_snapshot_field() {
        let mut state = GameState::default();
        state.main[3] = Some(ItemStack::one(items::STICK));
        let no_container = ContainerState::default();

        assert_eq!(
            read_slot(&state, &no_container, main_slot(3)),
            Some(ItemStack::one(items::STICK))
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
                cook: 0.0,
                fuel: 0.0,
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

    #[test]
    fn wear_fraction_is_none_for_a_fresh_tool() {
        let reg = ItemRegistry::prototype();
        assert_eq!(
            wear_fraction(ItemStack::one(items::STONE_PICKAXE), &reg),
            None
        );
    }

    #[test]
    fn wear_fraction_is_none_for_a_non_tool_item() {
        let reg = ItemRegistry::prototype();
        // `with_damage` is legal on any stack even though only tools ever
        // carry a nonzero value in practice; a non-tool item must still draw
        // no bar.
        let stack = ItemStack::one(items::STICK).with_damage(5);
        assert_eq!(wear_fraction(stack, &reg), None);
    }

    #[test]
    fn wear_fraction_is_damage_over_durability() {
        let reg = ItemRegistry::prototype();
        let durability = reg.tool(items::STONE_PICKAXE).unwrap().durability;
        let half_worn = ItemStack::one(items::STONE_PICKAXE).with_damage(durability / 2);

        let fraction = wear_fraction(half_worn, &reg).expect("a damaged tool shows a bar");
        assert!((fraction - 0.5).abs() < 0.01);
    }

    #[test]
    fn wear_fraction_shows_no_bar_at_zero_damage() {
        let reg = ItemRegistry::prototype();
        let fresh = ItemStack::one(items::IRON_PICKAXE).with_damage(0);
        assert_eq!(wear_fraction(fresh, &reg), None);
    }

    #[test]
    fn wear_fraction_is_full_at_max_durability() {
        let reg = ItemRegistry::prototype();
        let durability = reg.tool(items::IRON_PICKAXE).unwrap().durability;
        let worn_out = ItemStack::one(items::IRON_PICKAXE).with_damage(durability);

        assert_eq!(wear_fraction(worn_out, &reg), Some(1.0));
    }

    #[test]
    fn slot_visual_carries_the_wear_fraction_through() {
        let reg = ItemRegistry::prototype();
        let durability = reg.tool(items::WOODEN_AXE).unwrap().durability;
        let stack = ItemStack::one(items::WOODEN_AXE).with_damage(durability);

        assert_eq!(slot_visual(Some(stack), &reg).wear, Some(1.0));
        assert_eq!(
            slot_visual(Some(ItemStack::one(items::WOODEN_AXE)), &reg).wear,
            None
        );
    }

    #[test]
    fn available_recipe_ids_grow_with_a_crafting_table() {
        let reg = RecipeRegistry::prototype();
        let hand = available_recipe_ids(&reg, None);
        let table = available_recipe_ids(&reg, Some(CraftingStation::CraftingTable));

        assert!(hand.iter().all(|id| table.contains(id)));
        assert!(
            table.len() > hand.len(),
            "a crafting table must unlock something the recipe list can show"
        );
    }

    #[test]
    fn available_recipe_ids_matches_the_registry_order() {
        let reg = RecipeRegistry::prototype();
        let expected: Vec<RecipeId> = reg.available(None).map(|(id, _)| id).collect();
        assert_eq!(available_recipe_ids(&reg, None), expected);
    }

    #[test]
    fn recipe_is_affordable_reflects_current_materials() {
        let reg = RecipeRegistry::prototype();
        // Recipe 0 in the prototype set: one log -> four planks.
        let planks_recipe = &reg.recipes()[0];
        assert_eq!(planks_recipe.output.item, items::PLANKS);

        let empty: Vec<Option<ItemStack>> = vec![None; MAIN_INVENTORY_SIZE];
        assert!(!recipe_is_affordable(planks_recipe, &empty));

        let mut with_log = empty.clone();
        with_log[0] = Some(ItemStack::one(items::LOG));
        assert!(recipe_is_affordable(planks_recipe, &with_log));
    }

    #[test]
    fn recipe_is_affordable_is_false_with_only_a_partial_stack() {
        let reg = RecipeRegistry::prototype();
        // Recipe 1: two planks -> four sticks.
        let sticks_recipe = &reg.recipes()[1];
        assert_eq!(sticks_recipe.output.item, items::STICK);

        let mut main: Vec<Option<ItemStack>> = vec![None; MAIN_INVENTORY_SIZE];
        main[0] = Some(ItemStack::one(items::PLANKS));
        assert!(!recipe_is_affordable(sticks_recipe, &main));

        main[0] = Some(ItemStack::new(items::PLANKS, 2));
        assert!(recipe_is_affordable(sticks_recipe, &main));
    }

    #[test]
    fn dim_scales_alpha_and_leaves_color_untouched() {
        let color = Color::srgba(0.5, 0.25, 0.75, 1.0);
        let dimmed = dim(color, 0.5).to_srgba();
        assert_eq!(dimmed.red, 0.5);
        assert_eq!(dimmed.green, 0.25);
        assert_eq!(dimmed.blue, 0.75);
        assert_eq!(dimmed.alpha, 0.5);
    }

    #[test]
    fn dim_with_full_factor_is_unchanged() {
        let color = Color::srgba(0.1, 0.2, 0.3, 0.9);
        assert_eq!(dim(color, 1.0).to_srgba().alpha, 0.9);
    }
}

#[cfg(test)]
mod recipe_text_tests {
    use super::*;
    use tsumiki_world::items;

    #[test]
    fn display_name_spells_out_registry_names() {
        assert_eq!(display_name("planks"), "Planks");
        assert_eq!(display_name("crafting_table"), "Crafting Table");
        assert_eq!(display_name(""), "");
    }

    #[test]
    fn needs_line_lists_every_input_with_its_count() {
        let item_reg = ItemRegistry::prototype();
        let recipe = Recipe {
            inputs: vec![
                ItemStack::new(items::PLANKS, 2),
                ItemStack::one(items::STICK),
            ],
            output: ItemStack::one(items::CHEST),
            station: None,
        };

        assert_eq!(needs_line(&item_reg, &recipe), "Needs 2 Planks, 1 Stick");
    }

    #[test]
    fn every_prototype_recipe_has_a_readable_name_and_cost() {
        let item_reg = ItemRegistry::prototype();
        for recipe in RecipeRegistry::prototype().recipes() {
            let name = display_name(item_reg.get(recipe.output.item).name);
            assert!(!name.is_empty());
            assert!(!name.contains('_'), "{name} still looks like an id");
            assert!(needs_line(&item_reg, recipe).starts_with("Needs "));
        }
    }
}

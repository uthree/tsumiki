//! Session-wide survival state (roadmap.md M4/M5): the world's game mode
//! (fixed for the session, announced by `Welcome`), the local player's
//! health/inventory/time, and the currently-open container, kept in sync by
//! [`crate::net`]'s receive loop.
//!
//! All resources here are always present -- inserted unconditionally at
//! `run_client` time via [`install`], not gated to [`AppState::InGame`] -- so
//! HUD and gameplay systems that read them can never panic before a
//! `Welcome` arrives (e.g. [`crate::screenshot`]'s ephemeral creative world
//! never needs to special-case "not connected yet"; it just sees the
//! defaults below until the real `Welcome` lands).

use bevy::prelude::*;
use tsumiki_protocol::{ContainerKind, GameMode as ProtoGameMode, MAX_HP};
use tsumiki_world::{ItemRegistry, ItemStack, MAIN_INVENTORY_SIZE, RecipeRegistry};

use crate::AppState;

/// The world's fixed game mode, as announced by `Welcome`. Defaults to
/// [`ProtoGameMode::Creative`] until `Welcome` arrives, so survival-only HUD
/// and gameplay logic simply stays off pre-connect instead of needing a
/// separate "unknown" state.
#[derive(Resource, Clone, Copy, PartialEq, Eq, Debug)]
pub struct GameMode(pub ProtoGameMode);

impl Default for GameMode {
    fn default() -> Self {
        Self(ProtoGameMode::Creative)
    }
}

impl GameMode {
    pub fn is_survival(&self) -> bool {
        self.0 == ProtoGameMode::Survival
    }
}

/// The local player's survival state: health, death, the last inventory
/// snapshot the server sent, and the (locally-advanced, server-resynced)
/// time of day. See `net.rs` for how each field is updated from the server,
/// and `daynight.rs` for how `time_of_day` is advanced locally between
/// resyncs.
///
/// The inventory fields mirror `ServerToClient::InventoryUpdate` exactly and
/// are never mutated locally except by that message: the server owns every
/// inventory (design.md §7), so the client only ever renders the last
/// snapshot it received (see [`crate::inventory`]).
#[derive(Resource, Clone, Debug)]
pub struct GameState {
    pub hp: u16,
    /// Mirrors `hp == 0`; set on `Died` and re-derived on every
    /// `HealthUpdate` (see `net.rs`), so the death overlay
    /// ([`crate::death`]) closes the moment the post-respawn `HealthUpdate`
    /// arrives.
    pub dead: bool,
    /// [`MAIN_INVENTORY_SIZE`] entries; `0..HOTBAR_SIZE` is the hotbar
    /// ([`crate::hotbar`]), the rest is the backpack.
    pub main: Vec<Option<ItemStack>>,
    /// The stack held by the mouse cursor while the inventory screen is
    /// open, if any.
    pub cursor: Option<ItemStack>,
    /// Current time of day in `[0, 1)`; see `ServerToClient::Welcome`'s docs
    /// for the sunrise/noon/sunset convention.
    pub time_of_day: f32,
}

impl Default for GameState {
    fn default() -> Self {
        Self {
            hp: MAX_HP,
            dead: false,
            main: vec![None; MAIN_INVENTORY_SIZE],
            cursor: None,
            time_of_day: 0.25,
        }
    }
}

/// A container UI the server has granted access to (roadmap M5): a chest's
/// own slots, or a crafting table (no slots of its own -- it only unlocks the
/// recipes that need one, see [`RecipeReg`]). Kept separate from
/// [`GameState`] since it comes and goes independently of the player's own
/// inventory.
#[derive(Clone, Debug)]
pub struct OpenContainer {
    pub kind: ContainerKind,
    pub pos: IVec3,
    pub slots: Vec<Option<ItemStack>>,
}

/// The currently-open container, if any. Updated by [`crate::net`] from
/// `ContainerOpened`/`ContainerUpdate`/`ContainerClosed`, and read by
/// [`crate::inventory`] to decide what screen to show.
#[derive(Resource, Clone, Debug, Default)]
pub struct ContainerState {
    pub open: Option<OpenContainer>,
}

/// The client's item catalog, wrapped as a Bevy resource (mirrors
/// [`crate::view::Registry`] for blocks; the `world` crate itself stays free
/// of ECS dependencies). Used by [`crate::hotbar`], [`crate::interact`] and
/// [`crate::inventory`] for placeholder colors, stack limits and the
/// item-places-block mapping.
#[derive(Resource)]
pub struct ItemReg(pub ItemRegistry);

/// The client's recipe catalog, mirroring [`ItemReg`]. The server never
/// sends which recipes are craftable or even reachable ([`ContainerKind`]'s
/// doc comment) -- the client holds the same registry and derives both
/// itself (see [`crate::inventory`]'s recipe list).
#[derive(Resource)]
pub struct RecipeReg(pub RecipeRegistry);

/// Run condition: `true` unless the local player is dead. Gates
/// movement/interact/hotbar input off while the death overlay
/// ([`crate::death`]) is up.
pub fn is_alive(state: Res<GameState>) -> bool {
    !state.dead
}

fn reset_state(
    mut mode: ResMut<GameMode>,
    mut state: ResMut<GameState>,
    mut container: ResMut<ContainerState>,
) {
    *mode = GameMode::default();
    *state = GameState::default();
    *container = ContainerState::default();
}

/// Installs the always-present [`GameMode`]/[`GameState`]/[`ContainerState`]
/// resources and the fixed [`ItemReg`]/[`RecipeReg`] catalogs, and resets the
/// per-session resources on leaving the world, so a fresh session never
/// inherits a previous one's mode/health/inventory/open container.
pub fn install(app: &mut App, item_registry: ItemRegistry, recipe_registry: RecipeRegistry) {
    app.init_resource::<GameMode>()
        .init_resource::<GameState>()
        .init_resource::<ContainerState>()
        .insert_resource(ItemReg(item_registry))
        .insert_resource(RecipeReg(recipe_registry))
        .add_systems(OnExit(AppState::InGame), reset_state);
}

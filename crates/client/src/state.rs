//! Session-wide survival state (roadmap.md M4): the world's game mode
//! (fixed for the session, announced by `Welcome`) and the local player's
//! health/inventory/time, kept in sync by [`crate::net`]'s receive loop.
//!
//! Both resources are always present — inserted unconditionally at
//! `run_client` time via [`install`], not gated to [`AppState::InGame`] — so
//! HUD and gameplay systems that read them can never panic before a
//! `Welcome` arrives (e.g. [`crate::screenshot`]'s ephemeral creative world
//! never needs to special-case "not connected yet"; it just sees the
//! defaults below until the real `Welcome` lands).

use std::collections::HashMap;

use bevy::prelude::*;
use tsumiki_protocol::{GameMode as ProtoGameMode, MAX_HP};
use tsumiki_world::BlockId;

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

/// The local player's survival state: health, death, inventory counts and
/// the (locally-advanced, server-resynced) time of day. See `net.rs` for how
/// each field is updated from the server, and `daynight.rs` for how
/// `time_of_day` is advanced locally between resyncs.
#[derive(Resource, Clone, Debug)]
pub struct GameState {
    pub hp: u16,
    /// Mirrors `hp == 0`; set on `Died` and re-derived on every
    /// `HealthUpdate` (see `net.rs`), so the death overlay
    /// ([`crate::death`]) closes the moment the post-respawn `HealthUpdate`
    /// arrives.
    pub dead: bool,
    pub inventory: HashMap<BlockId, u32>,
    /// Current time of day in `[0, 1)`; see `ServerToClient::Welcome`'s docs
    /// for the sunrise/noon/sunset convention.
    pub time_of_day: f32,
}

impl Default for GameState {
    fn default() -> Self {
        Self {
            hp: MAX_HP,
            dead: false,
            inventory: HashMap::new(),
            time_of_day: 0.25,
        }
    }
}

impl GameState {
    pub fn inventory_count(&self, block: BlockId) -> u32 {
        self.inventory.get(&block).copied().unwrap_or(0)
    }
}

/// Run condition: `true` unless the local player is dead. Gates
/// movement/interact/hotbar input off while the death overlay
/// ([`crate::death`]) is up.
pub fn is_alive(state: Res<GameState>) -> bool {
    !state.dead
}

fn reset_state(mut mode: ResMut<GameMode>, mut state: ResMut<GameState>) {
    *mode = GameMode::default();
    *state = GameState::default();
}

/// Installs the always-present [`GameMode`]/[`GameState`] resources and
/// resets them on leaving the world, so a fresh session never inherits a
/// previous one's mode/health/inventory.
pub fn install(app: &mut App) {
    app.init_resource::<GameMode>()
        .init_resource::<GameState>()
        .add_systems(OnExit(AppState::InGame), reset_state);
}

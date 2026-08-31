//! Screenshot-and-exit mode for automated verification.
//!
//! Six modes, chosen by [`crate::ScreenshotTarget`] (active only when
//! `ClientOptions::screenshot` is set):
//! - World (the default): wait until the initial view is settled (no chunk
//!   currently ready to mesh and at least some chunks meshed) or a ~45s hard
//!   timeout expires, then capture.
//! - Menu: stay in the title menu and just wait a fixed ~3s so the
//!   decorative scene has a couple of rotation frames in it, then capture.
//! - WorldSelect/CreateWorld: wait ~2s, then drive the menu into that panel
//!   automatically -- via [`crate::menu::MenuScreenshotNav`], exactly as if
//!   Singleplayer (and, for `CreateWorld`, "Create New World") had been
//!   clicked -- and wait ~1s more before capturing, so both screens can be
//!   verified without a human at the keyboard.
//! - Pause ([`crate::StartMode::Direct`] only): wait for the same in-world
//!   settle condition as World, then open the pause menu and wait ~1s more
//!   before capturing, so the orchestrator can verify the pause UI visually.
//! - Inventory ([`crate::StartMode::Direct`] only): wait for the same
//!   in-world settle condition, populate the local inventory snapshot with a
//!   few sample stacks (see [`sample_game_state`] — a verification-only
//!   fixture; the client otherwise never mutates its own inventory, roadmap
//!   M5), open the inventory screen and wait ~1s more before capturing, so
//!   the orchestrator can verify the inventory UI visually with non-empty
//!   slots.
//!
//! Either way, trigger Bevy's screenshot capture to the configured path and
//! exit the app once it is saved.
//!
//! The pause- and inventory-capture modes open their screen by setting
//! [`PauseState`] directly, exactly like a real Escape/`E` press would — see
//! [`crate::pause`]/[`crate::inventory`]'s docs on why that alone is enough
//! to spawn the real UI and release the cursor, with nothing
//! screenshot-specific in either module itself.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk};
use tsumiki_world::{CHUNK_SIZE, ItemStack, MAIN_INVENTORY_SIZE, items};

use crate::ScreenshotTarget;
use crate::camera::{Player, PlayerMode};
use crate::lod_view::{self, LodStore};
use crate::menu::MenuScreenshotNav;
use crate::pause::PauseState;
use crate::settings::Settings;
use crate::state::GameState;
use crate::view::{ChunkStore, any_chunk_ready};

/// Extra frames to wait once the view looks settled, so the newly spawned
/// chunk meshes have actually been rendered at least once.
const SETTLE_FRAMES: u32 = 20;

/// Minimum number of chunks that must have been processed by the mesher
/// (meshed to something or to nothing, if all-air) before the view counts
/// as settled.
const MIN_MESHED_CHUNKS: usize = 50;

/// Minimum number of LOD chunks that must have been meshed before the view
/// counts as settled — or the full LOD wanted set, whichever is smaller (a
/// small view distance may want fewer than this many LOD chunks in total).
const MIN_MESHED_LOD_CHUNKS: usize = 150;

/// Hard cap: capture and exit regardless of view state past this point.
const HARD_TIMEOUT: Duration = Duration::from_secs(45);

/// Number of recent per-frame durations kept to compute the average FPS
/// reported just before capture.
const FPS_WINDOW: usize = 60;

/// Screenshot-mode-only camera override, applied once spawn resolves: drop
/// into Fly mode well above the terrain with a downward-ish pitch so the LOD
/// horizon is visible in the capture (the orchestrator verifies LOD
/// visually from this).
const CAPTURE_FEET_Y: f32 = 110.0;
const CAPTURE_PITCH: f32 = -0.2;

/// Fixed delay before capturing in menu-screenshot mode.
const MENU_CAPTURE_DELAY: Duration = Duration::from_secs(3);

/// Extra wait after opening the pause menu (pause-screenshot mode only), so
/// the overlay/panel have actually been rendered at least once.
const PAUSE_CAPTURE_DELAY: Duration = Duration::from_secs(1);

/// Extra wait after opening the inventory screen (inventory-screenshot mode
/// only), mirroring [`PAUSE_CAPTURE_DELAY`].
const INVENTORY_CAPTURE_DELAY: Duration = Duration::from_secs(1);

/// Delay before requesting the world-select/create-world menu navigation
/// (`ScreenshotTarget::WorldSelect`/`CreateWorld`), mirroring
/// [`MENU_CAPTURE_DELAY`]'s "let the scene settle first" reasoning.
const MENU_NAV_DELAY: Duration = Duration::from_secs(2);

/// Extra wait after requesting the menu navigation, so the target panel has
/// actually been spawned and rendered at least once.
const MENU_NAV_CAPTURE_DELAY: Duration = Duration::from_secs(1);

#[derive(Resource)]
struct ScreenshotConfig {
    path: PathBuf,
    /// Which screen to capture. See [`ScreenshotTarget`].
    target: ScreenshotTarget,
}

#[derive(Resource)]
struct ScreenshotState {
    started_at: Instant,
    settled_frames: u32,
    triggered: bool,
    /// Set once the pause menu has been requested (pause-screenshot mode
    /// only); the post-pause delay is measured from this.
    paused_at: Option<Instant>,
    /// Set once the inventory screen has been requested
    /// (inventory-screenshot mode only); the post-open delay is measured
    /// from this.
    inventory_opened_at: Option<Instant>,
    /// Set once [`MenuScreenshotNav`] has been requested (`WorldSelect`/
    /// `CreateWorld` targets only); the post-navigation delay is measured
    /// from this.
    menu_nav_requested_at: Option<Instant>,
    /// `true` once [`position_camera_for_capture`] has applied its one-time
    /// override.
    positioned_for_capture: bool,
    /// The last [`FPS_WINDOW`] frame durations, for the `fps_avg` report.
    recent_frame_secs: VecDeque<f32>,
}

impl Default for ScreenshotState {
    fn default() -> Self {
        Self {
            started_at: Instant::now(),
            settled_frames: 0,
            triggered: false,
            paused_at: None,
            inventory_opened_at: None,
            menu_nav_requested_at: None,
            positioned_for_capture: false,
            recent_frame_secs: VecDeque::with_capacity(FPS_WINDOW),
        }
    }
}

impl ScreenshotState {
    fn record_frame(&mut self, dt: f32) {
        if self.recent_frame_secs.len() == FPS_WINDOW {
            self.recent_frame_secs.pop_front();
        }
        self.recent_frame_secs.push_back(dt);
    }

    /// Average FPS over the recorded window, computed from `Time` deltas
    /// (no diagnostics plugin needed). `0.0` if nothing has been recorded
    /// yet, or the window's average delta is degenerate.
    fn avg_fps(&self) -> f32 {
        if self.recent_frame_secs.is_empty() {
            return 0.0;
        }
        let avg_dt: f32 =
            self.recent_frame_secs.iter().sum::<f32>() / self.recent_frame_secs.len() as f32;
        if avg_dt > 0.0 { 1.0 / avg_dt } else { 0.0 }
    }
}

/// Populates [`GameState`] with a handful of sample stacks so the inventory
/// screen has something to show a screenshot orchestrator -- a fixture for
/// [`ClientOptions::inventory_screenshot`] only. This is the one place the
/// client is allowed to write its own inventory snapshot: everywhere else
/// (roadmap M5, design.md §7) it is strictly server-owned.
fn sample_game_state(state: &mut GameState) {
    state.main = vec![None; MAIN_INVENTORY_SIZE];
    state.main[0] = Some(ItemStack::new(items::STONE, 42));
    state.main[1] = Some(ItemStack::one(items::LOG));
    state.main[2] = Some(ItemStack::new(items::PLANKS, 64));
    state.main[9] = Some(ItemStack::new(items::DIRT, 12));
    state.main[10] = Some(ItemStack::one(items::CRAFTING_TABLE));
    state.main[11] = Some(ItemStack::new(items::CHEST, 3));

    state.cursor = Some(ItemStack::new(items::STICK, 7));
}

/// Wires the screenshot-and-exit watcher into `app`. See the module docs for
/// the six modes.
pub fn install(app: &mut App, path: PathBuf, target: ScreenshotTarget) {
    app.insert_resource(ScreenshotConfig { path, target })
        .init_resource::<ScreenshotState>()
        .add_systems(
            Update,
            (position_camera_for_capture, watch_and_capture).chain(),
        );
}

/// Screenshot-mode-only: once spawn resolves, drops the player into Fly mode
/// well above the terrain with a downward-ish pitch, so the capture shows
/// the LOD horizon. A no-op in menu-screenshot mode (never enters the
/// world) and once already applied.
fn position_camera_for_capture(
    config: Res<ScreenshotConfig>,
    mut state: ResMut<ScreenshotState>,
    mut players: Query<&mut Player>,
) {
    if config.target.is_menu() || state.positioned_for_capture {
        return;
    }
    let Ok(mut player) = players.single_mut() else {
        return;
    };
    if !player.spawned {
        return;
    }
    player.mode = PlayerMode::Fly;
    player.feet.y = CAPTURE_FEET_Y;
    player.pitch = CAPTURE_PITCH;
    state.positioned_for_capture = true;
}

#[allow(clippy::too_many_arguments)]
fn watch_and_capture(
    mut commands: Commands,
    time: Res<Time>,
    config: Res<ScreenshotConfig>,
    mut state: ResMut<ScreenshotState>,
    store: Res<ChunkStore>,
    lod_store: Res<LodStore>,
    settings: Res<Settings>,
    cameras: Query<&Transform, With<Player>>,
    mut next_pause: ResMut<NextState<PauseState>>,
    mut game_state: ResMut<GameState>,
) {
    if state.triggered {
        return;
    }
    state.record_frame(time.delta_secs());

    let elapsed = state.started_at.elapsed();
    let timed_out = elapsed >= HARD_TIMEOUT;

    if config.target.is_menu() {
        match config.target {
            ScreenshotTarget::Menu => {
                if timed_out || elapsed >= MENU_CAPTURE_DELAY {
                    trigger_capture(&mut commands, config.path.clone(), &mut state);
                }
            }
            ScreenshotTarget::WorldSelect | ScreenshotTarget::CreateWorld => {
                match state.menu_nav_requested_at {
                    None if elapsed >= MENU_NAV_DELAY => {
                        commands.insert_resource(MenuScreenshotNav(config.target));
                        state.menu_nav_requested_at = Some(Instant::now());
                    }
                    Some(requested_at) if requested_at.elapsed() >= MENU_NAV_CAPTURE_DELAY => {
                        trigger_capture(&mut commands, config.path.clone(), &mut state);
                    }
                    _ => {}
                }
                if !state.triggered && timed_out {
                    trigger_capture(&mut commands, config.path.clone(), &mut state);
                }
            }
            ScreenshotTarget::World | ScreenshotTarget::Pause | ScreenshotTarget::Inventory => {
                unreachable!("is_menu() only returns true for Menu/WorldSelect/CreateWorld")
            }
        }
        return;
    }

    let lod_settled = {
        let camera_xz = cameras
            .single()
            .map(|t| Vec2::new(t.translation.x, t.translation.z))
            .unwrap_or(Vec2::ZERO);
        let vd_blocks = settings.view_distance_chunks * CHUNK_SIZE as i32;
        let wanted_count = lod_view::wanted_lod_chunks(camera_xz, vd_blocks).len();
        let target = wanted_count.min(MIN_MESHED_LOD_CHUNKS);
        lod_store.meshed.len() >= target
    };

    let settled =
        !any_chunk_ready(&store) && store.meshed.len() >= MIN_MESHED_CHUNKS && lod_settled;
    state.settled_frames = if settled { state.settled_frames + 1 } else { 0 };
    let world_ready = state.settled_frames >= SETTLE_FRAMES;

    match config.target {
        ScreenshotTarget::Inventory => match state.inventory_opened_at {
            None if world_ready => {
                sample_game_state(&mut game_state);
                next_pause.set(PauseState::Inventory);
                state.inventory_opened_at = Some(Instant::now());
            }
            Some(opened_at) if opened_at.elapsed() >= INVENTORY_CAPTURE_DELAY => {
                trigger_capture(&mut commands, config.path.clone(), &mut state);
            }
            _ => {}
        },
        ScreenshotTarget::Pause => match state.paused_at {
            None if world_ready => {
                next_pause.set(PauseState::Paused);
                state.paused_at = Some(Instant::now());
            }
            Some(paused_at) if paused_at.elapsed() >= PAUSE_CAPTURE_DELAY => {
                trigger_capture(&mut commands, config.path.clone(), &mut state);
            }
            _ => {}
        },
        ScreenshotTarget::World => {
            if world_ready {
                trigger_capture(&mut commands, config.path.clone(), &mut state);
            }
        }
        ScreenshotTarget::Menu | ScreenshotTarget::WorldSelect | ScreenshotTarget::CreateWorld => {
            unreachable!("menu targets are handled above via is_menu()")
        }
    }

    if !state.triggered && timed_out {
        trigger_capture(&mut commands, config.path.clone(), &mut state);
    }
}

fn trigger_capture(commands: &mut Commands, path: PathBuf, state: &mut ScreenshotState) {
    state.triggered = true;
    eprintln!("fps_avg={:.1}", state.avg_fps());
    commands
        .spawn(Screenshot::primary_window())
        .observe(save_to_disk(path))
        .observe(exit_after_capture);
}

fn exit_after_capture(_capture: On<ScreenshotCaptured>, mut exit: MessageWriter<AppExit>) {
    exit.write(AppExit::Success);
}

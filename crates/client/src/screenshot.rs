//! Screenshot-and-exit mode for automated verification.
//!
//! Three modes, chosen by `ClientOptions::menu_screenshot`/`pause_screenshot`
//! (active only when `ClientOptions::screenshot` is set):
//! - In-world (both `false`, the default): wait until the initial view is
//!   settled (no chunk currently ready to mesh and at least some chunks
//!   meshed) or a ~45s hard timeout expires, then capture.
//! - Menu (`menu_screenshot`): stay in the title menu and just wait a fixed
//!   ~3s so the decorative scene has a couple of rotation frames in it, then
//!   capture.
//! - Pause (`pause_screenshot`, [`crate::StartMode::Direct`] only): wait for the
//!   same in-world settle condition as the plain in-world mode, then open
//!   the pause menu and wait ~1s more before capturing, so the orchestrator
//!   can verify the pause UI visually.
//!
//! Either way, trigger Bevy's screenshot capture to the configured path and
//! exit the app once it is saved.
//!
//! The pause-capture mode opens the pause menu by setting
//! [`PauseState`] directly, exactly like a real Escape press would — see
//! [`crate::pause`]'s docs on why that alone is enough to spawn the real
//! pause UI and release the cursor, with nothing screenshot-specific in
//! `pause.rs` itself.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk};

use crate::pause::PauseState;
use crate::view::{ChunkStore, any_chunk_ready};

/// Extra frames to wait once the view looks settled, so the newly spawned
/// chunk meshes have actually been rendered at least once.
const SETTLE_FRAMES: u32 = 20;

/// Minimum number of chunks that must have been processed by the mesher
/// (meshed to something or to nothing, if all-air) before the view counts
/// as settled.
const MIN_MESHED_CHUNKS: usize = 50;

/// Hard cap: capture and exit regardless of view state past this point.
const HARD_TIMEOUT: Duration = Duration::from_secs(45);

/// Fixed delay before capturing in menu-screenshot mode.
const MENU_CAPTURE_DELAY: Duration = Duration::from_secs(3);

/// Extra wait after opening the pause menu (pause-screenshot mode only), so
/// the overlay/panel have actually been rendered at least once.
const PAUSE_CAPTURE_DELAY: Duration = Duration::from_secs(1);

#[derive(Resource)]
struct ScreenshotConfig {
    path: PathBuf,
    /// Capture the title menu (fixed delay) instead of the in-world view
    /// (settle detection). See [`crate::ClientOptions::menu_screenshot`].
    menu_screenshot: bool,
    /// Capture the pause menu after the world settles, instead of the plain
    /// in-world view. See [`crate::ClientOptions::pause_screenshot`].
    pause_screenshot: bool,
}

#[derive(Resource)]
struct ScreenshotState {
    started_at: Instant,
    settled_frames: u32,
    triggered: bool,
    /// Set once the pause menu has been requested (pause-screenshot mode
    /// only); the post-pause delay is measured from this.
    paused_at: Option<Instant>,
}

impl Default for ScreenshotState {
    fn default() -> Self {
        Self {
            started_at: Instant::now(),
            settled_frames: 0,
            triggered: false,
            paused_at: None,
        }
    }
}

/// Wires the screenshot-and-exit watcher into `app`. See the module docs for
/// the three modes.
pub fn install(app: &mut App, path: PathBuf, menu_screenshot: bool, pause_screenshot: bool) {
    app.insert_resource(ScreenshotConfig {
        path,
        menu_screenshot,
        pause_screenshot,
    })
    .init_resource::<ScreenshotState>()
    .add_systems(Update, watch_and_capture);
}

fn watch_and_capture(
    mut commands: Commands,
    config: Res<ScreenshotConfig>,
    mut state: ResMut<ScreenshotState>,
    store: Res<ChunkStore>,
    mut next_pause: ResMut<NextState<PauseState>>,
) {
    if state.triggered {
        return;
    }

    let elapsed = state.started_at.elapsed();
    let timed_out = elapsed >= HARD_TIMEOUT;

    if config.menu_screenshot {
        if timed_out || elapsed >= MENU_CAPTURE_DELAY {
            trigger_capture(&mut commands, config.path.clone(), &mut state);
        }
        return;
    }

    let settled = !any_chunk_ready(&store) && store.meshed.len() >= MIN_MESHED_CHUNKS;
    state.settled_frames = if settled { state.settled_frames + 1 } else { 0 };
    let world_ready = state.settled_frames >= SETTLE_FRAMES;

    if config.pause_screenshot {
        match state.paused_at {
            None if world_ready => {
                next_pause.set(PauseState::Paused);
                state.paused_at = Some(Instant::now());
            }
            Some(paused_at) if paused_at.elapsed() >= PAUSE_CAPTURE_DELAY => {
                trigger_capture(&mut commands, config.path.clone(), &mut state);
            }
            _ => {}
        }
    } else if world_ready {
        trigger_capture(&mut commands, config.path.clone(), &mut state);
    }

    if !state.triggered && timed_out {
        trigger_capture(&mut commands, config.path.clone(), &mut state);
    }
}

fn trigger_capture(commands: &mut Commands, path: PathBuf, state: &mut ScreenshotState) {
    state.triggered = true;
    commands
        .spawn(Screenshot::primary_window())
        .observe(save_to_disk(path))
        .observe(exit_after_capture);
}

fn exit_after_capture(_capture: On<ScreenshotCaptured>, mut exit: MessageWriter<AppExit>) {
    exit.write(AppExit::Success);
}

//! Screenshot-and-exit mode for automated verification.
//!
//! Responsibilities (implemented by the client agent):
//! - Active only when `ClientOptions::screenshot` is set.
//! - Two modes, chosen by `ClientOptions::menu_screenshot`:
//!   - In-world (`false`, the default): wait until the initial view is
//!     settled (no chunk currently ready to mesh and at least some chunks
//!     meshed) or a ~45s hard timeout expires.
//!   - Menu (`true`): stay in the title menu and just wait a fixed ~3s so the
//!     decorative scene has a couple of rotation frames in it.
//! - Either way, trigger Bevy's screenshot capture to the configured path and
//!   exit the app once it is saved.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk};

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

#[derive(Resource)]
struct ScreenshotConfig {
    path: PathBuf,
    /// Capture the title menu (fixed delay) instead of the in-world view
    /// (settle detection). See [`crate::ClientOptions::menu_screenshot`].
    menu_screenshot: bool,
}

#[derive(Resource)]
struct ScreenshotState {
    started_at: Instant,
    settled_frames: u32,
    triggered: bool,
}

impl Default for ScreenshotState {
    fn default() -> Self {
        Self {
            started_at: Instant::now(),
            settled_frames: 0,
            triggered: false,
        }
    }
}

/// Wires the screenshot-and-exit watcher into `app`, saving to `path` once
/// the view settles (or the hard timeout elapses), or, in menu-screenshot
/// mode, once the fixed delay elapses.
pub fn install(app: &mut App, path: PathBuf, menu_screenshot: bool) {
    app.insert_resource(ScreenshotConfig {
        path,
        menu_screenshot,
    })
    .init_resource::<ScreenshotState>()
    .add_systems(Update, watch_and_capture);
}

fn watch_and_capture(
    mut commands: Commands,
    config: Res<ScreenshotConfig>,
    mut state: ResMut<ScreenshotState>,
    store: Res<ChunkStore>,
) {
    if state.triggered {
        return;
    }

    let elapsed = state.started_at.elapsed();
    let ready = if config.menu_screenshot {
        elapsed >= MENU_CAPTURE_DELAY
    } else {
        let settled = !any_chunk_ready(&store) && store.meshed.len() >= MIN_MESHED_CHUNKS;
        state.settled_frames = if settled { state.settled_frames + 1 } else { 0 };
        state.settled_frames >= SETTLE_FRAMES
    };

    let timed_out = elapsed >= HARD_TIMEOUT;
    if timed_out || ready {
        state.triggered = true;
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(config.path.clone()))
            .observe(exit_after_capture);
    }
}

fn exit_after_capture(_capture: On<ScreenshotCaptured>, mut exit: MessageWriter<AppExit>) {
    exit.write(AppExit::Success);
}

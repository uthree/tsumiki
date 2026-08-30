//! Screenshot-and-exit mode for automated verification.
//!
//! Responsibilities (implemented by the client agent):
//! - Active only when `ClientOptions::screenshot` is set.
//! - Wait until the initial view is settled (no chunk currently ready to
//!   mesh and at least some chunks meshed) or a ~30s timeout expires, then
//!   trigger Bevy's screenshot capture to the configured path and exit the
//!   app once it is saved.

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

#[derive(Resource)]
struct ScreenshotConfig {
    path: PathBuf,
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
/// the view settles (or the hard timeout elapses).
pub fn install(app: &mut App, path: PathBuf) {
    app.insert_resource(ScreenshotConfig { path })
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

    let settled = !any_chunk_ready(&store) && store.meshed.len() >= MIN_MESHED_CHUNKS;
    state.settled_frames = if settled { state.settled_frames + 1 } else { 0 };

    let timed_out = state.started_at.elapsed() >= HARD_TIMEOUT;
    if timed_out || state.settled_frames >= SETTLE_FRAMES {
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

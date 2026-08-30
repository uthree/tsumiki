//! Game client: rendering, input, UI (design.md §1).
//!
//! Talks to the server exclusively through a [`ClientTransport`]; holds a
//! mirror cache of received chunks and renders them.

pub mod camera;
pub mod hotbar;
pub mod interact;
pub mod mesh;
pub mod net;
pub mod screenshot;
pub mod view;

use std::path::PathBuf;

use bevy::prelude::*;
use bevy::window::WindowPlugin;
use tsumiki_protocol::ClientTransport;
use tsumiki_world::BlockRegistry;

#[derive(Default)]
pub struct ClientOptions {
    /// When set, the client waits until the initial view is fully meshed
    /// (or a timeout expires), saves a screenshot to this path, and exits.
    /// Used for automated verification.
    pub screenshot: Option<PathBuf>,
}

/// Builds and runs the Bevy app. Blocks until the window closes.
///
/// Responsibilities (wired by the client agent):
/// - `DefaultPlugins`, sky-blue clear color, ambient + shadowed directional
///   light, player controller with walk/fly modes ([`camera`]).
/// - Networking systems ([`net`]): `Hello` on startup, request chunks around
///   the player, receive chunks into the [`view::ChunkStore`], resolve
///   spawn, periodic player updates, graceful disconnect.
/// - Meshing/spawning systems, including the dirty-chunk instant-remesh path
///   ([`view`]).
/// - Block targeting, highlighting and editing ([`interact`]).
/// - Hotbar UI and block selection ([`hotbar`]).
/// - Screenshot-and-exit mode ([`screenshot`]) when
///   [`ClientOptions::screenshot`] is set.
pub fn run_client<T: ClientTransport>(transport: T, options: ClientOptions) {
    let mut app = App::new();

    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "tsumiki".to_string(),
            ..default()
        }),
        ..default()
    }));

    app.insert_resource(ClearColor(Color::srgb(0.55, 0.78, 0.95)))
        .insert_resource(GlobalAmbientLight {
            color: Color::WHITE,
            brightness: 400.0,
            affects_lightmapped_meshes: true,
        })
        .add_systems(Startup, spawn_sun);

    camera::install(&mut app);
    net::install(&mut app, transport);
    view::install(&mut app, BlockRegistry::prototype());
    hotbar::install(&mut app);
    interact::install::<T>(&mut app);

    if let Some(path) = options.screenshot {
        screenshot::install(&mut app, path);
    }

    app.run();
}

fn spawn_sun(mut commands: Commands) {
    commands.spawn((
        DirectionalLight {
            color: Color::srgb(1.0, 0.98, 0.9),
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::default().with_rotation(
            Quat::from_rotation_y((-30f32).to_radians())
                * Quat::from_rotation_x((-50f32).to_radians()),
        ),
    ));
}

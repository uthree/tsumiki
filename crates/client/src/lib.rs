//! Game client: rendering, input, UI (design.md §1).
//!
//! Talks to the server exclusively through a [`ClientTransport`]; holds a
//! mirror cache of received chunks and renders them.
//!
//! The app has two phases, modeled as [`AppState`]: [`AppState::Menu`] (the
//! title screen, see [`menu`]) and [`AppState::InGame`] (the connected
//! world). [`StartMode`] picks which one the app boots into.

pub mod camera;
pub mod hotbar;
pub mod interact;
pub mod menu;
pub mod mesh;
pub mod net;
pub mod remote;
pub mod screenshot;
pub mod view;

use std::path::PathBuf;

use bevy::prelude::*;
use bevy::window::WindowPlugin;
use tsumiki_protocol::ClientTransport;
use tsumiki_world::BlockRegistry;

pub struct ClientOptions {
    /// When set, the client waits until the view (or, in [`StartMode::Menu`]
    /// with `menu_screenshot`, a fixed delay) settles, saves a screenshot to
    /// this path, and exits. Used for automated verification.
    pub screenshot: Option<PathBuf>,
    /// With `screenshot` set: capture the title menu itself (~3 s after
    /// startup) instead of entering the world. Ignored without `screenshot`.
    pub menu_screenshot: bool,
    /// Name sent to the server in `Hello`. In [`StartMode::Menu`], this is
    /// only the *default*: the Multiplayer connect form prefills its name
    /// field with it, and the field's value (once connected) is what
    /// actually gets sent.
    pub name: String,
    /// Overrides the default spawn column (world-space X/Z, `.x`/`.y`
    /// mapping to world X/Z) used to place a fresh player when the server
    /// has no saved state. Lets multiple client instances run on one
    /// machine for manual multiplayer testing without spawning stacked on
    /// top of each other.
    pub spawn_xz: Option<Vec2>,
    /// Whether to boot to the title menu or skip straight into the world.
    pub start: StartMode,
}

/// How the client app boots.
pub enum StartMode {
    /// Boot to the title menu ([`menu`]); the player picks Singleplayer or
    /// Multiplayer there.
    Menu(MenuHooks),
    /// Skip the menu and enter the world immediately (CLI `--connect` /
    /// screenshot verification / singleplayer-with-flags). The transport is
    /// inserted as a resource at startup, exactly as if the menu had already
    /// connected it.
    Direct(Box<dyn ClientTransport>),
}

/// Launcher-provided ways to obtain a transport, so this crate never depends
/// on the server or net crates (design.md §1 decoupling).
#[derive(Resource)]
#[allow(clippy::type_complexity)]
pub struct MenuHooks {
    /// Spawns the in-process server and returns the connected transport.
    /// `None` (or already taken) hides the Singleplayer button, since the
    /// hook can only be used once.
    pub start_singleplayer: Option<Box<dyn FnOnce() -> Box<dyn ClientTransport> + Send + Sync>>,
    /// Connects to a remote server ("host" or "host:port" string, parsed by
    /// the hook itself so the menu stays dumb).
    pub connect: Box<dyn Fn(&str) -> std::io::Result<Box<dyn ClientTransport>> + Send + Sync>,
}

/// The app's overall phase. See the module docs above.
#[derive(States, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum AppState {
    #[default]
    Menu,
    InGame,
}

/// The client's one and only font (doc/assets.md §1.1: Misaki Gothic, an
/// 8×8 bitmap font) — every piece of UI text the client spawns (menu, name
/// tags) must use it, via [`UiFont::text`], so nothing ever falls back to
/// Bevy's default font. Loaded once, directly (not via a `Startup` system:
/// [`menu::install`]'s `OnEnter(AppState::Menu)` can fire *before* `Startup`
/// when [`StartMode::Menu`] is the initial state, so the resource must
/// already exist by the time `app.run()` starts the schedule loop).
///
/// If `assets/fonts/misaki_gothic.ttf` is missing (e.g. running the binary
/// outside the project root), the handle simply never finishes loading;
/// `AssetServer::load` does not panic, and text using it just renders with
/// whatever Bevy's asset pipeline does for an unloaded font (no glyphs yet),
/// not a crash.
#[derive(Resource, Clone)]
pub struct UiFont(pub Handle<Font>);

impl UiFont {
    /// A `TextFont` using this font at `size` — pick a multiple of 8 (doc/assets.md
    /// §1.1: an 8×8 bitmap font stays crisp only at multiples of its grid) — with
    /// smoothing disabled, since antialiasing a bitmap font blurs it.
    pub fn text(&self, size: f32) -> TextFont {
        TextFont::from_font_size(size)
            .with_font(self.0.clone())
            .with_font_smoothing(FontSmoothing::None)
    }
}

/// Per-run configuration derived from [`ClientOptions`], installed as a
/// resource so [`camera`] (initial/default spawn column) and [`net`] (the
/// name sent in `Hello`) can read it. [`menu`] overwrites `name` with the
/// connect form's value before transitioning into [`AppState::InGame`] over
/// Multiplayer.
#[derive(Resource, Clone)]
pub struct ClientConfig {
    pub name: String,
    pub spawn_xz: Option<Vec2>,
}

/// Builds and runs the Bevy app. Blocks until the window closes.
///
/// Responsibilities (wired by the client agent):
/// - `DefaultPlugins`, sky-blue clear color, ambient + shadowed directional
///   light, player controller with walk/fly modes ([`camera`]), all gated to
///   [`AppState::InGame`].
/// - The title menu ([`menu`]) while in [`AppState::Menu`].
/// - Networking systems ([`net`]): `Hello` on entering the world, request
///   chunks around the player, receive chunks into the [`view::ChunkStore`],
///   resolve spawn, periodic player updates, graceful disconnect.
/// - Meshing/spawning systems, including the dirty-chunk instant-remesh path
///   ([`view`]).
/// - Block targeting, highlighting and editing ([`interact`]).
/// - Hotbar UI and block selection ([`hotbar`]).
/// - Screenshot-and-exit mode ([`screenshot`]) when
///   [`ClientOptions::screenshot`] is set.
pub fn run_client(options: ClientOptions) {
    let mut app = App::new();

    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "tsumiki".to_string(),
            ..default()
        }),
        ..default()
    }));

    // Loaded directly (see `UiFont`'s docs) rather than in a `Startup`
    // system, so it's guaranteed ready before either `AppState`'s `OnEnter`
    // can fire.
    let font: Handle<Font> = app
        .world()
        .resource::<AssetServer>()
        .load("fonts/misaki_gothic.ttf");
    app.insert_resource(UiFont(font));

    app.insert_resource(ClearColor(Color::srgb(0.55, 0.78, 0.95)))
        .insert_resource(GlobalAmbientLight {
            color: Color::WHITE,
            brightness: 400.0,
            affects_lightmapped_meshes: true,
        })
        .insert_resource(ClientConfig {
            name: options.name,
            spawn_xz: options.spawn_xz,
        })
        .add_systems(OnEnter(AppState::InGame), spawn_sun);

    camera::install(&mut app);
    net::install(&mut app);
    view::install(&mut app, BlockRegistry::prototype());
    hotbar::install(&mut app);
    interact::install(&mut app);
    remote::install(&mut app);
    menu::install(&mut app);

    let menu_screenshot = options.menu_screenshot && options.screenshot.is_some();

    match options.start {
        StartMode::Menu(hooks) => {
            app.insert_state(AppState::Menu).insert_resource(hooks);
        }
        StartMode::Direct(transport) => {
            app.insert_state(AppState::InGame)
                .insert_resource(net::Transport::new(transport));
        }
    }

    if let Some(path) = options.screenshot {
        screenshot::install(&mut app, path, menu_screenshot);
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

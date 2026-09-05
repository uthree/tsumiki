//! Game client: rendering, input, UI (design.md §1).
//!
//! Talks to the server exclusively through a [`ClientTransport`]; holds a
//! mirror cache of received chunks and renders them.
//!
//! The app has two phases, modeled as [`AppState`]: [`AppState::Menu`] (the
//! title screen, see [`menu`]) and [`AppState::InGame`] (the connected
//! world). [`StartMode`] picks which one the app boots into.

pub mod camera;
pub mod damage;
pub mod daynight;
pub mod death;
mod entity_light;
pub mod health;
pub mod hotbar;
pub mod interact;
pub mod inventory;
mod item_icons;
pub mod items;
pub mod lod_view;
pub mod menu;
pub mod mesh;
pub mod net;
pub mod pause;
pub mod remote;
pub mod screenshot;
pub mod settings;
pub mod state;
pub mod ui;
pub mod underwater;
pub mod view;
mod voxel_material;

use std::path::PathBuf;

use bevy::prelude::*;
use bevy::window::WindowPlugin;
use tsumiki_protocol::{ClientTransport, GameMode};
use tsumiki_world::{BlockRegistry, ItemRegistry, RecipeRegistry};

pub struct ClientOptions {
    /// When set, the client waits until the target screen has settled, saves
    /// a screenshot to this path, and exits. Used for automated
    /// verification.
    pub screenshot: Option<PathBuf>,
    /// Which screen `screenshot` captures. Ignored without `screenshot`.
    pub screenshot_target: ScreenshotTarget,
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

/// Which screen an automated screenshot run captures.
///
/// One enum rather than a bool per screen: the screens are mutually
/// exclusive, and a set of bools makes "which one wins" a rule nobody can
/// see from the type.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ScreenshotTarget {
    /// The plain in-world view, once chunks and LOD have settled. Requires
    /// [`StartMode::Direct`].
    #[default]
    World,
    /// The same world camera with the real hold-C zoom input active.
    Zoom,
    /// Preserve the saved camera for underground light verification.
    Cave,
    /// The title menu, ~3 s after startup. Requires [`StartMode::Menu`].
    Menu,
    /// The world-select screen, reached by clicking Singleplayer. Requires
    /// [`StartMode::Menu`].
    WorldSelect,
    /// The create-world form, reached from the world-select screen.
    /// Requires [`StartMode::Menu`].
    CreateWorld,
    /// In-world, then the pause menu, captured ~1 s later.
    Pause,
    /// In-world, then the inventory screen with a few sample stacks
    /// populated into the local snapshot (this bypasses the "client never
    /// mutates its own inventory" rule deliberately -- it is a
    /// verification-only fixture, not a gameplay path), captured ~1 s later.
    Inventory,
}

impl ScreenshotTarget {
    /// Whether this target is captured from the menu rather than in-world.
    pub fn is_menu(self) -> bool {
        matches!(self, Self::Menu | Self::WorldSelect | Self::CreateWorld)
    }
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
    /// Everything the world-select screen needs. `None` hides the
    /// Singleplayer button (a client built without a bundled server, e.g. a
    /// `--connect`-only launch).
    pub singleplayer: Option<SingleplayerHooks>,
    /// Connects to a remote server ("host" or "host:port" string, parsed by
    /// the hook itself so the menu stays dumb).
    pub connect: Box<dyn Fn(&str) -> std::io::Result<Box<dyn ClientTransport>> + Send + Sync>,
}

/// The launcher's implementation of world management, injected so the client
/// never touches the filesystem or the server crate itself (design.md §1
/// decoupling). Every hook is `Fn`, not `FnOnce`: the menu calls them again
/// after a "Back to Title" round trip.
#[allow(clippy::type_complexity)]
pub struct SingleplayerHooks {
    /// Every world that can be played, for the world-select list. Called on
    /// entering the screen and after every create/delete, so the list is
    /// never stale.
    pub list: Box<dyn Fn() -> Vec<WorldEntry> + Send + Sync>,
    /// Creates a world directory and its metadata. The name has already
    /// passed [`world_name_is_valid`], but the hook must validate again --
    /// it, not the UI, is what stands between a name and the filesystem.
    pub create: Box<dyn Fn(&NewWorld) -> std::io::Result<()> + Send + Sync>,
    /// Permanently deletes a world and everything in it. The UI asks for
    /// confirmation first.
    pub delete: Box<dyn Fn(&str) -> std::io::Result<()> + Send + Sync>,
    /// Spawns the in-process server on the named world and returns the
    /// connected transport.
    pub start: Box<dyn Fn(&str) -> std::io::Result<Box<dyn ClientTransport>> + Send + Sync>,
}

/// One row of the world-select list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorldEntry {
    pub name: String,
    pub game_mode: GameMode,
    /// Unix seconds when the world was last saved, for the "last played"
    /// line and for ordering the list (newest first). `None` if the
    /// timestamp could not be read.
    pub last_played: Option<u64>,
}

/// The create-world form's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewWorld {
    pub name: String,
    /// `None` means "pick a random seed" -- the form's seed field was left
    /// blank.
    pub seed: Option<u64>,
    pub game_mode: GameMode,
}

/// Whether `name` is acceptable as a world name.
///
/// The launcher maps a name onto a directory, so a name is rejected if it
/// could escape or confuse that mapping. Checked here so the create form can
/// grey out its button, and again in the hook, which is the boundary that
/// actually matters.
pub fn world_name_is_valid(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty()
        && trimmed.chars().count() <= MAX_WORLD_NAME_CHARS
        && trimmed != "."
        && trimmed != ".."
        && !trimmed.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
}

/// Longest accepted world name, in characters.
pub const MAX_WORLD_NAME_CHARS: usize = 32;

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
/// - `DefaultPlugins`, sky-blue clear color, ambient light, player controller
///   with walk/fly modes ([`camera`]), all gated to [`AppState::InGame`]; the
///   sun's direction/illuminance and the ambient/sky colors are then driven
///   continuously by the day/night cycle ([`daynight`]).
/// - The title menu ([`menu`]) while in [`AppState::Menu`].
/// - Session state ([`state`]): the world's game mode and the local player's
///   health/inventory/time of day, always present so HUD/gameplay code never
///   has to special-case "not connected yet".
/// - Networking systems ([`net`]): `Hello` on entering the world, request
///   chunks around the player, receive chunks into the [`view::ChunkStore`],
///   resolve spawn, periodic player updates, graceful disconnect.
/// - Meshing/spawning systems, including the dirty-chunk instant-remesh path
///   ([`view`]).
/// - Block targeting, hold-to-mine progress, placing, and opening containers
///   ([`interact`]).
/// - Hotbar UI and slot selection ([`hotbar`]), and the backpack/crafting/
///   container inventory screen ([`inventory`]), both rendering the
///   server-owned inventory snapshot in [`state::GameState`]/
///   [`state::ContainerState`].
/// - Survival HUD: hearts + air bubbles ([`health`]), fall/drowning damage
///   detection ([`damage`]), the death overlay and respawn ([`death`]), the
///   underwater screen tint ([`underwater`]), and dropped-item entities
///   ([`items`]).
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
        });

    ui::install(&mut app);
    item_icons::install(&mut app);
    settings::install(&mut app);
    state::install(
        &mut app,
        ItemRegistry::prototype(),
        RecipeRegistry::prototype(),
    );
    camera::install(&mut app);
    net::install(&mut app);
    view::install(&mut app, BlockRegistry::prototype());
    lod_view::install(&mut app);
    pause::install(&mut app);
    hotbar::install(&mut app);
    interact::install(&mut app);
    inventory::install(&mut app);
    remote::install(&mut app);
    items::install(&mut app);
    damage::install(&mut app);
    health::install(&mut app);
    death::install(&mut app);
    underwater::install(&mut app);
    daynight::install(&mut app);
    menu::install(&mut app);

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
        screenshot::install(&mut app, path, options.screenshot_target);
    }

    app.run();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_and_whitespace_only() {
        assert!(!world_name_is_valid(""));
        assert!(!world_name_is_valid("   "));
        assert!(!world_name_is_valid("\t\n"));
    }

    #[test]
    fn trims_surrounding_whitespace_before_checking() {
        assert!(world_name_is_valid("  My World  "));
    }

    #[test]
    fn rejects_dot_and_dotdot() {
        assert!(!world_name_is_valid("."));
        assert!(!world_name_is_valid(".."));
        // Padded with whitespace, it's still just "." / ".." once trimmed.
        assert!(!world_name_is_valid("  ..  "));
        // But a name that merely contains ".." elsewhere is fine.
        assert!(world_name_is_valid("World..2"));
    }

    #[test]
    fn rejects_path_separators_and_reserved_characters() {
        for bad in [
            "a/b", "a\\b", "a:b", "a*b", "a?b", "a\"b", "a<b", "a>b", "a|b",
        ] {
            assert!(!world_name_is_valid(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn rejects_control_characters() {
        assert!(!world_name_is_valid("a\u{0}b"));
        assert!(!world_name_is_valid("a\rb"));
    }

    #[test]
    fn accepts_name_at_the_length_limit() {
        let name: String = "a".repeat(MAX_WORLD_NAME_CHARS);
        assert!(world_name_is_valid(&name));
    }

    #[test]
    fn rejects_name_over_the_length_limit() {
        let name: String = "a".repeat(MAX_WORLD_NAME_CHARS + 1);
        assert!(!world_name_is_valid(&name));
    }

    #[test]
    fn length_limit_counts_unicode_scalars_not_bytes() {
        // Each "世" is 3 bytes in UTF-8 but one `char`; the limit is on
        // character count, so a name of exactly the limit in multi-byte
        // characters must still be accepted.
        let name: String = "世".repeat(MAX_WORLD_NAME_CHARS);
        assert!(world_name_is_valid(&name));
        assert!(!world_name_is_valid(&"世".repeat(MAX_WORLD_NAME_CHARS + 1)));
    }

    #[test]
    fn accepts_ordinary_unicode_names() {
        assert!(world_name_is_valid("世界のワールド"));
        assert!(world_name_is_valid("Café du Monde"));
    }
}

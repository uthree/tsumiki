//! Launcher (design.md §1.1).
//!
//! Modes:
//! - default: boots to the title menu; singleplayer worlds live under
//!   `worlds/<name>/` (see [`world_dir_for`]) and are listed/created/deleted/
//!   started through [`tsumiki_client::SingleplayerHooks`], multiplayer
//!   connects to a remote server. The menu obtains transports through hooks
//!   injected here, so the client crate stays decoupled from the server and
//!   net crates.
//! - `--server`: dedicated headless server over UDP.
//! - `--connect <addr>`: skip the menu, connect to a remote server.
//! - `--world DIR`: skip the menu, singleplayer, using `DIR` verbatim rather
//!   than the `worlds/<name>/` mapping (scripts and screenshot verification
//!   depend on pointing this at an exact directory). `--ephemeral` does the
//!   same but with no on-disk world at all.
//! - `--screenshot PATH`: skip the menu, singleplayer, capture the world and
//!   exit (automated verification). `--menu-screenshot`,
//!   `--world-select-screenshot`, `--create-world-screenshot`,
//!   `--pause-screenshot` and `--inventory-screenshot` capture the title
//!   menu, the world-select screen, the create-world form, the pause menu
//!   and the inventory screen instead.
//!
//! On startup, a world saved under the pre-`worlds/` layout (a single
//! `world/` directory at the repo root) is migrated to `worlds/world/` (see
//! [`migrate_legacy_world_dir`]) so it keeps showing up in the world-select
//! list under its old default name.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tsumiki_client::{
    ClientOptions, MenuHooks, NewWorld, ScreenshotTarget, SingleplayerHooks, StartMode, WorldEntry,
};
use tsumiki_net::DEFAULT_PORT;
use tsumiki_protocol::ClientTransport;

/// Root directory (relative to the launcher's working directory) under which
/// every named world lives, as `worlds/<name>/`.
const WORLDS_ROOT_NAME: &str = "worlds";

struct Args {
    seed: u64,
    screenshot: Option<PathBuf>,
    screenshot_target: ScreenshotTarget,
    world_dir: Option<PathBuf>,
    /// Set by `--world` or `--ephemeral`: an explicit request to play a
    /// specific (or no) world directory right away, bypassing the menu
    /// entirely. Distinct from `world_dir`'s *default* value, which is only
    /// consulted when a screenshot target also forces a direct start.
    world_dir_explicit: bool,
    server: bool,
    port: u16,
    connect: Option<String>,
    name: String,
    spawn_xz: Option<(f32, f32)>,
    game_mode: Option<tsumiki_protocol::GameMode>,
}

fn parse_args() -> Args {
    let mut args = Args {
        seed: 42,
        screenshot: None,
        screenshot_target: ScreenshotTarget::default(),
        world_dir: Some(PathBuf::from("world")),
        world_dir_explicit: false,
        server: false,
        port: DEFAULT_PORT,
        connect: None,
        name: "player".to_string(),
        spawn_xz: None,
        game_mode: None,
    };

    let mut it = std::env::args().skip(1);
    let next = |flag: &str, it: &mut dyn Iterator<Item = String>| -> String {
        it.next().unwrap_or_else(|| {
            eprintln!("{flag} needs a value");
            std::process::exit(2);
        })
    };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--seed" => {
                args.seed = next("--seed", &mut it)
                    .parse()
                    .expect("--seed value must be an integer")
            }
            "--screenshot" => {
                args.screenshot = Some(PathBuf::from(next("--screenshot", &mut it)));
                args.screenshot_target = ScreenshotTarget::World;
            }
            "--cave-screenshot" => {
                args.screenshot = Some(PathBuf::from(next("--cave-screenshot", &mut it)));
                args.screenshot_target = ScreenshotTarget::Cave;
            }
            "--menu-screenshot" => {
                args.screenshot = Some(PathBuf::from(next("--menu-screenshot", &mut it)));
                args.screenshot_target = ScreenshotTarget::Menu;
            }
            "--world-select-screenshot" => {
                args.screenshot = Some(PathBuf::from(next("--world-select-screenshot", &mut it)));
                args.screenshot_target = ScreenshotTarget::WorldSelect;
            }
            "--create-world-screenshot" => {
                args.screenshot = Some(PathBuf::from(next("--create-world-screenshot", &mut it)));
                args.screenshot_target = ScreenshotTarget::CreateWorld;
            }
            "--pause-screenshot" => {
                args.screenshot = Some(PathBuf::from(next("--pause-screenshot", &mut it)));
                args.screenshot_target = ScreenshotTarget::Pause;
            }
            "--inventory-screenshot" => {
                args.screenshot = Some(PathBuf::from(next("--inventory-screenshot", &mut it)));
                args.screenshot_target = ScreenshotTarget::Inventory;
            }
            "--world" => {
                args.world_dir = Some(PathBuf::from(next("--world", &mut it)));
                args.world_dir_explicit = true;
            }
            "--ephemeral" => {
                args.world_dir = None;
                args.world_dir_explicit = true;
            }
            "--server" => args.server = true,
            "--port" => {
                args.port = next("--port", &mut it)
                    .parse()
                    .expect("--port value must be a port number")
            }
            "--connect" => args.connect = Some(next("--connect", &mut it)),
            "--name" => args.name = next("--name", &mut it),
            "--mode" => {
                args.game_mode = Some(match next("--mode", &mut it).as_str() {
                    "survival" => tsumiki_protocol::GameMode::Survival,
                    "creative" => tsumiki_protocol::GameMode::Creative,
                    other => {
                        eprintln!("--mode must be survival or creative, got {other}");
                        std::process::exit(2);
                    }
                });
            }
            "--spawn" => {
                let x = next("--spawn", &mut it).parse().expect("--spawn needs X Z");
                let z = next("--spawn", &mut it).parse().expect("--spawn needs X Z");
                args.spawn_xz = Some((x, z));
            }
            other => {
                eprintln!("unknown argument: {other}");
                eprintln!(
                    "usage: tsumiki [--seed N] [--world DIR | --ephemeral] [--mode survival|creative]\n\
                     \x20      [--screenshot PATH | --menu-screenshot PATH | --world-select-screenshot PATH\n\
                     \x20       | --create-world-screenshot PATH | --pause-screenshot PATH\n\
                     \x20       | --inventory-screenshot PATH | --cave-screenshot PATH]\n\
                     \x20      [--server [--port P]] [--connect ADDR[:PORT]] [--name NAME] [--spawn X Z]"
                );
                std::process::exit(2);
            }
        }
    }
    args
}

/// Accepts "host" or "host:port"; a bare host gets the default port.
fn parse_server_addr(raw: &str) -> std::io::Result<SocketAddr> {
    raw.parse()
        .or_else(|_| format!("{raw}:{DEFAULT_PORT}").parse())
        .map_err(|_| std::io::Error::other(format!("invalid server address: {raw}")))
}

/// Creates the in-process server + connected local transport; the spawned
/// server thread's handle is stashed in `server_slot` so `main` can wait for
/// the world save after the client exits.
fn start_singleplayer(
    seed: u64,
    world_dir: Option<PathBuf>,
    game_mode: Option<tsumiki_protocol::GameMode>,
    server_slot: Arc<Mutex<Option<JoinHandle<()>>>>,
) -> Box<dyn ClientTransport> {
    let (server_transport, client_transport) = tsumiki_protocol::local::pair();
    let config = tsumiki_server::ServerConfig {
        seed,
        world_dir,
        game_mode,
        ..Default::default()
    };
    let handle = std::thread::spawn(move || tsumiki_server::run_server(server_transport, config));
    *server_slot.lock().unwrap() = Some(handle);
    Box::new(client_transport)
}

fn connect_remote(raw: &str) -> std::io::Result<Box<dyn ClientTransport>> {
    let addr = parse_server_addr(raw)?;
    Ok(Box::new(tsumiki_net::NetClientTransport::connect(addr)?))
}

/// Maps a world name to its directory under `<base>/worlds/`, re-validating
/// with [`tsumiki_client::world_name_is_valid`] rather than trusting the
/// menu's own check -- the client crate never touches the filesystem, so
/// this is the actual boundary between a name and a path. `None` covers
/// every way a name could be hostile: empty, too long, `.`/`..`, or
/// containing a path separator or other filesystem-meaningful character.
fn world_dir_for(base: &Path, name: &str) -> Option<PathBuf> {
    if !tsumiki_client::world_name_is_valid(name) {
        return None;
    }
    let worlds_root = base.join(WORLDS_ROOT_NAME);
    let dir = worlds_root.join(name.trim());
    // Defense in depth: `world_name_is_valid` already forbids path
    // separators and "..", so `dir` can only actually land here as a direct
    // child of `worlds_root` today -- but confirming that costs nothing and
    // keeps this function safe on its own if that validation is ever
    // loosened.
    if dir.parent() != Some(worlds_root.as_path()) {
        return None;
    }
    Some(dir)
}

fn invalid_name_error(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{name:?} is not a valid world name"),
    )
}

/// Picks a seed when the create-world form's seed field was left blank
/// (`NewWorld::seed == None`). There is no `rand` dependency anywhere in
/// this workspace (see the root `Cargo.toml`), so a fresh seed is derived
/// from the system clock instead -- good enough to "give me something
/// different each time," not meant to be unpredictable.
fn random_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Lists every named world under `<base>/worlds/`, newest-played first
/// (worlds whose timestamp couldn't be read sort last, never first). A
/// directory with no `meta.bin` is silently skipped (it isn't a world); one
/// whose `meta.bin` fails to parse is skipped with an `eprintln!` so one
/// corrupt world can't hide the rest of the list.
fn list_worlds(base: &Path) -> Vec<WorldEntry> {
    let worlds_root = base.join(WORLDS_ROOT_NAME);
    let read_dir = match std::fs::read_dir(&worlds_root) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(), // no worlds/ directory yet
    };

    let mut entries = Vec::new();
    for entry in read_dir {
        let Ok(entry) = entry else { continue };
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => {}
            _ => continue,
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        match peek_world_entry(&entry.path(), &name) {
            Ok(Some(world)) => entries.push(world),
            Ok(None) => {} // no meta.bin: not a world directory
            Err(e) => eprintln!("tsumiki: skipping worlds/{name}: {e}"),
        }
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.last_played));
    entries
}

/// Reads one world directory's game mode (via
/// [`tsumiki_server::peek_meta`], no chunk data touched) and `meta.bin`'s
/// mtime into a [`WorldEntry`]. `Ok(None)` means `dir` has no `meta.bin`,
/// the same "not a world (yet)" convention `peek_meta` itself uses.
fn peek_world_entry(dir: &Path, name: &str) -> io::Result<Option<WorldEntry>> {
    let Some(peeked) = tsumiki_server::peek_meta(dir)? else {
        return Ok(None);
    };
    let last_played = std::fs::metadata(dir.join("meta.bin"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    Ok(Some(WorldEntry {
        name: name.to_string(),
        game_mode: peeked.game_mode,
        last_played,
    }))
}

/// Creates `<base>/worlds/<name>/` and its initial `meta.bin`, so the world
/// exists (and shows up in [`list_worlds`]) before it is ever played.
fn create_world(base: &Path, new_world: &NewWorld) -> io::Result<()> {
    let dir =
        world_dir_for(base, &new_world.name).ok_or_else(|| invalid_name_error(&new_world.name))?;
    if dir.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("a world named {:?} already exists", new_world.name),
        ));
    }
    std::fs::create_dir_all(&dir)?;
    let seed = new_world.seed.unwrap_or_else(random_seed);
    tsumiki_server::create_world_meta(&dir, seed, new_world.game_mode)
}

/// Permanently deletes `<base>/worlds/<name>/`. This is the one irreversible
/// operation in the whole program, so the guard is deliberately narrow:
/// `dir` must resolve through the exact same name -> directory mapping used
/// everywhere else ([`world_dir_for`]), and it must look like an actual
/// world (have a `meta.bin`) rather than some other directory that happens
/// to sit under `worlds/`.
fn delete_world(base: &Path, name: &str) -> io::Result<()> {
    let dir = world_dir_for(base, name).ok_or_else(|| invalid_name_error(name))?;
    if !dir.join("meta.bin").is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no world named {name:?}"),
        ));
    }
    std::fs::remove_dir_all(&dir)
}

/// Spawns the in-process server on the named world, exactly like
/// [`start_singleplayer`] used for `--world`/`--screenshot`.
fn start_named_world(
    base: &Path,
    name: &str,
    server_slot: Arc<Mutex<Option<JoinHandle<()>>>>,
) -> io::Result<Box<dyn ClientTransport>> {
    let dir = world_dir_for(base, name).ok_or_else(|| invalid_name_error(name))?;
    // The seed and game mode passed here only matter if `meta.bin` is
    // somehow missing -- `create_world` always writes one first, so this is
    // a fallback, not the normal path. Once `meta.bin` exists, the server
    // always loads the world's own persisted seed and game mode instead
    // (see `ServerConfig::game_mode`'s docs).
    Ok(start_singleplayer(0, Some(dir), None, server_slot))
}

/// The action [`migrate_legacy_world_dir`] should take, computed from just
/// "does the legacy `world/meta.bin` exist" and "does `worlds/world/`
/// already exist" -- kept separate from the actual filesystem calls so the
/// decision itself is testable without touching disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyMigration {
    /// `worlds/world/` doesn't exist yet: move the legacy directory there.
    Move,
    /// Both exist: do nothing and warn, rather than guess which one to keep.
    Conflict,
    /// No legacy world (or it's already migrated): nothing to do.
    Skip,
}

fn legacy_migration_decision(legacy_has_meta: bool, target_exists: bool) -> LegacyMigration {
    match (legacy_has_meta, target_exists) {
        (true, true) => LegacyMigration::Conflict,
        (true, false) => LegacyMigration::Move,
        (false, _) => LegacyMigration::Skip,
    }
}

/// One-time migration from the pre-`worlds/` layout (a single `world/`
/// directory at the repo root) to `worlds/world/`, so an existing save keeps
/// showing up in the world-select list under its old default name. Never
/// overwrites an existing `worlds/world/`; if both directories exist, both
/// are left alone (see [`legacy_migration_decision`]).
fn migrate_legacy_world_dir(base: &Path) {
    let legacy = base.join("world");
    let target = base.join(WORLDS_ROOT_NAME).join("world");
    let decision = legacy_migration_decision(legacy.join("meta.bin").is_file(), target.exists());
    match decision {
        LegacyMigration::Skip => {}
        LegacyMigration::Conflict => {
            eprintln!(
                "tsumiki: both {} and {} exist; leaving both in place \
                 (remove one manually to finish the migration)",
                legacy.display(),
                target.display()
            );
        }
        LegacyMigration::Move => {
            if let Some(parent) = target.parent()
                && let Err(e) = std::fs::create_dir_all(parent)
            {
                eprintln!("tsumiki: could not create {}: {e}", parent.display());
                return;
            }
            match std::fs::rename(&legacy, &target) {
                Ok(()) => println!(
                    "tsumiki: migrated legacy {} to {}",
                    legacy.display(),
                    target.display()
                ),
                Err(e) => eprintln!(
                    "tsumiki: failed to migrate {} to {}: {e}",
                    legacy.display(),
                    target.display()
                ),
            }
        }
    }
}

/// Builds the singleplayer hooks the title menu drives the world-select
/// screen with, all rooted at `<base>/worlds/`.
fn singleplayer_hooks(
    base: PathBuf,
    server_slot: Arc<Mutex<Option<JoinHandle<()>>>>,
) -> SingleplayerHooks {
    SingleplayerHooks {
        list: Box::new({
            let base = base.clone();
            move || list_worlds(&base)
        }),
        create: Box::new({
            let base = base.clone();
            move |new_world: &NewWorld| create_world(&base, new_world)
        }),
        delete: Box::new({
            let base = base.clone();
            move |name: &str| delete_world(&base, name)
        }),
        start: Box::new(move |name: &str| start_named_world(&base, name, server_slot.clone())),
    }
}

fn main() {
    // Run before anything else touches `worlds/`, so a pre-existing legacy
    // save is visible to `list_worlds` from the very first menu render.
    migrate_legacy_world_dir(Path::new("."));

    let args = parse_args();

    if args.server && args.connect.is_some() {
        eprintln!("--server and --connect are mutually exclusive");
        std::process::exit(2);
    }

    if args.server {
        // Dedicated server: headless, blocks on the main thread.
        let bind: SocketAddr = format!("0.0.0.0:{}", args.port).parse().unwrap();
        let transport = tsumiki_net::NetServerTransport::bind(bind)
            .unwrap_or_else(|e| panic!("failed to bind UDP server on {bind}: {e}"));
        println!("tsumiki server listening on {bind}");
        tsumiki_server::run_server(
            transport,
            tsumiki_server::ServerConfig {
                seed: args.seed,
                world_dir: args.world_dir,
                game_mode: args.game_mode,
                ..Default::default()
            },
        );
        return;
    }

    // Everything below runs the client on the main thread (winit
    // requirement); `server_slot` is filled iff a local server was spawned.
    let server_slot: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::new(Mutex::new(None));

    // A menu-screen screenshot needs the actual menu to exist, even if
    // `--world`/`--ephemeral` was also passed; every other screenshot
    // target (in-world, pause, inventory) needs a running world instead. A
    // bare `--world`/`--ephemeral` (no screenshot at all) is a new explicit
    // request to skip the menu entirely.
    let direct_singleplayer = match &args.screenshot {
        Some(_) => !args.screenshot_target.is_menu(),
        None => args.world_dir_explicit,
    };

    let start = if let Some(raw) = &args.connect {
        // Skip the menu, connect straight to a remote server.
        StartMode::Direct(connect_remote(raw).unwrap_or_else(|e| panic!("{e}")))
    } else if direct_singleplayer {
        StartMode::Direct(start_singleplayer(
            args.seed,
            args.world_dir.clone(),
            args.game_mode,
            server_slot.clone(),
        ))
    } else {
        // Normal boot: title menu. The singleplayer hooks may be called
        // repeatedly (list, create, start, back to title, list again); each
        // `start` call spawns a fresh server on that world's directory (the
        // previous one saves and exits when its only client says goodbye).
        StartMode::Menu(MenuHooks {
            singleplayer: Some(singleplayer_hooks(PathBuf::from("."), server_slot.clone())),
            connect: Box::new(connect_remote),
        })
    };

    tsumiki_client::run_client(ClientOptions {
        screenshot: args.screenshot.clone(),
        screenshot_target: args.screenshot_target,
        name: args.name.clone(),
        spawn_xz: args.spawn_xz.map(|(x, z)| bevy_math::Vec2::new(x, z)),
        start,
    });

    // The client sends Goodbye on exit; give a locally-spawned server a
    // moment to save the world and shut down before the process dies.
    let handle = server_slot.lock().unwrap().take();
    if let Some(server) = handle {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !server.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if server.is_finished() {
            let _ = server.join();
        } else {
            eprintln!("server did not shut down in time; world may not be fully saved");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- world_dir_for -----------------------------------------------

    #[test]
    fn world_dir_for_accepts_a_plain_name() {
        let base = Path::new("base");
        assert_eq!(
            world_dir_for(base, "My World"),
            Some(PathBuf::from("base/worlds/My World"))
        );
    }

    #[test]
    fn world_dir_for_rejects_traversal_and_separators() {
        let base = Path::new("base");
        for hostile in ["..", ".", "", "a/b", "a\\b", "../escape", "a:b"] {
            assert_eq!(
                world_dir_for(base, hostile),
                None,
                "expected {hostile:?} to be rejected"
            );
        }
    }

    #[test]
    fn world_dir_for_rejects_an_overlong_name() {
        let base = Path::new("base");
        let too_long = "a".repeat(tsumiki_client::MAX_WORLD_NAME_CHARS + 1);
        assert_eq!(world_dir_for(base, &too_long), None);
    }

    // ---- list_worlds ----------------------------------------------------

    #[test]
    fn list_worlds_on_a_missing_worlds_dir_is_empty() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        assert!(list_worlds(dir.path()).is_empty());
    }

    #[test]
    fn list_worlds_skips_a_directory_with_no_meta_bin() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let worlds_root = dir.path().join(WORLDS_ROOT_NAME);
        std::fs::create_dir_all(worlds_root.join("not_a_world")).unwrap();
        assert!(list_worlds(dir.path()).is_empty());
    }

    #[test]
    fn list_worlds_skips_a_corrupt_meta_bin_without_hiding_the_rest() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let worlds_root = dir.path().join(WORLDS_ROOT_NAME);

        let good = world_dir_for(dir.path(), "good").unwrap();
        std::fs::create_dir_all(&good).unwrap();
        tsumiki_server::create_world_meta(&good, 1, tsumiki_protocol::GameMode::Survival).unwrap();

        let corrupt = worlds_root.join("corrupt");
        std::fs::create_dir_all(&corrupt).unwrap();
        std::fs::write(corrupt.join("meta.bin"), b"not a valid meta.bin").unwrap();

        let entries = list_worlds(dir.path());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "good");
    }

    #[test]
    fn list_worlds_orders_newest_played_first() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");

        let older = world_dir_for(dir.path(), "older").unwrap();
        std::fs::create_dir_all(&older).unwrap();
        tsumiki_server::create_world_meta(&older, 1, tsumiki_protocol::GameMode::Survival).unwrap();

        // Force a distinct, known mtime ordering rather than relying on two
        // writes happening in different filesystem-timestamp ticks.
        let older_meta = older.join("meta.bin");
        let newer = world_dir_for(dir.path(), "newer").unwrap();
        std::fs::create_dir_all(&newer).unwrap();
        tsumiki_server::create_world_meta(&newer, 2, tsumiki_protocol::GameMode::Survival).unwrap();
        let newer_meta = newer.join("meta.bin");

        let old_time = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let new_time = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(2_000);
        set_mtime(&older_meta, old_time);
        set_mtime(&newer_meta, new_time);

        let entries = list_worlds(dir.path());
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["newer", "older"]);
    }

    /// Sets a file's mtime, for deterministically testing [`list_worlds`]'s
    /// ordering without depending on real wall-clock timing between writes.
    /// Needs a write-capable handle -- a read-only one can't set mtime on
    /// Windows.
    fn set_mtime(path: &Path, time: std::time::SystemTime) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("failed to open file to set mtime");
        file.set_modified(time).expect("failed to set mtime");
    }

    // ---- create_world / delete_world -------------------------------------

    #[test]
    fn create_world_then_list_shows_it() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let new_world = NewWorld {
            name: "Homestead".to_string(),
            seed: Some(7),
            game_mode: tsumiki_protocol::GameMode::Creative,
        };
        create_world(dir.path(), &new_world).expect("create_world failed");

        let entries = list_worlds(dir.path());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Homestead");
        assert_eq!(entries[0].game_mode, tsumiki_protocol::GameMode::Creative);
    }

    #[test]
    fn create_world_with_no_seed_picks_one() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let new_world = NewWorld {
            name: "Blank Seed".to_string(),
            seed: None,
            game_mode: tsumiki_protocol::GameMode::Survival,
        };
        create_world(dir.path(), &new_world).expect("create_world failed");
        let world_dir = world_dir_for(dir.path(), "Blank Seed").unwrap();
        assert!(world_dir.join("meta.bin").is_file());
    }

    #[test]
    fn create_world_rejects_a_hostile_name() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let new_world = NewWorld {
            name: "../escape".to_string(),
            seed: Some(1),
            game_mode: tsumiki_protocol::GameMode::Survival,
        };
        assert!(create_world(dir.path(), &new_world).is_err());
    }

    #[test]
    fn create_world_refuses_to_clobber_an_existing_world() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let new_world = NewWorld {
            name: "Dup".to_string(),
            seed: Some(1),
            game_mode: tsumiki_protocol::GameMode::Survival,
        };
        create_world(dir.path(), &new_world).expect("first create_world failed");
        assert!(create_world(dir.path(), &new_world).is_err());
    }

    #[test]
    fn delete_world_removes_an_existing_world() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let new_world = NewWorld {
            name: "Gone Soon".to_string(),
            seed: Some(1),
            game_mode: tsumiki_protocol::GameMode::Survival,
        };
        create_world(dir.path(), &new_world).expect("create_world failed");
        delete_world(dir.path(), "Gone Soon").expect("delete_world failed");
        assert!(list_worlds(dir.path()).is_empty());
    }

    #[test]
    fn delete_world_refuses_a_hostile_name() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        assert!(delete_world(dir.path(), "../escape").is_err());
    }

    #[test]
    fn delete_world_refuses_a_directory_with_no_meta_bin() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let worlds_root = dir.path().join(WORLDS_ROOT_NAME);
        std::fs::create_dir_all(worlds_root.join("Empty")).unwrap();
        assert!(delete_world(dir.path(), "Empty").is_err());
        // The guard must actually refuse, not just complain: the directory
        // is still there afterwards.
        assert!(worlds_root.join("Empty").is_dir());
    }

    #[test]
    fn delete_world_refuses_a_nonexistent_world() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        assert!(delete_world(dir.path(), "NeverExisted").is_err());
    }

    // ---- legacy migration decision --------------------------------------

    #[test]
    fn legacy_migration_moves_when_only_the_legacy_dir_has_meta() {
        assert_eq!(
            legacy_migration_decision(true, false),
            LegacyMigration::Move
        );
    }

    #[test]
    fn legacy_migration_conflicts_when_both_exist() {
        assert_eq!(
            legacy_migration_decision(true, true),
            LegacyMigration::Conflict
        );
    }

    #[test]
    fn legacy_migration_skips_when_there_is_no_legacy_world() {
        assert_eq!(
            legacy_migration_decision(false, false),
            LegacyMigration::Skip
        );
        assert_eq!(
            legacy_migration_decision(false, true),
            LegacyMigration::Skip
        );
    }

    #[test]
    fn migrate_legacy_world_dir_moves_the_directory() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let legacy = dir.path().join("world");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("meta.bin"), b"pretend meta").unwrap();

        migrate_legacy_world_dir(dir.path());

        assert!(!legacy.exists());
        let target = dir.path().join(WORLDS_ROOT_NAME).join("world");
        assert!(target.join("meta.bin").is_file());
    }

    #[test]
    fn migrate_legacy_world_dir_never_overwrites_an_existing_target() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let legacy = dir.path().join("world");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("meta.bin"), b"legacy meta").unwrap();

        let target = dir.path().join(WORLDS_ROOT_NAME).join("world");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("meta.bin"), b"already-migrated meta").unwrap();

        migrate_legacy_world_dir(dir.path());

        // Both survive untouched.
        assert_eq!(
            std::fs::read(legacy.join("meta.bin")).unwrap(),
            b"legacy meta"
        );
        assert_eq!(
            std::fs::read(target.join("meta.bin")).unwrap(),
            b"already-migrated meta"
        );
    }

    #[test]
    fn migrate_legacy_world_dir_is_a_noop_with_no_legacy_world() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        migrate_legacy_world_dir(dir.path());
        assert!(!dir.path().join(WORLDS_ROOT_NAME).exists());
    }
}

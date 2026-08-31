//! Launcher (design.md §1.1).
//!
//! Modes:
//! - default: boots to the title menu; singleplayer spawns an in-process
//!   server, multiplayer connects to a remote one. The menu obtains
//!   transports through hooks injected here, so the client crate stays
//!   decoupled from the server and net crates.
//! - `--server`: dedicated headless server over UDP.
//! - `--connect <addr>`: skip the menu, connect to a remote server.
//! - `--screenshot PATH`: skip the menu, singleplayer, capture the world
//!   and exit (automated verification). `--menu-screenshot`,
//!   `--pause-screenshot` and `--inventory-screenshot` capture the title
//!   menu, the pause menu and the inventory screen instead.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tsumiki_client::{ClientOptions, MenuHooks, StartMode};
use tsumiki_net::DEFAULT_PORT;
use tsumiki_protocol::ClientTransport;

struct Args {
    seed: u64,
    screenshot: Option<PathBuf>,
    menu_screenshot: bool,
    pause_screenshot: bool,
    inventory_screenshot: bool,
    world_dir: Option<PathBuf>,
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
        menu_screenshot: false,
        pause_screenshot: false,
        inventory_screenshot: false,
        world_dir: Some(PathBuf::from("world")),
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
            "--screenshot" => args.screenshot = Some(PathBuf::from(next("--screenshot", &mut it))),
            "--menu-screenshot" => {
                args.screenshot = Some(PathBuf::from(next("--menu-screenshot", &mut it)));
                args.menu_screenshot = true;
            }
            "--pause-screenshot" => {
                args.screenshot = Some(PathBuf::from(next("--pause-screenshot", &mut it)));
                args.pause_screenshot = true;
            }
            "--inventory-screenshot" => {
                args.screenshot = Some(PathBuf::from(next("--inventory-screenshot", &mut it)));
                args.inventory_screenshot = true;
            }
            "--world" => args.world_dir = Some(PathBuf::from(next("--world", &mut it))),
            "--ephemeral" => args.world_dir = None,
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
                     \x20      [--screenshot PATH | --menu-screenshot PATH | --pause-screenshot PATH\n\
                     \x20       | --inventory-screenshot PATH]\n\
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

fn main() {
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

    let start = if let Some(raw) = &args.connect {
        // Skip the menu, connect straight to a remote server.
        StartMode::Direct(connect_remote(raw).unwrap_or_else(|e| panic!("{e}")))
    } else if args.screenshot.is_some() && !args.menu_screenshot {
        // Automated world verification: straight into singleplayer.
        StartMode::Direct(start_singleplayer(
            args.seed,
            args.world_dir.clone(),
            args.game_mode,
            server_slot.clone(),
        ))
    } else {
        // Normal boot: title menu. The singleplayer hook may be called
        // repeatedly (start, back to title, start again); each call spawns a
        // fresh server on the same world dir (the previous one saves and
        // exits when its only client says goodbye).
        let slot = server_slot.clone();
        let (seed, world_dir, game_mode) = (args.seed, args.world_dir.clone(), args.game_mode);
        StartMode::Menu(MenuHooks {
            start_singleplayer: Some(Box::new(move || {
                start_singleplayer(seed, world_dir.clone(), game_mode, slot.clone())
            })),
            connect: Box::new(connect_remote),
        })
    };

    tsumiki_client::run_client(ClientOptions {
        screenshot: args.screenshot.clone(),
        menu_screenshot: args.menu_screenshot,
        pause_screenshot: args.pause_screenshot,
        inventory_screenshot: args.inventory_screenshot,
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

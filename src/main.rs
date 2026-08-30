//! Launcher (design.md §1.1).
//!
//! Modes:
//! - default: singleplayer — in-process server + client over the local
//!   channel transport.
//! - `--server`: dedicated headless server over UDP.
//! - `--connect <addr>`: client only, connecting to a remote server.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tsumiki_net::DEFAULT_PORT;

struct Args {
    seed: u64,
    screenshot: Option<PathBuf>,
    world_dir: Option<PathBuf>,
    server: bool,
    port: u16,
    connect: Option<SocketAddr>,
    name: String,
    spawn_xz: Option<(f32, f32)>,
}

fn parse_args() -> Args {
    let mut args = Args {
        seed: 42,
        screenshot: None,
        world_dir: Some(PathBuf::from("world")),
        server: false,
        port: DEFAULT_PORT,
        connect: None,
        name: "player".to_string(),
        spawn_xz: None,
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
            "--world" => args.world_dir = Some(PathBuf::from(next("--world", &mut it))),
            "--ephemeral" => args.world_dir = None,
            "--server" => args.server = true,
            "--port" => {
                args.port = next("--port", &mut it)
                    .parse()
                    .expect("--port value must be a port number")
            }
            "--connect" => {
                let raw = next("--connect", &mut it);
                // Bare IPs get the default port appended.
                let parsed = raw
                    .parse()
                    .or_else(|_| format!("{raw}:{DEFAULT_PORT}").parse());
                args.connect = Some(parsed.expect("--connect value must be IP or IP:PORT"));
            }
            "--name" => args.name = next("--name", &mut it),
            "--spawn" => {
                let x = next("--spawn", &mut it).parse().expect("--spawn needs X Z");
                let z = next("--spawn", &mut it).parse().expect("--spawn needs X Z");
                args.spawn_xz = Some((x, z));
            }
            other => {
                eprintln!("unknown argument: {other}");
                eprintln!(
                    "usage: tsumiki [--seed N] [--world DIR | --ephemeral] [--screenshot PATH]\n\
                     \x20      [--server [--port P]] [--connect ADDR[:PORT]] [--name NAME] [--spawn X Z]"
                );
                std::process::exit(2);
            }
        }
    }
    args
}

fn client_options(args: &Args) -> tsumiki_client::ClientOptions {
    tsumiki_client::ClientOptions {
        screenshot: args.screenshot.clone(),
        name: args.name.clone(),
        spawn_xz: args.spawn_xz.map(|(x, z)| bevy_math::Vec2::new(x, z)),
    }
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
                ..Default::default()
            },
        );
        return;
    }

    if let Some(addr) = args.connect {
        // Remote client: no local server.
        let transport = tsumiki_net::NetClientTransport::connect(addr)
            .unwrap_or_else(|e| panic!("failed to start connecting to {addr}: {e}"));
        tsumiki_client::run_client(transport, client_options(&args));
        return;
    }

    // Singleplayer: in-process server + client.
    let options = client_options(&args);
    let (server_transport, client_transport) = tsumiki_protocol::local::pair();
    let config = tsumiki_server::ServerConfig {
        seed: args.seed,
        world_dir: args.world_dir,
        ..Default::default()
    };
    let server = std::thread::spawn(move || tsumiki_server::run_server(server_transport, config));

    // The client must run on the main thread (winit requirement).
    tsumiki_client::run_client(client_transport, options);

    // The client sends Goodbye on exit; give the server a moment to save the
    // world and shut down before the process dies.
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

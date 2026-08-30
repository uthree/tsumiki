//! Launcher: singleplayer spawns an in-process server and connects the
//! client to it over the local transport (design.md §1.1).

use std::path::PathBuf;
use std::time::{Duration, Instant};

fn main() {
    let mut seed: u64 = 42;
    let mut screenshot: Option<PathBuf> = None;
    let mut world_dir: Option<PathBuf> = Some(PathBuf::from("world"));

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seed" => {
                let value = args.next().expect("--seed needs a value");
                seed = value.parse().expect("--seed value must be an integer");
            }
            "--screenshot" => {
                let value = args.next().expect("--screenshot needs a path");
                screenshot = Some(PathBuf::from(value));
            }
            "--world" => {
                let value = args.next().expect("--world needs a directory path");
                world_dir = Some(PathBuf::from(value));
            }
            "--ephemeral" => {
                world_dir = None;
            }
            other => {
                eprintln!("unknown argument: {other}");
                eprintln!(
                    "usage: tsumiki [--seed N] [--world DIR | --ephemeral] [--screenshot PATH]"
                );
                std::process::exit(2);
            }
        }
    }

    let (server_transport, client_transport) = tsumiki_protocol::local::pair();

    let config = tsumiki_server::ServerConfig {
        seed,
        world_dir,
        ..Default::default()
    };
    let server = std::thread::spawn(move || tsumiki_server::run_server(server_transport, config));

    // The client must run on the main thread (winit requirement).
    tsumiki_client::run_client(
        client_transport,
        tsumiki_client::ClientOptions { screenshot },
    );

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

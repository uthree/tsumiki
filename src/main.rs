//! Launcher: singleplayer spawns an in-process server and connects the
//! client to it over the local transport (design.md §1.1).

use std::path::PathBuf;

fn main() {
    let mut seed: u64 = 42;
    let mut screenshot: Option<PathBuf> = None;

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
            other => {
                eprintln!("unknown argument: {other}");
                eprintln!("usage: tsumiki [--seed N] [--screenshot PATH]");
                std::process::exit(2);
            }
        }
    }

    let (server_transport, client_transport) = tsumiki_protocol::local::pair();

    let config = tsumiki_server::ServerConfig {
        seed,
        ..Default::default()
    };
    std::thread::spawn(move || tsumiki_server::run_server(server_transport, config));

    // The client must run on the main thread (winit requirement).
    tsumiki_client::run_client(client_transport, tsumiki_client::ClientOptions { screenshot });
}

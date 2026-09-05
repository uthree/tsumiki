# tsumiki
A 3D sandbox game set in a voxel-based world.

## Run

```
cargo run
```

Pick Singleplayer to see your worlds, create one (choosing its name, seed and
game mode), or delete one. Worlds are stored under `worlds/<name>/`.

Multiplayer: run a dedicated server with `cargo run -- --server`, then
connect from other machines with `cargo run -- --connect <ip> --name <you>`.

See [controls](doc/controls.md) for movement, creative flight, zoom, and the
RGB lighting demo.

CLI flags (`--world`, `--seed`, `--mode`, `--connect`, the screenshot flags)
exist for scripting and automated verification; pass an unknown flag to print
the full list.

See `doc/design.md` for the architecture.

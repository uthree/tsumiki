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

Controls: left click to grab the mouse (Escape to release), WASD to move,
Space to jump, 1-9 to pick a hotbar slot, left click to mine, right click to
place (or to open a chest or crafting table), E for the inventory, Q to drop.
Creative mode adds F to fly.

CLI flags (`--world`, `--seed`, `--mode`, `--connect`, the screenshot flags)
exist for scripting and automated verification; pass an unknown flag to print
the full list.

See `doc/design.md` for the architecture.

# tsumiki
A 3D sandbox game set in a voxel-based world.

## Run

```
cargo run
```

Optional flags: `--seed <N>` selects the world seed; `--mode survival|creative`
sets a new world's rules (existing worlds keep theirs).

Multiplayer: run a dedicated server with `cargo run -- --server`, then
connect from other machines with `cargo run -- --connect <ip> --name <you>`.

Controls: left click to grab the mouse (Escape to release), WASD to move,
Space to jump, 1-9 to pick a hotbar slot, left click to mine, right click to
place (or to open a chest or crafting table), E for the inventory, Q to drop.
Creative mode adds F to fly.

See `doc/design.md` for the architecture.

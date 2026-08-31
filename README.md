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

Controls: left click to grab the mouse (Escape to release), WASD to fly,
Space/Ctrl for up/down, Shift to boost.

See `doc/design.md` for the architecture.

# Controls

Click the game window to capture the mouse. Escape opens the pause menu and
releases it. Gameplay shortcuts are active while the mouse is captured.

| Action | Control |
| --- | --- |
| Look / move | Mouse / WASD |
| Jump / swim upward | Space |
| Toggle creative flight | Double-tap Space within 300 ms, or press F |
| Ascend / descend while flying | Space / Shift |
| Fly faster | Hold Ctrl while moving |
| Zoom 4× | Hold C; release to restore the normal view |
| Select a hotbar slot | 1–9 or mouse wheel |
| Mine / place or use a block | Hold left click / right click |
| Eat | Select bread or toast, then right-click |
| Till soil | Hold a shovel and right-click the top of grass or dirt |
| Plant wheat | Select wheat seeds and right-click farmland |
| Open a factory machine | Right-click the machine |
| Inventory | E |
| Drop an item | Q |

Flight is available in creative mode. Flight speed increases fourfold while
Ctrl is held. Zoom works in both modes and reduces mouse sensitivity to help
aim. Opening a menu, dying, or releasing the cursor cancels zoom; changing
FOV in Settings still sets the normal view.

In survival, a successfully harvested block becomes a dropped item in the
world. Walk within 1.5 blocks to collect it after its 0.5-second pickup delay.
Only the amount that fits is collected; the remainder stays on the ground.
Harvest requirements
and tool wear still apply. Creative mining clears blocks without producing
their drops; stored chest and furnace contents drop in either mode.

Block inventory icons show their textured top and two sides in an isometric
view. Materials and tools keep their distinct flat silhouettes.

The factory panel provides Rotate, Recipe / item, Deposit, Withdraw, and
Run / stop. Deposit uses the stack held by the inventory cursor; Withdraw
collects whole output into the inventory. See [farming.md](farming.md) for
food and crop growth, and [factories.md](factories.md) for production-line
setup, machine controls, and offline production.

## RGB lighting demo

Open the creative inventory with E. Move **Demo Red Light**, **Demo Green
Light**, or **Demo Blue Light** from the backpack into the hotbar and place
them. The three lamps emit pure red, green, and blue light. Put them close
together in an enclosed room to see the channels combine on nearby walls
and floors. Removing a lamp recalculates the light normally.

Demo lamps have no crafting recipes, natural generation, or harvest drops,
and the server rejects their placement in survival. To retire the demo,
set `DEMO_LIGHTS_ENABLED` to `false` in `crates/world/src/item.rs` and rebuild.
This hides the items from the creative inventory and disables new placement.
Keep the appended registry IDs so worlds with existing lamps remain readable.

## Reproducing visual checks

```sh
cargo test --workspace write_rgb_verification_world -- --ignored --nocapture
cargo run -- --world target/controls-qa/rgb --cave-screenshot target/controls-qa/rgb.png
cargo run -- --ephemeral --seed 2026 --inventory-screenshot target/controls-qa/inventory.png
cargo run -- --ephemeral --seed 2026 --screenshot target/controls-qa/normal.png
cargo run -- --ephemeral --seed 2026 --zoom-screenshot target/controls-qa/zoom.png
```

The last two captures use the same camera; the zoom capture holds C through
the normal input path. Control tests cover double-tap timing, creative-only
flight, ascent/descent, UI cancellation, FOV restoration, and sensitivity.

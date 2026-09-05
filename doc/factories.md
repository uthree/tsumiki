# Factories

Factories move material through finite buffers and directed connections.
Their production continues across chunk unloading and is advanced by the
elapsed wall-clock time when a saved world is reopened.

## Build an iron line

Craft the machine blocks at a crafting table, or use the creative
inventory. Place a miner directly above an iron ore block: it claims a
finite connected vein beneath it. Coal veins work too, producing coal.
The miner's panel shows the remaining ore.

Arrange these blocks next to each other at the same height, going east
(`+X`):

```text
Miner → Belt → Powered furnace → Factory storage
```

New blocks face east. The belt initially carries iron ore, the furnace
smelts iron ore, and storage holds iron ingots, so this line connects with
the default settings. Put a generator anywhere in the world to supply the
shared power grid. One generator supplies 4 power; the miner uses 1 and the
furnace uses 2. Additional demand shares the available power proportionally.

Right-click a machine to open its panel. It shows output direction,
power availability, buffer quantities, and their net rates of change.
Animated cargo above a belt follows the server's flow rate.

| Control | Effect |
| --- | --- |
| Rotate | Turn the output clockwise: east, south, west, north |
| Recipe / item | Cycle the selected recipe or carried item when buffers contain no whole items |
| Deposit | Transfer a matching stack from the inventory cursor into the input buffer |
| Withdraw | Collect up to 64 whole output items into the inventory, subject to available space |
| Run / stop | Enable or stop the machine |

Output connects to the adjacent compatible machine in the selected
direction. Use the belt's item setting when routing coal, bread, toast,
or ingots. Empty the buffers before changing recipes. For a cooking line,
select bread on a powered furnace and toast on its destination storage,
then deposit bread or feed it through matching belts.

## Capacity and production

| Component | Capacity or rate |
| --- | --- |
| Miner | 64 output items; 0.25 ore per second at full power |
| Powered furnace | 64 input and 64 output items |
| Iron smelting | 1 iron ore → 1 iron ingot in 10 seconds at full power |
| Cooking | 1 bread → 1 toast in 5 seconds at full power |
| Belt | 4 items |
| Factory storage | 4,096 items |
| Each directed connection | Up to 2 items per second |

Blocked output fills the available buffers and slows or stops upstream
production. Empty inputs and depleted ore likewise limit production.
Deposits and withdrawals transfer whole items. Breaking a machine drops
the whole items in its buffers; changing its selected item or breaking it
discards any fractional work below one item.
Large drops can be collected a little at a time: only the amount that fits
enters the inventory, and the remainder keeps its original expiry time.

## Simulation and saves

The server owns the factory graph and item quantities. Between events,
buffers keep constant rates anchored to a simulation time. A cached next
boundary makes an advance that crosses no boundary constant-time. A full
or empty buffer, depleted deposit, or graph edit triggers a deterministic
rate calculation for the graph.

Factories are saved independently of rendered chunks. Reopening a world
advances production through elapsed real time, including buffer limits
and finite ore exhaustion. Farming and hunger retain their saved state
through that closed-world interval and resume when the server runs again.

## Reproducing visual checks

```sh
cargo test -p tsumiki-server write_factory_verification_world -- --ignored --nocapture
cargo run -- --world target/m89-qa/factory --cave-screenshot target/m89-qa/factory.png
cargo run -- --world target/m89-qa/factory --factory-screenshot target/m89-qa/factory-panel.png
```

The panel capture opens a real machine through the server. The fixture has
a finite 256-cell iron vein; rerun its writer to reset it. See
[factory-performance.md](factory-performance.md) for the simulation probe.

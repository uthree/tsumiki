# Farming and food

Survival has a hunger gauge beside health. Select food in the hotbar and
right-click to eat, including while looking at the sky. Eating restores
hunger up to 20 and consumes one item when hunger is below maximum.

| Food | Preparation | Hunger restored |
| --- | --- | --- |
| Bread | Craft 3 wheat by hand | 5 |
| Toast | Cook 1 bread for 5 seconds in a furnace | 8 |

The ordinary furnace uses fuel. A powered furnace uses factory power for
the same conversion; see [factories.md](factories.md).

## Starting a wheat farm

1. Harvest grass blocks to collect wheat seeds along with dirt. Walk over
   the drops to pick them up.
2. Hold any shovel and right-click the top of grass or dirt to make
   farmland. Leave the block above it clear. Tilling wears the shovel in
   survival.
3. Select wheat seeds and place them on the farmland. Keep water within
   four blocks horizontally, at the farmland's height. A riverbank is a
   convenient starting site.
4. Give the plants light level 9 or higher through skylight or nearby
   torches. After 120 seconds in suitable conditions, the green sprouts
   become golden wheat. Harvest mature wheat for one wheat and two seeds,
   then replant. Harvesting a young crop returns its seed.

Growth pauses while water or light is insufficient and resumes when the
conditions recover. The saved growth timer advances while the server runs,
including outside the player's loaded view. Closing the world preserves
the timer; time spent with the world closed does not age crops. An open
pause menu leaves the server running.

## Hunger and recovery

Hunger falls gradually with time, movement, mining, tilling, and healing.
With at least 18 hunger, missing health regenerates by one point every two
seconds and uses energy. At zero hunger, starvation removes one health
point every four seconds and can cause death. Bread or toast interrupts
starvation; keep a food supply before a long mining trip.

Hunger and crop progress are saved with the world. Creative mode keeps the
survival gauges hidden and does not consume hunger. See
[controls.md](controls.md) for the other player controls.

## Reproducing the visual check

```sh
cargo test -p tsumiki-server write_survival_verification_world -- --ignored --nocapture
cargo run -- --world target/m89-qa/farm --cave-screenshot target/m89-qa/farm.png
```

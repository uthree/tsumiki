# Factory graph performance

The factory graph stores amounts at an anchor time plus constant rates. Calling
`advance_to` before the next predicted buffer boundary changes only its clock.
Production does not replay server ticks or individual items during offline
catch-up. A real boundary or a graph edit recomputes the simultaneous feasible
flows for the graph using a deterministic simplex solver.

## Reproducible timing probe

Run the ignored server test from the repository root:

```powershell
cargo test -p tsumiki-server --offline factory::tests::benchmark_factory_graph_boundaries -- --ignored --nocapture
```

The probe creates a directed chain containing one finite miner and the remaining
nodes as transport storage buffers. The terminal buffer holds one item; all
other transport buffers hold four. The miner produces 0.25 items per second,
and links carry up to two items per second. Setup excludes the first rate solve.
The boundary measurement advances past the terminal buffer filling at four
seconds, forcing one additional rate solve. The steady measurement makes
100,000 calls within the first second, before any boundary occurs.

Observed on September 6, 2026, on Windows 11 with an AMD Ryzen 7 9800X3D,
using the repository's development profile (`opt-level = 1` for workspace code):

| Nodes | Graph setup | Initial rate solve | 100,000 steady advances | One boundary solve |
| ---: | ---: | ---: | ---: | ---: |
| 100 | 0.059 ms | 0.45 ms | 0.245 ms | 0.35 ms |
| 250 | 0.070 ms | 2.05 ms | 0.246 ms | 1.98 ms |
| 500 | 0.142 ms | 7.93 ms | 0.245 ms | 8.15 ms |

These are one local timing run, not a latency guarantee. They isolate the pure
graph operations; voxel reconciliation, networking, rendering, save I/O, and
snapshot allocation are excluded. Steady advancement is independent of graph
size. Boundary work grows with graph size and topology. An offline interval may
cross several boundaries, so its cost depends on those state changes rather
than the interval's elapsed seconds or produced item count.

Automated conservation tests also cover 40 seeded branched and cyclic graphs,
each advanced once across one million seconds and compared with segmented
advances. Every case remains within buffer capacities and conserves its finite
source plus stored items; empty cycles never start circulating phantom items.

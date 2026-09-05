//! Bounded, asynchronous derivation of authoritative chunk lighting.
//!
//! A job solves one full-height column plus a 15-block halo. Its block
//! snapshot includes every edit in that region; missing pristine chunks are
//! regenerated on the worker. No chunk-loading order or neighboring light
//! cache entry can change the answer. Edits cancel affected snapshots and
//! rebuild from sources, including both shadow creation and source removal.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};

use bevy::prelude::Resource;
use bevy_math::{IVec2, IVec3, UVec3};
use tsumiki_protocol::{ClientId, ServerToClient, ServerTransport};
use tsumiki_world::light::{LightChunk, LightMaterial, solve_region};
use tsumiki_world::{
    BlockId, BlockRegistry, CHUNK_SIZE, Chunk, WORLD_HEIGHT_BLOCKS, WORLD_HEIGHT_CHUNKS,
    WorldGenerator,
};

use crate::{ChunkCache, ClientState, INTEREST_RADIUS};

const HALO: i32 = 15;
const REGION_WIDTH: usize = CHUNK_SIZE + HALO as usize * 2;
const MAX_CACHED_COLUMNS: usize = 2048;
const MAX_PENDING_COLUMNS: usize = 4096;
const MAX_SUBSCRIPTIONS_PER_CLIENT: usize = 8192;
const MAX_JOBS: usize = 2;

type Work = Box<dyn FnOnce() + Send>;

/// One process-wide pool also bounds concurrency when many test servers
/// coexist. The queue is bounded, and dropped servers cancel their work.
fn workers() -> &'static mpsc::SyncSender<Work> {
    static WORKERS: OnceLock<mpsc::SyncSender<Work>> = OnceLock::new();
    WORKERS.get_or_init(|| {
        let (send, receive) = mpsc::sync_channel::<Work>(MAX_JOBS * 2);
        let receive = Arc::new(Mutex::new(receive));
        for index in 0..MAX_JOBS {
            let receive = Arc::clone(&receive);
            std::thread::Builder::new()
                .name(format!("voxel-light-{index}"))
                .spawn(move || {
                    loop {
                        let work = receive.lock().expect("lighting queue poisoned").recv();
                        let Ok(work) = work else { break };
                        // A failed task disconnects its reply channel. The
                        // server retries without losing a worker permanently.
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
                    }
                })
                .expect("failed to start lighting worker");
        }
        send
    })
}

struct Job {
    receive: Mutex<mpsc::Receiver<Vec<LightChunk>>>,
    cancelled: Arc<AtomicBool>,
    /// An edit cancelled this snapshot; rebuild it before the initial-load
    /// backlog as soon as its worker releases the slot.
    retry_first: bool,
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

struct CachedColumn {
    chunks: Vec<LightChunk>,
    touched: u64,
}

#[derive(Clone, Copy)]
struct Subscription {
    requested: u64,
    needs_light: bool,
}

#[derive(Resource, Default)]
pub(crate) struct Lighting {
    cached: HashMap<IVec2, CachedColumn>,
    jobs: HashMap<IVec2, Job>,
    pending: VecDeque<IVec2>,
    pending_set: HashSet<IVec2>,
    subscriptions: HashMap<ClientId, HashMap<IVec3, Subscription>>,
}

fn column_of(pos: IVec3) -> IVec2 {
    IVec2::new(pos.x, pos.z)
}

impl Lighting {
    fn enqueue(&mut self, column: IVec2) {
        if self.pending.len() < MAX_PENDING_COLUMNS
            && !self.jobs.contains_key(&column)
            && self.pending_set.insert(column)
        {
            self.pending.push_back(column);
        }
    }

    /// Promote an edited column without adding an unbounded priority queue.
    /// If the queue is full, its last initial-load request remains recorded
    /// in subscriptions and is refilled after a slot becomes available.
    fn prioritize(&mut self, column: IVec2) {
        if let Some(job) = self.jobs.get_mut(&column) {
            job.retry_first = true;
            return;
        }
        if self.pending_set.contains(&column) {
            self.pending.retain(|&pending| pending != column);
        } else {
            if self.pending.len() == MAX_PENDING_COLUMNS
                && let Some(displaced) = self.pending.pop_back()
            {
                self.pending_set.remove(&displaced);
            }
            self.pending_set.insert(column);
        }
        self.pending.push_front(column);
    }

    /// Called after sending block data. Cached lighting follows immediately;
    /// otherwise a bounded background job supplies it on a later tick.
    pub(crate) fn request<T: ServerTransport>(
        &mut self,
        client: ClientId,
        pos: IVec3,
        tick: u64,
        transport: &mut T,
    ) {
        let subscriptions = self.subscriptions.entry(client).or_default();
        if subscriptions.len() >= MAX_SUBSCRIPTIONS_PER_CLIENT
            && !subscriptions.contains_key(&pos)
            && let Some(oldest) = subscriptions
                .iter()
                .min_by_key(|(_, s)| s.requested)
                .map(|(&p, _)| p)
        {
            subscriptions.remove(&oldest);
        }
        let mut subscription = Subscription {
            requested: tick,
            needs_light: true,
        };
        let column = column_of(pos);
        if let Some(cached) = self.cached.get_mut(&column) {
            cached.touched = tick;
            transport.send(
                client,
                ServerToClient::LightChunkData {
                    pos,
                    light: cached.chunks[pos.y as usize].clone(),
                },
            );
            subscription.needs_light = false;
        }
        subscriptions.insert(pos, subscription);
        if subscription.needs_light {
            self.enqueue(column);
        }
    }

    pub(crate) fn invalidate(&mut self, block_pos: IVec3) {
        let size = CHUNK_SIZE as i32;
        for x in (block_pos.x - HALO).div_euclid(size)..=(block_pos.x + HALO).div_euclid(size) {
            for z in (block_pos.z - HALO).div_euclid(size)..=(block_pos.z + HALO).div_euclid(size) {
                let column = IVec2::new(x, z);
                self.cached.remove(&column);
                if let Some(job) = self.jobs.get(&column) {
                    job.cancelled.store(true, Ordering::Relaxed);
                }
                let mut needed = false;
                for subscriptions in self.subscriptions.values_mut() {
                    for (pos, subscription) in subscriptions {
                        if column_of(*pos) == column {
                            subscription.needs_light = true;
                            needed = true;
                        }
                    }
                }
                if needed {
                    self.prioritize(column);
                }
            }
        }
    }

    /// Work and wire output occur only for requested nearby chunks. Receiving
    /// time-of-day updates never invalidates this derived light field.
    pub(crate) fn tick<T: ServerTransport>(
        &mut self,
        clients: &HashMap<ClientId, ClientState>,
        cache: &ChunkCache,
        world_gen: &WorldGenerator,
        registry: &BlockRegistry,
        tick: u64,
        transport: &mut T,
    ) {
        self.subscriptions.retain(|client, subscriptions| {
            let Some(state) = clients.get(client) else {
                return false;
            };
            if let Some(save) = state.save {
                // Keep one extra chunk around the maximum supported view
                // distance, including neighbor data requested for meshing.
                let radius = INTEREST_RADIUS + CHUNK_SIZE as f32 * 2.0;
                subscriptions.retain(|pos, _| {
                    let center = pos.as_vec3() * CHUNK_SIZE as f32 + CHUNK_SIZE as f32 * 0.5;
                    (center.x - save.pos.x).abs() <= radius
                        && (center.z - save.pos.z).abs() <= radius
                });
            }
            !subscriptions.is_empty()
        });

        let mut finished = Vec::new();
        for (&column, job) in &mut self.jobs {
            match job
                .receive
                .get_mut()
                .expect("lighting result poisoned")
                .try_recv()
            {
                Ok(chunks) => finished.push((column, Some(chunks))),
                Err(mpsc::TryRecvError::Disconnected) => finished.push((column, None)),
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        for (column, result) in finished {
            let job = self.jobs.remove(&column).expect("completed job exists");
            if job.cancelled.load(Ordering::Relaxed) {
                if job.retry_first {
                    self.prioritize(column);
                }
                continue;
            }
            if let Some(chunks) = result {
                for (&client, subscriptions) in &mut self.subscriptions {
                    for (&pos, subscription) in subscriptions {
                        if column_of(pos) == column && subscription.needs_light {
                            transport.send(
                                client,
                                ServerToClient::LightChunkData {
                                    pos,
                                    light: chunks[pos.y as usize].clone(),
                                },
                            );
                            subscription.needs_light = false;
                        }
                    }
                }
                self.cached.insert(
                    column,
                    CachedColumn {
                        chunks,
                        touched: tick,
                    },
                );
            }
        }

        // Unserved subscriptions survive a full queue, cancelled snapshots,
        // and cache eviction. Refilling from them prevents permanently dark
        // chunks when multiple players join faster than workers can solve.
        let needed: HashSet<_> = self
            .subscriptions
            .values()
            .flat_map(|s| {
                s.iter()
                    .filter(|(_, s)| s.needs_light)
                    .map(|(&pos, _)| column_of(pos))
            })
            .collect();
        self.pending.retain(|column| needed.contains(column));
        self.pending_set.retain(|column| needed.contains(column));
        for &column in &needed {
            self.enqueue(column);
        }
        for (&column, job) in &self.jobs {
            if !needed.contains(&column) {
                job.cancelled.store(true, Ordering::Relaxed);
            }
        }

        while self.jobs.len() < MAX_JOBS {
            let Some(column) = self.pending.pop_front() else {
                break;
            };
            self.pending_set.remove(&column);
            let snapshot = snapshot_column(column, cache);
            let world_gen = world_gen.clone();
            let materials: Vec<_> = (0..registry.len())
                .map(|index| {
                    let block = registry.get(BlockId(index as u16));
                    LightMaterial {
                        opacity: block.light_opacity,
                        emission: block.light_emission,
                    }
                })
                .collect();
            let cancelled = Arc::new(AtomicBool::new(false));
            let worker_cancelled = Arc::clone(&cancelled);
            let (send, receive) = mpsc::channel();
            let work = Box::new(move || {
                if !worker_cancelled.load(Ordering::Relaxed) {
                    let chunks = build_column(column, snapshot, &world_gen, &materials);
                    if !worker_cancelled.load(Ordering::Relaxed) {
                        let _ = send.send(chunks);
                    }
                }
            });
            if workers().try_send(work).is_err() {
                // Preserve the selected request's position, including edit
                // priority, while another server occupies the shared pool.
                self.prioritize(column);
                break;
            }
            self.jobs.insert(
                column,
                Job {
                    receive: Mutex::new(receive),
                    cancelled,
                    retry_first: false,
                },
            );
        }
        self.evict();
    }

    fn evict(&mut self) {
        if self.cached.len() <= MAX_CACHED_COLUMNS {
            return;
        }
        let mut oldest: Vec<_> = self
            .cached
            .iter()
            .map(|(&column, cached)| (column, cached.touched))
            .collect();
        oldest.sort_unstable_by_key(|(_, tick)| *tick);
        for (column, _) in oldest
            .into_iter()
            .take(self.cached.len() - MAX_CACHED_COLUMNS)
        {
            self.cached.remove(&column);
        }
    }
}

/// Snapshot only the nine source columns, including edited chunks that are
/// held outside player view. Workers regenerate missing pristine sources.
fn snapshot_column(column: IVec2, cache: &ChunkCache) -> HashMap<IVec3, Chunk> {
    let mut snapshot = HashMap::new();
    for x in column.x - 1..=column.x + 1 {
        for z in column.y - 1..=column.y + 1 {
            for y in 0..WORLD_HEIGHT_CHUNKS {
                let pos = IVec3::new(x, y, z);
                if let Some(chunk) = cache.chunks.get(&pos) {
                    snapshot.insert(pos, chunk.clone());
                }
            }
        }
    }
    snapshot
}

fn build_column(
    column: IVec2,
    mut chunks: HashMap<IVec3, Chunk>,
    world_gen: &WorldGenerator,
    materials: &[LightMaterial],
) -> Vec<LightChunk> {
    // Vector lookup inside the dense solve avoids half a million hash-map
    // lookups. Chunk order is ((y * 3) + z) * 3 + x.
    let mut sources = Vec::with_capacity(9 * WORLD_HEIGHT_CHUNKS as usize);
    for y in 0..WORLD_HEIGHT_CHUNKS {
        for z in column.y - 1..=column.y + 1 {
            for x in column.x - 1..=column.x + 1 {
                let pos = IVec3::new(x, y, z);
                sources.push(
                    chunks
                        .remove(&pos)
                        .unwrap_or_else(|| world_gen.generate_chunk(pos)),
                );
            }
        }
    }
    let size = UVec3::new(
        REGION_WIDTH as u32,
        WORLD_HEIGHT_BLOCKS as u32,
        REGION_WIDTH as u32,
    );
    let start = CHUNK_SIZE - HALO as usize;
    let values = solve_region(size, |p| {
        let x = p.x as usize + start;
        let z = p.z as usize + start;
        let y = p.y as usize;
        let source = &sources[((y / CHUNK_SIZE * 3) + z / CHUNK_SIZE) * 3 + x / CHUNK_SIZE];
        let block = source.get(UVec3::new(
            (x % CHUNK_SIZE) as u32,
            (y % CHUNK_SIZE) as u32,
            (z % CHUNK_SIZE) as u32,
        ));
        materials[block.0 as usize]
    });
    let mut result = Vec::with_capacity(WORLD_HEIGHT_CHUNKS as usize);
    for chunk_y in 0..WORLD_HEIGHT_CHUNKS as usize {
        let mut center = Vec::with_capacity(CHUNK_SIZE.pow(3));
        for y in chunk_y * CHUNK_SIZE..(chunk_y + 1) * CHUNK_SIZE {
            for z in HALO as usize..HALO as usize + CHUNK_SIZE {
                let start = (y * REGION_WIDTH + z) * REGION_WIDTH + HALO as usize;
                center.extend_from_slice(&values[start..start + CHUNK_SIZE]);
            }
        }
        result.push(LightChunk::from_packed(&center));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use tsumiki_protocol::ClientToServer;
    use tsumiki_world::blocks;
    use tsumiki_world::light::LightValue;

    #[derive(Default)]
    struct Transport(Vec<(ClientId, ServerToClient)>);

    impl ServerTransport for Transport {
        fn try_recv(&mut self) -> Option<(ClientId, ClientToServer)> {
            None
        }
        fn send(&mut self, to: ClientId, msg: ServerToClient) {
            self.0.push((to, msg));
        }
    }

    fn tunnel() -> HashMap<IVec3, Chunk> {
        let mut sources = HashMap::new();
        for x in -1..=2 {
            for z in -1..=1 {
                for y in 0..WORLD_HEIGHT_CHUNKS {
                    let mut chunk = Chunk::filled(blocks::STONE);
                    if y == 1 && z == 0 {
                        for local_x in 0..CHUNK_SIZE as u32 {
                            chunk.set(UVec3::new(local_x, 0, 10), blocks::AIR);
                        }
                    }
                    sources.insert(IVec3::new(x, y, z), chunk);
                }
            }
        }
        sources
    }

    #[test]
    fn initial_light_and_source_removal_reach_both_subscribed_clients() {
        let mut lighting = Lighting::default();
        let mut cache = ChunkCache {
            chunks: tunnel(),
            ..Default::default()
        };
        cache
            .chunks
            .get_mut(&IVec3::new(0, 1, 0))
            .unwrap()
            .set(UVec3::new(31, 0, 10), blocks::TORCH);
        let clients = [
            (1, ClientState::default()),
            (2, ClientState::default()),
            (3, ClientState::default()),
        ]
        .into_iter()
        .collect();
        let mut transport = Transport::default();
        let pos = IVec3::new(1, 1, 0);
        lighting.request(1, pos, 0, &mut transport);
        lighting.request(2, pos, 0, &mut transport);
        let world_gen = WorldGenerator::new(4);
        let registry = BlockRegistry::prototype();
        for expected in [LightValue::new([14, 11, 7], 0), LightValue::DARK] {
            let deadline = Instant::now() + Duration::from_secs(5);
            while transport.0.len() < 2 && Instant::now() < deadline {
                lighting.tick(&clients, &cache, &world_gen, &registry, 1, &mut transport);
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(transport.0.len(), 2, "both viewers must receive the result");
            for (client, message) in transport.0.drain(..) {
                assert!(
                    client == 1 || client == 2,
                    "unsubscribed clients receive no light data"
                );
                let ServerToClient::LightChunkData {
                    pos: received,
                    light,
                } = message
                else {
                    panic!("unexpected message")
                };
                assert_eq!(received, pos);
                assert_eq!(light.get(UVec3::new(0, 0, 10)), expected);
            }
            cache
                .chunks
                .get_mut(&IVec3::new(0, 1, 0))
                .unwrap()
                .set(UVec3::new(31, 0, 10), blocks::AIR);
            lighting.invalidate(IVec3::new(31, 32, 10));
        }
    }

    #[test]
    fn an_edit_during_an_async_solve_never_publishes_the_stale_snapshot() {
        let mut lighting = Lighting::default();
        let mut cache = ChunkCache {
            chunks: tunnel(),
            ..Default::default()
        };
        cache
            .chunks
            .get_mut(&IVec3::new(0, 1, 0))
            .unwrap()
            .set(UVec3::new(31, 0, 10), blocks::TORCH);
        let clients = [(1, ClientState::default())].into_iter().collect();
        let mut transport = Transport::default();
        let world_gen = WorldGenerator::new(4);
        let registry = BlockRegistry::prototype();
        lighting.request(1, IVec3::new(1, 1, 0), 0, &mut transport);
        lighting.tick(&clients, &cache, &world_gen, &registry, 1, &mut transport);
        assert!(transport.0.is_empty());
        cache
            .chunks
            .get_mut(&IVec3::new(0, 1, 0))
            .unwrap()
            .set(UVec3::new(31, 0, 10), blocks::AIR);
        lighting.invalidate(IVec3::new(31, 32, 10));
        let deadline = Instant::now() + Duration::from_secs(5);
        while transport.0.is_empty() && Instant::now() < deadline {
            lighting.tick(&clients, &cache, &world_gen, &registry, 2, &mut transport);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(transport.0.len(), 1);
        let ServerToClient::LightChunkData { light, .. } = &transport.0[0].1 else {
            panic!("unexpected message")
        };
        assert_eq!(light.get(UVec3::new(0, 0, 10)), LightValue::DARK);
    }

    #[test]
    fn an_edit_cancels_inflight_neighbor_columns_and_marks_all_vertical_chunks() {
        let mut lighting = Lighting::default();
        let (_, receive) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        lighting.jobs.insert(
            IVec2::new(-1, 0),
            Job {
                receive: Mutex::new(receive),
                cancelled: Arc::clone(&cancelled),
                retry_first: false,
            },
        );
        let chunks = (0..WORLD_HEIGHT_CHUNKS)
            .map(|y| {
                (
                    IVec3::new(-1, y, 0),
                    Subscription {
                        requested: 0,
                        needs_light: false,
                    },
                )
            })
            .collect();
        lighting.subscriptions.insert(1, chunks);
        lighting.invalidate(IVec3::new(0, 96, 10));
        assert!(cancelled.load(Ordering::Relaxed));
        assert!(lighting.jobs[&IVec2::new(-1, 0)].retry_first);
        assert!(lighting.subscriptions[&1].values().all(|s| s.needs_light));
    }

    #[test]
    fn an_edit_moves_a_column_ahead_of_a_full_initial_load_queue() {
        let mut lighting = Lighting::default();
        let mut subscriptions = HashMap::new();
        for x in 1..=MAX_PENDING_COLUMNS as i32 {
            lighting.enqueue(IVec2::new(x, 0));
            subscriptions.insert(
                IVec3::new(x, 1, 0),
                Subscription {
                    requested: 0,
                    needs_light: true,
                },
            );
        }
        subscriptions.insert(
            IVec3::new(0, 1, 0),
            Subscription {
                requested: 0,
                needs_light: false,
            },
        );
        lighting.subscriptions.insert(1, subscriptions);
        // This position affects only column zero; the 15-block halo stays
        // inside that column. Repeated edits must not duplicate its request.
        lighting.invalidate(IVec3::new(16, 40, 16));
        lighting.invalidate(IVec3::new(16, 40, 16));
        assert_eq!(lighting.pending.front(), Some(&IVec2::ZERO));
        assert_eq!(lighting.pending.len(), MAX_PENDING_COLUMNS);
        assert_eq!(lighting.pending_set.len(), MAX_PENDING_COLUMNS);
        let displaced = IVec3::new(MAX_PENDING_COLUMNS as i32, 1, 0);
        assert!(!lighting.pending_set.contains(&column_of(displaced)));
        assert!(
            lighting.subscriptions[&1][&displaced].needs_light,
            "the displaced initial request must remain eligible for retry"
        );
    }

    #[test]
    fn a_cancelled_inflight_edit_restarts_before_distant_initial_work() {
        let mut lighting = Lighting::default();
        let mut subscriptions = HashMap::new();
        for x in 1..=100 {
            lighting.enqueue(IVec2::new(x, 0));
            subscriptions.insert(
                IVec3::new(x, 1, 0),
                Subscription {
                    requested: 0,
                    needs_light: true,
                },
            );
        }
        subscriptions.insert(
            IVec3::new(0, 1, 0),
            Subscription {
                requested: 0,
                needs_light: true,
            },
        );
        subscriptions.insert(
            IVec3::new(1000, 1, 0),
            Subscription {
                requested: 0,
                needs_light: true,
            },
        );
        lighting.subscriptions.insert(1, subscriptions);

        let (finished, receive) = mpsc::channel();
        drop(finished);
        lighting.jobs.insert(
            IVec2::ZERO,
            Job {
                receive: Mutex::new(receive),
                cancelled: Arc::new(AtomicBool::new(false)),
                retry_first: false,
            },
        );
        // One occupied worker slot makes the next selected column directly
        // observable. This sender stays alive until after the assertions.
        let (_busy, receive) = mpsc::channel();
        lighting.jobs.insert(
            IVec2::new(1000, 0),
            Job {
                receive: Mutex::new(receive),
                cancelled: Arc::new(AtomicBool::new(false)),
                retry_first: false,
            },
        );
        lighting.invalidate(IVec3::new(16, 40, 16));
        let clients = [(1, ClientState::default())].into_iter().collect();
        let cache = ChunkCache {
            chunks: tunnel(),
            ..Default::default()
        };
        let mut transport = Transport::default();
        lighting.tick(
            &clients,
            &cache,
            &WorldGenerator::new(4),
            &BlockRegistry::prototype(),
            1,
            &mut transport,
        );

        assert!(
            transport.0.is_empty(),
            "the cancelled snapshot must not be sent"
        );
        assert!(
            lighting
                .jobs
                .keys()
                .all(|column| column.x == 0 || column.x == 1000),
            "distant initial work must not take the slot ahead of the edit"
        );
        assert!(
            lighting.jobs.contains_key(&IVec2::ZERO)
                || lighting.pending.front() == Some(&IVec2::ZERO),
            "an edited column starts next, or keeps first place if the shared worker pool is full"
        );
        assert!(lighting.jobs.len() <= MAX_JOBS);
    }

    #[test]
    fn a_loaded_torch_lights_the_adjacent_column_without_loading_order_dependencies() {
        let world_gen = WorldGenerator::new(4);
        let registry = BlockRegistry::prototype();
        let materials: Vec<_> = (0..registry.len())
            .map(|index| {
                let block = registry.get(BlockId(index as u16));
                LightMaterial {
                    opacity: block.light_opacity,
                    emission: block.light_emission,
                }
            })
            .collect();
        let mut sources = tunnel();
        sources
            .get_mut(&IVec3::new(0, 1, 0))
            .unwrap()
            .set(UVec3::new(31, 0, 10), blocks::TORCH);
        let left = build_column(IVec2::ZERO, sources.clone(), &world_gen, &materials);
        let right = build_column(IVec2::new(1, 0), sources.clone(), &world_gen, &materials);
        assert_eq!(
            left[1].get(UVec3::new(31, 0, 10)),
            LightValue::new([15, 12, 8], 0)
        );
        assert_eq!(
            right[1].get(UVec3::new(0, 0, 10)),
            LightValue::new([14, 11, 7], 0)
        );
        sources
            .get_mut(&IVec3::new(0, 1, 0))
            .unwrap()
            .set(UVec3::new(31, 0, 10), blocks::AIR);
        let removed = build_column(IVec2::new(1, 0), sources, &world_gen, &materials);
        assert_eq!(removed[1].get(UVec3::new(0, 0, 10)), LightValue::DARK);
    }

    #[test]
    fn light_cache_eviction_keeps_the_most_recent_columns() {
        let mut lighting = Lighting::default();
        for i in 0..MAX_CACHED_COLUMNS + 3 {
            lighting.cached.insert(
                IVec2::new(i as i32, 0),
                CachedColumn {
                    chunks: Vec::new(),
                    touched: i as u64,
                },
            );
        }
        lighting.evict();
        assert_eq!(lighting.cached.len(), MAX_CACHED_COLUMNS);
        assert!(!lighting.cached.contains_key(&IVec2::ZERO));
        assert!(
            lighting
                .cached
                .contains_key(&IVec2::new(MAX_CACHED_COLUMNS as i32 + 2, 0))
        );
    }
}

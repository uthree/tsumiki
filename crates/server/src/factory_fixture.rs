use super::*;

#[test]
#[ignore = "writes the M9 visual fixture under target/m89-qa/factory"]
fn write_factory_verification_world() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/m89-qa/factory");
    let mut persistence = Persistence::new(Some(dir.clone()), 9999.0);
    let mut chunks = HashMap::new();
    for x in 0..=1 {
        for y in 0..WORLD_HEIGHT_CHUNKS {
            let pos = IVec3::new(x, y, 0);
            let mut chunk = Chunk::filled(blocks::AIR);
            if y == 0 {
                for z in 0..32 {
                    for lx in 0..32 {
                        for ly in 0..8 {
                            chunk.set(
                                UVec3::new(lx, ly, z),
                                if ly == 7 {
                                    blocks::GRASS
                                } else {
                                    blocks::STONE
                                },
                            );
                        }
                    }
                }
            }
            chunks.insert(pos, chunk);
            persistence.mark_chunk_dirty(pos);
        }
    }
    let mut set = |pos: IVec3, block| {
        let (chunk, local) = split_block_pos(pos);
        chunks.get_mut(&chunk).unwrap().set(local.as_uvec3(), block);
    };
    // Exactly 256 connected ore cells; a long enough reserve for visual QA.
    for x in 24..56 {
        for z in 15..23 {
            set(IVec3::new(x, 7, z), blocks::IRON_ORE);
        }
    }
    let layout = [
        (27, blocks::MINER),
        (28, blocks::BELT),
        (29, blocks::POWERED_FURNACE),
        (30, blocks::FACTORY_STORAGE),
        (31, blocks::GENERATOR),
    ];
    for (x, block) in layout {
        set(IVec3::new(x, 8, 15), block);
    }
    for (x, block) in layout {
        persistence
            .factories
            .place(IVec3::new(x, 8, 15), block, |pos| {
                let (chunk, local) = split_block_pos(pos);
                chunks
                    .get(&chunk)
                    .map(|chunk| chunk.get(local.as_uvec3()))
                    .unwrap_or(blocks::AIR)
            });
    }
    persistence.factories.advance(20.0);
    for pos in persistence.factories.mined_blocks() {
        let (chunk, local) = split_block_pos(pos);
        chunks
            .get_mut(&chunk)
            .unwrap()
            .set(local.as_uvec3(), blocks::AIR);
    }
    let players = HashMap::from([(
        "player".to_string(),
        PlayerRecord {
            save: PlayerSave {
                pos: Vec3::new(29.5, 9.0, 20.5),
                yaw: 0.0,
                pitch: -0.2,
            },
            hp: MAX_HP,
            hunger: MAX_HUNGER,
            exhaustion: 0.0,
            main: vec![None; MAIN_INVENTORY_SIZE],
        },
    )]);
    persistence
        .save(
            42,
            GameMode::Creative,
            0.25,
            &players,
            &[],
            &[],
            &[],
            &chunks,
        )
        .unwrap();
    eprintln!("Factory verification world: {}", dir.display());
}

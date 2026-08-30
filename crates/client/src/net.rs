//! Client networking systems.
//!
//! Responsibilities (implemented by the client agent):
//! - Resource wrapping the `ClientTransport`.
//! - Startup: send `Hello`.
//! - Per frame: compute the camera's chunk position; request not-yet-requested
//!   chunks within the view distance (horizontal radius in chunks, all
//!   vertical chunks `0..WORLD_HEIGHT_CHUNKS`), nearest first, in bounded
//!   batches; mark them as requested in the `view::ChunkStore`.
//! - Per frame: drain received messages; insert `ChunkData` into the store.

use bevy::prelude::*;
use tsumiki_protocol::{ClientToServer, ClientTransport, ServerToClient};
use tsumiki_world::WORLD_HEIGHT_CHUNKS;

use crate::camera::FlyCam;
use crate::view::{ChunkStore, world_pos_to_chunk};

/// Horizontal view distance, in chunks. Chunks are meshed only when all
/// loaded neighbors are present, so the meshed radius is effectively one
/// less than this.
pub const VIEW_DISTANCE_CHUNKS: i32 = 8;

/// Upper bound on chunk positions requested in a single frame's message.
const MAX_CHUNK_REQUESTS_PER_FRAME: usize = 64;

/// Wraps the client's transport as a Bevy resource.
#[derive(Resource)]
struct Transport<T: ClientTransport>(T);

/// Wires the networking systems into `app`, taking ownership of `transport`.
pub fn install<T: ClientTransport>(app: &mut App, transport: T) {
    app.insert_resource(Transport(transport))
        .add_systems(Startup, send_hello::<T>)
        .add_systems(Update, (request_chunks::<T>, receive_chunks::<T>));
}

fn send_hello<T: ClientTransport>(mut transport: ResMut<Transport<T>>) {
    transport.0.send(ClientToServer::Hello {
        name: "player".to_string(),
    });
}

fn request_chunks<T: ClientTransport>(
    mut transport: ResMut<Transport<T>>,
    mut store: ResMut<ChunkStore>,
    cameras: Query<&Transform, With<FlyCam>>,
) {
    let Ok(transform) = cameras.single() else {
        return;
    };
    let cam_chunk = world_pos_to_chunk(transform.translation);

    let radius = VIEW_DISTANCE_CHUNKS;
    let radius_sq = radius * radius;
    let mut candidates: Vec<IVec3> = Vec::new();
    for dx in -radius..=radius {
        for dz in -radius..=radius {
            if dx * dx + dz * dz > radius_sq {
                continue;
            }
            for y in 0..WORLD_HEIGHT_CHUNKS {
                let pos = IVec3::new(cam_chunk.x + dx, y, cam_chunk.z + dz);
                if !store.chunks.contains_key(&pos) && !store.requested.contains(&pos) {
                    candidates.push(pos);
                }
            }
        }
    }
    if candidates.is_empty() {
        return;
    }

    candidates.sort_by_key(|&pos| {
        let d = pos - cam_chunk;
        d.x * d.x + d.y * d.y + d.z * d.z
    });
    candidates.truncate(MAX_CHUNK_REQUESTS_PER_FRAME);

    for &pos in &candidates {
        store.requested.insert(pos);
    }
    transport.0.send(ClientToServer::RequestChunks {
        positions: candidates,
    });
}

fn receive_chunks<T: ClientTransport>(
    mut transport: ResMut<Transport<T>>,
    mut store: ResMut<ChunkStore>,
) {
    while let Some(msg) = transport.0.try_recv() {
        match msg {
            ServerToClient::Welcome { .. } => {}
            ServerToClient::ChunkData { pos, chunk } => {
                store.chunks.insert(pos, chunk);
            }
        }
    }
}

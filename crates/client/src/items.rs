//! Dropped items share the inventory's pixel-art icons on double-sided,
//! alpha-masked cards. Propagated voxel light tints the texture while a
//! cosmetic bob and spin keep pickups visible.

use std::collections::HashMap;

use bevy::math::Affine2;
use bevy::prelude::*;
use tsumiki_world::ItemStack;

use crate::AppState;
use crate::entity_light::EntityLightTint;
use crate::item_icons::{self, ItemIcons};

/// Side length of a dropped item's icon card, in blocks.
const ITEM_SIZE: f32 = 0.45;
/// Vertical bob amplitude, in blocks.
const BOB_AMPLITUDE: f32 = 0.06;
/// Bob angular speed, radians/sec.
const BOB_SPEED: f32 = 2.0;
/// Spin speed, radians/sec.
const SPIN_SPEED: f32 = 1.2;

/// Per-entity bob state: the spawn height to bob around, and a phase offset
/// (derived from spawn time) so multiple items don't bob in lockstep.
#[derive(Component)]
struct DroppedItem {
    base_y: f32,
    phase: f32,
}

/// The geometry and atlas are shared; each material selects one icon cell.
#[derive(Resource)]
pub(crate) struct ItemMesh {
    mesh: Handle<Mesh>,
    atlas: Handle<Image>,
}

fn setup_item_mesh(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    icons: Res<ItemIcons>,
) {
    let mesh = meshes.add(Rectangle::new(ITEM_SIZE, ITEM_SIZE));
    commands.insert_resource(ItemMesh {
        mesh,
        atlas: icons.image.clone(),
    });
}

struct ItemEntry {
    entity: Entity,
    material: Handle<StandardMaterial>,
}

/// Live dropped items: server id -> entity/material. Populated/drained by
/// [`crate::net`] as `ItemSpawned`/`ItemDespawned` arrive.
#[derive(Resource, Default)]
pub(crate) struct DroppedItems(HashMap<u64, ItemEntry>);

/// Wires the dropped-item resources and animation system into `app`.
pub fn install(app: &mut App) {
    app.init_resource::<DroppedItems>()
        .add_systems(Startup, setup_item_mesh)
        .add_systems(OnExit(AppState::InGame), teardown_items)
        .add_systems(Update, animate_items.run_if(in_state(AppState::InGame)));
}

/// Spawns a dropped item's icon at `pos`.
/// Called by [`crate::net`] on `ServerToClient::ItemSpawned`. Replaces
/// (rather than leaks) any existing entry for `id`, defensively mirroring
/// [`crate::remote::spawn_remote_player`]'s same guard.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_item(
    commands: &mut Commands,
    item_mesh: &ItemMesh,
    materials: &mut Assets<StandardMaterial>,
    items: &mut DroppedItems,
    now: f32,
    id: u64,
    pos: Vec3,
    stack: ItemStack,
) {
    if items.0.contains_key(&id) {
        despawn_item(commands, materials, items, id);
    }

    let rect = item_icons::rect(stack.item);
    let atlas_size = item_icons::ATLAS_SIZE;
    let material = materials.add(StandardMaterial {
        base_color: Color::BLACK,
        base_color_texture: Some(item_mesh.atlas.clone()),
        uv_transform: Affine2::from_scale_angle_translation(
            rect.size() / atlas_size,
            0.0,
            rect.min / atlas_size,
        ),
        alpha_mode: AlphaMode::Mask(0.5),
        cull_mode: None,
        double_sided: true,
        unlit: true,
        perceptual_roughness: 1.0,
        ..default()
    });

    let entity = commands
        .spawn((
            Mesh3d(item_mesh.mesh.clone()),
            MeshMaterial3d(material.clone()),
            Transform::from_translation(pos),
            EntityLightTint(Color::WHITE),
            DroppedItem {
                base_y: pos.y,
                phase: now * BOB_SPEED,
            },
        ))
        .id();

    items.0.insert(id, ItemEntry { entity, material });
}

/// Despawns a dropped item and frees its material. Called by [`crate::net`]
/// on `ServerToClient::ItemDespawned`. A no-op for an unknown id.
pub(crate) fn despawn_item(
    commands: &mut Commands,
    materials: &mut Assets<StandardMaterial>,
    items: &mut DroppedItems,
    id: u64,
) {
    if let Some(entry) = items.0.remove(&id) {
        materials.remove(&entry.material);
        commands.entity(entry.entity).despawn();
    }
}

fn animate_items(time: Res<Time>, mut items: Query<(&DroppedItem, &mut Transform)>) {
    let t = time.elapsed_secs();
    for (item, mut transform) in &mut items {
        transform.translation.y = item.base_y + (t * BOB_SPEED + item.phase).sin() * BOB_AMPLITUDE;
        transform.rotate_y(SPIN_SPEED * time.delta_secs());
    }
}

/// Part of the `OnExit(AppState::InGame)` "despawn everything in-game"
/// contract (see `pause` module docs): despawns every dropped-item entity,
/// frees their materials, and clears [`DroppedItems`].
fn teardown_items(
    mut commands: Commands,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut items: ResMut<DroppedItems>,
) {
    for (_, entry) in items.0.drain() {
        materials.remove(&entry.material);
        commands.entity(entry.entity).despawn();
    }
}

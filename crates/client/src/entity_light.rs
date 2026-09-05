//! Voxel illumination for moving avatars and dropped items. Sampling one
//! cell per entity at 10 Hz keeps their lighting consistent with terrain
//! without adding point lights, rebuilding meshes, or allocating materials.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::common_conditions::on_timer;
use tsumiki_world::light::LightValue;

use crate::AppState;
use crate::daynight::lighting_for_time;
use crate::state::GameState;
use crate::view::ChunkStore;

/// Original item/avatar tint, retained while its material color changes.
#[derive(Component)]
pub(crate) struct EntityLightTint(pub Color);

/// Mirrors the voxel shader's per-channel combination of propagated RGB
/// and sunlight. Entity cuboids have one tint, so use an average face shade
/// instead of terrain's per-normal factor. Darkness has no ambient floor.
fn illuminated_color(tint: Color, light: LightValue, sunlight: Vec4) -> Color {
    let base = tint.to_linear();
    let rgb = Vec3::new(
        f32::from(light.rgb[0]),
        f32::from(light.rgb[1]),
        f32::from(light.rgb[2]),
    ) / 15.0;
    let sky = f32::from(light.sky) / 15.0;
    let daylight = sunlight.truncate() * sunlight.w * sky * sky;
    let irradiance = (rgb * rgb).max(daylight) * 0.84;
    Color::linear_rgba(
        base.red * irradiance.x,
        base.green * irradiance.y,
        base.blue * irradiance.z,
        base.alpha,
    )
}

fn light_at(store: &ChunkStore, position: Vec3) -> LightValue {
    let (chunk, local) = tsumiki_world::split_block_pos(position.floor().as_ivec3());
    store
        .light
        .get(&chunk)
        .map_or(LightValue::DARK, |light| light.get(local.as_uvec3()))
}

fn update_entity_lighting(
    store: Res<ChunkStore>,
    game: Res<GameState>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    entities: Query<(
        &Transform,
        &MeshMaterial3d<StandardMaterial>,
        &EntityLightTint,
    )>,
) {
    let sunlight = lighting_for_time(game.time_of_day).voxel_sunlight;
    for (transform, handle, tint) in &entities {
        let color = illuminated_color(tint.0, light_at(&store, transform.translation), sunlight);
        if materials
            .get(&handle.0)
            .is_some_and(|material| material.base_color != color)
            && let Some(mut material) = materials.get_mut(&handle.0)
        {
            material.base_color = color;
        }
    }
}

pub(crate) fn install(app: &mut App) {
    app.add_systems(
        Update,
        update_entity_lighting
            .run_if(in_state(AppState::InGame))
            .run_if(on_timer(Duration::from_millis(100))),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsumiki_world::light::LightChunk;

    #[test]
    fn torch_illuminates_items_at_night_without_lighting_an_unlit_cave() {
        let noon = lighting_for_time(0.25).voxel_sunlight;
        let night = lighting_for_time(0.75).voxel_sunlight;
        let tint = Color::srgb(0.8, 0.6, 0.4);
        let torch = LightValue::new([14, 11, 7], 0);
        let lit = illuminated_color(tint, torch, night).to_linear();
        assert!(lit.red > lit.green && lit.green > lit.blue);
        assert!(lit.blue > 0.0);
        assert_eq!(
            illuminated_color(tint, torch, noon),
            illuminated_color(tint, torch, night),
            "underground block light must be independent of day/night"
        );
        assert_eq!(
            illuminated_color(tint, LightValue::DARK, noon).to_linear(),
            Color::BLACK.to_linear()
        );
    }

    #[test]
    fn exposed_entities_follow_sunlight_and_preserve_their_original_tint() {
        let noon = lighting_for_time(0.25).voxel_sunlight;
        let midnight = lighting_for_time(0.75).voxel_sunlight;
        let tint = Color::linear_rgb(0.8, 0.4, 0.2);
        let day = illuminated_color(tint, LightValue::SKY, noon).to_linear();
        let night = illuminated_color(tint, LightValue::SKY, midnight).to_linear();
        assert!(day.red > night.red * 10.0);
        assert!(day.red > day.green && day.green > day.blue);
    }

    #[test]
    fn entity_samples_follow_negative_chunk_boundaries() {
        let mut store = ChunkStore::default();
        let warm = LightValue::new([12, 9, 5], 0);
        let mut west = LightChunk::filled(LightValue::DARK);
        west.set(UVec3::new(31, 4, 0), warm);
        store.light.insert(IVec3::NEG_X, west);
        store
            .light
            .insert(IVec3::ZERO, LightChunk::filled(LightValue::SKY));
        assert_eq!(light_at(&store, Vec3::new(-0.1, 4.2, 0.5)), warm);
        assert_eq!(light_at(&store, Vec3::new(0.1, 4.2, 0.5)), LightValue::SKY);
        assert_eq!(
            light_at(&store, Vec3::new(32.1, 4.2, 0.5)),
            LightValue::DARK
        );
    }
}

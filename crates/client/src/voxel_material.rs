//! Voxel lighting: propagated RGB stays independent of the daylight uniform.

use bevy::asset::embedded_asset;
use bevy::pbr::{ExtendedMaterial, MaterialExtension};
use bevy::prelude::*;
use bevy::render::render_resource::AsBindGroup;
use bevy::shader::ShaderRef;

pub(crate) type VoxelMaterial = ExtendedMaterial<StandardMaterial, VoxelLighting>;

#[derive(Asset, AsBindGroup, Reflect, Debug, Clone)]
pub(crate) struct VoxelLighting {
    /// Linear sky color and intensity; updated once for all terrain each frame.
    #[uniform(100)]
    pub sunlight: Vec4,
    /// Six 16-pixel face tiles per block, eight tiles per atlas row.
    #[texture(101)]
    pub atlas: Handle<Image>,
}

impl MaterialExtension for VoxelLighting {
    fn fragment_shader() -> ShaderRef {
        "embedded://tsumiki_client/voxel_material.wgsl".into()
    }
}

pub(crate) fn install(app: &mut App) {
    embedded_asset!(app, "voxel_material.wgsl");
    app.add_plugins(MaterialPlugin::<VoxelMaterial>::default());
    crate::entity_light::install(app);
}

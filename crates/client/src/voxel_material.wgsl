#import bevy_pbr::{
    forward_io::{VertexOutput, FragmentOutput},
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::main_pass_post_lighting_processing,
}

struct VoxelLighting {
    sunlight: vec4<f32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(100) var<uniform> lighting: VoxelLighting;
@group(#{MATERIAL_BIND_GROUP}) @binding(101) var block_atlas: texture_2d<f32>;

fn face_projection(position: vec3<f32>, normal: vec3<f32>) -> vec2<f32> {
    // Side textures are upright; the -Z machine front reads left to right
    // when viewed from outside. Top and bottom use horizontal planar axes.
    if normal.x < -0.5 { return vec2<f32>(position.z, 1.0 - position.y); }
    if normal.x > 0.5 { return vec2<f32>(1.0 - position.z, 1.0 - position.y); }
    if normal.y < -0.5 { return vec2<f32>(position.x, 1.0 - position.z); }
    if normal.y > 0.5 { return vec2<f32>(position.x, position.z); }
    if normal.z < -0.5 { return vec2<f32>(1.0 - position.x, 1.0 - position.y); }
    return vec2<f32>(position.x, 1.0 - position.y);
}

fn block_texture(position: vec3<f32>, normal: vec3<f32>, metadata: vec2<f32>) -> vec3<f32> {
    var uv: vec2<f32>;
    if metadata.y > 0.5 {
        // Torch shaft/head geometry fills its own tile instead of cropping
        // a narrow strip out of a block-sized texture.
        var origin = vec3<f32>(0.43, 0.0, 0.43);
        var size = vec3<f32>(0.14, 0.62, 0.14);
        if metadata.y > 1.5 {
            origin = vec3<f32>(0.40, 0.58, 0.40);
            size = vec3<f32>(0.20, 0.24, 0.20);
        }
        uv = clamp(face_projection((fract(position) - origin) / size, normal), vec2<f32>(0.0), vec2<f32>(1.0));
    } else {
        // World-space repetition stays continuous across greedy quads,
        // negative coordinates, and chunk borders.
        uv = fract(face_projection(position, normal));
    }
    let tile = u32(round(metadata.x));
    let origin = vec2<i32>(i32(tile % 8u), i32(tile / 8u)) * 16;
    let pixel = clamp(vec2<i32>(uv * 16.0), vec2<i32>(0), vec2<i32>(15));
    // Integer texel fetch is nearest-neighbor and cannot bleed adjacent
    // atlas tiles. The sRGB image is converted to linear by the GPU.
    return textureLoad(block_atlas, origin + pixel, 0).rgb;
}

@fragment
fn fragment(in: VertexOutput, @builtin(front_facing) is_front: bool) -> FragmentOutput {
    let pbr_input = pbr_input_from_standard_material(in, is_front);
    // A face has one exact light value. Packing RGB into UV.x avoids extra
    // vertex attributes and leaves the vertex colors available for block tint.
    let packed = u32(round(in.uv.x));
    let rgb = vec3<f32>(f32(packed & 15u), f32((packed >> 4u) & 15u), f32((packed >> 8u) & 15u)) / 15.0;
    let sky = in.uv.y * in.uv.y;
    let normal = normalize(in.world_normal);
    let shade = 0.72 + 0.20 * max(normal.y, 0.0) + 0.08 * abs(normal.z);
    let daylight = lighting.sunlight.rgb * lighting.sunlight.a * sky;
    let irradiance = max(rgb * rgb, daylight) * shade;
    var albedo = pbr_input.material.base_color.rgb;
    if in.uv_b.x >= 0.0 {
        albedo *= block_texture(in.world_position.xyz, normal, in.uv_b);
    }
    var out: FragmentOutput;
    out.color = vec4<f32>(albedo * irradiance, 1.0);
    out.color = main_pass_post_lighting_processing(pbr_input, out.color);
    return out;
}

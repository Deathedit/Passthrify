// Y + interleaved UV (NV12/NV16/NV24) -> RGB. Range and matrix come from the frame's ColorInfo
// (the 4K X sends full-range BT.601 NV12 but limited BT.709 YUY2). Drawn as a single
// full-viewport triangle; egui sets the viewport to the letterboxed preview rect.

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    var p = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    let xy = p[i];
    var o: VsOut;
    o.pos = vec4(xy, 0.0, 1.0);
    o.uv = vec2((xy.x + 1.0) * 0.5, 1.0 - (xy.y + 1.0) * 0.5);
    return o;
}

@group(0) @binding(0) var tex_y: texture_2d<f32>;
@group(0) @binding(1) var tex_uv: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct ColorParams {
    levels: vec4<f32>, // y_offset, y_scale, c_scale, unused
    coefs: vec4<f32>,  // r_v, g_u, g_v, b_u
};
@group(0) @binding(3) var<uniform> color: ColorParams;

fn yuv_to_rgb(uv: vec2<f32>) -> vec3<f32> {
    let y = (textureSample(tex_y, samp, uv).r - color.levels.x) * color.levels.y;
    let c = (textureSample(tex_uv, samp, uv).rg - vec2(128.0 / 255.0)) * color.levels.z;
    let k = color.coefs;
    let rgb = vec3(
        y + k.x * c.y,
        y - k.y * c.x - k.z * c.y,
        y + k.w * c.x,
    );
    return clamp(rgb, vec3(0.0), vec3(1.0));
}

// Non-sRGB swapchain: write gamma-encoded values as-is.
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return vec4(yuv_to_rgb(in.uv), 1.0);
}

// sRGB swapchain: the hardware re-encodes, so hand it linear values.
@fragment
fn fs_main_srgb(in: VsOut) -> @location(0) vec4<f32> {
    return vec4(pow(yuv_to_rgb(in.uv), vec3(2.2)), 1.0);
}

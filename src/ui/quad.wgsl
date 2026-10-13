struct Screen {
    size: vec2<f32>,
    pad: vec2<f32>,
}

@group(0) @binding(0) var<uniform> screen: Screen;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct VsIn {
    @builtin(vertex_index) vi: u32,
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) border_color: vec4<f32>,
    @location(4) radius_border: vec2<f32>,
}

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) border_color: vec4<f32>,
    @location(4) radius_border: vec2<f32>,
}

@vertex
fn vs_main(in: VsIn) -> VsOut {
    let corner = vec2<f32>(f32(in.vi & 1u), f32(in.vi >> 1u));
    let px = in.pos + corner * in.size;
    var out: VsOut;
    out.clip = vec4<f32>(px.x / screen.size.x * 2.0 - 1.0, 1.0 - px.y / screen.size.y * 2.0, 0.0, 1.0);
    out.local = corner * in.size;
    out.size = in.size;
    out.color = in.color;
    out.border_color = in.border_color;
    out.radius_border = in.radius_border;
    return out;
}

// Signed distance to a rounded box; negative inside.
fn sdf(local: vec2<f32>, size: vec2<f32>, r: f32) -> f32 {
    let half = size * 0.5;
    let q = abs(local - half) - half + r;
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_rect(in: VsOut) -> @location(0) vec4<f32> {
    let d = sdf(in.local, in.size, in.radius_border.x);
    let cover = clamp(0.5 - d, 0.0, 1.0);
    let inner = clamp(0.5 - (d + in.radius_border.y), 0.0, 1.0);
    let col = mix(in.border_color, in.color, inner);
    return vec4<f32>(col.rgb, col.a * cover);
}

@fragment
fn fs_image(in: VsOut) -> @location(0) vec4<f32> {
    let d = sdf(in.local, in.size, in.radius_border.x);
    let cover = clamp(0.5 - d, 0.0, 1.0);
    let c = textureSample(tex, samp, in.local / in.size);
    return vec4<f32>(c.rgb, c.a * cover);
}

@group(0) @binding(0) var source: texture_2d<f32>;

@vertex
fn fullscreen(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    return vec4<f32>(positions[index], 0.0, 1.0);
}
fn encode_srgb(linear: vec3<f32>) -> vec3<f32> {
    return select(1.055 * pow(linear, vec3<f32>(1.0 / 2.4)) - 0.055,
                  linear * 12.92, linear <= vec3<f32>(0.0031308));
}
fn straight_srgb(position: vec4<f32>) -> vec4<f32> {
    // SRGB textureLoad automatically decodes RGB to premultiplied linear.
    let pixel = textureLoad(source, vec2<i32>(position.xy), 0);
    if pixel.a == 0.0 { return vec4<f32>(0.0); }
    let straight = clamp(pixel.rgb / pixel.a, vec3<f32>(0.0), vec3<f32>(1.0));
    return vec4<f32>(encode_srgb(straight), pixel.a);
}
@fragment
fn premultiplied(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let pixel = straight_srgb(position);
    // Surface is plain UNORM: no second hardware SRGB encode.
    return vec4<f32>(pixel.rgb * pixel.a, pixel.a);
}
@fragment
fn postmultiplied(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    return straight_srgb(position);
}

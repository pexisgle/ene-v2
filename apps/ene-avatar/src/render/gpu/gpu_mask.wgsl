// One invocation per logical pixel. Alpha is unchanged by SRGB texture decoding.
struct Parameters {
    physical_width: u32,
    physical_height: u32,
    scale: u32,
    capacity: u32,
};
struct Runs {
    count: atomic<u32>,
    overflow: atomic<u32>,
    padding: vec2<u32>,
    values: array<vec4<u32>>,
};
@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var<uniform> parameters: Parameters;
@group(0) @binding(2) var<storage, read_write> runs: Runs;

fn covered(x: u32, y: u32) -> bool {
    let left = x * parameters.scale;
    let top = y * parameters.scale;
    let right = left + min(parameters.scale, parameters.physical_width - left);
    let bottom = top + min(parameters.scale, parameters.physical_height - top);
    for (var py = top; py < bottom; py += 1u) {
        for (var px = left; px < right; px += 1u) {
            // Comparing the quantized UNORM alpha to 8/255 keeps byte 8 out
            // and byte 9 in; RGB is never involved in coverage.
            if textureLoad(source, vec2<i32>(i32(px), i32(py)), 0).a > (8.0 / 255.0) {
                return true;
            }
        }
    }
    return false;
}
fn emit_run(x: u32, y: u32, width: u32) {
    let index = atomicAdd(&runs.count, 1u);
    if index < parameters.capacity {
        runs.values[index] = vec4<u32>(x, y, width, 1u);
    } else {
        atomicStore(&runs.overflow, 1u);
    }
}
// Test neighbours in parallel. Only the first covered pixel of an interval
// walks to its end, so empty background is not scanned serially along each row.
@compute @workgroup_size(256)
fn mask(@builtin(global_invocation_id) invocation: vec3<u32>) {
    let x = invocation.x;
    let y = invocation.y;
    let logical_width = (parameters.physical_width - 1u) / parameters.scale + 1u;
    let logical_height = (parameters.physical_height - 1u) / parameters.scale + 1u;
    if x >= logical_width || y >= logical_height { return; }
    if !covered(x, y) { return; }
    if x > 0u {
        if covered(x - 1u, y) { return; }
    }
    var end = x + 1u;
    while end < logical_width {
        if !covered(end, y) { break; }
        end += 1u;
    }
    emit_run(x, y, end - x);
}

// Model viewport shaders. Downlevel-safe: uniform buffers only, no storage
// buffers, no compute. Output is gamma-encoded into an Rgba8Unorm target,
// matching egui's convention for user textures.

struct Frame {
    view_proj: mat4x4<f32>,
    camera_pos: vec4<f32>,
    // xyz: direction towards the key light (world space).
    light_dir: vec4<f32>,
    wire_color: vec4<f32>,
    // x: shading mode; y, z, w unused. The base colour texture is always
    // bound (a light grey 1x1 texture when the material has none).
    params: vec4<u32>,
};

@group(0) @binding(0) var<uniform> frame: Frame;
@group(1) @binding(0) var base_tex: texture_2d<f32>;
@group(1) @binding(1) var base_sampler: sampler;

const MODE_LIT: u32 = 0u;
const MODE_NORMALS: u32 = 1u;
const MODE_UV_CHECKER: u32 = 2u;
const MODE_VERTEX_COLORS: u32 = 3u;

fn gamma_from_linear(c: vec3<f32>) -> vec3<f32> {
    let cl = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
    let lo = cl * 12.92;
    let hi = 1.055 * pow(cl, vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(hi, lo, cl <= vec3<f32>(0.0031308));
}

struct MeshIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) color: vec4<f32>,
};

struct MeshOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) world: vec3<f32>,
};

@vertex
fn vs_mesh(in: MeshIn) -> MeshOut {
    var out: MeshOut;
    out.clip = frame.view_proj * vec4<f32>(in.position, 1.0);
    out.normal = in.normal;
    out.uv = in.uv;
    out.color = in.color;
    out.world = in.position;
    return out;
}

@fragment
fn fs_mesh(in: MeshOut, @builtin(front_facing) front: bool) -> @location(0) vec4<f32> {
    // Sample unconditionally (uniform control flow).
    let tex = textureSample(base_tex, base_sampler, in.uv);
    let mode = frame.params.x;

    let len = length(in.normal);
    var n = vec3<f32>(0.0, 0.0, 1.0);
    if (len > 1e-6) {
        n = in.normal / len;
    }
    // Winding conventions of decoded data are not trusted: light both sides.
    if (!front) {
        n = -n;
    }

    if (mode == MODE_NORMALS) {
        return vec4<f32>(n * 0.5 + vec3<f32>(0.5), 1.0);
    }
    if (mode == MODE_VERTEX_COLORS) {
        // Vertex colours are shown as stored (no colour space conversion).
        return vec4<f32>(in.color.rgb, 1.0);
    }

    var albedo = tex.rgb;
    if (mode == MODE_UV_CHECKER) {
        let cells = floor(in.uv * 8.0);
        let parity = abs(cells.x + cells.y) - 2.0 * floor(abs(cells.x + cells.y) * 0.5);
        let base = mix(vec3<f32>(0.08), vec3<f32>(0.75), parity);
        // Tint by UV so orientation and wrapping are visible.
        albedo = base * vec3<f32>(0.6 + 0.4 * fract(in.uv.x), 0.6, 0.6 + 0.4 * fract(in.uv.y));
    }

    // Directional key light + hemispheric ambient (sky above, ground below).
    let l = normalize(frame.light_dir.xyz);
    let diffuse = max(dot(n, l), 0.0);
    let sky = vec3<f32>(0.42, 0.45, 0.52);
    let ground = vec3<f32>(0.16, 0.14, 0.12);
    let ambient = mix(ground, sky, n.z * 0.5 + 0.5);
    let v = normalize(frame.camera_pos.xyz - in.world);
    let h = normalize(l + v);
    let spec = pow(max(dot(n, h), 0.0), 48.0) * 0.15;
    let lit = albedo * (ambient + vec3<f32>(0.85) * diffuse) + vec3<f32>(spec);
    return vec4<f32>(gamma_from_linear(lit), 1.0);
}

// Native line polygon mode wireframe (POLYGON_MODE_LINE).
@fragment
fn fs_wire_native(in: MeshOut) -> @location(0) vec4<f32> {
    return frame.wire_color;
}

// Barycentric wireframe fallback for devices without POLYGON_MODE_LINE.
struct WireIn {
    @location(0) position: vec3<f32>,
    @location(1) bary: vec4<f32>,
};

struct WireOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) bary: vec3<f32>,
};

@vertex
fn vs_wire(in: WireIn) -> WireOut {
    var out: WireOut;
    out.clip = frame.view_proj * vec4<f32>(in.position, 1.0);
    out.bary = in.bary.xyz;
    return out;
}

@fragment
fn fs_wire(in: WireOut) -> @location(0) vec4<f32> {
    let w = fwidth(in.bary);
    let edge = smoothstep(vec3<f32>(0.0), w * 1.5, in.bary);
    let a = 1.0 - min(min(edge.x, edge.y), edge.z);
    if (a < 0.02) {
        discard;
    }
    return vec4<f32>(frame.wire_color.rgb, frame.wire_color.a * a);
}

// Grid, axes and skeleton lines.
struct LineIn {
    @location(0) position: vec3<f32>,
    @location(1) color: vec4<f32>,
};

struct LineOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_line(in: LineIn) -> LineOut {
    var out: LineOut;
    out.clip = frame.view_proj * vec4<f32>(in.position, 1.0);
    out.color = in.color;
    return out;
}

@fragment
fn fs_line(in: LineOut) -> @location(0) vec4<f32> {
    return in.color;
}

//! The cubemap-to-equirectangular reprojection, run on the GPU.
//!
//! On the CPU, `v360` is three quarters of a conversion's time: measured on a
//! Max 2 clip, ten seconds of video took 15 s end to end, of which 11 s was
//! `v360` alone and under 3 s the decode, crops and seam blends together. The
//! transform is a fixed per-pixel lookup, which is what a GPU is for.
//!
//! ffmpeg's own `v360_vulkan` would be the obvious tool, but it takes no
//! cubemap input at all. So the transform is written out here as an mpv-style
//! user shader and run by the `libplacebo` filter, which uploads, runs and
//! downloads on the Vulkan device it picks (the discrete GPU, given the choice).
//!
//! The shader is a port of `v360`'s `equirect_to_xyz`, quaternion rotation,
//! `xyz_to_cube` and `xyz_to_eac`, in that order, with the same face order,
//! face rotations and two-pixel pad. It samples the whole atlas clamped at its
//! edges, as `v360` does, rather than clamping per face. On a Max 2 clip it
//! agrees with CPU `v360` at 65 dB PSNR for lanczos and 66 dB for bilinear,
//! which is 10-bit rounding: the geometry is the same, not merely close.

use std::fmt::Write as _;

use super::Rotation;

/// How a reprojected pixel is interpolated from the cubemap.
///
/// Only the kernels ported from `v360` are offered. Anything else falls back
/// to the CPU, rather than being quietly swapped for a different filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interp {
    Nearest,
    Linear,
    Lanczos,
}

impl Interp {
    /// Parse one of `v360`'s names for an interpolation, if it is ported.
    pub fn parse(name: &str) -> Option<Interp> {
        match name {
            "near" | "nearest" => Some(Interp::Nearest),
            "line" | "linear" => Some(Interp::Linear),
            "lanc" | "lanczos" => Some(Interp::Lanczos),
            _ => None,
        }
    }
}

/// `v360`'s rotation quaternion for `rotation` applied in `rorder`.
///
/// Built exactly as `calculate_rotation` builds it, in single precision, from
/// the identity: one half-angle quaternion per axis, multiplied on in the
/// order `rorder` names. A malformed `rorder` falls back to `ypr`, which is
/// what `v360` does with one (after a warning this side never sees).
pub fn quaternion(rotation: Rotation, rorder: &str) -> [f32; 4] {
    let half = |deg: f64| (deg as f32).to_radians() * 0.5;
    let (y, p, r) = (half(rotation.yaw), half(rotation.pitch), half(rotation.roll));
    let axes = [
        [y.cos(), 0.0, y.sin(), 0.0],
        [p.cos(), p.sin(), 0.0, 0.0],
        [r.cos(), 0.0, 0.0, r.sin()],
    ];
    let order: Option<Vec<usize>> = rorder
        .chars()
        .take(3)
        .map(|c| match c.to_ascii_lowercase() {
            'y' => Some(0),
            'p' => Some(1),
            'r' => Some(2),
            _ => None,
        })
        .collect();
    let order = match order {
        Some(o) if o.len() == 3 => o,
        _ => vec![0, 1, 2],
    };
    let mut q = [1.0f32, 0.0, 0.0, 0.0];
    for &axis in &order {
        q = multiply(q, axes[axis]);
    }
    q
}

/// `v360`'s `multiply_quaternion`, term for term.
fn multiply(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [
        a[0] * b[0] - a[1] * b[1] - a[2] * b[2] - a[3] * b[3],
        a[1] * b[0] + a[0] * b[1] + a[2] * b[3] - a[3] * b[2],
        a[2] * b[0] + a[0] * b[2] + a[3] * b[1] - a[1] * b[3],
        a[3] * b[0] + a[0] * b[3] + a[1] * b[2] - a[2] * b[1],
    ]
}

/// The user shader that reprojects an equi-angular cubemap atlas of
/// `atlas_w` x `atlas_h` (3 x 2 faces) to `out_w` x `out_h` equirectangular.
pub fn shader(
    out_w: u32,
    out_h: u32,
    atlas_w: u32,
    atlas_h: u32,
    rotation: Rotation,
    rorder: &str,
    interp: Interp,
) -> String {
    let q = quaternion(rotation, rorder);
    let sample = match interp {
        Interp::Nearest => "nearest(p)",
        Interp::Linear => "HOOKED_tex(p)",
        Interp::Lanczos => "lanczos(p)",
    };
    let mut s = String::new();
    // The header declares the hook's output size, which is what lets one pass
    // both reproject and resize: libplacebo then has nothing left to scale.
    let _ = write!(
        s,
        "//!HOOK MAIN\n\
         //!BIND HOOKED\n\
         //!WIDTH {out_w}\n\
         //!HEIGHT {out_h}\n\
         //!DESC gmax: equi-angular cubemap to equirectangular\n\
         \n\
         const vec4 Q = vec4({:.9}, {:.9}, {:.9}, {:.9});\n\
         const vec2 ATLAS = vec2({atlas_w}.0, {atlas_h}.0);\n",
        q[0], q[1], q[2], q[3]
    );
    s.push_str(SHADER_BODY);
    let _ = write!(s, "\nvec4 hook() {{\n    vec2 p = eac_coord();\n    return {sample};\n}}\n");
    s
}

/// Everything in the shader that does not depend on the clip.
const SHADER_BODY: &str = r#"
const float PI = 3.14159265358979;
// v360 pads each face by two pixels of the atlas.
const vec2 PAD = 2.0 / ATLAS;

vec4 qmul(vec4 a, vec4 b) {
    return vec4(a.x * b.x - a.y * b.y - a.z * b.z - a.w * b.w,
                a.y * b.x + a.x * b.y + a.z * b.w - a.w * b.z,
                a.z * b.x + a.x * b.z + a.w * b.y - a.y * b.w,
                a.w * b.x + a.x * b.w + a.y * b.z - a.z * b.y);
}

// Where this output pixel's direction lands in the atlas, normalised to 0..1
// with texel centres at (k + 0.5) / size -- v360's coordinate before it
// subtracts half a pixel and floors.
vec2 eac_coord() {
    // equirect_to_xyz. HOOKED_pos is the output pixel's centre, so
    // 2 * pos - 1 is v360's rescale(i, width).
    float phi = (2.0 * HOOKED_pos.x - 1.0) * PI;
    float theta = (2.0 * HOOKED_pos.y - 1.0) * PI * 0.5;
    vec3 v = normalize(vec3(cos(theta) * sin(phi), sin(theta), cos(theta) * cos(phi)));
    // rotate: q v q*.
    vec4 qc = vec4(Q.x, -Q.yzw);
    v = normalize(qmul(qmul(Q, vec4(0.0, v)), qc).yzw);

    // xyz_to_cube. Directions: 0 right, 1 left, 2 up, 3 down, 4 front, 5 back.
    float ph = atan(v.x, v.z);
    float th = asin(v.y);
    float pn;
    int dir;
    if (ph >= -PI / 4.0 && ph < PI / 4.0) {
        dir = 4; pn = ph;
    } else if (ph >= -(PI / 2.0 + PI / 4.0) && ph < -PI / 4.0) {
        dir = 1; pn = ph + PI / 2.0;
    } else if (ph >= PI / 4.0 && ph < PI / 2.0 + PI / 4.0) {
        dir = 0; pn = ph - PI / 2.0;
    } else {
        dir = 5; pn = ph + (ph > 0.0 ? -PI : PI);
    }
    float limit = atan(cos(pn));
    if (th > limit) {
        dir = 3;
    } else if (th < -limit) {
        dir = 2;
    }

    // Per-direction face coordinates, then prepare_eac_in's face order:
    // top row left, front, right; bottom row down, back, up.
    float uf, vf;
    int face;
    if (dir == 0)      { uf = -v.z / v.x; vf =  v.y / v.x; face = 2; }
    else if (dir == 1) { uf = -v.z / v.x; vf = -v.y / v.x; face = 0; }
    else if (dir == 2) { uf = -v.x / v.y; vf = -v.z / v.y; face = 5; }
    else if (dir == 3) { uf =  v.x / v.y; vf = -v.z / v.y; face = 3; }
    else if (dir == 4) { uf =  v.x / v.z; vf =  v.y / v.z; face = 1; }
    else               { uf =  v.x / v.z; vf = -v.y / v.z; face = 4; }

    // rotate_cube_face: the back face is turned 90 degrees, down and up 270.
    float t;
    if (face == 4) {
        t = uf; uf = -vf; vf = t;
    } else if (face == 3 || face == 5) {
        t = -uf; uf = vf; vf = t;
    }

    // xyz_to_eac: equi-angular, then placed in its cell of the 3x2 atlas.
    uf = 2.0 / PI * atan(uf) + 0.5;
    vf = 2.0 / PI * atan(vf) + 0.5;
    uf = (uf + float(face % 3)) * (1.0 - 2.0 * PAD.x) / 3.0 + PAD.x;
    vf = vf * (0.5 - 2.0 * PAD.y) + PAD.y + 0.5 * float(face / 3);
    return vec2(uf, vf);
}

vec4 texel(ivec2 q) {
    return texelFetch(HOOKED_raw, clamp(q, ivec2(0), ivec2(ATLAS) - 1), 0) * HOOKED_mul;
}

// nearest_kernel: the tap lrintf picks.
vec4 nearest(vec2 p) {
    return texel(ivec2(floor(p * ATLAS)));
}

// calculate_lanczos_coeffs: a = 2 over a 4-tap window, normalised.
vec4 lanczos_weights(float t) {
    vec4 x = PI * vec4(t + 1.0, t, t - 1.0, t - 2.0);
    vec4 c = sin(x) * sin(x * 0.5) / (x * x * 0.5);
    c = mix(c, vec4(1.0), equal(x, vec4(0.0)));
    return c / dot(c, vec4(1.0));
}

// lanczos_kernel over v360's 4x4 window, each tap clamped to the atlas.
vec4 lanczos(vec2 p) {
    vec2 f = p * ATLAS - 0.5;
    vec2 i0 = floor(f);
    vec4 wx = lanczos_weights(f.x - i0.x);
    vec4 wy = lanczos_weights(f.y - i0.y);
    vec4 acc = vec4(0.0);
    for (int i = 0; i < 4; i++) {
        vec4 row = vec4(0.0);
        for (int j = 0; j < 4; j++) {
            row += wx[j] * texel(ivec2(i0) + ivec2(j - 1, i - 1));
        }
        acc += wy[i] * row;
    }
    return acc;
}
"#;

/// Characters that would need escaping, at two levels, to survive inside a
/// filter graph option value. A shader path containing one is refused rather
/// than escaped.
pub fn path_is_graph_safe(path: &str) -> bool {
    !path
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '\\' | '\'' | ':' | ',' | ';' | '[' | ']' | '='))
}

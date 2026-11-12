// The compositing steps of `frame.rs`, `fx.rs` and `transitions.rs`, pixel
// for pixel. Values are worked in whole 0..255 steps with the same roundings
// as on the processor, so both give the same picture; only blending onto the
// target is left to the card, which rounds where the processor truncates.

struct Params {
    // Destination box in pixels: x, y, width, height. Cube: output size in z, w.
    rect: vec4<f32>,
    // Blit: first source column and row, then source pixels per output pixel.
    map: vec4<f32>,
    // Fill colour 0..255, and opacity 0..255 in w.
    colour: vec4<f32>,
    // Corner radius, mix weight 0..256, exact copy flag, dim factor 0..256.
    misc: vec4<f32>,
    // Turn: sin, cos, zoom, mirror. Cube: sin, cos, zoom.
    turn: vec4<f32>,
    // Turn: centre x, y, pan x, pan y.
    centre: vec4<f32>,
    // Turn: left, right, top, bottom edges kept from the source.
    bounds: vec4<f32>,
    // Copy: where each pixel reads from, relative to itself.
    shift: vec4<i32>,
    // Colour matrix rows; m0.w set when there is one.
    m0: vec4<f32>,
    m1: vec4<f32>,
    m2: vec4<f32>,
    // Key: low edge, softness span, spill amount; colour alpha in w.
    key: vec4<f32>,
    // Chroma key: key chroma. Colour key: key colour.
    key_colour: vec4<f32>,
    // Process: colour on, key mode, dominant channel, fill present.
    // Lut: strength 0..256.
    flags: vec4<u32>,
}

@group(0) @binding(0) var<uniform> P: Params;
@group(1) @binding(0) var a: texture_2d<f32>;
@group(1) @binding(1) var b: texture_2d<f32>;
@group(1) @binding(2) var c: texture_2d<f32>;
@group(1) @binding(3) var<storage, read> lut: array<u32>;

// One triangle over the whole target; the scissor picks the pixels.
@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let x = f32((i << 1u) & 2u) * 2.0 - 1.0;
    let y = f32(i & 2u) * 2.0 - 1.0;
    return vec4<f32>(x, y, 0.0, 1.0);
}

fn texel(t: texture_2d<f32>, x: i32, y: i32) -> vec4<u32> {
    return vec4<u32>(round(textureLoad(t, vec2<i32>(x, y), 0) * 255.0));
}

fn put(v: vec4<u32>) -> vec4<f32> {
    return vec4<f32>(min(v, vec4<u32>(255u))) / 255.0;
}

fn outside_round(x: f32, y: f32, w: f32, h: f32, r: f32) -> bool {
    let dx = max(r - x, x - (w - r));
    let dy = max(r - y, y - (h - r));
    return dx > 0.0 && dy > 0.0 && dx * dx + dy * dy > r * r;
}

fn scaled(p: vec4<u32>, k: f32) -> vec4<u32> {
    return vec4<u32>(vec4<f32>(p) * k);
}

// Part of `a` scaled into a box, bilinear in 1/256 steps.
@fragment
fn fs_blit(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let size = textureDimensions(a);
    let sw = f32(size.x);
    let sh = f32(size.y);
    let sx = clamp(P.map.x + (pos.x - P.rect.x) * P.map.z - 0.5, 0.0, sw - 1.0);
    let sy = clamp(P.map.y + (pos.y - P.rect.y) * P.map.w - 0.5, 0.0, sh - 1.0);
    let ix = u32(sx);
    let iy = u32(sy);
    let nx = min(ix + 1u, size.x - 1u);
    let ny = min(iy + 1u, size.y - 1u);
    let wx = u32((sx - f32(ix)) * 256.0);
    let wy = u32((sy - f32(iy)) * 256.0);
    let p00 = texel(a, i32(ix), i32(iy));
    let p10 = texel(a, i32(nx), i32(iy));
    let p01 = texel(a, i32(ix), i32(ny));
    let p11 = texel(a, i32(nx), i32(ny));
    if ((p00.a | p10.a | p01.a | p11.a) == 0u) {
        discard;
    }
    if (P.misc.z > 0.5 && p00.a == 0u) {
        discard;
    }
    if (P.misc.x > 0.0 && outside_round(pos.x - P.rect.x, pos.y - P.rect.y, P.rect.z, P.rect.w, P.misc.x)) {
        discard;
    }
    let up = p00 * (256u - wx) + p10 * wx;
    let down = p01 * (256u - wx) + p11 * wx;
    var v = (up * (256u - wy) + down * wy) >> vec4<u32>(16u);
    let op = u32(P.colour.w);
    if (op != 255u) {
        v = v * op / 255u;
    }
    return put(v);
}

@fragment
fn fs_fill(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    if (P.misc.x > 0.0 && outside_round(pos.x - P.rect.x, pos.y - P.rect.y, P.rect.z, P.rect.w, P.misc.x)) {
        discard;
    }
    let op = P.colour.w / 255.0;
    return vec4<f32>(P.colour.rgb / 255.0 * op, op);
}

@fragment
fn fs_mix(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let x = i32(pos.x);
    let y = i32(pos.y);
    let tb = u32(P.misc.y);
    return put((texel(a, x, y) * (256u - tb) + texel(b, x, y) * tb) >> vec4<u32>(8u));
}

@fragment
fn fs_shift(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<i32>(pos.xy) + P.shift.xy;
    return textureLoad(a, at, 0);
}

fn sample(x: f32, y: f32) -> vec4<f32> {
    let size = vec2<i32>(textureDimensions(a));
    let x0 = floor(x);
    let y0 = floor(y);
    let fx = x - x0;
    let fy = y - y0;
    let ix = i32(x0);
    let iy = i32(y0);
    let l = clamp(ix, 0, size.x - 1);
    let r = clamp(ix + 1, 0, size.x - 1);
    let t = clamp(iy, 0, size.y - 1);
    let d = clamp(iy + 1, 0, size.y - 1);
    let p00 = vec4<f32>(texel(a, l, t));
    let p10 = vec4<f32>(texel(a, r, t));
    let p01 = vec4<f32>(texel(a, l, d));
    let p11 = vec4<f32>(texel(a, r, d));
    let top = p00 * (1.0 - fx) + p10 * fx;
    let bot = p01 * (1.0 - fx) + p11 * fx;
    return top * (1.0 - fy) + bot * fy;
}

// Each output pixel looks back into `a` through the turn.
@fragment
fn fs_turn(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    var dx = pos.x - P.centre.x - P.centre.z;
    let dy = pos.y - P.centre.y + P.centre.w;
    if (P.turn.w > 0.5) {
        dx = -dx;
    }
    let sx = P.centre.x + (dx * P.turn.y + dy * P.turn.x) / P.turn.z;
    let sy = P.centre.y + (-dx * P.turn.x + dy * P.turn.y) / P.turn.z;
    if (sx < P.bounds.x || sx >= P.bounds.y || sy < P.bounds.z || sy >= P.bounds.w) {
        discard;
    }
    var v = vec4<u32>(sample(sx - 0.5, sy - 0.5));
    if (v.a == 0u) {
        discard;
    }
    let op = u32(P.colour.w);
    if (op != 255u) {
        v = v * op / 255u;
    }
    return put(v);
}

@fragment
fn fs_cube(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let black = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    let s = P.turn.x;
    let co = P.turn.y;
    let zoom = P.turn.z;
    let f = 1.5;
    let cc = (pos.x / P.rect.z - 0.5) / zoom;
    if (abs(cc) > 0.5) {
        return black;
    }
    var found = false;
    var incoming = false;
    var u = 0.0;
    var scale = 1.0;
    let den_a = f * co + cc * s;
    if (abs(den_a) > 1e-5) {
        let x = (cc * (f + 0.5 - 0.5 * co) + 0.5 * f * s) / den_a;
        if (x >= -0.5 && x <= 0.5) {
            found = true;
            u = x + 0.5;
            scale = f / (f + 0.5 - x * s - 0.5 * co);
        }
    }
    if (!found) {
        let den_b = f * s - cc * co;
        if (abs(den_b) > 1e-5) {
            let v = (cc * (f + 0.5 - 0.5 * s) - 0.5 * f * co) / den_b;
            if (v >= -0.5 && v <= 0.5) {
                found = true;
                incoming = true;
                u = v + 0.5;
                scale = f / (f + 0.5 - 0.5 * s + v * co);
            }
        }
    }
    if (!found) {
        return black;
    }
    let v = pos.y / P.rect.w - 0.5;
    let sv = v / (scale * zoom) + 0.5;
    if (!(sv >= 0.0 && sv < 1.0)) {
        return black;
    }
    if (incoming) {
        let size = textureDimensions(b);
        let sx = min(u32(u * f32(size.x)), size.x - 1u);
        let sy = min(u32(sv * f32(size.y)), size.y - 1u);
        return textureLoad(b, vec2<i32>(i32(sx), i32(sy)), 0);
    }
    let size = textureDimensions(a);
    let sx = min(u32(u * f32(size.x)), size.x - 1u);
    let sy = min(u32(sv * f32(size.y)), size.y - 1u);
    return textureLoad(a, vec2<i32>(i32(sx), i32(sy)), 0);
}

// Fade to black, then opaque, as the output leaves.
@fragment
fn fs_finish(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let v = texel(a, i32(pos.x), i32(pos.y));
    let k = u32(P.misc.w);
    return put(vec4<u32>((v.rgb * k) >> vec3<u32>(8u), 255u));
}

fn luma(p: vec4<u32>) -> f32 {
    return 0.299 * f32(p.r) + 0.587 * f32(p.g) + 0.114 * f32(p.b);
}

// Colour correction then key, as `fx::process` does after the position.
@fragment
fn fs_process(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let x = i32(pos.x);
    let y = i32(pos.y);
    var p = texel(a, x, y);
    if (P.flags.x == 1u && p.a != 0u) {
        let r = texel(c, i32(p.r), 0).r;
        let g = texel(c, i32(p.g), 0).g;
        let bl = texel(c, i32(p.b), 0).b;
        if (P.m0.w > 0.5) {
            let v = vec3<f32>(f32(r), f32(g), f32(bl));
            p.r = u32(clamp(P.m0.x * v.x + P.m0.y * v.y + P.m0.z * v.z, 0.0, 255.0));
            p.g = u32(clamp(P.m1.x * v.x + P.m1.y * v.y + P.m1.z * v.z, 0.0, 255.0));
            p.b = u32(clamp(P.m2.x * v.x + P.m2.y * v.y + P.m2.z * v.z, 0.0, 255.0));
        } else {
            p.r = r;
            p.g = g;
            p.b = bl;
        }
        p = vec4<u32>(min(p.rgb, vec3<u32>(p.a)), p.a);
        if (P.key.w < 1.0) {
            p = scaled(p, P.key.w);
        }
    }
    let lo = P.key.x;
    let span = P.key.y;
    switch P.flags.y {
        // Colour: distance to the key colour.
        case 1u: {
            let d0 = f32(p.r) - P.key_colour.x;
            let d1 = f32(p.g) - P.key_colour.y;
            let d2 = f32(p.b) - P.key_colour.z;
            let d = sqrt(0.0 + d0 * d0 + d1 * d1 + d2 * d2);
            let k = clamp((d - lo) / span, 0.0, 1.0);
            if (k < 1.0) {
                p = scaled(p, k);
            }
        }
        // Chroma: distance in chroma, then the spill pulled down.
        case 2u: {
            let r = f32(p.r);
            let g = f32(p.g);
            let bl = f32(p.b);
            let cb = -0.169 * r - 0.331 * g + 0.5 * bl;
            let cr = 0.5 * r - 0.419 * g - 0.081 * bl;
            let db = cb - P.key_colour.x;
            let dr = cr - P.key_colour.y;
            let d = sqrt(db * db + dr * dr);
            let k = clamp((d - lo) / span, 0.0, 1.0);
            if (k < 1.0) {
                p = scaled(p, k);
            }
            let dom = P.flags.z;
            let limit = f32(max(p[(dom + 1u) % 3u], p[(dom + 2u) % 3u]));
            let v = f32(p[dom]);
            if (v > limit) {
                p[dom] = u32(v - (v - limit) * P.key.z);
            }
        }
        case 3u: {
            let k = clamp((luma(p) - lo) / span, 0.0, 1.0);
            if (k < 1.0) {
                p = scaled(p, k);
            }
        }
        case 4u: {
            if (P.flags.w == 1u) {
                p = scaled(p, luma(texel(b, x, y)) / 255.0);
            }
        }
        default: {}
    }
    return put(p);
}

const ORDER = array<vec3<u32>, 8>(
    vec3<u32>(2u, 1u, 0u),
    vec3<u32>(2u, 0u, 1u),
    vec3<u32>(1u, 2u, 0u),
    vec3<u32>(0u, 1u, 2u),
    vec3<u32>(0u, 1u, 2u),
    vec3<u32>(0u, 2u, 1u),
    vec3<u32>(1u, 0u, 2u),
    vec3<u32>(0u, 1u, 2u),
);

fn node(i: u32) -> vec3<u32> {
    let n = lut[771u + i];
    return vec3<u32>(n & 0xffu, (n >> 8u) & 0xffu, (n >> 16u) & 0xffu);
}

// Tetrahedral lookup in the table laid out by `Lut::packed`.
@fragment
fn fs_lut(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    var p = texel(a, i32(pos.x), i32(pos.y));
    if (p.a == 0u) {
        return put(p);
    }
    let cr = lut[p.r];
    let cg = lut[256u + p.g];
    let cb = lut[512u + p.b];
    let steps = vec3<u32>(lut[768u], lut[769u], lut[770u]);
    let f = vec3<u32>(cr >> 20u, cg >> 20u, cb >> 20u);
    let way = u32(f.x > f.y) | (u32(f.y > f.z) << 1u) | (u32(f.x > f.z) << 2u);
    var order = ORDER;
    let o = order[way];
    let fa = f[o.x];
    let fb = f[o.y];
    let fc = f[o.z];
    let mask = 0xfffffu;
    let base = (cr & mask) + (cg & mask) + (cb & mask);
    let near = base + steps[o.x];
    let next = near + steps[o.y];
    let far = next + steps[o.z];
    let col = (node(base) * (256u - fa) + node(near) * (fa - fb) + node(next) * (fb - fc) + node(far) * fc) >> vec3<u32>(8u);
    let mix = P.flags.x;
    if (mix == 256u && p.a == 255u) {
        return put(vec4<u32>(col, 255u));
    }
    let v = (p.rgb * (256u - mix) + col * mix) >> vec3<u32>(8u);
    return put(vec4<u32>(min(v, vec3<u32>(p.a)), p.a));
}

@fragment
fn fs_mask(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let x = i32(pos.x);
    let y = i32(pos.y);
    let m = texel(c, x, y).r;
    return put(scaled(texel(a, x, y), f32(m) / 255.0));
}

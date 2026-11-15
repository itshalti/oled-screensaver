// All modes in one module. Output is premultiplied alpha, blended additively over
// the black backdrop. Pixel coordinates: origin top-left, y down.

struct U {
    res: vec2f,
    time: f32,
    fade: f32,          // content opacity (mode fade in/out)
    text_rect: vec4f,   // clock text quad: x, y, w, h in px
};
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct VOut {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
    @location(1) col: vec4f,
};

fn hash(n: f32) -> f32 { return fract(sin(n) * 43758.5453); }
fn hash2(p: vec2f) -> f32 { return fract(sin(dot(p, vec2f(127.1, 311.7))) * 43758.5453); }

fn hsl(h: f32, s: f32, l: f32) -> vec3f {
    let k = (vec3f(0.0, 8.0, 4.0) + fract(h) * 12.0) % 12.0;
    let a = s * min(l, 1.0 - l);
    return l - a * clamp(min(k - 3.0, 9.0 - k), vec3f(-1.0), vec3f(1.0));
}

fn ndc(p: vec2f) -> vec4f { return vec4f(p.x / u.res.x * 2.0 - 1.0, 1.0 - p.y / u.res.y * 2.0, 0.0, 1.0); }
fn out(c: vec3f, a: f32) -> vec4f { let k = clamp(a, 0.0, 1.0) * u.fade; return vec4f(c * k, k); }
// tiny noise against banding in dark gradients
fn dither(p: vec2f) -> f32 { return (hash2(p) - 0.5) / 255.0; }

// ---------- full-screen triangle ----------
@vertex fn vs_full(@builtin(vertex_index) i: u32) -> VOut {
    let p = vec2f(f32((i << 1u) & 2u), f32(i & 2u));
    var o: VOut;
    o.pos = vec4f(p * 2.0 - 1.0, 0.0, 1.0);
    return o;
}

// ---------- anti-aliased strips (lines, star streaks) ----------
// Builds one strip vertex `v` of a line through points a->b with half-width w px.
// uv.x = signed distance from the centre line, uv.y = half-width.
fn strip_vertex(p: vec2f, dir: vec2f, side: f32, w: f32, col: vec4f) -> VOut {
    let n = vec2f(-dir.y, dir.x);
    var o: VOut;
    o.pos = ndc(p + n * side * (w + 1.0));
    o.uv = vec2f(side * (w + 1.0), w);
    o.col = col;
    return o;
}

@fragment fn fs_line(i: VOut) -> @location(0) vec4f {
    let cover = 1.0 - smoothstep(i.uv.y - 0.5, i.uv.y + 0.75, abs(i.uv.x));
    return out(i.col.rgb, i.col.a * cover);
}

// ---------- Topo: animated contour lines of 3D gradient noise ----------
fn grad3(p: vec3f) -> vec3f {
    let h = vec3f(dot(p, vec3f(127.1, 311.7, 74.7)), dot(p, vec3f(269.5, 183.3, 246.1)), dot(p, vec3f(113.5, 271.9, 124.6)));
    return fract(sin(h) * 43758.5453) * 2.0 - 1.0;
}
fn gnoise(p: vec3f) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let s = f * f * f * (f * (f * 6.0 - 15.0) + 10.0);
    let c000 = dot(grad3(i + vec3f(0., 0., 0.)), f - vec3f(0., 0., 0.));
    let c100 = dot(grad3(i + vec3f(1., 0., 0.)), f - vec3f(1., 0., 0.));
    let c010 = dot(grad3(i + vec3f(0., 1., 0.)), f - vec3f(0., 1., 0.));
    let c110 = dot(grad3(i + vec3f(1., 1., 0.)), f - vec3f(1., 1., 0.));
    let c001 = dot(grad3(i + vec3f(0., 0., 1.)), f - vec3f(0., 0., 1.));
    let c101 = dot(grad3(i + vec3f(1., 0., 1.)), f - vec3f(1., 0., 1.));
    let c011 = dot(grad3(i + vec3f(0., 1., 1.)), f - vec3f(0., 1., 1.));
    let c111 = dot(grad3(i + vec3f(1., 1., 1.)), f - vec3f(1., 1., 1.));
    return mix(mix(mix(c000, c100, s.x), mix(c010, c110, s.x), s.y),
               mix(mix(c001, c101, s.x), mix(c011, c111, s.x), s.y), s.z);
}

@fragment fn fs_topo(@builtin(position) fc: vec4f) -> @location(0) vec4f {
    let p = fc.xy / u.res.y;
    let t = u.time * 0.02;
    let q = vec3f(p * 1.3 + vec2f(t * 0.7, t * 0.4), t);
    let hgt = gnoise(q) + 0.5 * gnoise(q * 2.1 + 3.7) + 0.2 * gnoise(q * 4.3 + 7.1);
    let n = hgt * 9.0;                              // contour levels
    let d = abs(fract(n - 0.5) - 0.5) / fwidth(n);  // px to the nearest contour
    let major = f32(i32(floor(n + 0.5)) % 4 == 0);   // every 4th line is thicker
    let w = mix(0.6, 1.7, major);
    let line = 1.0 - smoothstep(w - 0.5, w + 0.8, d);
    return out(vec3f(mix(0.7, 0.92, major)), line);
}

// ---------- Starfield: radial warp streaks ----------
@vertex fn vs_star(@builtin(vertex_index) v: u32, @builtin(instance_index) i: u32) -> VOut {
    let fi = f32(i) + 1.0;
    let dir = vec2f(hash(fi * 1.31) * 2.0 - 1.0, hash(fi * 7.17) * 2.0 - 1.0);
    let speed = 0.18 + 0.05 * sin(u.time * 0.2);
    let z = max(1.0 - fract(hash(fi * 3.7) + u.time * speed * 0.5), 0.02);
    let zt = min(z + 0.025, 1.0);
    let c = u.res * 0.5;
    let k = u.res.x * 0.25;
    let head = c + dir / z * k;
    let tail = c + dir / zt * k;
    let b = 1.0 - z;
    let w = 0.35 + b * 1.25;
    let ax = head - tail;
    let dn = ax / max(length(ax), 0.001);
    let along = f32(v >> 1u);
    let side = f32(v & 1u) * 2.0 - 1.0;
    let p = mix(tail - dn * w, head + dn * w, along);
    let hue = (200.0 + 40.0 * sin(u.time * 0.05)) / 360.0;
    return strip_vertex(p, dn, side, w, vec4f(hsl(hue, 0.5, 0.7 + b * 0.25), min(1.0, 0.25 + b * 1.2)));
}

// ---------- Aurora: drifting soft glows ----------
@fragment fn fs_aurora(@builtin(position) fc: vec4f) -> @location(0) vec4f {
    let t = u.time;
    let r = min(u.res.x, u.res.y) * 0.55;
    var c = vec3f(0.0);
    for (var k = 0; k < 5; k++) {
        let fk = f32(k);
        let fx = 0.03 + 0.012 * fk + 0.017 * f32(k % 2);
        let fy = 0.06 - 0.008 * fk;
        let ph = fk * 1.4;
        let ctr = u.res * vec2f(0.5 + 0.4 * sin(t * fx * 3.0 + ph), 0.5 + 0.35 * sin(t * fy * 3.0 + ph * 1.3));
        let a = 0.3 + 0.12 * sin(t * 0.3 + ph);
        let fall = max(1.0 - length(fc.xy - ctr) / r, 0.0);
        c += hsl((120.0 + fk * 50.0 + t * 4.0) / 360.0, 0.85, 0.5) * a * fall;
    }
    return out(max(c + dither(fc.xy), vec3f(0.0)), 1.0);
}

// ---------- Lissajous: morphing curve with echo trail ----------
const LSEG: f32 = 600.0;
fn liss(s: f32, tt: f32) -> vec2f {
    let a = 3.0 + sin(tt * 0.021) * 2.0;
    let b = 2.0 + cos(tt * 0.017) * 2.0;
    let c = u.res * 0.5 + vec2f(sin(tt * 0.05) * u.res.x * 0.15, cos(tt * 0.04) * u.res.y * 0.12);
    let r = min(u.res.x, u.res.y) * 0.38;
    return c + r * vec2f(sin(a * s + tt * 0.4), sin(b * s));
}
@vertex fn vs_liss(@builtin(vertex_index) v: u32, @builtin(instance_index) i: u32) -> VOut {
    let k = (f32(i) + 1.0) / 6.0;            // 6 echoes, newest brightest
    let tt = u.time - (1.0 - k) * 0.5;
    let s = f32(v >> 1u) / LSEG * 6.2831853;
    let p = liss(s, tt);
    let d = liss(s + 0.002, tt) - liss(s - 0.002, tt);
    let side = f32(v & 1u) * 2.0 - 1.0;
    return strip_vertex(p, d / max(length(d), 1e-4), side, 0.8 + k * 1.6, vec4f(hsl(tt * 8.0 / 360.0, 0.75, 0.62), k * k));
}

// ---------- Waves: layered sine ribbons ----------
const WSEG: f32 = 256.0;
fn wave_y(x: f32, i: f32, k: f32) -> f32 {
    let t = u.time;
    let h = u.res.y;
    let xh = x * 0.5;
    return h * (0.5 + 0.2 * sin(t * 0.03)) + (k - 0.5) * h * 0.35
        + sin(xh * 0.006 + t * 0.6 + i * 0.5) * h * 0.08
        + sin(xh * 0.013 - t * 0.4 + i) * h * 0.04;
}
@vertex fn vs_wave(@builtin(vertex_index) v: u32, @builtin(instance_index) i: u32) -> VOut {
    let fi = f32(i);
    let k = fi / 8.0;
    let x = f32(v >> 1u) / WSEG * u.res.x;
    let p = vec2f(x, wave_y(x, fi, k));
    let d = vec2f(2.0, wave_y(x + 1.0, fi, k) - wave_y(x - 1.0, fi, k));
    let side = f32(v & 1u) * 2.0 - 1.0;
    let sk = sin(3.14159265 * k);
    let col = vec4f(hsl((190.0 + k * 120.0 + u.time * 5.0) / 360.0, 0.65, 0.58), 0.3 + 0.5 * sk);
    return strip_vertex(p, normalize(d), side, 1.0 + 1.5 * sk, col);
}

// ---------- Clock: floating dots + moving text texture ----------
@vertex fn vs_dot(@builtin(vertex_index) v: u32, @builtin(instance_index) i: u32) -> VOut {
    let fi = f32(i) + 1.0;
    let vel = (vec2f(hash(fi * 5.3), hash(fi * 9.1)) - 0.5) * 24.0;  // px/s
    let pos = fract(vec2f(hash(fi * 1.1), hash(fi * 2.3)) + vel * u.time / u.res) * u.res;
    let r = 1.2 + hash(fi * 4.7) * 3.6;
    let h = hash(fi * 6.1);
    let corner = vec2f(f32(v & 1u), f32(v >> 1u)) * 2.0 - 1.0;
    let q = corner * (r + 1.0);
    var o: VOut;
    o.pos = ndc(pos + q);
    o.uv = vec2f(length(q), r);  // distance from centre, radius
    o.col = vec4f(hsl(h + u.time * 6.0 / 360.0, 0.7, 0.65), 0.45 + 0.35 * sin(u.time * 0.8 + h * 360.0));
    return o;
}
@fragment fn fs_dot(i: VOut) -> @location(0) vec4f {
    let cover = 1.0 - smoothstep(i.uv.y - 0.5, i.uv.y + 0.5, i.uv.x);
    return out(i.col.rgb, i.col.a * cover);
}

@vertex fn vs_text(@builtin(vertex_index) v: u32) -> VOut {
    let corner = vec2f(f32(v & 1u), f32(v >> 1u));
    var o: VOut;
    o.pos = ndc(u.text_rect.xy + corner * u.text_rect.zw);
    o.uv = corner;
    return o;
}
@fragment fn fs_text(i: VOut) -> @location(0) vec4f {
    let m = textureSample(tex, samp, i.uv).rg;
    let hue = fract(u.time * 0.01);
    let c = hsl(hue, 0.45, 0.7) * m.r + hsl(hue + 0.1, 0.3, 0.6) * m.g;
    let a = max(m.r, m.g) * 0.75;
    return vec4f(c * 0.75 * u.fade, a * u.fade);
}

// ---------- Pipes: tubes through a 3D grid (instances from pipes.rs) ----------
const PL: u32 = 8u;       // rings along one piece
const PS: u32 = 16u;      // sides around the tube
const PSTEP: f32 = 0.22;  // seconds per cell, as STEP in pipes.rs
const PR: f32 = 0.17;     // tube radius in cells

struct PipeOut {
    @builtin(position) pos: vec4f,
    @location(0) n: vec3f,
    @location(1) w: vec3f,
    @location(2) @interpolate(flat) color: u32,
};

var<private> PIPE_COLORS: array<vec3f, 8> = array<vec3f, 8>(
    vec3f(1.0, 0.42, 0.12), vec3f(0.85, 0.75, 0.15), vec3f(0.35, 0.9, 0.4), vec3f(0.9, 0.9, 0.9),
    vec3f(0.25, 0.5, 1.0), vec3f(0.95, 0.2, 0.25), vec3f(0.2, 0.85, 0.9), vec3f(0.7, 0.35, 0.95));

fn dir3(i: u32) -> vec3f {
    let s = 1.0 - 2.0 * f32(i & 1u);
    if (i < 2u) { return vec3f(s, 0.0, 0.0); }
    if (i < 4u) { return vec3f(0.0, s, 0.0); }
    return vec3f(0.0, 0.0, s);
}

// slow orbit around the grid, gently bobbing up and down
fn pipe_eye() -> vec3f {
    let a = u.time * 0.06;
    return vec3f(sin(a) * 12.5, 3.0 + 4.0 * sin(u.time * 0.037), cos(a) * 12.5);
}

fn pipe_clip(p: vec3f) -> vec4f {
    let eye = pipe_eye();
    let f = normalize(-eye);
    let r = normalize(cross(f, vec3f(0.0, 1.0, 0.0)));
    let up = cross(r, f);
    let q = p - eye;
    let v = vec3f(dot(q, r), dot(q, up), dot(q, f)); // view space, +z forward
    let fl = 1.0 / tan(0.45);                          // ~52 degree vertical field of view
    let near = 0.1;
    let far = 80.0;
    return vec4f(v.x * fl * u.res.y / u.res.x, v.y * fl, (v.z - near) * far / (far - near), v.z);
}

@vertex fn vs_pipe(@builtin(vertex_index) v: u32, @location(0) pb: vec4f, @location(1) info: vec4u) -> PipeOut {
    // 6 vertices per quad; corner pattern (0,0)(1,0)(0,1) (0,1)(1,0)(1,1)
    let quad = v / 6u;
    let k = v % 6u;
    let i = quad / PS + ((50u >> k) & 1u); // ring 0..PL
    let j = quad % PS + ((44u >> k) & 1u); // side 0..PS
    let kind = info.z;
    let len = select(1.0, 0.5, kind == 1u || kind == 2u);
    let grow = clamp((u.time - pb.w) / (PSTEP * len), 0.0, 1.0);
    let phi = f32(j) / f32(PS) * 6.2831853;
    var p: vec3f;
    var n: vec3f;
    if (kind == 3u) {
        // ball at a pipe's start or end
        let th = f32(i) / f32(PL) * 3.14159265;
        n = vec3f(sin(th) * cos(phi), cos(th), sin(th) * sin(phi));
        p = pb.xyz + n * PR * 1.4 * grow;
    } else {
        let s = f32(i) / f32(PL) * grow;
        let din = dir3(info.x);
        let dout = dir3(info.y);
        var c: vec3f;
        var t: vec3f;
        var b: vec3f;
        if (kind == 0u && info.x != info.y) {
            // quarter-torus elbow from the entry midpoint to the exit midpoint
            let a = s * 1.5707963;
            let ctr = pb.xyz + dout * 0.5;
            c = ctr - dout * 0.5 * cos(a) + din * 0.5 * sin(a);
            t = din * cos(a) + dout * sin(a);
            b = normalize(cross(din, dout));
        } else {
            let d = select(din, dout, kind == 1u);
            c = pb.xyz + d * s * len;
            t = d;
            b = select(vec3f(0.0, 1.0, 0.0), vec3f(1.0, 0.0, 0.0), abs(d.y) > 0.5);
        }
        n = cross(t, b) * cos(phi) + b * sin(phi);
        p = c + n * PR;
    }
    var o: PipeOut;
    o.pos = pipe_clip(p);
    o.n = n;
    o.w = p;
    o.color = info.w;
    return o;
}

@fragment fn fs_pipe(i: PipeOut) -> @location(0) vec4f {
    let n = normalize(i.n);
    let to_eye = pipe_eye() - i.w;
    let vdir = normalize(to_eye);
    let l = normalize(vdir + vec3f(0.3, 0.9, 0.2)); // light from above, following the camera
    let h = normalize(l + vdir);
    let base = PIPE_COLORS[i.color % 8u];
    let diff = max(dot(n, l), 0.0);
    let spec = pow(max(dot(n, h), 0.0), 48.0);
    let depth = exp(-max(length(to_eye) - 9.0, 0.0) * 0.07); // farther pipes are darker
    return out((base * (0.1 + 0.9 * diff) + vec3f(0.55) * spec) * depth, 1.0);
}

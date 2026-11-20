//! wgpu renderer: every mode is a set of procedural draws from `shaders/screensaver.wgsl`,
//! blended additively over a black backdrop that fades in. Pipes is the exception: 3D tubes
//! from an instance buffer, drawn with a depth buffer and normal "over" blending.

use std::{collections::HashMap, error::Error, f32::consts::PI, ptr::NonNull, time::Instant};

use raw_window_handle::{RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle};
use wayland_client::{Connection, Proxy, protocol::wl_surface::WlSurface};

use crate::{pipes, text};

const BLACK_FADE: f32 = 2.5; // desktop -> black
const FIRST_DELAY: f32 = 1.8; // animation starts fading in while the black settles
const FADE_IN: f32 = 3.0;
const FADE_OUT: f32 = 1.5;

struct Draw {
    vs: &'static str,
    fs: &'static str,
    verts: u32,
    insts: u32,
}

const fn d(vs: &'static str, fs: &'static str, verts: u32, insts: u32) -> Draw {
    Draw { vs, fs, verts, insts }
}

// Strip vertex counts must match LSEG / WSEG in the shader: 2 * (segments + 1).
const MODES: &[(&str, &[Draw])] = &[
    ("Topo", &[d("vs_full", "fs_topo", 3, 1)]),
    ("Starfield", &[d("vs_star", "fs_line", 4, 700)]),
    ("Aurora", &[d("vs_full", "fs_aurora", 3, 1)]),
    ("Lissajous", &[d("vs_liss", "fs_line", 2 * 601, 6)]),
    ("Waves", &[d("vs_wave", "fs_line", 2 * 257, 9)]),
    ("Clock", &[d("vs_dot", "fs_dot", 4, 140), d("vs_text", "fs_text", 4, 1)]),
    ("Pipes", &[d("vs_pipe", "fs_pipe", PIPE_VERTS, 0)]), // instance count comes from the simulation
    ("Sandbox", &[d("vs_full", "fs_sandbox", 3, 1)]),
];

const PIPE_VERTS: u32 = 8 * 16 * 6; // PL rings * PS sides * 2 triangles, see the shader
const DEPTH: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipelines: HashMap<(&'static str, &'static str), wgpu::RenderPipeline>,
    bgl: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    ubuf: wgpu::Buffer,
    sampler: wgpu::Sampler,
    font: Option<fontdue::Font>,
    text_size: (f32, f32),
    text_key: String,
    start: Instant,
    mode: usize,
    mode_start: f64,
    mode_delay: f32,
    rng: u64,
    allowed: Vec<usize>,
    mode_secs: f32,
    pipes: pipes::Pipes,
    pipe_buf: wgpu::Buffer,
    uploaded: usize, // pieces already in pipe_buf
    depth: wgpu::TextureView,
}

impl Renderer {
    /// `allowed`: indices into the mode table (empty = all), `mode_secs`: time per mode.
    pub async fn new(conn: &Connection, wl_surface: &WlSurface, w: u32, h: u32, allowed: Vec<usize>, mode_secs: f32) -> Result<Self, Box<dyn Error>> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::VULKAN, ..wgpu::InstanceDescriptor::new_without_display_handle() });
        let target = wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
                NonNull::new(conn.backend().display_ptr() as *mut _).ok_or("no wl_display")?,
            ))),
            raw_window_handle: RawWindowHandle::Wayland(WaylandWindowHandle::new(
                NonNull::new(wl_surface.id().as_ptr() as *mut _).ok_or("no wl_surface")?,
            )),
        };
        // SAFETY: the connection and wl_surface live in main() for the whole life of the renderer.
        let surface = unsafe { instance.create_surface_unsafe(target)? };
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await?;
        let (device, queue) = adapter.request_device(&wgpu::DeviceDescriptor::default()).await?;

        let caps = surface.get_capabilities(&adapter);
        // 8-bit non-sRGB target: shader colours are already in display space, and unorm
        // clamps alpha to 1 (a float target would let additive alpha exceed 1)
        let format = [wgpu::TextureFormat::Bgra8Unorm, wgpu::TextureFormat::Rgba8Unorm]
            .into_iter()
            .find(|f| caps.formats.contains(f))
            .unwrap_or(caps.formats[0]);
        let alpha_mode = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::PreMultiplied) {
            wgpu::CompositeAlphaMode::PreMultiplied // lets the desktop show through during the fade
        } else {
            caps.alpha_modes[0]
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: w.max(1),
            height: h.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
            color_space: Default::default(),
        };
        surface.configure(&device, &config);

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("screensaver"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/screensaver.wgsl").into()),
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[Some(&bgl)], ..Default::default() });
        let add = wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Add };

        let mut pipelines = HashMap::new();
        for (_, draws) in MODES {
            for dr in *draws {
                let pipe = dr.vs == "vs_pipe";
                let instances = [Some(wgpu::VertexBufferLayout {
                    array_stride: 32,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Uint32x4],
                })];
                pipelines.entry((dr.vs, dr.fs)).or_insert_with(|| {
                    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                        label: Some(dr.fs),
                        layout: Some(&layout),
                        vertex: wgpu::VertexState {
                            module: &module,
                            entry_point: Some(dr.vs),
                            compilation_options: Default::default(),
                            buffers: if pipe { &instances } else { &[] },
                        },
                        fragment: Some(wgpu::FragmentState {
                            module: &module,
                            entry_point: Some(dr.fs),
                            compilation_options: Default::default(),
                            targets: &[Some(wgpu::ColorTargetState {
                                format,
                                // solid tubes hide each other, so they draw "over" rather than add up
                                blend: Some(if pipe { wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING } else { wgpu::BlendState { color: add, alpha: add } }),
                                write_mask: wgpu::ColorWrites::ALL,
                            })],
                        }),
                        primitive: wgpu::PrimitiveState {
                            topology: if pipe { wgpu::PrimitiveTopology::TriangleList } else { wgpu::PrimitiveTopology::TriangleStrip },
                            ..Default::default()
                        },
                        depth_stencil: pipe.then(|| wgpu::DepthStencilState {
                            format: DEPTH,
                            depth_write_enabled: Some(true),
                            depth_compare: Some(wgpu::CompareFunction::Less),
                            stencil: Default::default(),
                            bias: Default::default(),
                        }),
                        multisample: Default::default(),
                        multiview_mask: None,
                        cache: None,
                    })
                });
            }
        }

        let ubuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let mut rng = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1) | 1;
        let allowed = if allowed.is_empty() { (0..MODES.len()).collect() } else { allowed };
        let forced = std::env::var("SCREENSAVER_MODE").ok().and_then(|m| {
            let i = mode_index(&m);
            if i.is_none() {
                eprintln!("SCREENSAVER_MODE: unknown mode {m:?} ignored");
            }
            i
        });
        let mode = forced.unwrap_or_else(|| allowed[xorshift(&mut rng) as usize % allowed.len()]);

        let pipe_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pipes"),
            size: (pipes::CAPACITY * 32) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let depth = make_depth(&device, config.width, config.height);
        let placeholder = Self::make_bind_group(&device, &queue, &bgl, &ubuf, &sampler, 1, 1, &[0, 0]);
        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipelines,
            bgl,
            bind_group: placeholder,
            ubuf,
            sampler,
            font: text::load_font(),
            text_size: (0.0, 0.0),
            text_key: String::new(),
            start: Instant::now(),
            mode,
            mode_start: 0.0,
            mode_delay: FIRST_DELAY,
            rng,
            allowed,
            mode_secs,
            pipes: pipes::Pipes::new(rng),
            pipe_buf,
            uploaded: 0,
            depth,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn make_bind_group(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bgl: &wgpu::BindGroupLayout,
        ubuf: &wgpu::Buffer,
        sampler: &wgpu::Sampler,
        w: u32,
        h: u32,
        rg: &[u8],
    ) -> wgpu::BindGroup {
        let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("text"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            rg,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 2), rows_per_image: Some(h) },
            size,
        );
        let view = tex.create_view(&Default::default());
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: ubuf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(sampler) },
            ],
        })
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        if w > 0 && h > 0 && (w, h) != (self.config.width, self.config.height) {
            self.config.width = w;
            self.config.height = h;
            self.surface.configure(&self.device, &self.config);
            self.depth = make_depth(&self.device, w, h);
        }
    }

    /// Re-rasterize the clock once the minute changes.
    fn update_text(&mut self) {
        let Some(font) = &self.font else { return };
        let now = chrono::Local::now();
        let time = now.format("%H:%M").to_string();
        if time == self.text_key {
            return;
        }
        let mask = text::render(font, &time, &now.format("%A, %-d %B").to_string());
        self.bind_group = Self::make_bind_group(&self.device, &self.queue, &self.bgl, &self.ubuf, &self.sampler, mask.w, mask.h, &mask.rg);
        self.text_size = (mask.w as f32, mask.h as f32);
        self.text_key = time;
    }

    /// Draws one frame; returns true once the backdrop is fully black (surface opaque).
    pub fn render(&mut self) -> bool {
        let now = self.start.elapsed().as_secs_f64();
        if now - self.mode_start >= self.mode_secs as f64 {
            self.mode = next_mode(&self.allowed, self.mode, xorshift(&mut self.rng));
            self.mode_start = now;
            self.mode_delay = 0.0;
            self.pipes = pipes::Pipes::new(xorshift(&mut self.rng)); // fresh scene each time Pipes comes up
            self.uploaded = 0;
        }
        // Shader time restarts with each mode: a growing f32 loses precision after hours and motion stutters.
        let t = (now - self.mode_start) as f32;
        let (black, mut fade) = fades(now as f32, self.mode_start as f32, self.mode_delay, self.mode_secs);
        let is_pipes = MODES[self.mode].0 == "Pipes";
        if is_pipes {
            self.pipes.update(t);
            let n = self.pipes.pieces.len();
            if n < self.uploaded {
                self.uploaded = 0; // the scene restarted
            }
            if n > self.uploaded {
                let bytes = pipes::Piece::bytes(&self.pipes.pieces[self.uploaded..]);
                self.queue.write_buffer(&self.pipe_buf, (self.uploaded * 32) as u64, &bytes);
                self.uploaded = n;
            }
            fade *= self.pipes.fade(t);
        }

        let (w, h) = (self.config.width as f32, self.config.height as f32);
        let mut rect = [0.0f32; 4];
        if MODES[self.mode].0 == "Clock" {
            self.update_text();
            let (tw, th) = self.text_size;
            // slow Lissajous drift so no pixel stays lit
            rect = [(w - tw) / 2.0 + (t * 0.031).sin() * w * 0.3, (h - th) / 2.0 + (t * 0.047).sin() * h * 0.3, tw, th];
        }
        let uniforms = [w, h, t, fade, rect[0], rect[1], rect[2], rect[3]];
        let bytes: Vec<u8> = uniforms.iter().flat_map(|f| f.to_ne_bytes()).collect();
        self.queue.write_buffer(&self.ubuf, 0, &bytes);

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return false;
            }
            _ => return false, // timeout / occluded: skip this frame
        };
        let view = frame.texture.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: black as f64 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: is_pipes.then_some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                    stencil_ops: None,
                }),
                ..Default::default()
            });
            pass.set_bind_group(0, &self.bind_group, &[]);
            for dr in MODES[self.mode].1 {
                if dr.fs == "fs_text" && self.text_size.0 == 0.0 {
                    continue; // no font found
                }
                pass.set_pipeline(&self.pipelines[&(dr.vs, dr.fs)]);
                if dr.vs == "vs_pipe" {
                    pass.set_vertex_buffer(0, self.pipe_buf.slice(..));
                    pass.draw(0..dr.verts, 0..self.uploaded as u32);
                } else {
                    pass.draw(0..dr.verts, 0..dr.insts);
                }
            }
        }
        self.queue.submit([enc.finish()]);
        self.queue.present(frame);
        black >= 1.0
    }
}

/// (backdrop opacity, animation opacity) at `t` seconds since start, for a mode that
/// began at `mode_start` and waits `delay` before fading in.
fn fades(t: f32, mode_start: f32, delay: f32, mode_secs: f32) -> (f32, f32) {
    let ease = |x: f32| 0.5 - 0.5 * (PI * x.clamp(0.0, 1.0)).cos();
    let content = ease((t - mode_start - delay) / FADE_IN).min(ease((mode_start + mode_secs - t) / FADE_OUT));
    (ease(t / BLACK_FADE), content)
}

fn make_depth(device: &wgpu::Device, w: u32, h: u32) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("depth"),
            size: wgpu::Extent3d { width: w.max(1), height: h.max(1), depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&Default::default())
}

/// Index of a mode by name, ignoring case.
pub fn mode_index(name: &str) -> Option<usize> {
    MODES.iter().position(|(n, _)| n.eq_ignore_ascii_case(name))
}

/// A random allowed mode other than `current` (stays put when only one is allowed).
fn next_mode(allowed: &[usize], current: usize, rnd: u64) -> usize {
    let others: Vec<usize> = allowed.iter().copied().filter(|&m| m != current).collect();
    if others.is_empty() { current } else { others[rnd as usize % others.len()] }
}

pub(crate) fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

#[cfg(test)]
mod tests {
    use super::*;
    const MODE_SECS: f32 = 300.0;

    #[test]
    fn backdrop_fades_in_once_and_stays_black() {
        let (b0, _) = fades(0.0, 0.0, FIRST_DELAY, MODE_SECS);
        assert_eq!(b0, 0.0, "starts transparent so the desktop is visible");
        let mut prev = 0.0;
        for i in 0..=100 {
            let (b, _) = fades(i as f32 * 0.05, 0.0, FIRST_DELAY, MODE_SECS);
            assert!(b >= prev, "backdrop must never brighten back to the desktop");
            prev = b;
        }
        assert_eq!(fades(BLACK_FADE, 0.0, FIRST_DELAY, MODE_SECS).0, 1.0);
        assert_eq!(fades(10_000.0, 0.0, FIRST_DELAY, MODE_SECS).0, 1.0);
    }

    #[test]
    fn animation_fades_in_after_delay_and_out_before_switch() {
        assert_eq!(fades(FIRST_DELAY, 0.0, FIRST_DELAY, MODE_SECS).1, 0.0);
        assert_eq!(fades(FIRST_DELAY + FADE_IN, 0.0, FIRST_DELAY, MODE_SECS).1, 1.0);
        assert_eq!(fades(MODE_SECS / 2.0, 0.0, FIRST_DELAY, MODE_SECS).1, 1.0);
        assert!(fades(MODE_SECS - FADE_OUT / 2.0, 0.0, FIRST_DELAY, MODE_SECS).1 < 1.0);
        assert_eq!(fades(MODE_SECS, 0.0, FIRST_DELAY, MODE_SECS).1, 0.0);
        // next mode starts at MODE_SECS without the initial delay
        assert_eq!(fades(MODE_SECS + FADE_IN, MODE_SECS, 0.0, MODE_SECS).1, 1.0);
    }

    #[test]
    fn next_mode_never_repeats_and_stays_allowed() {
        let allowed = [0, 2, 5];
        for rnd in 0..50 {
            for cur in allowed {
                let n = next_mode(&allowed, cur, rnd);
                assert!(n != cur && allowed.contains(&n));
            }
        }
        assert_eq!(next_mode(&[3], 3, 7), 3, "a single mode stays");
    }

    #[test]
    fn short_custom_mode_time_still_fades_out() {
        assert_eq!(fades(30.0, 0.0, 0.0, 30.0).1, 0.0);
        assert_eq!(fades(15.0, 0.0, 0.0, 30.0).1, 1.0);
    }

    #[test]
    fn mode_tables_match_shader_segment_counts() {
        let src = include_str!("../shaders/screensaver.wgsl");
        for (name, draws) in MODES {
            for d in *draws {
                assert!(src.contains(&format!("fn {}(", d.vs)), "{name}: missing {}", d.vs);
                assert!(src.contains(&format!("fn {}(", d.fs)), "{name}: missing {}", d.fs);
            }
        }
        let seg = |c: &str| -> u32 {
            let rest = src.split(&format!("const {c}: f32 = ")).nth(1).expect(c);
            rest[..rest.find(".0;").expect(c)].parse().expect(c)
        };
        let verts = |name: &str| MODES.iter().find(|m| m.0 == name).unwrap().1[0].verts;
        assert_eq!(verts("Lissajous"), 2 * (seg("LSEG") + 1));
        assert_eq!(verts("Waves"), 2 * (seg("WSEG") + 1));
        assert!(src.contains("const PL: u32 = 8u;") && src.contains("const PS: u32 = 16u;") && PIPE_VERTS == 8 * 16 * 6);
        assert!(src.contains(&format!("const PSTEP: f32 = {:?};", pipes::STEP)), "shader growth speed must match pipes::STEP");
    }
}

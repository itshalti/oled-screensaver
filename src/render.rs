//! wgpu renderer: every mode is a set of procedural draws from `shaders/screensaver.wgsl`
//! (no vertex buffers), blended additively over a black backdrop that fades in.

use std::{collections::HashMap, error::Error, f32::consts::PI, ptr::NonNull, time::Instant};

use raw_window_handle::{RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle};
use wayland_client::{Connection, Proxy, protocol::wl_surface::WlSurface};

use crate::text;

/// Seconds per mode before switching to another random one.
const MODE_SECS: f32 = 300.0;
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
];

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
}

impl Renderer {
    pub async fn new(conn: &Connection, wl_surface: &WlSurface, w: u32, h: u32) -> Result<Self, Box<dyn Error>> {
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
                pipelines.entry((dr.vs, dr.fs)).or_insert_with(|| {
                    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                        label: Some(dr.fs),
                        layout: Some(&layout),
                        vertex: wgpu::VertexState { module: &module, entry_point: Some(dr.vs), compilation_options: Default::default(), buffers: &[] },
                        fragment: Some(wgpu::FragmentState {
                            module: &module,
                            entry_point: Some(dr.fs),
                            compilation_options: Default::default(),
                            targets: &[Some(wgpu::ColorTargetState {
                                format,
                                blend: Some(wgpu::BlendState { color: add, alpha: add }),
                                write_mask: wgpu::ColorWrites::ALL,
                            })],
                        }),
                        primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
                        depth_stencil: None,
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
        let forced = std::env::var("SCREENSAVER_MODE").ok().and_then(|m| MODES.iter().position(|(n, _)| n.eq_ignore_ascii_case(&m)));
        let mode = forced.unwrap_or_else(|| (xorshift(&mut rng) % MODES.len() as u64) as usize);

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
        if now - self.mode_start >= MODE_SECS as f64 {
            let step = 1 + xorshift(&mut self.rng) as usize % (MODES.len() - 1); // never the same mode twice
            self.mode = (self.mode + step) % MODES.len();
            self.mode_start = now;
            self.mode_delay = 0.0;
        }
        // Shader time restarts with each mode: a growing f32 loses precision after hours and motion stutters.
        let t = (now - self.mode_start) as f32;
        let (black, fade) = fades(now as f32, self.mode_start as f32, self.mode_delay);

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
                ..Default::default()
            });
            pass.set_bind_group(0, &self.bind_group, &[]);
            for dr in MODES[self.mode].1 {
                if dr.fs == "fs_text" && self.text_size.0 == 0.0 {
                    continue; // no font found
                }
                pass.set_pipeline(&self.pipelines[&(dr.vs, dr.fs)]);
                pass.draw(0..dr.verts, 0..dr.insts);
            }
        }
        self.queue.submit([enc.finish()]);
        self.queue.present(frame);
        black >= 1.0
    }
}

/// (backdrop opacity, animation opacity) at `t` seconds since start, for a mode that
/// began at `mode_start` and waits `delay` before fading in.
fn fades(t: f32, mode_start: f32, delay: f32) -> (f32, f32) {
    let ease = |x: f32| 0.5 - 0.5 * (PI * x.clamp(0.0, 1.0)).cos();
    let content = ease((t - mode_start - delay) / FADE_IN).min(ease((mode_start + MODE_SECS - t) / FADE_OUT));
    (ease(t / BLACK_FADE), content)
}

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backdrop_fades_in_once_and_stays_black() {
        let (b0, _) = fades(0.0, 0.0, FIRST_DELAY);
        assert_eq!(b0, 0.0, "starts transparent so the desktop is visible");
        let mut prev = 0.0;
        for i in 0..=100 {
            let (b, _) = fades(i as f32 * 0.05, 0.0, FIRST_DELAY);
            assert!(b >= prev, "backdrop must never brighten back to the desktop");
            prev = b;
        }
        assert_eq!(fades(BLACK_FADE, 0.0, FIRST_DELAY).0, 1.0);
        assert_eq!(fades(10_000.0, 0.0, FIRST_DELAY).0, 1.0);
    }

    #[test]
    fn animation_fades_in_after_delay_and_out_before_switch() {
        assert_eq!(fades(FIRST_DELAY, 0.0, FIRST_DELAY).1, 0.0);
        assert_eq!(fades(FIRST_DELAY + FADE_IN, 0.0, FIRST_DELAY).1, 1.0);
        assert_eq!(fades(MODE_SECS / 2.0, 0.0, FIRST_DELAY).1, 1.0);
        assert!(fades(MODE_SECS - FADE_OUT / 2.0, 0.0, FIRST_DELAY).1 < 1.0);
        assert_eq!(fades(MODE_SECS, 0.0, FIRST_DELAY).1, 0.0);
        // next mode starts at MODE_SECS without the initial delay
        assert_eq!(fades(MODE_SECS + FADE_IN, MODE_SECS, 0.0).1, 1.0);
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
    }
}

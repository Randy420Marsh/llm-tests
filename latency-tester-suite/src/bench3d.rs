//! 3D graphics benchmark: a lit scene of spinning, textured cubes rendered with wgpu (Vulkan first)
//! into off-screen images at the common 16:9 resolutions. Off screen, so the result does not depend on
//! the window, the display's refresh rate or VSync: it is what the GPU and driver can do.
//!
//! Per resolution:
//! * **Throughput:** frames rendered back to back with two in flight (like a game with a frame queued);
//!   frame times come from the gaps between frame completions: average FPS, 1 % and 0.1 % lows, p50 /
//!   p95 / p99 / worst frame time.
//! * **Latency:** one frame at a time, time from submitting the frame to the GPU having finished it (the
//!   render part of click-to-photon), and the GPU's own time for the frame from timestamp queries when the
//!   device supports them.
//!
//! The work per pixel is fixed (procedural texture with a set number of noise octaves), so higher
//! resolutions cost proportionally more, like a game's resolution scaling.

use anyhow::{anyhow, Result};
use eframe::egui_wgpu::wgpu;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use wgpu::util::DeviceExt;

use crate::cancel::{self, CancelFlag};
use crate::progress::{self, SharedProgress, SharedResults};
use crate::sensors::{Sampler, Telemetry};

/// The 16:9 resolutions offered
pub const RESOLUTIONS: [(u32, u32, &str); 4] = [(1280, 720, "720p"), (1920, 1080, "1080p"), (2560, 1440, "1440p"), (3840, 2160, "4K")];

pub fn resolution_name(w: u32, h: u32) -> String {
    RESOLUTIONS.iter().find(|r| r.0 == w && r.1 == h).map(|r| r.2.to_string()).unwrap_or_else(|| format!("{}×{}", w, h))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Detail {
    Low,
    Medium,
    High,
    Ultra,
}

impl Detail {
    pub const ALL: [Detail; 4] = [Detail::Low, Detail::Medium, Detail::High, Detail::Ultra];

    /// (cubes per side of the grid, noise octaves per pixel)
    pub fn params(self) -> (u32, u32) {
        match self {
            Detail::Low => (16, 2),
            Detail::Medium => (24, 4),
            Detail::High => (32, 6),
            Detail::Ultra => (40, 10),
        }
    }

    pub fn label(self) -> String {
        let (n, o) = self.params();
        format!("{:?} ({} cubes, {} texture octaves)", self, n * n * n, o)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Bench3dConfig {
    /// (width, height)
    pub resolutions: Vec<(u32, u32)>,
    pub detail: Detail,
    /// 1 or 4
    pub msaa: u32,
    pub warmup_s: f64,
    /// Back-to-back rendering per resolution
    pub duration_s: f64,
    /// One-frame-at-a-time latency measurement per resolution
    pub latency_s: f64,
}

impl Default for Bench3dConfig {
    fn default() -> Self {
        Self {
            resolutions: RESOLUTIONS.iter().map(|r| (r.0, r.1)).collect(),
            detail: Detail::High,
            msaa: 4,
            warmup_s: 2.0,
            duration_s: 10.0,
            latency_s: 3.0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Bench3dResult {
    pub width: u32,
    pub height: u32,
    /// "1080p"
    pub name: String,
    pub detail: String,
    pub msaa: u32,
    pub frames: u64,
    pub duration_s: f64,
    pub fps_avg: f64,
    /// FPS of the slowest 1 % / 0.1 % of frames (average frame time of that slice)
    pub fps_1pct_low: f64,
    pub fps_01pct_low: f64,
    pub frametime_avg_ms: f64,
    pub frametime_p50_ms: f64,
    pub frametime_p95_ms: f64,
    pub frametime_p99_ms: f64,
    pub frametime_max_ms: f64,
    /// Submit → GPU finished, one frame at a time
    pub latency_avg_ms: f64,
    pub latency_p50_ms: f64,
    pub latency_p99_ms: f64,
    /// GPU time of one frame from timestamp queries (None when the device has none)
    pub gpu_time_avg_ms: Option<f64>,
    pub gpu_time_p99_ms: Option<f64>,
    /// Frame times, ms, in order (thinned to at most 5000 for the charts)
    pub frametimes_ms: Vec<f64>,
    #[serde(default)]
    pub telemetry: Telemetry,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Bench3dSummary {
    pub adapter: String,
    pub backend: String,
    pub driver: String,
    pub config: Option<Bench3dConfig>,
    pub results: Vec<Bench3dResult>,
    /// Last frame of the first resolution, scaled down (width, height, RGBA), shown in the app
    #[serde(skip)]
    pub preview: Option<(u32, u32, Vec<u8>)>,
}

pub struct Bench3d {
    cfg: Bench3dConfig,
    sensors: Option<Arc<Sampler>>,
    progress: Option<SharedProgress>,
    partial: Option<SharedResults<Bench3dResult>>,
    cancel: CancelFlag,
}

// ---------------------------------------------------------------------------------------------
// scene
// ---------------------------------------------------------------------------------------------

const SHADER: &str = r#"
struct Globals {
    view_proj: mat4x4<f32>,
    eye: vec4<f32>,
    time: f32,
    octaves: u32,
    _pad0: f32,
    _pad1: f32,
};
@group(0) @binding(0) var<uniform> g: Globals;

struct VIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) inst: vec4<f32>,
};
struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) wpos: vec3<f32>,
    @location(1) n: vec3<f32>,
    @location(2) tint: vec3<f32>,
};

fn rotation(a: f32, axis: vec3<f32>) -> mat3x3<f32> {
    let c = cos(a);
    let s = sin(a);
    let t = 1.0 - c;
    let x = axis.x; let y = axis.y; let z = axis.z;
    return mat3x3<f32>(
        vec3<f32>(t*x*x + c,   t*x*y + s*z, t*x*z - s*y),
        vec3<f32>(t*x*y - s*z, t*y*y + c,   t*y*z + s*x),
        vec3<f32>(t*x*z + s*y, t*y*z - s*x, t*z*z + c));
}

@vertex
fn vs(v: VIn) -> VOut {
    let axis = normalize(vec3<f32>(sin(v.inst.w), 1.0, cos(v.inst.w * 1.7)));
    let r = rotation(g.time * 1.3 + v.inst.w * 6.0, axis);
    let world = r * (v.pos * 0.42) + v.inst.xyz;
    var o: VOut;
    o.clip = g.view_proj * vec4<f32>(world, 1.0);
    o.wpos = world;
    o.n = r * v.normal;
    o.tint = 0.55 + 0.45 * cos(vec3<f32>(0.0, 2.1, 4.2) + v.inst.w * 9.0);
    return o;
}

fn hash(p: vec3<f32>) -> f32 {
    let q = fract(p * 0.3183099 + vec3<f32>(0.1, 0.2, 0.3)) * 17.0;
    return fract(q.x * q.y * q.z * (q.x + q.y + q.z));
}

fn noise(x: vec3<f32>) -> f32 {
    let i = floor(x);
    let f = fract(x);
    let u = f * f * (3.0 - 2.0 * f);
    return mix(mix(mix(hash(i + vec3<f32>(0.0, 0.0, 0.0)), hash(i + vec3<f32>(1.0, 0.0, 0.0)), u.x),
                   mix(hash(i + vec3<f32>(0.0, 1.0, 0.0)), hash(i + vec3<f32>(1.0, 1.0, 0.0)), u.x), u.y),
               mix(mix(hash(i + vec3<f32>(0.0, 0.0, 1.0)), hash(i + vec3<f32>(1.0, 0.0, 1.0)), u.x),
                   mix(hash(i + vec3<f32>(0.0, 1.0, 1.0)), hash(i + vec3<f32>(1.0, 1.0, 1.0)), u.x), u.y), u.z);
}

@fragment
fn fs(i: VOut) -> @location(0) vec4<f32> {
    // procedural "marble": fixed work per pixel, so the cost follows the resolution
    var p = i.wpos * 3.0 + vec3<f32>(g.time * 0.2);
    var amp = 0.5;
    var v = 0.0;
    for (var k = 0u; k < g.octaves; k = k + 1u) {
        v = v + amp * noise(p);
        p = p * 2.03;
        amp = amp * 0.5;
    }
    let base = i.tint * (0.55 + 0.45 * sin(v * 12.0 + i.wpos.y * 2.0));
    let n = normalize(i.n);
    let view = normalize(g.eye.xyz - i.wpos);
    var col = base * 0.12;
    var lights = array<vec3<f32>, 3>(vec3<f32>(0.6, 0.8, 0.3), vec3<f32>(-0.7, 0.3, 0.6), vec3<f32>(0.1, -0.6, -0.8));
    var colours = array<vec3<f32>, 3>(vec3<f32>(1.0, 0.95, 0.85), vec3<f32>(0.35, 0.5, 1.0), vec3<f32>(1.0, 0.45, 0.3));
    for (var l = 0; l < 3; l = l + 1) {
        let ld = normalize(lights[l]);
        let diff = max(dot(n, ld), 0.0);
        let h = normalize(ld + view);
        let spec = pow(max(dot(n, h), 0.0), 48.0);
        col = col + colours[l] * (base * diff + vec3<f32>(0.35) * spec);
    }
    return vec4<f32>(col, 1.0);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    pos: [f32; 3],
    normal: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Globals {
    view_proj: [[f32; 4]; 4],
    eye: [f32; 4],
    time: f32,
    octaves: u32,
    _pad: [f32; 2],
}

/// 36 vertices of a unit cube with face normals
fn cube() -> Vec<Vertex> {
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
    ];
    let mut v = Vec::with_capacity(36);
    for (n, a, b) in faces {
        let corner = |sa: f32, sb: f32| Vertex { pos: [n[0] + a[0] * sa + b[0] * sb, n[1] + a[1] * sa + b[1] * sb, n[2] + a[2] * sa + b[2] * sb], normal: n };
        for (sa, sb) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            v.push(corner(sa, sb));
        }
    }
    v
}

/// Instance data: centre and a phase, cubes on an n×n×n grid around the origin
fn instances(n: u32) -> Vec<[f32; 4]> {
    let half = (n as f32 - 1.0) / 2.0;
    let mut v = Vec::with_capacity((n * n * n) as usize);
    for x in 0..n {
        for y in 0..n {
            for z in 0..n {
                let phase = ((x * 73 + y * 151 + z * 283) % 997) as f32 / 997.0;
                v.push([x as f32 - half, y as f32 - half, z as f32 - half, phase]);
            }
        }
    }
    v
}

fn mat_mul(a: [[f32; 4]; 4], b: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
    // column-major: out[c][r] = sum_k a[k][r] * b[c][k]
    let mut o = [[0.0f32; 4]; 4];
    for c in 0..4 {
        for r in 0..4 {
            o[c][r] = (0..4).map(|k| a[k][r] * b[c][k]).sum();
        }
    }
    o
}

/// Perspective (depth 0..1, as wgpu wants) times a look-at view, column-major
fn view_proj(eye: [f32; 3], aspect: f32) -> [[f32; 4]; 4] {
    let (fov, near, far) = (60f32.to_radians(), 0.1f32, 200.0f32);
    let f = 1.0 / (fov / 2.0).tan();
    let proj = [[f / aspect, 0.0, 0.0, 0.0], [0.0, f, 0.0, 0.0], [0.0, 0.0, far / (near - far), -1.0], [0.0, 0.0, near * far / (near - far), 0.0]];
    let sub = |a: [f32; 3], b: [f32; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    let norm = |a: [f32; 3]| {
        let l = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt();
        [a[0] / l, a[1] / l, a[2] / l]
    };
    let cross = |a: [f32; 3], b: [f32; 3]| [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let fwd = norm(sub([0.0, 0.0, 0.0], eye));
    let side = norm(cross(fwd, [0.0, 1.0, 0.0]));
    let up = cross(side, fwd);
    let view = [
        [side[0], up[0], -fwd[0], 0.0],
        [side[1], up[1], -fwd[1], 0.0],
        [side[2], up[2], -fwd[2], 0.0],
        [-dot(side, eye), -dot(up, eye), dot(fwd, eye), 1.0],
    ];
    mat_mul(proj, view)
}

/// Frame-time statistics: (fps avg, 1 % low, 0.1 % low, avg, p50, p95, p99, max) in ms / FPS
pub fn frame_stats(ft_ms: &[f64]) -> (f64, f64, f64, f64, f64, f64, f64, f64) {
    if ft_ms.is_empty() {
        return Default::default();
    }
    let mut s = ft_ms.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let n = s.len();
    let avg = s.iter().sum::<f64>() / n as f64;
    let at = |p: f64| s[((n as f64 * p) as usize).min(n - 1)];
    // "1 % low": the average frame time of the slowest 1 % of frames, as FPS
    let low = |frac: f64| {
        let k = ((n as f64 * frac).ceil() as usize).clamp(1, n);
        let slow = &s[n - k..];
        1000.0 / (slow.iter().sum::<f64>() / k as f64)
    };
    (1000.0 / avg, low(0.01), low(0.001), avg, at(0.5), at(0.95), at(0.99), s[n - 1])
}

impl Bench3d {
    pub fn new(cfg: Bench3dConfig) -> Self {
        Self { cfg, sensors: None, progress: None, partial: None, cancel: cancel::new_flag() }
    }

    pub fn with_sensors(mut self, s: Arc<Sampler>) -> Self {
        self.sensors = Some(s);
        self
    }

    pub fn with_progress(mut self, p: SharedProgress, partial: SharedResults<Bench3dResult>) -> Self {
        self.progress = Some(p);
        self.partial = Some(partial);
        self
    }

    pub fn with_cancel(mut self, flag: CancelFlag) -> Self {
        self.cancel = flag;
        self
    }

    pub fn run(&mut self) -> Result<Bench3dSummary> {
        let started = Instant::now();
        let total = self.cfg.resolutions.len();
        progress::update(&self.progress, |p| *p = progress::RunProgress { total, started: Some(started), title: "starting the GPU".into(), ..Default::default() });
        let out = self.run_inner();
        progress::update(&self.progress, |p| p.finished = Some(Instant::now()));
        out
    }

    fn run_inner(&mut self) -> Result<Bench3dSummary> {
        // Vulkan first (what the app draws with), anything else the machine has otherwise
        let (adapter, instance) = [wgpu::Backends::VULKAN, wgpu::Backends::all()]
            .into_iter()
            .find_map(|b| {
                let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: b, ..Default::default() });
                let a = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    force_fallback_adapter: false,
                    compatible_surface: None,
                }))?;
                Some((a, instance))
            })
            .ok_or_else(|| anyhow!("no GPU adapter found (Vulkan, DirectX or OpenGL)"))?;
        let _keep = instance;
        let info = adapter.get_info();
        let timestamps = adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY);
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("bench3d"),
                required_features: if timestamps { wgpu::Features::TIMESTAMP_QUERY } else { wgpu::Features::empty() },
                // the adapter's own texture size limit, so 4K targets fit
                required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .map_err(|e| anyhow!("could not open the GPU: {}", e))?;
        // wgpu panics on errors nobody catches: report them as a failed test instead
        let gpu_error: Arc<std::sync::Mutex<Option<String>>> = Default::default();
        {
            let slot = gpu_error.clone();
            device.on_uncaptured_error(Box::new(move |e| {
                if let Ok(mut s) = slot.lock() {
                    s.get_or_insert_with(|| e.to_string());
                }
            }));
        }
        device.push_error_scope(wgpu::ErrorFilter::Validation);
        let msaa = if self.cfg.msaa >= 4 {
            let f = adapter.get_texture_format_features(wgpu::TextureFormat::Rgba8Unorm);
            if f.flags.contains(wgpu::TextureFormatFeatureFlags::MULTISAMPLE_X4) { 4 } else { 1 }
        } else {
            1
        };
        let (grid, octaves) = self.cfg.detail.params();
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("bench3d"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
        let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("cube"), contents: bytemuck::cast_slice(&cube()), usage: wgpu::BufferUsages::VERTEX });
        let inst = instances(grid);
        let ibuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("instances"), contents: bytemuck::cast_slice(&inst), usage: wgpu::BufferUsages::VERTEX });
        let ubuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            }],
        });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &bgl, entries: &[wgpu::BindGroupEntry { binding: 0, resource: ubuf.as_entire_binding() }] });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[&bgl], push_constant_ranges: &[] });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("bench3d"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs",
                compilation_options: Default::default(),
                buffers: &[
                    wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<Vertex>() as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3],
                    },
                    wgpu::VertexBufferLayout { array_stride: 16, step_mode: wgpu::VertexStepMode::Instance, attributes: &wgpu::vertex_attr_array![2 => Float32x4] },
                ],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs",
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format: wgpu::TextureFormat::Rgba8Unorm, blend: None, write_mask: wgpu::ColorWrites::ALL })],
            }),
            primitive: wgpu::PrimitiveState { cull_mode: Some(wgpu::Face::Back), ..Default::default() },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: true,
                depth_compare: wgpu::CompareFunction::Less,
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState { count: msaa, ..Default::default() },
            multiview: None,
            cache: None,
        });
        if let Some(e) = pollster::block_on(device.pop_error_scope()) {
            return Err(anyhow!("the 3D scene could not be set up on {}: {}", info.name, e));
        }
        let queries = timestamps.then(|| {
            let set = device.create_query_set(&wgpu::QuerySetDescriptor { label: None, ty: wgpu::QueryType::Timestamp, count: 2 });
            let resolve = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: 16, usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC, mapped_at_creation: false });
            let read = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: 16, usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
            (set, resolve, read)
        });
        let period_ns = queue.get_timestamp_period() as f64;

        let mut summary = Bench3dSummary {
            adapter: info.name.clone(),
            backend: format!("{:?}", info.backend),
            driver: format!("{} {}", info.driver, info.driver_info).trim().to_string(),
            config: Some(self.cfg.clone()),
            results: Vec::new(),
            preview: None,
        };
        let clock = Instant::now();
        for (ri, &(w, h)) in self.cfg.resolutions.clone().iter().enumerate() {
            cancel::check(&self.cancel)?;
            let name = resolution_name(w, h);
            progress::update(&self.progress, |p| {
                p.title = format!("3D · {} ({}×{}) · {}", name, w, h, self.cfg.detail.label());
                p.detail = "warming up".into();
            });
            let make = |format: wgpu::TextureFormat, samples: u32, usage: wgpu::TextureUsages| {
                device
                    .create_texture(&wgpu::TextureDescriptor {
                        label: None,
                        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                        mip_level_count: 1,
                        sample_count: samples,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | usage,
                        view_formats: &[],
                    })
            };
            let color_tex = make(wgpu::TextureFormat::Rgba8Unorm, 1, wgpu::TextureUsages::COPY_SRC);
            let color = color_tex.create_view(&Default::default());
            let msaa_view = (msaa > 1).then(|| make(wgpu::TextureFormat::Rgba8Unorm, msaa, wgpu::TextureUsages::empty()).create_view(&Default::default()));
            let depth = make(wgpu::TextureFormat::Depth32Float, msaa, wgpu::TextureUsages::empty()).create_view(&Default::default());
            let aspect = w as f32 / h as f32;
            let sensor_t0 = self.sensors.as_ref().map(|s| s.now_ms());

            // one frame: update the camera and time, draw everything
            let frame = |with_ts: bool| -> wgpu::SubmissionIndex {
                let t = clock.elapsed().as_secs_f32();
                let dist = grid as f32 * 0.95 + 3.0;
                let eye = [dist * (t * 0.25).cos(), grid as f32 * 0.35 * (t * 0.17).sin(), dist * (t * 0.25).sin()];
                let g = Globals { view_proj: view_proj(eye, aspect), eye: [eye[0], eye[1], eye[2], 1.0], time: t, octaves, _pad: [0.0; 2] };
                queue.write_buffer(&ubuf, 0, bytemuck::bytes_of(&g));
                let mut enc = device.create_command_encoder(&Default::default());
                {
                    let (view, resolve) = match &msaa_view {
                        Some(m) => (m, Some(&color)),
                        None => (&color, None),
                    };
                    let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: None,
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view,
                            resolve_target: resolve,
                            ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.02, g: 0.025, b: 0.04, a: 1.0 }), store: wgpu::StoreOp::Store },
                        })],
                        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                            view: &depth,
                            depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                            stencil_ops: None,
                        }),
                        timestamp_writes: match (&queries, with_ts) {
                            (Some((set, _, _)), true) => Some(wgpu::RenderPassTimestampWrites { query_set: set, beginning_of_pass_write_index: Some(0), end_of_pass_write_index: Some(1) }),
                            _ => None,
                        },
                        occlusion_query_set: None,
                    });
                    pass.set_pipeline(&pipeline);
                    pass.set_bind_group(0, &bind, &[]);
                    pass.set_vertex_buffer(0, vbuf.slice(..));
                    pass.set_vertex_buffer(1, ibuf.slice(..));
                    pass.draw(0..36, 0..inst.len() as u32);
                }
                if let (Some((set, resolve, read)), true) = (&queries, with_ts) {
                    enc.resolve_query_set(set, 0..2, resolve, 0);
                    enc.copy_buffer_to_buffer(resolve, 0, read, 0, 16);
                }
                queue.submit(Some(enc.finish()))
            };

            // warm-up
            let warm_until = Instant::now() + std::time::Duration::from_secs_f64(self.cfg.warmup_s);
            while Instant::now() < warm_until {
                cancel::check(&self.cancel)?;
                let s = frame(false);
                device.poll(wgpu::Maintain::WaitForSubmissionIndex(s));
            }

            // throughput: two frames in flight, frame time = gap between completions
            progress::update(&self.progress, |p| p.detail = "measuring frame times".into());
            let mut ft: Vec<f64> = Vec::new();
            let mut in_flight: std::collections::VecDeque<wgpu::SubmissionIndex> = Default::default();
            let mut last_done: Option<Instant> = None;
            let end = Instant::now() + std::time::Duration::from_secs_f64(self.cfg.duration_s);
            while Instant::now() < end {
                if ft.len() % 64 == 0 {
                    cancel::check(&self.cancel)?;
                }
                in_flight.push_back(frame(false));
                if in_flight.len() >= 2 {
                    let s = in_flight.pop_front().unwrap();
                    device.poll(wgpu::Maintain::WaitForSubmissionIndex(s));
                    let now = Instant::now();
                    if let Some(prev) = last_done {
                        ft.push(now.duration_since(prev).as_secs_f64() * 1000.0);
                    }
                    last_done = Some(now);
                }
            }
            while let Some(s) = in_flight.pop_front() {
                device.poll(wgpu::Maintain::WaitForSubmissionIndex(s));
            }

            // latency: one frame at a time
            progress::update(&self.progress, |p| p.detail = "measuring latency".into());
            let mut lat: Vec<f64> = Vec::new();
            let mut gpu: Vec<f64> = Vec::new();
            let end = Instant::now() + std::time::Duration::from_secs_f64(self.cfg.latency_s);
            while Instant::now() < end || lat.len() < 10 {
                cancel::check(&self.cancel)?;
                let t0 = Instant::now();
                let s = frame(queries.is_some());
                device.poll(wgpu::Maintain::WaitForSubmissionIndex(s));
                lat.push(t0.elapsed().as_secs_f64() * 1000.0);
                if let Some((_, _, read)) = &queries {
                    let slice = read.slice(..);
                    slice.map_async(wgpu::MapMode::Read, |_| {});
                    device.poll(wgpu::Maintain::Wait);
                    {
                        let data = slice.get_mapped_range();
                        let ts: &[u64] = bytemuck::cast_slice(&data);
                        if ts[1] > ts[0] {
                            gpu.push((ts[1] - ts[0]) as f64 * period_ns / 1e6);
                        }
                    }
                    read.unmap();
                }
                if lat.len() > 100_000 {
                    break;
                }
            }

            let (fps, low1, low01, avg, p50, p95, p99, max) = frame_stats(&ft);
            let mut ls = lat.clone();
            ls.sort_by(|a, b| a.total_cmp(b));
            let lat_at = |p: f64| ls.get(((ls.len() as f64 * p) as usize).min(ls.len().saturating_sub(1))).copied().unwrap_or(0.0);
            let mut gs = gpu.clone();
            gs.sort_by(|a, b| a.total_cmp(b));
            let step = (ft.len() / 5000).max(1);
            let result = Bench3dResult {
                width: w,
                height: h,
                name: name.clone(),
                detail: self.cfg.detail.label(),
                msaa,
                frames: ft.len() as u64 + 1,
                duration_s: self.cfg.duration_s,
                fps_avg: fps,
                fps_1pct_low: low1,
                fps_01pct_low: low01,
                frametime_avg_ms: avg,
                frametime_p50_ms: p50,
                frametime_p95_ms: p95,
                frametime_p99_ms: p99,
                frametime_max_ms: max,
                latency_avg_ms: if lat.is_empty() { 0.0 } else { lat.iter().sum::<f64>() / lat.len() as f64 },
                latency_p50_ms: lat_at(0.5),
                latency_p99_ms: lat_at(0.99),
                gpu_time_avg_ms: (!gpu.is_empty()).then(|| gpu.iter().sum::<f64>() / gpu.len() as f64),
                gpu_time_p99_ms: (!gs.is_empty()).then(|| gs[((gs.len() as f64 * 0.99) as usize).min(gs.len() - 1)]),
                frametimes_ms: ft.iter().step_by(step).copied().collect(),
                telemetry: match (&self.sensors, sensor_t0) {
                    (Some(s), Some(t0)) => s.record("gpu", format!("GPU 3D · {} · {}×{}", name, w, h), t0, s.now_ms()),
                    _ => Telemetry::default(),
                },
            };
            if summary.preview.is_none() {
                summary.preview = read_preview(&device, &queue, &color_tex, w, h);
            }
            if let Some(e) = gpu_error.lock().ok().and_then(|mut e| e.take()) {
                return Err(anyhow!("GPU error while rendering at {}: {}", name, e));
            }
            progress::update(&self.progress, |p| p.done = ri + 1);
            if let Some(partial) = &self.partial {
                if let Ok(mut v) = partial.lock() {
                    v.push(result.clone());
                }
            }
            summary.results.push(result);
        }
        Ok(summary)
    }
}

/// The rendered image, scaled down to at most 480 px wide (nearest pixel), as RGBA
fn read_preview(device: &wgpu::Device, queue: &wgpu::Queue, tex: &wgpu::Texture, w: u32, h: u32) -> Option<(u32, u32, Vec<u8>)> {
    let row = (w * 4).div_ceil(256) * 256; // rows are padded to 256 bytes
    let buf = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (row * h) as u64, usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
    let mut enc = device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
        tex.as_image_copy(),
        wgpu::ImageCopyBuffer { buffer: &buf, layout: wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) } },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    queue.submit(Some(enc.finish()));
    let slice = buf.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r.is_ok());
    });
    device.poll(wgpu::Maintain::Wait);
    if !rx.recv().unwrap_or(false) {
        return None;
    }
    let scale = (w as f32 / 480.0).max(1.0);
    let (pw, ph) = ((w as f32 / scale) as u32, (h as f32 / scale) as u32);
    let data = slice.get_mapped_range();
    let mut out = Vec::with_capacity((pw * ph * 4) as usize);
    for y in 0..ph {
        for x in 0..pw {
            let (sx, sy) = ((x as f32 * scale) as u32, (y as f32 * scale) as u32);
            let i = (sy * row + sx * 4) as usize;
            out.extend_from_slice(&data[i..i + 4]);
        }
    }
    drop(data);
    buf.unmap();
    Some((pw, ph, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_stats_lows() {
        // 99 frames of 5 ms and one of 50 ms: the 1 % low is that one frame (20 FPS)
        let mut ft = vec![5.0; 99];
        ft.push(50.0);
        let (fps, low1, _low01, avg, p50, _p95, _p99, max) = frame_stats(&ft);
        assert!((avg - 5.45).abs() < 1e-9 && (fps - 1000.0 / 5.45).abs() < 1e-6);
        assert!((low1 - 20.0).abs() < 1e-9);
        assert_eq!((p50, max), (5.0, 50.0));
        assert_eq!(frame_stats(&[]).0, 0.0);
    }

    #[test]
    fn scene_geometry() {
        assert_eq!(cube().len(), 36);
        assert!(cube().iter().all(|v| v.pos.iter().all(|c| c.abs() <= 1.0 + 1e-6)));
        let i = instances(4);
        assert_eq!(i.len(), 64);
        assert_eq!(i[0][..3], [-1.5, -1.5, -1.5]);
        assert_eq!(resolution_name(2560, 1440), "1440p");
        assert_eq!(resolution_name(1000, 500), "1000×500");
        // the camera looks at the origin: it lands in the middle of the image
        let m = view_proj([0.0, 0.0, 10.0], 16.0 / 9.0);
        let o = [m[3][0], m[3][1], m[3][2], m[3][3]];
        assert!((o[0] / o[3]).abs() < 1e-6 && (o[1] / o[3]).abs() < 1e-6 && o[2] / o[3] > 0.0 && o[2] / o[3] < 1.0);
    }

    /// Renders on whatever GPU (or software Vulkan) the machine has; skipped when there is none
    #[test]
    fn renders_and_measures_a_tiny_run() {
        let cfg = Bench3dConfig { resolutions: vec![(320, 180)], detail: Detail::Low, msaa: 1, warmup_s: 0.1, duration_s: 0.4, latency_s: 0.2 };
        match Bench3d::new(cfg).run() {
            Ok(s) => {
                let (pw, ph, px) = s.preview.clone().expect("a preview image");
                assert_eq!((pw, ph), (320, 180));
                // something was drawn: not every pixel is the clear colour
                let lit = px.chunks(4).filter(|p| p[0] as u32 + p[1] as u32 + p[2] as u32 > 60).count();
                assert!(lit > 100, "only {} lit pixels", lit);
                if let Ok(dir) = std::env::var("BENCH3D_PREVIEW") {
                    let _ = std::fs::write(format!("{}/preview_{}x{}.rgba", dir, pw, ph), &px);
                }
                let r = &s.results[0];
                assert!(r.frames > 2 && r.fps_avg > 0.0 && r.latency_avg_ms > 0.0, "{:?}", r);
                assert!(r.fps_1pct_low <= r.fps_avg + 1e-9);
                assert_eq!(r.name, "320×180");
            }
            Err(e) => eprintln!("no GPU here, skipped: {}", e),
        }
    }
}

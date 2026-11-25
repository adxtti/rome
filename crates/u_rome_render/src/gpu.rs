//! wgpu resources for the model viewport.
//!
//! # Design: offscreen target composited as an egui user texture
//!
//! The scene is rendered into a per-view offscreen colour texture (with its
//! own depth buffer and optional MSAA) from [`CallbackTrait::prepare`], and
//! the resolved texture is registered with egui as a native user texture and
//! drawn as an ordinary image. We do not draw into egui's paint pass because:
//!
//! - egui's pass has no depth attachment unless the host app enables
//!   `NativeOptions::depth_buffer`, and its MSAA count is fixed by the host;
//!   a pipeline drawn into it must match both, and `RenderState` does not
//!   expose them. Rendering offscreen makes the widget independent of Studio's
//!   window options.
//! - Compositing through egui's own textured-quad pipeline guarantees format
//!   and sample-count compatibility with the egui pass on every backend,
//!   including GL.
//!
//! `prepare` records into egui's command encoder, which egui submits before
//! (and in the same submission as) the pass that samples the texture, so the
//! image is always current for the frame. `paint` is intentionally empty.
//!
//! All wgpu objects for every view live in `callback_resources` as one
//! [`RenderResources`] value: shared pipelines plus a map of per-view state.
//! Mesh and texture uploads happen eagerly on the UI thread (under the
//! renderer lock) so the CPU copies need not be retained for the GPU.

use std::collections::HashMap;
use std::future::Future;
use std::task::{Context, Poll, Waker};

use bytemuck::{Pod, Zeroable};
use egui_wgpu::wgpu;
use egui_wgpu::wgpu::util::DeviceExt;
use u_rome_core::{Image, ImageFormat};

use crate::error::RenderError;
use crate::image_view::mip_data;
use crate::prep::{GpuVertex, LineVertex, PreparedMesh, WireVertex};

pub(crate) const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
pub(crate) const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth24Plus;

/// Per-frame uniforms, matching `Frame` in `scene.wgsl` (128 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub(crate) struct FrameUniform {
    pub view_proj: [[f32; 4]; 4],
    pub camera_pos: [f32; 4],
    pub light_dir: [f32; 4],
    pub wire_color: [f32; 4],
    pub params: [u32; 4],
}

/// What the device can do, detected once at runtime.
#[derive(Clone, Debug)]
pub struct Capabilities {
    pub backend: wgpu::Backend,
    pub adapter_name: String,
    /// MSAA sample count used for the viewport (1 or 4).
    pub samples: u32,
    /// `Features::POLYGON_MODE_LINE` is enabled on the device.
    pub native_wireframe: bool,
    pub max_texture_dim: u32,
    pub max_buffer_size: u64,
}

impl Capabilities {
    pub(crate) fn detect(rs: &egui_wgpu::RenderState) -> Self {
        Self::from_device(&rs.adapter, &rs.device)
    }

    pub(crate) fn from_device(adapter: &wgpu::Adapter, device: &wgpu::Device) -> Self {
        let info = adapter.get_info();
        let limits = device.limits();
        let color = adapter.get_texture_format_features(COLOR_FORMAT);
        let depth = adapter.get_texture_format_features(DEPTH_FORMAT);
        let msaa_ok = color.flags.sample_count_supported(4)
            && depth.flags.sample_count_supported(4)
            && color
                .flags
                .contains(wgpu::TextureFormatFeatureFlags::MULTISAMPLE_RESOLVE);
        let msaa_disabled = std::env::var_os("U_ROME_NO_MSAA").is_some();
        let caps = Self {
            backend: info.backend,
            adapter_name: info.name,
            samples: if msaa_ok && !msaa_disabled { 4 } else { 1 },
            native_wireframe: device
                .features()
                .contains(wgpu::Features::POLYGON_MODE_LINE),
            max_texture_dim: limits.max_texture_dimension_2d.max(1),
            max_buffer_size: limits.max_buffer_size,
        };
        log::info!(
            "render: viewport on {:?} ({}), msaa {}x, wireframe {}, max texture {}",
            caps.backend,
            caps.adapter_name,
            caps.samples,
            if caps.native_wireframe {
                "native"
            } else {
                "barycentric"
            },
            caps.max_texture_dim
        );
        caps
    }
}

/// Run `f` inside a validation error scope so a bad descriptor becomes an
/// error instead of hitting the (panicking) uncaptured error handler.
pub(crate) fn scoped<T>(
    device: &wgpu::Device,
    what: &str,
    f: impl FnOnce() -> T,
) -> Result<T, String> {
    let oom = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
    let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let value = f();
    let err = poll_now(validation.pop()).or_else(|| poll_now(oom.pop()));
    match err {
        Some(e) => {
            log::warn!("render: {what} failed: {e}");
            Err(format!("{what}: {e}"))
        }
        None => Ok(value),
    }
}

/// Error scopes resolve immediately on native backends; if one is still
/// pending we cannot block, so treat it as success.
fn poll_now<F: Future<Output = Option<wgpu::Error>>>(fut: F) -> Option<wgpu::Error> {
    let mut fut = std::pin::pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(e) => e,
        Poll::Pending => None,
    }
}

pub(crate) struct Pipelines {
    pub caps: Capabilities,
    frame_layout: wgpu::BindGroupLayout,
    material_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    default_material: wgpu::BindGroup,
    mesh: wgpu::RenderPipeline,
    wire_native: Option<wgpu::RenderPipeline>,
    wire_bary: wgpu::RenderPipeline,
    lines_depth: wgpu::RenderPipeline,
    lines_overlay: wgpu::RenderPipeline,
}

const MESH_ATTRS: [wgpu::VertexAttribute; 4] = wgpu::vertex_attr_array![
    0 => Float32x3, 1 => Float32x3, 2 => Float32x2, 3 => Unorm8x4
];
const WIRE_ATTRS: [wgpu::VertexAttribute; 2] =
    wgpu::vertex_attr_array![0 => Float32x3, 1 => Unorm8x4];

fn layout<'a>(stride: usize, attrs: &'a [wgpu::VertexAttribute]) -> wgpu::VertexBufferLayout<'a> {
    wgpu::VertexBufferLayout {
        array_stride: stride as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: attrs,
    }
}

struct PipelineSpec<'a> {
    label: &'a str,
    layout: &'a wgpu::PipelineLayout,
    vs: &'a str,
    fs: &'a str,
    buffer: wgpu::VertexBufferLayout<'a>,
    topology: wgpu::PrimitiveTopology,
    polygon_mode: wgpu::PolygonMode,
    depth_write: bool,
    depth_compare: wgpu::CompareFunction,
    blend: bool,
}

impl Pipelines {
    pub(crate) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        caps: Capabilities,
    ) -> Result<Self, String> {
        scoped(device, "creating viewport pipelines", || {
            Self::create(device, queue, caps)
        })
    }

    fn create(device: &wgpu::Device, queue: &wgpu::Queue, caps: Capabilities) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("u_rome_render scene"),
            source: wgpu::ShaderSource::Wgsl(include_str!("scene.wgsl").into()),
        });
        let frame_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("u_rome_render frame"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(
                        std::mem::size_of::<FrameUniform>() as u64
                    ),
                },
                count: None,
            }],
        });
        let material_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("u_rome_render material"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let mesh_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("u_rome_render mesh layout"),
            bind_group_layouts: &[Some(&frame_layout), Some(&material_layout)],
            immediate_size: 0,
        });
        let frame_only_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("u_rome_render frame layout"),
            bind_group_layouts: &[Some(&frame_layout)],
            immediate_size: 0,
        });

        let samples = caps.samples;
        let make = |s: PipelineSpec| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(s.label),
                layout: Some(s.layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(s.vs),
                    compilation_options: Default::default(),
                    buffers: &[Some(s.buffer)],
                },
                primitive: wgpu::PrimitiveState {
                    topology: s.topology,
                    // Decoded winding is not trusted; draw both faces.
                    cull_mode: None,
                    polygon_mode: s.polygon_mode,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(s.depth_write),
                    depth_compare: Some(s.depth_compare),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: wgpu::MultisampleState {
                    count: samples,
                    ..Default::default()
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(s.fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: COLOR_FORMAT,
                        blend: s.blend.then_some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let mesh_buf = layout(std::mem::size_of::<GpuVertex>(), &MESH_ATTRS);
        let wire_buf = layout(std::mem::size_of::<WireVertex>(), &WIRE_ATTRS);
        let line_buf = layout(std::mem::size_of::<LineVertex>(), &WIRE_ATTRS);

        let mesh = make(PipelineSpec {
            label: "u_rome_render mesh",
            layout: &mesh_layout,
            vs: "vs_mesh",
            fs: "fs_mesh",
            buffer: mesh_buf.clone(),
            topology: wgpu::PrimitiveTopology::TriangleList,
            polygon_mode: wgpu::PolygonMode::Fill,
            depth_write: true,
            depth_compare: wgpu::CompareFunction::Less,
            blend: false,
        });
        let wire_native = caps.native_wireframe.then(|| {
            make(PipelineSpec {
                label: "u_rome_render wire native",
                layout: &mesh_layout,
                vs: "vs_mesh",
                fs: "fs_wire_native",
                buffer: mesh_buf.clone(),
                topology: wgpu::PrimitiveTopology::TriangleList,
                polygon_mode: wgpu::PolygonMode::Line,
                depth_write: true,
                depth_compare: wgpu::CompareFunction::LessEqual,
                blend: false,
            })
        });
        let wire_bary = make(PipelineSpec {
            label: "u_rome_render wire barycentric",
            layout: &frame_only_layout,
            vs: "vs_wire",
            fs: "fs_wire",
            buffer: wire_buf,
            topology: wgpu::PrimitiveTopology::TriangleList,
            polygon_mode: wgpu::PolygonMode::Fill,
            depth_write: true,
            depth_compare: wgpu::CompareFunction::LessEqual,
            blend: true,
        });
        let lines_depth = make(PipelineSpec {
            label: "u_rome_render lines",
            layout: &frame_only_layout,
            vs: "vs_line",
            fs: "fs_line",
            buffer: line_buf.clone(),
            topology: wgpu::PrimitiveTopology::LineList,
            polygon_mode: wgpu::PolygonMode::Fill,
            depth_write: false,
            depth_compare: wgpu::CompareFunction::LessEqual,
            blend: true,
        });
        let lines_overlay = make(PipelineSpec {
            label: "u_rome_render overlay lines",
            layout: &frame_only_layout,
            vs: "vs_line",
            fs: "fs_line",
            buffer: line_buf,
            topology: wgpu::PrimitiveTopology::LineList,
            polygon_mode: wgpu::PolygonMode::Fill,
            depth_write: false,
            depth_compare: wgpu::CompareFunction::Always,
            blend: true,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("u_rome_render material sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        // Untextured meshes render light grey.
        let default_tex = device.create_texture_with_data(
            queue,
            &wgpu::TextureDescriptor {
                label: Some("u_rome_render default material"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &[200, 200, 200, 255],
        );
        let default_material = material_bind_group(
            device,
            &material_layout,
            &sampler,
            &default_tex.create_view(&Default::default()),
        );
        Self {
            caps,
            frame_layout,
            material_layout,
            sampler,
            default_material,
            mesh,
            wire_native,
            wire_bary,
            lines_depth,
            lines_overlay,
        }
    }
}

fn material_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    view: &wgpu::TextureView,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("u_rome_render material"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

/// Choose which mip levels of `image` to upload: skip leading levels larger
/// than `max_dim`, then take consecutive valid levels. Returns
/// `(first level, level count)`.
pub(crate) fn select_mips(image: &Image, max_dim: u32) -> Result<(usize, usize), RenderError> {
    // Mip 0 must be valid for the image to be usable at all.
    mip_data(image, 0)?;
    let first = (0..image.mips.len())
        .find(|&l| {
            let (w, h) = crate::image_view::mip_size(image.width, image.height, l);
            w <= max_dim && h <= max_dim
        })
        .ok_or(RenderError::TextureTooLarge {
            width: image.width,
            height: image.height,
            limit: max_dim,
        })?;
    let count = (first..image.mips.len())
        .take_while(|&l| mip_data(image, l).is_ok())
        .count();
    if count == 0 {
        // The first level that fits is itself invalid.
        mip_data(image, first)?;
    }
    Ok((first, count.max(1)))
}

pub(crate) struct GpuMesh {
    vertex: wgpu::Buffer,
    index: wgpu::Buffer,
    index_count: u32,
    pub material: Option<usize>,
    /// Barycentric wireframe buffer: `None` until attempted, `Some(None)`
    /// when expansion was skipped (too large or failed).
    wire: Option<Option<(wgpu::Buffer, u32)>>,
}

impl GpuMesh {
    pub(crate) fn upload(
        device: &wgpu::Device,
        caps: &Capabilities,
        mesh: &PreparedMesh,
        label: &str,
    ) -> Option<Self> {
        if mesh.indices.is_empty() || mesh.vertices.is_empty() {
            return None;
        }
        let vbytes: &[u8] = bytemuck::cast_slice(&mesh.vertices);
        let ibytes: &[u8] = bytemuck::cast_slice(&mesh.indices);
        let too_big = |len: usize| len as u64 > caps.max_buffer_size;
        if too_big(vbytes.len()) || too_big(ibytes.len()) {
            log::warn!(
                "render: {label}: buffer of {} / {} bytes exceeds device max_buffer_size {}, skipped",
                vbytes.len(),
                ibytes.len(),
                caps.max_buffer_size
            );
            return None;
        }
        let index_count = u32::try_from(mesh.indices.len()).ok()?;
        scoped(device, label, || Self {
            vertex: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("u_rome_render vertices"),
                contents: vbytes,
                usage: wgpu::BufferUsages::VERTEX,
            }),
            index: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("u_rome_render indices"),
                contents: ibytes,
                usage: wgpu::BufferUsages::INDEX,
            }),
            index_count,
            material: mesh.material,
            wire: None,
        })
        .ok()
    }

    pub(crate) fn wire_attempted(&self) -> bool {
        self.wire.is_some()
    }

    /// Upload expanded wireframe vertices; an empty slice marks the mesh as
    /// attempted without a buffer.
    pub(crate) fn set_wire(
        &mut self,
        device: &wgpu::Device,
        caps: &Capabilities,
        verts: &[WireVertex],
    ) {
        self.wire = Some(None);
        let bytes: &[u8] = bytemuck::cast_slice(verts);
        let Ok(count) = u32::try_from(verts.len()) else {
            return;
        };
        if bytes.is_empty() || bytes.len() as u64 > caps.max_buffer_size {
            return;
        }
        if let Ok(buf) = scoped(device, "wireframe upload", || {
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("u_rome_render wire vertices"),
                contents: bytes,
                usage: wgpu::BufferUsages::VERTEX,
            })
        }) {
            self.wire = Some(Some((buf, count)));
        }
    }
}

pub(crate) struct Targets {
    #[cfg_attr(not(test), allow(dead_code))]
    pub color: wgpu::Texture,
    pub color_view: wgpu::TextureView,
    msaa_view: Option<wgpu::TextureView>,
    depth_view: wgpu::TextureView,
    pub size: [u32; 2],
}

impl Targets {
    pub(crate) fn new(device: &wgpu::Device, size: [u32; 2], samples: u32) -> Result<Self, String> {
        let extent = wgpu::Extent3d {
            width: size[0].max(1),
            height: size[1].max(1),
            depth_or_array_layers: 1,
        };
        let tex = |label, format, samples, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: extent,
                mip_level_count: 1,
                sample_count: samples,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        scoped(device, "creating viewport targets", || {
            let color = tex(
                "u_rome_render viewport colour",
                COLOR_FORMAT,
                1,
                wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
            );
            Self {
                color_view: color.create_view(&Default::default()),
                color,
                msaa_view: (samples > 1).then(|| {
                    tex(
                        "u_rome_render viewport msaa",
                        COLOR_FORMAT,
                        samples,
                        wgpu::TextureUsages::RENDER_ATTACHMENT,
                    )
                    .create_view(&Default::default())
                }),
                depth_view: tex(
                    "u_rome_render viewport depth",
                    DEPTH_FORMAT,
                    samples,
                    wgpu::TextureUsages::RENDER_ATTACHMENT,
                )
                .create_view(&Default::default()),
                size,
            }
        })
    }
}

pub(crate) struct ViewGpu {
    uniform: wgpu::Buffer,
    frame_bind_group: wgpu::BindGroup,
    lines: Option<wgpu::Buffer>,
    pub targets: Option<Targets>,
    /// `[lod][mesh]`.
    pub meshes: Vec<Vec<Option<GpuMesh>>>,
    materials: HashMap<usize, wgpu::BindGroup>,
}

/// Everything the viewport keeps in `egui_wgpu::CallbackResources`.
pub(crate) struct RenderResources {
    pub pipelines: Pipelines,
    pub views: HashMap<u64, ViewGpu>,
}

impl RenderResources {
    pub(crate) fn add_view(&mut self, device: &wgpu::Device, id: u64) -> Result<(), String> {
        let p = &self.pipelines;
        let view = scoped(device, "creating view uniforms", || {
            let uniform = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("u_rome_render frame uniform"),
                size: std::mem::size_of::<FrameUniform>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let frame_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("u_rome_render frame"),
                layout: &p.frame_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                }],
            });
            ViewGpu {
                uniform,
                frame_bind_group,
                lines: None,
                targets: None,
                meshes: Vec::new(),
                materials: HashMap::new(),
            }
        })?;
        self.views.insert(id, view);
        Ok(())
    }

    pub(crate) fn upload_material(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view_id: u64,
        material: usize,
        image: &Image,
    ) -> Result<(), RenderError> {
        let p = &self.pipelines;
        let (first, count) = select_mips(image, p.caps.max_texture_dim)?;
        let (w, h) = crate::image_view::mip_size(image.width, image.height, first);
        let format = match image.format {
            ImageFormat::Rgba8Srgb => wgpu::TextureFormat::Rgba8UnormSrgb,
            ImageFormat::Rgba8 => wgpu::TextureFormat::Rgba8Unorm,
        };
        if first > 0 {
            log::warn!(
                "render: material {material}: {}x{} exceeds device limit {}, using mip {first}",
                image.width,
                image.height,
                p.caps.max_texture_dim
            );
        }
        let bind_group = scoped(device, "material texture upload", || {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("u_rome_render material texture"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: count as u32,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            for (i, level) in (first..first + count).enumerate() {
                let Ok((data, lw, lh)) = mip_data(image, level) else {
                    continue;
                };
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &texture,
                        mip_level: i as u32,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(lw * 4),
                        rows_per_image: Some(lh),
                    },
                    wgpu::Extent3d {
                        width: lw,
                        height: lh,
                        depth_or_array_layers: 1,
                    },
                );
            }
            material_bind_group(
                device,
                &p.material_layout,
                &p.sampler,
                &texture.create_view(&Default::default()),
            )
        })
        .map_err(|_| RenderError::TextureTooLarge {
            width: w,
            height: h,
            limit: p.caps.max_texture_dim,
        })?;
        if let Some(view) = self.views.get_mut(&view_id) {
            view.materials.insert(material, bind_group);
        }
        Ok(())
    }

    pub(crate) fn clear_model(&mut self, view_id: u64) {
        if let Some(v) = self.views.get_mut(&view_id) {
            v.meshes.clear();
            v.materials.clear();
        }
    }
}

/// One frame of viewport drawing, recorded in `prepare`.
pub(crate) struct SceneCallback {
    pub view_id: u64,
    pub uniform: FrameUniform,
    pub lines: Vec<LineVertex>,
    /// Lines from this vertex on are drawn without depth testing.
    pub overlay_start: u32,
    /// `(lod, mesh)` pairs to draw.
    pub draws: Vec<(usize, usize)>,
    pub wireframe: bool,
    pub clear: wgpu::Color,
    pub viewport: [u32; 2],
}

impl egui_wgpu::CallbackTrait for SceneCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(res) = resources.get_mut::<RenderResources>() else {
            return Vec::new();
        };
        let RenderResources { pipelines, views } = res;
        let Some(view) = views.get_mut(&self.view_id) else {
            return Vec::new();
        };
        queue.write_buffer(&view.uniform, 0, bytemuck::bytes_of(&self.uniform));

        let line_bytes: &[u8] = bytemuck::cast_slice(&self.lines);
        let need = line_bytes.len() as u64;
        if need > 0 && view.lines.as_ref().is_none_or(|b| b.size() < need) {
            let size = need.next_power_of_two().max(4096);
            if size <= pipelines.caps.max_buffer_size {
                view.lines = scoped(device, "line buffer", || {
                    device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("u_rome_render lines"),
                        size,
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    })
                })
                .ok();
            }
        }
        let line_count = match &view.lines {
            Some(buf) if need > 0 && buf.size() >= need => {
                queue.write_buffer(buf, 0, line_bytes);
                self.lines.len() as u32
            }
            _ => 0,
        };

        let ViewGpu {
            frame_bind_group,
            lines,
            targets,
            meshes,
            materials,
            ..
        } = view;
        let Some(t) = targets.as_ref() else {
            return Vec::new();
        };
        let (color, resolve) = match &t.msaa_view {
            Some(msaa) => (msaa, Some(&t.color_view)),
            None => (&t.color_view, None),
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("u_rome_render viewport"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: color,
                depth_slice: None,
                resolve_target: resolve,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(self.clear),
                    store: if resolve.is_some() {
                        wgpu::StoreOp::Discard
                    } else {
                        wgpu::StoreOp::Store
                    },
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &t.depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Discard,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let w = self.viewport[0].clamp(1, t.size[0].max(1));
        let h = self.viewport[1].clamp(1, t.size[1].max(1));
        pass.set_viewport(0.0, 0.0, w as f32, h as f32, 0.0, 1.0);
        pass.set_scissor_rect(0, 0, w, h);
        pass.set_bind_group(0, &*frame_bind_group, &[]);

        let mesh_at = |(l, m): (usize, usize)| {
            meshes
                .get(l)
                .and_then(|lod| lod.get(m))
                .and_then(Option::as_ref)
        };
        match (self.wireframe, &pipelines.wire_native) {
            (true, Some(native)) => {
                pass.set_pipeline(native);
                pass.set_bind_group(1, &pipelines.default_material, &[]);
                for gm in self.draws.iter().filter_map(|&d| mesh_at(d)) {
                    pass.set_vertex_buffer(0, gm.vertex.slice(..));
                    pass.set_index_buffer(gm.index.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..gm.index_count, 0, 0..1);
                }
            }
            (true, None) => {
                pass.set_pipeline(&pipelines.wire_bary);
                for gm in self.draws.iter().filter_map(|&d| mesh_at(d)) {
                    if let Some(Some((buf, count))) = &gm.wire {
                        pass.set_vertex_buffer(0, buf.slice(..));
                        pass.draw(0..*count, 0..1);
                    }
                }
            }
            (false, _) => {
                pass.set_pipeline(&pipelines.mesh);
                for gm in self.draws.iter().filter_map(|&d| mesh_at(d)) {
                    let mat = gm
                        .material
                        .and_then(|m| materials.get(&m))
                        .unwrap_or(&pipelines.default_material);
                    pass.set_bind_group(1, mat, &[]);
                    pass.set_vertex_buffer(0, gm.vertex.slice(..));
                    pass.set_index_buffer(gm.index.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..gm.index_count, 0, 0..1);
                }
            }
        }

        if let Some(buf) = lines.as_ref().filter(|_| line_count > 0) {
            let split = self.overlay_start.min(line_count);
            pass.set_vertex_buffer(0, buf.slice(..));
            if split > 0 {
                pass.set_pipeline(&pipelines.lines_depth);
                pass.draw(0..split, 0..1);
            }
            if split < line_count {
                pass.set_pipeline(&pipelines.lines_overlay);
                pass.draw(split..line_count, 0..1);
            }
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        _render_pass: &mut wgpu::RenderPass<'static>,
        _resources: &egui_wgpu::CallbackResources,
    ) {
        // Composited as an egui user texture instead; see the module docs.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(w: u32, h: u32, levels: usize) -> Image {
        Image {
            width: w,
            height: h,
            format: ImageFormat::Rgba8,
            mips: (0..levels)
                .map(|l| {
                    let (mw, mh) = crate::image_view::mip_size(w, h, l);
                    vec![0; (mw * mh * 4) as usize]
                })
                .collect(),
            provisional: false,
        }
    }

    #[test]
    fn uniform_layout_matches_shader() {
        assert_eq!(std::mem::size_of::<FrameUniform>(), 128);
    }

    #[test]
    fn mip_selection() {
        assert_eq!(select_mips(&image(64, 32, 7), 8192).unwrap(), (0, 7));
        assert_eq!(select_mips(&image(64, 32, 7), 16).unwrap(), (2, 5));
        assert!(matches!(
            select_mips(&image(64, 32, 1), 16),
            Err(RenderError::TextureTooLarge { .. })
        ));
        let mut broken = image(64, 32, 4);
        broken.mips[2].pop();
        assert_eq!(select_mips(&broken, 8192).unwrap(), (0, 2));
        let mut bad0 = image(8, 8, 1);
        bad0.mips[0].clear();
        assert!(select_mips(&bad0, 8192).is_err());
    }
}

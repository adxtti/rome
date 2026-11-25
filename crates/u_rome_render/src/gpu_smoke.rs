//! Real-GPU smoke test: renders a quad offscreen through the same `prepare`
//! path egui uses and reads pixels back. The device is created with
//! WebGL2-class downlevel limits and without `POLYGON_MODE_LINE`, to catch
//! anything that would fail on the GL backend. Ignored by default because CI
//! machines may lack a GPU: `cargo test -p u_rome_render -- --ignored`.

use std::future::Future;
use std::task::{Context, Poll, Waker};

use egui_wgpu::CallbackTrait;
use egui_wgpu::wgpu;
use glam::Vec3;
use u_rome_core::{Image, ImageFormat};

use crate::camera::{Aabb, OrbitCamera};
use crate::gpu::{
    Capabilities, FrameUniform, GpuMesh, Pipelines, RenderResources, SceneCallback, Targets,
};
use crate::prep::{expand_wireframe, grid_lines, prepare_mesh};

fn block<F: Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..1_000_000 {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::yield_now();
    }
    panic!("future did not resolve");
}

const SIZE: u32 = 64;
const CLEAR: [u8; 4] = [10, 20, 30, 255];

fn render(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    res: &mut egui_wgpu::CallbackResources,
    cb: &SceneCallback,
) -> Vec<u8> {
    let mut encoder = device.create_command_encoder(&Default::default());
    let extra = cb.prepare(
        device,
        queue,
        &egui_wgpu::ScreenDescriptor {
            size_in_pixels: [SIZE, SIZE],
            pixels_per_point: 1.0,
        },
        &mut encoder,
        res,
    );
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(SIZE * SIZE * 4),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let r = res.get::<RenderResources>().expect("resources");
    let t = r
        .views
        .get(&1)
        .and_then(|v| v.targets.as_ref())
        .expect("targets");
    encoder.copy_texture_to_buffer(
        t.color.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(SIZE * 4),
                rows_per_image: Some(SIZE),
            },
        },
        wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(extra.into_iter().chain([encoder.finish()]));
    readback.map_async(wgpu::MapMode::Read, .., |r| r.expect("map"));
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let data = readback.get_mapped_range(..).expect("mapped").to_vec();
    readback.unmap();
    data
}

fn px(data: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * SIZE + x) * 4) as usize;
    [data[i], data[i + 1], data[i + 2], data[i + 3]]
}

fn is_clear(p: [u8; 4]) -> bool {
    p.iter().zip(CLEAR).all(|(a, b)| a.abs_diff(b) <= 2)
}

#[test]
#[ignore = "needs a GPU adapter"]
fn renders_lit_and_wireframe_under_downlevel_limits() {
    let instance = wgpu::Instance::default();
    let Ok(adapter) = block(instance.request_adapter(&Default::default())) else {
        eprintln!("no GPU adapter, skipping");
        return;
    };
    let (device, queue) = block(adapter.request_device(&wgpu::DeviceDescriptor {
        required_limits:
            wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits()),
        ..Default::default()
    }))
    .expect("device");
    let caps = Capabilities::from_device(&adapter, &device);
    assert!(!caps.native_wireframe, "feature was not requested");
    let mut res = RenderResources {
        pipelines: Pipelines::new(&device, &queue, caps.clone()).expect("pipelines"),
        views: Default::default(),
    };
    res.add_view(&device, 1).expect("view");

    // A 2 x 2 quad in the Z = 0 plane, textured red.
    let mesh = u_rome_core::Mesh {
        positions: vec![
            [-1.0, -1.0, 0.0],
            [1.0, -1.0, 0.0],
            [1.0, 1.0, 0.0],
            [-1.0, 1.0, 0.0],
        ],
        uvs: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
        indices: vec![0, 1, 2, 0, 2, 3],
        material: Some(0),
        ..Default::default()
    };
    let prepared = prepare_mesh(&mesh, "smoke");
    let mut gm = GpuMesh::upload(&device, &caps, &prepared, "smoke").expect("upload");
    let positions: Vec<_> = prepared.vertices.iter().map(|v| v.position).collect();
    let wire = expand_wireframe(&positions, &prepared.indices).expect("wire");
    gm.set_wire(&device, &caps, &wire);
    let red = Image {
        width: 2,
        height: 2,
        format: ImageFormat::Rgba8Srgb,
        mips: vec![[255, 0, 0, 255].repeat(4), vec![255, 0, 0, 255]],
        provisional: false,
    };
    res.upload_material(&device, &queue, 1, 0, &red)
        .expect("material");
    let view = res.views.get_mut(&1).expect("view");
    view.meshes = vec![vec![Some(gm)]];
    view.targets = Some(Targets::new(&device, [SIZE, SIZE], caps.samples).expect("targets"));
    let mut cbres = egui_wgpu::CallbackResources::new();
    cbres.insert(res);

    // Look straight down at the quad.
    let mut cam = OrbitCamera {
        pitch: 89f32.to_radians(),
        ..Default::default()
    };
    cam.frame_bounds(
        Aabb {
            min: Vec3::new(-1.0, -1.0, 0.0),
            max: Vec3::new(1.0, 1.0, 0.0),
        },
        1.0,
    );
    let mut cb = SceneCallback {
        view_id: 1,
        uniform: FrameUniform {
            view_proj: cam.view_proj(1.0, 1.5).to_cols_array_2d(),
            camera_pos: cam.eye().extend(1.0).to_array(),
            light_dir: [0.0, 0.0, 1.0, 0.0],
            wire_color: [1.0, 1.0, 1.0, 1.0],
            params: [0, 0, 0, 0],
        },
        lines: grid_lines(1.0),
        overlay_start: 0,
        draws: vec![(0, 0)],
        wireframe: false,
        clear: wgpu::Color {
            r: f64::from(CLEAR[0]) / 255.0,
            g: f64::from(CLEAR[1]) / 255.0,
            b: f64::from(CLEAR[2]) / 255.0,
            a: 1.0,
        },
        viewport: [SIZE, SIZE],
    };
    let lit = render(&device, &queue, &mut cbres, &cb);
    let c = px(&lit, SIZE / 2 + 3, SIZE / 2 + 3);
    assert!(
        // Red albedo plus a small white specular highlight.
        c[0] > 150 && c[0] > c[1] + 50 && c[0] > c[2] + 50,
        "lit red texture, got {c:?}"
    );
    assert!(is_clear(px(&lit, 0, 0)), "corner {:?}", px(&lit, 0, 0));

    cb.wireframe = true;
    cb.lines.clear();
    let wire = render(&device, &queue, &mut cbres, &cb);
    let bright = wire
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] > 200 && p[1] > 200)
        .count();
    assert!(bright > 20, "wire pixels: {bright}");
    // The interior of a triangle stays at the clear colour.
    let inner = px(&wire, SIZE / 2 + 10, SIZE / 2 - 4);
    assert!(is_clear(inner), "inner {inner:?}");
}

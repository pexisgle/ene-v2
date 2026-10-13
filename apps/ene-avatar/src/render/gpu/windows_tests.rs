use super::*;
use std::{sync::Arc, time::Duration};
#[test]
#[ignore = "requires a DX12 graphics session; run explicitly for hardware verification"]
fn dx12_composition_presents_gpu_mask_on_the_same_device() {
    use bevy::render::renderer::{RenderDevice, RenderQueue, WgpuWrapper};
    let mut state = crate::wayland::State::for_test().unwrap();
    let window = Arc::new(state.native_window());
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::DX12,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter =
        bevy::tasks::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .unwrap();
    let (device, queue) =
        bevy::tasks::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();
    let mut frames = GpuFrames::new(
        RenderDevice::from(device.clone()),
        RenderQueue(Arc::new(WgpuWrapper::new(queue.clone()))),
    );
    let mut presenter =
        surface::GpuSurface::new(&instance, &adapter, &device, &queue, window).unwrap();
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("synthetic transparent avatar"),
        size: wgpu::Extent3d {
            width: 8,
            height: 4,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let mut pixels = vec![0; 8 * 4 * 4];
    for y in 0..4 {
        for x in [1, 2, 5, 6] {
            let p = (y * 8 + x) * 4;
            pixels[p] = 255;
            pixels[p + 3] = 255;
        }
    }
    queue.write_texture(
        texture.as_image_copy(),
        &pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(32),
            rows_per_image: Some(4),
        },
        texture.size(),
    );
    assert!(frames.enqueue(&texture, 8, 4, 1, 1, 1));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let frame = loop {
        if let Some(frame) = frames.poll().into_iter().next() {
            break frame;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "GPU mask did not complete"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(frame.runs.len(), 8);
    assert!(
        frame
            .runs
            .iter()
            .all(|r| r.width == 2 && [1, 5].contains(&r.x))
    );
    let output = presenter
        .draw(&frame.texture, 8, 4)
        .unwrap()
        .expect("composition must acquire a frame");
    state.prepare_gpu_present(&frame.runs);
    output.present();
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    presenter.commit().unwrap();
}

#[test]
#[ignore = "requires DX12 and a VRM 1.0 file in ENE_AVATAR_TEST_MODEL"]
fn vrm_transparent_pixels_pass_input_through_the_composition_window() {
    use bevy::render::renderer::{RenderAdapter, RenderDevice, RenderInstance, RenderQueue};
    let model = std::env::var("ENE_AVATAR_TEST_MODEL")
        .expect("set ENE_AVATAR_TEST_MODEL to a VRM 1.0 file");
    let mut state = crate::wayland::State::for_test().unwrap();
    state.settings.model = model;
    state.show_for_test();
    let mut renderer = crate::render::renderer::Renderer::new(
        state.width,
        state.height,
        state.scale,
        &state.settings,
        state.native_window(),
        state.id,
        &state.output,
    )
    .unwrap();
    let world = renderer.app.world();
    let device = world.resource::<RenderDevice>().clone();
    let queue = world.resource::<RenderQueue>().clone();
    let mut frames = GpuFrames::new(device.clone(), queue.clone());
    let mut presenter = {
        let world = renderer.app.sub_app(bevy::render::RenderApp).world();
        surface::GpuSurface::new(
            world.resource::<RenderInstance>(),
            world.resource::<RenderAdapter>(),
            device.wgpu_device(),
            &queue,
            Arc::new(state.native_window()),
        )
        .unwrap()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    let mut sequence = 0;
    let frame = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "VRM GPU frame did not complete"
        );
        state.dispatch(Duration::from_millis(2));
        renderer.app.update();
        assert!(renderer.load_error().is_none(), "VRM load failed");
        if renderer.ready(state.avatar_height(), state.height)
            && let Some(texture) = renderer.rendered_texture()
        {
            sequence += 1;
            frames.enqueue(
                &texture,
                state.width,
                state.height,
                state.scale,
                renderer.generation,
                sequence,
            );
        }
        if let Some(frame) = frames.poll().into_iter().find(|frame| {
            frame.identity.generation == renderer.generation && !frame.runs.is_empty()
        }) {
            break frame;
        }
        assert!(frames.take_errors().is_empty(), "GPU mask error");
    };
    let output = presenter
        .draw(&frame.texture, state.width, state.height)
        .unwrap()
        .expect("composition must acquire a frame");
    state.prepare_gpu_present(&frame.runs);
    output.present();
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    presenter.commit().unwrap();
    let covered: Vec<_> = frame
        .runs
        .iter()
        .step_by((frame.runs.len() / 20).max(1))
        .map(|run| (run.x + run.width / 2, run.y))
        .collect();
    let mut transparent = Vec::new();
    for y in (0..state.height as i32).step_by(20) {
        for x in (0..state.width as i32).step_by(20) {
            if !frame
                .runs
                .iter()
                .any(|run| run.y == y && x >= run.x && x < run.x + run.width)
            {
                transparent.push((x, y));
            }
        }
    }
    assert!(!covered.is_empty());
    assert!(!transparent.is_empty());
    state.assert_input_points_for_test(&covered, &transparent);
    eprintln!(
        "VRM hit test: {} covered points and {} transparent points passed",
        covered.len(),
        transparent.len()
    );
}

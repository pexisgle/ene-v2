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

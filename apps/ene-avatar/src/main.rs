use bevy::prelude::*;
use bevy::render::settings::{MemoryHints, WgpuSettings};
use bevy::render::{Render, RenderApp, RenderPlugin, settings::RenderCreation};
use ene_vrm::VrmPlugin;

mod character;
mod control;
mod desktop;
mod platform;
mod rendering;

fn main() -> bevy::app::AppExit {
    // DXGI HWND swapchains do not support desktop alpha composition.
    #[cfg(windows)]
    unsafe {
        std::env::set_var("WGPU_DX12_PRESENTATION_SYSTEM", "Visual");
    }
    let mut app = App::new();
    app.insert_resource(ClearColor(Color::NONE));
    app.add_plugins(
        DefaultPlugins
            .set(WindowPlugin {
                primary_window: None,
                exit_condition: bevy::window::ExitCondition::DontExit,
                ..default()
            })
            .set(AssetPlugin {
                file_path: ".".into(),
                unapproved_path_mode: bevy::asset::UnapprovedPathMode::Allow,
                ..default()
            })
            .set(RenderPlugin {
                render_creation: RenderCreation::Automatic(Box::new(WgpuSettings {
                    memory_hints: MemoryHints::MemoryUsage,
                    #[cfg(windows)]
                    backends: Some(bevy::render::settings::Backends::DX12),
                    ..default()
                })),
                ..default()
            }),
    );
    app.add_plugins((
        VrmPlugin,
        control::setup,
        character::setup,
        desktop::setup,
        rendering::setup,
    ));
    #[cfg(windows)]
    app.add_plugins(platform::windows::setup);
    #[cfg(target_os = "linux")]
    app.add_plugins(platform::wayland::setup);
    // Avoid dispatch overhead between the dependency-ordered VRM systems.
    app.edit_schedule(PostUpdate, |schedule| {
        schedule.set_executor(bevy::ecs::schedule::SingleThreadedExecutor::new());
    });
    // Small avatar scenes spend more CPU dispatching render systems than they
    // save by running them concurrently. Pipelined rendering and parallel work
    // inside individual systems still use their own threads.
    app.sub_app_mut(RenderApp)
        .edit_schedule(Render, |schedule| {
            schedule.set_executor(bevy::ecs::schedule::SingleThreadedExecutor::new());
        });
    control::run(app)
}

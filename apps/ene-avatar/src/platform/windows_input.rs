//! Replay native pointer events before consulting the current cursor position.

use crate::desktop::{Output, PointerInput};
use bevy::{input::ButtonState, prelude::*, window::WindowEvent};
use std::collections::HashMap;

pub(super) fn input(
    In(current): In<Option<Vec2>>,
    mut events: MessageReader<WindowEvent>,
    outputs: Query<&Output>,
    mut positions: Local<HashMap<Entity, Vec2>>,
    mut pointer: MessageWriter<PointerInput>,
) {
    positions.retain(|entity, _| outputs.contains(*entity));
    for event in events.read() {
        let (window, button) = match event {
            WindowEvent::CursorMoved(event) => {
                positions.insert(event.window, event.position);
                (event.window, None)
            }
            WindowEvent::MouseButtonInput(event) if event.button == MouseButton::Left => {
                (event.window, Some(event.state == ButtonState::Pressed))
            }
            _ => continue,
        };
        let Ok(output) = outputs.get(window) else {
            continue;
        };
        let local = match positions.get(&window) {
            Some(&position) => position,
            None => {
                // A new window may not have sent a motion event yet.
                let Some(point) = current else { continue };
                (point - output.origin) / output.units_per_logical_pixel
            }
        };
        pointer.write(PointerInput {
            output: window,
            local,
            button,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        character::{AvatarState, Character, Notification},
        desktop::{self, Drag, HitRegions},
    };
    use bevy::{input::mouse::MouseButtonInput, window::CursorMoved};

    fn test_app() -> (App, [Entity; 2], Entity) {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .init_resource::<Drag>()
            .add_message::<WindowEvent>()
            .add_message::<PointerInput>()
            .add_message::<Notification>()
            .add_systems(
                Update,
                (
                    (|| Some(Vec2::new(200., 50.))).pipe(input),
                    desktop::pointer,
                )
                    .chain(),
            );
        let mut regions = Vec::new();
        for (name, x) in [("a", 50.), ("b", 200.)] {
            let mut character = Character::new(name.into(), default());
            character.state = AvatarState::Ready;
            let avatar = app
                .world_mut()
                .spawn((character, Transform::from_xyz(x, -50., 0.)))
                .id();
            regions.push((
                avatar,
                Rect::from_center_size(Vec2::new(x, 50.), Vec2::splat(40.)),
            ));
        }
        let avatar = regions[0].0;
        let output = app
            .world_mut()
            .spawn((
                Output {
                    name: "left".into(),
                    origin: Vec2::ZERO,
                    size: Vec2::new(300., 200.),
                    units_per_logical_pixel: 1.,
                },
                HitRegions(regions),
            ))
            .id();
        let right = app
            .world_mut()
            .spawn(Output {
                name: "right".into(),
                origin: Vec2::new(300., 0.),
                size: Vec2::new(300., 200.),
                units_per_logical_pixel: 2.,
            })
            .id();
        (app, [output, right], avatar)
    }

    fn moved(window: Entity, position: Vec2) -> WindowEvent {
        CursorMoved {
            window,
            position,
            delta: None,
        }
        .into()
    }

    fn button(window: Entity, state: ButtonState) -> WindowEvent {
        MouseButtonInput {
            window,
            state,
            button: MouseButton::Left,
        }
        .into()
    }

    #[test]
    fn queued_drag_keeps_the_pressed_character_across_monitors() {
        let (mut app, [output, _], avatar) = test_app();
        for event in [
            moved(output, Vec2::new(50., 50.)),
            button(output, ButtonState::Pressed),
            moved(output, Vec2::new(400., 100.)),
            button(output, ButtonState::Released),
        ] {
            app.world_mut().write_message(event);
        }
        app.update();
        let config = &app.world().get::<Character>(avatar).unwrap().config;
        assert_eq!(config.monitor.as_deref(), Some("right"));
        assert_eq!(config.position, Some([50., 50.]));
        assert!(app.world().resource::<Drag>().entity.is_none());
        assert!(
            app.world_mut()
                .resource_mut::<Messages<Notification>>()
                .drain()
                .next()
                .is_none(),
            "a drag must not become a click"
        );
    }
}

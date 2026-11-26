//! Ordered native input through Bevy picking over the desktop hit regions.

use super::{HitRegions, Output, bounds, desktop_position};
use crate::character::{AvatarState, Character, Notification};
use bevy::{
    camera::NormalizedRenderTarget,
    ecs::schedule::ScheduleLabel,
    picking::{
        InteractionPlugin, PickingPlugin, PickingSettings, PickingSystems,
        backend::{HitData, PointerHits},
        events::{PointerClick, PointerDragEnd, PointerPress, PointerState, pointer_events},
        hover::{PointerCaptureMap, generate_hovermap, update_interactions},
        pointer::{
            Location, PointerAction, PointerButton, PointerId, PointerInput, PointerLocation,
            PointerMap, PointerPressState,
        },
    },
    prelude::*,
    window::WindowRef,
};
use serde_json::json;
use std::collections::VecDeque;

#[derive(Resource, Default)]
pub(crate) struct NativeInput(VecDeque<PointerInput>);

impl NativeInput {
    pub fn send(&mut self, output: Entity, position: Vec2, action: PointerAction) {
        let Some(target) = WindowRef::Entity(output).normalize(None) else {
            return;
        };
        self.0.push_back(PointerInput::new(
            PointerId::Mouse,
            Location {
                target: NormalizedRenderTarget::Window(target),
                position,
            },
            action,
        ));
    }
}

#[derive(Component)]
struct Placement {
    start: Vec2,
    offset: Vec2,
}

#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
struct PointerStep;

pub(super) fn setup(app: &mut App) {
    if !app.is_plugin_added::<PickingPlugin>() {
        app.add_plugins((PickingPlugin, InteractionPlugin));
    }
    app.insert_resource(PickingSettings {
        is_input_enabled: false,
        is_hover_enabled: false,
        is_window_picking_enabled: false,
        ..default()
    })
    .configure_sets(PreUpdate, PickingSystems::Backend.run_if(|| false))
    .init_resource::<NativeInput>()
    .init_schedule(PointerStep)
    .add_systems(
        PointerStep,
        (
            PointerInput::receive,
            region_hits,
            generate_hovermap,
            update_interactions,
            pointer_events,
            capture_press,
            place_drag,
            publish_clicks,
        )
            .chain(),
    )
    .add_systems(PreUpdate, pointer)
    .add_observer(|_: On<Remove<Output>>, mut commands: Commands| commands.queue(cancel_pointer))
    .add_observer(
        |event: On<Remove<Character>>, capture: Res<PointerCaptureMap>, mut commands: Commands| {
            if capture
                .get(&PointerId::Mouse)
                .is_some_and(|(entity, _)| entity == event.entity)
            {
                commands.queue(cancel_pointer);
            }
        },
    );
    app.world_mut().spawn(PointerId::Mouse);
}

pub(crate) fn cancel_pointer(world: &mut World) {
    let queued = &world.resource::<NativeInput>().0;
    if queued
        .back()
        .is_some_and(|input| matches!(input.action, PointerAction::Cancel))
    {
        return;
    }
    if queued.is_empty()
        && !world
            .resource::<PointerCaptureMap>()
            .is_captured(&PointerId::Mouse)
        && !world
            .resource::<PointerMap>()
            .get_entity(PointerId::Mouse)
            .and_then(|entity| world.get::<PointerPressState>(entity))
            .is_some_and(PointerPressState::is_any_pressed)
    {
        return;
    }
    let location = world
        .resource::<NativeInput>()
        .0
        .back()
        .map(|input| input.location.clone())
        .or_else(|| {
            world
                .resource::<PointerMap>()
                .get_entity(PointerId::Mouse)
                .and_then(|entity| world.get::<PointerLocation>(entity)?.location.clone())
        });
    if let Some(location) = location {
        world
            .resource_mut::<NativeInput>()
            .0
            .push_back(PointerInput::new(
                PointerId::Mouse,
                location,
                PointerAction::Cancel,
            ));
    }
}

fn step(world: &mut World, input: PointerInput) {
    let action = input.action;
    world.write_message(input);
    world.run_schedule(PointerStep);
    if matches!(
        action,
        PointerAction::Cancel | PointerAction::Release(PointerButton::Primary)
    ) && let Some(pointer) = world.resource::<PointerMap>().get_entity(PointerId::Mouse)
    {
        let mut pointer = world.entity_mut(pointer);
        pointer.remove::<Placement>();
        // Bevy clears PointerState on Cancel, but not PointerPressState.
        if matches!(action, PointerAction::Cancel) {
            pointer.insert(PointerPressState::default());
        }
    }
}

pub(crate) fn pointer(world: &mut World) {
    while let Some(mut input) = world.resource_mut::<NativeInput>().0.pop_front() {
        let NormalizedRenderTarget::Window(output) = input.location.target else {
            continue;
        };
        if matches!(input.action, PointerAction::Cancel) {
            step(world, input);
            continue;
        }
        let Some(screen) = world.get::<Output>(output.entity()) else {
            input.action = PointerAction::Cancel;
            step(world, input);
            continue;
        };
        let position = desktop_position(screen, input.location.position);
        let scale = screen.units_per_logical_pixel;
        let Some(pointer) = world.resource::<PointerMap>().get_entity(PointerId::Mouse) else {
            continue;
        };
        let dragging = world
            .resource::<PointerState>()
            .get(PointerId::Mouse, PointerButton::Primary)
            .is_some_and(|state| !state.dragging.is_empty());
        let delta = world
            .get::<Placement>(pointer)
            .map_or(Vec2::ZERO, |placement| {
                let displacement = position - placement.start;
                if dragging || displacement.length() / scale > 4. {
                    displacement
                } else {
                    Vec2::ZERO
                }
            });
        // Receive only updates location on Move. Release also needs its own movement
        // sample, and a prior hover step for Bevy's PreviousHoverMap click target.
        let movement = PointerInput::new(
            input.pointer_id,
            input.location.clone(),
            PointerAction::Move { delta },
        );
        step(world, movement);
        if !matches!(input.action, PointerAction::Move { .. }) {
            step(world, input);
        }
    }
}

fn region_hits(
    pointers: Query<(&PointerId, &PointerLocation)>,
    regions: Query<&HitRegions, With<Output>>,
    characters: Query<&Character>,
    mut hits: MessageWriter<PointerHits>,
) {
    for (id, pointer) in &pointers {
        let Some(Location {
            target: NormalizedRenderTarget::Window(output),
            position,
        }) = pointer.location()
        else {
            continue;
        };
        let Ok(regions) = regions.get(output.entity()) else {
            continue;
        };
        let mut owners: Vec<_> = regions
            .0
            .iter()
            .filter(|(entity, rect)| {
                rect.contains(*position)
                    && characters
                        .get(*entity)
                        .is_ok_and(|character| character.state == AvatarState::Ready)
            })
            .map(|(entity, _)| *entity)
            .collect();
        owners.sort_unstable_by_key(|entity| entity.to_bits());
        owners.dedup();
        let picks = owners
            .iter()
            .enumerate()
            .map(|(rank, entity)| {
                (
                    *entity,
                    HitData::new(output.entity(), (owners.len() - rank) as f32, None, None),
                )
            })
            .collect();
        hits.write(PointerHits::new(*id, picks, 0.));
    }
}

fn capture_press(
    mut events: MessageReader<PointerPress>,
    outputs: Query<&Output>,
    characters: Query<(&Character, &Transform)>,
    map: Res<PointerMap>,
    mut capture: ResMut<PointerCaptureMap>,
    mut commands: Commands,
) {
    let mouse = map.into_inner().get_entity(PointerId::Mouse);
    for event in events.read().filter(|event| {
        event.pointer.id == PointerId::Mouse && event.button == PointerButton::Primary
    }) {
        let NormalizedRenderTarget::Window(output) = event.pointer.target else {
            continue;
        };
        let Ok(screen) = outputs.get(output.entity()) else {
            continue;
        };
        let Ok((character, transform)) = characters.get(event.entity) else {
            continue;
        };
        if character.state != AvatarState::Ready {
            continue;
        }
        let start = desktop_position(screen, event.pointer.position);
        let anchor = Vec2::new(transform.translation.x, -transform.translation.y);
        capture.capture(PointerId::Mouse, event.entity, event.hit.clone());
        if let Some(pointer_entity) = mouse {
            commands.entity(pointer_entity).insert(Placement {
                start,
                offset: (anchor - start) / screen.units_per_logical_pixel,
            });
        }
    }
}

fn place_drag(
    pointers: Query<(&Placement, &PointerLocation)>,
    outputs: Query<&Output>,
    state: Res<PointerState>,
    mut characters: Query<&mut Character>,
) {
    let Some(state) = state
        .into_inner()
        .get(PointerId::Mouse, PointerButton::Primary)
    else {
        return;
    };
    for (placement, location) in &pointers {
        let Some(Location {
            target: NormalizedRenderTarget::Window(output),
            position,
        }) = location.location()
        else {
            continue;
        };
        let Ok(screen) = outputs.get(output.entity()) else {
            continue;
        };
        let position = desktop_position(screen, *position);
        let destination = outputs
            .iter()
            .find(|output| bounds(output).contains(position))
            .unwrap_or(screen);
        for entity in state.dragging.keys() {
            if let Ok(mut character) = characters.get_mut(*entity)
                && character.state == AvatarState::Ready
            {
                character.config.position = Some(
                    ((position - destination.origin) / destination.units_per_logical_pixel
                        + placement.offset)
                        .to_array(),
                );
                character.config.monitor = Some(destination.name.clone());
            }
        }
    }
}

fn publish_clicks(
    mut clicks: MessageReader<PointerClick>,
    mut ends: MessageReader<PointerDragEnd>,
    characters: Query<&Character>,
    mut notifications: MessageWriter<Notification>,
) {
    let dragged = ends.read().any(|event| {
        event.pointer.id == PointerId::Mouse && event.button == PointerButton::Primary
    });
    for click in clicks.read().filter(|event| {
        event.pointer.id == PointerId::Mouse && event.button == PointerButton::Primary
    }) {
        if !dragged
            && let Ok(character) = characters.get(click.entity)
            && character.state == AvatarState::Ready
        {
            notifications.write(Notification(
                json!({"event":"click", "character":character.id}),
            ));
        }
    }
}

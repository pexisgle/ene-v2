use super::*;
use crate::material::{BaseUv, EmissiveStrength, MaterialSlot};
use crate::{VrmSystems, animation, expressions, schema};
use bevy::{
    animation::AnimatedBy, gltf::Gltf, platform::collections::HashMap as BevyHashMap,
    world_serialization::WorldInstanceReady,
};
use serde_json::json;

#[derive(Resource, Default)]
struct ReadyCount(usize);

fn build_interpolation_clip(
    app: &mut App,
    files: &Files,
    interpolation: &str,
) -> (AnimationNodeIndex, Handle<AnimationGraph>) {
    let mut motion = files.motion(interpolation);
    motion["extensions"]["VRMC_vrm_animation"]["lookAt"] = json!({"node":0});
    motion["extensions"]["VRMC_vrm_animation"]["expressions"] = json!({"preset":{"aa":{"node":4}}});
    motion["animations"][0]["channels"]
        .as_array_mut()
        .unwrap()
        .extend([
            json!({"sampler":0,"target":{"node":4,"path":"translation"}}),
            json!({"sampler":1,"target":{"node":0,"path":"rotation"}}),
        ]);
    if interpolation == "CUBICSPLINE" {
        let mut values = vec![0., 1.];
        for value in [[0., 1., 0.], [1., 1., 0.]] {
            values.extend([0.; 3]);
            values.extend(value);
            values.extend([0.; 3]);
        }
        for value in [Quat::IDENTITY, Quat::from_rotation_x(0.4)] {
            values.extend([0.; 4]);
            values.extend(value.to_array());
            values.extend([0.; 4]);
        }
        std::fs::write(
            files.join("motion.bin"),
            values
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        motion["buffers"][0]["byteLength"] = json!(176);
        motion["bufferViews"][1]["byteLength"] = json!(72);
        motion["bufferViews"][2]["byteOffset"] = json!(80);
        motion["bufferViews"][2]["byteLength"] = json!(96);
        motion["accessors"][1]["count"] = json!(6);
        motion["accessors"][2]["count"] = json!(6);
    }
    let handle: Handle<Gltf> = app
        .world()
        .resource::<AssetServer>()
        .load(files.write("inputs.vrma", &motion));
    settle(app, std::slice::from_ref(&handle));
    let clip = app
        .world()
        .resource::<Assets<Gltf>>()
        .get(&handle)
        .unwrap()
        .animations[0]
        .clone();
    let (graph, node) = AnimationGraph::from_clip(clip);
    let graph = app
        .world_mut()
        .resource_mut::<Assets<AnimationGraph>>()
        .add(graph);
    (node, graph)
}

fn assert_interpolation_case(app: &mut App, interpolation: &str) {
    let files = Files::new();
    let (node, graph) = build_interpolation_clip(app, &files, interpolation);
    let mut player = AnimationPlayer::default();
    player.start(node).pause().set_seek_time(0.25);
    let root = app
        .world_mut()
        .spawn((player, AnimationGraphHandle(graph)))
        .id();
    let expression = app
        .world_mut()
        .spawn((
            animation::ExpressionInput::default(),
            animation::expression_target(&expressions::ExpressionName::Preset(
                schema::ExpressionPresetName::Aa,
            )),
            AnimatedBy(root),
            ChildOf(root),
        ))
        .id();
    let gaze = app
        .world_mut()
        .spawn((
            animation::GazeInput::default(),
            animation::gaze_target(),
            AnimatedBy(root),
            ChildOf(root),
        ))
        .id();
    for _ in 0..2 {
        app.update();
    }
    let expected = match interpolation {
        "STEP" => 0.,
        "LINEAR" => 0.25,
        _ => 0.15625,
    };
    let rotation = match interpolation {
        "STEP" => Quat::IDENTITY,
        "LINEAR" => Quat::from_rotation_x(0.1),
        _ => Quat::from_vec4(
            Vec4::from_array(Quat::IDENTITY.to_array())
                .lerp(
                    Vec4::from_array(Quat::from_rotation_x(0.4).to_array()),
                    expected,
                )
                .normalize(),
        ),
    };
    for _ in 0..2 {
        app.update();
        assert!(
            (app.world()
                .get::<animation::ExpressionInput>(expression)
                .unwrap()
                .value
                .unwrap()
                - expected)
                .abs()
                < 1e-5,
            "{interpolation}"
        );
        assert!(
            app.world()
                .get::<animation::GazeInput>(gaze)
                .unwrap()
                .rotation
                .unwrap()
                .abs_diff_eq(rotation, 1e-5),
            "{interpolation}"
        );
    }
    app.world_mut()
        .get_mut::<AnimationPlayer>(root)
        .unwrap()
        .stop_all();
    app.update();
    assert_eq!(
        app.world()
            .get::<animation::ExpressionInput>(expression)
            .unwrap()
            .value,
        None
    );
    assert_eq!(
        app.world()
            .get::<animation::GazeInput>(gaze)
            .unwrap()
            .rotation,
        None
    );
    app.world_mut().entity_mut(root).despawn();
}

#[test]
fn typed_expression_and_gaze_inputs_keep_native_interpolation_and_pause() {
    let mut app = app();
    for interpolation in ["STEP", "LINEAR", "CUBICSPLINE"] {
        assert_interpolation_case(&mut app, interpolation);
    }
}

fn holder_materials(app: &App, holder: Entity) -> Vec<Handle<StandardMaterial>> {
    let children = app.world().get::<Children>(holder).unwrap();
    let root = children
        .iter()
        .find(|&entity| app.world().get::<expressions::Binds>(entity).is_some())
        .unwrap();
    app.world()
        .get::<Children>(root)
        .unwrap()
        .iter()
        .filter_map(|entity| {
            app.world()
                .get::<MeshMaterial3d<StandardMaterial>>(entity)
                .map(|m| m.0.clone())
        })
        .collect()
}

fn assert_isolated_holder_materials(
    app: &App,
    handles: &[Handle<StandardMaterial>],
    original: &Handle<StandardMaterial>,
    missing: &Handle<StandardMaterial>,
    uv: schema::UvTransform,
) {
    assert_eq!(handles[0], handles[1]);
    assert_ne!(handles[0], handles[2]);
    assert_ne!(&handles[0], original);
    assert_ne!(&handles[2], missing);
    let material = app
        .world()
        .resource::<Assets<StandardMaterial>>()
        .get(&handles[0])
        .unwrap();
    assert!(
        material
            .uv_transform
            .matrix2
            .abs_diff_eq(uv.affine().matrix2, 1e-6)
    );
    assert!(
        material
            .uv_transform
            .translation
            .abs_diff_eq(uv.offset, 1e-6)
    );
}

#[test]
fn expression_materials_wait_atomically_and_isolate_ready_instances() {
    let mut app = app();
    app.init_resource::<ReadyCount>()
        .add_observer(|_: On<WorldInstanceReady>, mut ready: ResMut<ReadyCount>| ready.0 += 1);
    let (original, missing) = {
        let mut assets = app.world_mut().resource_mut::<Assets<StandardMaterial>>();
        (
            assets.add(StandardMaterial::default()),
            assets.reserve_handle(),
        )
    };
    let definitions = serde_json::from_value(json!({"custom":{"slide":{
        "textureTransformBinds":[{"material":0,"scale":[2,2],"offset":[0.5,0]}]
    }}}))
    .unwrap();
    let uv = schema::UvTransform {
        scale: Vec2::new(0., -2.),
        rotation: -0.5,
        offset: Vec2::new(0.2, 0.4),
    };
    let mut world = World::new();
    let root = world.spawn(Transform::default()).id();
    expressions::attach(
        &mut world,
        root,
        Some(&definitions),
        &BevyHashMap::default(),
    )
    .unwrap();
    for handle in [&original, &original, &missing] {
        world.spawn((
            MeshMaterial3d(handle.clone()),
            MaterialSlot(0),
            BaseUv(uv),
            EmissiveStrength(2.),
            ChildOf(root),
        ));
    }
    let scene = app
        .world_mut()
        .resource_mut::<Assets<WorldAsset>>()
        .add(WorldAsset::new(world));
    let holders: Vec<_> = (0..3)
        .map(|_| app.world_mut().spawn(WorldAssetRoot(scene.clone())).id())
        .collect();
    for _ in 0..4 {
        app.update();
    }
    assert_eq!(app.world().resource::<ReadyCount>().0, 3);
    for &holder in &holders {
        assert_eq!(
            holder_materials(&app, holder),
            [original.clone(), original.clone(), missing.clone()]
        );
    }
    app.world_mut().entity_mut(holders[2]).despawn();
    app.world_mut()
        .resource_mut::<Assets<StandardMaterial>>()
        .insert(missing.id(), StandardMaterial::default())
        .unwrap();
    for _ in 0..4 {
        app.update();
    }
    let a = holder_materials(&app, holders[0]);
    let b = holder_materials(&app, holders[1]);
    assert_isolated_holder_materials(&app, &a, &original, &missing, uv);
    assert_isolated_holder_materials(&app, &b, &original, &missing, uv);
    assert_ne!(a[0], b[0]);
    assert_ne!(a[2], b[2]);
    for _ in 0..10 {
        app.update();
    }
    assert_eq!(holder_materials(&app, holders[0]), a);
    assert_eq!(holder_materials(&app, holders[1]), b);

    let root = app.world_mut().spawn(Transform::default()).id();
    expressions::attach(
        app.world_mut(),
        root,
        Some(&definitions),
        &BevyHashMap::default(),
    )
    .unwrap();
    let mesh = app
        .world_mut()
        .spawn((
            MeshMaterial3d(original.clone()),
            MaterialSlot(0),
            ChildOf(root),
        ))
        .id();
    app.update();
    assert_ne!(
        app.world()
            .get::<MeshMaterial3d<StandardMaterial>>(mesh)
            .unwrap()
            .0,
        original
    );
}

fn assert_pending_material_change(
    app: &mut App,
    definitions: &schema::Expressions,
    missing: &Handle<StandardMaterial>,
    available: &Handle<StandardMaterial>,
    change: &str,
) {
    let root = app.world_mut().spawn(Transform::default()).id();
    expressions::attach(
        app.world_mut(),
        root,
        Some(definitions),
        &BevyHashMap::default(),
    )
    .unwrap();
    let mesh = app
        .world_mut()
        .spawn((
            MaterialSlot(0),
            MeshMaterial3d(missing.clone()),
            ChildOf(root),
        ))
        .id();
    app.update();
    assert_eq!(
        &app.world()
            .get::<MeshMaterial3d<StandardMaterial>>(mesh)
            .unwrap()
            .0,
        missing
    );
    let live = match change {
        "handle" => {
            app.world_mut()
                .entity_mut(mesh)
                .insert(MeshMaterial3d(available.clone()));
            mesh
        }
        "slot" => {
            app.world_mut().entity_mut(mesh).insert(MaterialSlot(1));
            app.world_mut()
                .spawn((
                    MaterialSlot(0),
                    MeshMaterial3d(available.clone()),
                    ChildOf(root),
                ))
                .id()
        }
        _ => {
            app.world_mut().entity_mut(mesh).despawn();
            app.world_mut()
                .spawn((
                    MaterialSlot(0),
                    MeshMaterial3d(available.clone()),
                    ChildOf(root),
                ))
                .id()
        }
    };
    app.update();
    let cloned = app
        .world()
        .get::<MeshMaterial3d<StandardMaterial>>(live)
        .unwrap()
        .0
        .clone();
    assert_ne!(
        &cloned, available,
        "{change} must initialize without the old asset arriving"
    );
    assert_ne!(&cloned, missing);
    let target = app
        .world()
        .get::<expressions::Binds>(root)
        .unwrap()
        .target(&expressions::ExpressionName::Custom("tint".into()))
        .unwrap();
    app.add_systems(
        PostUpdate,
        (move |mut inputs: Query<&mut animation::ExpressionInput>| {
            inputs.get_mut(target).unwrap().value = Some(1.);
        })
        .in_set(VrmSystems::LookAt),
    );
    app.update();
    assert_eq!(
        app.world()
            .resource::<Assets<StandardMaterial>>()
            .get(&cloned)
            .unwrap()
            .base_color,
        Color::linear_rgba(1., 0., 0., 1.)
    );
    if change == "slot" {
        assert_eq!(
            &app.world()
                .get::<MeshMaterial3d<StandardMaterial>>(mesh)
                .unwrap()
                .0,
            missing
        );
    }
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(
        app.world()
            .get::<MeshMaterial3d<StandardMaterial>>(live)
            .unwrap()
            .0,
        cloned
    );
}

#[test]
fn review_pending_materials_follow_live_handles_slots_and_children() {
    let mut app = app();
    let (missing, available) = {
        let mut assets = app.world_mut().resource_mut::<Assets<StandardMaterial>>();
        (
            assets.reserve_handle(),
            assets.add(StandardMaterial::default()),
        )
    };
    let definitions: schema::Expressions = serde_json::from_value(json!({"custom":{"tint":{
        "materialColorBinds":[{"material":0,"type":"color","targetValue":[1,0,0,1]}]
    }}}))
    .unwrap();
    for change in ["handle", "slot", "child"] {
        assert_pending_material_change(&mut app, &definitions, &missing, &available, change);
    }
}

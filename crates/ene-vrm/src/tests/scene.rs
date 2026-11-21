use super::*;
use crate::{
    HumanBoneName, SpringJointProps, SpringRig, Vrm, animation, expressions, loader, schema,
};
use bevy::{
    animation::{AnimatedBy, AnimationTargetId},
    gltf::Gltf,
};
use serde_json::{Value, json};

struct SceneRig {
    root: Entity,
    hips: Entity,
    spine: Entity,
    head: Entity,
    left_eye: Entity,
}

fn prepare_scene_test_assets(
    files: &Files,
    app: &mut App,
) -> (
    Handle<WorldAsset>,
    AnimationNodeIndex,
    Handle<AnimationGraph>,
) {
    let mut model = files.model();
    model["scenes"] = json!([{"nodes":[0]},{"nodes":[0]}]);
    model["scene"] = json!(1);
    let eye = model["nodes"].as_array().unwrap().len();
    model["nodes"][3]["children"] = json!([eye, eye + 1]);
    model["nodes"].as_array_mut().unwrap().extend([
        json!({"name":"LeftEye","translation":[0.1,0.1,0.],"rotation":Quat::from_rotation_z(0.15).to_array()}),
        json!({"name":"RightEye","translation":[-0.1,0.1,0.]})
    ]);
    model["extensions"]["VRMC_vrm"]["humanoid"]["humanBones"]["leftEye"] = json!({"node":eye});
    model["extensions"]["VRMC_vrm"]["humanoid"]["humanBones"]["rightEye"] = json!({"node":eye + 1});
    model["extensions"]["VRMC_vrm"]["lookAt"] =
        json!({"type":"bone","offsetFromHeadBone":[0.02,0.03,0.04]});
    model["extensions"]["VRMC_vrm"]["expressions"]["preset"]["blink"] =
        json!({"overrideLookAt":"blend"});

    let mut motion_val = files.motion("LINEAR");
    motion_val["extensions"]["VRMC_vrm_animation"]["lookAt"] = json!({"node":0});
    motion_val["extensions"]["VRMC_vrm_animation"]["expressions"] =
        json!({"preset":{"aa":{"node":4}}});
    motion_val["animations"][0]["channels"]
        .as_array_mut()
        .unwrap()
        .push(json!({"sampler":0,"target":{"node":4,"path":"translation"}}));
    motion_val["animations"][0]["channels"]
        .as_array_mut()
        .unwrap()
        .push(json!({"sampler":1,"target":{"node":0,"path":"rotation"}}));

    let h_model: Handle<Gltf> = app
        .world()
        .resource::<AssetServer>()
        .load(files.write("model.vrm", &model));
    let h_motion: Handle<Gltf> = app
        .world()
        .resource::<AssetServer>()
        .load(files.write("motion.vrma", &motion_val));
    settle(app, &[h_model.clone(), h_motion.clone()]);
    assert!(
        app.world().resource::<Assets<Gltf>>().contains(&h_motion),
        "{:?}",
        app.world()
            .resource::<AssetServer>()
            .get_load_state(h_motion.id())
    );

    let scene = app
        .world()
        .resource::<Assets<Gltf>>()
        .get(&h_model)
        .unwrap()
        .default_scene
        .clone()
        .unwrap();
    let clip = app
        .world()
        .resource::<Assets<Gltf>>()
        .get(&h_motion)
        .unwrap()
        .animations[0]
        .clone();
    let (graph, node) = AnimationGraph::from_clip(clip);
    let graph = app
        .world_mut()
        .resource_mut::<Assets<AnimationGraph>>()
        .add(graph);
    (scene, node, graph)
}

fn spawn_and_inspect_scene_rigs(
    app: &mut App,
    scene: &Handle<WorldAsset>,
) -> (Vec<Entity>, Vec<SceneRig>) {
    let holders: Vec<_> = [
        Transform::IDENTITY,
        Transform::from_xyz(20., -30., 7.)
            .with_rotation(Quat::from_rotation_y(0.7))
            .with_scale(Vec3::splat(160.)),
    ]
    .into_iter()
    .map(|t| {
        app.world_mut()
            .spawn((WorldAssetRoot(scene.clone()), t))
            .id()
    })
    .collect();
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(
        app.world_mut()
            .query::<&loader::VrmScene>()
            .iter(app.world())
            .count(),
        0
    );
    let rigs: Vec<_> = app
        .world_mut()
        .query::<(Entity, &Vrm)>()
        .iter(app.world())
        .map(|(root, v)| SceneRig {
            root,
            hips: v.bone(HumanBoneName::Hips).unwrap(),
            spine: v.bone(HumanBoneName::Spine).unwrap(),
            head: v.bone(HumanBoneName::Head).unwrap(),
            left_eye: v.bone(HumanBoneName::LeftEye).unwrap(),
        })
        .collect();
    assert_eq!(rigs.len(), 2);
    assert_eq!(
        app.world_mut()
            .query_filtered::<Entity, Or<(
                With<animation::NativeNode>,
                With<animation::NativeSceneRoot>
            )>>()
            .iter(app.world())
            .count(),
        0
    );
    for rig in &rigs {
        app.world_mut()
            .entity_mut(rig.head)
            .insert(bevy::mesh::morph::MorphWeights::new(vec![0.], None).unwrap());
        let mut binds = app
            .world_mut()
            .get_mut::<expressions::Binds>(rig.root)
            .unwrap();
        let index = binds
            .bindings
            .iter()
            .position(|binding| {
                binding.name
                    == expressions::ExpressionName::Preset(schema::ExpressionPresetName::Aa)
            })
            .unwrap();
        let target = binds.bindings[index].target;
        binds.morphs.push(expressions::MorphTarget {
            entity: rig.head,
            indices: vec![(0, vec![(index, 1.)])],
        });
        assert!(app.world().get::<Transform>(target).is_none());
    }
    assert_eq!(
        app.world().get::<Vrm>(rigs[0].root).unwrap().meta.name,
        "Test",
        "Vrm must expose the parsed VRM meta"
    );
    let bone_count: usize = app
        .world_mut()
        .query::<&Vrm>()
        .iter(app.world())
        .map(|vrm| vrm.bones.len())
        .sum();
    assert_eq!(
        app.world_mut()
            .query::<&animation::Pose>()
            .iter(app.world())
            .count(),
        bone_count,
        "humanoid bones own Pose directly; no per-bone target entities"
    );
    for rig in &rigs {
        assert_eq!(
            app.world().get::<AnimationTargetId>(rig.hips),
            Some(&animation::bone_target(HumanBoneName::Hips))
        );
        for bone in [rig.hips, rig.spine, rig.head, rig.left_eye] {
            assert!(app.world().get::<animation::Pose>(bone).is_some());
            assert!(
                app.world()
                    .get::<AnimatedBy>(bone)
                    .is_some_and(|by| by.0 == rig.root)
            );
        }
    }
    (holders, rigs)
}

fn head_morph_weight(app: &App, head: Entity) -> f32 {
    app.world()
        .get::<bevy::mesh::morph::MorphWeights>(head)
        .unwrap()
        .weights()[0]
}

fn assert_scene_playback(
    app: &mut App,
    rigs: &[SceneRig],
    node: AnimationNodeIndex,
    graph: &Handle<AnimationGraph>,
) {
    for rig in rigs {
        let mut player = AnimationPlayer::default();
        player.play(node).seek_to(0.5).pause();
        app.world_mut()
            .entity_mut(rig.root)
            .insert((player, AnimationGraphHandle(graph.clone())));
    }
    for _ in 0..2 {
        app.update();
    }
    for rig in rigs {
        assert!(
            app.world()
                .get::<Transform>(rig.hips)
                .unwrap()
                .translation
                .abs_diff_eq(Vec3::new(0.5, 1., 0.), 1e-4)
        );
        assert!(
            app.world()
                .get::<Transform>(rig.spine)
                .unwrap()
                .rotation
                .abs_diff_eq(Quat::from_rotation_x(0.2), 1e-4)
        );
        assert!(gaze_input_active(app, rig.root));
        assert!(
            app.world()
                .get::<Transform>(gaze_input(app, rig.root))
                .is_none()
        );
        let head = app
            .world()
            .get::<Vrm>(rig.root)
            .unwrap()
            .bone(HumanBoneName::Head)
            .unwrap();
        assert!((head_morph_weight(app, head) - 0.5).abs() < 1e-5);
    }
    let gaze_eye_rest = app
        .world()
        .get::<animation::RestPose>(rigs[0].left_eye)
        .unwrap()
        .local
        .rotation;
    assert!(
        app.world()
            .get::<Transform>(rigs[0].left_eye)
            .unwrap()
            .rotation
            .angle_between(gaze_eye_rest)
            > 0.01,
        "VRMA gaze did not rotate the eye bone"
    );
}

fn assert_stopped_and_masked_playback(
    app: &mut App,
    rigs: &[SceneRig],
    node: AnimationNodeIndex,
    graph: &Handle<AnimationGraph>,
) {
    // A stopped animation leaves every humanoid bone at rest.
    let primary = &rigs[0];
    let rest = [
        ("hips", primary.hips),
        ("spine", primary.spine),
        ("eye", primary.left_eye),
    ]
    .map(|(name, entity)| {
        (
            name,
            entity,
            app.world()
                .get::<animation::RestPose>(entity)
                .unwrap()
                .local,
        )
    });
    let eye_rest = rest[2].2.rotation;
    app.world_mut()
        .get_mut::<AnimationPlayer>(primary.root)
        .unwrap()
        .stop_all();
    app.update();
    assert!(!gaze_input_active(app, primary.root));
    for (name, entity, expected) in rest {
        let actual = *app.world().get::<Transform>(entity).unwrap();
        assert!(
            actual.rotation.abs_diff_eq(expected.rotation, 1e-5)
                && actual.translation.abs_diff_eq(expected.translation, 1e-5),
            "{name} did not return to rest: {actual:?} != {expected:?}"
        );
    }
    assert!(
        app.world()
            .get::<Transform>(primary.left_eye)
            .unwrap()
            .rotation
            .abs_diff_eq(eye_rest, 1e-5)
    );

    let secondary = &rigs[1];
    assert_eq!(head_morph_weight(app, primary.head), 0.);
    assert!((head_morph_weight(app, secondary.head) - 0.5).abs() < 1e-5);
    assert!(gaze_input_active(app, secondary.root));
    app.world_mut()
        .get_mut::<AnimationPlayer>(secondary.root)
        .unwrap()
        .animation_mut(node)
        .unwrap()
        .set_weight(0.);
    app.update();
    assert_eq!(head_morph_weight(app, secondary.head), 0.);
    assert!(!gaze_input_active(app, secondary.root));
    app.world_mut()
        .get_mut::<AnimationPlayer>(secondary.root)
        .unwrap()
        .animation_mut(node)
        .unwrap()
        .set_weight(1.);
    app.update();
    assert!((head_morph_weight(app, secondary.head) - 0.5).abs() < 1e-5);
    {
        let mut graphs = app.world_mut().resource_mut::<Assets<AnimationGraph>>();
        let mut graph_asset = graphs.get_mut(graph).unwrap();
        graph_asset.add_target_to_mask_group(animation::gaze_target(), 0);
        graph_asset.add_target_to_mask_group(
            animation::expression_target(&expressions::ExpressionName::Preset(
                schema::ExpressionPresetName::Aa,
            )),
            0,
        );
        graph_asset.graph[node].add_mask_group(0);
    }
    for _ in 0..2 {
        app.update();
    }
    assert_eq!(head_morph_weight(app, secondary.head), 0.);
    assert!(!gaze_input_active(app, secondary.root));
}

#[test]
fn scene_instances_remap_bones_and_preserve_native_and_vrma_playback() {
    let files = Files::new();
    let mut app = app();
    let (scene, node, graph) = prepare_scene_test_assets(&files, &mut app);
    let (holders, rigs) = spawn_and_inspect_scene_rigs(&mut app, &scene);
    assert_scene_playback(&mut app, &rigs, node, &graph);
    assert_stopped_and_masked_playback(&mut app, &rigs, node, &graph);
    for h in holders {
        app.world_mut().entity_mut(h).despawn();
    }
    app.update();
    assert_eq!(app.world_mut().query::<&Vrm>().iter(app.world()).count(), 0);
}

#[test]
fn native_scene_instances_remap_one_shared_collider_for_multiple_chains() {
    let files = Files::new();
    let mut value = files.model();
    let base = value["nodes"].as_array().unwrap().len();
    value["nodes"][1]["children"] = Value::from([2, 4, 7, base, base + 2]);
    value["nodes"].as_array_mut().unwrap().extend([
        json!({"translation":[0.1,1.,0.],"children":[base + 1]}),
        json!({"translation":[0.,0.1,0.]}),
        json!({"translation":[-0.1,1.,0.],"children":[base + 3]}),
        json!({"translation":[0.,0.1,0.]}),
    ]);
    value["extensions"]["VRMC_springBone"] = json!({
        "specVersion":"1.0",
        "colliders":[{"node":1,"shape":{"sphere":{"offset":[0.,10.,0.],"radius":0.01}}}],
        "colliderGroups":[{"colliders":[0]}],
        "springs":[
            {"joints":[{"node":base},{"node":base + 1}],"colliderGroups":[0]},
            {"joints":[{"node":base + 2},{"node":base + 3}],"colliderGroups":[0]}
        ]
    });
    let mut app = app();
    let handle: Handle<Gltf> = app
        .world()
        .resource::<AssetServer>()
        .load(files.write("shared-collider.vrm", &value));
    settle(&mut app, std::slice::from_ref(&handle));
    let scene = app
        .world()
        .resource::<Assets<Gltf>>()
        .get(&handle)
        .unwrap()
        .default_scene
        .clone()
        .unwrap();
    let holders: Vec<_> = [0., 20.]
        .into_iter()
        .map(|x| {
            app.world_mut()
                .spawn((
                    WorldAssetRoot(scene.clone()),
                    Transform::from_xyz(x, 0., 0.),
                ))
                .id()
        })
        .collect();
    for _ in 0..5 {
        app.update();
    }
    let mut q = app.world_mut().query::<(&Vrm, &SpringRig)>();
    let rigs: Vec<_> = q.iter(app.world()).collect();
    assert_eq!(rigs.len(), 2);
    for (vrm, rig) in &rigs {
        assert_eq!(
            rig.colliders[0].entity,
            vrm.bone(schema::HumanBoneName::Hips).unwrap()
        );
        assert_eq!(rig.chains.len(), 2);
        for chain in &rig.chains {
            assert_eq!(chain.colliders, vec![0]);
            assert!(
                chain.joints[0]
                    .motion
                    .unwrap()
                    .iter()
                    .all(|p| p.is_finite())
            );
            assert!(
                app.world()
                    .get::<SpringJointProps>(chain.joints[0].entity)
                    .is_some()
            );
        }
    }
    assert_ne!(
        rigs[0].1.chains[0].joints[0].entity,
        rigs[1].1.chains[0].joints[0].entity
    );
    app.world_mut().entity_mut(holders[0]).despawn();
    for _ in 0..3 {
        app.update();
    }
    let mut q = app.world_mut().query::<&SpringRig>();
    let rig = q.single(app.world()).unwrap();
    assert!(rig.chains[0].joints[0].motion.is_some());
}

#[test]
fn paused_spring_holds_last_pose() {
    let files = Files::new();
    let mut app = app();
    let mut model = files.model();
    let head = 3;
    let tail = model["nodes"].as_array().unwrap().len();
    model["nodes"][head]["children"] = json!([tail]);
    model["nodes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"translation":[0.2,0.0,0.0]}));
    model["nodes"][head]["extensions"] = json!({
        "VRMC_node_constraint": {
            "specVersion":"1.0",
            "constraint":{"rotation":{"source":2}}
        }
    });
    model["extensions"]["VRMC_springBone"] = json!({
        "specVersion":"1.0",
        "springs":[{"joints":[
            {"node":head,"gravityPower":1.0,"stiffness":0.5,"dragForce":0.4},
            {"node":tail}
        ]}]
    });

    let handle: Handle<Gltf> = app
        .world()
        .resource::<AssetServer>()
        .load(files.write("pause.vrm", &model));
    settle(&mut app, std::slice::from_ref(&handle));
    let scene = app
        .world()
        .resource::<Assets<Gltf>>()
        .get(&handle)
        .unwrap()
        .default_scene
        .clone()
        .unwrap();
    app.world_mut()
        .spawn((WorldAssetRoot(scene), Transform::default()));

    let (root, head_entity) = loop {
        app.update();
        let mut query = app.world_mut().query::<(Entity, &Vrm)>();
        if let Some((root, vrm)) = query.iter(app.world()).next() {
            break (root, vrm.bone(HumanBoneName::Head).expect("head bone"));
        }
    };
    let mut simulated = Quat::IDENTITY;
    for _ in 0..120 {
        app.update();
        let rotation = app.world().get::<Transform>(head_entity).unwrap().rotation;
        if rotation.angle_between(Quat::IDENTITY) > 0.01 {
            simulated = rotation;
            break;
        }
    }
    assert!(
        simulated.angle_between(Quat::IDENTITY) > 0.01,
        "spring joint did not move"
    );

    app.world_mut().get_mut::<SpringRig>(root).unwrap().paused = true;
    for _ in 0..10 {
        app.update();
        let held = app.world().get::<Transform>(head_entity).unwrap().rotation;
        assert!(
            held.abs_diff_eq(simulated, 1e-5),
            "paused spring moved: {held:?} != {simulated:?}"
        );
    }
    app.world_mut().get_mut::<SpringRig>(root).unwrap().paused = false;
    assert!(
        (0..10).any(|_| {
            app.update();
            !app.world()
                .get::<Transform>(head_entity)
                .unwrap()
                .rotation
                .abs_diff_eq(simulated, 1e-5)
        }),
        "resumed spring did not move"
    );
}

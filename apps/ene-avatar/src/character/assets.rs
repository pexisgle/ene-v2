//! Avatar scene and motion asset loading.

use super::model::{AvatarState, CharacterId, Config, Notification};
use bevy::{
    asset::LoadState,
    gltf::{Gltf, GltfAssetLabel},
    prelude::*,
};
use ene_vrm::Vrm;
use serde_json::json;

#[derive(Component)]
pub(crate) struct ModelAssets {
    pub(super) model: Handle<Gltf>,
    pub(super) motion: Option<Handle<AnimationClip>>,
    pub(super) scene: Entity,
}

pub(crate) fn reload(
    In(entity): In<Entity>,
    mut commands: Commands,
    actors: Query<(&CharacterId, &Config, Option<&ModelAssets>)>,
    server: Res<AssetServer>,
    mut states: MessageWriter<Notification>,
) {
    let Ok((id, config, old)) = actors.get(entity) else {
        return;
    };
    if let Some(old) = old {
        commands.entity(old.scene).despawn();
    }
    commands
        .entity(entity)
        .insert((AvatarState::Loading, Visibility::Hidden));
    let scene = commands
        .spawn((Transform::default(), Visibility::Inherited, ChildOf(entity)))
        .id();
    for path in std::iter::once(&config.model).chain(config.motion.iter()) {
        if server
            .get_path_id(path.clone())
            .is_some_and(|id| matches!(server.get_load_state(id), Some(LoadState::Failed(_))))
        {
            server.reload(path.clone());
        }
    }
    commands.entity(entity).insert(ModelAssets {
        model: server.load(config.model.clone()),
        motion: config
            .motion
            .as_ref()
            .map(|path| server.load(GltfAssetLabel::Animation(0).from_asset(path.clone()))),
        scene,
    });
    states.write(Notification(
        json!({"event":"state", "character":id.0, "status":"loading", "error":null}),
    ));
}

#[allow(clippy::too_many_arguments)]
pub(super) fn load_scene(
    mut commands: Commands,
    actors: Query<&ModelAssets>,
    gltfs: Res<Assets<Gltf>>,
    worlds: Res<Assets<WorldAsset>>,
    server: Res<AssetServer>,
    scenes: Query<Has<WorldAssetRoot>>,
    children: Query<&Children>,
    rigs: Query<(), With<Vrm>>,
    graphs_present: Query<(), With<AnimationGraphHandle>>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
) {
    for assets in &actors {
        let Some(model) = gltfs.get(&assets.model) else {
            continue;
        };
        let Ok(spawned) = scenes.get(assets.scene) else {
            continue;
        };
        if !spawned {
            if let Some(scene) = model
                .default_scene
                .as_ref()
                .or_else(|| model.scenes.first())
                && worlds.contains(scene.id())
                && server.is_loaded_with_dependencies(scene.id())
            {
                commands
                    .entity(assets.scene)
                    .insert(WorldAssetRoot(scene.clone()));
            }
        } else if let Some(clip) = &assets.motion
            && server.is_loaded_with_dependencies(clip.id())
        {
            for root in children
                .iter_descendants(assets.scene)
                .filter(|e| rigs.contains(*e) && !graphs_present.contains(*e))
            {
                let (graph, node) = AnimationGraph::from_clip(clip.clone());
                let mut player = AnimationPlayer::default();
                player.play(node).repeat();
                commands
                    .entity(root)
                    .insert((player, AnimationGraphHandle(graphs.add(graph))));
            }
        }
    }
}

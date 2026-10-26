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
    /// The VRM instance root bound once per load. `Vrm` and the spring rig are
    /// attached there by `ene-vrm`; the motion player is attached there too.
    root: Option<Entity>,
}

impl ModelAssets {
    /// The VRM instance root, once the spawned scene has been scanned.
    pub(crate) fn root(&self) -> Option<Entity> {
        self.root
    }

    #[cfg(test)]
    pub(crate) fn with_root(root: Entity) -> Self {
        Self {
            model: Handle::default(),
            motion: None,
            scene: root,
            root: Some(root),
        }
    }
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
        root: None,
    });
    states.write(Notification(
        json!({"event":"state", "character":id.0, "status":"loading", "error":null}),
    ));
}

/// One-time scene setup. The scene spawns asynchronously, so this keeps
/// checking until the instance exists, then binds its root and player.
#[allow(clippy::too_many_arguments)]
pub(super) fn load_scene(
    mut commands: Commands,
    mut actors: Query<&mut ModelAssets>,
    gltfs: Res<Assets<Gltf>>,
    worlds: Res<Assets<WorldAsset>>,
    server: Res<AssetServer>,
    scenes: Query<Has<WorldAssetRoot>>,
    children: Query<&Children>,
    rigs: Query<(), With<Vrm>>,
    graphs_present: Query<(), With<AnimationGraphHandle>>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
) {
    for mut assets in &mut actors {
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
            continue;
        }
        if assets.root.is_none() {
            assets.root = children
                .iter_descendants(assets.scene)
                .find(|entity| rigs.contains(*entity));
        }
        let Some(root) = assets.root else {
            continue;
        };
        // `ene-vrm` already puts an empty player on the root; the graph marks
        // that the motion is wired up.
        if let Some(clip) = &assets.motion
            && server.is_loaded_with_dependencies(clip.id())
            && !graphs_present.contains(root)
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

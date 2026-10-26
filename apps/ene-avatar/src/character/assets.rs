//! Avatar scene and motion asset loading.

use super::model::{AvatarState, CharacterId, Config, Notification};
use bevy::{
    asset::{AssetEvent, LoadState, RecursiveDependencyLoadState, UntypedAssetId},
    gltf::{Gltf, GltfAssetLabel},
    prelude::*,
};
use ene_vrm::Vrm;

#[derive(Component)]
pub(crate) struct ModelAssets {
    pub(super) model: Handle<Gltf>,
    pub(super) motion: Option<Handle<AnimationClip>>,
    pub(super) scene: Entity,
    /// The VRM instance root bound once per load. `Vrm` and the spring rig are
    /// attached there by `ene-vrm`; the motion player is attached there too.
    root: Option<Entity>,
    pub(super) retrying_model_dependencies: bool,
    pub(super) retrying_motion_dependencies: bool,
}

impl ModelAssets {
    /// The VRM instance root, once the spawned scene has been scanned.
    pub(crate) fn root(&self) -> Option<Entity> {
        self.root
    }

    #[cfg(test)]
    pub(crate) fn model_handle(&self) -> Handle<Gltf> {
        self.model.clone()
    }

    #[cfg(test)]
    pub(crate) fn with_root(root: Entity) -> Self {
        Self {
            model: Handle::default(),
            motion: None,
            scene: root,
            root: Some(root),
            retrying_model_dependencies: false,
            retrying_motion_dependencies: false,
        }
    }
}

/// `AssetServer::load` retries a failed asset itself. Dependencies of an
/// already loaded asset need a reload of their owner to be requested again.
fn dependencies_failed(server: &AssetServer, id: UntypedAssetId) -> bool {
    matches!(
        server.get_load_states(id),
        Some((
            LoadState::Loaded,
            _,
            RecursiveDependencyLoadState::Failed(_)
        ))
    )
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
    let model = server.load(config.model.clone());
    let motion = config
        .motion
        .as_ref()
        .map(|path| server.load(GltfAssetLabel::Animation(0).from_asset(path.clone())));
    let retrying_model_dependencies = dependencies_failed(&server, model.id().untyped());
    if retrying_model_dependencies {
        server.reload(config.model.clone());
    }
    let retrying_motion_dependencies = motion
        .as_ref()
        .is_some_and(|handle| dependencies_failed(&server, handle.id().untyped()));
    if retrying_motion_dependencies && let Some(path) = &config.motion {
        server.reload(path.clone());
    }
    commands.entity(entity).insert(ModelAssets {
        model,
        motion,
        scene,
        root: None,
        retrying_model_dependencies,
        retrying_motion_dependencies,
    });
    states.write(Notification::state(id, &AvatarState::Loading));
}

/// A reload is asynchronous. Wait for the owner asset to be written again
/// before treating its old dependency failure as final.
pub(super) fn finish_retries(
    mut actors: Query<&mut ModelAssets>,
    mut models: MessageReader<AssetEvent<Gltf>>,
    mut motions: MessageReader<AssetEvent<AnimationClip>>,
) {
    let models_modified = models
        .read()
        .filter_map(|event| match event {
            AssetEvent::Modified { id } => Some(*id),
            _ => None,
        })
        .collect::<Vec<_>>();
    let motions_modified = motions
        .read()
        .filter_map(|event| match event {
            AssetEvent::Modified { id } => Some(*id),
            _ => None,
        })
        .collect::<Vec<_>>();
    for mut assets in &mut actors {
        if assets.retrying_model_dependencies && models_modified.contains(&assets.model.id()) {
            assets.retrying_model_dependencies = false;
        }
        if assets.retrying_motion_dependencies
            && assets
                .motion
                .as_ref()
                .is_some_and(|handle| motions_modified.contains(&handle.id()))
        {
            assets.retrying_motion_dependencies = false;
        }
    }
}

/// One-time scene setup. The scene spawns asynchronously, so this keeps
/// checking until the instance exists, then binds its root and player.
#[allow(clippy::too_many_arguments)]
pub(super) fn load_scene(
    mut commands: Commands,
    mut actors: Query<(&mut ModelAssets, &AvatarState)>,
    gltfs: Res<Assets<Gltf>>,
    worlds: Res<Assets<WorldAsset>>,
    server: Res<AssetServer>,
    scenes: Query<Has<WorldAssetRoot>>,
    children: Query<&Children>,
    rigs: Query<(), With<Vrm>>,
    graphs_present: Query<(), With<AnimationGraphHandle>>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
) {
    for (mut assets, state) in &mut actors {
        if *state != AvatarState::Loading {
            continue;
        }
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
        if assets.root.is_none_or(|root| !rigs.contains(root)) {
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

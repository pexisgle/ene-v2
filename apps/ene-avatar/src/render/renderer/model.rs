use super::*;

impl Renderer {
    pub(super) fn set_model(&mut self, settings: &AvatarConfig) -> Result<(), String> {
        let path = local_path(&settings.model).map_err(|error| {
            format!("VRMを読めません: {error}。設定でモデルを選び直してください。")
        })?;
        let credit = crate::render::model::validate(&Path::new("/").join(&path))
            .map_err(|error| error.to_string())?;
        // Validate before removing the current model. A bad selection leaves
        // the existing model usable and can be corrected through SetAvatarConfig.
        let asset_path = AssetPath::from(path);
        let server = self.app.world().resource::<AssetServer>();
        if self.model_failed && self.model_path == settings.model {
            server.reload(asset_path.clone());
        }
        let handle = server.load(asset_path);
        self.set_motion(None);
        let world = self.app.world_mut();
        world.despawn(self.model);
        self.model = world.spawn(VrmHandle(handle.clone())).id();
        self.handle = handle;
        self.model_path = settings.model.clone();
        self.model_failed = false;
        self.credit = credit;
        self.center = None;
        self.distance = 0.;
        self.body_height = 0.;
        self.fitted = false;
        self.set_motion(settings.motion.as_deref());
        Ok(())
    }
    pub fn ready(&mut self, avatar_height: f32, viewport_height: u32) -> bool {
        if self.model_failed {
            return false;
        }
        let world = self.app.world_mut();
        let initialized = world.get::<Initialized>(self.model).is_some();
        if initialized && !self.fitted {
            use bevy::camera::primitives::MeshAabb;
            let mut minimum = Vec3::splat(f32::INFINITY);
            let mut maximum = Vec3::splat(f32::NEG_INFINITY);
            let mut query =
                world.query::<(&Mesh3d, &GlobalTransform, Option<&RestGlobalTransform>)>();
            let meshes = world.resource::<Assets<Mesh>>();
            // Frame the undeformed mesh once. Conservative skinned bounds
            // remain enabled for culling, but must not shrink the displayed body.
            for (mesh, global, rest) in query.iter(world) {
                let Some(bounds) = meshes.get(&mesh.0).and_then(MeshAabb::compute_aabb) else {
                    continue;
                };
                let transform = rest.map(|rest| &rest.0).unwrap_or(global);
                let center: Vec3 = bounds.center.into();
                let half: Vec3 = bounds.half_extents.into();
                for x in [-1., 1.] {
                    for y in [-1., 1.] {
                        for z in [-1., 1.] {
                            let corner =
                                transform.transform_point(center + half * Vec3::new(x, y, z));
                            minimum = minimum.min(corner);
                            maximum = maximum.max(corner);
                        }
                    }
                }
            }
            if minimum.is_finite() && maximum.is_finite() {
                // Fix the camera's axis to the rest pose. Never chase the
                // current silhouette or move the model to its bounding center.
                let anchor = world
                    .get::<HipsBoneEntity>(self.model)
                    .and_then(|hips| world.get::<RestGlobalTransform>(**hips))
                    .map(|rest| rest.translation())
                    .unwrap_or((minimum + maximum) * 0.5);
                self.center = Some(anchor);
                let half = (maximum - anchor).abs().max((minimum - anchor).abs());
                self.distance = (half.length() * 3.).max(1.);
                self.body_height = (maximum.y - minimum.y).max(0.01);
                self.fitted = true;
            }
        }
        if self.fitted
            && let Ok((mut projection, mut transform)) = world
                .query_filtered::<(&mut Projection, &mut Transform), With<AvatarCamera>>()
                .single_mut(world)
            && let Projection::Perspective(p) = &mut *projection
        {
            let fov = 2.
                * (self.body_height * viewport_height as f32
                    / (2. * avatar_height * self.distance))
                    .atan();
            let target = self.center.unwrap();
            let translation = target + Vec3::Z * self.distance;
            if (p.fov - fov).abs() > 0.00001 || transform.translation != translation {
                p.fov = fov;
                *transform = Transform::from_translation(translation).looking_at(target, Vec3::Y);
                self.generation += 1;
            }
        }
        initialized && self.fitted
    }
    pub fn load_error(&mut self) -> Option<String> {
        if self.model_failed {
            return None;
        }
        match self
            .app
            .world()
            .resource::<AssetServer>()
            .get_load_state(self.handle.id())
        {
            Some(LoadState::Failed(error)) => {
                self.model_failed = true;
                Some(format!(
                    "VRMを読み込めません: {error}。設定でモデルを選び直してください。"
                ))
            }
            _ => None,
        }
    }
}

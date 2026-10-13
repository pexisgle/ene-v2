use super::*;

impl Renderer {
    pub fn set_motion(&mut self, path: Option<&str>) {
        let retry = self.motion_failed && self.motion_path.as_deref() == path;
        self.motion_path = path.map(str::to_owned);
        let world = self.app.world_mut();
        if let Some((_, entity)) = self.motion.take() {
            // ChildOf uses linked despawning: the old skeleton is removed too.
            world.despawn(entity);
        }
        let players = world
            .query::<(
                Entity,
                &mut AnimationPlayer,
                Option<&mut AnimationTransitions>,
            )>()
            .iter_mut(world)
            .map(|(entity, mut player, transitions)| {
                player.stop_all();
                if let Some(mut transitions) = transitions {
                    *transitions = AnimationTransitions::default();
                }
                entity
            })
            .collect::<Vec<_>>();
        for entity in players {
            world.entity_mut(entity).remove::<AnimationGraphHandle>();
        }
        if let Some(hips) = world.get::<HipsBoneEntity>(self.model).map(|hips| **hips) {
            world
                .entity_mut(hips)
                .remove::<crate::render::motion::MotionAxes>();
        }
        for (rest, mut pose) in world
            .query::<(&RestTransform, &mut Transform)>()
            .iter_mut(world)
        {
            *pose = rest.0;
        }
        let expressions = world
            .get::<ExpressionEntityMap>(self.model)
            .map(|map| map.0.values().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        for expression in expressions {
            if let Some(mut pose) = world.get_mut::<Transform>(expression) {
                pose.translation.x = 0.;
            }
        }
        world.resource_mut::<Time<Virtual>>().unpause();
        self.motion_started = false;
        self.motion_failed = false;
        self.motion_started_at = std::time::Instant::now();
        self.motion_state = MotionState::None;
        // Reject readbacks of the previous pose; retain the displayed frame and
        // its input region until a matching new frame is available.
        self.generation += 1;
        let Some(path) = path else { return };
        let result = (|| -> Result<_, Box<dyn std::error::Error>> {
            let path = local_path(path)?;
            crate::render::motion::validate(&path)?;
            Ok(path)
        })();
        match result {
            Ok(path) => {
                let asset_path = AssetPath::from(path);
                let server = world.resource::<AssetServer>();
                if retry {
                    server.reload(asset_path.clone());
                }
                let handle = server.load_builder().override_unapproved().load(asset_path);
                let entity = world
                    .spawn((VrmaHandle(handle.clone()), ChildOf(self.model)))
                    .id();
                self.motion = Some((handle, entity));
                self.motion_state = MotionState::Loading;
            }
            Err(error) => {
                self.motion_failed = true;
                self.motion_state = MotionState::Failed {
                    message: format!("モーションを読めません: {error}"),
                };
            }
        }
    }
    pub fn resume(&mut self) {
        // Drop the wall-clock gap so hidden time never advances animation or
        // spring physics when updates start again.
        let world = self.app.world_mut();
        if let Some(receiver) = world.get_resource::<bevy::time::TimeReceiver>() {
            for _ in receiver.0.try_iter() {}
        }
        world
            .resource_mut::<Time<Real>>()
            .update_with_instant(bevy::platform::time::Instant::now());
        self.motion_started_at = std::time::Instant::now();
        self.generation += 1;
    }
    pub fn update_motion(&mut self, settings: &AvatarConfig) {
        if self.model_failed {
            return;
        }
        let Some((handle, entity)) = self.motion.as_ref() else {
            return;
        };
        if self.motion_failed {
            return;
        }
        let world = self.app.world_mut();
        if !self.motion_started {
            if let Some(LoadState::Failed(error)) =
                world.resource::<AssetServer>().get_load_state(handle.id())
            {
                self.motion_state = MotionState::Failed {
                    message: format!("VRMAを読めません: {error}"),
                };
                self.motion_failed = true;
                return;
            }
            if self.motion_started_at.elapsed() > std::time::Duration::from_secs(90) {
                self.motion_state = MotionState::Failed {
                    message: "モーションの読み込みが90秒以内に完了しませんでした。".into(),
                };
                self.motion_failed = true;
                return;
            }
            if self.fitted && world.get::<MotionLoaded>(*entity).is_some() {
                let Some((hips, axes)) =
                    crate::render::motion::MotionAxes::from_world(world, self.model, *entity)
                else {
                    self.motion_state = MotionState::Failed {
                        message: "モーションの骨格の基準を取得できません。".into(),
                    };
                    self.motion_failed = true;
                    return;
                };
                world.entity_mut(hips).insert(axes);
                world.trigger(PlayVrma {
                    vrma: *entity,
                    repeat: if settings.motion_loop {
                        bevy::animation::RepeatAnimation::Forever
                    } else {
                        bevy::animation::RepeatAnimation::Never
                    },
                    transition_duration: std::time::Duration::ZERO,
                    reset_spring_bones: true,
                });
                self.motion_started = true;
                self.generation += 1;
            }
        }
        if self.motion_started {
            if settings.motion_playing
                && self.motion_state == MotionState::Paused
                && world
                    .query::<&AnimationPlayer>()
                    .iter(world)
                    .all(AnimationPlayer::all_finished)
            {
                world.trigger(PlayVrma {
                    vrma: *entity,
                    repeat: if settings.motion_loop {
                        bevy::animation::RepeatAnimation::Forever
                    } else {
                        bevy::animation::RepeatAnimation::Never
                    },
                    transition_duration: std::time::Duration::ZERO,
                    reset_spring_bones: true,
                });
                self.generation += 1;
            }
            for mut player in world.query::<&mut AnimationPlayer>().iter_mut(world) {
                for (_, animation) in player.playing_animations_mut() {
                    if settings.motion_loop {
                        animation.set_repeat(bevy::animation::RepeatAnimation::Forever);
                    } else if animation.repeat_mode() == bevy::animation::RepeatAnimation::Forever {
                        animation.set_repeat(bevy::animation::RepeatAnimation::Count(
                            animation.completions().saturating_add(1),
                        ));
                    }
                }
                if settings.motion_playing {
                    player.resume_all();
                } else {
                    player.pause_all();
                }
            }
            // Pausing also freezes spring physics, keeping the visible pose
            // and the corresponding click silhouette stable.
            if settings.motion_playing {
                world.resource_mut::<Time<Virtual>>().unpause();
            } else {
                world.resource_mut::<Time<Virtual>>().pause();
            }
            let previous_state = self.motion_state.clone();
            self.motion_state = if !settings.motion_playing {
                MotionState::Paused
            } else if world
                .query::<&AnimationPlayer>()
                .iter(world)
                .all(AnimationPlayer::all_finished)
            {
                MotionState::Finished
            } else {
                MotionState::Playing
            };
            if (previous_state == MotionState::Paused) != (self.motion_state == MotionState::Paused)
            {
                self.generation += 1;
            }
        }
    }
}

use crate::{
    VrmSystems, animation,
    loader::{invalid, require_node},
    material::MToonMaterial,
    schema::{self, Expression, Expressions},
};
use bevy::{
    animation::AnimatedBy, ecs::entity::MapEntities, mesh::morph::MorphWeights,
    platform::collections::HashMap, prelude::*,
};

mod material;
use material::{MaterialBinds, apply_material, initialize_materials};

/// VRM expression identifier. Preset and custom names are distinct namespaces.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ExpressionName {
    Preset(schema::ExpressionPresetName),
    Custom(String),
}

/// Explicit manual weights override animation. Remove a key to restore animation.
#[derive(Component, Clone, Default, Reflect, Deref, DerefMut)]
#[reflect(Component, Clone, opaque)]
pub struct ExpressionWeights(pub HashMap<ExpressionName, f32>);

#[derive(Clone, MapEntities)]
pub(crate) struct ExpressionBinding {
    pub(crate) name: ExpressionName,
    pub(crate) definition: Expression,
    #[entities]
    pub(crate) target: Entity,
    pub(crate) category: Option<usize>,
}

#[derive(Clone, MapEntities)]
pub(crate) struct MorphTarget {
    #[entities]
    pub(crate) entity: Entity,
    // Precomputed sparse expression contributions for each bound morph index.
    pub(crate) indices: Vec<(usize, Vec<(usize, f32)>)>,
}

#[derive(Component, Clone, Reflect)]
#[reflect(Component, Clone, opaque)]
pub(crate) struct Binds {
    #[entities]
    pub(crate) bindings: Vec<ExpressionBinding>,
    #[entities]
    pub(crate) morphs: Vec<MorphTarget>,
}

impl Binds {
    pub(crate) fn target(&self, name: &ExpressionName) -> Option<Entity> {
        self.bindings
            .iter()
            .find(|binding| &binding.name == name)
            .map(|binding| binding.target)
    }

    fn materials(&self) -> impl Iterator<Item = usize> + '_ {
        self.bindings.iter().flat_map(|binding| {
            binding
                .definition
                .material_color_binds
                .iter()
                .flatten()
                .map(|bind| schema::index(bind.material))
                .chain(
                    binding
                        .definition
                        .texture_transform_binds
                        .iter()
                        .flatten()
                        .map(|bind| schema::index(bind.material)),
                )
        })
    }
}

pub(crate) fn weight<'a>(
    manual: Option<&'a ExpressionWeights>,
    transforms: impl Fn(Entity) -> Option<f32> + 'a,
) -> impl Fn(&ExpressionBinding) -> f32 + 'a {
    move |binding| {
        manual
            .and_then(|weights| weights.get(&binding.name).copied())
            .or_else(|| transforms(binding.target))
            .unwrap_or(0.)
    }
}

fn expressions(definitions: &Expressions) -> impl Iterator<Item = (ExpressionName, &Expression)> {
    let preset = definitions
        .preset
        .iter()
        .flat_map(|preset| preset.0.iter())
        .map(|(name, expression)| (ExpressionName::Preset(*name), expression));
    let custom = definitions
        .custom
        .iter()
        .flat_map(|custom| custom.iter())
        .map(|(name, expression)| (ExpressionName::Custom(name.clone()), expression));
    preset.chain(custom)
}

fn category(name: &ExpressionName) -> Option<usize> {
    let ExpressionName::Preset(name) = name else {
        return None;
    };
    match name {
        schema::ExpressionPresetName::Aa
        | schema::ExpressionPresetName::Ih
        | schema::ExpressionPresetName::Ou
        | schema::ExpressionPresetName::Ee
        | schema::ExpressionPresetName::Oh => Some(0),
        schema::ExpressionPresetName::Blink
        | schema::ExpressionPresetName::BlinkLeft
        | schema::ExpressionPresetName::BlinkRight => Some(1),
        schema::ExpressionPresetName::LookUp
        | schema::ExpressionPresetName::LookDown
        | schema::ExpressionPresetName::LookLeft
        | schema::ExpressionPresetName::LookRight => Some(2),
        _ => None,
    }
}

fn validate_expression(expression: &Expression) -> std::io::Result<()> {
    for bind in expression.material_color_binds.iter().flatten() {
        if bind.target_value.len() != 4
            || bind
                .target_value
                .iter()
                .any(|value| !(*value as f32).is_finite())
        {
            return Err(invalid("VRM color bind requires 4 finite components"));
        }
    }
    for bind in expression.texture_transform_binds.iter().flatten() {
        for values in [bind.scale.as_deref(), bind.offset.as_deref()]
            .into_iter()
            .flatten()
        {
            if values.len() != 2 || values.iter().any(|value| !(*value as f32).is_finite()) {
                return Err(invalid("VRM texture bind requires 2 finite components"));
            }
        }
    }
    Ok(())
}

pub(crate) fn attach(
    world: &mut World,
    root: Entity,
    definitions: Option<&Expressions>,
    nodes: &HashMap<usize, Entity>,
) -> std::io::Result<()> {
    let mut bindings = Vec::new();
    let mut morphs: HashMap<usize, MorphTarget> = HashMap::default();
    if let Some(definitions) = definitions {
        for (name, expression) in expressions(definitions) {
            validate_expression(expression)?;
            let target = world
                .spawn((
                    Transform::default(),
                    animation::expression_target(&name),
                    AnimatedBy(root),
                    ChildOf(root),
                ))
                .id();
            for bind in expression.morph_target_binds.iter().flatten() {
                let node = schema::index(bind.node);
                let entity = require_node(nodes, node)?;
                let exists = world
                    .get::<MorphWeights>(entity)
                    .is_some_and(|weights| weights.weights().get(bind.index).is_some());
                if !exists {
                    return Err(invalid("VRM morph bind index out of range"));
                }
                let weight = bind.weight as f32;
                if !weight.is_finite() || !(0. ..=1.).contains(&weight) {
                    return Err(invalid("VRM morph bind weight out of range"));
                }
                let morph = morphs.entry(node).or_insert_with(|| MorphTarget {
                    entity,
                    indices: Vec::new(),
                });
                if let Some((_, contributions)) = morph
                    .indices
                    .iter_mut()
                    .find(|(index, _)| *index == bind.index)
                {
                    contributions.push((bindings.len(), weight));
                } else {
                    morph
                        .indices
                        .push((bind.index, vec![(bindings.len(), weight)]));
                }
            }
            let category = category(&name);
            bindings.push(ExpressionBinding {
                name,
                definition: expression.clone(),
                target,
                category,
            });
        }
    }
    world.entity_mut(root).insert((
        ExpressionWeights::default(),
        Binds {
            bindings,
            morphs: morphs.into_values().collect(),
        },
    ));
    Ok(())
}

pub(crate) fn setup(app: &mut App) {
    app.register_type::<ExpressionWeights>()
        .register_type::<Binds>()
        .register_type::<MaterialBinds<StandardMaterial>>()
        .register_type::<MaterialBinds<MToonMaterial>>()
        .add_systems(
            PostUpdate,
            (
                initialize_materials::<StandardMaterial>,
                initialize_materials::<MToonMaterial>,
                apply,
            )
                .chain()
                .in_set(VrmSystems::Expressions),
        );
}

fn binding_weight(binding: &ExpressionBinding, raw: &impl Fn(&ExpressionBinding) -> f32) -> f32 {
    let value = raw(binding).clamp(0., 1.);
    if binding.definition.is_binary.unwrap_or(false) {
        f32::from(value > 0.5)
    } else {
        value
    }
}

fn override_rate(rule: Option<schema::ExpressionOverrideType>, weight: f32) -> f32 {
    match rule.unwrap_or(schema::ExpressionOverrideType::None) {
        schema::ExpressionOverrideType::None => 0.,
        schema::ExpressionOverrideType::Block => f32::from(weight > 0.),
        schema::ExpressionOverrideType::Blend => weight,
    }
}

fn effective<'a>(
    bindings: &'a [ExpressionBinding],
    raw: &impl Fn(&ExpressionBinding) -> f32,
) -> Vec<(&'a ExpressionBinding, f32)> {
    let mut weighted: Vec<_> = bindings
        .iter()
        .map(|binding| (binding, binding_weight(binding, raw)))
        .collect();
    let mut rates = [0f32; 3];
    for (binding, weight) in &weighted {
        for (index, (rate, rule)) in rates
            .iter_mut()
            .zip([
                binding.definition.override_mouth,
                binding.definition.override_blink,
                binding.definition.override_look_at,
            ])
            .enumerate()
        {
            if binding.category != Some(index) {
                *rate += override_rate(rule, *weight);
            }
        }
    }
    for (binding, weight) in &mut weighted {
        if let Some(category) = binding.category {
            let multiplier = 1. - rates[category].clamp(0., 1.);
            *weight = if binding.definition.is_binary.unwrap_or(false) && multiplier < 1. {
                0.
            } else {
                *weight * multiplier
            };
        }
    }
    weighted
}

pub(crate) fn look_at_scale(binds: &Binds, raw: impl Fn(&ExpressionBinding) -> f32) -> f32 {
    let mut rate = 0.;
    for binding in &binds.bindings {
        if binding.category != Some(2) {
            rate += override_rate(
                binding.definition.override_look_at,
                binding_weight(binding, &raw),
            );
        }
    }
    1. - rate.clamp(0., 1.)
}

#[allow(clippy::type_complexity)]
pub(crate) fn apply(
    roots: Query<(
        &ExpressionWeights,
        &Binds,
        Option<&MaterialBinds<StandardMaterial>>,
        Option<&MaterialBinds<MToonMaterial>>,
    )>,
    transforms: Query<&Transform>,
    mut morphs: Query<&mut MorphWeights>,
    mut standard: ResMut<Assets<StandardMaterial>>,
    mut toon: ResMut<Assets<MToonMaterial>>,
) {
    for (manual, binds, standard_binds, toon_binds) in &roots {
        let raw = weight(Some(manual), |entity| {
            transforms
                .get(entity)
                .ok()
                .map(|transform| transform.translation.x)
        });
        let effective = effective(&binds.bindings, &raw);
        for target in &binds.morphs {
            if let Ok(mut morph) = morphs.get_mut(target.entity) {
                for (index, contributions) in &target.indices {
                    let index = *index;
                    // Preserve bind order and duplicate contributions, including
                    // zero-weight expressions which reset their bound morphs.
                    let next = contributions.iter().fold(0., |sum, &(expression, weight)| {
                        sum + weight * effective[expression].1
                    });
                    if morph.weights().get(index) != Some(&next)
                        && let Some(weight) = morph.weights_mut().get_mut(index)
                    {
                        *weight = next;
                    }
                }
            }
        }
        apply_material(standard_binds, &mut standard, &effective);
        apply_material(toon_binds, &mut toon, &effective);
    }
}

#[cfg(test)]
mod tests {
    use super::material::{MaterialBinds, apply_material};
    use super::*;
    use crate::schema::UvTransform;
    use bevy::math::Affine2;

    fn bindings(definitions: &Expressions) -> Vec<ExpressionBinding> {
        expressions(definitions)
            .map(|(name, definition)| ExpressionBinding {
                category: category(&name),
                name,
                definition: definition.clone(),
                target: Entity::PLACEHOLDER,
            })
            .collect()
    }

    #[test]
    fn morph_contributions_accumulate_reset_and_preserve_unbound_weights() {
        let mut app = App::new();
        app.init_resource::<Assets<StandardMaterial>>()
            .init_resource::<Assets<MToonMaterial>>()
            .add_systems(Update, apply);
        let mesh = app
            .world_mut()
            .spawn(MorphWeights::new(vec![0.9, 0.7, 0.4], None).unwrap())
            .id();
        let root = app.world_mut().spawn_empty().id();
        let definitions = serde_json::from_value(serde_json::json!({
            "custom": {
                "a": {"morphTargetBinds":[
                    {"node":0,"index":0,"weight":0.5},
                    {"node":0,"index":0,"weight":0.25},
                    {"node":0,"index":1,"weight":1.0}
                ]},
                "b": {"morphTargetBinds":[{"node":0,"index":0,"weight":1.0}]}
            }
        }))
        .unwrap();
        attach(
            app.world_mut(),
            root,
            Some(&definitions),
            &HashMap::from([(0, mesh)]),
        )
        .unwrap();
        {
            let mut weights = app.world_mut().get_mut::<ExpressionWeights>(root).unwrap();
            weights.insert(ExpressionName::Custom("a".into()), 0.8);
            weights.insert(ExpressionName::Custom("b".into()), 0.6);
        }
        app.update();
        let weights = app.world().get::<MorphWeights>(mesh).unwrap().weights();
        assert!(
            (weights[0] - 1.2).abs() < 1e-6,
            "duplicate and multiple expression binds accumulate without clamping"
        );
        assert!((weights[1] - 0.8).abs() < 1e-6);
        assert_eq!(weights[2], 0.4);
        app.world_mut()
            .get_mut::<ExpressionWeights>(root)
            .unwrap()
            .clear();
        app.update();
        assert_eq!(
            app.world().get::<MorphWeights>(mesh).unwrap().weights(),
            &[0., 0., 0.4]
        );
    }

    #[test]
    fn expressions_leave_unbound_material_fields_alone() {
        let definitions: Expressions = serde_json::from_value(serde_json::json!({
            "custom": {
                "slide": {
                    "textureTransformBinds": [
                        {"material":0,"scale":[2.,2.],"offset":[0.5,0.]}
                    ]
                }
            }
        }))
        .unwrap();
        let bindings = bindings(&definitions);
        let weights = effective(&bindings, &|_| 1.);
        let toon = schema::ToonParams {
            shade_color_factor: Vec3::X,
            matcap_factor: Vec3::Y,
            ..default()
        };
        let extension = crate::material::MToonExtension {
            params: toon,
            shade: None,
            shift: None,
            rim: None,
            matcap: None,
            uv_mask: None,
            outline_width: None,
            outline: None,
            rest_uv: [UvTransform::default(); 6],
        };
        let original = MToonMaterial {
            base: StandardMaterial::default(),
            extension: extension.clone(),
        };
        let mut instance = original.clone();
        instance.base.base_color = Color::srgb(1., 0., 0.);
        instance.base.emissive = LinearRgba::rgb(0.1, 0.2, 0.3);
        instance.extension.params.shade_color_factor = Vec3::Z;
        instance.extension.params.matcap_factor = Vec3::new(0.3, 0.4, 0.5);

        let mut assets = Assets::<MToonMaterial>::default();
        let original = assets.add(original);
        let instance = assets.add(instance);
        let binds = MaterialBinds(HashMap::from([(
            (0, original),
            (instance.clone(), UvTransform::default(), 1.),
        )]));
        apply_material(Some(&binds), &mut assets, &weights);

        let result = assets.get(&instance).unwrap();
        assert_eq!(
            result.base.base_color,
            Color::srgb(1., 0., 0.),
            "a UV-only bind must not restore the base color"
        );
        assert_eq!(
            result.base.emissive,
            LinearRgba::rgb(0.1, 0.2, 0.3),
            "a UV-only bind must not restore emission"
        );
        assert_eq!(
            result.extension.params.shade_color_factor,
            Vec3::Z,
            "a UV-only bind must not restore MToon parameters"
        );
        assert_eq!(
            result.extension.params.matcap_factor,
            Vec3::new(0.3, 0.4, 0.5)
        );
        let expected =
            Affine2::from_scale_angle_translation(Vec2::splat(2.), 0., Vec2::new(0.5, 0.));
        assert!(
            result
                .base
                .uv_transform
                .matrix2
                .abs_diff_eq(expected.matrix2, 1e-5)
        );
        assert!(
            result
                .base
                .uv_transform
                .translation
                .abs_diff_eq(expected.translation, 1e-5)
        );
        assert_eq!(result.extension.params.uv_transforms, [expected.into(); 6]);
    }
}

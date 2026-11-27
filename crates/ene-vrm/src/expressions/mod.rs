use crate::{
    VrmSystems, animation,
    loader::{invalid, require_node},
    material::MToonMaterial,
    schema::{self, Expression, Expressions},
};
use bevy::{
    animation::AnimatedBy,
    ecs::entity::MapEntities,
    mesh::morph::MorphWeights,
    platform::collections::{HashMap, HashSet},
    prelude::*,
};

mod material;
use material::{MaterialBinds, apply_material, initialize_materials};

/// VRM expression identifier. Preset and custom names are distinct namespaces.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ExpressionName {
    Preset(schema::ExpressionPresetName),
    Custom(String),
}

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
    material_slots: HashSet<usize>,
}

impl Binds {
    pub(crate) fn target(&self, name: &ExpressionName) -> Option<Entity> {
        self.bindings
            .iter()
            .find(|binding| &binding.name == name)
            .map(|binding| binding.target)
    }
}

fn material_slots(bindings: &[ExpressionBinding]) -> HashSet<usize> {
    bindings
        .iter()
        .flat_map(|binding| {
            let expression = &binding.definition;
            expression
                .material_color_binds
                .iter()
                .flatten()
                .map(|bind| schema::index(bind.material))
                .chain(
                    expression
                        .texture_transform_binds
                        .iter()
                        .flatten()
                        .map(|bind| schema::index(bind.material)),
                )
        })
        .collect()
}

/// Expression weights a client sets directly, one entry per expression name.
///
/// The value is applied after the animation reset and before the morphs are
/// written, so it needs no animation clip and no clip can wipe it. Names are VRM
/// 1.0 preset names in lower case (`happy`, `aa`, ...), with `_` and `-` ignored,
/// or a custom expression name. A name the model does not declare is ignored.
#[derive(Component, Clone, Debug, Default, PartialEq, Reflect)]
#[reflect(Component, Clone)]
pub struct ExpressionWeights(pub Vec<(String, f32)>);

/// Maps a client-facing expression name into the model's namespace.
fn expression_name(name: &str) -> ExpressionName {
    let normalized: String = name
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect();
    let preset = match normalized.as_str() {
        "aa" => Some(schema::ExpressionPresetName::Aa),
        "angry" => Some(schema::ExpressionPresetName::Angry),
        "blink" => Some(schema::ExpressionPresetName::Blink),
        "blinkleft" => Some(schema::ExpressionPresetName::BlinkLeft),
        "blinkright" => Some(schema::ExpressionPresetName::BlinkRight),
        "ee" => Some(schema::ExpressionPresetName::Ee),
        "happy" => Some(schema::ExpressionPresetName::Happy),
        "ih" => Some(schema::ExpressionPresetName::Ih),
        "lookdown" => Some(schema::ExpressionPresetName::LookDown),
        "lookleft" => Some(schema::ExpressionPresetName::LookLeft),
        "lookright" => Some(schema::ExpressionPresetName::LookRight),
        "lookup" => Some(schema::ExpressionPresetName::LookUp),
        "neutral" => Some(schema::ExpressionPresetName::Neutral),
        "oh" => Some(schema::ExpressionPresetName::Oh),
        "ou" => Some(schema::ExpressionPresetName::Ou),
        "relaxed" => Some(schema::ExpressionPresetName::Relaxed),
        "sad" => Some(schema::ExpressionPresetName::Sad),
        "surprised" => Some(schema::ExpressionPresetName::Surprised),
        _ => None,
    };
    match preset {
        Some(preset) => ExpressionName::Preset(preset),
        None => ExpressionName::Custom(name.to_owned()),
    }
}

fn apply_weights(
    roots: Query<(&Binds, &ExpressionWeights)>,
    mut inputs: Query<&mut animation::ExpressionInput>,
) {
    for (binds, weights) in &roots {
        for (name, weight) in &weights.0 {
            let Some(target) = binds.target(&expression_name(name)) else {
                continue;
            };
            if let Ok(mut input) = inputs.get_mut(target) {
                input.value = Some(weight.clamp(0., 1.));
            }
        }
    }
}

pub(crate) fn weight<'a>(
    inputs: impl Fn(Entity) -> Option<f32> + 'a,
) -> impl Fn(&ExpressionBinding) -> f32 + 'a {
    move |binding| inputs(binding.target).unwrap_or(0.)
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
                    animation::ExpressionInput::default(),
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
    world.entity_mut(root).insert(Binds {
        material_slots: material_slots(&bindings),
        bindings,
        morphs: morphs.into_values().collect(),
    });
    Ok(())
}

pub(crate) fn setup(app: &mut App) {
    material::observe_world_ready(app);
    app.register_type::<Binds>()
        .register_type::<MaterialBinds<StandardMaterial>>()
        .register_type::<MaterialBinds<MToonMaterial>>()
        .register_type::<ExpressionWeights>()
        .add_systems(
            PostUpdate,
            (
                initialize_materials::<StandardMaterial>,
                initialize_materials::<MToonMaterial>,
                // A client-set weight has to land after the animation reset and
                // before `apply` reads the inputs.
                apply_weights.before(apply),
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

type ExpressionRoots<'w, 's> = Query<
    'w,
    's,
    (
        &'static Binds,
        Option<&'static MaterialBinds<StandardMaterial>>,
        Option<&'static MaterialBinds<MToonMaterial>>,
    ),
>;

pub(crate) fn apply(
    roots: ExpressionRoots<'_, '_>,
    inputs: Query<&animation::ExpressionInput>,
    mut morphs: Query<&mut MorphWeights>,
    mut standard: ResMut<Assets<StandardMaterial>>,
    mut toon: ResMut<Assets<MToonMaterial>>,
) {
    for (binds, standard_binds, toon_binds) in &roots {
        let raw = weight(|entity| inputs.get(entity).ok().and_then(|input| input.value));
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

    /// Drives the animation channel the expression reads.
    fn set_weight(app: &mut App, root: Entity, name: &str, value: f32) {
        let target = app
            .world()
            .get::<Binds>(root)
            .and_then(|binds| binds.target(&ExpressionName::Custom(name.into())))
            .unwrap();
        app.world_mut()
            .get_mut::<animation::ExpressionInput>(target)
            .unwrap()
            .value = Some(value);
    }

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
        set_weight(&mut app, root, "a", 0.8);
        set_weight(&mut app, root, "b", 0.6);
        app.update();
        let weights = app.world().get::<MorphWeights>(mesh).unwrap().weights();
        assert!(
            (weights[0] - 1.2).abs() < 1e-6,
            "duplicate and multiple expression binds accumulate without clamping"
        );
        assert!((weights[1] - 0.8).abs() < 1e-6);
        assert_eq!(weights[2], 0.4);
        set_weight(&mut app, root, "a", 0.);
        set_weight(&mut app, root, "b", 0.);
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
    #[test]
    fn client_expression_names_map_to_presets_and_custom_names() {
        assert_eq!(
            expression_name("happy"),
            ExpressionName::Preset(schema::ExpressionPresetName::Happy)
        );
        // Clients spell these with underscores or different case.
        assert_eq!(
            expression_name("Blink_Left"),
            ExpressionName::Preset(schema::ExpressionPresetName::BlinkLeft)
        );
        assert_eq!(
            expression_name("AA"),
            ExpressionName::Preset(schema::ExpressionPresetName::Aa)
        );
        assert_eq!(
            expression_name("tint"),
            ExpressionName::Custom("tint".into())
        );
    }

    #[test]
    fn expression_weights_reach_the_bound_targets() {
        let mut app = App::new();
        app.init_resource::<Assets<StandardMaterial>>()
            .init_resource::<Assets<MToonMaterial>>()
            .add_systems(Update, apply_weights);
        let mesh = app
            .world_mut()
            .spawn(MorphWeights::new(vec![0.0, 0.0], None).unwrap())
            .id();
        let root = app
            .world_mut()
            .spawn(ExpressionWeights(vec![
                ("happy".into(), 0.7),
                ("tint".into(), 0.2),
                ("not-a-declared-expression".into(), 1.0),
            ]))
            .id();
        let definitions = serde_json::from_value(serde_json::json!({
            "preset": {
                "happy": {"morphTargetBinds":[{"node":0,"index":0,"weight":1.0}]}
            },
            "custom": {
                "tint": {"morphTargetBinds":[{"node":0,"index":1,"weight":1.0}]}
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
        app.update();
        let binds = app.world().get::<Binds>(root).unwrap();
        let happy = binds
            .target(&ExpressionName::Preset(schema::ExpressionPresetName::Happy))
            .unwrap();
        let tint = binds
            .target(&ExpressionName::Custom("tint".into()))
            .unwrap();
        assert_eq!(
            app.world()
                .get::<animation::ExpressionInput>(happy)
                .unwrap()
                .value,
            Some(0.7)
        );
        assert_eq!(
            app.world()
                .get::<animation::ExpressionInput>(tint)
                .unwrap()
                .value,
            Some(0.2)
        );
    }
}

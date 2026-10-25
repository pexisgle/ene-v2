use crate::{
    VrmSystems, animation,
    loader::{invalid, require_node},
    material::{BaseUv, EmissiveStrength, MToonMaterial},
    schema::{self, Expression, Expressions, MaterialColorType, UvTransform},
};
use bevy::{
    animation::AnimatedBy,
    ecs::entity::MapEntities,
    math::Affine2,
    mesh::morph::MorphWeights,
    platform::collections::{HashMap, hash_map::Entry},
    prelude::*,
};

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
    pub(crate) indices: Vec<usize>,
    pub(crate) node: usize,
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

    pub(crate) fn weight<'a>(
        &'a self,
        manual: Option<&'a ExpressionWeights>,
        transforms: impl Fn(Entity) -> Option<f32> + 'a,
    ) -> impl Fn(&ExpressionName) -> f32 + 'a {
        move |name| {
            manual
                .and_then(|weights| weights.get(name).copied())
                .or_else(|| self.target(name).and_then(&transforms))
                .unwrap_or(0.)
        }
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

/// A per-instance material clone with its authored base UV transform and
/// `KHR_materials_emissive_strength`.
type MaterialInstance<M> = (Handle<M>, UvTransform, f32);

#[derive(Component, Clone, Reflect)]
#[reflect(Component, Clone)]
pub(crate) struct MaterialBinds<M: bevy::pbr::Material>(
    HashMap<(usize, Handle<M>), MaterialInstance<M>>,
);

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
                    node,
                    indices: Vec::new(),
                });
                if !morph.indices.contains(&bind.index) {
                    morph.indices.push(bind.index);
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

/// Clone each affected material once per scene instance.
///
/// A root without `MaterialBinds` is retried every frame until every affected
/// material is available; only then are handles replaced. An empty map marks
/// roots whose expressions do not target materials.
#[allow(clippy::type_complexity)]
fn initialize_materials<M: bevy::pbr::Material + Clone>(
    mut commands: Commands,
    roots: Query<(Entity, &Binds), Without<MaterialBinds<M>>>,
    children: Query<&Children>,
    mut meshes: ParamSet<(
        Query<(
            &crate::material::MaterialSlot,
            &MeshMaterial3d<M>,
            Option<&BaseUv>,
            Option<&EmissiveStrength>,
        )>,
        Query<&mut MeshMaterial3d<M>>,
    )>,
    mut materials: ResMut<Assets<M>>,
) {
    for (root, binds) in &roots {
        let affected: std::collections::HashSet<_> = binds.materials().collect();
        let mut targets = Vec::new();
        let mut pending = false;
        {
            let meshes = meshes.p0();
            for entity in std::iter::once(root).chain(children.iter_descendants(root)) {
                let Ok((slot, handle, base_uv, emissive_strength)) = meshes.get(entity) else {
                    continue;
                };
                if !affected.contains(&slot.0) {
                    continue;
                }
                if !materials.contains(&handle.0) {
                    pending = true;
                    break;
                }
                let base_uv = base_uv.map_or_else(UvTransform::default, |uv| uv.0);
                let emissive_strength = emissive_strength.map_or(1., |strength| strength.0);
                targets.push((entity, slot.0, handle.0.clone(), base_uv, emissive_strength));
            }
        }
        if pending {
            continue;
        }
        let mut instances = HashMap::default();
        let mut handles = meshes.p1();
        for (entity, slot, original, base_uv, emissive_strength) in targets {
            let clone = match instances.entry((slot, original.clone())) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let material = materials.get(&original).cloned().expect("checked above");
                    entry.insert((materials.add(material), base_uv, emissive_strength))
                }
            };
            if let Ok(mut handle) = handles.get_mut(entity) {
                handle.0 = clone.0.clone();
            }
        }
        commands.entity(root).insert(MaterialBinds::<M>(instances));
    }
}

fn binding_weight(binding: &ExpressionBinding, raw: &impl Fn(&ExpressionName) -> f32) -> f32 {
    let value = raw(&binding.name).clamp(0., 1.);
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
    raw: &impl Fn(&ExpressionName) -> f32,
) -> Vec<(&'a ExpressionName, &'a Expression, f32)> {
    let weighted: Vec<_> = bindings
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
    weighted
        .into_iter()
        .map(|(binding, mut weight)| {
            if let Some(category) = binding.category {
                let multiplier = 1. - rates[category].clamp(0., 1.);
                weight = if binding.definition.is_binary.unwrap_or(false) && multiplier < 1. {
                    0.
                } else {
                    weight * multiplier
                };
            }
            (&binding.name, &binding.definition, weight)
        })
        .collect()
}

pub(crate) fn look_at_scale(binds: &Binds, raw: impl Fn(&ExpressionName) -> f32) -> f32 {
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

trait BindMaterial: bevy::pbr::Material + Clone {
    fn base(&self) -> (&StandardMaterial, Option<&schema::ToonParams>);
    fn base_mut(&mut self) -> (&mut StandardMaterial, Option<&mut schema::ToonParams>);
    fn uv_rest(&self) -> Option<[UvTransform; 6]> {
        None
    }
}
impl BindMaterial for StandardMaterial {
    fn base(&self) -> (&StandardMaterial, Option<&schema::ToonParams>) {
        (self, None)
    }
    fn base_mut(&mut self) -> (&mut StandardMaterial, Option<&mut schema::ToonParams>) {
        (self, None)
    }
}
impl BindMaterial for MToonMaterial {
    fn base(&self) -> (&StandardMaterial, Option<&schema::ToonParams>) {
        (&self.base, Some(&self.extension.params))
    }
    fn base_mut(&mut self) -> (&mut StandardMaterial, Option<&mut schema::ToonParams>) {
        (&mut self.base, Some(&mut self.extension.params))
    }
    fn uv_rest(&self) -> Option<[UvTransform; 6]> {
        Some(self.extension.rest_uv)
    }
}

fn apply_material<M: BindMaterial>(
    binds: Option<&MaterialBinds<M>>,
    assets: &mut Assets<M>,
    effective: &[(&ExpressionName, &Expression, f32)],
) {
    for (&(index, ref original), (handle, base_uv, emissive_strength)) in
        binds.into_iter().flat_map(|binds| &binds.0)
    {
        let Some((base_color, emissive, rest_toon, toon_uv)) =
            assets.get(original).map(|material| {
                let (base, toon) = material.base();
                (
                    base.base_color,
                    base.emissive,
                    toon.copied(),
                    material.uv_rest(),
                )
            })
        else {
            continue;
        };
        let Some(current) = assets.get(handle) else {
            continue;
        };
        let (cur_base, cur_toon) = current.base();
        let (color, emissive, uv, toon) = compute_material(
            (base_color, emissive),
            *emissive_strength,
            *base_uv,
            rest_toon.as_ref(),
            toon_uv,
            index,
            effective,
        );
        if (cur_base.base_color != color
            || cur_base.emissive != emissive
            || cur_base.uv_transform != uv
            || cur_toon.copied() != toon)
            && let Some(mut material) = assets.get_mut(handle)
        {
            let (base, extension) = material.base_mut();
            base.base_color = color;
            base.emissive = emissive;
            base.uv_transform = uv;
            if let (Some(dst), Some(src)) = (extension, toon) {
                *dst = src;
            }
        }
    }
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
        let raw = binds.weight(Some(manual), |entity| {
            transforms
                .get(entity)
                .ok()
                .map(|transform| transform.translation.x)
        });
        let effective = effective(&binds.bindings, &raw);
        for target in &binds.morphs {
            if let Ok(mut morph) = morphs.get_mut(target.entity) {
                for &index in &target.indices {
                    // VRM expressions set bound morphs to 0 and accumulate
                    // `bind.weight * expression weight` from there. The final
                    // value is not clamped.
                    let mut next = 0.;
                    for (_, expression, value) in &effective {
                        for bind in expression.morph_target_binds.iter().flatten() {
                            if schema::index(bind.node) == target.node && bind.index == index {
                                next += bind.weight as f32 * value;
                            }
                        }
                    }
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

/// `emissive` is the loaded value, which already includes
/// `KHR_materials_emissive_strength`; expression targets are raw
/// `emissiveFactor` values, so only the target is scaled again.
pub(crate) fn compute_material(
    (base_color, emissive): (Color, LinearRgba),
    emissive_strength: f32,
    base_uv: UvTransform,
    rest_toon: Option<&schema::ToonParams>,
    toon_uv: Option<[UvTransform; 6]>,
    index: usize,
    weights: &[(&ExpressionName, &Expression, f32)],
) -> (Color, LinearRgba, Affine2, Option<schema::ToonParams>) {
    let mut value = base_color.to_linear().to_vec4();
    let mut emission = emissive.to_vec3();
    let mut color_bound = false;
    let mut uv_bound = false;
    let mut uv_weight = 0.;
    let mut uv_scale = Vec2::ZERO;
    let mut uv_offset = Vec2::ZERO;
    let mut toon_params = rest_toon.copied();
    for (_, expression, weight) in weights {
        for bind in expression
            .material_color_binds
            .iter()
            .flatten()
            .filter(|bind| schema::index(bind.material) == index)
        {
            let Some(target) = vec4(&bind.target_value) else {
                continue;
            };
            match bind.r#type {
                MaterialColorType::Color => {
                    color_bound = true;
                    value += (target - base_color.to_linear().to_vec4()) * weight;
                }
                MaterialColorType::EmissionColor => {
                    emission +=
                        (target.truncate() * emissive_strength - emissive.to_vec3()) * weight;
                }
                other => {
                    if let (Some(params), Some(rest)) = (&mut toon_params, rest_toon) {
                        let (field, base) = match other {
                            MaterialColorType::ShadeColor => {
                                (&mut params.shade_color_factor, rest.shade_color_factor)
                            }
                            MaterialColorType::MatcapColor => {
                                (&mut params.matcap_factor, rest.matcap_factor)
                            }
                            MaterialColorType::RimColor => (
                                &mut params.parametric_rim_color_factor,
                                rest.parametric_rim_color_factor,
                            ),
                            _ => (&mut params.outline_color_factor, rest.outline_color_factor),
                        };
                        *field += (target.truncate() - base) * weight;
                    }
                }
            }
        }
        for bind in expression
            .texture_transform_binds
            .iter()
            .flatten()
            .filter(|bind| schema::index(bind.material) == index)
        {
            uv_bound = true;
            uv_weight += weight;
            uv_scale += vec2(bind.scale.as_deref()).unwrap_or(Vec2::ONE) * weight;
            uv_offset += vec2(bind.offset.as_deref()).unwrap_or(Vec2::ZERO) * weight;
        }
    }
    let mut uv = base_uv;
    if uv_bound {
        uv = blend_uv(base_uv, uv_weight, uv_scale, uv_offset);
        if let (Some(params), Some(rest)) = (&mut toon_params, toon_uv) {
            for (index, transform) in params.uv_transforms.iter_mut().enumerate() {
                *transform = blend_uv(rest[index], uv_weight, uv_scale, uv_offset)
                    .affine()
                    .into();
            }
        }
    }
    (
        if color_bound {
            Color::LinearRgba(LinearRgba::from_vec4(value))
        } else {
            base_color
        },
        LinearRgba::from_vec3(emission).with_alpha(emissive.alpha),
        uv.affine(),
        toon_params,
    )
}

fn vec2(value: Option<&[f64]>) -> Option<Vec2> {
    let value = value?;
    if value.len() != 2 {
        return None;
    }
    Some(Vec2::new(value[0] as f32, value[1] as f32))
}

fn vec4(value: &[f64]) -> Option<Vec4> {
    if value.len() != 4 {
        return None;
    }
    Some(Vec4::new(
        value[0] as f32,
        value[1] as f32,
        value[2] as f32,
        value[3] as f32,
    ))
}

/// Blends the authored scale and offset while keeping the authored rotation.
///
/// `KHR_texture_transform` allows zero and negative scales, so blending the
/// source parameters avoids decomposing a matrix that can be singular.
fn blend_uv(base: UvTransform, weight: f32, scale: Vec2, offset: Vec2) -> UvTransform {
    UvTransform {
        scale: base.scale * (1. - weight) + scale,
        rotation: base.rotation,
        offset: base.offset * (1. - weight) + offset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn texture_transform_binds_interpolate_from_the_base_uv_transform() {
        let definitions: Expressions = serde_json::from_value(serde_json::json!({
            "preset": {
                "happy": {
                    "textureTransformBinds": [
                        {"material":0,"scale":[2.,1.],"offset":[0.5,0.]}
                    ]
                }
            }
        }))
        .unwrap();
        let bindings = bindings(&definitions);
        let weights = effective(&bindings, &|_| 1.);
        let base = UvTransform {
            scale: Vec2::splat(3.),
            rotation: 0.4,
            offset: Vec2::new(0.5, 0.25),
        };
        let rest_toon = schema::ToonParams {
            uv_transforms: [base.affine().into(); 6],
            ..default()
        };
        let (_, _, uv, toon) = compute_material(
            (Color::WHITE, LinearRgba::BLACK),
            1.,
            base,
            Some(&rest_toon),
            Some([base; 6]),
            0,
            &weights,
        );
        let (scale, angle, offset) = uv.to_scale_angle_translation();
        assert!(scale.abs_diff_eq(Vec2::new(2., 1.), 1e-5));
        assert!((angle - 0.4).abs() < 1e-5);
        assert!(offset.abs_diff_eq(Vec2::new(0.5, 0.), 1e-5));
        let toon = toon.unwrap();
        let (scale, angle, offset) =
            Affine2::from_mat3(toon.uv_transforms[0]).to_scale_angle_translation();
        assert!(scale.abs_diff_eq(Vec2::new(2., 1.), 1e-5));
        assert!((angle - 0.4).abs() < 1e-5);
        assert!(offset.abs_diff_eq(Vec2::new(0.5, 0.), 1e-5));
    }

    #[test]
    fn texture_transform_binds_survive_a_zero_base_scale() {
        let definitions: Expressions = serde_json::from_value(serde_json::json!({
            "preset": {
                "happy": {
                    "textureTransformBinds": [
                        {"material":0,"scale":[1.,1.],"offset":[0.5,0.]}
                    ]
                }
            }
        }))
        .unwrap();
        let base = UvTransform {
            scale: Vec2::new(0., 1.),
            rotation: 0.4,
            offset: Vec2::new(0.25, 0.1),
        };
        let rest_toon = schema::ToonParams {
            uv_transforms: [base.affine().into(); 6],
            ..default()
        };
        let bindings = bindings(&definitions);
        let weights = effective(&bindings, &|_| 1.);
        let (_, _, uv, _) = compute_material(
            (Color::WHITE, LinearRgba::BLACK),
            1.,
            base,
            Some(&rest_toon),
            Some([base; 6]),
            0,
            &weights,
        );
        let expected = Affine2::from_scale_angle_translation(Vec2::ONE, 0.4, Vec2::new(0.5, 0.));
        assert!(uv.matrix2.abs_diff_eq(expected.matrix2, 1e-5));
        assert!(uv.translation.abs_diff_eq(expected.translation, 1e-5));

        let weights = effective(&bindings, &|_| 0.);
        let (_, _, uv, _) = compute_material(
            (Color::WHITE, LinearRgba::BLACK),
            1.,
            base,
            Some(&rest_toon),
            Some([base; 6]),
            0,
            &weights,
        );
        assert!(uv.matrix2.abs_diff_eq(base.affine().matrix2, 1e-5));
        assert!(uv.translation.abs_diff_eq(base.affine().translation, 1e-5));
    }

    #[test]
    fn texture_transform_binds_preserve_negative_and_zero_scales() {
        // A negative scale on the zero-length axis must not flip the matrix at
        // weight 0 (a signed zero determinant used to invert it).
        let base = UvTransform {
            scale: Vec2::new(-1., 0.),
            ..default()
        };
        let kept = blend_uv(base, 0., Vec2::ZERO, Vec2::ZERO);
        assert!(kept.scale.abs_diff_eq(base.scale, 1e-6));
        assert!(
            kept.affine()
                .matrix2
                .abs_diff_eq(base.affine().matrix2, 1e-6)
        );

        // Both axes zero still carry the authored rotation into the bind.
        let flat = UvTransform {
            scale: Vec2::ZERO,
            rotation: 0.4,
            offset: Vec2::ZERO,
        };
        let restored = blend_uv(flat, 1., Vec2::ONE, Vec2::ZERO);
        assert_eq!(restored.rotation, 0.4);
        let expected = Affine2::from_scale_angle_translation(Vec2::ONE, 0.4, Vec2::ZERO);
        assert!(
            restored
                .affine()
                .matrix2
                .abs_diff_eq(expected.matrix2, 1e-6)
        );
    }

    #[test]
    fn material_binds_and_overrides_follow_spec() {
        let definitions: Expressions = serde_json::from_value(serde_json::json!({
            "preset": {
                "happy": {
                    "overrideBlink": "blend",
                    "overrideMouth": "block",
                    "materialColorBinds": [
                        {"material":0,"type":"shadeColor","targetValue":[1.,0.,0.,1.]},
                        {"material":0,"type":"emissionColor","targetValue":[1.,0.5,0.,1.]}
                    ]
                },
                "blink": {
                    "isBinary": true,
                    "materialColorBinds": [{"material":0,"type":"shadeColor","targetValue":[0.,0.,1.,1.]}]
                }
            },
            "custom": {
                "smile": {}
            }
        }))
        .unwrap();
        let bindings = bindings(&definitions);
        let weights = effective(&bindings, &|name| match name {
            ExpressionName::Preset(schema::ExpressionPresetName::Happy) => 0.25,
            ExpressionName::Preset(schema::ExpressionPresetName::Blink) => 0.4,
            _ => 1.,
        });
        let weight = |name: schema::ExpressionPresetName| {
            weights
                .iter()
                .find(|(candidate, _, _)| **candidate == ExpressionName::Preset(name))
                .unwrap()
                .2
        };
        assert_eq!(weight(schema::ExpressionPresetName::Blink), 0.);
        assert_eq!(
            weights
                .iter()
                .find(|(name, _, _)| **name == ExpressionName::Custom("smile".into()))
                .unwrap()
                .2,
            1.
        );

        let weights = effective(&bindings, &|name| {
            if *name == ExpressionName::Preset(schema::ExpressionPresetName::Happy) {
                0.25
            } else {
                0.6
            }
        });
        assert_eq!(
            weights
                .iter()
                .find(|(name, _, _)| {
                    **name == ExpressionName::Preset(schema::ExpressionPresetName::Blink)
                })
                .unwrap()
                .2,
            0.
        );

        let weights = effective(&bindings, &|name| {
            if *name == ExpressionName::Preset(schema::ExpressionPresetName::Happy) {
                0.
            } else {
                0.6
            }
        });
        assert_eq!(
            weights
                .iter()
                .find(|(name, _, _)| {
                    **name == ExpressionName::Preset(schema::ExpressionPresetName::Blink)
                })
                .unwrap()
                .2,
            1.
        );

        let weights = effective(&bindings, &|name| {
            if *name == ExpressionName::Preset(schema::ExpressionPresetName::Happy) {
                0.25
            } else {
                0.
            }
        });
        let rest = StandardMaterial::default();
        let rest_toon = schema::ToonParams {
            shade_color_factor: Vec3::ONE,
            parametric_rim_color_factor: Vec3::splat(0.3),
            outline_width_factor: 0.2,
            ..default()
        };
        let (color, emission, uv, toon) = compute_material(
            (rest.base_color, rest.emissive),
            1.,
            UvTransform::default(),
            Some(&rest_toon),
            None,
            0,
            &weights,
        );
        let toon = toon.unwrap();
        assert_eq!(toon.shade_color_factor, Vec3::new(1., 0.75, 0.75));
        assert_eq!(toon.parametric_rim_color_factor, Vec3::splat(0.3));
        assert_eq!(toon.outline_width_factor, 0.2);
        assert_eq!(color, rest.base_color);
        assert_eq!(
            emission,
            LinearRgba::new(0.25, 0.125, 0., rest.emissive.alpha)
        );
        assert_eq!(uv, rest.uv_transform);
    }

    #[test]
    fn emission_color_binds_apply_the_emissive_strength_once() {
        let parse = |target: [f64; 4]| -> Expressions {
            serde_json::from_value(serde_json::json!({
                "custom": {
                    "glow": {"materialColorBinds": [
                        {"material":0,"type":"emissionColor","targetValue":target}
                    ]}
                }
            }))
            .unwrap()
        };
        let sample = |definitions: &Expressions, base: LinearRgba, strength: f32, weight: f32| {
            let bindings = bindings(definitions);
            let weights = effective(&bindings, &|_| weight);
            compute_material(
                (Color::WHITE, base),
                strength,
                UvTransform::default(),
                None,
                None,
                0,
                &weights,
            )
            .1
        };

        let target = parse([0.4, 0.2, 0.1, 1.]);
        // 0.2 / 0.1 / 0.05 already scaled by 10, as loaded by Bevy.
        let base = LinearRgba::rgb(2., 1., 0.5);
        assert_eq!(sample(&target, base, 10., 0.), base);
        assert_eq!(
            sample(&target, base, 10., 0.5),
            LinearRgba::rgb(3., 1.5, 0.75)
        );
        assert_eq!(sample(&target, base, 10., 1.), LinearRgba::rgb(4., 2., 1.));

        // Strength 1 keeps the plain glTF factor interpolation.
        assert_eq!(
            sample(&target, LinearRgba::rgb(0.2, 0.1, 0.05), 1., 1.),
            LinearRgba::rgb(0.4, 0.2, 0.1)
        );
        // Strength 0 suppresses emission even for a bright target.
        assert_eq!(
            sample(&target, LinearRgba::BLACK, 0., 1.),
            LinearRgba::BLACK
        );
        // A target equal to the initial factor must not change a scaled base.
        let same = parse([0.2, 0.1, 0.05, 1.]);
        assert_eq!(sample(&same, base, 10., 1.), base);
    }
}

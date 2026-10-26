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

/// Material fields an expression bind owns. Fields outside this set keep
/// whatever the application wrote to the instance material.
#[derive(Clone, Copy, Default)]
pub(crate) struct MaterialBounds {
    base_color: bool,
    emissive: bool,
    uv: bool,
    shade: bool,
    matcap: bool,
    rim: bool,
    outline: bool,
}

fn apply_material<M: BindMaterial>(
    binds: Option<&MaterialBinds<M>>,
    assets: &mut Assets<M>,
    effective: &[(&ExpressionBinding, f32)],
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
        let (color, emissive, uv, toon, bounds) = compute_material(
            (base_color, emissive),
            *emissive_strength,
            *base_uv,
            rest_toon.as_ref(),
            toon_uv,
            index,
            effective,
        );
        let toon_changed = match (cur_toon, toon.as_ref()) {
            (Some(cur), Some(next)) => {
                (bounds.shade && cur.shade_color_factor != next.shade_color_factor)
                    || (bounds.matcap && cur.matcap_factor != next.matcap_factor)
                    || (bounds.rim
                        && cur.parametric_rim_color_factor != next.parametric_rim_color_factor)
                    || (bounds.outline && cur.outline_color_factor != next.outline_color_factor)
                    || (bounds.uv && cur.uv_transforms != next.uv_transforms)
            }
            _ => false,
        };
        if ((bounds.base_color && cur_base.base_color != color)
            || (bounds.emissive && cur_base.emissive != emissive)
            || (bounds.uv && cur_base.uv_transform != uv)
            || toon_changed)
            && let Some(mut material) = assets.get_mut(handle)
        {
            let (base, extension) = material.base_mut();
            if bounds.base_color {
                base.base_color = color;
            }
            if bounds.emissive {
                base.emissive = emissive;
            }
            if bounds.uv {
                base.uv_transform = uv;
            }
            if let (Some(dst), Some(src)) = (extension, toon) {
                if bounds.shade {
                    dst.shade_color_factor = src.shade_color_factor;
                }
                if bounds.matcap {
                    dst.matcap_factor = src.matcap_factor;
                }
                if bounds.rim {
                    dst.parametric_rim_color_factor = src.parametric_rim_color_factor;
                }
                if bounds.outline {
                    dst.outline_color_factor = src.outline_color_factor;
                }
                if bounds.uv {
                    dst.uv_transforms = src.uv_transforms;
                }
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
        let raw = weight(Some(manual), |entity| {
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
                    for (binding, value) in &effective {
                        for bind in binding.definition.morph_target_binds.iter().flatten() {
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
    weights: &[(&ExpressionBinding, f32)],
) -> (
    Color,
    LinearRgba,
    Affine2,
    Option<schema::ToonParams>,
    MaterialBounds,
) {
    let mut value = base_color.to_linear().to_vec4();
    let mut emission = emissive.to_vec3();
    let mut bounds = MaterialBounds::default();
    let mut uv_weight = 0.;
    let mut uv_scale = Vec2::ZERO;
    let mut uv_offset = Vec2::ZERO;
    let mut toon_params = rest_toon.copied();
    for &(binding, weight) in weights {
        for bind in binding
            .definition
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
                    bounds.base_color = true;
                    value += (target - base_color.to_linear().to_vec4()) * weight;
                }
                MaterialColorType::EmissionColor => {
                    bounds.emissive = true;
                    emission +=
                        (target.truncate() * emissive_strength - emissive.to_vec3()) * weight;
                }
                other => {
                    if let (Some(params), Some(rest)) = (&mut toon_params, rest_toon) {
                        let (field, base, bound) = match other {
                            MaterialColorType::ShadeColor => (
                                &mut params.shade_color_factor,
                                rest.shade_color_factor,
                                &mut bounds.shade,
                            ),
                            MaterialColorType::MatcapColor => (
                                &mut params.matcap_factor,
                                rest.matcap_factor,
                                &mut bounds.matcap,
                            ),
                            MaterialColorType::RimColor => (
                                &mut params.parametric_rim_color_factor,
                                rest.parametric_rim_color_factor,
                                &mut bounds.rim,
                            ),
                            _ => (
                                &mut params.outline_color_factor,
                                rest.outline_color_factor,
                                &mut bounds.outline,
                            ),
                        };
                        *bound = true;
                        *field += (target.truncate() - base) * weight;
                    }
                }
            }
        }
        for bind in binding
            .definition
            .texture_transform_binds
            .iter()
            .flatten()
            .filter(|bind| schema::index(bind.material) == index)
        {
            bounds.uv = true;
            uv_weight += weight;
            uv_scale += vec2(bind.scale.as_deref()).unwrap_or(Vec2::ONE) * weight;
            uv_offset += vec2(bind.offset.as_deref()).unwrap_or(Vec2::ZERO) * weight;
        }
    }
    let mut uv = base_uv;
    if bounds.uv {
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
        if bounds.base_color {
            Color::LinearRgba(LinearRgba::from_vec4(value))
        } else {
            base_color
        },
        LinearRgba::from_vec3(emission).with_alpha(emissive.alpha),
        uv.affine(),
        toon_params,
        bounds,
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
        let (_, _, uv, toon, _) = compute_material(
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

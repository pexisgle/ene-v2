use crate::{
    VrmSystems, animation,
    loader::{invalid, require_node},
    material::{BaseUv, MToonMaterial},
    schema::{self, ColorType, UvTransform},
};
use bevy::{
    animation::AnimatedBy,
    ecs::entity::MapEntities,
    math::Affine2,
    mesh::morph::MorphWeights,
    platform::collections::{HashMap, hash_map::Entry},
    prelude::*,
};

/// Explicit manual weights override animation. Remove a key to restore animation.
#[derive(Component, Clone, Default, Reflect, Deref, DerefMut)]
#[reflect(Component, Clone)]
pub struct ExpressionWeights(pub HashMap<String, f32>);

#[derive(Clone, Reflect, MapEntities)]
pub(crate) struct MorphTarget {
    #[entities]
    pub entity: Entity,
    pub indices: Vec<usize>,
    pub node: usize,
}

#[derive(Component, Clone, Reflect)]
#[reflect(Component, Clone)]
pub(crate) struct Binds {
    pub definitions: std::sync::Arc<schema::Expressions>,
    #[entities]
    pub targets: Vec<Entity>,
    #[entities]
    pub morphs: Vec<MorphTarget>,
}

impl Binds {
    pub fn target(&self, name: &str) -> Option<Entity> {
        self.definitions
            .iter()
            .zip(&self.targets)
            .find(|((n, _), _)| *n == name)
            .map(|(_, &entity)| entity)
    }
    pub fn weight<'a>(
        &'a self,
        manual: Option<&'a ExpressionWeights>,
        transforms: impl Fn(Entity) -> Option<f32> + 'a,
    ) -> impl Fn(&str) -> f32 + 'a {
        move |name| {
            manual
                .and_then(|m| m.get(name).copied())
                .or_else(|| self.target(name).and_then(&transforms))
                .unwrap_or(0.)
        }
    }
}

#[derive(Component, Clone, Reflect)]
#[reflect(Component, Clone)]
pub(crate) struct MaterialBinds<M: bevy::pbr::Material>(
    HashMap<(usize, Handle<M>), (Handle<M>, UvTransform)>,
);

pub(crate) fn attach(
    world: &mut World,
    root: Entity,
    definitions: &schema::Expressions,
    nodes: &HashMap<usize, Entity>,
) -> std::io::Result<()> {
    let definitions = std::sync::Arc::new(definitions.clone());
    let targets = definitions
        .iter()
        .map(|(name, _)| {
            world
                .spawn((
                    Transform::default(),
                    animation::expression_target(name),
                    AnimatedBy(root),
                    ChildOf(root),
                ))
                .id()
        })
        .collect();
    let mut morphs: HashMap<usize, MorphTarget> = HashMap::default();
    for (_, expr) in definitions.iter() {
        for bind in &expr.morph_target_binds {
            let entity = require_node(nodes, bind.node)?;
            let exists = world
                .get::<MorphWeights>(entity)
                .is_some_and(|w| w.weights().get(bind.index).is_some());
            if !exists {
                return Err(invalid("VRM morph bind index out of range"));
            }
            if !bind.weight.is_finite() || !(0. ..=1.).contains(&bind.weight) {
                return Err(invalid("VRM morph bind weight out of range"));
            }
            let target = morphs.entry(bind.node).or_insert_with(|| MorphTarget {
                entity,
                node: bind.node,
                indices: Vec::new(),
            });
            if !target.indices.contains(&bind.index) {
                target.indices.push(bind.index);
            }
        }
    }
    world.entity_mut(root).insert((
        ExpressionWeights::default(),
        Binds {
            definitions,
            targets,
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
        )>,
        Query<&mut MeshMaterial3d<M>>,
    )>,
    mut materials: ResMut<Assets<M>>,
) {
    for (root, binds) in &roots {
        let affected: std::collections::HashSet<_> = binds.definitions.materials().collect();
        let mut targets = Vec::new();
        let mut pending = false;
        {
            let meshes = meshes.p0();
            for entity in std::iter::once(root).chain(children.iter_descendants(root)) {
                let Ok((slot, handle, base_uv)) = meshes.get(entity) else {
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
                targets.push((entity, slot.0, handle.0.clone(), base_uv));
            }
        }
        if pending {
            continue;
        }
        let mut instances = HashMap::default();
        let mut handles = meshes.p1();
        for (entity, slot, original, base_uv) in targets {
            let clone = match instances.entry((slot, original.clone())) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => {
                    let material = materials.get(&original).cloned().expect("checked above");
                    e.insert((materials.add(material), base_uv))
                }
            };
            if let Ok(mut handle) = handles.get_mut(entity) {
                handle.0 = clone.0.clone();
            }
        }
        commands.entity(root).insert(MaterialBinds::<M>(instances));
    }
}

fn category(name: &str) -> Option<usize> {
    match name {
        "aa" | "ih" | "ou" | "ee" | "oh" => Some(0),
        "blink" | "blinkLeft" | "blinkRight" => Some(1),
        "lookUp" | "lookDown" | "lookLeft" | "lookRight" => Some(2),
        _ => None,
    }
}

fn weight(name: &str, e: &schema::Expression, raw: impl Fn(&str) -> f32) -> f32 {
    let v = raw(name).clamp(0., 1.);
    if e.is_binary { f32::from(v > 0.5) } else { v }
}

#[allow(clippy::type_complexity)]
fn weights_and_rates(
    binds: &schema::Expressions,
    raw: impl Fn(&str) -> f32,
) -> (
    Vec<(&str, &schema::Expression, f32, Option<usize>)>,
    [f32; 3],
) {
    let weights: Vec<_> = binds
        .preset
        .iter()
        .map(|(n, e)| (n.as_str(), e, weight(n, e, &raw), category(n)))
        .chain(
            binds
                .custom
                .iter()
                .map(|(n, e)| (n.as_str(), e, weight(n, e, &raw), None)),
        )
        .collect();
    let mut rates = [0f32; 3];
    for &(_, e, w, cat) in &weights {
        for (i, (rate, rule)) in rates
            .iter_mut()
            .zip([e.override_mouth, e.override_blink, e.override_look_at])
            .enumerate()
        {
            if cat != Some(i) {
                *rate += rule.rate(w);
            }
        }
    }
    (weights, rates)
}

pub(crate) fn look_at_scale(binds: &schema::Expressions, raw: impl Fn(&str) -> f32) -> f32 {
    let (_, rates) = weights_and_rates(binds, raw);
    1. - rates[2].clamp(0., 1.)
}

fn effective(
    binds: &schema::Expressions,
    raw: impl Fn(&str) -> f32,
) -> Vec<(&str, &schema::Expression, f32)> {
    let (weights, rates) = weights_and_rates(binds, raw);
    weights
        .into_iter()
        .map(|(name, e, mut w, cat)| {
            if let Some(c) = cat {
                let m = 1. - rates[c].clamp(0., 1.);
                w *= if e.is_binary && m < 1. { 0. } else { m };
            }
            (name, e, w)
        })
        .collect()
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
    effective: &[(&str, &schema::Expression, f32)],
) {
    for (&(index, ref original), (handle, base_uv)) in binds.into_iter().flat_map(|b| &b.0) {
        let Some((base_color, emissive, rest_toon, toon_uv)) = assets.get(original).map(|m| {
            let (base, toon) = m.base();
            (base.base_color, base.emissive, toon.copied(), m.uv_rest())
        }) else {
            continue;
        };
        let Some(current) = assets.get(handle) else {
            continue;
        };
        let (cur_base, cur_toon) = current.base();
        let (color, emissive, uv, toon) = compute_material(
            (base_color, emissive),
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
            && let Some(mut mat) = assets.get_mut(handle)
        {
            let (base, ext) = mat.base_mut();
            base.base_color = color;
            base.emissive = emissive;
            base.uv_transform = uv;
            if let (Some(dst), Some(src)) = (ext, toon) {
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
        let effective = effective(
            &binds.definitions,
            binds.weight(Some(manual), |e| {
                transforms.get(e).ok().map(|t| t.translation.x)
            }),
        );
        for target in &binds.morphs {
            if let Ok(mut morph) = morphs.get_mut(target.entity) {
                for &index in &target.indices {
                    // VRM expressions set bound morphs to 0 and accumulate
                    // `bind.weight * expression weight` from there. The final
                    // value is not clamped.
                    let mut next = 0.;
                    for (_, expr, value) in &effective {
                        for bind in &expr.morph_target_binds {
                            if bind.node == target.node && bind.index == index {
                                next += bind.weight * value;
                            }
                        }
                    }
                    if morph.weights().get(index) != Some(&next)
                        && let Some(w) = morph.weights_mut().get_mut(index)
                    {
                        *w = next;
                    }
                }
            }
        }
        apply_material(standard_binds, &mut standard, &effective);
        apply_material(toon_binds, &mut toon, &effective);
    }
}

pub(crate) fn compute_material(
    (base_color, emissive): (Color, LinearRgba),
    base_uv: UvTransform,
    rest_toon: Option<&schema::ToonParams>,
    toon_uv: Option<[UvTransform; 6]>,
    index: usize,
    weights: &[(&str, &schema::Expression, f32)],
) -> (Color, LinearRgba, Affine2, Option<schema::ToonParams>) {
    let mut value = base_color.to_linear().to_vec4();
    let mut emission = emissive.to_vec3();
    let mut color_bound = false;
    let mut uv_bound = false;
    let mut uv_weight = 0.;
    let mut uv_scale = Vec2::ZERO;
    let mut uv_offset = Vec2::ZERO;
    let mut toon_params = rest_toon.copied();
    for (_, e, weight) in weights {
        for bind in e
            .material_color_binds
            .iter()
            .filter(|b| b.material == index)
        {
            let target = bind.target_value;
            match bind.r#type {
                ColorType::Color => {
                    color_bound = true;
                    value += (target - base_color.to_linear().to_vec4()) * weight;
                }
                ColorType::EmissionColor => {
                    emission += (target.truncate() - emissive.to_vec3()) * weight;
                }
                other => {
                    if let (Some(params), Some(rest)) = (&mut toon_params, rest_toon) {
                        let (field, base) = match other {
                            ColorType::ShadeColor => {
                                (&mut params.shade_color_factor, rest.shade_color_factor)
                            }
                            ColorType::MatcapColor => {
                                (&mut params.matcap_factor, rest.matcap_factor)
                            }
                            ColorType::RimColor => (
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
        for bind in e
            .texture_transform_binds
            .iter()
            .filter(|b| b.material == index)
        {
            uv_bound = true;
            uv_weight += weight;
            uv_scale += bind.scale * weight;
            uv_offset += bind.offset * weight;
        }
    }
    let mut uv = base_uv;
    if uv_bound {
        uv = blend_uv(base_uv, uv_weight, uv_scale, uv_offset);
        if let (Some(params), Some(rest)) = (&mut toon_params, toon_uv) {
            for (i, transform) in params.uv_transforms.iter_mut().enumerate() {
                *transform = blend_uv(rest[i], uv_weight, uv_scale, uv_offset)
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

    #[test]
    fn texture_transform_binds_interpolate_from_the_base_uv_transform() {
        let defs: schema::Expressions = serde_json::from_value(serde_json::json!({
            "preset": {
                "happy": {
                    "textureTransformBinds": [
                        {"material":0,"scale":[2.,1.],"offset":[0.5,0.]}
                    ]
                }
            }
        }))
        .unwrap();
        let weights = effective(&defs, |_| 1.);
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
        let defs: schema::Expressions = serde_json::from_value(serde_json::json!({
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
        let weights = effective(&defs, |_| 1.);
        let (_, _, uv, _) = compute_material(
            (Color::WHITE, LinearRgba::BLACK),
            base,
            Some(&rest_toon),
            Some([base; 6]),
            0,
            &weights,
        );
        let expected = Affine2::from_scale_angle_translation(Vec2::ONE, 0.4, Vec2::new(0.5, 0.));
        assert!(uv.matrix2.abs_diff_eq(expected.matrix2, 1e-5));
        assert!(uv.translation.abs_diff_eq(expected.translation, 1e-5));

        let weights = effective(&defs, |_| 0.);
        let (_, _, uv, _) = compute_material(
            (Color::WHITE, LinearRgba::BLACK),
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
        let defs: schema::Expressions = serde_json::from_value(serde_json::json!({
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
        })).unwrap();
        let weights = effective(&defs, |n| match n {
            "happy" => 0.25,
            "blink" => 0.4,
            _ => 1.,
        });
        assert_eq!(
            weights.iter().find(|(n, _, _)| *n == "blink").unwrap().2,
            0.
        );
        assert_eq!(
            weights.iter().find(|(n, _, _)| *n == "smile").unwrap().2,
            1.
        );

        let weights = effective(&defs, |n| if n == "happy" { 0.25 } else { 0.6 });
        assert_eq!(
            weights.iter().find(|(n, _, _)| *n == "blink").unwrap().2,
            0.
        );

        let weights = effective(&defs, |n| if n == "happy" { 0. } else { 0.6 });
        assert_eq!(
            weights.iter().find(|(n, _, _)| *n == "blink").unwrap().2,
            1.
        );

        let weights = effective(&defs, |n| if n == "happy" { 0.25 } else { 0. });
        let rest = StandardMaterial::default();
        let rest_toon = schema::ToonParams {
            shade_color_factor: Vec3::ONE,
            parametric_rim_color_factor: Vec3::splat(0.3),
            outline_width_factor: 0.2,
            ..default()
        };
        let (color, emission, uv, toon) = compute_material(
            (rest.base_color, rest.emissive),
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
}

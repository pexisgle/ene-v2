//! Material-side expression binds: per-instance clones, color/UV targets and bounds.
use super::{Binds, ExpressionBinding};
use crate::{
    material::{BaseUv, EmissiveStrength, MToonMaterial},
    schema::{self, MaterialColorType, UvTransform},
};
use bevy::{
    math::Affine2,
    platform::collections::{HashMap, hash_map::Entry},
    prelude::*,
};

/// A per-instance material clone with its authored base UV transform and
/// `KHR_materials_emissive_strength`.
pub(crate) type MaterialInstance<M> = (Handle<M>, UvTransform, f32);

#[derive(Component, Clone, Reflect)]
#[reflect(Component, Clone)]
pub(crate) struct MaterialBinds<M: bevy::pbr::Material>(
    pub(super) HashMap<(usize, Handle<M>), MaterialInstance<M>>,
);

/// Clone each affected material once per scene instance.
///
/// A root without `MaterialBinds` is retried every frame until every affected
/// material is available; only then are handles replaced. An empty map marks
/// roots whose expressions do not target materials.
#[allow(clippy::type_complexity)]
pub(super) fn initialize_materials<M: bevy::pbr::Material + Clone>(
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

pub(super) trait BindMaterial: bevy::pbr::Material + Clone {
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

pub(super) fn apply_material<M: BindMaterial>(
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

/// `emissive` is the loaded value, which already includes
/// `KHR_materials_emissive_strength`; expression targets are raw
/// `emissiveFactor` values, so only the target is scaled again.
fn compute_material(
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

use std::num::NonZeroU32;

use super::gpu_contract::{
    TEXTURE_TABLE_2D_CAPACITY, TEXTURE_TABLE_3D_CAPACITY, TEXTURE_TABLE_CUBE_CAPACITY,
    TEXTURE_TABLE_SAMPLER_CAPACITY,
};
use super::gpu_resources::{DecodedSampler, UploadedTextureBind, UploadedTextureIdentity};
use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use bevy::render::render_resource::{
    BindGroup, BindGroupEntry, BindGroupLayoutDescriptor, BindingResource, Extent3d, Sampler,
    SamplerDescriptor, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    TextureView, TextureViewDescriptor, TextureViewDimension, TextureViewId, WgpuSampler,
    WgpuTextureView,
};
use bevy::render::renderer::RenderDevice;
use d3d9_sm3::{TEXTURE_TABLE_BINDING_2D, TEXTURE_TABLE_BINDING_3D, TEXTURE_TABLE_BINDING_CUBE};
use render_material::{MaterialGenerationId, PortId, SamplerBinding, SamplerTextureDimension};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct SamplerKey {
    sampler_state: u8,
    packed_word: u32,
    anisotropy_clamp: u16,
    uses_mipmaps: bool,
}

impl From<&DecodedSampler> for SamplerKey {
    fn from(sampler: &DecodedSampler) -> Self {
        Self {
            sampler_state: sampler.sampler_state,
            packed_word: sampler.packed_word(),
            anisotropy_clamp: sampler.anisotropy_clamp(),
            uses_mipmaps: sampler.uses_mipmaps(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TextureTableRefusal {
    TexturesFull {
        dimension: SamplerTextureDimension,
        capacity: u32,
    },
    SamplersFull {
        capacity: u32,
    },
}

pub(super) struct TextureTableBinds {
    pub scene: BindGroup,
    pub sun_caster: BindGroup,
}

struct Placeholders {
    views: [TextureView; 3],
    sampler: Sampler,
    /// Shadow maps are read with `textureGather`, which wants a non-filtering sampler.
    nearest: Sampler,
}

pub(super) struct RegisterBinding {
    pub register: u16,
    pub dimension: SamplerTextureDimension,
    pub view: Option<TextureView>,
    pub sampler: Option<Sampler>,
    pub shadow: bool,
}

/// Textures a filtering sampler cannot read in a browser (32-bit float depth
/// and the shadow maps) stay on the placeholder.
fn placeholder_identity(identity: UploadedTextureIdentity) -> bool {
    match identity {
        UploadedTextureIdentity::Code(code) => matches!(code, 0x0f | 6 | 7),
        UploadedTextureIdentity::SpotShadowRt(_) => true,
        _ => false,
    }
}

#[derive(Resource, Default, Deref, DerefMut)]
pub(super) struct ShadowTextureTable(pub ExactTextureTable);

#[derive(Resource, Default, Deref, DerefMut)]
pub(super) struct SceneTextureTables(pub [ExactTextureTable; 4]);

pub(super) fn scene_table_index(after_scene_resolve: bool, srgb_write: bool) -> usize {
    usize::from(after_scene_resolve) * 2 + usize::from(srgb_write)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct TableEpoch {
    pub generation: MaterialGenerationId,

    pub replaced_revision: u64,
}

#[derive(Resource, Default)]
pub(super) struct ExactTextureTable {
    epoch: TableEpoch,
    views: [Vec<TextureView>; 3],
    view_index: HashMap<TextureViewId, u16>,

    sun_shadow_indices: Vec<u16>,
    samplers: Vec<Sampler>,
    sampler_index: HashMap<SamplerKey, u16>,
    placeholders: Option<Placeholders>,
    binds: Option<TextureTableBinds>,
    /// Browsers: one bind group per distinct set of material textures.
    draw_binds: HashMap<(Arc<[u32]>, PortId), BindGroup>,

    pub rebuild_n: u32,
}

const fn dimension_lane(dimension: SamplerTextureDimension) -> usize {
    match dimension {
        SamplerTextureDimension::D2 => 0,
        SamplerTextureDimension::Cube => 1,
        SamplerTextureDimension::D3 => 2,
    }
}

const fn capacity(dimension: SamplerTextureDimension) -> u32 {
    match dimension {
        SamplerTextureDimension::D2 => TEXTURE_TABLE_2D_CAPACITY,
        SamplerTextureDimension::Cube => TEXTURE_TABLE_CUBE_CAPACITY,
        SamplerTextureDimension::D3 => TEXTURE_TABLE_3D_CAPACITY,
    }
}

impl ExactTextureTable {
    pub fn epoch(&self) -> TableEpoch {
        self.epoch
    }

    pub fn open_epoch(&mut self, epoch: TableEpoch) {
        if self.epoch == epoch {
            return;
        }
        self.epoch = epoch;
        for lane in &mut self.views {
            lane.clear();
        }
        self.view_index.clear();
        self.sun_shadow_indices.clear();
        self.samplers.clear();
        self.sampler_index.clear();
        self.draw_binds.clear();
        self.binds = None;
    }

    pub fn slot_word(
        &mut self,
        device: &RenderDevice,
        lane: &UploadedTextureBind,
    ) -> Result<u32, TextureTableRefusal> {
        self.ensure_placeholders(device);
        let lane_i = dimension_lane(lane.dimension);
        let texture = match self.view_index.get(&lane.view.id()) {
            Some(&index) => index,
            None => {
                let cap = capacity(lane.dimension);
                let index = u32::try_from(self.views[lane_i].len()).expect("table index fits u32");
                if index >= cap {
                    return Err(TextureTableRefusal::TexturesFull {
                        dimension: lane.dimension,
                        capacity: cap,
                    });
                }
                let index = u16::try_from(index).expect("capacities fit the 16-bit slot half");
                self.views[lane_i].push(lane.view.clone());
                self.view_index.insert(lane.view.id(), index);
                if lane.identity == UploadedTextureIdentity::Code(super::CODE_TEXTURE_SHADOWMAP_SUN)
                {
                    self.sun_shadow_indices.push(index);
                }
                self.binds = None;
                index
            }
        };
        let key = SamplerKey::from(&lane.sampler);
        let sampler = match self.sampler_index.get(&key) {
            Some(&index) => index,
            None => {
                let index = u32::try_from(self.samplers.len()).expect("table index fits u32");
                if index >= TEXTURE_TABLE_SAMPLER_CAPACITY {
                    return Err(TextureTableRefusal::SamplersFull {
                        capacity: TEXTURE_TABLE_SAMPLER_CAPACITY,
                    });
                }
                let index = u16::try_from(index).expect("capacities fit the 16-bit slot half");
                self.samplers
                    .push(device.create_sampler(&lane.sampler.descriptor()));
                self.sampler_index.insert(key, index);
                self.binds = None;
                index
            }
        };
        Ok(d3d9_sm3::texture_slot_word(texture, sampler))
    }

    pub fn cached_draw_bind(&self, slots: &Arc<[u32]>, port: PortId) -> Option<BindGroup> {
        self.draw_binds.get(&(Arc::clone(slots), port)).cloned()
    }

    /// Bind group carrying one texture and sampler per sampler register of
    /// the program, for runtimes without binding arrays.
    pub fn draw_bind_group(
        &mut self,
        device: &RenderDevice,
        bgl: &bevy::render::render_resource::BindGroupLayout,
        abi: &[SamplerBinding],
        textures: &[UploadedTextureBind],
        slots: &Arc<[u32]>,
        port: PortId,
        shadow_pass: bool,
    ) -> BindGroup {
        self.ensure_placeholders(device);
        let bindings: Vec<RegisterBinding> = abi
            .iter()
            .enumerate()
            .map(|(ordinal, binding)| {
                let lane = textures.iter().find(|lane| lane.register == binding.register);
                let view = lane
                    .filter(|lane| {
                        if binding.depth_compare {
                            // The shadow map cannot be sampled while it is being drawn into.
                            !shadow_pass
                        } else {
                            !placeholder_identity(lane.identity)
                        }
                    })
                    .map(|lane| lane.view.clone());
                let sampler = slots
                    .get(ordinal)
                    .and_then(|word| self.samplers.get(usize::try_from(word >> 16).ok()?))
                    .cloned();
                RegisterBinding {
                    register: binding.register,
                    dimension: binding.dimension,
                    view,
                    sampler,
                    shadow: binding.depth_compare,
                }
            })
            .collect();
        let group = self.register_bind_group(device, bgl, "iw4_draw_textures", &bindings);
        self.draw_binds
            .insert((Arc::clone(slots), port), group.clone());
        group
    }

    /// One binding pair per sampler register; missing pieces fall back to
    /// the 1x1 placeholders.
    pub fn register_bind_group(
        &mut self,
        device: &RenderDevice,
        bgl: &bevy::render::render_resource::BindGroupLayout,
        label: &'static str,
        bindings: &[RegisterBinding],
    ) -> BindGroup {
        self.ensure_placeholders(device);
        let placeholders = self.placeholders.as_ref().expect("built above");
        let mut entries = Vec::with_capacity(bindings.len() * 2);
        for binding in bindings {
            let first = d3d9_sm3::sampler_texture_binding(binding.register);
            let view: &WgpuTextureView = match &binding.view {
                Some(view) => &**view,
                None => &*placeholders.views[dimension_lane(binding.dimension)],
            };
            let sampler: &WgpuSampler = match &binding.sampler {
                _ if binding.shadow => &*placeholders.nearest,
                Some(sampler) => &**sampler,
                None => &*placeholders.sampler,
            };
            entries.push(BindGroupEntry {
                binding: first,
                resource: BindingResource::TextureView(view),
            });
            entries.push(BindGroupEntry {
                binding: first + 1,
                resource: BindingResource::Sampler(sampler),
            });
        }
        device.create_bind_group(label, bgl, &entries)
    }

    #[cfg(target_arch = "wasm32")]
    fn placeholder_group(
        &self,
        device: &RenderDevice,
        bgl: &bevy::render::render_resource::BindGroupLayout,
        layout: &BindGroupLayoutDescriptor,
    ) -> BindGroup {
        use bevy::render::render_resource::BindingType;
        let placeholders = self.placeholders.as_ref().expect("placeholders exist once the table is built");
        let entries: Vec<BindGroupEntry> = layout
            .entries
            .iter()
            .filter_map(|entry| {
                let resource = match entry.ty {
                    BindingType::Texture { view_dimension, .. } => BindingResource::TextureView(
                        &*placeholders.views[match view_dimension {
                            TextureViewDimension::Cube => 1,
                            TextureViewDimension::D3 => 2,
                            _ => 0,
                        }],
                    ),
                    BindingType::Sampler(_) => BindingResource::Sampler(&*placeholders.sampler),
                    _ => return None,
                };
                Some(BindGroupEntry {
                    binding: entry.binding,
                    resource,
                })
            })
            .collect();
        device.create_bind_group("iw4_texture_table_placeholder", bgl, &entries)
    }

    pub fn census(&self) -> (usize, usize, usize, usize) {
        let n = |lane: &Vec<TextureView>| lane.len().saturating_sub(1);
        (
            n(&self.views[0]),
            n(&self.views[1]),
            n(&self.views[2]),
            self.samplers.len().saturating_sub(1),
        )
    }

    pub fn binds(
        &mut self,
        device: &RenderDevice,
        registry: &super::exact_pipeline::ExactPipelineRegistry,
        layout: &BindGroupLayoutDescriptor,
    ) -> &TextureTableBinds {
        self.ensure_placeholders(device);
        if self.binds.is_none() {
            let bgl = registry.bind_group_layout(device, layout);
            #[cfg(target_arch = "wasm32")]
            {
                // Draws carry their own textures; this group only fills the slot.
                let scene = self.placeholder_group(device, &bgl, layout);
                self.rebuild_n = self.rebuild_n.saturating_add(1);
                self.binds = Some(TextureTableBinds {
                    sun_caster: scene.clone(),
                    scene,
                });
                return self.binds.as_ref().expect("built above");
            }
            #[allow(unreachable_code)]
            let placeholder_2d = self.views[0][0].clone();
            let scene = self.create_bind_group(device, &bgl, "iw4_texture_table_scene", None);
            let sun_caster = if self.sun_shadow_indices.is_empty() {
                scene.clone()
            } else {
                self.create_bind_group(
                    device,
                    &bgl,
                    "iw4_texture_table_sun_caster",
                    Some(&placeholder_2d),
                )
            };
            self.rebuild_n = self.rebuild_n.saturating_add(1);
            self.binds = Some(TextureTableBinds { scene, sun_caster });
        }
        self.binds.as_ref().expect("built above")
    }

    pub fn views_bind_group(
        &mut self,
        device: &RenderDevice,
        bgl: &bevy::render::render_resource::BindGroupLayout,
        label: &'static str,
        views: &[&TextureView],
        samplers: &[&Sampler],
        abi: &[SamplerBinding],
        slots: &[u32],
    ) -> BindGroup {
        #[cfg(target_arch = "wasm32")]
        {
            let bindings: Vec<RegisterBinding> = abi
                .iter()
                .enumerate()
                .map(|(ordinal, binding)| {
                    let word = slots.get(ordinal).copied().unwrap_or(0);
                    RegisterBinding {
                        register: binding.register,
                        dimension: binding.dimension,
                        view: views.get((word & 0xffff) as usize).map(|view| (*view).clone()),
                        sampler: samplers.get((word >> 16) as usize).map(|sampler| (*sampler).clone()),
                        shadow: binding.depth_compare,
                    }
                })
                .collect();
            return self.register_bind_group(device, bgl, label, &bindings);
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = (abi, slots);
        #[allow(unreachable_code)]
        let views: Vec<&WgpuTextureView> = views.iter().map(|v| &***v).collect();
        let samplers: Vec<&WgpuSampler> = samplers.iter().map(|v| &***v).collect();
        self.ensure_placeholders(device);
        let placeholders = self.placeholders.as_ref().expect("built above");
        device.create_bind_group(
            label,
            bgl,
            &[
                BindGroupEntry {
                    binding: TEXTURE_TABLE_BINDING_2D,
                    #[cfg(not(target_arch = "wasm32"))]
                    resource: BindingResource::TextureViewArray(&views),
                    #[cfg(target_arch = "wasm32")]
                    resource: BindingResource::TextureView(views.first().copied().unwrap_or(&*placeholders.views[0])),
                },
                BindGroupEntry {
                    binding: TEXTURE_TABLE_BINDING_CUBE,
                    #[cfg(not(target_arch = "wasm32"))]
                    resource: BindingResource::TextureViewArray(&[&*placeholders.views[1]]),
                    #[cfg(target_arch = "wasm32")]
                    resource: BindingResource::TextureView(&*placeholders.views[1]),
                },
                BindGroupEntry {
                    binding: TEXTURE_TABLE_BINDING_3D,
                    #[cfg(not(target_arch = "wasm32"))]
                    resource: BindingResource::TextureViewArray(&[&*placeholders.views[2]]),
                    #[cfg(target_arch = "wasm32")]
                    resource: BindingResource::TextureView(&*placeholders.views[2]),
                },
                BindGroupEntry {
                    binding: d3d9_sm3::TEXTURE_TABLE_BINDING_SAMPLERS,
                    #[cfg(not(target_arch = "wasm32"))]
                    resource: BindingResource::SamplerArray(&samplers),
                    #[cfg(target_arch = "wasm32")]
                    resource: BindingResource::Sampler(samplers.first().copied().unwrap_or(&*placeholders.sampler)),
                },
            ],
        )
    }

    fn create_bind_group(
        &self,
        device: &RenderDevice,
        bgl: &bevy::render::render_resource::BindGroupLayout,
        label: &'static str,
        mask_sun_shadow_with: Option<&TextureView>,
    ) -> BindGroup {
        let mut views_2d: Vec<&WgpuTextureView> = self.views[0].iter().map(|v| &**v).collect();
        if let Some(placeholder) = mask_sun_shadow_with {
            for &index in &self.sun_shadow_indices {
                views_2d[usize::from(index)] = &**placeholder;
            }
        }
        let views_cube: Vec<&WgpuTextureView> = self.views[1].iter().map(|v| &**v).collect();
        let views_3d: Vec<&WgpuTextureView> = self.views[2].iter().map(|v| &**v).collect();
        let samplers: Vec<&WgpuSampler> = self.samplers.iter().map(|s| &**s).collect();
        device.create_bind_group(
            label,
            bgl,
            &[
                BindGroupEntry {
                    binding: TEXTURE_TABLE_BINDING_2D,
                    #[cfg(not(target_arch = "wasm32"))]
                    resource: BindingResource::TextureViewArray(&views_2d),
                    #[cfg(target_arch = "wasm32")]
                    resource: BindingResource::TextureView(views_2d[0]),
                },
                BindGroupEntry {
                    binding: TEXTURE_TABLE_BINDING_CUBE,
                    #[cfg(not(target_arch = "wasm32"))]
                    resource: BindingResource::TextureViewArray(&views_cube),
                    #[cfg(target_arch = "wasm32")]
                    resource: BindingResource::TextureView(views_cube[0]),
                },
                BindGroupEntry {
                    binding: TEXTURE_TABLE_BINDING_3D,
                    #[cfg(not(target_arch = "wasm32"))]
                    resource: BindingResource::TextureViewArray(&views_3d),
                    #[cfg(target_arch = "wasm32")]
                    resource: BindingResource::TextureView(views_3d[0]),
                },
                BindGroupEntry {
                    binding: d3d9_sm3::TEXTURE_TABLE_BINDING_SAMPLERS,
                    #[cfg(not(target_arch = "wasm32"))]
                    resource: BindingResource::SamplerArray(&samplers),
                    #[cfg(target_arch = "wasm32")]
                    resource: BindingResource::Sampler(samplers.first().copied().unwrap_or(&*self.placeholders.as_ref().expect("placeholders exist once the table is built").sampler)),
                },
            ],
        )
    }

    fn ensure_placeholders(&mut self, device: &RenderDevice) {
        let placeholders = self.placeholders.get_or_insert_with(|| {
            let make = |label, layers, dimension, view_dimension| {
                let texture = device.create_texture(&TextureDescriptor {
                    label: Some(label),
                    size: Extent3d {
                        width: 1,
                        height: 1,
                        depth_or_array_layers: layers,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension,
                    format: TextureFormat::R8Unorm,
                    usage: TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                });
                texture.create_view(&TextureViewDescriptor {
                    label: Some(label),
                    dimension: Some(view_dimension),
                    ..default()
                })
            };
            Placeholders {
                views: [
                    make(
                        "iw4_texture_table_placeholder_2d",
                        1,
                        TextureDimension::D2,
                        TextureViewDimension::D2,
                    ),
                    make(
                        "iw4_texture_table_placeholder_cube",
                        6,
                        TextureDimension::D2,
                        TextureViewDimension::Cube,
                    ),
                    make(
                        "iw4_texture_table_placeholder_3d",
                        1,
                        TextureDimension::D3,
                        TextureViewDimension::D3,
                    ),
                ],
                sampler: device.create_sampler(&SamplerDescriptor {
                    label: Some("iw4_texture_table_placeholder_sampler"),
                    ..default()
                }),
                nearest: device.create_sampler(&SamplerDescriptor {
                    label: Some("iw4_texture_table_nearest_sampler"),
                    mag_filter: bevy::render::render_resource::FilterMode::Nearest,
                    min_filter: bevy::render::render_resource::FilterMode::Nearest,
                    mipmap_filter: bevy::render::render_resource::MipmapFilterMode::Nearest,
                    ..default()
                }),
            }
        });
        for (lane, view) in self.views.iter_mut().zip(placeholders.views.iter()) {
            if lane.is_empty() {
                lane.push(view.clone());
            }
        }
        if self.samplers.is_empty() {
            self.samplers.push(placeholders.sampler.clone());
        }
    }
}

pub(super) fn array_count(count: u32) -> NonZeroU32 {
    NonZeroU32::new(count).expect("texture table capacities are non-zero")
}

/// Browsers have no binding arrays: there the table entries stay single bindings.
pub(super) trait CountIfNative {
    fn count_if_native(self, count: u32) -> Self;
}

impl CountIfNative for bevy::render::render_resource::BindGroupLayoutEntryBuilder {
    #[cfg(not(target_arch = "wasm32"))]
    fn count_if_native(self, count: u32) -> Self {
        self.count(array_count(count))
    }

    #[cfg(target_arch = "wasm32")]
    fn count_if_native(self, _count: u32) -> Self {
        self
    }
}

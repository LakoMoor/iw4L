//! Browser WebGPU has no `binding_array`. The generated programs index
//! texture and sampler tables by a packed slot word; for the browser each
//! sampler register gets its own plain binding pair instead (`sm3_tex_sN` at
//! binding 2N, `sm3_smp_sN` at 2N+1) and the draw supplies a bind group that
//! carries exactly the textures its material uses. The `dx_*` programs keep a
//! single shared binding per table.

extern crate alloc;
use alloc::{
    collections::BTreeMap,
    format,
    string::{String, ToString},
    vec::Vec,
};

static SHADOWS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// Debug switch for A/B comparisons: with shadows off every comparison reads "lit".
/// Takes effect for programs flattened after the call.
pub fn set_shadows(on: bool) {
    SHADOWS.store(on, core::sync::atomic::Ordering::Relaxed);
}

const TABLES: [&str; 4] = [
    "dx_textures_2d",
    "dx_textures_cube",
    "dx_textures_3d",
    "dx_samplers",
];

/// Binding number of the texture for sampler `register`; its sampler follows.
pub const fn sampler_texture_binding(register: u16) -> u32 {
    register as u32 * 2
}

const SM3_TABLES: [(&str, &str); 3] = [
    ("sm3_textures_2d[", "texture_2d<f32>"),
    ("sm3_textures_cube[", "texture_cube<f32>"),
    ("sm3_textures_3d[", "texture_3d<f32>"),
];

/// Index of the delimiter closing the one opened just before `from`.
fn closing(text: &[u8], from: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 1_u32;
    for (offset, &byte) in text[from..].iter().enumerate() {
        if byte == open {
            depth += 1;
        } else if byte == close {
            depth -= 1;
            if depth == 0 {
                return Some(from + offset);
            }
        }
    }
    None
}

/// Rewrites `sm3_textures_*[sm3_slot_sN & 0xffffu]` and
/// `sm3_samplers[sm3_slot_sN >> 16u]` into per-register bindings and returns
/// the register to texture-type map the declarations must be made from.
fn per_register_bindings(text: &mut String) -> BTreeMap<u16, &'static str> {
    let mut used = BTreeMap::new();
    for (needle, ty) in SM3_TABLES {
        let mut from = 0;
        while let Some(found) = text[from..].find(needle) {
            let start = from + found;
            let open = start + needle.len();
            let Some(end) = closing(text.as_bytes(), open, b'[', b']') else {
                break;
            };
            let register = register_of(&text[open..end], " & 0xffffu");
            match register {
                Some(register) => {
                    used.insert(register, ty);
                    let name = format!("sm3_tex_s{register}");
                    text.replace_range(start..=end, &name);
                    from = start + name.len();
                }
                None => from = end,
            }
        }
    }
    let mut from = 0;
    while let Some(found) = text[from..].find("sm3_samplers[") {
        let start = from + found;
        let open = start + "sm3_samplers[".len();
        let Some(end) = closing(text.as_bytes(), open, b'[', b']') else {
            break;
        };
        match register_of(&text[open..end], " >> 16u") {
            Some(register) => {
                let name = format!("sm3_smp_s{register}");
                text.replace_range(start..=end, &name);
                from = start + name.len();
            }
            None => from = end,
        }
    }
    used
}

fn register_of(index: &str, suffix: &str) -> Option<u16> {
    index
        .strip_prefix("sm3_slot_s")?
        .strip_suffix(suffix)?
        .parse()
        .ok()
}

fn drop_declaration_lines(text: &mut String, names: &[&str]) {
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| {
            !(line.starts_with("@group(")
                && names
                    .iter()
                    .any(|name| line.contains(&format!(" var {name}:"))))
        })
        .collect();
    let mut out = kept.join("\n");
    out.push('\n');
    *text = out;
}

/// Rewrites `sm3_shadow_compare(sm3_slot_sN, ...)` calls into per-register
/// functions over the bound texture, returns the registers it declared.
fn per_register_shadow_compare(text: &mut String) -> alloc::vec::Vec<u16> {
    let mut registers = alloc::vec::Vec::new();
    let needle = "sm3_shadow_compare(sm3_slot_s";
    let mut from = 0;
    while let Some(found) = text[from..].find(needle) {
        let start = from + found;
        let digits_at = start + needle.len();
        let digits: String = text[digits_at..].chars().take_while(char::is_ascii_digit).collect();
        let Ok(register) = digits.parse::<u16>() else {
            from = digits_at;
            continue;
        };
        let after = digits_at + digits.len();
        let rest = text[after..].strip_prefix(", ").map_or(0, |_| 2);
        let name = format!("sm3_shadow_compare_s{register}(");
        text.replace_range(start..after + rest, &name);
        from = start + name.len();
        if !registers.contains(&register) {
            registers.push(register);
        }
    }
    registers
}

/// The shared comparison helper indexes a table that no longer exists.
fn drop_shadow_compare(text: &mut String) {
    const SIGNATURE: &str = "fn sm3_shadow_compare(";
    if let Some(start) = text.find(SIGNATURE)
        && let Some(close) = text[start..].find("\n}\n")
    {
        text.replace_range(
            start..start + close + 3,
            "fn sm3_shadow_compare(slot: u32, uv: vec2<f32>, z: f32) -> f32 {\n    return 1.0;\n}\n",
        );
    }
}

pub fn flatten_texture_tables(wgsl: &str) -> String {
    let mut text = wgsl.replace("enable wgpu_binding_array;\n", "");

    let shadow_registers = per_register_shadow_compare(&mut text);
    drop_shadow_compare(&mut text);
    let mut used = per_register_bindings(&mut text);
    for register in &shadow_registers {
        used.insert(*register, "texture_2d<f32>");
    }
    if text.contains(" var sm3_") {
    drop_declaration_lines(
        &mut text,
        &[
            "sm3_textures_2d",
            "sm3_textures_cube",
            "sm3_textures_3d",
            "sm3_samplers",
        ],
    );
    }
    if !used.is_empty() {
        let mut declarations = String::new();
        for (register, ty) in &used {
            let binding = sampler_texture_binding(*register);
            declarations.push_str(&format!(
                "@group(1) @binding({binding}) var sm3_tex_s{register}: {ty};\n@group(1) @binding({}) var sm3_smp_s{register}: sampler;\n",
                binding + 1
            ));
        }
        let shadows = SHADOWS.load(core::sync::atomic::Ordering::Relaxed);
        for register in shadow_registers.iter().filter(|_| !shadows) {
            declarations.push_str(&format!(
                "fn sm3_shadow_compare_s{register}(uv: vec2<f32>, z: f32) -> f32 {{
    return 1.0;
}}
"
            ));
        }
        for register in shadow_registers.iter().filter(|_| shadows) {
            declarations.push_str(&format!(
                "fn sm3_shadow_compare_s{register}(uv: vec2<f32>, z: f32) -> f32 {{
    let size = vec2<f32>(textureDimensions(sm3_tex_s{register}));
    let stored = textureGather(0, sm3_tex_s{register}, sm3_smp_s{register}, uv);
    let lit = select(vec4<f32>(0.0), vec4<f32>(1.0), stored >= vec4<f32>(z));
    let f = fract(uv * size - 0.5);
    return mix(mix(lit.w, lit.z, f.x), mix(lit.x, lit.y, f.x), f.y);
}}
"
            ));
        }
        // Module-scope bindings may come first; an entry-point attribute stays with its function.
        text.insert_str(0, &declarations);
    }

    // `binding_array<T>` -> `T` (the `dx_*` tables)
    while let Some(start) = text.find("binding_array<") {
        let inner = start + "binding_array<".len();
        let Some(end) = closing(text.as_bytes(), inner, b'<', b'>') else {
            break;
        };
        let element = text[inner..end].to_string();
        text.replace_range(start..=end, &element);
    }

    // `table[index]` -> `table`
    for table in TABLES {
        let needle = format!("{table}[");
        let mut from = 0;
        while let Some(found) = text[from..].find(&needle) {
            let start = from + found;
            let open = start + needle.len();
            let Some(end) = closing(text.as_bytes(), open, b'[', b']') else {
                break;
            };
            text.replace_range(start + table.len()..=end, "");
            from = start + table.len();
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::flatten_texture_tables;

    #[test]
    fn each_register_gets_its_own_binding_pair() {
        let wgsl = "enable wgpu_binding_array;\n\
            @group(1) @binding(0) var sm3_textures_2d: binding_array<texture_2d<f32>>;\n\
            @group(1) @binding(1) var sm3_textures_cube: binding_array<texture_cube<f32>>;\n\
            @group(1) @binding(3) var sm3_samplers: binding_array<sampler>;\n\
            fn f(uv: vec2<f32>) -> vec4<f32> {\n\
            let sm3_slot_s0 = 1u;\n\
            let sm3_slot_s3 = 2u;\n\
            return textureSample(sm3_textures_2d[sm3_slot_s0 & 0xffffu], sm3_samplers[sm3_slot_s0 >> 16u], uv) + textureSample(sm3_textures_cube[sm3_slot_s3 & 0xffffu], sm3_samplers[sm3_slot_s3 >> 16u], vec3<f32>(uv, 0.0));\n}\n";
        let out = flatten_texture_tables(wgsl);
        assert!(!out.contains("binding_array"));
        assert!(!out.contains("enable"));
        assert!(out.contains("@group(1) @binding(0) var sm3_tex_s0: texture_2d<f32>;"));
        assert!(out.contains("@group(1) @binding(1) var sm3_smp_s0: sampler;"));
        assert!(out.contains("@group(1) @binding(6) var sm3_tex_s3: texture_cube<f32>;"));
        assert!(out.contains("@group(1) @binding(7) var sm3_smp_s3: sampler;"));
        assert!(out.contains("textureSample(sm3_tex_s0, sm3_smp_s0, uv)"));
        assert!(out.contains("textureSample(sm3_tex_s3, sm3_smp_s3, vec3<f32>(uv, 0.0))"));
    }

    #[test]
    fn shadow_compare_becomes_a_constant() {
        let wgsl = "fn sm3_shadow_compare(slot: u32, uv: vec2<f32>, z: f32) -> f32 {\n    let size = vec2<f32>(textureDimensions(sm3_textures_2d[slot & 0xffffu]));\n    return 0.0;\n}\nfn g() {}\n";
        let out = flatten_texture_tables(wgsl);
        assert!(out.contains("return 1.0;"));
        assert!(out.contains("fn g() {}"));
        assert!(!out.contains("textureDimensions"));
    }

    #[test]
    fn shadow_samples_read_the_bound_register() {
        let wgsl = "@group(1) @binding(2) var sm3_textures_2d: binding_array<texture_2d<f32>>;
@group(1) @binding(3) var sm3_samplers: binding_array<sampler>;
fn sm3_shadow_compare(slot: u32, uv: vec2<f32>, z: f32) -> f32 {
    return 0.0;
}
fn f(uv: vec2<f32>) -> f32 {
    let sm3_slot_s2 = 1u;
    return sm3_shadow_compare(sm3_slot_s2, uv, 0.5);
}
";
        let out = flatten_texture_tables(wgsl);
        assert!(out.contains("return sm3_shadow_compare_s2(uv, 0.5);"));
        assert!(out.contains("fn sm3_shadow_compare_s2(uv: vec2<f32>, z: f32) -> f32 {"));
        assert!(out.contains("textureGather(0, sm3_tex_s2, sm3_smp_s2, uv)"));
        assert!(out.contains("@group(1) @binding(4) var sm3_tex_s2: texture_2d<f32>;"));
    }

    #[test]
    fn dx_tables_keep_one_shared_binding() {
        let out =
            flatten_texture_tables("a(dx_textures_2d[t[3] + 1u], dx_samplers[dx_slot >> 16u]);");
        assert_eq!(out, "a(dx_textures_2d, dx_samplers);");
    }
}

//! Browser WebGPU has no `binding_array`. Until textures reach the shaders by
//! another route, the generated programs read a single bound texture and
//! sampler per dimension: the table declarations become plain bindings and
//! every `table[index]` becomes `table`.

extern crate alloc;
use alloc::{format, string::{String, ToString}};

const TABLES: [&str; 8] = [
    "sm3_textures_2d",
    "sm3_textures_cube",
    "sm3_textures_3d",
    "sm3_samplers",
    "dx_textures_2d",
    "dx_textures_cube",
    "dx_textures_3d",
    "dx_samplers",
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

pub fn flatten_texture_tables(wgsl: &str) -> String {
    let mut text = wgsl.replace("enable wgpu_binding_array;\n", "");

    // `binding_array<T>` -> `T`
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
    fn declarations_and_indexing_become_single_bindings() {
        let wgsl = "enable wgpu_binding_array;\n\
            @group(1) @binding(0) var sm3_textures_2d: binding_array<texture_2d<f32>>;\n\
            @group(1) @binding(3) var sm3_samplers: binding_array<sampler>;\n\
            fn f(slot: u32, uv: vec2<f32>) -> vec4<f32> {\n\
            return textureSample(sm3_textures_2d[slot & 0xffffu], sm3_samplers[slot >> 16u], uv);\n}\n";
        let out = flatten_texture_tables(wgsl);
        assert!(!out.contains("binding_array"));
        assert!(!out.contains("enable"));
        assert!(out.contains("var sm3_textures_2d: texture_2d<f32>;"));
        assert!(out.contains("var sm3_samplers: sampler;"));
        assert!(out.contains("textureSample(sm3_textures_2d, sm3_samplers, uv)"));
    }

    #[test]
    fn nested_index_expressions_are_removed_whole() {
        let out = flatten_texture_tables("a(dx_textures_2d[t[3] + 1u], dx_samplers[dx_slot >> 16u]);");
        assert_eq!(out, "a(dx_textures_2d, dx_samplers);");
    }
}

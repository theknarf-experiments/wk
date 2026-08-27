//! glTF / GLB scene loading (CPU side): flattens a file's default scene into a
//! list of triangle meshes with node transforms baked in, ready for the 3D
//! renderer to upload. Supports positions/normals/uvs/vertex-colors/indices
//! and PBR base colour (factor + texture); everything else is ignored.

/// One flattened mesh: interleaved-ready vertex arrays (transforms baked),
/// triangle indices, and its base-colour material.
pub struct CpuMesh {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    pub colors: Vec<[f32; 4]>,
    pub indices: Vec<u32>,
    pub base_color: [f32; 4],
    /// RGBA8 base-colour texture pixels, if the material has one.
    pub texture: Option<(u32, u32, Vec<u8>)>,
}

/// Load a GLB from bytes — the only way geometry reaches the view: every
/// mesh, from a spinning totem to the surrounding world itself, arrives as a
/// `wk:scene` entity's self-contained blob.
pub fn load_bytes(bytes: &[u8]) -> Result<Vec<CpuMesh>, String> {
    let (doc, buffers, images) =
        gltf::import_slice(bytes).map_err(|e| format!("parse glb: {e}"))?;
    Ok(flatten(&doc, &buffers, &images))
}

fn flatten(
    doc: &gltf::Document,
    buffers: &[gltf::buffer::Data],
    images: &[gltf::image::Data],
) -> Vec<CpuMesh> {
    let mut out = Vec::new();
    let scene = doc.default_scene().or_else(|| doc.scenes().next());
    let Some(scene) = scene else {
        return out;
    };
    for node in scene.nodes() {
        walk(&node, mat_identity(), buffers, images, &mut out);
    }
    out
}

fn walk(
    node: &gltf::Node,
    parent: [[f32; 4]; 4],
    buffers: &[gltf::buffer::Data],
    images: &[gltf::image::Data],
    out: &mut Vec<CpuMesh>,
) {
    let local = node.transform().matrix();
    let m = mat_mul(parent, local);
    if let Some(mesh) = node.mesh() {
        for prim in mesh.primitives() {
            if prim.mode() != gltf::mesh::Mode::Triangles {
                continue;
            }
            if let Some(cm) = primitive_mesh(&prim, m, buffers, images) {
                out.push(cm);
            }
        }
    }
    for child in node.children() {
        walk(&child, m, buffers, images, out);
    }
}

fn primitive_mesh(
    prim: &gltf::Primitive,
    m: [[f32; 4]; 4],
    buffers: &[gltf::buffer::Data],
    images: &[gltf::image::Data],
) -> Option<CpuMesh> {
    let reader = prim.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
    let positions: Vec<[f32; 3]> = reader
        .read_positions()?
        .map(|p| transform_point(m, p))
        .collect();
    let n = positions.len();
    let normals: Vec<[f32; 3]> = match reader.read_normals() {
        Some(ns) => ns.map(|v| transform_dir(m, v)).collect(),
        None => vec![[0.0, 1.0, 0.0]; n],
    };
    let uvs: Vec<[f32; 2]> = match reader.read_tex_coords(0) {
        Some(tc) => tc.into_f32().collect(),
        None => vec![[0.0, 0.0]; n],
    };
    let colors: Vec<[f32; 4]> = match reader.read_colors(0) {
        Some(c) => c.into_rgba_f32().collect(),
        None => vec![[1.0, 1.0, 1.0, 1.0]; n],
    };
    let indices: Vec<u32> = match reader.read_indices() {
        Some(ix) => ix.into_u32().collect(),
        None => (0..n as u32).collect(),
    };
    let pbr = prim.material().pbr_metallic_roughness();
    let base_color = pbr.base_color_factor();
    let texture = pbr.base_color_texture().and_then(|info| {
        let img = images.get(info.texture().source().index())?;
        rgba8(img)
    });
    Some(CpuMesh {
        positions,
        normals,
        uvs,
        colors,
        indices,
        base_color,
        texture,
    })
}

/// Convert a glTF image to tightly-packed RGBA8 (the formats exporters
/// actually emit; anything exotic is skipped).
fn rgba8(img: &gltf::image::Data) -> Option<(u32, u32, Vec<u8>)> {
    use gltf::image::Format;
    let (w, h) = (img.width, img.height);
    let px = match img.format {
        Format::R8G8B8A8 => img.pixels.clone(),
        Format::R8G8B8 => {
            let mut out = Vec::with_capacity((w * h * 4) as usize);
            for c in img.pixels.chunks_exact(3) {
                out.extend_from_slice(&[c[0], c[1], c[2], 255]);
            }
            out
        }
        Format::R8 => {
            let mut out = Vec::with_capacity((w * h * 4) as usize);
            for &v in &img.pixels {
                out.extend_from_slice(&[v, v, v, 255]);
            }
            out
        }
        _ => return None,
    };
    Some((w, h, px))
}

fn mat_identity() -> [[f32; 4]; 4] {
    let mut m = [[0.0; 4]; 4];
    for (i, row) in m.iter_mut().enumerate() {
        row[i] = 1.0;
    }
    m
}

/// Column-major multiply (glTF matrices are column-major like ours).
fn mat_mul(a: [[f32; 4]; 4], b: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut out = [[0.0f32; 4]; 4];
    for (col, out_col) in out.iter_mut().enumerate() {
        for (row, out_cell) in out_col.iter_mut().enumerate() {
            *out_cell = (0..4).map(|k| a[k][row] * b[col][k]).sum();
        }
    }
    out
}

fn transform_point(m: [[f32; 4]; 4], p: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * p[0] + m[1][0] * p[1] + m[2][0] * p[2] + m[3][0],
        m[0][1] * p[0] + m[1][1] * p[1] + m[2][1] * p[2] + m[3][1],
        m[0][2] * p[0] + m[1][2] * p[1] + m[2][2] * p[2] + m[3][2],
    ]
}

/// Rotate (and renormalize) a direction by the matrix's upper 3×3 — correct
/// for the rigid/uniform transforms world scenes actually use.
fn transform_dir(m: [[f32; 4]; 4], v: [f32; 3]) -> [f32; 3] {
    let d = [
        m[0][0] * v[0] + m[1][0] * v[1] + m[2][0] * v[2],
        m[0][1] * v[0] + m[1][1] * v[1] + m[2][1] * v[2],
        m[0][2] * v[0] + m[1][2] * v[1] + m[2][2] * v[2],
    ];
    let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(1e-6);
    [d[0] / len, d[1] / len, d[2] / len]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid GLB in memory: one triangle, translated +2 in x
    /// by its node, with normals and a red base colour.
    fn tiny_glb() -> Vec<u8> {
        // Binary buffer: 3 positions + 3 normals + 3 indices (u16 padded).
        let mut bin: Vec<u8> = Vec::new();
        let positions: [[f32; 3]; 3] = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let normals: [[f32; 3]; 3] = [[0.0, 0.0, 1.0]; 3];
        for p in positions.iter().chain(normals.iter()) {
            for v in p {
                bin.extend_from_slice(&v.to_le_bytes());
            }
        }
        let ibase = bin.len();
        for i in [0u16, 1, 2, 0] {
            bin.extend_from_slice(&i.to_le_bytes());
        }
        let json = format!(
            r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],
"nodes":[{{"mesh":0,"translation":[2,0,0]}}],
"meshes":[{{"primitives":[{{"attributes":{{"POSITION":0,"NORMAL":1}},"indices":2,"material":0}}]}}],
"materials":[{{"pbrMetallicRoughness":{{"baseColorFactor":[1,0,0,1]}}}}],
"accessors":[
 {{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}},
 {{"bufferView":1,"componentType":5126,"count":3,"type":"VEC3"}},
 {{"bufferView":2,"componentType":5123,"count":3,"type":"SCALAR"}}],
"bufferViews":[
 {{"buffer":0,"byteOffset":0,"byteLength":36}},
 {{"buffer":0,"byteOffset":36,"byteLength":36}},
 {{"buffer":0,"byteOffset":{ibase},"byteLength":6}}],
"buffers":[{{"byteLength":{}}}]}}"#,
            bin.len()
        );
        let mut json = json.into_bytes();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
        let mut glb = Vec::new();
        glb.extend_from_slice(b"glTF");
        glb.extend_from_slice(&2u32.to_le_bytes());
        let total = 12 + 8 + json.len() + 8 + bin.len();
        glb.extend_from_slice(&(total as u32).to_le_bytes());
        glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json);
        glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"BIN\0");
        glb.extend_from_slice(&bin);
        glb
    }

    #[test]
    fn home_world_glb_loads() {
        // The shipped home world parses and has real geometry in every mesh.
        // (The world node reads exactly these bytes out of its vfs and hands
        // them to `wk:scene`.)
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../example/home.glb");
        let bytes = std::fs::read(path).expect("home.glb is readable");
        let meshes = load_bytes(&bytes).expect("home.glb loads");
        assert!(meshes.len() >= 4, "sky/floor/columns/pedestals");
        for m in &meshes {
            assert!(!m.positions.is_empty() && !m.indices.is_empty());
            assert_eq!(m.positions.len(), m.colors.len());
        }
    }

    #[test]
    fn glb_triangle_loads_with_baked_transform_and_material() {
        let meshes = load_bytes(&tiny_glb()).expect("parses");
        assert_eq!(meshes.len(), 1);
        let m = &meshes[0];
        assert_eq!(m.indices, vec![0, 1, 2]);
        // Node translation [2,0,0] baked into positions.
        assert_eq!(m.positions[0], [2.0, 0.0, 0.0]);
        assert_eq!(m.positions[1], [3.0, 0.0, 0.0]);
        assert_eq!(m.normals[0], [0.0, 0.0, 1.0]);
        assert_eq!(m.base_color, [1.0, 0.0, 0.0, 1.0]);
        assert!(m.texture.is_none());
        // Defaults fill in for missing attributes.
        assert_eq!(m.uvs.len(), 3);
        assert_eq!(m.colors[0], [1.0, 1.0, 1.0, 1.0]);
    }
}

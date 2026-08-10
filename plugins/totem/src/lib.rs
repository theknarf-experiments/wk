//! The totem: wk's first node that *is* a 3D object. It assembles a crystal
//! as a GLB in code, hands it to `wk:scene`, floats it above its node, and
//! spins. Clicking the crystal reverses the spin with a kick; hovering makes
//! it swell. Everything the node "renders" is real geometry in the world.

#[allow(warnings)]
mod bindings;

use bindings::wk::scene::scene::{Entity, RayEvent};
use bindings::Guest;

struct Component;

impl Guest for Component {
    fn run() {
        let glb = build_crystal_glb();
        let ent = Entity::new(&glb);
        ent.set_position(0.0, 0.9, 0.0);

        println!("totem: a crystal floats above this node — click it to reverse the spin.");

        let mut angle = 0.0f32;
        let mut speed = 0.9f32; // rad/s, eased back to cruise after a kick
        let mut dir = 1.0f32;
        let mut pulse = 0.0f32; // hover swell, decays each frame
        loop {
            while let Some(ev) = ent.poll_event() {
                match ev {
                    RayEvent::Press => {
                        dir = -dir;
                        speed = 3.2;
                        println!("totem: *spin!*");
                    }
                    RayEvent::Hover => pulse = 1.0,
                    RayEvent::Release => {}
                }
            }
            speed += (0.9 - speed) * 0.03;
            pulse *= 0.90;
            angle += dir * speed * 0.016;
            ent.set_rotation_y(angle);
            ent.set_scale(1.0 + pulse * 0.18);
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
    }
}

/// An octahedral crystal, flat-shaded (per-face normals, so every face gets
/// its own vertices), with a warm top and cool base — assembled as a
/// self-contained binary glTF.
fn build_crystal_glb() -> Vec<u8> {
    let top = [0.0f32, 0.55, 0.0];
    let bot = [0.0, -0.35, 0.0];
    let ring: Vec<[f32; 3]> = (0..4)
        .map(|i| {
            let t = i as f32 / 4.0 * std::f32::consts::TAU;
            [0.30 * t.sin(), 0.12, -0.30 * t.cos()]
        })
        .collect();

    let mut pos: Vec<[f32; 3]> = Vec::new();
    let mut nrm: Vec<[f32; 3]> = Vec::new();
    let mut col: Vec<[f32; 4]> = Vec::new();
    let mut idx: Vec<u32> = Vec::new();

    let mut face = |a: [f32; 3], b: [f32; 3], c: [f32; 3], color: [f32; 4]| {
        let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let mut n = [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ];
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-6);
        for k in &mut n {
            *k /= len;
        }
        let base = pos.len() as u32;
        pos.extend_from_slice(&[a, b, c]);
        nrm.extend_from_slice(&[n, n, n]);
        col.extend_from_slice(&[color, color, color]);
        idx.extend_from_slice(&[base, base + 1, base + 2]);
    };

    let warm = [0.95, 0.75, 0.35, 1.0];
    let cool = [0.30, 0.65, 0.75, 1.0];
    for i in 0..4 {
        let a = ring[i];
        let b = ring[(i + 1) % 4];
        face(a, b, top, warm);
        face(b, a, bot, cool);
    }

    glb(&pos, &nrm, &col, &idx)
}

/// Pack mesh arrays into a minimal valid GLB (one mesh, one vertex-coloured
/// material).
fn glb(pos: &[[f32; 3]], nrm: &[[f32; 3]], col: &[[f32; 4]], idx: &[u32]) -> Vec<u8> {
    let mut bin: Vec<u8> = Vec::new();
    let view = |bin: &mut Vec<u8>, data: &[u8]| -> (usize, usize) {
        let off = bin.len();
        bin.extend_from_slice(data);
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
        (off, data.len())
    };
    let f3: Vec<u8> = pos
        .iter()
        .flat_map(|p| p.iter().flat_map(|v| v.to_le_bytes()))
        .collect();
    let n3: Vec<u8> = nrm
        .iter()
        .flat_map(|p| p.iter().flat_map(|v| v.to_le_bytes()))
        .collect();
    let c4: Vec<u8> = col
        .iter()
        .flat_map(|p| p.iter().flat_map(|v| v.to_le_bytes()))
        .collect();
    let iu: Vec<u8> = idx.iter().flat_map(|v| v.to_le_bytes()).collect();
    let (po, pl) = view(&mut bin, &f3);
    let (no, nl) = view(&mut bin, &n3);
    let (co, cl) = view(&mut bin, &c4);
    let (io, il) = view(&mut bin, &iu);

    let (mut mn, mut mx) = ([f32::MAX; 3], [f32::MIN; 3]);
    for p in pos {
        for k in 0..3 {
            mn[k] = mn[k].min(p[k]);
            mx[k] = mx[k].max(p[k]);
        }
    }

    let json = format!(
        concat!(
            r#"{{"asset":{{"version":"2.0","generator":"wk totem"}},"scene":0,"#,
            r#""scenes":[{{"nodes":[0]}}],"nodes":[{{"mesh":0}}],"#,
            r#""meshes":[{{"primitives":[{{"attributes":{{"POSITION":0,"NORMAL":1,"COLOR_0":2}},"indices":3,"material":0}}]}}],"#,
            r#""materials":[{{"pbrMetallicRoughness":{{"baseColorFactor":[1,1,1,1],"metallicFactor":0,"roughnessFactor":1}}}}],"#,
            r#""accessors":[{{"bufferView":0,"componentType":5126,"count":{vc},"type":"VEC3","min":[{},{},{}],"max":[{},{},{}]}},"#,
            r#"{{"bufferView":1,"componentType":5126,"count":{vc},"type":"VEC3"}},"#,
            r#"{{"bufferView":2,"componentType":5126,"count":{vc},"type":"VEC4"}},"#,
            r#"{{"bufferView":3,"componentType":5125,"count":{ic},"type":"SCALAR"}}],"#,
            r#""bufferViews":[{{"buffer":0,"byteOffset":{po},"byteLength":{pl}}},"#,
            r#"{{"buffer":0,"byteOffset":{no},"byteLength":{nl}}},"#,
            r#"{{"buffer":0,"byteOffset":{co},"byteLength":{cl}}},"#,
            r#"{{"buffer":0,"byteOffset":{io},"byteLength":{il}}}],"#,
            r#""buffers":[{{"byteLength":{bl}}}]}}"#
        ),
        mn[0],
        mn[1],
        mn[2],
        mx[0],
        mx[1],
        mx[2],
        vc = pos.len(),
        ic = idx.len(),
        po = po,
        pl = pl,
        no = no,
        nl = nl,
        co = co,
        cl = cl,
        io = io,
        il = il,
        bl = bin.len(),
    );
    let mut json = json.into_bytes();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }

    let total = 12 + 8 + json.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(&bin);
    out
}

bindings::export!(Component with_types_in bindings);

#!/usr/bin/env python3
"""Generate example/home.glb — wk's default "home world" scene.

A VRChat-style home plaza sized for wk's 3D view (eye height y=0, floor at
y=-1.6, node panels hovering around the origin): a circular two-tone floor,
a ring of columns, a few pedestals, and a sky dome. Everything is vertex-
coloured (no textures), one glTF mesh per part, Y-up, metres.

Pure stdlib; regenerate with:  python3 scripts/gen-home-world.py
"""

import json
import math
import struct
from pathlib import Path

FLOOR_Y = -1.6
OUT = Path(__file__).resolve().parent.parent / "example" / "home.glb"


class Mesh:
    def __init__(self, name):
        self.name = name
        self.pos = []
        self.nrm = []
        self.col = []
        self.idx = []

    def quad(self, a, b, c, d, n, col):
        """Two triangles for corners a-b-c-d (counter-clockwise seen from +n)."""
        base = len(self.pos)
        self.pos += [a, b, c, d]
        self.nrm += [n] * 4
        self.col += [col] * 4
        self.idx += [base, base + 1, base + 2, base, base + 2, base + 3]

    def tri(self, a, b, c, n, col):
        base = len(self.pos)
        self.pos += [a, b, c]
        self.nrm += [n] * 3
        self.col += [col] * 3
        self.idx += [base, base + 1, base + 2]


def disc(mesh, cy, r0, r1, segs, col, up=True):
    """A flat ring (r0=0 makes a disc) at height cy."""
    n = (0, 1, 0) if up else (0, -1, 0)
    for i in range(segs):
        t0 = i / segs * math.tau
        t1 = (i + 1) / segs * math.tau
        p = lambda r, t: (r * math.sin(t), cy, -r * math.cos(t))
        if r0 == 0:
            mesh.tri(p(r1, t0), p(r1, t1), (0, cy, 0), n, col)
        else:
            mesh.quad(p(r0, t0), p(r1, t0), p(r1, t1), p(r0, t1), n, col)


def box(mesh, cx, cz, w, d, y0, y1, col, ry=0.0):
    """An axis-aligned box (optionally yawed) from y0..y1."""
    hw, hd = w / 2, d / 2
    cs, sn = math.cos(ry), math.sin(ry)

    def pt(x, z, y):
        rx = x * cs - z * sn
        rz = x * sn + z * cs
        return (cx + rx, y, cz + rz)

    def rot_n(n):
        return (n[0] * cs - n[2] * sn, n[1], n[0] * sn + n[2] * cs)

    # 4 sides
    for (x0, z0, x1, z1, n) in [
        (-hw, -hd, hw, -hd, (0, 0, -1)),
        (hw, -hd, hw, hd, (1, 0, 0)),
        (hw, hd, -hw, hd, (0, 0, 1)),
        (-hw, hd, -hw, -hd, (-1, 0, 0)),
    ]:
        mesh.quad(
            pt(x0, z0, y0), pt(x0, z0, y1), pt(x1, z1, y1), pt(x1, z1, y0), rot_n(n), col
        )
    # top
    mesh.quad(
        pt(-hw, -hd, y1), pt(-hw, hd, y1), pt(hw, hd, y1), pt(hw, -hd, y1), (0, 1, 0), col
    )


def column(mesh, cx, cz, r, y0, y1, segs, col):
    for i in range(segs):
        t0 = i / segs * math.tau
        t1 = (i + 1) / segs * math.tau
        p = lambda t, y: (cx + r * math.sin(t), y, cz - r * math.cos(t))
        n = (math.sin((t0 + t1) / 2), 0, -math.cos((t0 + t1) / 2))
        mesh.quad(p(t0, y0), p(t0, y1), p(t1, y1), p(t1, y0), n, col)
    disc_at = lambda: None  # caps hidden (below floor / seen from below rarely)
    # top cap
    base = len(mesh.pos)
    for i in range(segs):
        t0 = i / segs * math.tau
        t1 = (i + 1) / segs * math.tau
        mesh.tri(
            (cx + r * math.sin(t0), y1, cz - r * math.cos(t0)),
            (cx, y1, cz),
            (cx + r * math.sin(t1), y1, cz - r * math.cos(t1)),
            (0, 1, 0),
            col,
        )


def sky(mesh, radius, bands, segs, horizon, zenith):
    """An inward-facing dome. Normals point straight up so the renderer's
    directional light shades it evenly (only the ambient+constant term)."""

    def lerp(a, b, t):
        return tuple(a[i] + (b[i] - a[i]) * t for i in range(4))

    def p(band, seg):
        phi = band / bands * (math.pi / 2)  # 0 = horizon, pi/2 = zenith
        t = seg / segs * math.tau
        r = radius * math.cos(phi)
        return (r * math.sin(t), FLOOR_Y + radius * math.sin(phi) * 0.55, -r * math.cos(t))

    for b in range(bands):
        c0 = lerp(horizon, zenith, b / bands)
        c1 = lerp(horizon, zenith, (b + 1) / bands)
        for sgi in range(segs):
            base = len(mesh.pos)
            mesh.pos += [p(b, sgi), p(b, sgi + 1), p(b + 1, sgi + 1), p(b + 1, sgi)]
            mesh.nrm += [(0, 1, 0)] * 4
            mesh.col += [c0, c0, c1, c1]
            mesh.idx += [base, base + 1, base + 2, base, base + 2, base + 3]


def build():
    meshes = []

    # ---- sky ----
    m = Mesh("sky")
    sky(m, 60, 6, 24, horizon=(0.10, 0.11, 0.18, 1), zenith=(0.03, 0.03, 0.07, 1))
    meshes.append(m)

    # ---- floor: two-tone rings ----
    m = Mesh("floor")
    disc(m, FLOOR_Y, 0, 4.0, 48, (0.16, 0.16, 0.20, 1))
    disc(m, FLOOR_Y, 4.0, 4.35, 48, (0.32, 0.30, 0.22, 1))  # accent ring
    disc(m, FLOOR_Y, 4.35, 11.0, 48, (0.12, 0.12, 0.155, 1))
    disc(m, FLOOR_Y, 11.0, 12.0, 48, (0.09, 0.09, 0.12, 1))
    meshes.append(m)

    # ---- ring of columns + lintels ----
    m = Mesh("columns")
    ncol = 8
    for i in range(ncol):
        t = i / ncol * math.tau
        cx, cz = 9.5 * math.sin(t), -9.5 * math.cos(t)
        column(m, cx, cz, 0.28, FLOOR_Y, FLOOR_Y + 3.4, 10, (0.42, 0.43, 0.50, 1))
        # a plinth under each column
        box(m, cx, cz, 0.9, 0.9, FLOOR_Y, FLOOR_Y + 0.18, (0.30, 0.31, 0.37, 1), ry=t)
    meshes.append(m)

    # ---- pedestals near the centre (places to park nodes) ----
    m = Mesh("pedestals")
    for (cx, cz, ry) in [(-2.6, -2.6, 0.8), (2.6, -2.6, -0.8), (0.0, 3.4, 0.0)]:
        box(m, cx, cz, 1.1, 0.7, FLOOR_Y, FLOOR_Y + 0.9, (0.16, 0.28, 0.30, 1), ry=ry)
        box(m, cx, cz, 1.25, 0.85, FLOOR_Y + 0.9, FLOOR_Y + 0.98, (0.35, 0.55, 0.55, 1), ry=ry)
    meshes.append(m)

    return meshes


def glb(meshes):
    bin_data = bytearray()
    accessors, buffer_views, out_meshes, nodes = [], [], [], []

    def view(data, target):
        off = len(bin_data)
        bin_data.extend(data)
        while len(bin_data) % 4:
            bin_data.append(0)
        buffer_views.append(
            {"buffer": 0, "byteOffset": off, "byteLength": len(data), "target": target}
        )
        return len(buffer_views) - 1

    for mi, m in enumerate(meshes):
        pdata = b"".join(struct.pack("<3f", *p) for p in m.pos)
        ndata = b"".join(struct.pack("<3f", *n) for n in m.nrm)
        cdata = b"".join(struct.pack("<4f", *c) for c in m.col)
        idata = b"".join(struct.pack("<I", i) for i in m.idx)
        xs, ys, zs = zip(*m.pos)
        pv, nv, cv, iv = (
            view(pdata, 34962),
            view(ndata, 34962),
            view(cdata, 34962),
            view(idata, 34963),
        )
        base = len(accessors)
        accessors += [
            {
                "bufferView": pv,
                "componentType": 5126,
                "count": len(m.pos),
                "type": "VEC3",
                "min": [min(xs), min(ys), min(zs)],
                "max": [max(xs), max(ys), max(zs)],
            },
            {"bufferView": nv, "componentType": 5126, "count": len(m.nrm), "type": "VEC3"},
            {"bufferView": cv, "componentType": 5126, "count": len(m.col), "type": "VEC4"},
            {"bufferView": iv, "componentType": 5125, "count": len(m.idx), "type": "SCALAR"},
        ]
        out_meshes.append(
            {
                "name": m.name,
                "primitives": [
                    {
                        "attributes": {
                            "POSITION": base,
                            "NORMAL": base + 1,
                            "COLOR_0": base + 2,
                        },
                        "indices": base + 3,
                        "material": 0,
                    }
                ],
            }
        )
        nodes.append({"mesh": mi, "name": m.name})

    doc = {
        "asset": {"version": "2.0", "generator": "wk gen-home-world"},
        "scene": 0,
        "scenes": [{"nodes": list(range(len(nodes)))}],
        "nodes": nodes,
        "meshes": out_meshes,
        "materials": [
            {
                "name": "vertex-coloured",
                "pbrMetallicRoughness": {
                    "baseColorFactor": [1, 1, 1, 1],
                    "metallicFactor": 0.0,
                    "roughnessFactor": 1.0,
                },
            }
        ],
        "accessors": accessors,
        "bufferViews": buffer_views,
        "buffers": [{"byteLength": len(bin_data)}],
    }

    jdata = json.dumps(doc, separators=(",", ":")).encode()
    while len(jdata) % 4:
        jdata += b" "
    total = 12 + 8 + len(jdata) + 8 + len(bin_data)
    out = bytearray()
    out += b"glTF" + struct.pack("<II", 2, total)
    out += struct.pack("<I", len(jdata)) + b"JSON" + jdata
    out += struct.pack("<I", len(bin_data)) + b"BIN\0" + bin_data
    return bytes(out)


if __name__ == "__main__":
    meshes = build()
    data = glb(meshes)
    OUT.write_bytes(data)
    tris = sum(len(m.idx) for m in meshes) // 3
    print(f"wrote {OUT} ({len(data)} bytes, {len(meshes)} meshes, {tris} tris)")

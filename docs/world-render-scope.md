# Top-down world render: scope

This is the scope for drawing a top-down orthographic picture of the game world under the path planes in the visualizer: textured terrain, water and prop models.

Zone foliage is out of scope. It depends on the zone-placement port, which is deferred (`mapblob-format.md` §7b.1).

The data comes from the fileserver on demand, as the pathing data does. A local Gw.dat is used only as a test oracle.

Scoped 2026-10-02 and implemented the same day: terrain, water and props. The scope below is kept as written; this section records what was built and where it differs.

## 0. Implementation

### Where it runs
The render is **baked on the CPU into a flat image when the pathing data is generated**, not drawn live on the GPU. Pathing generation runs in the desktop loader thread, the CLI and the headless relay (which has no GPU stack), so the renderer is plain Rust in the lib crate (`src/render/`).

### Flow
1. **Bake with the pathing.** `PathingStore::load_chunk` bakes the render after generating pathing (`bake_render`). A render failure is reported as `Progress::RenderFailed` and does not fail the pathing.
2. **Fetch the inputs.** `render_inputs` fetches in two rounds:
   - the terrain textures (`0x11000002`) and every prop model (`0x11000004`)
   - then the textures those models list (`0xBBB`)

   Downloads are retried on a fresh connection. With `best_effort`, render files that keep failing are skipped. Pathing models are never skipped, so pathing can't be cached without one.
3. **Cache.** The image is stored as `cache/render/<mapfile>-r<file>-v<RENDER_VERSION>.gwri`. `RENDER_VERSION` is separate from the pathing `FORMAT_VERSION`, so render changes re-render without regenerating pathing.
4. **Serve.** `PathingStore::load_render` returns the cached image, or bakes it for maps whose pathing was cached before rendering existed. The relay serves it at `GET /api/render/<mapfile_id>`.
5. **Draw.** The visualizer gets it as `Event::Background`, composites the terrain and the visible props, and uploads the result as textures of at most 2048 px a side (`app.rs`, `Background`). It's drawn under the path layers. The Layers panel has:
- a "World render" toggle and an opacity slider
- a "Props without planes" toggle for every drawn prop that owns no path plane (decoration), which leaves the terrain and the walkable props
- a Props list that hides or shows each prop

A prop is drawn only when both its layer and its own checkbox allow it.

### Per-prop sprites
Each prop is rasterized on its own over the terrain (`props::rasterize`). Its depth test is against the terrain only, and the result is trimmed to the pixels where the prop is above the terrain. That is its **sprite**: colour and elevation per pixel.

`WorldRender::compose` puts the visible sprites over the terrain with a z-buffer, in prop order. With every prop visible, this gives the same picture as drawing all props into one depth buffer. Props that interpenetrate (rocks, buildings in cliffs, trees through bridges) still occlude correctly, which a painter's-order composite wouldn't.

When a prop is toggled, the visualizer re-composites only the 2048 px tiles that its sprite touches.

### Render format (`render::WorldRender`, file version 2)
- `GWRI`, `u32` version, 4 × `f32` bounds (`[min_x, min_y, max_x, max_y]` of the whole image), `u32` width and height.
- The props table: every placed prop's model file id and sprite index (`u32::MAX` if it drew nothing).
- The sprite table: per sprite, its prop, its pixel rect in the image and its position in the atlas.
- Three length-prefixed images:
  - the terrain (with water) as a JPEG at quality 85
  - the sprite atlas's colour as a JPEG
  - the atlas's depth as a 16-bit grey PNG: `0` is empty, and higher is above. The steps are 1 world unit, or coarser if the elevation range needs more than 65534 of them.
- The atlas is shelf-packed, 4096 px wide, and widened if it would pass the JPEG side limit. Sprites are aligned to 16 px, and their edge pixels are repeated into the padding, so JPEG blocks never mix two sprites.
- 12 world units per pixel (`bake::SCALE`), at most 8192 px a side.

| Map | Image | Props (drawn) | File | Bake, files cached |
|---|---|---|---|---|
| Jaga Moraine | 3328×5120 | 1879 (1382) | 15.2 MB: terrain 4.1, atlas 5.2, depth 5.5 | about 3 s |
| Eye of the North | 2560×5632 | 1082 (780) | 12.2 MB | about 2.7 s |

The single-image format (v1) was about 5 MB and 1 s for Jaga Moraine. The all-props composite matches v1 apart from JPEG noise: a mean difference of 1.7/255, and 0.06% of pixels differing by more than 24.

### CLI
- `cargo run --bin gw-nav-cli -- render <mapfile_id> [--refresh] [--out x.jpg]` loads or bakes the cached render.
- `--scale <units/px> --out x.png` renders a preview at another scale without caching it.

### Modules

| Module | What |
|---|---|
| `render::atex` | ATEX/ATTX decoder: the run-length pre-pass, then DXT1/3/5. All 626 reference textures decode. GWMB decodes to BGRA, so standard DXT bit order applies (red in the top 5 bits). |
| `render::terrain` | Texture splatting, as planned (Phase 3). Texture index = terrain file-ref index (GWMB ignores tag 4). Mip level from texels per pixel. Hill-shading from normals with a fixed light from the north-west. Water is tinted over terrain below the surface. |
| `mapfile::environment` | Lighting (section 3) and water (section 6) records. The tail after section 7 doesn't match GWMB's "section 8", so the first record is used. |
| `render::model` | `0xBB8` geometry, see below. 270 of 272 reference models parse; the two with morph targets are skipped. |
| `render::props` | Props placed with the collision transform (yaw and scale; `pathgen::props`, without the rounding), rasterized one sprite per prop, in parallel across props. Depth-tested against the terrain elevation, flat-shaded, mip-sampled, texels with alpha below 0.5 cut out. |

### `0xBB8` model findings
Fileserver models are all in the "other" format (`0xBB8`…`0xBC0` chunks). GWMB's parse of it drifts after the first submesh.

- **Submesh layout:** the 32-byte header, then:
  - `ni × u16` indices
  - `nv × 12` positions
  - `nv × 4` per-vertex data
  - **an extra `nv × 4` stream for each of submesh flags `0x8` and `0x10`** (always set together in the samples). GWMB misses these, which is why its offsets drift.
  - the UV section: `u16 runs_u, u16 runs_v`, `(runs_u + runs_v) × 4` bytes of run lengths and offsets, then `nv × sets × 4` bytes of `u16` fractions
  - `groups × 3 + bones + triangle_groups × 12` bytes
- **UVs:** a coordinate is `fraction / 65535 + offset[run]`. The runs continue across UV sets (`MdlDecomp_ConvertSubmesh @79a3e0`).
- **Base texture, Prophecies/Factions:** the material index picks a shader. A shader's last header byte is its slot count. The slot arrays are struct-of-arrays: `u16` flags, `u8` UV set (255 skips the slot, 253 takes the next slot's set), 4 zero bytes, `u8` blend, `u8` texture.
- **Base texture, Nightfall/EotN** (`texture_groups > 0`): the material index picks a 9-byte group; byte 6 is its slot count. The table after the groups is `slots × u16` flags, then `slots × u8` texture indices.
- **Textures:** `0xBBB` holds `u32`, `u32 count`, then 6-byte file references.

### Still open
- Zone foliage (deferred, as scoped).
- The environment's active record selection.
- Textures beyond each submesh's base texture (detail and specular layers are ignored).
- Full prop tilt (only yaw is applied).
- The web build compiles and bundles, but hasn't been run in a browser.

## 1. Approach

The render only has to look right. Unlike the pathing port, it doesn't need to be bit-exact.

GuildWarsMapBrowser (GWMB, `G:\dev\gw\GuildWarsMapBrowser`) already renders all of this from Gw.dat. It also has a top-down orthographic export (`MapBrowser.cpp` L545–705). So most of the work is porting readable C++, not reverse-engineering the client.

The catch is that GWMB reads stage-2 (bloated) chunks only. Where the fileserver's stage-1 data differs, we decode it ourselves; see Phase 1.

What the existing code already provides:

| Need | Where |
|---|---|
| Map file download and cache, asset manifest | `pathing::PathingStore` (`map_file`, `cache/files/*.bin`) |
| Prop model files for a map | `PathingStore::download_models` |
| Terrain heights (bit-exact) | `mapfile::terrain::TerrainStrip` |
| Terrain texture indices, water mask, chunk grid | `TerrainStrip::parse_with_surface` → `TerrainSurface` (Phase 1) |
| Prop placements and their transform | `mapfile::props::PropsStrip`; `mapblob-format.md` §6 Props |
| Map bounds | `mapfile::params` |
| A wgpu paint callback with a world-unit view uniform | `src/bin/visualizer/render.rs` (`Gpu`, `DrawMap`) |

The background shares the path planes' view uniform, so the two line up exactly.

## 2. Phases

Sizes are estimates of new Rust code. For scale, `pathgen` is about 3,500 lines.

### Phase 1: stage-1 terrain surface tags (done, about 140 lines)

`TerrainStrip::parse_with_surface` decodes tags 2, 4, 5, 3 and 7. It matches stage 2 byte for byte on both test maps (`surface_matches_bloated_oracle`). The layout is in `mapblob-format.md` §6, terrain.

Findings:
- **Copied unchanged:** tag 2 (texture index per sample), tag 3 (water mask) and tag 7 (chunk grid). Only tags 4 and 5 are bit-packed.
- **Tag 4:** has as many entries as the terrain file-ref list (`0x11000002`). It is probably texture index → file-ref entry; this is unconfirmed.
- **Tag 9:** stage-2 generated lighting. Not needed; lighting is computed from the heights.
- **Kept apart from pathing:** `TerrainStrip::parse`, which pathing uses, doesn't parse the surface. A render-only parse failure can't break pathing.

### Phase 2: texture decode (about 1,300 lines)

New module `src/texture/`. Sources are in GWMB `SourceFiles/`:
- **ATEX/ATTX container and dispatch:** `AtexReader.cpp` (311 lines).
- **GW's custom ATEX compression:** `AtexDecompress.cpp` plus `AtexAsm.cpp` (about 1,000 lines). The original was assembly. This is the hardest-to-read code in the whole job.
- **Plain DXT1–5, DXTN and DXTL to RGBA:** a crate (`texture2ddecoder` or `bcdec_rs`) or hand-written code.
- **Fetching:** textures are fetched through the manifest and file client. A `download_files` helper, generalised from `download_models`.

Verify against RGBA dumps exported once from GWMB.

### Phase 3: terrain mesh, blending and lighting (about 400 lines Rust + 60 WGSL)

**Corner masks and UV layers.** Port GWMB `Terrain.cpp` `GenerateTerrainMesh` (about 250 lines):
- For each cell, take the texture indices at its 4 corners and build a corner bitmask per distinct texture.
- `VARIANT_LOOKUP` (16 entries) maps each mask to a tile quadrant and a 180° rotation flag.
- The base texture gets a random quadrant from a Park–Miller PRNG per 32×32 chunk (multiplier 48271, seed `cz ^ (cx << 16)`).
- At most 3 UV layers per cell.

**Textures.** A 2D texture array of 256² tiles, from the terrain file-ref list (indexed through tag 4 if that's confirmed). Skip the normal-map files GWMB skips (`draw_dat_browser.cpp` L1491–1495).

**Blend.** `TerrainRevPixelShader.hlsl`: `r = t0`, then `lerp(r, t1, t1.a)`, then `lerp(r, t2, t2.a)`, then multiply by the lighting.

**Lighting.** Normals from the heights; directional light colours from the Environment chunk (`0x10000009`, which has the same layout in both stages).

**Depth.** Terrain seen from straight above is a heightfield, so it needs no depth buffer.

### Phase 4: water and shore (about 200 lines)

**Water.** The water level comes from Environment sub-chunk 6 (GWMB `FFNA_MapFile.h`, about L1054–1190). Draw a flat tinted or textured surface where the terrain is under water, using the tag 3 mask plus the heights.

**Shore (optional).** A strip mesh from `0x10000010`, following GWMB `draw_dat_browser.cpp` L1968+.

### Phase 5: visualizer integration (about 500 lines)

- **Draw order:** new pipelines in `render.rs` (terrain, water, props), drawn before the path fills with the same `View` uniform.
- **Layer toggles:** a "World" group in the visualizer's Layers panel (terrain, water, props, lighting).
- **Depth buffer for props:** `depth_buffer` in eframe's native and web options. The projection is orthographic with z from the height; GW heights are negated.
- **`WorldScene` in the lib crate:** decoded meshes and RGBA textures, shared by native and web. `source.rs` loads it after `PathingData`, as it already does for annotations.
  - Locally, it is built on the loader thread.
  - On the web, either the relay serves raw cached files and the wasm side decodes them, or the relay serves a serialized scene.
- **PNG export:** "Save top-down PNG" at N px per 96-unit sample, for comparison with GWMB's export.

### Phase 6: prop models (about 3,000–4,000 lines)

New module `src/model/`, ported from GWMB:
- **Geometry:** `FFNA_ModelFile.h` (1,577 lines: `0xFA0` geometry with FVF vertex decode, `0xFA5` texture refs, `0xFAD` material refs) and parts of `FFNA_ModelFile_Other.h`.
- **Older `0xBB8` format:** `Parsers/BB8GeometryParser.h` and `VLEDecoder.h`.
- **Materials:** `AMAT_file.h`. Top-down, the first diffuse texture with alpha test should be enough, which skips most of the material system.
- **Extra downloads:** each model's textures must be fetched too, adding tens of MB per map on first load.
- **Transform:** the `PropsStrip` rotation/scale maths, in float without the collision rounding.
- **Draw:** an opaque pass, then an alpha-tested pass, both depth-tested.
- **Unsupported variants:** skip and log model variants that aren't supported yet, rather than failing the map.

## 3. Summary

| Tier | Phases | New code | Notes |
|---|---|---|---|
| Textured terrain + water | 1–5 | about 2,700 lines | about 0.75× `pathgen`, but a port of readable code, checked by eye |
| + prop models | 6 | about 3,500 lines | about 1× `pathgen` on its own, with an unknown tail of format variants |
| **Full ortho render** | 1–6 | **about 6,000 lines** | about 1.5–2× the pathing port's size, lower difficulty per line |

The suggested shipping order, each step usable on its own:
1. Shaded relief from the heights only, before any textures. This is about a day, and it tests the Phase 5 plumbing early.
2. Textured terrain.
3. Water.
4. Props.

## 4. Risks

- **Model format variants:** Prophecies/Factions vs Nightfall/EotN, and older layouts. These are most of Phase 6's uncertainty.
- **Web payload:** the browser would download every texture and model per map. The fallback is a relay-baked PNG, which needs headless wgpu or a CPU rasterizer in the relay.
- **ATEX decompressor:** the original was assembly, so expect a debugging loop against reference outputs.
- **Tag 4's meaning:** if it isn't the texture → file-ref map, Phase 3 needs a Ghidra look at `TerrainBuild_SetIndexArray`.

## 5. Verification

- **Unit tests:**
  - the terrain surface matches stage 2 (done)
  - ATEX decode matches reference RGBA dumps
- **Visual:** `cargo run`, load Jaga Moraine and Eye of the North, and check:
  - the path planes line up with roads, cliffs and bridges
  - the PNG export matches GWMB's top-down export
- **Web:** run the web build through the relay to confirm load time and memory are acceptable.

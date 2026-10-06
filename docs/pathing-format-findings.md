# Pathing data in fileserver map blobs: findings

Date: 2026-09-28
Samples:
- `290943.mapblob`: mapid 546, Jaga Moraine. An earlier version of this doc said "mapid 109, The Amnoon Oasis". That was wrong: `GW-Pathing-Map-Visualization/.../mapinfo.csv` and GuildWarsMapBrowser's `data.csv` both give 546.
- `290923.mapblob`: mapid 232, Shadow's Passage

> **Correction:** the samples used below (and in sections 1–3) are *stale revisions*. Map file ids are base ids, and the fileserver serves the original revision under them. The current revision comes from the asset manifest (290943 → 381071, 288299 → 380894); see `mapblob-format.md` §1. The conclusions still hold, but chunk sizes and contents differ from the current files.
>
> The byte-level format spec now lives in **`mapblob-format.md`**. This file is the research log.
>
> **Update (Ghidra, Gw.exe build 38888): the answer is in section 0 below.** The fileserver serves *stage-1* ("bloat source") map files. The client **generates** the trapezoid navmesh itself, during "bloat". Sections 1–5 are the original investigation and are kept for reference.

## 0. Resolution: the client builds the navmesh (Ghidra)

### Stage-1 vs stage-2 chunks
- `0x10xxxxxx` / `0x11xxxxxx` chunks are **stage-1**. This is what the fileserver sends.
- The client "bloats" each stage-1 chunk into a **stage-2** `0x20xxxxxx` / `0x21xxxxxx` chunk. The bloated file is what tools such as GuildWarsMapBrowser read from Gw.dat.
- Stage-2 `0x20000008` is the navmesh the old tool expected. Its layout is documented in `G:\dev\gw\GuildWarsMapBrowser\FFNA_ImHexPatterns\gw_file_pattern_complete.hexpat`, around line 2767, with a C++ reader in `SourceFiles/FFNA_MapFile.h:1473`.
- Source files named in asserts: `Engine\Map\MapData.cpp`, `Engine\Map\Path\{PathApi,PathDataImport,PathData,PathBuild,PathBsp,PathObstacle,PathTracer?}.cpp`, `Engine\Map\Terrain\TrnDataBloat.cpp`, `Engine\Map\Props\PrDataBloat.cpp`, `Engine\Map\Zones\ZnDataBloat.cpp`, `Gw\Download\DnBloat.cpp`.

### The path chunk bloat pipeline
The call chain is:

`MapData_BloatPathing @712ac0` → `PathApi_CreateFromData @721c40` → `PathStatic_Import @724ec0`

`PathStatic_Import` runs a table of stage functions at `0xbfa338`. Each one reads stage-1 input and/or writes stage-2 output:

| # | Function | Stage-1 input | Stage-2 output |
|---|---|---|---|
| 1 | `PathChunk_ReadHeader @724d80` | `u32 sig 0xEEFE704C, u8 ver 0x0C, u32 seq` (9 B) | `u32 sig, u32 12, u32 seq` (12 B) |
| 2 | `PathChunk_ReadStartPoints @724be0` | tag 7: `u16 n`, `n × vec2f` | tag 7, copied as-is |
| 3 | `PathChunk_WriteMaps @724ae0` | none | tag 8: **all planes, generated** |
| 4 | `PathChunk_WriteIndices @724830` | none | tag 12: u16 list (model/prop indices) |
| 5 | `PathChunk_WriteObstacles @724930` | none | tag 13: obstacles, from zone placements (`Zones_GetPlacements`, `PathObstacle_BuildFromAgents`) |
| 6 | `PathChunk_ReadChecksum @724750` | tag 14: `u32 crc`, `u8` | tag 14: `u32 crc`, `u8 (crc != computed)` |
| 7 | `@7247f0` | tag `0xFF` | tag `0xFF` |

Tags 1–3 above mean stage header, tags 7, 8 and 12. Tags are written with `Chunk_write_tag_header`, which emits `u8 tag, u32 size` in stage 2. Stage-1 tag headers are smaller: `Chunk_read_tag_header(1, …)`.

The **tag-14 CRC** is `Crc_Compute` over the generated tag-8 payload, XORed with the CRC of the tag-13 payload. At first this looked like a byte-exact oracle, but it isn't: in both Gw.dat samples the client stored `mismatch = 1`, so its own output doesn't reproduce the checksum either. Use the Gw.dat stage-2 copies as the reference instead.

For 290943, the stage-1 `0x10000008` decodes as:
- sig `0xEEFE704C`, ver 12, seq `0x71`
- tag 7 with 6 start points: (16164, −20552), (−11256, −2684), (1552, 26728), (−8264, −9860), (−13732, −24244), (0, 0)
- tag 14 with crc `0xEA92DA4A`, then `00`
- `0xFF`

### What the navmesh generator consumes
Tag 8 is built by `PathChunk_BuildPathData @72ffe0`, which calls these steps in order:
1. `TerrainMap_ExportHeights @74b400`: the heightfield from the *already bloated* terrain (the stage-1 `0x10000002` chunk run through `TrnDataBloat`).
2. `Collision_build_geometry @715f50`: collision polygons (x, y, flags) from the map's collision data.
3. `PrApi_GetCollisionAndPortalPoints @738f60`: prop collision and portal points from the *bloated props*. These come from `0x10000004` plus the **prop model files** listed in `0x11000004` (client log strings: "missing collision data for model file … Rebloating").
4. `PathTracer_trace_and_cleanup @72e260`: traces the heightfield plus the collision into boundary segments.
5. The start points (tag 7) are quantized with `Math_Sin`… `>>2 <<2`, i.e. snapped to a 4-unit grid.
6. `PathData_process_prop_segments @72fe80`, then `PrApi_GetModels`.

After that, `PathData_build_maps @72fa00` calls `PathMap_Build @7318a0`. This is a full trapezoidal-map construction:
- `PathMapBuilder_InitSegments`
- `PathBuild_InsertEdgeEndpoints`
- `PathBuild_SplitTrapezoidsHorizontal`
- `PathBuild_CreateTrapezoids`
- `PathBuild_RemoveDegenerateTrapezoids`
- `PathBuild_SortAndCreatePortals`
- the BSP / point-location tree

It then serializes with `PathChunk_WriteHeader`, `WriteVertices`, `WriteEdges`, `WriteNodes` and `WritePortals`.

### Consequence for FileConn
Getting pathing **on demand from the fileserver** means reimplementing the client's bloat for everything the path builder depends on:
- terrain heightfield bloat
- prop placement bloat, including downloading and parsing collision from each referenced model file
- collision polygons
- zone placements (for obstacles)
- the path tracer
- the trapezoid builder

The stage-2 layout (MapBrowser pattern) is the serialization target, and the tag-14 CRC verifies the result.

### Tag headers (`Engine\Map\Services\MsChunk.cpp`)
`Chunk_read_tag_header(stage, …)` / `Chunk_write_tag_header(stage, …)`. The stage values are 0, 1 = `MAP_STAGE_STRIP` (what the fileserver sends) and 2 = bloated.
- Stage 1: a tag is a bare `u8` with **no size**. Each reader knows its own payload length.
- Stages 0 and 2: `u8 tag, u32 size`. Sizes that aren't known up front are back-patched by `Chunk_finalize_tag_size`.

### Map chunk descriptor table (`0xa6f9b8`, 0x28-byte entries, the name is the *last* field)
Entries are indexed by `chunk_id & 0xff`. Only chunks that have a bloat function are listed. The state offsets are fields of the bloat state that is passed to later stages.

| id | name | bloat fn | state slot |
|---|---|---|---|
| 02 | Terrain | `MapData_BloatAndImportTerrain @712340` | `+0x2c` |
| 03 | Zones | `MapData_BloatAndImportZones @712560` | `+0x28` |
| 04 | Props | `MapData_BloatAndImportProps @712700` | `+0x24` |
| 08 | Path | `MapData_BloatPathing @712ac0` | none |
| 0A | Locations | `@712e00` | |
| 0C | Map Parameters | `@712e90` | |
| 0E | Collision | `MapData_BloatAndImportCollision @712fc0` (a plain byte copy; 9 bytes / empty in 290943) | `+0x30` |
| 11 | Sight | `@713260` | |
| 16 | PathEngine | `@7136c0` | |

The other ids have no bloat function: Header, Water, Mission, Environment, Light, Shore, Sound, CubeMap, VisData and Occluders. Mission (07) does have import/export functions. The processing-order list at `0xa6fd18` is: 12, 1, 4, 3, 14, 19, 2, 7, 6, 8, 9, 10, 15, 16, 17, 18, 20, 21, 22, 13.

**Path depends on terrain (+0x2c), props (+0x24), zones (+0x28) and collision (+0x30).**

### Terrain bloat (`TrnDataBloat.cpp`, stage table `0xa77358` of `(fn, progress-cost)` pairs)
Stage-1 `0x10000002` starts with `u32 0x87821134, u8 0x11` (`Terrain_bloat_validate_header`), then the stage-1 tags:

- **tag 0** (`Terrain_bloat_convert_header @759120`) is a packed `u32`:
  - bits 0–5 × 3072 = a float (height scale?)
  - bits 6–7 = `0b10`
  - byte 1 = 96 (`XY_DIST`)
  - `dimY = (byte2 + 1) * 32`, `dimX = (byte3 + 1) * 32`

  Then a `u8` angle (`× 282.74335 / 45720`, i.e. degrees → radians over a 45720 scale), a `u16`, and 2 index bytes. The client asserts `dims * 96 == mapRect` extent. **Verified on 290943:** 416×640 cells × 96 = 39936×61440, which matches the `0x1000000c` bounds.
- **tag 1**: heights via `TrnCodecHeight_Decode`. A custom codec; this is the ~245 KB high-entropy head. Output is `dimX*dimY` floats, reordered into 32×32 tiles.
- **tag 2**: material (`TerrainData_AppendBlock`).
- **tags 4 and 5**: bit-packed palettes (`TrnBitStore_*`).
- **tag 3**: water mask (dim/4 bytes).
- **tag 7**: quadtree (`TerrainChunkGrid_*`).
- **tag 9**: normals / intensity, *generated* (`TerrainTexIntensity_Build`).
- `0xFF`

### Props bloat (`PrDataBloat.cpp`)
The chain is `PropData_Bloat` → `PropDataBloat_process @73e6b0` → `PropData_read_prop_definitions` → `PropCollision_Build @73a960`.

For each prop, `PropFileCache_GetOrCreate` → `Model_open_collision_file` / `Model_parse_ffna_file` **opens the prop's model file by file id and reads its collision**. That means downloading every model in `0x11000004` (185 files for 290943). The points are then transformed and simplified (`PropCollision_TransformPoints`, `PropCollision_SimplifyVertices`).

### Porting roadmap (each step checked against a Gw.dat stage-2 oracle, used in tests only)
1. FFNA container + `MsChunk` tag reader/writer (stage 1 / 2) in Rust. Add a test-only extractor for the stage-2 copy of a file from `E:\GW\GW10\Gw.dat`.
2. Terrain bloat → byte-compare with the oracle's `0x20000002`. The main unknown is `TrnCodecHeight_Decode`.
3. Zones bloat (may need the zone `.ini` refs from `0x11000003`).
4. Props bloat: model downloads, model FFNA collision parse, transform and simplify → compare with `0x20000004`.
5. Collision: a copy.
6. Path:
   - `TerrainMap_ExportHeights`
   - `PathTracer_*`
   - segment assembly
   - `PathMap_Build` (trapezoidation + BSP)
   - obstacles
   - serialize, then check the tag-14 CRC and compare with the oracle's `0x20000008`

Step 1 is done: `src/mapfile/{ffna,tags,path}.rs`, `fileconn::AssetManifest` and `examples/dat_extract.rs`. Comparing stage 1 with stage 2 on the current revisions shows that only Terrain, Zones, Props, Path, Locations and Sight change during bloat (table in `mapblob-format.md` §3).

Risk: **float determinism.** The client is 32-bit MSVC code: SSE for most float math, x87 in `__CIcos`/`__CIsin`, and a custom `Math_Sin` that actually rounds to int. Byte-exact output will need care with f32 vs f64 and rounding modes.

Both were downloaded with `gw-nav-cli download` and decompressed with the Rust port of `xentax.c`. The output is byte-identical to the original C decompressor and matches the sizes in the server manifest, so the blobs are intact.

## TL;DR

1. **The old parser can't load these blobs.**
   - The FFNA container itself parses fine: 23 chunks, no corruption.
   - The pathing code (`references/pathingmaps/Pathingmap Builder/FFNA.cpp`) looks up chunk `0x20000008` (trapezoids) and `0x20000004` (transitions). **Neither ID exists.**
   - Every chunk in these files uses the `0x1000xxxx` / `0x1100xxxx` range.
   - Pointing the old code at the renumbered IDs (`0x10000008` / `0x10000004`) also fails. The byte layout inside the chunks is different.
2. **The pathing trapezoids are not stored as raw records anywhere in the map blob.**
   - A byte-by-byte scan of both files for runs of 44-byte `GWPathingTrapezoid` records (the old on-disk layout, which is also the in-memory layout) found **zero** runs of 8 or more.
3. **Most chunks now have an identified role** (map bounds, props, zones, terrain, water, light, shore, sound, and more; table below). None of them is the pathing data. Where the pathing lives is still **open**. The most likely options are listed below, along with a concrete way to settle it using GWBS.

## 1. Does the existing parsing code still work?

I ported the old logic faithfully to Python and ran it against both blobs:
- the container reader, `FFNA::FFNA`
- `ReadTrapazoids`
- `ReadTransitionVectors`

| Step | Result |
|---|---|
| `FFNA::FFNA`: `ffna` magic, type byte at +4, then `(u32 type, u32 len, data)` chunks from +5 | **OK.** Type 3 (map), 23 chunks, all lengths consistent |
| `ReadTrapazoids()` looks for chunk `0x20000008` | **FILE_CORRUPT:** no such chunk |
| `ReadTransitionVectors()` looks for chunk `0x20000004` | **FILE_CORRUPT:** no such chunk |
| Same, remapped to `0x10000008` | **FILE_CORRUPT:** the "skip first section" length at +13 points far past the end (4,619,430 in a 67-byte chunk) |
| Same, remapped to `0x10000004` | **FILE_CORRUPT:** section lengths are garbage (e.g. 25,983,640) |

`ProcessFile()` in `Pathingmap Builder.cpp` catches these exceptions and silently skips the file. The old tool would therefore produce **no .pmap output** for current map files.

Side note: in the current files, chunk `…04` is clearly **prop placement**, and its companion `0x11000004` lists 185 model files. That fits with `ReadTransitionVectors` being marked "obsolete" and its only use being commented out in `CompileAdjacentList`. The old "transitions" were probably never real portal data.

## 2. What is in the map blob now

### Container
The FFNA container is unchanged:
- `66 66 6e 61` ("ffna")
- type byte `03`
- then a flat list of `(u32 chunk_id, u32 length, payload)`

The **chunk payload format** changed from what the old code expects. Almost every payload starts with a 4-byte signature. That signature is either an ASCII FourCC (`LITE`, `SHOR`, `CUBE`, `VISD`, `SGHT`, `msnd`, `BCUF`) or what looks like a 32-bit type hash (`24118739`, `efee4359`, `4c70feee`, …). After the signature come tagged fields, and many payloads end in a `0xFF` terminator byte. Several payloads are bit-packed rather than byte-aligned.

### File references (`0x11xxxxxx` chunks)
Every `0x11xxxxxx` chunk is a list of file references:
- a 4-byte signature `30 98 93 29`, then one byte `01`
- then 6-byte entries `(u16 w0, u16 w1, u16 0)`

Each entry decodes with GW's filename-hash formula, `file_id = (w0 - 0xFF00FF) + w1 * 0xFF00`. **Verified:** I downloaded all 35 files referenced by `0x11000006` and `0x11000009` from the fileserver, and every one is a valid `ATEX` texture.

### Chunk inventory (290943; 290923 has the same chunk list)

| Chunk | Size | Contents | Confidence |
|---|---:|---|---|
| `0x10000000` | 8 | signature + `u32 3`, probably a format/version header | guess |
| `0x1000000c` | 41 | **map bounds**: floats `-18432, -30720, 21504, 30720` (minX, minY, maxX, maxY), then ~20 unknown bytes. Matches `MapCtx::map_boundaries` | high |
| `0x10000004` | 35,796 | **prop placement** (positions/rotations), with models listed in `0x11000004` | high |
| `0x11000004` | 1,115 | 185 file refs (prop models) | high |
| `0x10000003` | 3,228 | zone data. It contains the UTF-16 path `Chapter4\Missions\Mountain\Ridge\Zones\MountainRidgeMidGrass.ini` plus floats. Probably `MapCtx::zones` | medium |
| `0x11000003` | 743 | 123 file refs for the zones | high (refs) |
| `0x1000000e` | 9 | signature + zeros | unknown |
| `0x10000013` | 65,776 | `CUBE`: environment cubemap. Contains an embedded `DDS`/`DXT1` texture. It is only 12 bytes in 290923 | high |
| `0x10000002` | 1,046,051 | **terrain**; see section 3 | high |
| `0x11000002` | 353 | 58 file refs (terrain textures) | high (refs) |
| `0x10000007` | 148 | float pairs inside the map bounds, nearly the same values as `0x10000008` (differences in the low mantissa bits), plus extra fields | medium |
| `0x10000008` | 67 | tag `07`, count 6, then 6 × vec2f inside the map bounds. Likely **spawn points** (cf. `MapCtx::spawns1..3`), **not pathing** | medium |
| `0x10000006` + `0x11000006` | 5 + 197 | 32 texture refs (ids 10052–10114, step 2), the same list in both maps. Looks like **animated water** frames | medium |
| `0x10000009` + `0x11000009` | 433 + 23 | many `1.0f` params, plus 3 texture refs. Likely **water settings** (`MapCtx::water`, flags bit 1) | medium |
| `0x1000000a` | 9 | signature + zeros | unknown |
| `0x1000000f` | 247 | `LITE`: lights (`MapCtx::lights`) | high |
| `0x10000010` | 17 | `SHOR`: shore (`MapCtx::shore`) | high |
| `0x10000011` | 9 | `SGHT` | unknown |
| `0x10000012` | 17 | `msnd`: map sound | high |
| `0x10000014` | 45,297 | `VISD`: visibility data. A header, then an offset table, then high-entropy data | medium |
| `0x10000015` | 399 | `BCUF`: occlusion volumes. Box corner floats and the UTF-16 string "New Occlusion Plane" | medium |

## 3. Where did the pathing go?

What I checked:
- **Raw trapezoid scan.** At every byte offset of both blobs, I tested for a run of `GWPathingTrapezoid` records:
  - 4 × u32 adjacency (index or `0xFFFFFFFF`)
  - 2 × i16 portal indices
  - 6 floats, with `YT > YB`, `XTL ≤ XTR`, `XBL ≤ XBR`, and all values finite with |v| < 1e6

  **No runs of 8 or more** were found in either file. The old format stored exactly this layout, so the pathing is not stored as it used to be.
- **Size.** Only `0x10000002` (1 MB) is big enough to hold a full pathing set. A single town like Amnoon Oasis should need tens to hundreds of KB of trapezoids.
- **Inside the terrain chunk (`0x10000002`)**, on a 16 KB entropy/float-density profile:
  - `0x0000`–`0x3CC00` (~245 KB): entropy 7.96 bits/byte, so it is compressed or bit-packed. It is **not** a GW-compressed stream at any byte offset. Trying the `xentax` decompressor at offsets 4–63 only gives degenerate all-zero output.
  - `0x3CC00`–`0x7DD1A`: low entropy (~3–4.5), byte maps (per-vertex/per-cell data).
  - `0x7DD1A`–`0x8E112`: ~66 KB of zeros.
  - `0x8E112`–end (~460 KB): **run-length encoded scanline masks**. There are long runs of `ff 12` (empty rows), skip/fill byte pairs, and small block headers (`40 00 00 00 60 00 00 00 …`). The chunk ends in a mip-like count table (`1f … 0f … 07 … 03`). These look like rasters (e.g. terrain holes/collision/texture blend), **not** trapezoids.
- **Referenced files.** The 35 files from the two small ref lists are all textures. The props (185), zone (123) and terrain (58) lists weren't downloaded, because their chunks' roles already explain them.

### Candidate locations, most likely first
1. **A separate pathing file that the map's ref lists don't name.** The client could reach it through `MapStaticData`/map id rather than from inside the map FFNA. The old builder scanned *all* of Gw.dat for type-3 files, so it never proved that pathing lived in the file that `map_zones.mapfile` names.
2. **Bit-packed inside the terrain chunk's ~245 KB high-entropy head.** It would need the new tagged/bit-packed serializer to be reverse engineered.
3. **A newer file revision (e.g. the Reforged-era client) with pathing moved to a new place.** The chunk-ID and serialization changes show the format has been revised since the old tool was written.

## 4. What the in-memory structures say to look for

`references/pathingmaps_in_client_memory.h` (GWBS) shows the loaded form. Compared with what the old tool extracted:

| In-memory (`gw::pathing`) | In old DAT format / old tool? | Notes |
|---|---|---|
| `Trapezoid` (48 B: `id` + 4 adjacent ptrs + `portal_left/right` u16 + 6 floats) | **Yes.** The old 44-byte record is this struct minus `id`, with pointers stored as indices | the same field order means the old format was close to a memory image |
| `PathingMap.zplane` / `subplane` (ground = -1, several maps per plane) | partly: the old tool counted one "plane" per tag-`2` section | it ignored subplanes, so its `Plane` value may conflate them |
| `Portal` (plane, neighbor plane, flags `0x4` = "not used for pathfinding", pair ptr, trapezoid list) | **No** | probably in old section tags other than `2` (the tag `0x0B` "length/2" arrays look like u16 trapezoid index lists) |
| `portal_trapezoids` (`Trapezoid**`) | **No** | the flat list that portals index into |
| `XNode` / `YNode` / `SinkNode`, `root_node` | **No** | a trapezoidal-map point-location tree: XNode = segment test (pos + dir), YNode = horizontal split, Sink = leaf → trapezoid. Needed for fast "which trapezoid is (x,y) in" |
| `dat_vectors` (vec2f array) | **No** | the comment says XNode/YNode positions are *indices* into this array. That fits the old chunk's first skipped section (the length at +13) |
| `h0010` vec2f array | **No** | unknown |
| `MapStaticData.trapezoidCount`, `nextTrapezoidId`, `map_id` | n/a | the global trapezoid id is what `PathContext.pathNodes` is indexed by |
| `BlockedPlaneArray` | never in the file | the server sends it (e.g. Foundry gates) |
| `MapCtx.map_boundaries` | not in the old tool | **found:** chunk `0x1000000c` |
| `MapCtx.spawns1..3` | not in the old tool | **likely:** `0x10000007` / `0x10000008` |

So even the old tool only recovered trapezoid geometry. It rebuilt adjacency itself (`CompileAdjacentList`) by matching shared Y edges, and it dropped the portals, the node tree, and the plane/subplane structure. Our visualizer and path builder will want the portals and the plane structure.

## 5. Recommended next steps

1. **Get ground truth from GWBS** in Amnoon Oasis (mapid 109):
   - the pathing map count, with the `zplane`/`subplane` of each
   - the trapezoid count, and a few trapezoids' 6 floats as raw hex
   - portal and node counts
   - the `dat_vectors` count

   Exact float bit patterns can then be searched for in any candidate file. That settles "where" immediately, even if the data is packed a little differently.
2. **Log file loads during map entry.** Hook or poll the `RecObject.file_id` of each file the client opens while loading the map. The pathing file (if separate) will be in that list, and each id can be pulled with `gw-nav-cli download`.
3. If the pathing turns out to be inside `0x10000002`:
   - Reverse engineer the tagged/bit-packed chunk serializer, starting with the small chunks (`0x10000008`, `0x1000000c`), whose contents are already partly understood.
   - Look in Gw.exe for the chunk-id constants (`0x10000002`, `0x20000008`) to find the reader.
4. Download and classify the remaining ref-list files (zones/props/terrain) only if steps 1–2 don't point somewhere first.

## Appendix: reproducing

The analysis scripts are in this session's scratchpad (a Python port of `FFNA.cpp`, the trapezoid scanner, the ref-list decoder, and the entropy profiler). The C decompressor oracle was built from `references/file/compression/src/xentax.c`.

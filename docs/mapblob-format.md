# Map file (`.mapblob`) format

This is the byte-level spec for Guild Wars map files as served by the fileserver, plus what the game client turns them into. It is built up as the Rust port (`src/mapfile/`) confirms each part. Each section is marked:

- **verified**: parsed by our code and/or checked against the client's own output
- **from Ghidra**: read from the client's code, but not yet exercised by our code
- **unverified**: a guess

For the research history, see `pathing-format-findings.md`. All integers are little-endian.

## 1. Getting the right file (verified)

Map file ids in MapDb, and in the game's SpawnInfo packet (StoC 405, first field), are **base ids**. The fileserver still serves the *original* revision under a base id. The current revision has a different id, which is looked up in the **asset manifest**:

1. Connect. The server hello carries 7 file ids. Id `[1]` is the asset manifest (`FileClient::asset_manifest_id`).
2. Download and parse the asset manifest (`fileconn::AssetManifest`).
3. `current_id = manifest.resolve(base_id)`, then download `current_id`.

Example: base 290943 (Jaga Moraine) resolves to 381071, and base 288299 (Eye of the North) to 380894. Only the current revision matches what the client actually uses. The Gw.dat stores the bloated file under both ids.

### Asset manifest (verified)
It is a flat array of `u32`:

```
u32 0                                    // leading terminator
repeat:
    u32 current_id
    u32 base_id
    u32 dependency_base_ids[...]         // textures, models, sounds, ...
    u32 0                                // record terminator
```

Manifest 389602 has 132,835 records with unique base ids, and every dependency id is itself a base id in the manifest. For a map, the dependency list includes the prop model files that bloat needs (see Props).

#### Finding map files in the manifest
The manifest has no file types, but map files have a clear signature: **no file depends on them, and they depend on many files** (their models and textures). In manifest 390279 (133,158 records):
- All 246 map files known to MapDb have this signature, with 74 to 940 dependencies (median 312).
- 370 root files have at least 50 dependencies: the 246 known map files plus 124 that no MapDb row names.

`AssetManifest::map_file_candidates` applies this rule (`MIN_MAP_DEPENDENCIES` = 50). `MapDb::record_manifest` stores the results in the `manifest_mapfiles` table, and `scan-manifest --verify` checks each one's FFNA type.

The manifest gives no **map ids**, and neither does Gw.exe. I read the `AreaInfo` table (`s_missionClientData`, returned by `ConstMission_GetClientData @5a89e0`; 0x381 records of 0x7C bytes at `0x96ee78` in build 38888):
- Its `file_id` field (+0x68) never equals a map's map file id, and is 0 for most maps.
- No field of any record matches any of the 384 known map files.

The server sends the map file id in SpawnInfo, so map ids still come only from logging map loads in game (GWBS `maploadlog.lua`).

### Transport and compression (verified)
Downloads come back compressed. `fileconn::decompress` decodes them, given the decompressed size from the file manifest packet. In Gw.dat, the decompressed size is the last `u32` of the stored stream. A Gw.dat MFT entry is 24 bytes: `u64 offset, u32 size, u16 compressed, u8 flags, u8, u32 id, u32 crc`. The special entries are 0 = MFT header, 1 = dat header, **2 = hash list** of `(u32 file_id, u32 mft_index)` pairs, and 3 = the MFT itself. See `examples/dat_extract.rs`, which is a test-oracle tool only.

**Flow control (verified live).** The client acknowledges every completed file with `u8 0xF3, u8 7, u16 8, u32 bytes_received`, which is the same packet as `RequestMore`. The server limits unacknowledged data per connection; about 120 KB was observed. Without the acknowledgement, a run of small downloads stalls after that much data, and the connection hangs until the client's read timeout. `references/file/cli.py` doesn't send it. With the acknowledgement, 30 consecutive model downloads took 5.3 s instead of 63.8 s.

### On-demand pathing (`pathing::PathingStore`)
`PathingStore::load(mapfile_id)` returns the cached pathing data, or else generates it:
1. Resolve the id through the asset manifest (cached as `manifest-<id>.bin`).
2. Download the map file.
3. Download the models of props without flag bit 0, over 4 connections.
4. Run `pathgen` and cache the resulting stage-2 path chunk as `pathing/<mapfile_id>-r<file_id>-v<FORMAT_VERSION>.path`.

Downloads are cached in `files/<file_id>.bin`, keyed by the exact revision. A cold load of Jaga Moraine (157 models) takes about 8 s, a cached load under 1 ms. The CLI is `gw-nav-cli pathing <mapfile_id> [--refresh]`. Tag 13 (obstacles) is written empty until zone placement is ported; bump `FORMAT_VERSION` when it is.

## 2. FFNA container (verified)

```
char[4] "ffna"
u8      type                             // 3 = map
repeat until end of file:
    u32 chunk_id
    u32 length
    u8  data[length]
```

`mapfile::Ffna`. Chunk order is the same in both stages.

## 3. Chunk ids and stages (verified)

`chunk_id = stage_byte << 24 | kind`:

| stage byte | meaning |
|---|---|
| `0x10` | stage 1 (`MAP_STAGE_STRIP`) chunk: **what the fileserver serves** |
| `0x11` | stage 1 file-reference list for the chunk kind |
| `0x20` | stage 2 (bloated) chunk: what the client stores in Gw.dat and loads |
| `0x21` | stage 2 file-reference list |

`kind` is `mapfile::ChunkKind`; the names are the client's own. The last column compares stage 1 against stage 2 for two maps (290943 and 288299, current revisions):

| kind | name | bloat |
|---|---|---|
| 00 | Header | copied |
| 02 | **Terrain** | **rebuilt** (2× larger) |
| 03 | **Zones** | **rebuilt** (3–6× larger) |
| 04 | **Props** | **rebuilt** (5× larger) |
| 06 | Water | copied |
| 07 | Mission | copied |
| 08 | **Path** | **generated** (75 B → 938 KB) |
| 09 | Environment | copied |
| 0A | Locations | +4 bytes (stage 1: 9 B, stage 2: 13 B) |
| 0C | Map Parameters | copied |
| 0E | Collision | copied |
| 0F | Light | copied |
| 10 | Shore | copied |
| 11 | Sight | generated (9 B → 285 KB, line of sight; not needed for pathing) |
| 12 | Sound | copied |
| 13 | CubeMap | copied |
| 14 | VisData | copied |
| 15 | Occluders | copied |
| `0x11xxxxxx` | file-ref lists | copied |

Kinds 01, 05, 0B, 0D and 16 (Editor (old), Obsolete (1), Obsolete (2), Editor, PathEngine) did not appear in the samples.

**Pathing needs Terrain, Zones, Props and Collision to be bloated before Path.** The client does them in that order: 12, 1, 4, 3, 14, 19, 2, 7, 6, 8, … (the processing-order list at `0xa6fd18`).

## 4. Tagged payloads (verified)

Chunk payloads are sequences of tagged sections (`Engine\Map\Services\MsChunk.cpp`; `mapfile::tags`):

| stage | section header |
|---|---|
| 1 | `u8 tag`, with no length; the reader knows each section's size |
| 2 | `u8 tag, u32 length` |

Tag `0xFF` ends a payload in both stages. In stage 2 it has length 0.

## 5. File-reference lists `0x11xxxxxx` (verified in an earlier session)

```
u32 0x29939830
u8  1
repeat: u16 w0, u16 w1, u16 0            // 6-byte entries
```

`file_id = (w0 - 0xFF00FF) + w1 * 0xFF00`

## 6. Chunk layouts

### 0x00 Header (verified, byte-identical in both stages)
8 bytes: `u32 0x39871124, u32 3`.

### 0x0C Map Parameters (verified; `mapfile::params`, the client's `MapParams.cpp`)
```
u32 0x5943EEEF
u8  2                                    // version
f32 min_x, min_y, max_x, max_y           // map bounds
u32 flags                                // top byte: map type (0 is treated as 1; type > 2 also sets |0x20)
                                         // bit 0: the map has water
u8  unknown[16]                          // differs between revisions; the client xors it with a hash of the file name
```

The map type selects the path tracer's slope limits, and the water bit enables walkable water. Examples: Jaga Moraine `0x04000020` (type 4, no water) and Eye of the North `0x04000021` (type 4, water).

### 0x08 Path, stage 1 (verified)
```
u32 0xEEFE704C                           // signature
u8  12                                   // version
u32 sequence                             // differs between revisions
u8  7                                    // tag: start points
u16 count
f32 points[count][2]
u8  14                                   // tag: checksum
u32 checksum
u8  0                                    // skipped by the client
u8  0xFF
```

### 0x08 Path, stage 2 (verified; generated byte-exact by `pathgen::chunk::bloat_path`, except tag 13)
```
u32 0xEEFE704C, u32 12, u32 sequence     // same sequence as stage 1
tag 7:  u16 count, f32 points[count][2]  // copied from stage 1
tag 8:  planes                           // generated, see below
tag 12: u16 count, u16 values[count]     // generated; count == plane count in both samples
tag 13: obstacles                        // generated from zone placements
tag 14: u32 checksum, u8 mismatch        // checksum copied from stage 1
tag 0xFF (length 0)
```

The client xors the CRCs of the tag-8 and tag-13 payloads it generated, compares that with `checksum`, and stores `mismatch = (computed != checksum)`. **In both samples `mismatch = 1`**, so the client's own output does not reproduce the stored checksum. It can't serve as a correctness test; the Gw.dat copy is the reference instead.

The details (`PathChunk_WriteMaps @724ae0`, `PathChunk_WriteObstacles @724930`, `PathChunk_ReadChecksum @724750`):
- `Crc_Compute @4716a0` is standard CRC-32 (IEEE, reflected `0xEDB88320`; table at `0x93e8a0`), called with initial value 0.
- The CRCs cover the section payloads only, without the `u8 tag, u32 len` headers.
- The accumulator starts at 0, so `computed = crc32(tag 8 payload) ^ crc32(tag 13 payload)`.

`mapfile::path::write_bloated` computes the flag this way and reproduces both samples' chunks byte for byte from their parts.

#### Tag 8: planes (verified; `mapfile::navmesh` parses and writes it, `pathgen::build` generates it bit-exact)

The two samples have 14 planes (Jaga Moraine, 5,366 trapezoids) and 24 planes (Eye of the North, 3,361 trapezoids). Plane 0 is the ground; the rest come from props (bridges and the like) and connect through portals.

```
u32 plane_count                          // equals the tag 12 count
per plane, the sections in this order:
  tag 0,  len 32: u32 start_count, vector_count, trapezoid_count, x_node_count,
                  y_node_count, sink_count, portal_count, portal_trapezoid_count
  tag 11: the length field is 16 * start_count, but only start_count * vec2f follows
                                         // the plane's start points
  tag 1:  vector_count * vec2f           // vertices, referenced by X-nodes and Y-nodes
  tag 2:  trapezoid_count * 44 bytes:
            u32 neighbors[4]             // TL, TR, BL, BR; 0xFFFFFFFF = none
            u16 portal_left, portal_right // 0xFFFF = none
            f32 y_top, y_bottom, x_top_left, x_top_right, x_bottom_left, x_bottom_right
  tag 3:  u8 root_type                   // 0 X, 1 Y, 2 sink; the root is node 0 of that type
  tag 4:  x_node_count * { u32 v0, u32 v1, u32 left, u32 right }    // segment test
  tag 5:  y_node_count * { u32 v, u32 above, u32 below }            // horizontal split
  tag 6:  sink_count * u32 trapezoid
  tag 10: portal_trapezoid_count * u32 trapezoid
  tag 9:  portal_count * 9 bytes: u16 count, u16 start (into tag 10),
            u16 neighbor_plane, u16 pair (portal id; the matching portal in the
            neighbour plane has neighbor_plane = this plane and the same id),
            u8 flags (0x4 = not used for pathfinding)
```

Node references are `u32`. The top 2 bits give the node type (0 = X-node, 1 = Y-node, 2 = sink) and the low 30 bits the index; `0xFFFFFFFF` = none. In-memory these become the `gw::pathing` structs; GWBS `references/pathingmaps_in_client_memory.h` has the same field order.

### 0x02 Terrain, stage 1 (all tags verified; `TrnDataBloat.cpp`)
`mapfile::terrain`.

```
u32 0x87821134
u8  0x11                                 // version
u8  0                                    // tag 0: header, 9 bytes
u32 packed:
      bits 0-5   * 3072.0 -> f32         // unverified meaning
      bits 6-7   == 0b10
      bits 8-15  == 96                   // XY_DIST, world units per height sample
      bits 16-23 dim_y / 32 - 1
      bits 24-31 dim_x / 32 - 1
u8  angle                                // radians = angle * 282.74335 / 45720
u16 unknown
u8, u8                                   // stored in stage 2 as n * 2 / 255
u8  1                                    // tag 1: heights, TrnCodecHeight bit stream
...                                      // tags 2, 4, 5, 3, 7, 0xFF
```

Verified on 290943: dims 416 × 640, and 416·96 × 640·96 = 39936 × 61440, which equals the Map Parameters bounds. The client asserts this equality.

Stage-2 tag 0 (26 bytes): `u32 dim_x, u32 dim_y, f32 unknown_scale, f32 angle_radians, u16 unknown, f32 byte0*2/255, f32 byte1*2/255`.

#### Tag 1: heights (verified, bit-exact against stage 2 for both samples)
The client's `TrnCodecHeight.cpp`, `TrnHuffman.cpp` and `TrnBitStore.h`. It is a bit stream read **MSB-first, bytes in order** (`mapfile::bits::BitReader`). Reading past the end yields zeros.

```
for each 32x32 chunk (chunk rows top to bottom, chunks left to right):
    i16 dc_base      = read(16);   dc_bits     = read(4) + 1
    i16 escape_base  = read(16);   escape_bits = read(4) + 1
    align to byte
    huffman table (alphabet 0x400, max code length 18):
        count_bits = read(8) + 1
        counts[1..=18] = read(count_bits) each
        symbols[sum(counts)] = read(10) each      // 10 = bits for alphabet 0x400
    for block_y in 0..8, block_x in 0..8:       // 4x4 sample blocks
        c[0] = dc_base + read(dc_bits)
        c[1..16]: s = huffman symbol
                  c = (s == 0x3FF) ? escape_base + read(escape_bits) : s - 0x200
        inverse transform (i32, wrapping):
            columns j=0..3, (a,b,c,d) = c[j], c[4+j], c[8+j], c[12+j]:
                t[j]=(a-b)-c  t[4+j]=c+(a-b)  t[8+j]=(a+b)-d  t[12+j]=d+(a+b)
            rows r=0..3,   (a,b,c,d) = t[4r..4r+4]:
                out[r][0]=(a-b)-c  out[r][1]=c+(a-b)  out[r][2]=(a+b)-d  out[r][3]=d+(a+b)
        heights[block origin + r*32 + col] = (f32) out[r][col]
    align to byte
align to byte
```

The Huffman code is canonical. Symbols are listed in code order, codes count up within a length, and the code shifts left by one between lengths. The client decodes codes up to 8 bits with a lookup table; longer codes are read one bit at a time and match when `code <= last_code[len]`.

Stage-2 tag 1 is `dim_x * dim_y` f32 in the same **chunk-major** order: chunk index `(y/32) * (dim_x/32) + x/32`, then `(y%32) * 32 + x%32` within the chunk. The client briefly converts to row-major and back, which is a no-op. `TerrainMap_ExportHeights`, which feeds pathing, hands out the heights row-major.

#### Tags after the heights (verified byte-exact against stage 2 for both samples; `mapfile::terrain::TerrainSurface`)

After the heights, stage 1 has tags 2, 4, 5, 3, 7, then `0xFF`. Stage 2 has the same tags plus a generated tag 9 between 3 and 7. Everything except tags 4 and 5 is copied unchanged.

| tag | stage 1 | stage 2 |
|---|---|---|
| 2 | `dim_x * dim_y` bytes: terrain texture index per sample, chunk-major like the heights | the same |
| 4 | packed byte array (see below) (`0x7599c0`) | `u8 n, n × u8`. The client masks the values with `0x7F` (`TerrainBuild_SetIndexArray`). Has as many entries as the terrain file-ref list (58 on Jaga). It looks like texture index → file-ref entry, with duplicates. |
| 5 | packed byte array (`0x759790`) | `u8 n, n × u8` (`TerrainBuild_SetDataArray`); flag-like values (`0x01`, `0x0d`, `0x2d`, …) |
| 3 | water mask, `dim_x * dim_y / 4` bytes (2 bits per sample) | the same |
| 9 | — | `dim_x * dim_y` bytes of generated lighting (`Terrain_bloat_write_normals`) |
| 7 | chunk grid: per 32×32 chunk, `u32 n, n bytes, 128 bytes shadow` (`TerrainChunkGrid_Deserialize`) | the same bytes, written back by `TerrainChunkGrid_Serialize` |

**Packed byte array** (`TrnBitStore`, MSB-first):
- `count` in 8 bits; if it is 0, the tag is one byte long and empty
- `width` in 3 bits (1–7; 0 is an assert in the client)
- `count` values of `width` bits each, then padding to a byte

Both samples use a width of 6.

Rendering (GWMB `Terrain.cpp`) uses tag 2 for texture blending. Tag 9 isn't needed; normals can be computed from the heights.

### 0x04 Props (collision sections verified against path tag 12; the rest from Ghidra)
The client's `PrDataBloat.cpp` / `PrCollision.cpp`; parsed by `mapfile::props`. The header is `u32 0x39583392, u8 0x11` in both stages (stage 1 may also use `0x12`).

| stage-2 tag | contents |
|---|---|
| 0 | `u16 count`, then per prop a 48-byte record (model index, position, rotation vectors, scale, …), followed by `n × vec2f` points when the prop has any. Stage 1 stores 20 bytes per prop: `u16 model, f32 x, y, z, u8 rot[3], u8 scale, u8, u8 n`, then `n × (i16, i16)` offsets. |
| 1 | `u32 n`, then `n × 24 bytes`: `f32 x, y, z, u32 plane, u32 flags, u16 portal, u16 portal_plane`. These are world-space collision outline points: ground-plane (plane 0) points first, then prop-plane points. Flags: `1` = plane start point, `2` = last point of a polygon, `4` = the edge to the next point is a portal, `8`/`0x10` = portal side flags. |
| 2 | `u32 n`, then `n × u16`: the owning prop of each path plane. Identical to path tag 12 on both samples. |
| 3 | `u32 n`, then `n × vec2f`: portal edges as point pairs. |
| 4 | Links, copied from stage 1: `u16 n`, then `n × 4 bytes`. |
| 6 | Optional: `u8 0, u16 n`, then `n × 4 bytes`. |
| 0xFF | End. |

For both samples: Jaga has 7,405 collision points, 14 planes and 168 portal points; EotN has 3,360, 24 and 272.

**Collision generation** (`PropCollision_Build @73a960`; ported as `pathgen::props`). Model files are fetched by base id through the asset manifest (`cargo run --bin gw-nav-cli -- fetch-models <map id>` caches them in `testdata/models`). Model index `i` of a prop is entry `i` of the map's `0x11000004` file-ref list.

Details:
- **Stage-1 prop record** (20 bytes): `u16 model, f32 x, y, z, u8 rot[3] (1/256 turns), u8 scale, u8 flags (bit 0: no model collision), u8 n`, then `n × (i16 dx, i16 dy)`. The `n` points form an extra ground polygon at `position + (dx, dy)` (z 0, and the last point is flagged `2`), added before the model's points.
- **Scale:** `byte · 255/32768 + 1/128`.
- **Rotation:** angles are `f32(byte · (2π_f32/256))`. The transform uses `v1.xy = (sin a2·cos a1 − cos a2·sin a0·sin a1, cos a2·cos a0)`.
- **World point:**
  - `x = round((v1.y·px + v1.x·py)·scale + pos.x)`
  - `y = round((v1.y·py − v1.x·px)·scale + pos.y)`
  - `z = pos.z + pz·scale`

  `round` is the client's `trunc(f32(v ± 0.5))`. A point is skipped if its x, y, z and plane equal the previous point's, unless the previous point ended a polygon.
- **Simplification:** from an anchor with flags `& 3 == 0`, drop following points of the same plane, flags and portal while `|d1 − d2| < 2`. Here:
  - `n = (a.x − m.x, m.y − a.y) · rsqrt(len²)`, with `m` the point after the candidate
  - `d1 = c.x·n.y + c.y·n.x`, with `c` the candidate
  - `d2 = a.x·n.y + a.y·n.x`
- **Planes and portals:** the model's plane ids are offset by the running plane count, and its portal ids by the running portal count. The plane→prop table (tag 2) is `[0]` followed by the owning prop of each new plane.

The original notes follow:
1. For each prop, the model file named by the prop's model index is looked up by file id. Its collision stream (stream `0xb`, an FFNA type-2 file) holds:
   - chunk `0xfa4`: `u32 2, u32 n`, then `n × 16 bytes` of `u8 plane, u8 flags, u8 portal, u8 portal_plane, f32 x, y, z`
   - chunk `0xfa7`: `u32 1, u32 n`, then `n × 16 bytes`; only one float is used, scaled into the prop record
   - chunk `0xfac`: `u32 1, a, b`
2. If that stream is missing, the client generates it from the downloaded model (`MdlDecomp_ProcessModelFile`). **`0xfa4` is a straight copy of the model's chunk `0xBBA`**; `0xfa7` comes from the `0xBB9` animation conversion.
3. The points are simplified (`PropCollision_SimplifyVertices`: collinear points within 2 units of the line are dropped, using the fast rsqrt).
4. They're transformed by the prop's rotation (cos/sin), scale and position. x/y are rounded to integers, and consecutive duplicates are removed.
5. Plane ids are allocated globally: each prop's local plane ids are offset by the running total.
6. Portal points are exported as pairs: each point flagged `4`, plus the following point.

### 0x09 Environment (sections 0–7 verified on both samples; copied unchanged into stage 2; `mapfile::environment`)

The layout is from GuildWarsMapBrowser (`EnvironmentInfoChunk`):

```
u32 0x92991030, u16 0x10, u16 sky_variant
8 sections, each: u8 type, u16 count, count × fixed-size records:
  0: 10 B    1: 6 B    2: 19 B (fog)    3: 8 B (lighting: ambient BGR + intensity, sun BGR + intensity)
  4: 2 B     5: 15 B (16 B if sky_variant > 0; sky textures)
  6: 57 B (water: u8 mode, u8 flags, 3 B, f32 surface z, 36 B, BGRA absorption, BGRA pattern,
           u16 colour texture, u16 distortion texture; indices into 0x11000009)
  7: 4 B (wind)
then a tail of unknown layout
```

GWMB reads a "section 8" with per-setting record indices next, but the bytes after section 7 don't fit it in either sample, so which record a map uses isn't known yet. The water surface is at z = 0 in both samples.

### 0x07 Mission (layout verified on 22 maps; meaning from in-game observation; `mapfile::mission`)

Copied unchanged into stage 2. Named points, mostly spawn points:

```text
u32 0x40010020, u8 10, 6 × f32 (not decoded; looks like a position and a nearby point)
2 × { u16 n, n × { i32 x, i32 y, u8 facing, u32 tag } }
... (not decoded)
```

- `tag` is a C multi-character constant (`'vale'`), so its bytes are reversed in the file. 4-digit tags are map ids; zero means no name.
- Arriving by map travel puts the character 75 units from one of the points tagged with the map's own id, facing `facing / 256` of a turn counter-clockwise from +x. The offset direction looks random. Seen in maps 179, 642 and 648. Each map has 3–4 of these points.
- Other tags look like arrivals from particular neighbouring maps, placed near the portal back to them (Shing Jea Monastery `vale` for Sunqua Vale, Tsumei Village `peni` for Panjiang Peninsula). They sit roughly 200–1300 units from where gwbs recorded the exit. Some exits have no point near them, so this is only a rough guide to portal locations.
- No zone-transition trigger geometry has been found in the map file; the server decides transitions. The visualizer shows the recorded exits from gwbs's `zones.db` next to these points.
- **The visible portal is a prop.** Its model is 43045 in Prophecies and Factions maps and 247212 in Nightfall and Eye of the North maps (`mapfile::props::PORTAL_MODELS`; checked in game in Ascalon City and Boreal Station).
  - One of these props sits 13–380 units from 24 of the 40 recorded exits in 22 map files, and 71 of 101 placements have a mission point within 1000 units.
  - Nothing else marks them: prop flags, scale and model chunks differ between the two, their shared texture (9350) and collision chunk are common to many models, and the props links section doesn't reference them.
  - A few copies are decoration.
  - In Boreal Station a gadget agent (id 9200) stands on the portal, but Ascalon City's portal has none.

## 7b. Obstacles (path tag 13; the grid is ported and verified in `pathgen::obstacles`, the placements are not ported yet)
`PathChunk_WriteObstacles` gets the zone object placements from `Zones_GetPlacements(zones, level 2, …)`. These are procedurally placed zone objects, 48 bytes each, with x/y at +4 and a radius at +0x28; only objects with a nonzero radius count. They're bucketed into a grid of 1024-unit cells over the map bounds (`PathObstacle_BuildFromAgents`). `PathObstacle_Export` writes:
- `u16 grid_w, u16 grid_h, u16 obstacle_count`
  - `grid_w = ceil((max_x − min_x)/1024)` and `grid_h = ceil((max_y − min_y)/1024)`, from the map parameters. The client's `Math_SqrtToInt @46e050` is really `ceil`, and `ClampFloat @46e0a0` is `floor`.
  - `obstacle_count` is the sum of the cell counts, so an obstacle is counted once per cell.
- per cell, rows from the top (`max_y`) down: `u8 count, u16 first`
- per cell reference: `f32 x, y, radius`

`PathObstacle_Add @7230a0` puts an obstacle into every cell overlapped by its box grown by `radius + 100`, clamped to the bounds:
- rows `floor((max_y − box.max_y)/1024) .. ceil((max_y − box.min_y)/1024)`
- columns `floor((box.min_x − min_x)/1024) .. ceil((box.max_x − min_x)/1024)`

Boxes with zero area are skipped. Re-bucketing the client's obstacles reproduces the client's cell membership on both samples: Jaga has 39×60 cells and 1,759 obstacles (2,737 references), EotN 30×66 cells and 287 obstacles. The order within a cell follows the placement order. Some placements are exact duplicates.

The placements come from the zone foliage generator (`Zone_PopulateCell @773110`, `ZnZonePop.cpp`), which:
- works per cell, per zone and per mip level (`ZoneLevel_Update`)
- seeds `Random_Init` from the cell coordinates, level and zone id
- uses noise patterns, zone polygon tests (`Map_zone_bsp_point_in_radius`), collision with props at levels ≤ 3, `Zone_ValidatePlacement`, and the model radius of the zone models (`ZoneModel_GetOrCreate`)

Its input is the bloated zones chunk (`ZoneData_Export @772460`), which keeps tags 1 and 2, rebuilds the zones (tag 3) and adds a generated tag 4.

This needs the zone bloat plus the placement generator ported. The trapezoids, planes and portals don't depend on it.

### 7b.1 Zone placement research (from Ghidra; not ported)
The findings are recorded here so the port can start from them.

**Zones chunk, stage 1** (`0x10000003`): `u32 0x59220320, u8 10`, then tagged sections (bare `u8` tag, `u32` length).
- **Tag 1, zone defs:** `u32 count`, then per def:
  - `u32 id`, then a NUL-terminated UTF-16 `.ini` path, which stage 2 drops
  - `u32 layer_count (L)`, `u32 model_count (M)`
  - per-layer arrays, each of length L, in this order:
    - `u32 type` (0 or 2)
    - `f32 spacing`
    - `f32 collision_radius`
    - `f32 density`
    - `f32 scale_variance` (≤ 0.5)
    - `u8 pattern` (0–4)
    - `u32 models_in_layer`
  - per-model arrays, each of length M: `f32 cumulative_probability`, then `u32 flags`

  `ZoneDef_Create @76f7f0` sets the layer's mip level to the smallest `l` in 0..4 with `spacing < 2^l · 96 · 0.1`, and 4 if there is none. Only levels ≥ 2 are generated for path obstacles.
- **Models** aren't named in the def. They are the zones file-reference list (`0x11000003`), consumed in order across the defs. For EotN that is 27 + 27 + 6 = 60 references.
- **Tag 2, prop files** (`ZoneData_ReadPropFiles`): `u8 n`, `n × u32` model indices, then nibble-coded texture atlas sizes. Rendering only.
- **Tag 3, zones:** `u32 count`, then per zone:
  - `u32 def id`, `u8 flags`
  - `u16 height`, where `height = h · 0.76293945 − 25000`
  - `u32 vertex_count`, then `vertex_count × (f32 x, y)`

  Stage 2 rebuilds them as `u32 id, u8, u32, u32 n, n × vec2, u32, 16 bytes` (`ZoneData_WriteZoneBlock`).
- **Stage-2 tag 4** is the per-zone coverage map (`Zone_BuildCoverageMap @770920`). It holds 2 bits per 192-unit cell: 0 outside, 1 partial, 3 full. Each cell is classified with ray traces through the zone's polygon BSP (`Map_zone_bsp_*`). Because it's in the stage-2 file, it can verify a port of the zone BSP.

**Model radius.** The obstacle radius is `scale · r`, where `r` is the first float of the model's type-1 bounding cylinder.
- The cylinders are copied verbatim from the model's `0xBB9` chunk: header `0x2C` bytes, count at `+0x14`, then `count × {u32 type, f32 radius, f32 height, f32 z}`. They become collision stream `0xFA7` (`MdlDecomp_ConvertGeometryChunk_0xBB9_to_0xFA1`, `ZoneProp_Create @76eff0`).
- Example: EotN's tree 288194 has a type-1 cylinder of radius 29.89.
- `scale = round(250 · (1 + variance · (u − 1))) · 0.004`, where `u = 2·rand`, or `rand` for model flag `0x800`.

**Placement order** (`ZoneLevel_Update @76d510`, called with minimum level 2):
1. The level-2 cell range is walked in tiles, y outer and x inner. The tile size comes from the tables at `0xa79de4`, `0xa79df8`, `0xa79e0c` and a quality value (`DAT_00bfaa30`).
2. For each tile, the parent rectangles at levels 3 and 4 are grown by one cell. Levels 4, 3 and 2 are then populated in that order (`ZoneMap_ProcessCells` → `Zone_PopulateLevel`), and all levels are cleared again.
3. Coarser cells are therefore populated once per tile that needs them. A per-cell hash (`AUCPlacementChunk`) records a cell's placements only the first time.

**Per cell** (`Zone_PopulateLevel @773940`, `Zone_PopulateCell @773110`):
- The candidate zones come from a spatial partition (1536-unit cells), deduplicated with a visit stamp. Each candidate is kept if its coverage map touches the cell (`Zone_TestCoverageRect`), and only its layers at this level are used.
- For each (zone, layer): `Random_Init(((cx << 4 ^ cy) << 8 ^ level) << 12 ^ zone_id)`. Then for each of the 5 slots (offset table `0xa78358`):
  - Skip the slot if it's already used.
  - Get a pattern value: random, or the 16×16 noise table at `0xa78380` (`Zone_SampleNoiseTable`), or a constant.
  - Place the point if `rand ≤ density · value`. The position is the slot offset plus `r + rand · (0.4 · cell − 2r)` on each axis, with `r = spacing`.
  - Tests:
    - the point is inside the zone with radius `r` (`Map_zone_bsp_point_in_radius`, skipped when coverage is full)
    - at levels ≤ 3, no collision with coarser placements (`Zone_CheckPropCollision`, using the layer's collision radius)
    - `Zone_ValidatePlacement`: terrain tile flags (`TerrainMap_GetTileArray`, a tile-id map built during terrain bloat), terrain heights against the zone height, and the slope via surface normals for model flags `8`/`0x10`
  - Choose the model by cumulative probability, then `ZoneProp_Populate`. That draws a random angle, plus the scale for layers above level 0, and records `{model, pos, orientation, radius, scale}`.

Porting this means porting:
- the zones bloat (the polygon BSP, the coverage map, and the partition)
- the tile-id map from terrain bloat
- terrain normals
- the noise table
- the tiling order above

Only the final obstacle grid can be checked against the client, apart from tag 4.

## 7a. Prop segments and plane assembly (ported, bit-exact: `pathgen::assemble`, `pathgen::clip`, `pathgen::build`)
- **Start points** (`PathChunk_BuildPathData`): the tag-7 start points are snapped with `round()` then `/4` truncated, then `×4`, and stored as `(f64 x, f64 y, u16 plane 0)`.
- **Prop segments** (`PathData_process_prop_segments @72fe80`). For each props tag-1 point in order:
  - flag `1`: it becomes a start point of its plane.
  - otherwise, unless flag `2`: it forms a segment to the next point (`PathData_add_segment`). Degenerate segments are dropped.

  The 56-byte segment record is:
  - `f64 p0[2]`, the upper end (larger y; on a tie, larger x)
  - `f64 p1[2]`
  - `f64 dx = p1.x - p0.x`
  - `f64 dy = f32(p1.y - p0.y)`
  - `u16 plane`
  - `u16 portal`: the index within the plane if flag `4`, else `0xFFFF`
  - `u32`

  Portal edges also get a 16-byte portal record: `u16 plane, u16 portal_plane, u16 portal, u8 side_flags, …`. Terrain segments from the tracer have plane 0 and portal `0xFFFF`.
- **Segment order:** the shuffled terrain segments first, then the prop segments in tag-1 order. That puts plane-0 prop outlines first, then each prop plane in increasing order.
- **Plane assembly** (`PathData_build_maps @72fa00`): for each plane in turn, take the contiguous run of segments, portals and start points with that plane id and call `PathMap_Build`. The output is prefixed with `u32 plane_count`.
- **`PathMap_Build @7318a0` / `PathMapBuilder_build`**:
  1. `PathMapBuilder_process_segments`: for each segment in order, clip it against a BSP of the segments inserted so far (`PathBsp_ClipSegment`), deduplicate endpoints through a vertex hash, then insert each sub-segment into the trapezoidal map. That insertion uses DAG point location with 0.01 tolerances, `SplitTrapezoidVertical`, `CreateTrapezoids`, `SplitTrapezoidsHorizontal` and `ConvertTrapezoidsToLeaves`/merge.
     - The BSP clip (`PathBsp_ClipSegment @736490`) uses **x87 double** arithmetic. It returns the split parameters of the new segment where it crosses or touches earlier segments, each with a flag, plus the collinear overlap ranges. The parameters are sorted with a hand-written quicksort. A segment fully covered by existing collinear segments produces nothing.
     - **Vertices are quantized** (`PathMapNode_init_with_hash @730730`): `v = trunc(x · 100) · 0.01` per coordinate, stored as a double. The hash is a byte hash over the two doubles, seeded `0x325D1EAE`, using `s_cmpHashTable`. Checked: the first intersection vertex in Jaga's plane 0 is `(8975.82, 10224.17)` = `trunc` of the exact intersection `(8975.8216, 10224.1784)`. Rounding would give `.18`. The output vertex list is the hash's insertion order.
  2. `RemoveDegenerateTrapezoids`, repeated until nothing changes.
  3. `PathTree_MarkVisitedBFS` from the start points.
  4. `PathBuild_ProcessNodeStack`, then link the portals.
  5. Serialize with the writer table at `0xa727e4` (10 functions: header, vertices, edges, nodes, portals, …).

  Section 7c describes each step. `pathgen::build` reproduces every plane of both samples exactly: 38 planes, 8,727 trapezoids, about 52,000 nodes and 448 portals.

## 7c. The trapezoidal map builder (ported, bit-exact: `pathgen::build`)
All arithmetic is plain f64. `approx(a, b)` means `|a − b| ≤ 0.01`. Comparisons are written the way the client makes them; negating one changes the result for the ±∞ bounds of the outer trapezoids.

**Records.**
- A trapezoid has:
  - `above[2]` and `below[2]` neighbour links
  - left and right sub-segments (none = unbounded)
  - two portal ids
  - `top_x[2]` and a top point; `bot_x[2]` and a bottom point
  - its sink node
- The trapezoid list is in creation order, minus freed ones. That is the export order.
- The initial trapezoid has top `(+∞, +∞)`, bottom `(−∞, −∞)` and x bounds `(−∞, +∞)`. Its sink is the root.
- Node kinds:
  - **X:** a copy of the clipped segment, with left and right children
  - **Y:** a point, with above and below children
  - **sink:** a trapezoid

**Point location** (`PathTree_QueryByCoords`):
- X node: go right if `0 < (q.y − p0.y)·v.x − (q.x − p0.x)·v.y`, else left.
- Y node: go below if `q.y < p.y`, or if `q.y == p.y` and `p.x > q.x`; else above.

**Helpers.**
- `Interp(s, y)` (`PathBuild_InterpolateSegmentX`):
  - `p0.x` if `y == p0.y`
  - `p1.x` if `y == p1.y`
  - else `((y − p0.y) / v.y)·v.x + p0.x`
- `InitEdges(t, left, right, top, bottom)`:
  - A missing side is ±∞.
  - A side with `|p1.y − p0.y| > 0.01` interpolates at `top.y` and `bottom.y`.
  - A flat side uses `top.x` and `bottom.x`.

**Predicates** (`ty` and `by` are the trapezoid's top and bottom y):
- `crosses_top(s, t)` (`SegmentIntersectsTrapezoid`) is false in each of these cases:
  - the segment is flat
  - `|p1.y − ty| > 0.01` and `p0.y < ty`
  - `approx(p1.y, ty)` and `top.x ≤ p1.x`
  - `|p0.y − ty| > 0.01` and `ty < p1.y`
  - `approx(p0.y, ty)` and `!(top.x < p1.x)`

  Otherwise let `x = Interp(s, ty)`:
  - false if `top_x[1] < x` and not `approx(x, top_x[1])`
  - if `!(top_x[0] ≤ x)`, the result is `approx(x, top_x[0])`
  - otherwise true
- `crosses_bottom(s, t)` (`CheckSegmentIntersection`) requires all of these:
  - a non-flat segment
  - `approx(p0.y, by) ? bottom.x < p0.x : by ≤ p0.y`
  - `approx(p1.y, by) ? !(bottom.x ≤ p1.x) : !(by < p1.y)`

  Then the same x test as `crosses_top`, at `by` against `bot_x`.
- `contains(t, p)` requires all of these:
  - `p.y < top.y`
  - `|p.y − by| > 0.01 ? by ≤ p.y : bottom.x ≤ p.x`
  - left side missing, or `0 ≤ (p.y − L.p0.y)·L.v.x − (p.x − L.p0.x)·L.v.y`
  - right side missing, or the same expression `≤ 0`

**Neighbour lists** are normalized after every replacement:
- If `[0]` is none and `[1]` is set, shift `[1]` down.
- If `[0] == [1]`, clear `[1]`.

**Inserting a segment** (`PathMapBuilder_InitSegments`):
1. Clip it (7a). Split points are `t·v + p0` of the *original* segment. Register the new ones in the vertex hash.
2. Each consecutive pair of points becomes a sub-segment: `v = p1 − p0`, with the segment's portal. Each one is inserted as follows.
3. **Start trapezoid.** Query at `p0 + 0.1·v`, then walk up.
   - With one above link: take it if `crosses_top`, `crosses_bottom` or `contains(sub.p0)` holds.
   - With two: try `crosses_bottom(a0)`, `crosses_bottom(a1)`, `crosses_top(a0)`, `crosses_top(a1)`, `contains(a0)`, `contains(a1)`, in that order.
   - Stop, keeping the current trapezoid, when no candidate is found or the candidate satisfies `|bottom.y − p0.y| > 0.01 ? p0.y < bottom.y : p0.x ≤ bottom.x`.
4. If `p0 ≠ top` (exact comparison), split the trapezoid there (`SplitTrapezoidVertical`, below).
5. **End trapezoid** (`FindSegmentTrapezoid`). Query at `p1 − 0.1·v`, then walk down the same way.
   - One below link: `crosses_top`, `crosses_bottom`, `contains(sub.p0)`.
   - Two: `crosses_top(b0)`, `crosses_top(b1)`, `crosses_bottom(b0)`, `crosses_bottom(b1)`, `contains(b0)`, `contains(b1)`.
   - Stop when `|top.y − p1.y| > 0.01 ? top.y < p1.y : top.x ≤ p1.x`.

   If `p1 ≠ bottom`, split there and continue from the upper piece.
6. **`CreateTrapezoids`.** From the end trapezoid, walk up and allocate a left and then a right trapezoid for each one crossed.
   - Stop when `approx(sub.p0.y, top.y)` and `sub.p0.x ≤ top.x`.
   - Next is `above[0]` if `crosses_bottom(clipped segment, above[0])`, else `above[1]`.
7. **`SplitTrapezoidsHorizontal`**, bottom to top:
   - `L = InitEdges(orig.left, sub, …)` and `R = InitEdges(sub, orig.right, …)`, with the original top and bottom. Create L's sink, then R's.
   - Above links:
     - top entry: `DistributeBelowLinks`
     - `orig.above[0]` is the next original: `L.above = [next.L]`, `R.above = [next.R, orig.above[1]]`, and `orig.above[1].below[0] = R`
     - otherwise: `L.above = [orig.above[0], next.L]`, `R.above = [next.R]`, and `L.above[0].below[0] = L`
   - Below links are the mirror image; the bottom entry uses `DistributeAboveLinks`.
   - The distribute functions treat L or R as degenerate when its top (or bottom) x bounds are within 0.01. Degenerate pieces get no links on that side. The single neighbour's own links are patched for the cases where it pointed at `orig` in slot 0 or slot 1.
8. **`ConvertTrapezoidsToLeaves`**, bottom to top:
   - Each original's sink becomes an X node with the clipped segment, whose children are the current L's and R's sinks. The original is freed.
   - If the next L (or R) has the same left and right sub-segments as the current one, it merges into it (`MergeTrapezoids`). The lower trapezoid takes over the upper one's above links, `top_x` and top. The above neighbours' below links are redirected. The upper one is freed; its sink stays referenced by the node that pointed to it.

**`SplitTrapezoidVertical(T, P)`:**
- Create U (top to P), then L (P to bottom), both with T's sides.
- `U.above = T.above`, `U.below = [L]`; `L.above = [U]`, `L.below = T.below`.
- Redirect the neighbours' links from T to U or L.
- Create U's sink, then L's. T's sink becomes a Y node on P with children (U, L). T is freed.

**Removing flat trapezoids** (`RemoveDegenerateTrapezoids`), repeated until a pass removes nothing. A trapezoid is degenerate when all of these hold:
- `top.y == bottom.y`
- `below[1]` and `above[1]` are none
- neither side is a portal edge

A DFS from the root (flag 4 marks visited nodes) handles each parent:
- A degenerate child of an X node is recorded and its link cleared.
- A degenerate above child of a Y node is replaced by the first non-degenerate trapezoid down its `below[0]` chain; a below child by the same up its `above[0]` chain.
- Afterwards each recorded trapezoid (except the root's) is spliced out: its `above[0]`'s below link becomes its `below[0]` and vice versa. Then it is freed.

**Pruning:**
- `PathTree_MarkVisitedBFS` flags (1) every trapezoid reachable through neighbour links from a start point's trapezoid.
- `PathTree_ValidateRecursive` flags (2) dead nodes:
  - a sink without flag 1
  - an X or Y node whose children are both dead; a missing child counts as dead
- `PathBuild_ProcessNodeStack` deletes dead nodes (flag 0x10), clears links to them and frees their trapezoids.

**Portals** (`PathBuild_LinkTrapezoidsToPortals`):
1. For planes other than 0 only, `PathBuild_InsertEdgeEndpoints` runs on each live trapezoid with a portal side. It looks up the neighbour plane's record of that portal (keyed `portal | plane << 16`). The trapezoid is split at the record's top and bottom points so the portal edge matches the neighbour's extent, and the vertices are added. The side outside the extent is cleared.
2. Every live trapezoid records its sides' portal ids. It is pushed onto that portal's list, head first: a left-side portal lists the trapezoid on its right side, and vice versa.
3. A portal with trapezoids on both sides and `neighbor_plane == plane` is split into two, with the copy appended. For one-sided flags (1 or 2), the flag of the side it doesn't face becomes 4.
4. `PathBuild_SortAndCreatePortals`, per portal:
   - Quicksort its list descending by top `(y, x)`. The client's quicksort uses the middle element as pivot and an explicit stack.
   - Keep the first contiguous run: each next `top.y` must be `approx` the previous `bottom.y`.
   - Store the count and start in the plane's portal-trapezoid array.
   - For a portal to another plane, record the run's extent (top point, bottom point, and the x on the portal's side) for that plane's `InsertEdgeEndpoints`.

**Serialization:**
- Trapezoids are written in list order, their position being their index. Floats are stored as f32 and clamped: `x_top_right ≥ x_top_left`, `x_bottom_right ≥ x_bottom_left`, `y_top ≥ y_bottom`. Flat trapezoids that survive (for example next to portal edges) are exported too.
- `PathChunk_WriteTreeStructure` numbers the nodes in LIFO stack order from the root:
  - X nodes count up from 0, Y nodes from `0x40000000`, sinks from `0x80000000`.
  - Left (or above) is pushed before right (or below), so right and below are numbered first.
  - X nodes reference the vertex indices of their segment's quantized endpoints; Y nodes that of their point.

## 7. How the client generates the path planes (ported, bit-exact: `pathgen`)

`PathChunk_BuildPathData @72ffe0` works in these steps:

1. **Heights.** `TerrainMap_ExportHeights` gives the row-major height grid.
2. **Collision polygons** come from the Collision chunk (empty in both samples).
3. **Prop collision and portal points** come from the bloated Props.
4. **Tracer** (`PathTracer_Init @72cdf0`, `Engine\Map\Path\PathFlood.cpp`):
   - It builds a grid of `(dim_x + 2) × (dim_y + 2)` cells, 16 bytes each, 96 units apart. The origin is `(min_x - 96, max_y + 96)`.
   - Slope limits depend on the map type (the first `PathTracer_Init` argument):

     | map type | limits (radians) |
     |---|---|
     | `< 2` | 0.2618 / 0.6109 / 0.5236 (15° / 35° / 30°) |
     | otherwise | 0.1745 / 0.7854 / 0.6981 (10° / 45° / 40°) |

   - `Terrain_generate_grid_vertices` marks cells by slope. `PathFlood_MarkPolygon` burns in the collision polygons, and `PathFlood_mark_blocked_cells` runs at each start point.
   - For each **start point (tag 7)** it runs `PathFlood_world_to_grid`, `PathFlood_trace_edges` (a flood fill from the point), `PathFlood_trace_contours` (which traces and simplifies the flooded region's outline) and `PathFlood_add_segments`. **The start points seed the walkable area.**
   - The output segments (56 bytes each) are shuffled with Fisher–Yates using `Random_Init(0)` / `Random_NextUInt`. This is the randomized order for incremental trapezoidal-map construction, so the RNG must be reproduced exactly.
   Tracer details from Ghidra (`Engine\Map\Path\PathFlood.cpp`):
   - **Grid cell:** 16 bytes, i.e. two triangle halves of `(f32 slope_angle, u32 flags)`. `Terrain_generate_grid_vertices @72d430` fills cells (1, 1) to (dim_x - 1, dim_y - 1) from height quads, where `v00 = (0, 0, h[y][x])`, `v01 = (96, 0, h[y][x+1])`, `v10 = (0, -96, h[y+1][x])` and `v11 = (96, -96, h[y+1][x+1])`. The halves are `(v00, v01, v10)` and `(v01, v11, v10)`. The last column and row are copies of their neighbours. `Terrain_init_grid_edges` sets the outer ring to `(π/2, flags 1)`.
   - **Triangle classification** (`Terrain_compute_grid_cell @72d740`, from the x87 disassembly). For a triangle (A, B, C):
     - edges `e1 = A - B` and `e2 = C - B`, each component rounded to f32
     - `N = (e1z·e2y − e1y·e2z, e1x·e2z − e2x·e1z, e2x·e1y − e1x·e2y)`, each rounded to f32
     - `L = f32((Nx² + Ny²) + Nz²)`
     - `r = fast_rsqrt(L)`, a table method: `bits(r) = table[(bits(L) >> 16) & 0xFF] - (bits(L) >> 24) · 0x800000 + 0x5E800000`, with the 256 × u32 table at `0x93d6c8`
     - `angle = acos(-f32(Nz · r))`

     The flags:
     - With the water flag set and `A.z ≥ 0`: 0 (walkable).
     - Otherwise, if all three z > 40 or `angle > limit[1]`: 1 (blocked).
     - Otherwise, if `angle > limit[0]`: 2.
     - Otherwise: 0.

     **The x87 unit runs at 53-bit (double) precision during bloat.** Verified: prop collision z values only match the client in that mode (`X87::Double`), even though GW uses D3D9, which usually switches the unit to 24-bit mode. The tracer's results are the same in either mode.
   - **Seeds.** `PathFlood_mark_blocked_cells` ORs `0x20` into a 5×5 block of cells around each start point and sets the start point's own cell to `0x30`. `PathFlood_MarkPolygon` does the same along collision polygon edges, using a grid line iterator.
   - **Flood** (`PathFlood_trace_edges @72dc40`): a FIFO flood from the seed triangle. A neighbour is entered if `flags & 5 == 0`, and, if `flags & 2`, only when `|angle - angle_neighbour| <= limit[2]`. Visited triangles get `|4`. Each triangle has three neighbours: the other half of its cell, and one across a cell edge that depends on the half's parity. The boundary edges between visited and unvisited triangles are then collected into a hash table that keeps insertion order (`TraceEdge`, 44 bytes).
   - **Contours** (`PathFlood_trace_contours @72ec70`): repeatedly takes the first remaining edge and walks the boundary using the direction tables at `0xbfa390`, `0xbfa408` and `0xbfa480`. Grid vertices become world `(min_x - 96 + gx·96, max_y + 96 - gy·96)`. Each contour then goes through `PathFlood_simplify_polygon @72e620`. The output is `(f32 x, f32 y, u32 flags)` vertices, and each polygon ends with a `(0, 0, 0x40)` marker.
   - **Ported, bit-exact:** `pathgen::tracer`. The traced outlines, the segment list and the seeded shuffle reproduce the client exactly on both samples. Every traced vertex appears in the client's plane-0 vertex list, and in the same order once the vertices the trapezoid builder adds later (segment intersections, prop outlines) are filtered out. That list is in insertion order: endpoints of the shuffled segments, deduplicated.

     Two details were needed for exactness:
     - the prop portal points (currently taken from the client's props tag 3)
     - a float-faithful port of the `GridIterator` line walk: `init_line` normalizes by `sqrt`, and each step intersects the current cell's edges (row edge first) and accepts it if the edge parameter is in [0, 1] and the distance is in [0, remaining]

     The x87 precision mode made no difference.
     Further details from the port:
     - Seed cells are forced to flags `0x30`, i.e. walkable.
     - The contour walk uses the triangle-offset table set up in `PathFlood_trace_contours` and three 6×5 tables: next edge type, dx and dy.
     - The hash's full list appends at the tail.
     - Every start point re-collects *all* boundary edges of the grid, so outlines already traced from an earlier start point are traced again. This matches the client, which also produces the duplicates.
     - `simplify_polygon`, pass 1: from the anchor, try candidates `anchor+2, anchor+3, …`. A candidate is accepted if every skipped vertex:
       - is on the left or on the line (`cross ≥ 0`)
       - lies within 136 units of the segment, using the table-based fast reciprocal at `0x93d2c8`
       - keeps the running sum of distances ≤ 300
       - if flagged `0x20`, has no `0x10` cell on the line between anchor and candidate (grid line walk)

       It stops after 7 consecutive failures and takes the last accepted candidate. Collinear output vertices are merged.
     - `simplify_polygon`, pass 2: for each corner with `dot > 0` and `cos² > 0.75` (or `> 0.49999997` when an edge is ≤ 136 long), move the vertex by the projection of the shorter edge onto the longer one, then round with `trunc(f32(x ± 0.5))`.
   - **Order matters.** The final Fisher–Yates shuffle (Park–Miller "minimal standard", multiplier 48271 with Schrage's method, seed `0x75BD924`) permutes by position. So contour order, and therefore hash-list order, has to match the client exactly.
5. The start points are snapped to a 4-unit grid, then `PathData_process_prop_segments` adds the prop segments.
6. **`PathMap_Build`** builds the trapezoidal map and BSP per plane and serializes them into tag 8 (sections 7a and 7c).

`pathgen::chunk::bloat_path` runs the whole pipeline. Its inputs are the stage-1 terrain, params and path chunks, the props collision (`pathgen::props::build_collision`, which needs the prop models) and the tag-13 payload. With the client's props collision and obstacles, it reproduces the `0x20000008` chunk of both samples byte for byte (938,151 and 503,449 bytes).

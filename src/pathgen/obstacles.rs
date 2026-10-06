//! Path obstacles (path tag 13): `PathObstacle_BuildFromAgents`,
//! `PathObstacle_Add` and `PathObstacle_Export` from the client's
//! `Engine\Map\Path\PathObstacle.cpp`.
//!
//! Obstacles are round zone objects (trees and the like) placed by the zone
//! generator. They are bucketed into a grid of 1024-unit cells covering the
//! map bounds; an obstacle goes into every cell its circle, grown by 100
//! units, overlaps.

use crate::mapfile::tags::Writer;

const CELL: f32 = 1024.0;
const MARGIN: f32 = 100.0;

/// A round obstacle: position and radius.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Obstacle {
    pub x: f32,
    pub y: f32,
    pub radius: f32,
}

/// `Math_SqrtToInt` (misnamed): `ceil` to an integer.
fn ceil_int(v: f32) -> usize {
    v.ceil() as usize
}

/// `ClampFloat` (misnamed): `floor` to an integer.
fn floor_int(v: f32) -> usize {
    v.floor() as usize
}

/// Bucket the obstacles and write the tag 13 payload. `bounds` are the map
/// bounds `[min_x, min_y, max_x, max_y]`; obstacles with radius 0 are
/// skipped. Rows count down from `max_y`.
pub fn export(bounds: [f32; 4], obstacles: &[Obstacle]) -> Vec<u8> {
    let [x0, y0, x1, y1] = bounds;
    let w = ceil_int((x1 - x0) / CELL);
    let h = ceil_int((y1 - y0) / CELL);
    let mut cells: Vec<Vec<Obstacle>> = vec![Vec::new(); w * h];
    for o in obstacles.iter().filter(|o| o.radius != 0.0) {
        assert!(o.radius >= 0.0, "radius >= 0");
        let min_x = ((o.x - o.radius) - MARGIN).max(x0);
        let min_y = ((o.y - o.radius) - MARGIN).max(y0);
        let max_x = (o.radius + o.x + MARGIN).min(x1);
        let max_y = (o.y + o.radius + MARGIN).min(y1);
        assert!(min_x <= max_x && min_y <= max_y, "MathRectIsValid(rect)");
        if (max_x - min_x) * (max_y - min_y) == 0.0 {
            continue;
        }
        let (row0, row1) = (floor_int((y1 - max_y) / CELL), ceil_int((y1 - min_y) / CELL));
        let (col0, col1) = (floor_int((min_x - x0) / CELL), ceil_int((max_x - x0) / CELL));
        for row in row0..row1 {
            for col in col0..col1 {
                cells[row * w + col].push(*o);
            }
        }
    }

    let total: usize = cells.iter().map(Vec::len).sum();
    let mut out = Writer::new();
    out.u16(w as u16).u16(h as u16).u16(total as u16);
    let mut first = 0;
    for cell in &cells {
        out.u8(cell.len() as u8).u16(first as u16);
        first += cell.len();
    }
    for o in cells.iter().flatten() {
        out.f32(o.x).f32(o.y).f32(o.radius);
    }
    out.into_bytes()
}

/// Parse a tag 13 payload back into its grid: `(width, height, cells)`,
/// each cell listing its obstacles.
pub fn parse(data: &[u8]) -> crate::mapfile::Result<(usize, usize, Vec<Vec<Obstacle>>)> {
    let mut r = crate::mapfile::tags::Reader::new(data);
    let (w, h, n) = (r.u16()? as usize, r.u16()? as usize, r.u16()? as usize);
    let ranges = (0..w * h).map(|_| Ok((r.u8()? as usize, r.u16()? as usize))).collect::<crate::mapfile::Result<Vec<_>>>()?;
    let all = (0..n)
        .map(|_| Ok(Obstacle { x: r.f32()?, y: r.f32()?, radius: r.f32()? }))
        .collect::<crate::mapfile::Result<Vec<_>>>()?;
    let cells = ranges
        .into_iter()
        .map(|(count, first)| {
            all.get(first..first + count)
                .map(<[Obstacle]>::to_vec)
                .ok_or(crate::mapfile::MapFileError::Invalid("obstacle cell range"))
        })
        .collect::<crate::mapfile::Result<_>>()?;
    Ok((w, h, cells))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapfile::params::MapParams;
    use crate::mapfile::path::PathBloated;
    use crate::mapfile::testdata::MapPair;

    fn key(o: &Obstacle) -> [u32; 3] {
        [o.x.to_bits(), o.y.to_bits(), o.radius.to_bits()]
    }

    /// Re-bucketing the client's obstacles gives the client's grid: same
    /// size and the same obstacles in every cell. (The order within a cell
    /// follows the placement order, which is not reproduced yet.)
    #[test]
    fn bucketing_matches_client() {
        for pair in MapPair::all() {
            let Some((_, path)) = pair.chunks(0x1000_0008, 0x2000_0008) else { continue };
            let (_, params) = pair.chunks(0x1000_000C, 0x2000_000C).unwrap();
            let params = MapParams::parse(&params).unwrap();
            let bounds = [params.min_x, params.min_y, params.max_x, params.max_y];
            let client = PathBloated::parse(&path).unwrap().obstacles;
            let (w, h, cells) = parse(client).unwrap();

            let mut unique: Vec<Obstacle> = Vec::new();
            for o in cells.iter().flatten() {
                if !unique.iter().any(|u| key(u) == key(o)) {
                    unique.push(*o);
                }
            }
            let ours = export(bounds, &unique);
            let (ow, oh, ours) = parse(&ours).unwrap();
            assert_eq!((ow, oh), (w, h), "{pair:?}");
            for (i, (a, b)) in ours.iter().zip(&cells).enumerate() {
                let mut a: Vec<_> = a.iter().map(key).collect();
                let mut b: Vec<_> = b.iter().map(key).collect();
                a.sort();
                a.dedup();
                b.sort();
                b.dedup();
                assert_eq!(a, b, "{pair:?} cell {i}");
            }
            eprintln!("{pair:?}: {w}x{h} cells, {} obstacles", unique.len());
        }
    }

    #[test]
    fn empty_grid() {
        let data = export([-2048.0, -1024.0, 2048.0, 1024.0], &[]);
        assert_eq!(&data[..6], &[4, 0, 2, 0, 0, 0]);
        assert_eq!(data.len(), 6 + 8 * 3);
        assert!(data[6..].iter().all(|&b| b == 0));
    }
}

//! The shards mark in a terminal, for `kiln setup`'s opening: hyperlight-site's project mark for
//! shards (`components/project-mark.tsx`), which shards' own logo fills (its `docs/assets/brand`).
//! Three faceted shards float apart at unequal heights, each split along its ridge into a solid
//! face (the one left of the ridge) and a lit one. Drawn with half blocks, two pixels to a cell;
//! the shards rise to their places as setup starts, then drift on their own while their colours
//! move through the shards study's palette (`components/studies/shards.tsx`), as on the site.

/// A colour: red, green, blue.
pub type Rgb = (u8, u8, u8);

/// The shards study's palette.
pub const PALETTE: [Rgb; 5] = [(155, 191, 198), (154, 165, 211), (190, 155, 189), (212, 188, 152), (189, 212, 190)];

/// What a lit face lightens toward: hyperlight's foreground, `#ededee`.
const LIGHT: Rgb = (237, 237, 238);
/// How far a lit face goes toward [`LIGHT`].
const LIT: f32 = 0.42;

/// The mark's shards on its 32-unit grid: each one's outline, and its ridge from top to bottom.
const SHARDS: [(&[(f32, f32)], &[(f32, f32)]); 3] = [
    (&[(14.0, 6.0), (19.0, 2.0), (21.0, 12.0), (16.0, 19.0), (12.0, 14.0)], &[(19.0, 2.0), (16.0, 12.0), (16.0, 19.0)]),
    (&[(3.0, 14.0), (9.0, 9.0), (11.0, 18.0), (7.0, 26.0), (4.0, 20.0)], &[(9.0, 9.0), (6.0, 18.0), (7.0, 26.0)]),
    (&[(23.0, 17.0), (29.0, 11.0), (27.0, 25.0), (22.0, 30.0), (21.0, 23.0)], &[(29.0, 11.0), (24.0, 23.0), (22.0, 30.0)]),
];
/// The grid the shards are drawn on.
const GRID: f32 = 32.0;
/// How far below its place a shard starts, in grid units: out of sight, under the drawing.
const DEPTH: f32 = 34.0;

/// One cell of the drawing: its upper and lower pixel (`None`: the terminal's own background).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    pub top: Option<Rgb>,
    pub bottom: Option<Rgb>,
}

/// `a`, `t` of the way to `b`.
pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let one = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round().clamp(0.0, 255.0) as u8;
    (one(a.0, b.0), one(a.1, b.1), one(a.2, b.2))
}

/// The study's colours at `t` seconds: each of its palette's, `t * 0.45` of the way on toward the
/// ones after it.
pub fn colors(t: f32) -> [Rgb; 5] {
    let n = PALETTE.len() as f32;
    let at = |i: usize| {
        let p = (i as f32 + t * 0.45).rem_euclid(n);
        let a = p.floor() as usize % PALETTE.len();
        let from = PALETTE.get(a).copied().unwrap_or(LIGHT);
        let to = PALETTE.get((a + 1) % PALETTE.len()).copied().unwrap_or(LIGHT);
        mix(from, to, p - p.floor())
    };
    [at(0), at(1), at(2), at(3), at(4)]
}

/// Whether `(x, y)` is inside `outline` (even-odd).
fn inside(outline: &[(f32, f32)], x: f32, y: f32) -> bool {
    let mut odd = false;
    for (i, &(x1, y1)) in outline.iter().enumerate() {
        let Some(&(x2, y2)) = outline.get((i + 1) % outline.len()) else { continue };
        if (y1 > y) != (y2 > y) && x < x1 + (y - y1) * (x2 - x1) / (y2 - y1) {
            odd = !odd;
        }
    }
    odd
}

/// Where the ridge is across at height `y` (it runs from the shard's top to its bottom).
fn ridge_x(ridge: &[(f32, f32)], y: f32) -> f32 {
    for pair in ridge.windows(2) {
        if let [(x1, y1), (x2, y2)] = *pair
            && (y1..=y2).contains(&y)
        {
            return if y2 > y1 { x1 + (y - y1) * (x2 - x1) / (y2 - y1) } else { x1 };
        }
    }
    match (ridge.first(), ridge.last()) {
        (Some(&(x, top)), _) if y < top => x,
        (_, Some(&(x, _))) => x,
        _ => GRID / 2.0,
    }
}

/// How far shard `k` has risen when the mark as a whole has `rise` (0 to 1): one after the
/// other, each slowing as it arrives.
fn risen(rise: f32, k: usize) -> f32 {
    let own = (rise * 1.6 - k as f32 * 0.3).clamp(0.0, 1.0);
    1.0 - (1.0 - own).powi(3)
}

/// The mark `size` pixels square, as rows of `size` cells (two pixel rows each), at `t` seconds;
/// `rise` (0 to 1) is how far the shards have come up to their places.
pub fn frame(size: usize, t: f32, rise: f32) -> Vec<Vec<Cell>> {
    let size = size.max(2);
    let unit = GRID / size as f32;
    let colors = colors(t);
    let mut pixels = vec![vec![None::<Rgb>; size]; size + size % 2];
    for (k, (outline, ridge)) in SHARDS.iter().enumerate() {
        // Each its own way, as the study's containers drift: a unit or so, never in step.
        let drift = (t * 1.06 + k as f32 * 1.97).sin() * 1.1;
        let below = (1.0 - risen(rise, k)) * DEPTH;
        let solid = colors.get((k * 2) % colors.len()).copied().unwrap_or(LIGHT);
        let lit = mix(solid, LIGHT, LIT);
        for (j, row) in pixels.iter_mut().enumerate() {
            let y = (j as f32 + 0.5) * unit - drift - below;
            for (i, px) in row.iter_mut().enumerate() {
                let x = (i as f32 + 0.5) * unit;
                if inside(outline, x, y) {
                    *px = Some(if x < ridge_x(ridge, y) { solid } else { lit });
                }
            }
        }
    }
    pixels.chunks(2).map(|pair| (0..size).map(|i| Cell { top: pair.first().and_then(|r| r.get(i).copied().flatten()), bottom: pair.get(1).and_then(|r| r.get(i).copied().flatten()) }).collect()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(frame: &[Vec<Cell>]) -> usize {
        frame.iter().flatten().map(|c| usize::from(c.top.is_some()) + usize::from(c.bottom.is_some())).sum()
    }

    #[test]
    fn the_mark_is_three_shards_with_a_solid_and_a_lit_face() {
        let f = frame(22, 0.0, 1.0);
        assert_eq!(f.len(), 11, "two pixel rows to a cell");
        assert!(f.iter().all(|r| r.len() == 22));
        // The three shards, apart: columns with nothing drawn between them.
        let drawn: Vec<bool> = (0..22).map(|i| f.iter().any(|r| r.get(i).is_some_and(|c| c.top.is_some() || c.bottom.is_some()))).collect();
        let groups = drawn.windows(2).filter(|w| matches!(w, [false, true])).count() + usize::from(drawn.first() == Some(&true));
        assert!(groups >= 2, "{drawn:?}");
        // Each shard's colours are its solid face and that lightened.
        let colours = colors(0.0);
        let mut seen: Vec<Rgb> = f.iter().flatten().flat_map(|c| [c.top, c.bottom]).flatten().collect();
        seen.sort_unstable();
        seen.dedup();
        for k in 0..3 {
            let solid = colours.get(k * 2).copied().unwrap_or(LIGHT);
            assert!(seen.contains(&solid), "shard {k}'s solid face");
            assert!(seen.contains(&mix(solid, LIGHT, LIT)), "shard {k}'s lit face");
        }
        assert_eq!(seen.len(), 6, "{seen:?}");
    }

    #[test]
    fn the_shards_rise_into_place_and_drift_there() {
        assert_eq!(lit(&frame(22, 0.0, 0.0)), 0, "before they rise, nothing shows");
        let halfway = lit(&frame(22, 0.0, 0.45));
        let there = lit(&frame(22, 0.0, 1.0));
        assert!(0 < halfway && halfway < there, "{halfway} {there}");
        // Settled, they still move and change colour, a little at a time.
        let (a, b) = (frame(22, 0.0, 1.0), frame(22, 1.5, 1.0));
        assert_ne!(a, b);
        assert!(lit(&b).abs_diff(there) * 5 < there, "the shards keep their size as they drift");
        assert_eq!(frame(22, 0.7, 1.0), frame(22, 0.7, 1.0), "a frame is its time's");
    }

    #[test]
    fn colours_move_through_the_palette() {
        assert_eq!(colors(0.0), PALETTE);
        // After 1 / 0.45 seconds each colour is the next one's.
        let later = colors(1.0 / 0.45);
        for (i, c) in later.iter().enumerate() {
            let next = PALETTE.get((i + 1) % PALETTE.len()).copied().unwrap_or(LIGHT);
            assert!(c.0.abs_diff(next.0) <= 1 && c.1.abs_diff(next.1) <= 1 && c.2.abs_diff(next.2) <= 1, "{c:?} {next:?}");
        }
        assert_eq!(mix((0, 0, 0), (200, 100, 50), 0.5), (100, 50, 25));
        assert_eq!(mix((10, 10, 10), (20, 20, 20), 7.0), (20, 20, 20), "clamped");
    }
}

//! `scrawl measure`: exact sizes and gaps of what a keyframe shows, so an agent reads them
//! instead of estimating them from the image (a screen ruler the agent points).
//!
//! The agent gives a point or a region as fractions of the keyframe image it sees
//! (`frames/frame-NN.png`, auto-cropped). They are mapped through the keyframe's crop onto the
//! full, ink-free frame decoded from the movie, else the bundle's uncropped `frames-full/`
//! image, else the cropped image itself. The measuring below works on any RGBA frame.

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use crate::autocrop::{FULL, Rect};
use crate::model::{CaptureMode, RgbaFrame, ScrawlBundle};

/// Largest per-channel difference from the start color a filled pixel may have.
const FILL_TOLERANCE: u8 = 10;
/// Largest per-channel difference from the background a pixel may have and still be background.
const MASK_TOLERANCE: u8 = 8;
/// A fill stops after covering this share of the image (a point on the page background).
const FILL_CAP: f64 = 0.5;
/// Region mode drops parts smaller than this on either side (pt).
const MIN_SIDE_PT: f64 = 6.0;
/// Region mode keeps at most this many boxes, largest first.
const MAX_BOXES: usize = 30;
/// Region mode joins parts on one row that are at most this far apart (pt), so the letters of a
/// word are one box and never a gap.
const JOIN_PT: f64 = 1.5;
/// Point mode adds at most this many blurred edge pixels to each side of a box.
const BLUR_PX: u32 = 2;
/// A border or an antialiased rim next to a box is at most this wide (px), or 1 pt when that is
/// more: wider is a gap.
const RIM_PX: u32 = 3;

/// A pixel rect: `x..x + width`, `y..y + height`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct PxBox {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PxBox {
    fn x1(&self) -> u32 {
        self.x + self.width
    }
    fn y1(&self) -> u32 {
        self.y + self.height
    }
    fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
    fn contains(&self, o: &PxBox) -> bool {
        o.x >= self.x && o.y >= self.y && o.x1() <= self.x1() && o.y1() <= self.y1()
    }
}

#[inline]
fn rgb(frame: &RgbaFrame, x: u32, y: u32) -> [u8; 3] {
    let i = (y as usize * frame.width as usize + x as usize) * 4;
    [frame.rgba[i], frame.rgba[i + 1], frame.rgba[i + 2]]
}

#[inline]
fn near(a: [u8; 3], b: [u8; 3], tolerance: u8) -> bool {
    a[0].abs_diff(b[0]) <= tolerance
        && a[1].abs_diff(b[1]) <= tolerance
        && a[2].abs_diff(b[2]) <= tolerance
}

/// Squared distance between two colors.
fn distance(a: [u8; 3], b: [u8; 3]) -> u32 {
    (0..3).map(|i| u32::from(a[i].abs_diff(b[i])).pow(2)).sum()
}

fn luma(c: [u8; 3]) -> i32 {
    (299 * i32::from(c[0]) + 587 * i32::from(c[1]) + 114 * i32::from(c[2])) / 1000
}

/// Whether a blurred edge pixel is on the `own` side of the edge between `own` and `beyond`: it
/// is nearer `own`, and no darker or lighter than both (a border is, and H.264 4:2:0 blurs only
/// the hue of an edge pixel, not its brightness).
fn leans(pixel: [u8; 3], own: [u8; 3], beyond: [u8; 3]) -> bool {
    let (y, a, b) = (luma(pixel), luma(own), luma(beyond));
    let slack = i32::from(FILL_TOLERANCE);
    !near(own, beyond, FILL_TOLERANCE)
        && distance(pixel, own) < distance(pixel, beyond)
        && y >= a.min(b) - slack
        && y <= a.max(b) + slack
}

/// The component that holds a point.
#[derive(Clone, Debug, PartialEq)]
pub struct Fill {
    pub bbox: PxBox,
    /// Where the fill started: the pixel nearest the point with the area's main color.
    pub seed: (u32, u32),
    pub color: [u8; 3],
    pub pixels: usize,
    /// The fill hit its cap, so `bbox` is only part of it.
    pub capped: bool,
    pub touches_border: bool,
    /// The fill's pixels on the left, right, top and bottom side of `bbox`.
    sides: [Vec<(u32, u32)>; 4],
}

/// The main color within `radius` of the point, and the pixel nearest the point that has it, so
/// a point on a line of text or a few px off still starts on the surface around it.
fn seed_at(frame: &RgbaFrame, x: u32, y: u32, radius: u32) -> ((u32, u32), [u8; 3]) {
    let (x0, x1) = (x.saturating_sub(radius), (x + radius).min(frame.width - 1));
    let (y0, y1) = (y.saturating_sub(radius), (y + radius).min(frame.height - 1));
    let mut colors: Vec<([u8; 3], usize)> = Vec::with_capacity(64);
    for py in y0..=y1 {
        for px in x0..=x1 {
            let c = rgb(frame, px, py);
            match colors.iter_mut().find(|(k, _)| near(*k, c, 3)) {
                Some((_, n)) => *n += 1,
                None => colors.push((c, 1)),
            }
        }
    }
    let group = colors
        .iter()
        .max_by_key(|(_, n)| *n)
        .map(|(c, _)| *c)
        .unwrap_or_else(|| rgb(frame, x, y));
    // The group's most common shade, so a fill's tolerance sits round the color itself and not
    // round the first shade of it that the window happened to hold.
    let mut shades: Vec<([u8; 3], usize)> = Vec::new();
    for py in y0..=y1 {
        for px in x0..=x1 {
            let c = rgb(frame, px, py);
            if near(c, group, 3) {
                match shades.iter_mut().find(|(k, _)| *k == c) {
                    Some((_, n)) => *n += 1,
                    None => shades.push((c, 1)),
                }
            }
        }
    }
    // `max_by_key` keeps the last of equal counts: reversed, the first shade found.
    let color = shades
        .iter()
        .rev()
        .max_by_key(|(_, n)| *n)
        .map_or(group, |(c, _)| *c);
    let mut best = ((x, y), u64::MAX);
    for py in y0..=y1 {
        for px in x0..=x1 {
            if near(rgb(frame, px, py), color, 3) {
                let d = u64::from(px.abs_diff(x)).pow(2) + u64::from(py.abs_diff(y)).pow(2);
                if d < best.1 {
                    best = ((px, py), d);
                }
            }
        }
    }
    (best.0, color)
}

/// Flood fill (4-connected) from the area around `(x, y)` over pixels within `tolerance` of its
/// color, stopping after `cap` pixels.
pub fn fill_at(frame: &RgbaFrame, x: u32, y: u32, radius: u32, tolerance: u8, cap: usize) -> Fill {
    let (x, y) = (x.min(frame.width - 1), y.min(frame.height - 1));
    let (seed, color) = seed_at(frame, x, y, radius);
    flood(frame, seed, color, tolerance, cap)
}

/// Flood fill (4-connected) from `seed` over pixels within `tolerance` of `color`, stopping
/// after `cap` pixels.
fn flood(frame: &RgbaFrame, seed: (u32, u32), color: [u8; 3], tolerance: u8, cap: usize) -> Fill {
    let (w, h) = (frame.width, frame.height);
    let mut visited = vec![false; w as usize * h as usize];
    let mut stack = vec![seed];
    visited[seed.1 as usize * w as usize + seed.0 as usize] = true;
    let (mut bx0, mut by0, mut bx1, mut by1) = (seed.0, seed.1, seed.0, seed.1);
    // The first and last filled pixel of each row and of each column.
    let mut rows = vec![(u32::MAX, 0u32); h as usize];
    let mut columns = vec![(u32::MAX, 0u32); w as usize];
    let mut pixels = 0usize;
    let mut capped = false;
    while let Some((px, py)) = stack.pop() {
        pixels += 1;
        bx0 = bx0.min(px);
        by0 = by0.min(py);
        bx1 = bx1.max(px);
        by1 = by1.max(py);
        let (row, column) = (&mut rows[py as usize], &mut columns[px as usize]);
        *row = (row.0.min(px), row.1.max(px));
        *column = (column.0.min(py), column.1.max(py));
        if pixels >= cap {
            capped = true;
            break;
        }
        let mut push = |nx: u32, ny: u32| {
            let i = ny as usize * w as usize + nx as usize;
            if !visited[i] && near(rgb(frame, nx, ny), color, tolerance) {
                visited[i] = true;
                stack.push((nx, ny));
            }
        };
        if px > 0 {
            push(px - 1, py);
        }
        if px + 1 < w {
            push(px + 1, py);
        }
        if py > 0 {
            push(px, py - 1);
        }
        if py + 1 < h {
            push(px, py + 1);
        }
    }
    let ys = || (by0..=by1).filter(|&y| rows[y as usize].0 != u32::MAX);
    let xs = || (bx0..=bx1).filter(|&x| columns[x as usize].0 != u32::MAX);
    let sides = [
        ys().filter(|&y| rows[y as usize].0 == bx0)
            .map(|y| (bx0, y))
            .collect(),
        ys().filter(|&y| rows[y as usize].1 == bx1)
            .map(|y| (bx1, y))
            .collect(),
        xs().filter(|&x| columns[x as usize].0 == by0)
            .map(|x| (x, by0))
            .collect(),
        xs().filter(|&x| columns[x as usize].1 == by1)
            .map(|x| (x, by1))
            .collect(),
    ];
    Fill {
        bbox: PxBox {
            x: bx0,
            y: by0,
            width: bx1 - bx0 + 1,
            height: by1 - by0 + 1,
        },
        seed,
        color,
        pixels,
        capped,
        touches_border: bx0 == 0 || by0 == 0 || bx1 == w - 1 || by1 == h - 1,
        sides,
    }
}

/// The pixels of `window` joined (4-connected) to `seed` through pixels within `tolerance` of
/// `color`, as a mask over `window`.
fn fill_within(
    frame: &RgbaFrame,
    window: PxBox,
    seed: (u32, u32),
    color: [u8; 3],
    tolerance: u8,
) -> Vec<bool> {
    let (w, h) = (window.width as usize, window.height as usize);
    let mut mask = vec![false; w * h];
    let at = |x: u32, y: u32| (y - window.y) as usize * w + (x - window.x) as usize;
    let mut stack = vec![seed];
    mask[at(seed.0, seed.1)] = true;
    while let Some((px, py)) = stack.pop() {
        let mut push = |nx: u32, ny: u32| {
            if !mask[at(nx, ny)] && near(rgb(frame, nx, ny), color, tolerance) {
                mask[at(nx, ny)] = true;
                stack.push((nx, ny));
            }
        };
        if px > window.x {
            push(px - 1, py);
        }
        if px + 1 < window.x1() {
            push(px + 1, py);
        }
        if py > window.y {
            push(px, py - 1);
        }
        if py + 1 < window.y1() {
            push(px, py + 1);
        }
    }
    mask
}

/// A fill's box with its blurred edges. Antialiasing and H.264 leave a pixel or two of blend at
/// an edge, which the fill stops short of. Each side takes in up to `BLUR_PX` px while most of
/// the pixels next to the fill's own edge pixels lean to the fill's color rather than to the
/// color 2 px further out, so the edge sits midway between the two colors. A pixel of the same
/// shade as the one after it is no blend: it starts a line or a gap.
fn blurred_box(frame: &RgbaFrame, fill: &Fill) -> PxBox {
    let b = fill.bbox;
    if fill.capped {
        return b;
    }
    let (w, h) = (i64::from(frame.width), i64::from(frame.height));
    let grow = |edge: &[(u32, u32)], dx: i64, dy: i64| -> u32 {
        let at = |(x, y): (u32, u32), k: i64| {
            let (px, py) = (i64::from(x) + dx * k, i64::from(y) + dy * k);
            (px >= 0 && py >= 0 && px < w && py < h).then(|| rgb(frame, px as u32, py as u32))
        };
        let mut grown = 0;
        for k in 1..=i64::from(BLUR_PX) {
            let votes = edge
                .iter()
                .filter(|&&p| match (at(p, k), at(p, k + 1), at(p, k + 2)) {
                    (Some(pixel), Some(next), Some(beyond)) => {
                        !near(pixel, next, 3) && leans(pixel, fill.color, beyond)
                    }
                    _ => false,
                })
                .count();
            if votes * 2 <= edge.len() {
                break;
            }
            grown += 1;
        }
        grown
    };
    let [left, right, up, down] = &fill.sides;
    let (left, right) = (grow(left, -1, 0), grow(right, 1, 0));
    let (up, down) = (grow(up, 0, -1), grow(down, 0, 1));
    PxBox {
        x: b.x - left,
        y: b.y - up,
        width: b.width + left + right,
        height: b.height + up + down,
    }
}

/// Whether a small fill is one solid shape, as a dot, a square or a rounded box is and a glyph
/// is not: it covers 0.65 of its box (a circle covers 0.79) and its rows and columns have
/// almost no gaps.
fn solid(frame: &RgbaFrame, fill: &Fill) -> bool {
    let b = fill.bbox;
    let (w, h) = (b.width as usize, b.height as usize);
    let mask = fill_within(frame, b, fill.seed, fill.color, FILL_TOLERANCE);
    // The pixels from the first to the last filled one of a line, summed over the lines.
    let spans = |lines: usize, len: usize, at: &dyn Fn(usize, usize) -> usize| -> usize {
        (0..lines)
            .map(|line| {
                let mut filled = (0..len).filter(|&i| mask[at(line, i)]);
                match (filled.next(), filled.next_back()) {
                    (Some(first), Some(last)) => last - first + 1,
                    (Some(_), None) => 1,
                    _ => 0,
                }
            })
            .sum()
    };
    let pixels = mask.iter().filter(|&&on| on).count();
    let (rows, columns) = (
        spans(h, w, &|y, x| y * w + x),
        spans(w, h, &|x, y| y * w + x),
    );
    pixels * 20 >= w * h * 13 && pixels * 10 >= rows * 9 && pixels * 10 >= columns * 9
}

/// Where a small fill's own color starts again when the fill is a hole in a mark, such as the
/// counter of an "o" or the space shut inside two letters: within half of `min_side` px past its
/// box on at least three sides, as one region that goes round it.
fn hole_exit(frame: &RgbaFrame, fill: &Fill, min_side: u32) -> Option<((u32, u32), [u8; 3])> {
    let b = fill.bbox;
    let own = |x: u32, y: u32| near(rgb(frame, x, y), fill.color, FILL_TOLERANCE);
    let (cx, cy) = (b.x + b.width / 2, b.y + b.height / 2);
    let reach = (min_side / 2).max(1);
    let found: Vec<(u32, u32)> = [
        (1..=reach)
            .map_while(|k| b.x.checked_sub(k))
            .map(|x| (x, cy))
            .find(|&(x, y)| own(x, y)),
        (0..reach)
            .map(|k| (b.x1() + k, cy))
            .take_while(|&(x, _)| x < frame.width)
            .find(|&(x, y)| own(x, y)),
        (1..=reach)
            .map_while(|k| b.y.checked_sub(k))
            .map(|y| (cx, y))
            .find(|&(x, y)| own(x, y)),
        (0..reach)
            .map(|k| (cx, b.y1() + k))
            .take_while(|&(_, y)| y < frame.height)
            .find(|&(x, y)| own(x, y)),
    ]
    .into_iter()
    .flatten()
    .collect();
    if found.len() < 3 {
        return None;
    }
    // One region, when it joins inside a window a few components wide.
    let margin = 2 * min_side;
    let (x0, y0) = (b.x.saturating_sub(margin), b.y.saturating_sub(margin));
    let window = PxBox {
        x: x0,
        y: y0,
        width: (b.x1() + margin).min(frame.width) - x0,
        height: (b.y1() + margin).min(frame.height) - y0,
    };
    let ww = window.width as usize;
    let mask = found.iter().find_map(|&from| {
        let mask = fill_within(frame, window, from, fill.color, FILL_TOLERANCE);
        let joined =
            |&(x, y): &(u32, u32)| mask[(y - window.y) as usize * ww + (x - window.x) as usize];
        (found.iter().filter(|p| joined(p)).count() >= 3).then_some(mask)
    })?;
    // The region's main color (the hole's own may be a blurred shade of it), and its pixel
    // nearest the hole.
    let region = || {
        mask.iter()
            .enumerate()
            .filter(|(_, on)| **on)
            .map(|(i, _)| (window.x + (i % ww) as u32, window.y + (i / ww) as u32))
    };
    let mut colors: std::collections::HashMap<[u8; 3], (usize, [u8; 3])> =
        std::collections::HashMap::new();
    for (x, y) in region() {
        let c = rgb(frame, x, y);
        colors.entry(c.map(|v| v >> 2)).or_insert((0, c)).0 += 1;
    }
    let (_, color) = colors.into_values().max_by_key(|(n, c)| (*n, *c))?;
    let seed = region()
        .filter(|&(x, y)| near(rgb(frame, x, y), color, 3))
        .min_by_key(|&(x, y)| {
            (
                u64::from(x.abs_diff(cx)).pow(2) + u64::from(y.abs_diff(cy)).pow(2),
                y,
                x,
            )
        })?;
    Some((seed, color))
}

/// The color around a box: the main color, other than the fill's, on the ring 2 px outside
/// `around`, the ring pixel with that color nearest `(x, y)`, and whether at least 3/4 of the
/// ring has it.
fn ring(
    frame: &RgbaFrame,
    fill: &Fill,
    around: PxBox,
    x: u32,
    y: u32,
) -> Option<((u32, u32), [u8; 3], bool)> {
    let b = around;
    let (x0, y0) = (b.x.saturating_sub(2), b.y.saturating_sub(2));
    let (x1, y1) = (
        (b.x1() + 1).min(frame.width - 1),
        (b.y1() + 1).min(frame.height - 1),
    );
    let pixels: Vec<(u32, u32)> = (x0..=x1)
        .flat_map(|px| [(px, y0), (px, y1)])
        .chain((y0..=y1).flat_map(|py| [(x0, py), (x1, py)]))
        .collect();
    let mut colors: Vec<([u8; 3], usize, (u32, u32), u64)> = Vec::new();
    for &(px, py) in &pixels {
        let c = rgb(frame, px, py);
        if near(c, fill.color, FILL_TOLERANCE) {
            continue;
        }
        let d = u64::from(px.abs_diff(x)).pow(2) + u64::from(py.abs_diff(y)).pow(2);
        match colors.iter_mut().find(|(k, ..)| near(*k, c, 3)) {
            Some((_, n, at, best)) => {
                *n += 1;
                if d < *best {
                    *at = (px, py);
                    *best = d;
                }
            }
            None => colors.push((c, 1, (px, py), d)),
        }
    }
    let (color, _, at, _) = colors.into_iter().max_by_key(|(_, n, ..)| *n)?;
    let same = pixels
        .iter()
        .filter(|&&(px, py)| near(rgb(frame, px, py), color, FILL_TOLERANCE))
        .count();
    Some((at, color, same * 4 >= pixels.len() * 3))
}

/// What a fill is.
enum Kind {
    /// A component.
    Part,
    /// A hole in a mark: past the mark, the surface goes on from this pixel in this color.
    Hole((u32, u32), [u8; 3]),
    /// A mark on a surface, such as one glyph of a button's label or a thin line.
    Mark,
}

/// A fill is a mark when it is 2 px thin or less, or when it is under `min_side` px on both
/// sides and its surroundings say so, since size alone cannot tell a status dot from the counter
/// of an "o". A small component is solid, at least a quarter of `min_side` on a side, and has
/// one color round it (3/4 of the ring 2 px outside its blurred edges) whose own region is a
/// surface. A hole has its own color again just past a stroke on three sides (`hole_exit`). A
/// piece of a letter has a ring of mixed shades, or of a stroke that is itself small and not
/// solid.
fn kind(frame: &RgbaFrame, fill: &Fill, min_side: u32) -> Kind {
    let b = fill.bbox;
    if fill.capped {
        return Kind::Part;
    }
    let small = b.width < min_side && b.height < min_side;
    if small && let Some((seed, color)) = hole_exit(frame, fill, min_side) {
        return Kind::Hole(seed, color);
    }
    if b.width.min(b.height) <= 2 {
        return Kind::Mark;
    }
    if !small {
        return Kind::Part;
    }
    let grown = blurred_box(frame, fill);
    if grown.width * 4 < min_side && grown.height * 4 < min_side {
        return Kind::Mark;
    }
    let on_a_surface = || {
        let Some((at, color, true)) = ring(frame, fill, grown, fill.seed.0, fill.seed.1) else {
            return false;
        };
        // More than `min_side` squared px cannot be a small mark.
        let cap = (min_side as usize).pow(2) + 1;
        let around = flood(frame, at, color, FILL_TOLERANCE, cap);
        let a = around.bbox;
        around.capped
            || (a.width.min(a.height) > 2
                && (a.width >= min_side
                    || a.height >= min_side
                    || (around.pixels as u64) * 4 >= a.area() * 3))
    };
    if solid(frame, fill) && on_a_surface() {
        Kind::Part
    } else {
        Kind::Mark
    }
}

fn is_mark(frame: &RgbaFrame, fill: &Fill, min_side: u32) -> bool {
    !matches!(kind(frame, fill, min_side), Kind::Part)
}

/// The fill of the main color within `radius` of the point that is not a mark, trying that
/// color's pixels nearest first (at most 24 fills).
fn surface_near(frame: &RgbaFrame, x: u32, y: u32, radius: u32, cap: usize) -> Option<Fill> {
    let (_, color) = seed_at(frame, x, y, radius);
    let (x0, x1) = (x.saturating_sub(radius), (x + radius).min(frame.width - 1));
    let (y0, y1) = (y.saturating_sub(radius), (y + radius).min(frame.height - 1));
    let mut candidates: Vec<(u64, u32, u32)> = Vec::new();
    for py in y0..=y1 {
        for px in x0..=x1 {
            if near(rgb(frame, px, py), color, 3) {
                let d = u64::from(px.abs_diff(x)).pow(2) + u64::from(py.abs_diff(y)).pow(2);
                candidates.push((d, px, py));
            }
        }
    }
    candidates.sort_unstable();
    let mut tried: Vec<PxBox> = Vec::new();
    for (_, px, py) in candidates {
        let at = PxBox {
            x: px,
            y: py,
            width: 1,
            height: 1,
        };
        if tried.iter().any(|b| b.contains(&at)) {
            continue;
        }
        if tried.len() == 24 {
            break;
        }
        let fill = flood(frame, (px, py), color, FILL_TOLERANCE, cap);
        if !is_mark(frame, &fill, radius) {
            return Some(fill);
        }
        tried.push(fill.bbox);
    }
    None
}

/// The component that holds a point, as `fill_at` finds it, with two exceptions. A small solid
/// component under the point, such as a status dot too small to be the main color near it, is
/// the component. A point on a mark (a glyph of a label, the hole of an "o", a thin line)
/// measures the surface around the mark: the hole's own color past the letter, else the color on
/// a ring around the mark, then the main color of a `min_side` px window (`surface_near`).
/// Returns whether it stepped out of a mark.
pub fn component_at(
    frame: &RgbaFrame,
    x: u32,
    y: u32,
    radius: u32,
    min_side: u32,
    cap: usize,
) -> (Fill, bool) {
    let (x, y) = (x.min(frame.width - 1), y.min(frame.height - 1));
    let mut fill = fill_at(frame, x, y, radius, FILL_TOLERANCE, cap);
    let under = rgb(frame, x, y);
    if !near(under, fill.color, FILL_TOLERANCE) {
        let dot = flood(frame, (x, y), under, FILL_TOLERANCE, cap);
        let b = dot.bbox;
        // Roughly round or square: the stem of an "l" is solid too.
        if b.width < min_side
            && b.height < min_side
            && b.width <= 2 * b.height
            && b.height <= 2 * b.width
            && !is_mark(frame, &dot, min_side)
        {
            return (dot, false);
        }
    }
    let mut stepped = false;
    for _ in 0..3 {
        let next = match kind(frame, &fill, min_side) {
            Kind::Part => break,
            Kind::Hole(seed, color) => Some((seed, color)),
            Kind::Mark => ring(frame, &fill, fill.bbox, x, y).map(|(at, color, _)| (at, color)),
        };
        let Some((seed, color)) = next else {
            break;
        };
        fill = flood(frame, seed, color, FILL_TOLERANCE, cap);
        stepped = true;
    }
    if is_mark(frame, &fill, min_side) {
        // Antialiased text can ring a letter with more letter shades: take the main color of a
        // wider window, the surface the text sits on, and fill from the nearest pixel of it that
        // is not shut inside a letter (the hole of an "o").
        if let Some(wide) = surface_near(frame, x, y, min_side, cap) {
            fill = wide;
            stepped = true;
        }
    }
    (fill, stepped)
}

/// How far one ray from a box edge runs before the color changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Reach {
    pub px: u32,
    /// The ray reached the image edge without a change.
    pub image_edge: bool,
}

/// What the rays of a box start with, which tells a border from a divider from a narrow gap.
struct Surround {
    /// The box's own fill color.
    own: [u8; 3],
    /// A run up to this long (px) can be a border or a rim.
    rim: u32,
    /// The colors a ray starts with for longer than `rim`: the surfaces the box sits on.
    surfaces: Vec<[u8; 3]>,
    /// The color every ray starts with for at most `rim` px and that stays by the box: its
    /// border.
    border: Option<[u8; 3]>,
}

/// The run of one color that starts at `ray[i]`: its color and its length. The color is taken
/// 2 px in when the run goes on that far, past the blurred tail of the edge behind it, so the
/// run ends where the settled color does.
fn run_at(ray: &[[u8; 3]], i: usize, tolerance: u8) -> ([u8; 3], u32) {
    let start = ray[i];
    let settled = |c: &&[u8; 3]| near(**c, start, tolerance);
    let color = *ray
        .get(i + BLUR_PX as usize)
        .filter(settled)
        .unwrap_or(&start);
    let n = ray[i..]
        .iter()
        .take_while(|c| near(**c, color, tolerance))
        .count();
    (color, n.max(1) as u32)
}

/// Pixels from a box edge outward to the next component, walking the ray's runs of one color.
/// Short runs are skipped while they add up to at most `rim` px (a border, an antialiased rim),
/// and the first run that does not fit, or that is a surface the box sits on, is the gap: the
/// reach is the skipped runs plus that run. A run of the box's own color ends the ray sooner:
/// the reach is only the skipped runs (a divider between two rows, a narrow gap), unless they
/// were the box's border (an outlined card on a page of its own color).
fn reach(ray: &[[u8; 3]], by: &Surround, tolerance: u8) -> Reach {
    let mut skipped = 0u32;
    let mut past_border = false;
    while (skipped as usize) < ray.len() {
        let (color, n) = run_at(ray, skipped as usize, tolerance);
        let image_edge = (skipped + n) as usize >= ray.len();
        let own = near(color, by.own, tolerance);
        if own && !past_border {
            break;
        }
        let border = skipped == 0
            && !own
            && !image_edge
            && n <= by.rim
            && by.border.is_some_and(|b| near(color, b, tolerance));
        let surface = by.surfaces.iter().any(|s| near(color, *s, tolerance));
        if !border && (own || surface || image_edge || skipped + n > by.rim) {
            // The far end of the gap, as `blurred_box` puts an edge: a blurred pixel that leans
            // to the gap's color is still gap.
            let mut px = skipped + n;
            for _ in 0..BLUR_PX {
                let i = px as usize;
                match (ray.get(i), ray.get(i + 1), ray.get(i + 2)) {
                    (Some(&pixel), Some(&next), Some(&beyond))
                        if !near(pixel, next, 3) && leans(pixel, color, beyond) =>
                    {
                        px += 1
                    }
                    _ => break,
                }
            }
            return Reach { px, image_edge };
        }
        skipped += n;
        past_border |= border;
    }
    Reach {
        px: skipped,
        image_edge: skipped as usize >= ray.len(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Edges {
    pub left: Reach,
    pub right: Reach,
    pub up: Reach,
    pub down: Reach,
}

/// From each side of `bbox`, along the row and column through `at`, the distance to the next
/// component outward (`reach`). `own` is the box's fill color and `rim` the widest border in px.
pub fn edges_around(
    frame: &RgbaFrame,
    bbox: PxBox,
    at: (u32, u32),
    own: [u8; 3],
    rim: u32,
    tolerance: u8,
) -> Edges {
    let (ax, ay) = (
        at.0.clamp(bbox.x, bbox.x1() - 1),
        at.1.clamp(bbox.y, bbox.y1() - 1),
    );
    // Left, right, up, down: each ray's pixels outward, and where it starts.
    let rays: [Vec<[u8; 3]>; 4] = [
        (0..bbox.x).rev().map(|x| rgb(frame, x, ay)).collect(),
        (bbox.x1()..frame.width)
            .map(|x| rgb(frame, x, ay))
            .collect(),
        (0..bbox.y).rev().map(|y| rgb(frame, ax, y)).collect(),
        (bbox.y1()..frame.height)
            .map(|y| rgb(frame, ax, y))
            .collect(),
    ];
    let starts = [
        (bbox.x.wrapping_sub(1), ay),
        (bbox.x1(), ay),
        (ax, bbox.y.wrapping_sub(1)),
        (ax, bbox.y1()),
    ];
    let firsts: Vec<(usize, [u8; 3], u32)> = rays
        .iter()
        .enumerate()
        .filter(|(_, ray)| !ray.is_empty())
        .map(|(i, ray)| {
            let (color, n) = run_at(ray, 0, tolerance);
            (i, color, n)
        })
        .filter(|(_, c, _)| !near(*c, own, tolerance))
        .collect();
    let surfaces = firsts
        .iter()
        .filter(|(_, _, n)| *n > rim)
        .map(|(_, c, _)| *c)
        .collect();
    // A border is short on every side, one color, and goes no further than the box: the lines
    // of a table go on past a cell, so they are dividers.
    let (x0, y0) = (bbox.x.saturating_sub(rim), bbox.y.saturating_sub(rim));
    let (x1, y1) = (bbox.x1() + rim, bbox.y1() + rim);
    let ring_px = 2 * rim as usize * (bbox.width + bbox.height + 2 * rim) as usize;
    let stays = |&(i, color, _): &(usize, [u8; 3], u32)| {
        let strip = flood(frame, starts[i], color, tolerance, ring_px + 1);
        let s = strip.bbox;
        !strip.capped && s.x >= x0 && s.y >= y0 && s.x1() <= x1 && s.y1() <= y1
    };
    let border = firsts
        .first()
        .map(|(_, c, _)| *c)
        .filter(|_| firsts.len() == rays.iter().filter(|r| !r.is_empty()).count())
        .filter(|c| {
            firsts
                .iter()
                .all(|(_, o, n)| *n <= rim && near(*o, *c, tolerance))
        })
        .filter(|_| firsts.iter().all(stays));
    let by = Surround {
        own,
        rim,
        surfaces,
        border,
    };
    let [left, right, up, down] = rays.map(|ray| reach(&ray, &by, tolerance));
    Edges {
        left,
        right,
        up,
        down,
    }
}

/// The region's background: the most common color on its border.
fn background(frame: &RgbaFrame, r: PxBox) -> [u8; 3] {
    let mut counts: std::collections::HashMap<u32, (usize, [u8; 3])> =
        std::collections::HashMap::new();
    let mut add = |x: u32, y: u32| {
        let c = rgb(frame, x, y);
        let key = (u32::from(c[0] >> 2) << 12) | (u32::from(c[1] >> 2) << 6) | u32::from(c[2] >> 2);
        counts.entry(key).or_insert((0, c)).0 += 1;
    };
    for x in r.x..r.x1() {
        add(x, r.y);
        add(x, r.y1() - 1);
    }
    for y in r.y..r.y1() {
        add(r.x, y);
        add(r.x1() - 1, y);
    }
    counts
        .into_values()
        .max_by_key(|(n, _)| *n)
        .map(|(_, c)| c)
        .unwrap_or([255, 255, 255])
}

/// The parts of a region.
pub struct Parts {
    /// The `max` largest top-level parts, in reading order.
    pub kept: Vec<PxBox>,
    /// Every top-level part, in reading order.
    pub all: Vec<PxBox>,
    /// How many parts were under `min_side` px on a side, such as a thin line or small text.
    pub small: usize,
}

/// The parts that are not inside another part's box. Largest first, each part is tested only
/// against the top-level parts over the 64 px cell of its corner, so a region full of small
/// parts takes about one pass.
fn top_level(mut parts: Vec<PxBox>, region: PxBox) -> Vec<PxBox> {
    const CELL: u32 = 64;
    let columns = (region.width / CELL + 1) as usize;
    let cell = |x: u32, y: u32| {
        ((y - region.y) / CELL) as usize * columns + ((x - region.x) / CELL) as usize
    };
    let mut cells: Vec<Vec<u32>> = vec![Vec::new(); columns * (region.height / CELL + 1) as usize];
    // A stable sort: of two equal boxes the first one found stays.
    parts.sort_by_key(|b| std::cmp::Reverse(b.area()));
    let mut top: Vec<PxBox> = Vec::new();
    for b in parts {
        if cells[cell(b.x, b.y)]
            .iter()
            .any(|&i| top[i as usize].contains(&b))
        {
            continue;
        }
        for y in ((b.y - region.y) / CELL)..=((b.y1() - 1 - region.y) / CELL) {
            for x in ((b.x - region.x) / CELL)..=((b.x1() - 1 - region.x) / CELL) {
                cells[y as usize * columns + x as usize].push(top.len() as u32);
            }
        }
        top.push(b);
    }
    top
}

/// A part's box without the background's side of its blurred edges, which the mask takes in
/// with the part: H.264 tints the background for up to 3 px next to a saturated edge. Each side
/// gives up to 3 lines while most of a line's pixels off the background lean to the background
/// rather than to the part's color 4 px further in (a line with none goes too). The mirror of
/// `blurred_box`, so both modes put an edge midway between the two colors.
fn trimmed(frame: &RgbaFrame, mut b: PxBox, bg: [u8; 3], tolerance: u8) -> PxBox {
    const INNER: u32 = 2 * BLUR_PX;
    // Whether a line goes: each entry is a pixel and the one `INNER` px further in.
    let goes = |line: &mut dyn Iterator<Item = [(u32, u32); 2]>| -> Option<bool> {
        let (mut on, mut weak) = (0usize, 0usize);
        for [at, inner] in line {
            let [pixel, inner] = [at, inner].map(|(x, y)| rgb(frame, x, y));
            if !near(pixel, bg, tolerance) {
                on += 1;
                weak += usize::from(leans(pixel, bg, inner));
            }
        }
        // `Some(true)` for a line of blur, `None` for an empty one.
        (on > 0).then_some(weak * 2 > on)
    };
    for side in 0..4 {
        let mut blurred = 0;
        while blurred < INNER - 1 {
            let (x0, x1, y0, y1) = (b.x, b.x1() - 1, b.y, b.y1() - 1);
            if (side < 2 && b.width <= INNER) || (side >= 2 && b.height <= INNER) {
                break;
            }
            let line = match side {
                0 => goes(&mut (y0..=y1).map(|y| [(x0, y), (x0 + INNER, y)])),
                1 => goes(&mut (y0..=y1).map(|y| [(x1, y), (x1 - INNER, y)])),
                2 => goes(&mut (x0..=x1).map(|x| [(x, y0), (x, y0 + INNER)])),
                _ => goes(&mut (x0..=x1).map(|x| [(x, y1), (x, y1 - INNER)])),
            };
            match line {
                Some(false) => break,
                Some(true) => blurred += 1,
                None => {}
            }
            match side {
                0 => (b.x, b.width) = (b.x + 1, b.width - 1),
                1 => b.width -= 1,
                2 => (b.y, b.height) = (b.y + 1, b.height - 1),
                _ => b.height -= 1,
            }
        }
    }
    b
}

/// The top-level parts inside `region` (in frame px): pixels off the region's background,
/// 8-connected and also joined across up to `join` background px along a row, without parts
/// inside another, parts under `min_side` px on either side, and beyond the `max` largest.
pub fn boxes_in(
    frame: &RgbaFrame,
    region: PxBox,
    tolerance: u8,
    join: u32,
    min_side: u32,
    max: usize,
) -> Parts {
    let reach = join as usize + 1;
    let bg = background(frame, region);
    let (rw, rh) = (region.width as usize, region.height as usize);
    let mut mask = vec![false; rw * rh];
    for y in 0..rh {
        for x in 0..rw {
            mask[y * rw + x] = !near(
                rgb(frame, region.x + x as u32, region.y + y as u32),
                bg,
                tolerance,
            );
        }
    }
    let mut parts: Vec<PxBox> = Vec::new();
    let mut small = 0usize;
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for start in 0..rw * rh {
        if !mask[start] {
            continue;
        }
        mask[start] = false;
        stack.push((start % rw, start / rw));
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
        while let Some((x, y)) = stack.pop() {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
            for ny in y.saturating_sub(1)..=(y + 1).min(rh - 1) {
                let r = if ny == y { reach } else { 1 };
                for nx in x.saturating_sub(r)..=(x + r).min(rw - 1) {
                    let i = ny * rw + nx;
                    if mask[i] {
                        mask[i] = false;
                        stack.push((nx, ny));
                    }
                }
            }
        }
        let b = PxBox {
            x: region.x + x0 as u32,
            y: region.y + y0 as u32,
            width: (x1 - x0 + 1) as u32,
            height: (y1 - y0 + 1) as u32,
        };
        let b = trimmed(frame, b, bg, tolerance);
        if b.width >= min_side && b.height >= min_side {
            parts.push(b);
        } else {
            small += 1;
        }
    }
    let mut kept = top_level(parts, region);
    let all = reading_order(kept.clone());
    kept.sort_by_key(|b| std::cmp::Reverse(b.area()));
    kept.truncate(max);
    Parts {
        kept: reading_order(kept),
        all,
        small,
    }
}

/// Lines top to bottom, each left to right: a box joins the line whose first box's rows hold its
/// vertical centre, so words a px or two apart in height stay on one line.
fn reading_order(mut boxes: Vec<PxBox>) -> Vec<PxBox> {
    boxes.sort_by_key(|b| (b.y, b.x));
    let mut lines: Vec<Vec<PxBox>> = Vec::new();
    for b in boxes {
        let centre = b.y + b.height / 2;
        match lines
            .iter_mut()
            .find(|line| line[0].y <= centre && centre < line[0].y1())
        {
            Some(line) => line.push(b),
            None => lines.push(vec![b]),
        }
    }
    lines
        .into_iter()
        .flat_map(|mut line| {
            line.sort_by_key(|b| b.x);
            line
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// The gap between two boxes, by their 0-based indexes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gap {
    pub a: usize,
    pub b: usize,
    pub px: u32,
    pub axis: Axis,
}

/// The gaps of the `kept` boxes among `all` the boxes found: each kept box's nearest neighbor
/// among those that overlap it on the other axis, once per pair, and only when that neighbor
/// was kept too, so a gap never spans a box that was left out. Numbered as in `kept`. Only the
/// kept boxes look for a neighbor, so a region full of small parts costs one pass per kept box.
fn kept_gaps(all: &[PxBox], kept: &[PxBox]) -> Vec<Gap> {
    let between = |a: &PxBox, b: &PxBox| -> Option<(u32, Axis)> {
        let rows = a.y < b.y1() && b.y < a.y1();
        let cols = a.x < b.x1() && b.x < a.x1();
        if rows && !cols {
            Some((
                if a.x1() <= b.x {
                    b.x - a.x1()
                } else {
                    a.x - b.x1()
                },
                Axis::Horizontal,
            ))
        } else if cols && !rows {
            Some((
                if a.y1() <= b.y {
                    b.y - a.y1()
                } else {
                    a.y - b.y1()
                },
                Axis::Vertical,
            ))
        } else {
            None
        }
    };
    let mut gaps: Vec<Gap> = Vec::new();
    for (i, a) in kept.iter().enumerate() {
        // `min_by_key` keeps the first of equal gaps: the earlier box in reading order.
        let nearest = all
            .iter()
            .filter(|b| *b != a)
            .filter_map(|b| between(a, b).map(|(px, axis)| (b, px, axis)))
            .min_by_key(|(_, px, _)| *px);
        let Some((neighbor, px, axis)) = nearest else {
            continue;
        };
        let Some(j) = kept.iter().position(|k| k == neighbor) else {
            continue;
        };
        let (a, b) = (i.min(j), i.max(j));
        if !gaps.iter().any(|g| g.a == a && g.b == b) {
            gaps.push(Gap { a, b, px, axis });
        }
    }
    gaps.sort_by_key(|g| (g.a, g.b));
    gaps
}

// ---------------------------------------------------------------------------------------------
// The bundle side
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// The full, ink-free frame decoded from the movie.
    Movie,
    /// The bundle's uncropped keyframe (`frames-full/`).
    FullFrame,
    /// The keyframe image itself (`frames/`).
    Keyframe,
}

impl Source {
    fn says(self) -> &'static str {
        match self {
            Source::Movie => "the movie",
            Source::FullFrame => "the full keyframe",
            Source::Keyframe => "the keyframe image",
        }
    }
}

/// The measured image and where it sits in the full frame.
pub struct Picture {
    pub frame: RgbaFrame,
    pub source: Source,
    /// The image's part of the full frame (`FULL` unless the image is the cropped keyframe).
    pub rect: Rect,
    pub warnings: Vec<String>,
}

/// Decodes the full frames of a movie at bundle times (`movie_frames_at` in use).
pub type Decode = fn(&Path, &[u64], u64) -> Result<Vec<RgbaFrame>>;

fn decode_movie(movie: &Path, times: &[u64], offset_ms: u64) -> Result<Vec<RgbaFrame>> {
    crate::movie_frames_at(movie, times, offset_ms, 0)
}

/// The `k`th keyframe's pixels (0-based): the movie, else `frames-full/`, else `frames/`.
pub fn load_picture(
    dir: &Path,
    bundle: &ScrawlBundle,
    k: usize,
    decode: Decode,
) -> Result<Picture> {
    let keyframe = &bundle.keyframes[k];
    let crop = crate::zoom::crop_of(bundle, k).unwrap_or(FULL);
    let mut warnings = Vec::new();
    let pending = dir.join(crate::handoff::MEDIA_PENDING).exists();
    let video = bundle
        .video
        .as_ref()
        .map(|v| dir.join(v))
        .filter(|p| p.is_file() && !pending);
    if let Some(video) = video {
        match decode(&video, &[keyframe.t], bundle.video_offset_ms) {
            Ok(mut frames) if !frames.is_empty() => {
                return Ok(Picture {
                    frame: frames.swap_remove(0),
                    source: Source::Movie,
                    rect: FULL,
                    warnings,
                });
            }
            Ok(_) => warnings.push("the movie gave no frame; measured a saved image".into()),
            Err(error) => warnings.push(format!(
                "the movie could not be read ({error}); measured a saved image"
            )),
        }
    } else if pending {
        warnings.push("the movie is still copying; measured a saved image".into());
    }
    let name = Path::new(&keyframe.file)
        .file_name()
        .context("The keyframe has no file")?;
    let ink = "red ink, pins and click rings drawn on it may show".to_owned();
    if let Some(info) = &bundle.autocrop {
        let path = dir.join(&info.full_frames).join(name);
        if path.is_file() {
            warnings.push(format!("measured {}: {ink}", Source::FullFrame.says()));
            return Ok(Picture {
                frame: read_png(&path)?,
                source: Source::FullFrame,
                rect: FULL,
                warnings,
            });
        }
    }
    let path = dir.join(&keyframe.file);
    ensure!(
        path.is_file(),
        "No pixels for keyframe {}: no movie and no {}",
        keyframe.index,
        path.display()
    );
    warnings.push(format!("measured {}: {ink}", Source::Keyframe.says()));
    Ok(Picture {
        frame: read_png(&path)?,
        source: Source::Keyframe,
        rect: crop,
        warnings,
    })
}

fn read_png(path: &Path) -> Result<RgbaFrame> {
    let file =
        std::fs::File::open(path).with_context(|| format!("Could not open {}", path.display()))?;
    crate::render::decode_png(std::io::BufReader::new(file))
        .with_context(|| format!("Could not decode {}", path.display()))
}

/// The capture's width in points, when known: a phone's screen width (its target frame is
/// video pixels), else the target frame's width.
pub fn width_pt(bundle: &ScrawlBundle) -> Option<f64> {
    let w = if bundle.target.mode == CaptureMode::Device || bundle.device.is_some() {
        bundle.device.as_ref()?.screen?[0]
    } else {
        bundle.target.frame.width
    };
    (w.is_finite() && w > 0.0).then_some(w)
}

/// The image's place in full-frame px: its offset there, and px per pt.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Space {
    pub offset: (f64, f64),
    pub ppp: Option<f64>,
}

pub fn space(picture: &Picture, width_pt: Option<f64>) -> Space {
    let r = picture.rect;
    let full_w = f64::from(picture.frame.width) / f64::from((r[2] - r[0]).max(1e-6));
    let full_h = f64::from(picture.frame.height) / f64::from((r[3] - r[1]).max(1e-6));
    Space {
        offset: (f64::from(r[0]) * full_w, f64::from(r[1]) * full_h),
        ppp: width_pt.map(|pt| full_w / pt),
    }
}

/// A fraction of the keyframe image as a pixel of the picture: through the keyframe's crop
/// when the picture is the full frame, directly when it is the cropped image.
pub fn to_px(picture: &Picture, crop: Rect, fx: f64, fy: f64) -> (f64, f64) {
    let (w, h) = (
        f64::from(picture.frame.width),
        f64::from(picture.frame.height),
    );
    if picture.rect == FULL && crop != FULL {
        let c = crop.map(f64::from);
        (
            (c[0] + fx * (c[2] - c[0])) * w,
            (c[1] + fy * (c[3] - c[1])) * h,
        )
    } else {
        (fx * w, fy * h)
    }
}

pub enum Query {
    At(f64, f64),
    Region([f64; 4]),
}

fn fractions(text: &str, n: usize, flag: &str) -> Result<Vec<f64>> {
    let values: Vec<f64> = text
        .split(',')
        .map(|v| v.trim().parse::<f64>())
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("{flag} takes {n} numbers from 0 to 1, comma-separated"))?;
    ensure!(
        values.len() == n && values.iter().all(|v| (0.0..=1.0).contains(v)),
        "{flag} takes {n} numbers from 0 to 1, comma-separated (fractions of the keyframe image)"
    );
    Ok(values)
}

pub fn parse_point(text: &str) -> Result<Query> {
    let v = fractions(text, 2, "--at")?;
    Ok(Query::At(v[0], v[1]))
}

pub fn parse_region(text: &str) -> Result<Query> {
    let v = fractions(text, 4, "--region")?;
    ensure!(
        v[2] > v[0] && v[3] > v[1],
        "--region is X0,Y0,X1,Y1 with X1 > X0 and Y1 > Y0"
    );
    Ok(Query::Region([v[0], v[1], v[2], v[3]]))
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Dims {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Full-frame px.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct PxDims {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Measured {
    /// 1-based, in reading order (region mode).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<usize>,
    pub px: PxDims,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pt: Option<Dims>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeReport {
    pub px: [u32; 4],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pt: Option<[f64; 4]>,
    /// The sides (`left`, `right`, `up`, `down`) whose ray ran to the image edge.
    pub image_edge: Vec<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GapReport {
    pub from: usize,
    pub to: usize,
    pub axis: Axis,
    pub px: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pt: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Measurement {
    pub frame: usize,
    pub source: Source,
    pub px_per_pt: Option<f64>,
    /// The measured image's size in px.
    pub image: [u32; 2],
    #[serde(rename = "box", skip_serializing_if = "Option::is_none")]
    pub component: Option<Measured>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_edges: Option<EdgeReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boxes: Option<Vec<Measured>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gaps: Option<Vec<GapReport>>,
    pub warnings: Vec<String>,
}

/// To the nearest half point.
fn half(v: f64) -> f64 {
    (v * 2.0).round() / 2.0
}

fn measured(b: PxBox, space: Space, n: Option<usize>) -> Measured {
    let px = PxDims {
        x: (f64::from(b.x) + space.offset.0).round() as i64,
        y: (f64::from(b.y) + space.offset.1).round() as i64,
        width: i64::from(b.width),
        height: i64::from(b.height),
    };
    let pt = space.ppp.map(|p| Dims {
        x: half(px.x as f64 / p),
        y: half(px.y as f64 / p),
        width: half(px.width as f64 / p),
        height: half(px.height as f64 / p),
    });
    Measured { n, px, pt }
}

/// Measures a picture: `crop` is the keyframe's crop (how the agent's fractions map onto it).
pub fn measure_picture(
    frame_n: usize,
    picture: Picture,
    crop: Rect,
    width_pt: Option<f64>,
    query: &Query,
) -> Measurement {
    let space = space(&picture, width_pt);
    let img = &picture.frame;
    let (w, h) = (img.width, img.height);
    let mut warnings = picture.warnings.clone();
    if space.ppp.is_none() {
        warnings.push("the capture's size in points is unknown: px only".into());
    }
    let ppp = space.ppp.unwrap_or(1.0);
    let clamp_px = |v: f64, max: u32| (v.floor().max(0.0) as u32).min(max - 1);
    let mut out = Measurement {
        frame: frame_n,
        source: picture.source,
        px_per_pt: space.ppp.map(|p| (p * 100.0).round() / 100.0),
        image: [w, h],
        component: None,
        to_edges: None,
        boxes: None,
        gaps: None,
        warnings: Vec::new(),
    };
    match *query {
        Query::At(fx, fy) => {
            let (x, y) = to_px(&picture, crop, fx, fy);
            let (x, y) = (clamp_px(x, w), clamp_px(y, h));
            let radius = (2.0 * ppp).round().clamp(3.0, 8.0) as u32;
            let cap = ((f64::from(w) * f64::from(h) * FILL_CAP) as usize).max(1);
            let min_side = (2.0 * MIN_SIDE_PT * ppp).round() as u32;
            let (fill, stepped) = component_at(img, x, y, radius, min_side, cap);
            if stepped {
                warnings.push(
                    "the point was on text or a thin line: this is the surface around it".into(),
                );
            }
            if is_mark(img, &fill, min_side) {
                warnings.push(
                    "the box is a glyph or a thin line, not a component: put the point on the \
                     component's fill"
                        .into(),
                );
            }
            if fill.capped {
                warnings.push(
                    "the fill stopped at half the image: the point is likely on the page \
                     background, and the box is only part of it"
                        .into(),
                );
            } else if fill.touches_border {
                warnings.push("the box touches the image edge: it may run past it".into());
            }
            let bbox = blurred_box(img, &fill);
            let rim = RIM_PX.max(ppp.ceil() as u32);
            let edges = edges_around(img, bbox, fill.seed, fill.color, rim, FILL_TOLERANCE);
            let sides = [edges.left, edges.right, edges.up, edges.down];
            out.component = Some(measured(bbox, space, None));
            out.to_edges = Some(EdgeReport {
                px: sides.map(|s| s.px),
                pt: space.ppp.map(|p| sides.map(|s| half(f64::from(s.px) / p))),
                image_edge: ["left", "right", "up", "down"]
                    .into_iter()
                    .zip(sides)
                    .filter(|(_, s)| s.image_edge)
                    .map(|(n, _)| n)
                    .collect(),
            });
        }
        Query::Region(r) => {
            let (x0, y0) = to_px(&picture, crop, r[0], r[1]);
            let (x1, y1) = to_px(&picture, crop, r[2], r[3]);
            let (x0, y0) = (clamp_px(x0, w), clamp_px(y0, h));
            let (x1, y1) = (
                (x1.ceil() as u32).clamp(x0 + 1, w),
                (y1.ceil() as u32).clamp(y0 + 1, h),
            );
            let region = PxBox {
                x: x0,
                y: y0,
                width: x1 - x0,
                height: y1 - y0,
            };
            let min_side = (MIN_SIDE_PT * ppp).round().max(1.0) as u32;
            let join = (JOIN_PT * ppp).round().max(1.0) as u32;
            let Parts {
                kept: boxes,
                all,
                small,
            } = boxes_in(img, region, MASK_TOLERANCE, join, min_side, MAX_BOXES);
            if boxes.is_empty() && small > 0 {
                warnings.push(format!(
                    "no boxes found: the region holds only parts under {MIN_SIDE_PT} pt on a \
                     side, such as a thin line or small text"
                ));
            } else if boxes.is_empty() {
                warnings.push("no boxes found: the region looks like one plain color".into());
            }
            if all.len() > boxes.len() {
                warnings.push(format!(
                    "{} smaller boxes left out (the {MAX_BOXES} largest are shown): draw a \
                     tighter region",
                    all.len() - boxes.len()
                ));
            }
            let gaps = kept_gaps(&all, &boxes);
            out.gaps = Some(
                gaps.iter()
                    .map(|g| GapReport {
                        from: g.a + 1,
                        to: g.b + 1,
                        axis: g.axis,
                        px: g.px,
                        pt: space.ppp.map(|p| half(f64::from(g.px) / p)),
                    })
                    .collect(),
            );
            out.boxes = Some(
                boxes
                    .iter()
                    .enumerate()
                    .map(|(i, b)| measured(*b, space, Some(i + 1)))
                    .collect(),
            );
        }
    }
    out.warnings = warnings;
    out
}

/// Measures keyframe `frame_n` (1-based) of a bundle. `Err` with exit code 2 text when the
/// frame is out of range.
pub fn measure(
    dir: &Path,
    bundle: &ScrawlBundle,
    frame_n: usize,
    query: &Query,
    decode: Decode,
) -> Result<std::result::Result<Measurement, String>> {
    let Some(k) = frame_n
        .checked_sub(1)
        .filter(|&k| k < bundle.keyframes.len())
    else {
        return Ok(Err(crate::appshot::frame_out_of_range(
            frame_n,
            bundle.keyframes.len(),
        )));
    };
    let picture = load_picture(dir, bundle, k, decode)?;
    if picture.frame.width == 0 || picture.frame.height == 0 {
        bail!("Keyframe {frame_n} decoded to an empty image");
    }
    let crop = crate::zoom::crop_of(bundle, k).unwrap_or(FULL);
    Ok(Ok(measure_picture(
        bundle.keyframes[k].index,
        picture,
        crop,
        width_pt(bundle),
        query,
    )))
}

fn num(v: f64) -> String {
    if (v - v.round()).abs() < 1e-9 {
        format!("{}", v.round() as i64)
    } else {
        let s = format!("{v:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    }
}

fn size(m: &Measured) -> String {
    match &m.pt {
        Some(pt) => format!(
            "{}×{} pt ({}×{} px) at {},{} pt",
            num(pt.width),
            num(pt.height),
            m.px.width,
            m.px.height,
            num(pt.x),
            num(pt.y)
        ),
        None => format!(
            "{}×{} px at {},{} px",
            m.px.width, m.px.height, m.px.x, m.px.y
        ),
    }
}

pub fn render_text(m: &Measurement) -> String {
    let mut out = format!(
        "frame {} · {} · from {}\n",
        m.frame,
        m.px_per_pt.map_or("pt unavailable".to_owned(), |p| format!(
            "{} px per pt",
            num(p)
        )),
        m.source.says()
    );
    if let (Some(b), Some(e)) = (&m.component, &m.to_edges) {
        out.push_str(&format!("  box {}\n", size(b)));
        let names = ["left", "right", "up", "down"];
        let unit = if e.pt.is_some() { "pt" } else { "px" };
        let parts: Vec<String> = (0..4)
            .map(|i| {
                let v = e.pt.map_or(f64::from(e.px[i]), |p| p[i]);
                let edge = if e.image_edge.contains(&names[i]) {
                    " (image edge)"
                } else {
                    ""
                };
                format!("{} {}{edge}", names[i], num(v))
            })
            .collect();
        out.push_str(&format!("  to edges: {} {unit}\n", parts.join(" · ")));
    }
    if let Some(boxes) = &m.boxes {
        for b in boxes {
            out.push_str(&format!("  box {}  {}\n", b.n.unwrap_or(0), size(b)));
        }
    }
    if let Some(gaps) = &m.gaps {
        for g in gaps {
            let value =
                g.pt.map_or(format!("{} px", g.px), |p| format!("{} pt", num(p)));
            let axis = match g.axis {
                Axis::Horizontal => "horizontal",
                Axis::Vertical => "vertical",
            };
            out.push_str(&format!("  gap {}→{}  {value} ({axis})\n", g.from, g.to));
        }
    }
    for w in &m.warnings {
        out.push_str(&format!("  note: {w}\n"));
    }
    out
}

/// `scrawl measure`: what to print, and the exit code.
pub fn report(
    dir: &Path,
    bundle: &ScrawlBundle,
    frame_n: usize,
    query: &Query,
    json: bool,
) -> Result<(String, i32)> {
    Ok(match measure(dir, bundle, frame_n, query, decode_movie)? {
        Err(text) => (text, 2),
        Ok(m) if json => (format!("{}\n", serde_json::to_string_pretty(&m)?), 0),
        Ok(m) => (render_text(&m), 0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AutocropInfo, DeviceInfo, Keyframe, KeyframeCrop, KeyframeReason};

    const BG: [u8; 3] = [0xF2, 0xF2, 0xF2];
    const CARD: [u8; 3] = [0xFF, 0xFF, 0xFF];
    const TEXT: [u8; 3] = [0x33, 0x33, 0x33];

    fn canvas(w: u32, h: u32, c: [u8; 3]) -> RgbaFrame {
        let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
        for _ in 0..w * h {
            rgba.extend_from_slice(&[c[0], c[1], c[2], 255]);
        }
        RgbaFrame::new(0, w, h, rgba)
    }

    fn rect(f: &mut RgbaFrame, x: u32, y: u32, w: u32, h: u32, c: [u8; 3]) {
        for py in y..y + h {
            for px in x..x + w {
                let i = (py * f.width + px) as usize * 4;
                f.rgba[i..i + 3].copy_from_slice(&c);
            }
        }
    }

    /// Two 320×184 px cards 40 px apart at (192, 280) on #F2F2F2, lines of "text" in the first.
    fn cards(w: u32, h: u32) -> RgbaFrame {
        let mut f = canvas(w, h, BG);
        rect(&mut f, 192, 280, 320, 184, CARD);
        rect(&mut f, 552, 280, 320, 184, CARD);
        for line in 0..4 {
            rect(&mut f, 216, 304 + line * 24, 200 + line * 10, 12, TEXT);
        }
        f
    }

    fn close(a: u32, b: u32) -> bool {
        a.abs_diff(b) <= 1
    }

    fn assert_box(b: PxBox, x: u32, y: u32, w: u32, h: u32) {
        assert!(
            close(b.x, x) && close(b.y, y) && close(b.width, w) && close(b.height, h),
            "{b:?} is not {x},{y} {w}×{h}"
        );
    }

    /// The gaps when every box is shown.
    fn nearest_gaps(boxes: &[PxBox]) -> Vec<Gap> {
        kept_gaps(boxes, boxes)
    }

    /// The edges of a fill's box as point mode reads them at 2 px per pt.
    fn edges(f: &RgbaFrame, fill: &Fill) -> Edges {
        let bbox = blurred_box(f, fill);
        edges_around(f, bbox, fill.seed, fill.color, RIM_PX, FILL_TOLERANCE)
    }

    fn picture(frame: RgbaFrame, source: Source, rect: Rect) -> Picture {
        Picture {
            frame,
            source,
            rect,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn a_point_in_a_card_measures_the_card_even_off_or_next_to_text() {
        let f = cards(1200, 800);
        // The centre, 3 px off it, just under a line of text, just left of the text and 2 px
        // past the end of a line (its window still holds some text).
        for (x, y) in [(480, 440), (483, 437), (300, 316), (215, 300), (428, 330)] {
            let fill = fill_at(&f, x, y, 4, FILL_TOLERANCE, 480_000);
            assert_box(fill.bbox, 192, 280, 320, 184);
            assert!(!fill.capped && !fill.touches_border);
        }
        let fill = fill_at(&f, 480, 440, 4, FILL_TOLERANCE, 480_000);
        let e = edges(&f, &fill);
        // Left to the image edge, right across the 40 px gap to the next card.
        assert!(close(e.left.px, 192) && e.left.image_edge);
        assert!(close(e.right.px, 40) && !e.right.image_edge);
        assert!(close(e.up.px, 280) && e.up.image_edge);
    }

    /// A point on a button's label, even right on a glyph or a stray pixel, measures the button.
    #[test]
    fn a_point_on_a_label_measures_the_button_around_it() {
        const BLUE: [u8; 3] = [0x25, 0x63, 0xEB];
        const WHITE: [u8; 3] = [0xFF, 0xFF, 0xFF];
        let mut f = canvas(800, 400, WHITE);
        rect(&mut f, 300, 180, 188, 44, BLUE);
        // "Glyphs": 9×14 px rings with 2 px strokes, 3 px apart along the middle of the button.
        for i in 0..10 {
            let x = 330 + i * 12;
            rect(&mut f, x, 195, 9, 14, WHITE);
            rect(&mut f, x + 2, 197, 5, 10, BLUE);
        }
        // One stray pixel of another color, as a video frame has in antialiased text.
        rect(&mut f, 400, 216, 1, 1, [0x90, 0xB0, 0xF0]);
        let cap = 800 * 400 / 2;
        // 24 px: 12 pt at 2 px per pt.
        // On a glyph's stroke (radius 1 seeds on the glyph), and on the stray pixel (radius 0
        // seeds on it, as noisy video can when every color near the point differs).
        for (x, y, radius) in [(330, 202, 1), (400, 216, 0)] {
            let (fill, stepped) = component_at(&f, x, y, radius, 24, cap);
            assert!(stepped, "{x},{y}");
            assert_eq!(fill.color, BLUE);
            assert_box(fill.bbox, 300, 180, 188, 44);
        }
        // A point on the fill measures the button without stepping.
        let (fill, stepped) = component_at(&f, 310, 185, 3, 24, cap);
        assert!(!stepped);
        assert_box(fill.bbox, 300, 180, 188, 44);
        // A small solid part (a 14 px icon, 7 pt) is a component, not a mark.
        rect(&mut f, 600, 100, 14, 14, BLUE);
        let (fill, stepped) = component_at(&f, 605, 105, 1, 24, cap);
        assert!(!stepped);
        assert_box(fill.bbox, 600, 100, 14, 14);
        // A solid 3×5 px piece of a dark letter on the button is a mark.
        rect(&mut f, 460, 200, 3, 5, [0x10, 0x20, 0x40]);
        let (fill, stepped) = component_at(&f, 461, 202, 1, 24, cap);
        assert!(stepped);
        assert_box(fill.bbox, 300, 180, 188, 44);
    }

    /// A card with a 2 px border reads its distance to the next card, not 1 px to its own border.
    #[test]
    fn a_bordered_card_reads_the_gap_past_its_border() {
        const BORDER: [u8; 3] = [0xD0, 0xD0, 0xD0];
        let mut f = canvas(800, 400, BG);
        for x in [100, 330] {
            rect(&mut f, x, 100, 200, 100, BORDER);
            rect(&mut f, x + 2, 102, 196, 96, CARD);
        }
        let fill = fill_at(&f, 200, 150, 4, FILL_TOLERANCE, 800 * 400 / 2);
        assert_box(fill.bbox, 102, 102, 196, 96);
        let e = edges(&f, &fill);
        // 2 px of border, then the 30 px gap to the next card's border.
        assert!(close(e.right.px, 32), "{:?}", e.right);
        assert!(!e.right.image_edge);
    }

    /// A line between two rows of one color is a divider: the reach is its width, not the next
    /// row. A line all round a card is its border: the reach goes on to the next card.
    #[test]
    fn a_divider_reads_its_width_and_a_border_reads_the_gap_past_it() {
        const LINE: [u8; 3] = [0xE0, 0xE0, 0xE0];
        let cap = 800 * 600 / 2;
        // Three white rows 88 px tall, split by 2 px lines, on the page.
        let mut f = canvas(800, 600, BG);
        rect(&mut f, 100, 100, 600, 268, CARD);
        for y in [188, 278] {
            rect(&mut f, 100, y, 600, 2, LINE);
        }
        let fill = fill_at(&f, 400, 230, 4, FILL_TOLERANCE, cap);
        assert_box(fill.bbox, 100, 190, 600, 88);
        let e = edges(&f, &fill);
        assert_eq!((e.up.px, e.down.px), (2, 2), "{e:?}");
        assert!(e.left.px == 100 && e.left.image_edge);
        // A cell of a table: lines on all four sides that go on past the cell.
        let mut f = canvas(800, 600, CARD);
        for x in [100, 302, 504, 706] {
            rect(&mut f, x, 100, 2, 400, LINE);
        }
        for y in [100, 232, 364, 498] {
            rect(&mut f, 100, y, 608, 2, LINE);
        }
        let fill = fill_at(&f, 400, 300, 4, FILL_TOLERANCE, cap);
        assert_box(fill.bbox, 304, 234, 200, 130);
        let e = edges(&f, &fill);
        assert_eq!([e.left.px, e.right.px, e.up.px, e.down.px], [2; 4]);
        // Two outlined cards on a page of their own color: 2 px of border, then the 30 px gap.
        let mut f = canvas(800, 400, CARD);
        for x in [100, 330] {
            rect(&mut f, x, 100, 200, 100, LINE);
            rect(&mut f, x + 2, 102, 196, 96, CARD);
        }
        let fill = fill_at(&f, 200, 150, 4, FILL_TOLERANCE, cap);
        assert_box(fill.bbox, 102, 102, 196, 96);
        assert_eq!(edges(&f, &fill).right.px, 32);
        // A 3 px border (1 pt at 3 px per pt) before a 30 px gap.
        let mut f = canvas(900, 400, BG);
        for x in [100, 430] {
            rect(&mut f, x, 100, 300, 150, LINE);
            rect(&mut f, x + 3, 103, 294, 144, CARD);
        }
        let fill = fill_at(&f, 250, 175, 6, FILL_TOLERANCE, cap);
        let bbox = blurred_box(&f, &fill);
        let e = edges_around(&f, bbox, fill.seed, fill.color, 3, FILL_TOLERANCE);
        assert_eq!(e.right.px, 33);
    }

    /// Two boxes 2 px apart read 2 px, of one color or of two, not the box behind the gap.
    #[test]
    fn a_narrow_gap_between_two_boxes_reads_the_gap() {
        for neighbor in [CARD, [0xC8, 0xD2, 0xFF]] {
            let mut f = canvas(800, 400, BG);
            rect(&mut f, 100, 100, 200, 100, CARD);
            rect(&mut f, 302, 100, 200, 100, neighbor);
            let fill = fill_at(&f, 200, 150, 4, FILL_TOLERANCE, 800 * 400 / 2);
            assert_box(fill.bbox, 100, 100, 200, 100);
            assert_eq!(blurred_box(&f, &fill), fill.bbox);
            let e = edges(&f, &fill);
            assert_eq!(e.right.px, 2, "{neighbor:?}");
            assert!(e.left.px == 100 && e.left.image_edge);
        }
    }

    /// An antialiased dot `d` px wide with its corner at (x, y): a 4× supersampled circle.
    fn dot(f: &mut RgbaFrame, x: u32, y: u32, d: u32, c: [u8; 3]) {
        let r = f64::from(d) / 2.0;
        for py in 0..d {
            for px in 0..d {
                let inside = (0..16)
                    .filter(|i| {
                        let dx = f64::from(px) + (f64::from(i % 4) + 0.5) / 4.0 - r;
                        let dy = f64::from(py) + (f64::from(i / 4) + 0.5) / 4.0 - r;
                        dx * dx + dy * dy <= r * r
                    })
                    .count() as f64
                    / 16.0;
                let under = rgb(f, x + px, y + py);
                let mix = [0, 1, 2].map(|i| {
                    (f64::from(c[i]) * inside + f64::from(under[i]) * (1.0 - inside)).round() as u8
                });
                rect(f, x + px, y + py, 1, 1, mix);
            }
        }
    }

    /// The hole of a letter is no component: the point measures the button, with a note. A
    /// status dot of the same size is one: the surroundings tell them apart.
    #[test]
    fn a_point_in_the_hole_of_a_letter_measures_the_button_and_a_dot_measures_itself() {
        const BLUE: [u8; 3] = [0x25, 0x63, 0xEB];
        const WHITE: [u8; 3] = [0xFF, 0xFF, 0xFF];
        const GREEN: [u8; 3] = [0x34, 0xC7, 0x59];
        let mut f = canvas(800, 400, WHITE);
        rect(&mut f, 300, 180, 188, 44, BLUE);
        // An "o" (17×22 px, a 9×14 px hole) and a "D" (20×27 px, a 12×19 px hole), 4 px strokes.
        rect(&mut f, 340, 192, 17, 22, WHITE);
        rect(&mut f, 344, 196, 9, 14, BLUE);
        rect(&mut f, 380, 188, 20, 27, WHITE);
        rect(&mut f, 384, 192, 12, 19, BLUE);
        let cap = 800 * 400 / 2;
        // 2 px per pt: a 4 px window, 24 px for 12 pt.
        for (x, y) in [(348, 203), (390, 201)] {
            let (fill, stepped) = component_at(&f, x, y, 4, 24, cap);
            assert!(stepped, "{x},{y}");
            assert_eq!(fill.color, BLUE);
            assert_box(fill.bbox, 300, 180, 188, 44);
        }
        // Dots of 3 to 6 pt on the button and on the page, at whole and at half pixels: each
        // measures itself to the pixel, with no note. The 6 px ones are not the main color
        // near the point.
        for (x, y, d, on) in [
            (420, 190, 6, BLUE),
            (440, 196, 8, BLUE),
            (100, 100, 6, WHITE),
            (130, 100, 10, WHITE),
            (160, 100, 12, WHITE),
        ] {
            let mut f = f.clone();
            dot(&mut f, x, y, d, GREEN);
            let (fill, stepped) = component_at(&f, x + d / 2, y + d / 2, 4, 24, cap);
            assert!(!stepped, "{d} px on {on:?}");
            assert_eq!(fill.color, GREEN);
            let b = PxBox {
                x,
                y,
                width: d,
                height: d,
            };
            assert_eq!(blurred_box(&f, &fill), b, "{d} px on {on:?}");
            assert!(!is_mark(&f, &fill, 24));
        }
        // A point on the button next to a dot still measures the button.
        dot(&mut f, 420, 190, 6, GREEN);
        let (fill, stepped) = component_at(&f, 430, 205, 4, 24, cap);
        assert!(!stepped);
        assert_box(fill.bbox, 300, 180, 188, 44);
    }

    /// What H.264 4:2:0 does to a frame: each 2×2 block of pixels shares one hue (BT.601).
    fn chroma_420(f: &RgbaFrame) -> RgbaFrame {
        let ycc = |c: [u8; 3]| {
            let [r, g, b] = c.map(f64::from);
            let y = 0.299 * r + 0.587 * g + 0.114 * b;
            (y, (b - y) * 0.564, (r - y) * 0.713)
        };
        let mut out = canvas(f.width, f.height, [0, 0, 0]);
        for by in (0..f.height).step_by(2) {
            for bx in (0..f.width).step_by(2) {
                let block: Vec<(u32, u32)> =
                    [(bx, by), (bx + 1, by), (bx, by + 1), (bx + 1, by + 1)]
                        .into_iter()
                        .filter(|&(x, y)| x < f.width && y < f.height)
                        .collect();
                let n = block.len() as f64;
                let cb = block.iter().map(|&(x, y)| ycc(rgb(f, x, y)).1).sum::<f64>() / n;
                let cr = block.iter().map(|&(x, y)| ycc(rgb(f, x, y)).2).sum::<f64>() / n;
                for (x, y) in block {
                    let luma = ycc(rgb(f, x, y)).0;
                    let c = [
                        luma + 1.403 * cr,
                        luma - 0.344 * cb - 0.714 * cr,
                        luma + 1.773 * cb,
                    ]
                    .map(|v| v.round().clamp(0.0, 255.0) as u8);
                    rect(&mut out, x, y, 1, 1, c);
                }
            }
        }
        out
    }

    /// A movie blurs the hue of an edge that sits on an odd pixel. Both modes still put the
    /// edge on the pixel, so a point and a region give one size and the gap is exact.
    #[test]
    fn a_point_and_a_region_agree_on_a_movie_frame_with_blurred_edges() {
        let mut f = canvas(1200, 800, [0xFF, 0xFF, 0xFF]);
        rect(&mut f, 401, 301, 199, 77, [37, 99, 235]);
        rect(&mut f, 643, 301, 121, 77, [220, 38, 38]);
        let f = chroma_420(&f);
        // The edge pixels are neither color now.
        assert!(!near(rgb(&f, 401, 340), rgb(&f, 420, 340), FILL_TOLERANCE));
        assert!(!near(rgb(&f, 400, 340), rgb(&f, 390, 340), MASK_TOLERANCE));
        let boxes = [(401, 301, 199, 77), (643, 301, 121, 77)].map(|(x, y, width, height)| PxBox {
            x,
            y,
            width,
            height,
        });
        let cap = 1200 * 800 / 2;
        for (b, x) in boxes.iter().zip([500, 700]) {
            let (fill, stepped) = component_at(&f, x, 340, 4, 24, cap);
            assert!(!stepped);
            assert_eq!(blurred_box(&f, &fill), *b);
        }
        let (fill, _) = component_at(&f, 500, 340, 4, 24, cap);
        assert_eq!(edges(&f, &fill).right.px, 43);
        let region = PxBox {
            x: 300,
            y: 250,
            width: 600,
            height: 180,
        };
        let parts = boxes_in(&f, region, MASK_TOLERANCE, 3, 12, MAX_BOXES);
        assert_eq!(parts.kept, boxes);
        assert_eq!(nearest_gaps(&parts.kept)[0].px, 43);
    }

    /// A region that holds only a thin line says so, and does not call it one plain color.
    #[test]
    fn a_region_with_only_a_thin_line_says_so() {
        let mut f = canvas(1200, 800, BG);
        rect(&mut f, 200, 400, 400, 2, TEXT);
        let measure = |f: RgbaFrame| {
            measure_picture(
                1,
                picture(f, Source::Movie, FULL),
                FULL,
                Some(600.0),
                &Query::Region([0.1, 0.45, 0.7, 0.55]),
            )
        };
        let m = measure(f);
        assert_eq!(m.boxes, Some(Vec::new()));
        assert!(m.warnings[0].contains("only parts under 6 pt"), "{m:?}");
        let m = measure(canvas(1200, 800, BG));
        assert!(m.warnings[0].contains("one plain color"), "{m:?}");
    }

    /// A region full of small parts: every part is counted, a part inside another is not, and
    /// only the shown boxes look for a neighbor.
    #[test]
    fn a_region_full_of_small_parts_counts_them_and_gaps_only_the_shown_boxes() {
        // 60 rows of 60 squares, 8 px wide and 4 px apart.
        let mut f = canvas(760, 760, BG);
        for row in 0..60 {
            for column in 0..60 {
                rect(&mut f, 20 + column * 12, 20 + row * 12, 8, 8, TEXT);
            }
        }
        let region = PxBox {
            x: 0,
            y: 0,
            width: 760,
            height: 760,
        };
        let parts = boxes_in(&f, region, MASK_TOLERANCE, 3, 6, MAX_BOXES);
        assert_eq!(
            (parts.kept.len(), parts.all.len(), parts.small),
            (30, 3600, 0)
        );
        // The 30 shown are the first of the top row: 29 gaps of 4 px, and none to the rows
        // below, which were left out.
        let gaps = kept_gaps(&parts.all, &parts.kept);
        assert_eq!(gaps.len(), 29);
        assert!(
            gaps.iter()
                .all(|g| g.px == 4 && g.axis == Axis::Horizontal && g.b == g.a + 1)
        );
        // A part inside another part's box, or equal to one, is not top-level.
        let b = |x, y, width, height| PxBox {
            x,
            y,
            width,
            height,
        };
        let top = top_level(
            vec![
                b(210, 10, 20, 20),
                b(200, 0, 100, 100),
                b(400, 0, 50, 50),
                b(400, 0, 50, 50),
                b(290, 90, 20, 20),
            ],
            region,
        );
        assert_eq!(
            top,
            vec![b(200, 0, 100, 100), b(400, 0, 50, 50), b(290, 90, 20, 20)]
        );
    }

    /// When a region has more parts than are shown, no gap spans a part that was left out.
    #[test]
    fn gaps_never_span_a_box_that_was_left_out() {
        let mut f = canvas(900, 200, BG);
        // Two big boxes with a small one between them.
        rect(&mut f, 100, 50, 200, 100, TEXT);
        rect(&mut f, 340, 90, 20, 20, TEXT);
        rect(&mut f, 400, 50, 200, 100, TEXT);
        let region = PxBox {
            x: 50,
            y: 20,
            width: 800,
            height: 160,
        };
        let Parts { kept, all, .. } = boxes_in(&f, region, MASK_TOLERANCE, 3, 6, 2);
        assert_eq!((kept.len(), all.len()), (2, 3));
        assert!(nearest_gaps(&kept).iter().any(|g| g.px == 100));
        // The 100 px gap spans the small box, so it is not reported.
        assert!(kept_gaps(&all, &kept).iter().all(|g| g.px != 100));
    }

    #[test]
    fn a_point_on_the_background_stops_at_the_cap() {
        let f = cards(1200, 800);
        let fill = fill_at(&f, 20, 20, 4, FILL_TOLERANCE, 1200 * 800 / 2);
        assert!(fill.capped);
    }

    #[test]
    fn a_region_finds_both_cards_and_their_gap_not_the_text() {
        let f = cards(1200, 800);
        let region = PxBox {
            x: 100,
            y: 200,
            width: 900,
            height: 400,
        };
        let boxes = boxes_in(&f, region, MASK_TOLERANCE, 3, 12, MAX_BOXES).kept;
        assert_eq!(boxes.len(), 2, "{boxes:?}");
        assert_box(boxes[0], 192, 280, 320, 184);
        assert_box(boxes[1], 552, 280, 320, 184);
        let gaps = nearest_gaps(&boxes);
        assert_eq!(gaps.len(), 1);
        assert!(close(gaps[0].px, 40) && gaps[0].axis == Axis::Horizontal);
    }

    #[test]
    fn letters_a_few_px_apart_are_one_box_and_a_word_space_is_a_gap() {
        // Two "words" of 8×12 px letters: 2 px between letters, 10 px between the words.
        let mut f = canvas(200, 40, CARD);
        for x in [10, 20, 30, 50, 60] {
            rect(&mut f, x, 10, 8, 12, TEXT);
        }
        let region = PxBox {
            x: 0,
            y: 0,
            width: 200,
            height: 40,
        };
        let boxes = boxes_in(&f, region, MASK_TOLERANCE, 3, 6, MAX_BOXES).kept;
        assert_eq!(boxes.len(), 2, "{boxes:?}");
        assert_box(boxes[0], 10, 10, 28, 12);
        assert_box(boxes[1], 50, 10, 18, 12);
        assert_eq!(nearest_gaps(&boxes)[0].px, 12);
    }

    #[test]
    fn fractions_map_through_the_crop_onto_the_full_frame() {
        let crop: Rect = [0.25, 0.5, 0.75, 1.0];
        let full = picture(canvas(1000, 800, BG), Source::Movie, FULL);
        let (x, y) = to_px(&full, crop, 0.5, 0.5);
        assert!((x - 500.0).abs() < 1e-6 && (y - 600.0).abs() < 1e-6);
        // The cropped image takes the fractions as they are, and is placed in the full frame.
        let cropped = picture(canvas(500, 400, BG), Source::Keyframe, crop);
        let (x, y) = to_px(&cropped, crop, 0.5, 0.5);
        assert!((x - 250.0).abs() < 1e-6 && (y - 200.0).abs() < 1e-6);
        let s = space(&cropped, Some(500.0));
        assert_eq!(s.offset, (250.0, 400.0));
        assert_eq!(s.ppp, Some(2.0));
    }

    fn mac_bundle() -> ScrawlBundle {
        serde_json::from_value(serde_json::json!({
            "version": 1, "id": "m", "createdAtMs": 0, "durationMs": 5_000,
            "target": {"mode": "display", "displayId": 1,
                "frame": {"x": 0.0, "y": 0.0, "width": 600.0, "height": 400.0, "scale": 2.0}},
            "transcript": {"source": "test", "text": "", "segments": []},
            "strokes": [], "marks": [], "events": [],
            "keyframes": [{"index": 1, "t": 1000, "reason": "initial",
                "file": "frames/frame-01.png", "event": ""}],
            "contactSheet": "contact-sheet.png"
        }))
        .unwrap()
    }

    #[test]
    fn points_follow_the_mac_scale() {
        let bundle = mac_bundle();
        let m = measure_picture(
            1,
            picture(cards(1200, 800), Source::Movie, FULL),
            FULL,
            width_pt(&bundle),
            &Query::At(0.4, 0.55),
        );
        assert_eq!(m.px_per_pt, Some(2.0));
        let b = m.component.unwrap();
        assert_eq!(
            b.pt.unwrap(),
            Dims {
                x: 96.0,
                y: 140.0,
                width: 160.0,
                height: 92.0
            }
        );
        let text = render_text(&measure_picture(
            1,
            picture(cards(1200, 800), Source::Movie, FULL),
            FULL,
            width_pt(&bundle),
            &Query::Region([0.1, 0.25, 0.85, 0.75]),
        ));
        assert!(
            text.contains("box 1  160×92 pt (320×184 px) at 96,140 pt"),
            "{text}"
        );
        assert!(text.contains("gap 1→2  20 pt (horizontal)"), "{text}");
    }

    #[test]
    fn a_downscaled_phone_frame_uses_the_screen_in_points() {
        let mut bundle = mac_bundle();
        bundle.target.mode = CaptureMode::Device;
        bundle.target.frame.width = 1320.0;
        bundle.device = Some(DeviceInfo {
            width: 1320,
            height: 2868,
            screen: Some([402.0, 874.0, 3.0]),
            ..Default::default()
        });
        assert_eq!(width_pt(&bundle), Some(402.0));
        let mut f = canvas(1600, 1000, BG);
        rect(&mut f, 398, 200, 398, 199, CARD);
        let m = measure_picture(
            1,
            picture(f, Source::Keyframe, FULL),
            FULL,
            width_pt(&bundle),
            &Query::At(0.3, 0.3),
        );
        let ppp = 1600.0 / 402.0;
        assert_eq!(m.px_per_pt, Some((ppp * 100.0_f64).round() / 100.0));
        let pt = m.component.unwrap().pt.unwrap();
        assert_eq!((pt.width, pt.height, pt.x), (100.0, 50.0, 100.0));
        bundle.device.as_mut().unwrap().screen = None;
        assert_eq!(width_pt(&bundle), None);
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("scrawl-measure-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("frames")).unwrap();
        dir
    }

    /// A Mac bundle whose keyframe 1 is cropped to the right half (300×400 pt of 600×400).
    fn cropped_bundle(dir: &Path, with_full: bool) -> ScrawlBundle {
        let mut bundle = mac_bundle();
        bundle.keyframes.push(Keyframe {
            index: 2,
            t: 2000,
            reason: KeyframeReason::SceneChange,
            file: "frames/frame-02.png".into(),
            event: String::new(),
            said: None,
            mark_id: None,
            cursor: None,
            appshot: None,
        });
        bundle.autocrop = Some(AutocropInfo {
            label: "test".into(),
            full_frames: "frames-full".into(),
            full_contact_sheet: "contact-sheet-full.png".into(),
            crops: vec![KeyframeCrop {
                index: 1,
                x: 300.0,
                y: 0.0,
                width: 300.0,
                height: 400.0,
                kind: crate::autocrop::CropKind::Window,
                reason: String::new(),
                apps: Vec::new(),
            }],
            changes: Vec::new(),
        });
        // A 200×100 px card at (800, 300) in the full 1200×800 px frame.
        let mut full = canvas(1200, 800, BG);
        rect(&mut full, 800, 300, 200, 100, CARD);
        let cropped = crate::autocrop::crop_frame(&full, [0.5, 0.0, 1.0, 1.0]);
        crate::render::save_png(&cropped, &dir.join("frames/frame-01.png")).unwrap();
        if with_full {
            std::fs::create_dir_all(dir.join("frames-full")).unwrap();
            crate::render::save_png(&full, &dir.join("frames-full/frame-01.png")).unwrap();
        }
        std::fs::write(
            dir.join("scrawl.json"),
            serde_json::to_vec(&bundle).unwrap(),
        )
        .unwrap();
        bundle
    }

    #[test]
    fn without_a_movie_it_reads_the_full_frame_then_the_cropped_keyframe() {
        // The card's centre, (900, 350) px, is 0.5, 0.4375 of the cropped keyframe image.
        let at = Query::At(0.5, 0.4375);
        for with_full in [true, false] {
            let dir = temp_dir(if with_full { "full" } else { "cropped" });
            let bundle = cropped_bundle(&dir, with_full);
            let m = measure(&dir, &bundle, 1, &at, no_movie).unwrap().unwrap();
            let expected = if with_full {
                Source::FullFrame
            } else {
                Source::Keyframe
            };
            assert_eq!(m.source, expected);
            assert_eq!(m.px_per_pt, Some(2.0));
            let b = m.component.unwrap();
            assert_eq!(
                b.pt.unwrap(),
                Dims {
                    x: 400.0,
                    y: 150.0,
                    width: 100.0,
                    height: 50.0
                },
                "{with_full}"
            );
            assert!(m.warnings.iter().any(|w| w.contains("ink")));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    fn no_movie(_: &Path, _: &[u64], _: u64) -> Result<Vec<RgbaFrame>> {
        bail!("no movie in this test")
    }

    /// The full 1200×800 px frame of `cropped_bundle`, ink-free, as the movie gives it.
    fn movie_frame(_: &Path, times: &[u64], _: u64) -> Result<Vec<RgbaFrame>> {
        assert_eq!(times, &[1000]);
        let mut full = canvas(1200, 800, BG);
        rect(&mut full, 800, 300, 200, 100, CARD);
        rect(&mut full, 1040, 300, 120, 100, CARD);
        Ok(vec![full])
    }

    #[test]
    fn with_a_movie_it_measures_the_ink_free_full_frame_through_the_crop() {
        let dir = temp_dir("movie");
        let mut bundle = cropped_bundle(&dir, true);
        std::fs::write(dir.join("recording.mov"), b"stub").unwrap();
        bundle.video = Some("recording.mov".into());
        // The card's centre, (900, 350) px, is 0.5, 0.4375 of the cropped keyframe image.
        let m = measure(&dir, &bundle, 1, &Query::At(0.5, 0.4375), movie_frame)
            .unwrap()
            .unwrap();
        assert_eq!(m.source, Source::Movie);
        assert_eq!(m.image, [1200, 800]);
        assert!(m.warnings.is_empty(), "{:?}", m.warnings);
        let b = m.component.unwrap();
        assert_eq!(
            b.px,
            PxDims {
                x: 800,
                y: 300,
                width: 200,
                height: 100
            }
        );
        assert_eq!(
            b.pt.unwrap(),
            Dims {
                x: 400.0,
                y: 150.0,
                width: 100.0,
                height: 50.0
            }
        );
        let e = m.to_edges.unwrap();
        assert!(close(e.px[1], 40), "{e:?}");
        // The region is the keyframe image's lower-middle band: both cards, 40 px (20 pt) apart.
        let m = measure(
            &dir,
            &bundle,
            1,
            &Query::Region([0.2, 0.3, 1.0, 0.6]),
            movie_frame,
        )
        .unwrap()
        .unwrap();
        let boxes = m.boxes.unwrap();
        assert_eq!(boxes.len(), 2, "{boxes:?}");
        assert_eq!(
            boxes[0].px,
            PxDims {
                x: 800,
                y: 300,
                width: 200,
                height: 100
            }
        );
        assert_eq!(
            boxes[1].px,
            PxDims {
                x: 1040,
                y: 300,
                width: 120,
                height: 100
            }
        );
        let gaps = m.gaps.unwrap();
        assert_eq!((gaps.len(), gaps[0].px, gaps[0].pt), (1, 40, Some(20.0)));
        // A movie that fails falls back to the saved full frame and says so.
        let m = measure(&dir, &bundle, 1, &Query::At(0.5, 0.4375), no_movie)
            .unwrap()
            .unwrap();
        assert_eq!(m.source, Source::FullFrame);
        assert!(m.warnings.iter().any(|w| w.contains("could not be read")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_frame_out_of_range_names_the_range_and_exits_2() {
        let dir = temp_dir("range");
        let bundle = cropped_bundle(&dir, false);
        let (text, code) = report(&dir, &bundle, 3, &Query::At(0.5, 0.5), false).unwrap();
        assert_eq!(code, 2);
        assert_eq!(
            text,
            "--frame 3 is out of range: this capture has keyframes 1 to 2\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn boxes_a_px_apart_in_height_read_as_one_line() {
        let b = |x, y, height| PxBox {
            x,
            y,
            width: 10,
            height,
        };
        // A word one px lower, left of a taller one, and a second line below.
        let order = reading_order(vec![b(300, 10, 20), b(100, 11, 18), b(50, 60, 20)]);
        assert_eq!(order, vec![b(100, 11, 18), b(300, 10, 20), b(50, 60, 20)]);
    }

    #[test]
    fn flags_take_fractions() {
        assert!(
            matches!(parse_point("0.5,0.25").unwrap(), Query::At(x, y) if x == 0.5 && y == 0.25)
        );
        assert!(parse_point("50,20").is_err());
        assert!(parse_region("0.5,0.5,0.4,0.9").is_err());
        assert!(parse_region("0.1,0.1,0.9,0.9").is_ok());
    }
}

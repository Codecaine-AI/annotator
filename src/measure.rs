//! `scrawl measure`: exact sizes and gaps of what a keyframe shows, so an agent reads them
//! instead of estimating them from the image (a screen ruler the agent points).
//!
//! The agent gives a point or a region as fractions of the keyframe image it sees
//! (`frames/frame-NN.png`, auto-cropped). They are mapped through the keyframe's crop onto the
//! full, ink-free frame decoded from the movie, else the bundle's uncropped `frames-full/`
//! image, else the cropped image itself. The measuring below works on any RGBA frame.
//!
//! A region gives every box in it and the gaps between them. A point gives the box that holds
//! it, found the way a region finds a box (the pixels off one background color, joined), so
//! the two agree. A point gives no gaps: a movie's noise next to an edge made every ray from
//! a box unsure, and a region measures the same gap from both boxes.

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
/// A fill's box takes in at most this many blurred edge pixels on each side.
const BLUR_PX: u32 = 2;
/// A pixel this far per channel from a color is clearly not that color, whatever a movie did
/// to it.
const CLEAR: u8 = 24;

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

/// What a flood fill from one pixel covers.
#[derive(Clone, Debug, PartialEq)]
struct Fill {
    bbox: PxBox,
    color: [u8; 3],
    /// The fill hit its cap, so `bbox` is only part of it.
    capped: bool,
    /// The fill's pixels on the left, right, top and bottom side of `bbox`.
    sides: [Vec<(u32, u32)>; 4],
    /// The fill's pixel nearest the seed, within `reach` px of it, with the fill `room` px all
    /// round it (else half of that, else 1 px): a place on the surface that no stroke of a
    /// label is next to.
    open: Option<(u32, u32)>,
    /// The fill's most common color (of every pixel up to 4,096, then of every 31st).
    main: Option<[u8; 3]>,
}

/// Flood fill (4-connected) from `seed` over pixels within `tolerance` of `color`, stopping
/// after `cap` pixels.
fn flood(
    frame: &RgbaFrame,
    seed: (u32, u32),
    color: [u8; 3],
    tolerance: u8,
    cap: usize,
    (reach, room): (u32, u32),
) -> Fill {
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
    let mut samples: Vec<[u8; 3]> = Vec::new();
    while let Some((px, py)) = stack.pop() {
        pixels += 1;
        if pixels <= 4096 || pixels % 31 == 0 {
            samples.push(rgb(frame, px, py));
        }
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
    let near_seed = window(
        frame,
        PxBox {
            x: seed.0,
            y: seed.1,
            width: 1,
            height: 1,
        },
        reach,
    );
    let filled = |x: u32, y: u32| x < w && y < h && visited[y as usize * w as usize + x as usize];
    let mut rooms = vec![room.max(1), (room / 2).max(1), 1];
    rooms.dedup();
    let open = rooms.into_iter().find_map(|r| {
        (near_seed.y..near_seed.y1())
            .flat_map(|y| (near_seed.x..near_seed.x1()).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                x >= r
                    && y >= r
                    && (y - r..=y + r).all(|py| (x - r..=x + r).all(|px| filled(px, py)))
            })
            .min_by_key(|&(x, y)| {
                (
                    u64::from(x.abs_diff(seed.0)).pow(2) + u64::from(y.abs_diff(seed.1)).pow(2),
                    y,
                    x,
                )
            })
    });
    Fill {
        bbox: PxBox {
            x: bx0,
            y: by0,
            width: bx1 - bx0 + 1,
            height: by1 - by0 + 1,
        },
        color,
        capped,
        sides,
        open,
        main: commonest(&samples).map(|(c, _)| c),
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

/// A fill's box with its blurred edges, for a fill that no one color closes (`Found::Lines`,
/// `Found::Mixed`). Antialiasing and H.264 leave a pixel or two of blend at an edge, which the
/// fill stops short of. Each side takes in up to `BLUR_PX` px while most of the pixels next to
/// the fill's own edge pixels lean to the fill's color rather than to the color 2 px further
/// out, so the edge sits midway between the two colors. A pixel of the same shade as the one
/// after it is no blend: it starts a line or a gap.
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

/// Where the color of a small box `b` starts again when the box is a hole in a mark, such as
/// the counter of an "o" or the space shut inside two letters: within half of `min_side` px
/// past the box on at least three sides, as one region that goes round it. Size alone cannot
/// tell a status dot from the counter of an "o".
fn hole_exit(
    frame: &RgbaFrame,
    b: PxBox,
    color: [u8; 3],
    min_side: u32,
) -> Option<((u32, u32), [u8; 3])> {
    let own = |x: u32, y: u32| near(rgb(frame, x, y), color, FILL_TOLERANCE);
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
        let mask = fill_within(frame, window, from, color, FILL_TOLERANCE);
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

/// How far two colors are apart: the largest difference of one channel.
fn contrast(a: [u8; 3], b: [u8; 3]) -> u8 {
    (0..3).map(|i| a[i].abs_diff(b[i])).max().unwrap_or(0)
}

fn mean(colors: impl Iterator<Item = [u8; 3]>) -> Option<[u8; 3]> {
    let (mut sum, mut n) = ([0u64; 3], 0u64);
    for c in colors {
        for (s, v) in sum.iter_mut().zip(c) {
            *s += u64::from(v);
        }
        n += 1;
    }
    (n > 0).then(|| sum.map(|s| ((s + n / 2) / n) as u8))
}

/// A color's shade, 4 levels to a step.
fn shade(c: [u8; 3]) -> u32 {
    (u32::from(c[0] >> 2) << 12) | (u32::from(c[1] >> 2) << 6) | u32::from(c[2] >> 2)
}

/// The most common color of `colors` and its shade: the mean of the colors within 6 per channel
/// of the first color of the most common shade, so a tolerance round it sits on the middle of a
/// color that a movie made noisy.
fn commonest(colors: &[[u8; 3]]) -> Option<([u8; 3], u32)> {
    let mut shades: std::collections::HashMap<u32, (usize, usize)> =
        std::collections::HashMap::new();
    for (i, c) in colors.iter().enumerate() {
        shades.entry(shade(*c)).or_insert((0, i)).0 += 1;
    }
    // Of equal counts, the shade seen first.
    let (&key, &(_, first)) = shades
        .iter()
        .max_by_key(|(_, (n, first))| (*n, std::cmp::Reverse(*first)))?;
    let mut color = colors[first];
    for _ in 0..2 {
        if let Some(c) = mean(colors.iter().copied().filter(|c| near(*c, color, 6))) {
            color = c;
        }
    }
    Some((color, key))
}

/// The main colors round `r`, most common first (at most three), each with how many of the
/// border's pixels it holds, and the border's length. A side of `r` on the image edge is left
/// out: its pixels are inside what `r` holds, not round it.
fn border_colors(frame: &RgbaFrame, r: PxBox) -> (Vec<([u8; 3], usize)>, usize) {
    let mut left: Vec<[u8; 3]> = Vec::with_capacity(2 * (r.width + r.height) as usize);
    if r.y > 0 {
        left.extend((r.x..r.x1()).map(|x| rgb(frame, x, r.y)));
    }
    if r.y1() < frame.height {
        left.extend((r.x..r.x1()).map(|x| rgb(frame, x, r.y1() - 1)));
    }
    if r.x > 0 {
        left.extend((r.y..r.y1()).map(|y| rgb(frame, r.x, y)));
    }
    if r.x1() < frame.width {
        left.extend((r.y..r.y1()).map(|y| rgb(frame, r.x1() - 1, y)));
    }
    let total = left.len();
    let mut colors = Vec::new();
    while colors.len() < 3 {
        let Some((color, key)) = commonest(&left) else {
            break;
        };
        let before = left.len();
        left.retain(|c| !near(*c, color, MASK_TOLERANCE) && shade(*c) != key);
        colors.push((color, before - left.len()));
    }
    (colors, total)
}

/// The region's background: the most common color on its border.
fn background(frame: &RgbaFrame, r: PxBox) -> [u8; 3] {
    let mut counts: std::collections::HashMap<u32, (usize, [u8; 3])> =
        std::collections::HashMap::new();
    let mut add = |x: u32, y: u32| {
        let c = rgb(frame, x, y);
        counts.entry(shade(c)).or_insert((0, c)).0 += 1;
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

/// `b` with `margin` px more on each side, inside the frame.
fn window(frame: &RgbaFrame, b: PxBox, margin: u32) -> PxBox {
    let (x, y) = (b.x.saturating_sub(margin), b.y.saturating_sub(margin));
    PxBox {
        x,
        y,
        width: (b.x1() + margin).min(frame.width) - x,
        height: (b.y1() + margin).min(frame.height) - y,
    }
}

/// A part of a window: the pixels off one background color that are joined to a seed, as region
/// mode finds a part.
struct Part {
    /// The window, in frame px.
    win: PxBox,
    bg: [u8; 3],
    /// How far off `bg` per channel the part's pixels are at least.
    hi: u8,
    /// The part's pixels, over the window.
    on: Vec<bool>,
    /// The box of `on`, in frame px.
    bbox: PxBox,
}

impl Part {
    fn index(&self, x: u32, y: u32) -> usize {
        (y - self.win.y) as usize * self.win.width as usize + (x - self.win.x) as usize
    }

    /// The part's pixels, in frame px.
    fn pixels(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        let w = self.win.width as usize;
        self.on
            .iter()
            .enumerate()
            .filter(|(_, on)| **on)
            .map(move |(i, _)| (self.win.x + (i % w) as u32, self.win.y + (i / w) as u32))
    }
}

/// Which pixels of a window are clearly off a background color, judged once each as they are
/// asked for (a part is often far smaller than its window).
struct Judge<'a> {
    frame: &'a RgbaFrame,
    win: PxBox,
    bg: [u8; 3],
    hi: u8,
    /// 0: not judged, 1: strong, 2: not.
    judged: Vec<u8>,
    /// The same for `core`.
    cored: Vec<u8>,
}

impl Judge<'_> {
    fn inside(&self, x: i64, y: i64) -> bool {
        x >= 0 && y >= 0 && x < i64::from(self.win.width) && y < i64::from(self.win.height)
    }

    /// Whether a pixel of the window is more than `by` per channel off the background.
    fn off(&self, x: i64, y: i64, by: u8) -> bool {
        let c = rgb(self.frame, self.win.x + x as u32, self.win.y + y as u32);
        !near(c, self.bg, by)
    }

    /// More than `hi` off the background. Past the window counts as strong.
    fn strong(&mut self, x: i64, y: i64) -> bool {
        if !self.inside(x, y) {
            return true;
        }
        let i = (y * i64::from(self.win.width) + x) as usize;
        if self.judged[i] == 0 {
            self.judged[i] = if self.off(x, y, self.hi) { 1 } else { 2 };
        }
        self.judged[i] == 1
    }

    /// Strong, with eight strong neighbors. For a pixel of the window.
    fn core(&mut self, x: i64, y: i64) -> bool {
        let i = (y * i64::from(self.win.width) + x) as usize;
        if self.cored[i] == 0 {
            let core = (-1..=1).all(|dy| (-1..=1).all(|dx| self.strong(x + dx, y + dy)));
            self.cored[i] = if core { 1 } else { 2 };
        }
        self.cored[i] == 1
    }
}

/// The part of `win` that holds `seed` against the background `bg`, when it is closed: it ends
/// inside the window, or at the image edge. `None` when the seed is background or the part
/// goes on past the window.
///
/// Region mode joins every pixel more than 8 per channel off the background. A movie tints the
/// background for a few px next to a saturated edge, and those specks would bridge a 2 px gap
/// to the next part. So the part is joined only through pixels that are clearly off the
/// background (half the contrast of `own`, the seed's color, from 8 to 24 per channel) and
/// whose eight neighbors are too, and it takes in the pixels next to those. A part too thin to
/// have such a pixel (a glyph, a line) is joined through its clear pixels, or failing that
/// through every pixel off the background.
///
/// With `solid`, the seed must be on the part: clearly off the background itself, or within
/// 2 px of a pixel that is, with its neighbors. Else `None`.
fn part_at(
    frame: &RgbaFrame,
    win: PxBox,
    bg: [u8; 3],
    seed: (u32, u32),
    own: [u8; 3],
    solid: bool,
) -> Option<Part> {
    if near(rgb(frame, seed.0, seed.1), bg, MASK_TOLERANCE) {
        return None;
    }
    let hi = (contrast(own, bg) / 2).clamp(MASK_TOLERANCE, CLEAR);
    let (w, h) = (i64::from(win.width), i64::from(win.height));
    let n = (w * h) as usize;
    let mut judge = Judge {
        frame,
        win,
        bg,
        hi,
        judged: vec![0; n],
        cored: vec![0; n],
    };
    let (sx, sy) = (i64::from(seed.0 - win.x), i64::from(seed.1 - win.y));
    // The core pixel to start from: the seed, or one within 2 px of it.
    let mut start = None;
    'search: for r in 0..=2 {
        for y in (sy - r).max(0)..=(sy + r).min(h - 1) {
            for x in (sx - r).max(0)..=(sx + r).min(w - 1) {
                if judge.core(x, y) {
                    start = Some((x, y));
                    break 'search;
                }
            }
        }
    }
    // A pixel on a side of the window that is not a side of the image: the part goes on.
    let open = |x: i64, y: i64| {
        (x == 0 && win.x > 0)
            || (y == 0 && win.y > 0)
            || (x == w - 1 && win.x1() < frame.width)
            || (y == h - 1 && win.y1() < frame.height)
    };
    let index = |x: i64, y: i64| (y * w + x) as usize;
    let mut on = vec![false; n];
    let (mut x0, mut y0, mut x1, mut y1) = (i64::MAX, i64::MAX, -1, -1);
    let mut stack: Vec<(i64, i64)> = Vec::new();
    if let Some(start) = start {
        let mut seen = vec![false; n];
        seen[index(start.0, start.1)] = true;
        stack.push(start);
        while let Some((x, y)) = stack.pop() {
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let (nx, ny) = (x + dx, y + dy);
                    if !judge.inside(nx, ny) {
                        continue;
                    }
                    let i = index(nx, ny);
                    if !on[i] {
                        if open(nx, ny) {
                            return None;
                        }
                        on[i] = true;
                        (x0, y0, x1, y1) = (x0.min(nx), y0.min(ny), x1.max(nx), y1.max(ny));
                    }
                    if !seen[i] && judge.core(nx, ny) {
                        seen[i] = true;
                        stack.push((nx, ny));
                    }
                }
            }
        }
    } else {
        let by = if judge.strong(sx, sy) {
            hi
        } else if solid {
            return None;
        } else {
            MASK_TOLERANCE
        };
        on[index(sx, sy)] = true;
        stack.push((sx, sy));
        while let Some((x, y)) = stack.pop() {
            if open(x, y) {
                return None;
            }
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let (nx, ny) = (x + dx, y + dy);
                    if judge.inside(nx, ny) && !on[index(nx, ny)] && judge.off(nx, ny, by) {
                        on[index(nx, ny)] = true;
                        stack.push((nx, ny));
                    }
                }
            }
        }
    }
    Some(Part {
        win,
        bg,
        hi,
        on,
        bbox: PxBox {
            x: win.x + x0 as u32,
            y: win.y + y0 as u32,
            width: (x1 - x0 + 1) as u32,
            height: (y1 - y0 + 1) as u32,
        },
    })
}

/// The closed part round `seed` against the first main border color of `win` that gives one.
/// A color counts when it holds at least 1/8 of the border and is not the seed's own.
fn closed_by(
    frame: &RgbaFrame,
    win: PxBox,
    (colors, total): &(Vec<([u8; 3], usize)>, usize),
    seed: (u32, u32),
    own: [u8; 3],
) -> Option<Part> {
    colors
        .iter()
        .filter(|(c, n)| n * 8 >= *total && !near(own, *c, MASK_TOLERANCE))
        .find_map(|(c, _)| {
            let part = part_at(frame, win, *c, seed, own, false)?;
            // A seed a little off the background takes a low threshold, and at that one the
            // letters of a word and the tint a movie leaves round them join into one part. So
            // a part whose main color is further off the background than the seed is found
            // again at that color's threshold, as a seed on its fill finds it. A seed that is
            // not on that part is a tinted pixel of the background: this color closes nothing.
            // At the highest threshold already, no color takes a higher one.
            if part.hi >= CLEAR {
                return Some(part);
            }
            match main_color(frame, &part) {
                Some(main)
                    if (contrast(main, part.bg) / 2).clamp(MASK_TOLERANCE, CLEAR) > part.hi =>
                {
                    let again = part_at(frame, win, part.bg, seed, main, true)?;
                    Some(if part.bbox.contains(&again.bbox) {
                        again
                    } else {
                        part
                    })
                }
                _ => Some(part),
            }
        })
}

/// The color of the surface at a pixel: the mean of the pixels within 2 px that are within 6
/// per channel of it, so a movie's noise does not shift a fill's tolerance.
fn settled(frame: &RgbaFrame, x: u32, y: u32) -> [u8; 3] {
    let at = rgb(frame, x, y);
    let around = window(
        frame,
        PxBox {
            x,
            y,
            width: 1,
            height: 1,
        },
        2,
    );
    let like = (around.y..around.y1())
        .flat_map(|py| (around.x..around.x1()).map(move |px| (px, py)))
        .map(|(px, py)| rgb(frame, px, py))
        .filter(|c| near(*c, at, 6));
    mean(like).unwrap_or(at)
}

/// Whether the pixels like `own` through `(x, y)` are at most `thin` px long along its row or
/// its column: a line, or the stroke of a letter.
fn thin_at(frame: &RgbaFrame, (x, y): (u32, u32), own: [u8; 3], thin: u32) -> bool {
    let like = |px: i64, py: i64| {
        px >= 0
            && py >= 0
            && px < i64::from(frame.width)
            && py < i64::from(frame.height)
            && near(rgb(frame, px as u32, py as u32), own, CLEAR)
    };
    let run = |dx: i64, dy: i64| -> usize {
        1 + [-1i64, 1]
            .iter()
            .map(|s| {
                (1..=i64::from(thin) + 1)
                    .take_while(|k| like(i64::from(x) + s * k * dx, i64::from(y) + s * k * dy))
                    .count()
            })
            .sum::<usize>()
    };
    run(1, 0).min(run(0, 1)) <= thin as usize
}

/// Whether the pixels joined to `seed` through its color `own` in `win` reach open surface: a
/// pixel with that color `thin / 2 + 1` px all round it. A line or the stroke of a letter does
/// not, and so does not the crossing of two lines. The fill of a button in the channel between
/// two letters of its label does: it is as thin there as a stroke, and it is the surface.
fn reaches_open_surface(
    frame: &RgbaFrame,
    win: PxBox,
    seed: (u32, u32),
    own: [u8; 3],
    thin: u32,
) -> bool {
    let mask = fill_within(frame, win, seed, own, CLEAR);
    let (w, h) = (win.width as i64, win.height as i64);
    let r = i64::from(thin / 2 + 1);
    let on = |x: i64, y: i64| x >= 0 && y >= 0 && x < w && y < h && mask[(y * w + x) as usize];
    (r..h - r).any(|y| (r..w - r).any(|x| (-r..=r).all(|dy| (-r..=r).all(|dx| on(x + dx, y + dy)))))
}

/// The surface beside a thin mark at `seed`, of the color `own`: the nearest pixel within 15 pt
/// that is more than `off` per channel from the mark's color and is not on something thin
/// itself (a blurred edge pixel, the next letter, the hole of an "o"), and the color there.
/// With `flat`, its eight neighbors have its color too: beside a sliver of a blurred edge lies
/// the next line of that blur, and the surface starts past it.
fn beside(
    frame: &RgbaFrame,
    seed: (u32, u32),
    own: [u8; 3],
    scale: &Scale,
    off: u8,
    flat: bool,
) -> Option<((u32, u32), [u8; 3])> {
    let at = PxBox {
        x: seed.0,
        y: seed.1,
        width: 1,
        height: 1,
    };
    let win = window(frame, at, scale.small + scale.margin);
    let mut others: Vec<(u64, u32, u32)> = (win.y..win.y1())
        .flat_map(|y| (win.x..win.x1()).map(move |x| (x, y)))
        .filter(|&(x, y)| !near(rgb(frame, x, y), own, off))
        .map(|(x, y)| {
            let d = u64::from(x.abs_diff(seed.0)).pow(2) + u64::from(y.abs_diff(seed.1)).pow(2);
            (d, y, x)
        })
        .collect();
    others.sort_unstable();
    let level = |x: u32, y: u32, color: [u8; 3]| {
        let around = window(
            frame,
            PxBox {
                x,
                y,
                width: 1,
                height: 1,
            },
            1,
        );
        around.area() == 9
            && (around.y..around.y1()).all(|py| {
                (around.x..around.x1()).all(|px| near(rgb(frame, px, py), color, FILL_TOLERANCE))
            })
    };
    others.into_iter().find_map(|(_, y, x)| {
        let color = settled(frame, x, y);
        (!thin_at(frame, (x, y), color, scale.thin) && (!flat || level(x, y, color)))
            .then_some(((x, y), color))
    })
}

/// The pixels of a window that `free` holds and that are joined (4-connected) to its border.
fn from_border(w: usize, h: usize, free: impl Fn(usize, usize) -> bool) -> Vec<bool> {
    let mut out = vec![false; w * h];
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let border = (0..w)
        .flat_map(|x| [(x, 0), (x, h - 1)])
        .chain((0..h).flat_map(|y| [(0, y), (w - 1, y)]));
    for (x, y) in border {
        if !out[y * w + x] && free(x, y) {
            out[y * w + x] = true;
            stack.push((x, y));
        }
    }
    while let Some((x, y)) = stack.pop() {
        let mut push = |nx: usize, ny: usize| {
            if !out[ny * w + nx] && free(nx, ny) {
                out[ny * w + nx] = true;
                stack.push((nx, ny));
            }
        };
        if x > 0 {
            push(x - 1, y);
        }
        if x + 1 < w {
            push(x + 1, y);
        }
        if y > 0 {
            push(x, y - 1);
        }
        if y + 1 < h {
            push(x, y + 1);
        }
    }
    out
}

/// The pixel nearest `seed` on the surface round a mark: of the color `bg`, and joined through
/// it to the border of `win`, so never shut inside the mark (the hole of an "o"). A pixel with
/// the surface `open` px all round it comes first, then one with half of that, then any: in
/// the channel between two letters a movie's noise can shut a fill in, and the open surface
/// past the word is one piece.
fn surface_near(
    frame: &RgbaFrame,
    win: PxBox,
    seed: (u32, u32),
    bg: [u8; 3],
    open: u32,
) -> Option<(u32, u32)> {
    let (w, h) = (win.width as usize, win.height as usize);
    let surface = from_border(w, h, |x, y| {
        near(
            rgb(frame, win.x + x as u32, win.y + y as u32),
            bg,
            MASK_TOLERANCE,
        )
    });
    // Past the window counts as surface: the window ends where the search does, not the surface.
    let clear = |x: u32, y: u32, r: i64| {
        (-r..=r).all(|dy| {
            (-r..=r).all(|dx| {
                let (px, py) = (i64::from(x - win.x) + dx, i64::from(y - win.y) + dy);
                px < 0
                    || py < 0
                    || px >= w as i64
                    || py >= h as i64
                    || surface[py as usize * w + px as usize]
            })
        })
    };
    let mut radii = vec![i64::from(open), i64::from(open / 2), 0];
    radii.dedup();
    // One pass for each radius: a pixel is tested only when it is nearer than the best so far.
    radii.into_iter().find_map(|r| {
        let mut best: Option<(u64, u32, u32)> = None;
        for i in (0..w * h).filter(|&i| surface[i]) {
            let (x, y) = (win.x + (i % w) as u32, win.y + (i / w) as u32);
            let d = u64::from(x.abs_diff(seed.0)).pow(2) + u64::from(y.abs_diff(seed.1)).pow(2);
            if best.is_none_or(|b| (d, y, x) < b) && (r == 0 || clear(x, y, r)) {
                best = Some((d, y, x));
            }
        }
        best.map(|(_, y, x)| (x, y))
    })
}

/// The surface round a pocket: a fill with the box `b` that is no larger than the hole of a
/// letter, on a surface of the color `own`. The pixel nearest `seed` of that color that is
/// joined through it to the border of a window two small parts wider than the box, and the
/// color there. A pocket of that color is not joined to that border, or its fill would have
/// run on.
fn round_pocket(
    frame: &RgbaFrame,
    b: PxBox,
    seed: (u32, u32),
    own: [u8; 3],
    scale: &Scale,
) -> Option<((u32, u32), [u8; 3])> {
    let win = window(frame, b, 2 * scale.small);
    let (x, y) = surface_near(frame, win, seed, own, scale.thin)?;
    Some(((x, y), settled(frame, x, y)))
}

/// How many pixels of `b` a part covers, with the holes shut inside it (the digit of a count
/// badge, the label of a button).
fn covered(part: &Part, b: PxBox) -> usize {
    let (w, h) = (part.win.width as usize, part.win.height as usize);
    let outside = from_border(w, h, |x, y| !part.on[y * w + x]);
    (b.y..b.y1())
        .flat_map(|y| (b.x..b.x1()).map(move |x| (x, y)))
        .filter(|&(x, y)| !outside[part.index(x, y)])
        .count()
}

/// Whether a small part is a letter of a word: more clear pixels start within `join` px of it
/// on its rows and go on past its box `b`.
fn in_a_word(frame: &RgbaFrame, part: &Part, b: PxBox, join: u32) -> bool {
    let (w, h) = (part.win.width as usize, part.win.height as usize);
    let clear = |x: usize, y: usize| {
        !near(
            rgb(frame, part.win.x + x as u32, part.win.y + y as u32),
            part.bg,
            part.hi,
        )
    };
    let mut seen = part.on.clone();
    let mut stack: Vec<(usize, usize)> = (0..w * h)
        .filter(|&i| seen[i])
        .map(|i| (i % w, i / w))
        .collect();
    let (left, right) = ((b.x - part.win.x) as usize, (b.x1() - part.win.x) as usize);
    let reach = join as usize + 1;
    while let Some((x, y)) = stack.pop() {
        // One px past the box is its blurred edge.
        if x + 1 < left || x > right {
            return true;
        }
        for ny in y.saturating_sub(1)..=(y + 1).min(h - 1) {
            let r = if ny == y { reach } else { 1 };
            for nx in x.saturating_sub(r)..=(x + r).min(w - 1) {
                if !seen[ny * w + nx] && clear(nx, ny) {
                    seen[ny * w + nx] = true;
                    stack.push((nx, ny));
                }
            }
        }
    }
    false
}

/// The box of a part's pixels that are nearer `own` than the part's background: its fill,
/// without a border or a shadow round it.
fn fill_box(frame: &RgbaFrame, part: &Part, own: [u8; 3]) -> Option<PxBox> {
    let mut b: Option<(u32, u32, u32, u32)> = None;
    for (x, y) in part.pixels() {
        let c = rgb(frame, x, y);
        if distance(c, own) < distance(c, part.bg) {
            let (x0, y0, x1, y1) = b.unwrap_or((x, y, x, y));
            b = Some((x0.min(x), y0.min(y), x1.max(x), y1.max(y)));
        }
    }
    b.map(|(x0, y0, x1, y1)| PxBox {
        x: x0,
        y: y0,
        width: x1 - x0 + 1,
        height: y1 - y0 + 1,
    })
}

/// Whether a small part has its background next to it: of the pixels up to 3 px past its box,
/// one in eight is the background's color. The bowl of an "a" in white text is shut in by
/// strokes that are as far off a near-white page as the page's own noise, so the page closes
/// it as a part. The page is nowhere near it: the bowl is a pocket of the button's fill.
fn touches_background(frame: &RgbaFrame, part: &Part) -> bool {
    let (b, ring) = (part.bbox, window(frame, part.bbox, 3));
    let (x0, y0) = (ring.x.max(part.win.x), ring.y.max(part.win.y));
    let (x1, y1) = (ring.x1().min(part.win.x1()), ring.y1().min(part.win.y1()));
    let (mut all, mut background) = (0usize, 0usize);
    for y in y0..y1 {
        for x in x0..x1 {
            if x < b.x || y < b.y || x >= b.x1() || y >= b.y1() {
                all += 1;
                background += usize::from(near(rgb(frame, x, y), part.bg, MASK_TOLERANCE));
            }
        }
    }
    background * 8 >= all
}

/// Whether a part is strokes rather than a shape: in its box `b`, under half of the pixels
/// are on the side of its main color (not of its background), and its top and bottom lines
/// are not both of that color over 0.7 of their length (an outlined field's are). A word whose
/// letters touch in the recording is such a part.
fn is_strokes(frame: &RgbaFrame, part: &Part, b: PxBox) -> bool {
    let Some(main) = main_color(frame, part) else {
        return false;
    };
    let ink = |x: u32, y: u32| {
        let c = rgb(frame, x, y);
        distance(c, main) < distance(c, part.bg)
    };
    let row = |y: u32| (b.x..b.x1()).filter(|&x| ink(x, y)).count();
    let all: usize = (b.y..b.y1()).map(row).sum();
    let lined = |y: u32| row(y) * 10 >= b.width as usize * 7;
    (all as u64) * 2 < b.area() && !(lined(b.y) && lined(b.y1() - 1))
}

/// The most common color of a part (of at most 65,536 of its pixels, evenly spread).
fn main_color(frame: &RgbaFrame, part: &Part) -> Option<[u8; 3]> {
    let count = part.on.iter().filter(|on| **on).count();
    let colors: Vec<[u8; 3]> = part
        .pixels()
        .step_by((count >> 16).max(1))
        .map(|(x, y)| rgb(frame, x, y))
        .collect();
    commonest(&colors).map(|(c, _)| c)
}

/// What point mode says about its box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Note {
    /// The point was on a mark, and the box is the surface round it.
    OnMark,
    /// The box is a small part under the point.
    SmallPart,
    /// The box is a glyph or a line: no surface was found round it.
    Mark,
    /// Lines bound the box, with its own color past them.
    Lines,
    /// More than one color bounds the box, so its edges follow its own color.
    Mixed,
    /// The point's pixel is not the box's main color.
    OffFill,
    /// The box holds more than its fill, which is this wide and tall (px).
    Rim(u32, u32),
    /// The box's part covers under 0.65 of it.
    NotSolid,
    /// No box: the fill hit its cap.
    Page,
    /// No box: a thin line with no surface found round it.
    Thin,
    /// The box touches the image edge.
    ImageEdge,
    /// The box covers over half the image.
    Large,
    /// Points close to this one give other boxes, and the box is the point's own.
    Edge,
    /// The box is the one the points close to this one give. The point itself gave another, of
    /// this width and height (px), or none.
    Voted(Option<(u32, u32)>),
    /// The box is a strip: under 6 pt on one side and 4 times as long.
    Strip,
}

impl Note {
    fn says(self, ppp: Option<f64>) -> String {
        match self {
            Note::OnMark => "the point was on text, a glyph or a thin line: this is the surface \
                             around it"
                .into(),
            Note::SmallPart => "this is a small part under the point (a dot, a badge, an icon, \
                                or text whose letters run together): put the point next to it \
                                for the surface it sits on"
                .into(),
            Note::Mark => "the box is a glyph or a thin line, not a component: put the point \
                           on the component's fill"
                .into(),
            Note::Lines => "the box ends at lines, with its own color past them (a list row, a \
                            table cell, an outlined card): a faint line lets it run on, so \
                            check it against the image"
                .into(),
            Note::Mixed => "more than one color borders this box, so its edges follow its own \
                            color: a gradient fill gives only a part of it, and a faint edge \
                            can be 2 px off. Check it with --region"
                .into(),
            Note::OffFill => "the point is on text, an icon or a line inside this box, not on \
                              its fill"
                .into(),
            Note::Rim(w, h) => {
                let fill = match ppp {
                    Some(p) => format!(
                        "{}×{} pt ({w}×{h} px)",
                        num(half(f64::from(w) / p)),
                        num(half(f64::from(h) / p))
                    ),
                    None => format!("{w}×{h} px"),
                };
                format!(
                    "the box includes a border, a shadow or a part against its fill: the fill \
                     alone is {fill}"
                )
            }
            Note::NotSolid => "the box is not a solid shape (a large letter, an icon, or a \
                               color that wraps round other parts): check it against the image"
                .into(),
            Note::Page => "no box: the point is on the page background, whose color covers \
                           over half the image. Use --region for the parts on it"
                .into(),
            Note::Thin => "no box: the point is on text or a thin line and no surface was \
                           found around it. Put the point on a fill"
                .into(),
            Note::ImageEdge => "the box touches the image edge: it may run past it".into(),
            Note::Strip => "the box is a thin strip (under 6 pt on one side): it may be a \
                            stroke or a slice of the component, not all of it. Check it against \
                            the image, or use --region"
                .into(),
            Note::Large => "the box covers over half the image: the component's edge may be \
                            too faint to find (a white card on a near-white page, a faint 1 px \
                            border). Draw a tight --region around the component"
                .into(),
            Note::Edge => "points close to this one give other boxes: the point is at an edge, \
                           or on text or a small part. Check the box against the image, or \
                           put the point well inside the component's fill"
                .into(),
            Note::Voted(own) => {
                let own = match (own, ppp) {
                    (Some((w, h)), Some(p)) => format!(
                        "{}×{} pt ({w}×{h} px)",
                        num(half(f64::from(w) / p)),
                        num(half(f64::from(h) / p))
                    ),
                    (Some((w, h)), None) => format!("{w}×{h} px"),
                    (None, _) => "no box".into(),
                };
                format!(
                    "points close to this one give this box, and the point itself gave {own}: \
                     it is on text, a small part or noise in the recording. Check the box \
                     against the image, or use --region"
                )
            }
        }
    }
}

/// Point mode's sizes in px, from px per pt.
struct Scale {
    /// A part under this on both sides is small: a dot, a glyph or a hole (12 pt).
    small: u32,
    /// How far a window reaches past what it holds (3 pt, at least 6 px).
    margin: u32,
    /// Letters this close on a row are one word (1.5 pt, as in region mode).
    join: u32,
    /// A run up to this long is a line or a stroke (1.5 pt, at least 3 px).
    thin: u32,
    /// A dot's short side is at least this (2.5 pt, at least 3 px).
    least: u32,
    /// How far from the point its four other seeds are (0.5 pt, at least 1 px).
    apart: u32,
    /// A line of text is under this tall (20 pt).
    text: u32,
}

impl Scale {
    fn of(ppp: f64) -> Self {
        let px = |pt: f64| (pt * ppp).round() as u32;
        Scale {
            small: px(2.0 * MIN_SIDE_PT),
            margin: px(3.0).max(6),
            join: px(JOIN_PT).max(1),
            thin: px(1.5).max(3),
            least: px(2.5).max(3),
            apart: px(0.5).max(1),
            text: px(20.0),
        }
    }

    fn is_small(&self, b: PxBox) -> bool {
        b.width < self.small && b.height < self.small
    }

    /// Under `small` on one side: a word is, and so is a pill or a bar.
    fn is_short(&self, b: PxBox) -> bool {
        b.width.min(b.height) < self.small
    }

    /// A fill under 24 pt on both sides whose main color round it is a shade of its own: a
    /// patch of that surface that a movie tinted, next to the letters of a label.
    fn is_a_patch(&self, b: PxBox, found: &Found, own: [u8; 3]) -> bool {
        matches!(found, Found::Mixed { bg, .. } if near(own, *bg, CLEAR))
            && b.width.max(b.height) < 2 * self.small
    }

    /// Thinner than the smallest dot.
    fn is_sliver(&self, b: PxBox) -> bool {
        b.width.min(b.height) < self.least
    }

    /// Under `small` on one side and twice that on the other: the hole of a large letter is
    /// no larger, and a checkbox is.
    fn can_be_a_hole(&self, b: PxBox) -> bool {
        self.is_short(b) && b.width.max(b.height) < 2 * self.small
    }
}

/// What a point is on.
enum Found {
    /// A part that one color round it closes, and its box.
    Part(Part, PxBox),
    /// A fill that lines bound: past them its own color goes on.
    Lines { bbox: PxBox },
    /// A fill with more than one color round it.
    Mixed {
        bbox: PxBox,
        win: PxBox,
        bg: [u8; 3],
    },
    /// A thin line or stroke.
    Line,
    /// The fill hit its cap.
    Page,
}

/// What `seed`, of the color `own`, is on.
///
/// First a window 15 pt round the seed: a color on its border that closes a part round the
/// seed gives a small part (a dot, a badge, a glyph on a button), whatever a movie did to its
/// shades. Else a thin run of the seed's color is a line. Else a flood fill gives a first guess
/// of the size, and the same test on a window round it gives the part: the box region mode
/// gives. When the fill's own color is the main color round it, lines bound it and the fill is
/// the box. When no one color closes it, the fill is the box too.
///
/// Without `small`, the first window is not tried: the seed is known to be on a surface.
fn find(
    frame: &RgbaFrame,
    seed: (u32, u32),
    own: [u8; 3],
    scale: &Scale,
    cap: usize,
    small: bool,
) -> Found {
    let at = PxBox {
        x: seed.0,
        y: seed.1,
        width: 1,
        height: 1,
    };
    let near_win = window(frame, at, scale.small + scale.margin);
    let colors = border_colors(frame, near_win);
    // A closed part too large to be small still tells the next window what to hold.
    let mut hold = None;
    if small && let Some(part) = closed_by(frame, near_win, &colors, seed, own) {
        let b = trimmed(frame, part.bbox, part.bg, MASK_TOLERANCE, true);
        if !scale.is_small(b) {
            hold = Some(part.bbox);
        } else if touches_background(frame, &part) {
            return Found::Part(part, b);
        }
    }
    // A thin run of a color that is not the main one round it: a line, the stroke of a letter.
    let around = colors.0.first().map_or(own, |(c, _)| *c);
    if hold.is_none()
        && !near(own, around, MASK_TOLERANCE)
        && thin_at(frame, seed, own, scale.thin)
        && !reaches_open_surface(frame, near_win, seed, own, scale.thin)
    {
        return Found::Line;
    }
    let room = (scale.small, scale.thin);
    let mut fill = flood(frame, seed, own, FILL_TOLERANCE, cap, room);
    // The fill is taken again round its own main color when the seed's is off it: next to
    // text a movie tints a pixel by up to the fill's tolerance, and a fill round that tint
    // holds only the half of the surface's shades that are on its side.
    if let Some(main) = fill
        .main
        .filter(|main| !fill.capped && !near(*main, own, 3))
    {
        fill = flood(frame, seed, main, FILL_TOLERANCE, cap, room);
    }
    if fill.capped {
        return Found::Page;
    }
    let mut held = fill.bbox;
    if let Some(b) = hold {
        let (x, y) = (held.x.min(b.x), held.y.min(b.y));
        held = PxBox {
            x,
            y,
            width: held.x1().max(b.x1()) - x,
            height: held.y1().max(b.y1()) - y,
        };
    }
    let win = window(frame, held, scale.margin);
    let colors = border_colors(frame, win);
    let around = colors.0.first().map_or(own, |(c, _)| *c);
    if near(fill.color, around, MASK_TOLERANCE) {
        return Found::Lines {
            bbox: blurred_box(frame, &fill),
        };
    }
    // The part is looked for from an open place of the fill. A seed between the strokes of a
    // label has no pixel clearly off the page next to it on every side, and from there the
    // part is the few pixels between those strokes.
    match closed_by(frame, win, &colors, fill.open.unwrap_or(seed), fill.color) {
        Some(part) => {
            let b = trimmed(frame, part.bbox, part.bg, MASK_TOLERANCE, true);
            Found::Part(part, b)
        }
        None => Found::Mixed {
            bbox: blurred_box(frame, &fill),
            win,
            bg: around,
        },
    }
}

/// The box that holds a point, and what to know about it.
#[derive(Clone, Debug, PartialEq)]
pub struct Pointed {
    pub bbox: Option<PxBox>,
    pub notes: Vec<Note>,
}

/// The box that holds `(x, y)`, at `ppp` px per pt.
///
/// One pixel decides little on a movie frame: noise can shut a fill in between two letters,
/// or join the letters of a word into one part. So the point and four pixels 0.5 pt from it
/// (`Scale::apart`) are each measured on their own (`box_from`), and boxes within 1 px of each
/// other on every side are one answer.
/// - The point's box, when two of the four give it too (all four, when the point stepped off
///   a mark to the surface next to it).
/// - The point's box with `Note::Edge`, when fewer do: the point is at an edge, or on a part
///   too small for its neighbors.
/// - The box three of the four give, with `Note::Voted`, when none gives the point's box.
pub fn box_at(frame: &RgbaFrame, x: u32, y: u32, ppp: f64) -> Pointed {
    let (x, y) = (x.min(frame.width - 1), y.min(frame.height - 1));
    let scale = Scale::of(ppp);
    let cap = ((f64::from(frame.width) * f64::from(frame.height) * FILL_CAP) as usize).max(1);
    let first = box_from(frame, (x, y), &scale, cap);
    let d = i64::from(scale.apart);
    let others: Vec<Pointed> = [(-d, 0), (d, 0), (0, -d), (0, d)]
        .into_iter()
        .map(|(dx, dy)| (i64::from(x) + dx, i64::from(y) + dy))
        .filter(|&(sx, sy)| {
            sx >= 0 && sy >= 0 && sx < i64::from(frame.width) && sy < i64::from(frame.height)
        })
        .map(|(sx, sy)| box_from(frame, (sx as u32, sy as u32), &scale, cap))
        .collect();
    let mut pointed = vote(first, &others);
    if let Some(b) = pointed.bbox
        && let (short, long) = (b.width.min(b.height), b.width.max(b.height))
        && f64::from(short) < 6.0 * ppp
        && long >= 4 * short
        && !pointed.notes.contains(&Note::Mark)
    {
        pointed.notes.push(Note::Strip);
    }
    pointed
}

/// The answer of a point (`first`) and of the pixels close to it (`others`), as one: see
/// `box_at`.
fn vote(mut first: Pointed, others: &[Pointed]) -> Pointed {
    let same = |a: &Pointed, b: &Pointed| match (a.bbox, b.bbox) {
        (Some(a), Some(b)) => {
            a.x.abs_diff(b.x) <= 1
                && a.y.abs_diff(b.y) <= 1
                && a.x1().abs_diff(b.x1()) <= 1
                && a.y1().abs_diff(b.y1()) <= 1
        }
        (None, None) => true,
        _ => false,
    };
    let agree = others.iter().filter(|o| same(&first, o)).count();
    // A point that stepped off a mark took the nearest surface: at an edge, that is a guess.
    let stepped = first.notes.contains(&Note::OnMark);
    if agree == others.len() || (agree * 2 >= others.len() && !stepped) {
        return first;
    }
    // The first of the others that all but one of them agree with.
    let voted = others
        .iter()
        .find(|o| (others.iter().filter(|p| same(o, p)).count() + 1) >= others.len().max(3));
    match voted {
        Some(most) if agree == 0 => {
            // What the other pixel was on says nothing of the point.
            let mut notes: Vec<Note> = most
                .notes
                .iter()
                .copied()
                .filter(|n| !matches!(n, Note::OnMark | Note::OffFill))
                .collect();
            notes.push(Note::Voted(first.bbox.map(|b| (b.width, b.height))));
            Pointed {
                bbox: most.bbox,
                notes,
            }
        }
        _ => {
            first.notes.push(Note::Edge);
            first
        }
    }
}

/// The box that holds the pixel `at`, measured from that pixel alone.
///
/// A small part under the pixel (a dot, a badge) is the box. A pixel on a mark steps out to
/// the surface round it, at most three times:
/// - a thin line or stroke, a small part that is no solid shape or that is a letter of a word,
///   and the hole of a letter (`hole_exit`);
/// - a part as low as a line of text (under 20 pt) that is letters of a word, or a word whose
///   letters touch: at 1 px per pt the letters of 17 pt text are 12 px tall;
/// - a fill that no color closes and that is thinner than a dot (a line of an edge's blur),
///   no larger than the hole of a letter, or a patch a movie tinted (`round_pocket`): on a
///   movie frame, noise shuts a fill in between the strokes of a label.
///
/// When a mark has no surface to step to, or the three steps end on one, there is no box
/// (`Note::Thin`): a pocket or a sliver is never given as a component.
fn box_from(frame: &RgbaFrame, at: (u32, u32), scale: &Scale, cap: usize) -> Pointed {
    let (x, y) = at;
    let under = rgb(frame, x, y);
    let (mut seed, mut own) = ((x, y), settled(frame, x, y));
    let mut notes = Vec::new();
    let (mut steps, mut small_part, mut mark) = (0, false, false);
    // The last thing found was a mark with nowhere to step to.
    let lost;
    // The seed is on a surface: a step led back into the small part it left, which is then a
    // patch of that surface in another shade (a movie leaves such patches next to text).
    let mut surface = false;
    let found = loop {
        let found = find(frame, seed, own, scale, cap, !surface);
        surface = false;
        let onto = |win: PxBox, bg: [u8; 3]| {
            surface_near(frame, win, seed, bg, scale.thin).map(|s| (s, bg))
        };
        // Whether what was found must not be the box.
        let mut no_box = false;
        let step = match &found {
            Found::Line => beside(frame, seed, own, scale, CLEAR, false),
            Found::Part(part, b) if scale.is_short(*b) => {
                // A solid shape of its own: a dot, a badge, a pill. A letter is not solid, or
                // is one of a word, and the stem of an "l" is not roughly round or square.
                let solid = b.width.min(b.height) >= scale.least
                    && (!scale.is_small(*b)
                        || (b.width <= 2 * b.height && b.height <= 2 * b.width))
                    && covered(part, *b) as u64 * 20 >= b.area() * 13
                    && !in_a_word(frame, part, *b, scale.join);
                let hole = scale
                    .can_be_a_hole(*b)
                    .then(|| hole_exit(frame, *b, own, scale.small))
                    .flatten();
                small_part = hole.is_none() && solid;
                mark = hole.is_none() && !solid;
                match hole {
                    Some(exit) => {
                        no_box = true;
                        let (ex, ey) = exit.0;
                        surface = ex >= b.x && ey >= b.y && ex < b.x1() && ey < b.y1();
                        Some(exit)
                    }
                    None if solid => None,
                    // A mark on its background. Or strokes shut it in and its background is
                    // not next to it: a pocket of the surface of its own color.
                    None if touches_background(frame, part) => onto(part.win, part.bg),
                    None => round_pocket(frame, *b, seed, own, scale),
                }
            }
            // Over 12 pt tall and text all the same, as low as a line of it: letters with the
            // next letter of their word beside them, or a word whose letters touch in the
            // recording (strokes, in a box at least 1.5 times as wide as tall).
            Found::Part(part, b)
                if b.height < scale.text
                    && (in_a_word(frame, part, *b, scale.join)
                        || (2 * b.width >= 3 * b.height && is_strokes(frame, part, *b))) =>
            {
                mark = true;
                if touches_background(frame, part) {
                    onto(part.win, part.bg)
                } else {
                    round_pocket(frame, *b, seed, own, scale)
                }
            }
            // A fill that no color closes, no larger than the hole of a letter (a pocket of
            // the surface between strokes) or thinner than a dot (one line of an edge's blur).
            Found::Lines { bbox: b } | Found::Mixed { bbox: b, .. }
                if scale.can_be_a_hole(*b)
                    || scale.is_sliver(*b)
                    || scale.is_a_patch(*b, &found, own) =>
            {
                // The surface is of the fill's own color. Or the fill is a patch that a movie
                // tinted, and the surface is of the main color round it.
                let around = match &found {
                    Found::Mixed { bg, .. } if near(own, *bg, CLEAR) => *bg,
                    _ => own,
                };
                let pocket = (scale.can_be_a_hole(*b) || scale.is_a_patch(*b, &found, own))
                    .then(|| round_pocket(frame, *b, seed, around, scale))
                    .flatten();
                let exit = pocket.or_else(|| {
                    scale
                        .is_sliver(*b)
                        .then(|| beside(frame, seed, own, scale, FILL_TOLERANCE, true))
                        .flatten()
                });
                match &found {
                    // A small fill between two colors that are not its own.
                    Found::Mixed { win, bg, .. } if exit.is_none() && !scale.is_sliver(*b) => {
                        no_box = scale.is_small(*b);
                        no_box.then(|| onto(*win, *bg)).flatten()
                    }
                    _ => {
                        no_box = true;
                        exit
                    }
                }
            }
            _ => None,
        };
        match step {
            Some((next, color)) if steps < 3 => {
                (seed, own, steps) = (next, color, steps + 1);
                (small_part, mark) = (false, false);
            }
            _ => {
                lost = no_box;
                break found;
            }
        }
    };
    if lost {
        return Pointed {
            bbox: None,
            notes: vec![Note::Thin],
        };
    }
    let bbox = match &found {
        Found::Page => {
            notes.push(Note::Page);
            None
        }
        Found::Line => {
            notes.push(Note::Thin);
            None
        }
        Found::Lines { bbox } => {
            notes.push(Note::Lines);
            Some(*bbox)
        }
        Found::Mixed { bbox, .. } => {
            notes.push(Note::Mixed);
            Some(*bbox)
        }
        Found::Part(_, b) if small_part => {
            notes.push(Note::SmallPart);
            Some(*b)
        }
        Found::Part(_, b) if mark => {
            notes.push(Note::Mark);
            Some(*b)
        }
        Found::Part(part, b) => {
            let main = main_color(frame, part).unwrap_or(own);
            if steps == 0 && !near(under, main, CLEAR) {
                notes.push(Note::OffFill);
            } else if near(own, main, CLEAR)
                && let Some(fill) = fill_box(frame, part, own)
                && (fill.x > b.x + 1
                    || fill.y > b.y + 1
                    || fill.x1() + 1 < b.x1()
                    || fill.y1() + 1 < b.y1())
            {
                notes.push(Note::Rim(fill.width, fill.height));
            }
            if (covered(part, *b) as u64) * 20 < b.area() * 13 {
                notes.push(Note::NotSolid);
            }
            Some(*b)
        }
    };
    // A step led to this box, unless the box is the mark itself.
    if steps > 0 && bbox.is_some() && !notes.contains(&Note::Mark) {
        notes.insert(0, Note::OnMark);
    }
    if let Some(b) = bbox
        && (b.x == 0 || b.y == 0 || b.x1() == frame.width || b.y1() == frame.height)
    {
        notes.push(Note::ImageEdge);
    }
    if let Some(b) = bbox
        && b.area() * 2 > u64::from(frame.width) * u64::from(frame.height)
    {
        notes.push(Note::Large);
    }
    Pointed { bbox, notes }
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
/// rather than to the part's color 4 px further in (a line with none goes too). With `round`,
/// a line with 2 pixels that do not lean to the background stays: the end line of a circle is
/// a few of its pixels between blurred ones. Point mode sets it for every part, whose box
/// holds no tinted specks (`part_at`). Region mode sets it for a box small enough to be a dot
/// or a badge: on a longer edge its mask takes in tinted lines that hold a few such pixels.
/// Both modes use this, so both put an edge midway between the two colors.
fn trimmed(frame: &RgbaFrame, mut b: PxBox, bg: [u8; 3], tolerance: u8, round: bool) -> PxBox {
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
        (on > 0).then_some(weak * 2 > on && !(round && on - weak >= 2))
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

/// Takes the specks a movie leaves on a plain background out of a region's mask: a group of
/// joined pixels (8-connected) under `min_side` px on both sides in which no pixel is more than
/// twice `tolerance` off the background. H.264 tints a few pixels up to a block away from a
/// saturated edge, and the join along a row would make them part of the box.
fn drop_specks(
    frame: &RgbaFrame,
    region: PxBox,
    bg: [u8; 3],
    tolerance: u8,
    min_side: u32,
    mask: &mut [bool],
) {
    let (rw, rh) = (region.width as usize, region.height as usize);
    let clear = |i: usize| {
        let (x, y) = (region.x + (i % rw) as u32, region.y + (i / rw) as u32);
        !near(rgb(frame, x, y), bg, tolerance.saturating_mul(2))
    };
    let mut seen = vec![false; rw * rh];
    let mut group: Vec<usize> = Vec::new();
    for start in 0..rw * rh {
        if !mask[start] || seen[start] {
            continue;
        }
        seen[start] = true;
        group.clear();
        group.push(start);
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
        let mut faint = true;
        let mut next = 0;
        while next < group.len() {
            let i = group[next];
            next += 1;
            let (x, y) = (i % rw, i / rw);
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            faint &= !clear(i);
            for ny in y.saturating_sub(1)..=(y + 1).min(rh - 1) {
                for nx in x.saturating_sub(1)..=(x + 1).min(rw - 1) {
                    let n = ny * rw + nx;
                    if mask[n] && !seen[n] {
                        seen[n] = true;
                        group.push(n);
                    }
                }
            }
        }
        let small = ((x1 - x0 + 1) as u32) < min_side && ((y1 - y0 + 1) as u32) < min_side;
        if faint && small {
            for &i in &group {
                mask[i] = false;
            }
        }
    }
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
    drop_specks(frame, region, bg, tolerance, min_side, &mut mask);
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
        // Under twice the smallest side on both sides: as small as point mode's small part.
        let round = b.width < 2 * min_side && b.height < 2 * min_side;
        let b = trimmed(frame, b, bg, tolerance, round);
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
    let ink = "red ink, pins and click rings drawn on it may show";
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
    warnings.push(format!(
        "measured {}: {ink}, and so does the keyframe's number chip in a corner",
        Source::Keyframe.says()
    ));
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

/// The image's place in full-frame px: its offset there, the full frame's size, and px per pt.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Space {
    pub offset: (f64, f64),
    pub full: (f64, f64),
    pub ppp: Option<f64>,
}

/// Px per pt for the size limits of a capture that does not say its size in points (a cabled
/// iPhone), from the shape of its full frame. The limits are in pt (12 pt for a small part,
/// 1.5 pt between the letters of a word), and at 1 px per pt on a 3x frame a point on a label
/// gave one letter and a region cut a word in two.
/// - An iPhone's screen is 16:9 or taller and 375 to 440 pt wide: its short side over 400,
///   2 for a 750 or 828 px frame and 3 for 1080 to 1320 px.
/// - A wider shape is an iPad's, 744 to 1032 pt wide: its short side over 834.
///
/// A whole number from 1.75 up, as every iPhone and iPad screen is 2x or 3x. Under that it is
/// not rounded, and at least 1: a saved iPad image fit to 1600 px is 1.26 to 1.41, and at 1 a
/// point on a label gave a letter. It sets the limits only: no size is given in pt from it.
fn estimated_ppp((w, h): (f64, f64)) -> f64 {
    let (short, long) = (w.min(h), w.max(h));
    let pt = if long >= 1.65 * short { 400.0 } else { 834.0 };
    let ppp = short / pt;
    if ppp >= 1.75 {
        ppp.round()
    } else {
        ppp.max(1.0)
    }
}

pub fn space(picture: &Picture, width_pt: Option<f64>) -> Space {
    let r = picture.rect;
    let full_w = f64::from(picture.frame.width) / f64::from((r[2] - r[0]).max(1e-6));
    let full_h = f64::from(picture.frame.height) / f64::from((r[3] - r[1]).max(1e-6));
    Space {
        offset: (f64::from(r[0]) * full_w, f64::from(r[1]) * full_h),
        full: (full_w, full_h),
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
    /// Px per pt that the size limits used when `px_per_pt` is unknown: an estimate from the
    /// frame's size. Left out when `px_per_pt` is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limits_px_per_pt: Option<f64>,
    /// The measured image's size in px.
    pub image: [u32; 2],
    /// Point mode: the box that holds the point. Left out when there is none (the point is on
    /// the page background), and `warnings` says why.
    #[serde(rename = "box", skip_serializing_if = "Option::is_none")]
    pub component: Option<Measured>,
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
    let estimate = space.ppp.is_none().then(|| estimated_ppp(space.full));
    if let Some(ppp) = estimate {
        warnings.push(format!(
            "the capture's size in points is unknown: px only. The size limits (a small part, \
             a thin line, the join of letters) used {} px per pt, an estimate from the frame's \
             size",
            num(ppp)
        ));
    }
    let ppp = space.ppp.or(estimate).unwrap_or(1.0);
    let clamp_px = |v: f64, max: u32| (v.floor().max(0.0) as u32).min(max - 1);
    let mut out = Measurement {
        frame: frame_n,
        source: picture.source,
        px_per_pt: space.ppp.map(|p| (p * 100.0).round() / 100.0),
        limits_px_per_pt: estimate,
        image: [w, h],
        component: None,
        boxes: None,
        gaps: None,
        warnings: Vec::new(),
    };
    match *query {
        Query::At(fx, fy) => {
            let (x, y) = to_px(&picture, crop, fx, fy);
            let (x, y) = (clamp_px(x, w), clamp_px(y, h));
            let pointed = box_at(img, x, y, ppp);
            warnings.extend(pointed.notes.iter().map(|note| note.says(space.ppp)));
            out.component = pointed.bbox.map(|b| measured(b, space, None));
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
            // A box on the region's edge is cut there, unless it ends exactly on it.
            let cut: Vec<String> = boxes
                .iter()
                .enumerate()
                .filter(|(_, b)| {
                    b.x == region.x
                        || b.y == region.y
                        || b.x1() == region.x1()
                        || b.y1() == region.y1()
                })
                .map(|(i, _)| (i + 1).to_string())
                .collect();
            if !cut.is_empty() {
                warnings.push(format!(
                    "box {} touches the region edge and may be cut off there: draw a wider \
                     region",
                    cut.join(", ")
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
    if let Some(b) = &m.component {
        out.push_str(&format!("  box {}\n", size(b)));
        out.push_str(
            "  gaps: a point gives none. Use --region around this box and its neighbors\n",
        );
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

    fn picture(frame: RgbaFrame, source: Source, rect: Rect) -> Picture {
        Picture {
            frame,
            source,
            rect,
            warnings: Vec::new(),
        }
    }

    /// The box that holds a point of `f` at 2 px per pt.
    fn at(f: &RgbaFrame, x: u32, y: u32) -> Pointed {
        box_at(f, x, y, 2.0)
    }

    /// A point whose box runs over most of the image, as on a white card on a near-white page
    /// whose edge is too faint to find: the output says so rather than give it as the card.
    #[test]
    fn a_box_over_most_of_the_image_says_its_edge_may_be_too_faint() {
        // A window's white content, full of lines of text, inside a thin grey frame.
        let mut f = canvas(400, 300, [0x80, 0x80, 0x80]);
        rect(&mut f, 30, 30, 340, 240, [0xFF, 0xFF, 0xFF]);
        for line in 0..20 {
            rect(&mut f, 50, 34 + line * 12, 300, 8, TEXT);
        }
        let p = at(&f, 40, 150);
        assert!(p.notes.contains(&Note::Large), "{:?}", p.notes);
        // A card of its own size says nothing of the kind.
        let p = at(&cards(1200, 800), 480, 440);
        assert!(!p.notes.contains(&Note::Large));
    }

    #[test]
    fn a_point_in_a_card_measures_the_card_even_off_or_next_to_text() {
        let f = cards(1200, 800);
        // The centre, 3 px off it, just under a line of text, just left of the text and 2 px
        // past the end of a line.
        for (x, y) in [(480, 440), (483, 437), (300, 316), (215, 300), (428, 330)] {
            let p = at(&f, x, y);
            assert_box(p.bbox.unwrap(), 192, 280, 320, 184);
            assert_eq!(p.notes, [], "{x},{y}");
        }
    }

    /// A point on a button's label, even right on a glyph or a stray pixel, measures the
    /// button and says so.
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
        // A solid 3×5 px piece of a dark letter on the button.
        rect(&mut f, 460, 200, 3, 5, [0x10, 0x20, 0x40]);
        // On a glyph's stroke, on the stray pixel, in a glyph's hole and on the dark piece.
        for (x, y) in [(330, 202), (400, 216), (334, 202), (461, 202)] {
            let p = at(&f, x, y);
            assert_eq!(p.notes, [Note::OnMark], "{x},{y}");
            assert_box(p.bbox.unwrap(), 300, 180, 188, 44);
        }
        // A point on the fill measures the button with nothing to say.
        let p = at(&f, 310, 185);
        assert_eq!(p.notes, []);
        assert_box(p.bbox.unwrap(), 300, 180, 188, 44);
        // A small solid part (a 14 px icon, 7 pt) is the box, and the output says it is small.
        rect(&mut f, 600, 100, 14, 14, BLUE);
        let p = at(&f, 605, 105);
        assert_eq!(p.notes, [Note::SmallPart]);
        assert_eq!(
            p.bbox.unwrap(),
            PxBox {
                x: 600,
                y: 100,
                width: 14,
                height: 14
            }
        );
    }

    /// A card with a border on a page of another color: a point and a region give the same
    /// box, with the border, and the point says how large the fill alone is.
    #[test]
    fn a_point_and_a_region_give_a_bordered_card_with_its_border() {
        const BORDER: [u8; 3] = [0xD0, 0xD0, 0xD0];
        let mut f = canvas(800, 400, BG);
        for x in [100, 330] {
            rect(&mut f, x, 100, 200, 100, BORDER);
            rect(&mut f, x + 2, 102, 196, 96, CARD);
        }
        let card = PxBox {
            x: 100,
            y: 100,
            width: 200,
            height: 100,
        };
        let p = at(&f, 200, 150);
        assert_eq!(p.bbox, Some(card));
        assert_eq!(p.notes, [Note::Rim(196, 96)]);
        assert!(
            Note::Rim(196, 96)
                .says(Some(2.0))
                .ends_with("the fill alone is 98×48 pt (196×96 px)")
        );
        // On the border itself: the same box, from the fill beside the line. The pixel next
        // to it is on the page, and the output says that points close by differ.
        let p = at(&f, 101, 150);
        assert_eq!(
            (p.bbox, p.notes),
            (
                Some(card),
                vec![Note::OnMark, Note::Rim(196, 96), Note::Edge]
            )
        );
        let region = PxBox {
            x: 50,
            y: 50,
            width: 530,
            height: 200,
        };
        let parts = boxes_in(&f, region, MASK_TOLERANCE, 3, 12, MAX_BOXES);
        assert_eq!(parts.kept[0], card);
        assert_eq!(nearest_gaps(&parts.kept)[0].px, 30);
    }

    /// Lines bound a list row, a table cell and an outlined card on a page of its own color:
    /// the box is the fill between the lines, and the output says lines bound it. A point on
    /// a line measures a surface next to it.
    #[test]
    fn a_fill_between_lines_is_the_box_and_says_so() {
        const LINE: [u8; 3] = [0xE0, 0xE0, 0xE0];
        let b = |x, y, width, height| PxBox {
            x,
            y,
            width,
            height,
        };
        // Three white rows 88 px tall, split by 2 px lines, on the page.
        let mut f = canvas(800, 600, BG);
        rect(&mut f, 100, 100, 600, 268, CARD);
        for y in [188, 278] {
            rect(&mut f, 100, y, 600, 2, LINE);
        }
        let p = at(&f, 400, 230);
        assert_eq!(
            (p.bbox, p.notes),
            (Some(b(100, 190, 600, 88)), vec![Note::Lines])
        );
        // On the divider: the row nearest the point. The pixel above gives the row above,
        // and the output says that points close by differ.
        let p = at(&f, 400, 189);
        assert_eq!(
            (p.bbox, p.notes),
            (
                Some(b(100, 190, 600, 88)),
                vec![Note::OnMark, Note::Lines, Note::Edge]
            )
        );
        // A cell of a table.
        let mut f = canvas(800, 600, CARD);
        for x in [100, 302, 504, 706] {
            rect(&mut f, x, 100, 2, 400, LINE);
        }
        for y in [100, 232, 364, 498] {
            rect(&mut f, 100, y, 608, 2, LINE);
        }
        let p = at(&f, 400, 300);
        assert_eq!(
            (p.bbox, p.notes),
            (Some(b(304, 234, 200, 130)), vec![Note::Lines])
        );
        // An outlined card on a page of its own color: the fill inside the outline. A 16 pt
        // checkbox is one too, not the hole of a letter.
        let mut f = canvas(800, 400, CARD);
        rect(&mut f, 100, 100, 200, 100, LINE);
        rect(&mut f, 102, 102, 196, 96, CARD);
        rect(&mut f, 400, 100, 32, 32, LINE);
        rect(&mut f, 402, 102, 28, 28, CARD);
        for (x, y, fill) in [
            (200, 150, b(102, 102, 196, 96)),
            (416, 116, b(402, 102, 28, 28)),
        ] {
            let p = at(&f, x, y);
            assert_eq!((p.bbox, p.notes), (Some(fill), vec![Note::Lines]));
        }
    }

    /// Two boxes 2 px apart, of one color or of two: the box ends at the gap.
    #[test]
    fn a_box_2_px_from_the_next_ends_at_the_gap() {
        for neighbor in [CARD, [0xC8, 0xD2, 0xFF]] {
            let mut f = canvas(800, 400, BG);
            rect(&mut f, 100, 100, 200, 100, CARD);
            rect(&mut f, 302, 100, 200, 100, neighbor);
            let p = at(&f, 200, 150);
            assert_eq!(p.notes, [], "{neighbor:?}");
            assert_eq!(
                p.bbox.unwrap(),
                PxBox {
                    x: 100,
                    y: 100,
                    width: 200,
                    height: 100
                }
            );
        }
    }

    /// A panel between a sidebar and a toolbar of other colors has no one color round it: the
    /// box is its fill, and the output says so.
    #[test]
    fn a_panel_with_two_colors_round_it_is_its_fill_and_says_so() {
        let mut f = canvas(1200, 800, CARD);
        rect(&mut f, 0, 0, 400, 800, [0x20, 0x24, 0x30]);
        rect(&mut f, 400, 0, 800, 250, [0xD8, 0xDC, 0xE4]);
        let p = at(&f, 800, 500);
        assert_eq!(
            p.bbox.unwrap(),
            PxBox {
                x: 400,
                y: 250,
                width: 800,
                height: 550
            }
        );
        assert_eq!(p.notes, [Note::Mixed, Note::ImageEdge]);
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
        for (x, y) in [(348, 203), (390, 201)] {
            let p = at(&f, x, y);
            assert_eq!(p.notes, [Note::OnMark], "{x},{y}");
            assert_box(p.bbox.unwrap(), 300, 180, 188, 44);
        }
        // Dots of 3 to 6 pt on the button and on the page: each measures itself to the pixel.
        for (x, y, d, on) in [
            (420, 190, 6, BLUE),
            (440, 196, 8, BLUE),
            (100, 100, 6, WHITE),
            (130, 100, 10, WHITE),
            (160, 100, 12, WHITE),
        ] {
            let mut f = f.clone();
            dot(&mut f, x, y, d, GREEN);
            let p = at(&f, x + d / 2, y + d / 2);
            assert_eq!(p.notes, [Note::SmallPart], "{d} px on {on:?}");
            let b = PxBox {
                x,
                y,
                width: d,
                height: d,
            };
            assert_eq!(p.bbox.unwrap(), b, "{d} px on {on:?}");
        }
        // A point on the button next to a dot still measures the button.
        dot(&mut f, 420, 190, 6, GREEN);
        let p = at(&f, 430, 205);
        assert_eq!(p.notes, []);
        assert_box(p.bbox.unwrap(), 300, 180, 188, 44);
    }

    /// A point on the page background gives no box, not a part of the page.
    #[test]
    fn a_point_on_the_page_background_gives_no_box() {
        let p = at(&cards(1200, 800), 20, 20);
        assert_eq!((p.bbox, p.notes), (None, vec![Note::Page]));
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

    /// A point prints its box and sends the reader to a region for gaps. On the page
    /// background it prints no box.
    #[test]
    fn a_point_prints_its_box_and_no_gaps() {
        let measure = |at| {
            measure_picture(
                1,
                picture(cards(1200, 800), Source::Movie, FULL),
                FULL,
                Some(600.0),
                &at,
            )
        };
        let m = measure(Query::At(0.4, 0.55));
        assert_eq!(
            render_text(&m),
            "frame 1 · 2 px per pt · from the movie\n  \
             box 160×92 pt (320×184 px) at 96,140 pt\n  \
             gaps: a point gives none. Use --region around this box and its neighbors\n"
        );
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["box"]["px"]["width"], 320);
        assert!(json.get("toEdges").is_none() && json.get("gaps").is_none());
        let m = measure(Query::At(0.02, 0.02));
        let text = render_text(&m);
        assert!(
            !text.contains("  box ") && !text.contains("gaps:"),
            "{text}"
        );
        assert!(
            text.contains("note: no box: the point is on the page background"),
            "{text}"
        );
        assert!(serde_json::to_value(&m).unwrap().get("box").is_none());
    }

    /// A box that the region's edge cuts says so: its size is only what the region holds.
    #[test]
    fn a_box_on_the_region_edge_says_it_may_be_cut_off() {
        // The region ends inside the first card and holds the second one whole.
        let m = measure_picture(
            1,
            picture(cards(1200, 800), Source::Movie, FULL),
            FULL,
            Some(600.0),
            &Query::Region([0.3, 0.25, 0.85, 0.75]),
        );
        let boxes = m.boxes.unwrap();
        assert_eq!(boxes.len(), 2);
        assert_eq!((boxes[0].px.x, boxes[0].px.width), (360, 152));
        assert_eq!(
            m.warnings,
            ["box 1 touches the region edge and may be cut off there: draw a wider region"]
        );
    }

    // ------------------------------------------------------------------------------------------
    // Frames that went through a real encoder
    // ------------------------------------------------------------------------------------------

    const WHITE: [u8; 3] = [0xFF, 0xFF, 0xFF];
    const BLUE: [u8; 3] = [37, 99, 235];
    const RED: [u8; 3] = [220, 38, 38];
    const GREEN: [u8; 3] = [52, 199, 89];
    /// The buttons of `ui`: four on white, four on #F2F2F2.
    const BUTTONS: [[u8; 3]; 8] = [
        BLUE,
        RED,
        GREEN,
        [255, 149, 0],
        [175, 82, 222],
        [30, 41, 59],
        [0, 122, 255],
        [255, 204, 0],
    ];
    const DOT_SIZES: [u32; 5] = [6, 8, 10, 12, 16];
    const DOT_COLORS: [[u8; 3]; 3] = [GREEN, [255, 59, 48], [0, 122, 255]];

    fn px_box(x: u32, y: u32, width: u32, height: u32) -> PxBox {
        PxBox {
            x,
            y,
            width,
            height,
        }
    }

    /// The buttons of `ui`: 199×77 px at odd offsets.
    fn ui_buttons() -> impl Iterator<Item = (PxBox, [u8; 3])> {
        (0..8).map(|i| {
            let (x, y) = (101 + (i as u32 % 4) * 300, [61, 261][i / 4]);
            (px_box(x, y, 199, 77), BUTTONS[i])
        })
    }

    /// The dots of `ui`: 3 to 8 pt, on #F2F2F2 and on white.
    fn ui_dots() -> impl Iterator<Item = (PxBox, [u8; 3])> {
        [672, 752].into_iter().flat_map(|y| {
            (0..15).map(move |i: usize| {
                let d = DOT_SIZES[i % 5];
                (px_box(60 + i as u32 * 80, y, d, d), DOT_COLORS[i / 5])
            })
        })
    }

    const CHIPS: [(u32, u32, u32, u32); 2] = [(101, 421, 199, 77), (343, 421, 121, 77)];
    const PANEL: (u32, u32, u32, u32) = (560, 420, 400, 200);
    const BADGES: [(u32, u32, u32); 2] = [(1040, 440, 16), (1100, 440, 22)];
    const LABELED: (u32, u32, u32, u32) = (101, 850, 188, 44);

    /// A 1280×960 px screen at 2 px per pt with known boxes: buttons with a neighbor 2 px to
    /// the right, two chips 43 px apart, a white panel on #F2F2F2, count badges, status dots
    /// and a button with a label.
    fn ui() -> RgbaFrame {
        let mut f = canvas(1280, 960, BG);
        rect(&mut f, 0, 0, 1280, 200, WHITE);
        rect(&mut f, 0, 720, 1280, 80, WHITE);
        for (b, c) in ui_buttons() {
            rect(&mut f, b.x, b.y, b.width, b.height, c);
            let tint = c.map(|v| v.saturating_add(40));
            rect(&mut f, b.x1() + 2, b.y, 60, b.height, tint);
        }
        for ((x, y, w, h), c) in CHIPS.into_iter().zip([BLUE, RED]) {
            rect(&mut f, x, y, w, h, c);
        }
        rect(&mut f, PANEL.0, PANEL.1, PANEL.2, PANEL.3, WHITE);
        // A badge: a red circle with a white "1" in it.
        for (x, y, d) in BADGES {
            dot(&mut f, x, y, d, [255, 59, 48]);
            rect(&mut f, x + d / 2 - 1, y + d / 4, 2, d / 2, WHITE);
        }
        for (b, c) in ui_dots() {
            dot(&mut f, b.x, b.y, b.width, c);
        }
        let (x, y, w, h) = LABELED;
        rect(&mut f, x, y, w, h, BLUE);
        for i in 0..10 {
            rect(&mut f, x + 30 + i * 12, y + 15, 9, 14, WHITE);
            rect(&mut f, x + 32 + i * 12, y + 17, 5, 10, BLUE);
        }
        f
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("Tests/fixtures/measure")
            .join(name)
    }

    /// With `SCRAWL_MEASURE_FIXTURES=write`, makes the fixture `name` again: the PNG `source`
    /// encoded to H.264 4:2:0 by `encoder` (`x264`, or `vt`, Apple's VideoToolbox, which
    /// records every capture, at `bitrate`) and one frame of that movie decoded again. Needs
    /// ffmpeg.
    fn encode_fixture(source: &Path, name: &str, encoder: &str, bitrate: &str) {
        if std::env::var("SCRAWL_MEASURE_FIXTURES").as_deref() != Ok("write") {
            return;
        }
        // Tests run side by side, and more than one asks for a fixture.
        static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = temp_dir(&format!("encode-{call}-{name}"));
        let movie = dir.join("frame.mp4");
        let codec: &[&str] = match encoder {
            "x264" => &["-c:v", "libx264", "-crf", "23"],
            _ => &["-c:v", "h264_videotoolbox", "-b:v", bitrate],
        };
        let ffmpeg = |args: &[&str]| {
            let status = std::process::Command::new("ffmpeg")
                .args(["-loglevel", "error", "-y"])
                .args(args)
                .status()
                .expect("ffmpeg runs");
            assert!(status.success(), "ffmpeg {args:?}");
        };
        let (source, movie) = (source.to_str().unwrap(), movie.to_str().unwrap());
        let mut encode = vec!["-loop", "1", "-i", source, "-t", "3", "-r", "30"];
        encode.extend_from_slice(codec);
        encode.extend_from_slice(&["-pix_fmt", "yuv420p", movie]);
        ffmpeg(&encode);
        let path = fixture(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        ffmpeg(&[
            "-ss",
            "1",
            "-i",
            movie,
            "-frames:v",
            "1",
            path.to_str().unwrap(),
        ]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// How many pixels of `f` are more than 8 per channel off `clean`: the marks an encoder
    /// left. A stale or a clean fixture fails the range its test gives.
    fn changed(f: &RgbaFrame, clean: &RgbaFrame) -> usize {
        assert_eq!((f.width, f.height), (clean.width, clean.height));
        (0..f.height)
            .flat_map(|y| (0..f.width).map(move |x| (x, y)))
            .filter(|&(x, y)| !near(rgb(f, x, y), rgb(clean, x, y), MASK_TOLERANCE))
            .count()
    }

    /// `ui` as a movie gives it back: encoded by `encoder` (`x264` or `vt`) and decoded again,
    /// saved in `Tests/fixtures/measure/`. `SCRAWL_MEASURE_FIXTURES=write cargo test measure`
    /// makes the files again with ffmpeg, after a change to `ui`.
    fn encoded_ui(encoder: &str) -> RgbaFrame {
        let name = format!("ui-{encoder}.png");
        if std::env::var("SCRAWL_MEASURE_FIXTURES").as_deref() == Ok("write") {
            static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = temp_dir(&format!("source-{call}-{encoder}"));
            let source = dir.join("ui.png");
            crate::render::save_png(&ui(), &source).unwrap();
            // 1200 kbit/s here is the bits per pixel of a 2560×1440 capture at 3.4 Mbit/s.
            encode_fixture(&source, &name, encoder, "1200k");
            let _ = std::fs::remove_dir_all(&dir);
        }
        let f = read_png(&fixture(&name)).unwrap();
        // The file is `ui`, and the encoder left its marks on it.
        let changed = changed(&f, &ui());
        assert!((1_000..60_000).contains(&changed), "{encoder}: {changed}");
        f
    }

    fn assert_within_1(b: PxBox, want: (u32, u32, u32, u32), what: &str) {
        let (x, y, w, h) = want;
        assert!(
            close(b.x, x) && close(b.y, y) && close(b.width, w) && close(b.height, h),
            "{what}: {b:?} is not {want:?}"
        );
    }

    /// On a frame from a real encoder, a region gives each box and gap within 1 px, and a
    /// point gives the same box.
    #[test]
    fn a_region_and_a_point_agree_within_1_px_on_an_encoded_frame() {
        for encoder in ["x264", "vt"] {
            let f = encoded_ui(encoder);
            // Both chips and the 43 px between them.
            let parts = boxes_in(
                &f,
                px_box(60, 390, 460, 140),
                MASK_TOLERANCE,
                3,
                12,
                MAX_BOXES,
            );
            assert_eq!(parts.kept.len(), 2, "{encoder}: {:?}", parts.kept);
            for (i, chip) in CHIPS.into_iter().enumerate() {
                assert_within_1(parts.kept[i], chip, encoder);
                let p = at(&f, chip.0 + chip.2 / 2, chip.1 + chip.3 / 2);
                assert_eq!(p.notes, [], "{encoder} chip {i}");
                assert_within_1(p.bbox.unwrap(), chip, encoder);
                let (a, b) = (p.bbox.unwrap(), parts.kept[i]);
                assert_within_1(a, (b.x, b.y, b.width, b.height), encoder);
            }
            let gaps = nearest_gaps(&parts.kept);
            assert!(
                gaps.len() == 1 && close(gaps[0].px, 43),
                "{encoder}: {gaps:?}"
            );
            // A white panel on #F2F2F2: 13 levels of contrast.
            let parts = boxes_in(
                &f,
                px_box(540, 400, 440, 240),
                MASK_TOLERANCE,
                3,
                12,
                MAX_BOXES,
            );
            assert_eq!(parts.kept.len(), 1, "{encoder}: {:?}", parts.kept);
            assert_within_1(parts.kept[0], PANEL, encoder);
            for (x, y) in [(760, 520), (570, 430), (950, 610)] {
                let p = at(&f, x, y);
                assert_eq!(p.notes, [], "{encoder} panel at {x},{y}");
                assert_eq!(p.bbox, Some(parts.kept[0]), "{encoder} panel at {x},{y}");
            }
        }
    }

    /// On a frame from a real encoder, a button ends at the 2 px gap to its neighbor, on white
    /// and on #F2F2F2, and a point on its label measures it.
    #[test]
    fn a_point_measures_a_button_within_1_px_on_an_encoded_frame() {
        for encoder in ["x264", "vt"] {
            let f = encoded_ui(encoder);
            for (b, color) in ui_buttons() {
                let p = at(&f, b.x + 100, b.y + 38);
                assert_eq!(p.notes, [], "{encoder} {color:?}");
                let want = (b.x, b.y, b.width, b.height);
                assert_within_1(p.bbox.unwrap(), want, &format!("{encoder} {color:?}"));
            }
            // On the stroke of a glyph, and on the fill beside the label.
            let (x, y, w, h) = LABELED;
            let p = at(&f, x + 30, y + 22);
            assert_eq!(p.notes, [Note::OnMark], "{encoder}");
            assert_within_1(p.bbox.unwrap(), LABELED, encoder);
            let p = at(&f, x + w - 12, y + h / 2);
            assert_eq!(p.notes, [], "{encoder}");
            assert_within_1(p.bbox.unwrap(), LABELED, encoder);
        }
    }

    /// On a frame from a real encoder, a 3 to 8 pt dot and a count badge measure themselves
    /// within 1 px, not the page round them.
    #[test]
    fn a_point_measures_a_dot_and_a_badge_within_1_px_on_an_encoded_frame() {
        for encoder in ["x264", "vt"] {
            let f = encoded_ui(encoder);
            for (b, color) in ui_dots() {
                let what = format!("{encoder} {} px {color:?} at {},{}", b.width, b.x, b.y);
                let p = at(&f, b.x + b.width / 2, b.y + b.height / 2);
                assert_eq!(p.notes, [Note::SmallPart], "{what}");
                assert_within_1(p.bbox.unwrap(), (b.x, b.y, b.width, b.height), &what);
            }
            // On the red of a badge, left of its digit.
            for (x, y, d) in BADGES {
                let what = format!("{encoder} {d} px badge");
                let p = at(&f, x + d / 4, y + d / 2);
                assert_eq!(p.notes, [Note::SmallPart], "{what}");
                assert_within_1(p.bbox.unwrap(), (x, y, d, d), &what);
            }
        }
    }

    fn pointed(bbox: Option<PxBox>, notes: &[Note]) -> Pointed {
        Pointed {
            bbox,
            notes: notes.to_vec(),
        }
    }

    /// The point and the pixels close to it each give an answer. The point's own stands when
    /// enough of them give it too, and the output says so when they do not.
    #[test]
    fn a_point_whose_neighbors_disagree_says_so() {
        let button = Some(px_box(200, 700, 300, 80));
        // The same box within 1 px, as a movie's edges give.
        let nearly = Some(px_box(201, 700, 299, 81));
        let sliver = Some(px_box(374, 746, 5, 1));
        let fill = pointed(button, &[]);
        // Two of the four agree: the point is at an edge, and its box stands with no note.
        let others = [
            pointed(nearly, &[]),
            pointed(button, &[Note::OnMark]),
            pointed(None, &[Note::Page]),
            pointed(None, &[Note::Page]),
        ];
        assert_eq!(vote(fill.clone(), &others), fill);
        // One noisy pixel next to the point changes nothing.
        let others = [
            fill.clone(),
            fill.clone(),
            fill.clone(),
            pointed(sliver, &[Note::Lines]),
        ];
        assert_eq!(vote(fill.clone(), &others), fill);
        // The point alone gives a sliver: the box the others give, and what the point gave.
        let noisy = pointed(sliver, &[Note::OnMark, Note::Lines]);
        let others = [
            pointed(button, &[Note::OnMark]),
            fill.clone(),
            pointed(nearly, &[Note::OffFill]),
            pointed(None, &[Note::Thin]),
        ];
        assert_eq!(
            vote(noisy.clone(), &others),
            pointed(button, &[Note::Voted(Some((5, 1)))])
        );
        assert!(
            Note::Voted(Some((5, 1)))
                .says(Some(2.0))
                .contains("the point itself gave 2.5×0.5 pt (5×1 px)")
        );
        assert!(Note::Voted(None).says(None).contains("gave no box"));
        // The others are split: the point's own box, and the output says they differ.
        let others = [
            fill.clone(),
            fill.clone(),
            pointed(None, &[Note::Page]),
            pointed(None, &[Note::Page]),
        ];
        let p = vote(noisy.clone(), &others);
        assert_eq!((p.bbox, p.notes.last()), (sliver, Some(&Note::Edge)));
        // A point that stepped off a mark took the surface nearest to it. When one pixel
        // close by gives another box, the point is at an edge and the output says so.
        let stepped = pointed(button, &[Note::OnMark]);
        let others = [
            fill.clone(),
            fill.clone(),
            fill.clone(),
            pointed(None, &[Note::Page]),
        ];
        assert_eq!(
            vote(stepped.clone(), &others).notes,
            [Note::OnMark, Note::Edge]
        );
        let others = [fill.clone(), fill.clone(), fill.clone(), fill.clone()];
        assert_eq!(vote(stepped.clone(), &others), stepped);
        // At the image's corner there are two other pixels.
        assert_eq!(vote(fill.clone(), &others[..2]), fill);
    }

    /// The fill of a button between two letters of its label is as thin as a stroke there.
    /// It is the button's surface all the same, not a line with the page beside it.
    #[test]
    fn a_point_between_two_letters_of_a_label_measures_the_button() {
        const TEAL: [u8; 3] = [48, 110, 149];
        const WHITE: [u8; 3] = [0xFF, 0xFF, 0xFF];
        // A 96×24 px button at 0.85 px per pt, as a 6K screen records it. Its "letters" are
        // bars 2 px wide and 10 px tall, 2 px apart.
        let mut f = canvas(400, 200, WHITE);
        rect(&mut f, 100, 80, 96, 24, TEAL);
        for i in 0..12 {
            rect(&mut f, 120 + i * 4, 87, 2, 10, WHITE);
        }
        let button = px_box(100, 80, 96, 24);
        // Between two letters, and on the fill beside the label.
        for (x, y) in [(122, 92), (123, 90), (150, 95), (108, 92)] {
            let p = box_at(&f, x, y, 0.85);
            assert_eq!((p.bbox, p.notes), (Some(button), vec![]), "{x},{y}");
        }
        // On a letter: the button, and the output says the point was on text.
        let p = box_at(&f, 120, 92, 0.85);
        assert_eq!((p.bbox, p.notes), (Some(button), vec![Note::OnMark]));
    }

    /// A label fixture: its px per pt, its button, and a grid of 405 points over the label.
    struct Label {
        name: &'static str,
        ppp: f64,
        button: (u32, u32, u32, u32),
        /// The grid's first point, its step, and its columns and rows.
        grid: (u32, u32, u32),
        /// The VideoToolbox bit rate with the bits per pixel of a 2560×1440 capture at
        /// 3.4 Mbit/s.
        bitrate: &'static str,
    }

    /// `label-1x.png` is a Mac screen at 1 px per pt (a 6K screen records at 0.85), and
    /// `label-2x.png` and `label-3x.png` an iPhone's at 2 and 3. Each holds one button with
    /// "Load Docs" in 17 pt Helvetica on it: real text, which the frames drawn here do not
    /// hold.
    const LABELS: [Label; 2] = [
        Label {
            name: "label-1x",
            ppp: 1.0,
            button: (100, 350, 150, 40),
            grid: (131, 362, 2),
            bitrate: "850k",
        },
        Label {
            name: "label-2x",
            ppp: 2.0,
            button: (225, 600, 300, 80),
            grid: (287, 624, 4),
            bitrate: "1000k",
        },
    ];

    impl Label {
        fn grid(&self) -> impl Iterator<Item = (u32, u32)> {
            let (x, y, step) = self.grid;
            (0..9).flat_map(move |row| (0..45).map(move |col| (x + col * step, y + row * step)))
        }

        fn clean(&self) -> RgbaFrame {
            read_png(&fixture(&format!("{}.png", self.name))).unwrap()
        }

        /// The frame as a movie gives it back (see `encode_fixture`).
        fn encoded(&self, encoder: &str) -> RgbaFrame {
            let name = format!("{}-{encoder}.png", self.name);
            let source = fixture(&format!("{}.png", self.name));
            encode_fixture(&source, &name, encoder, self.bitrate);
            let f = read_png(&fixture(&name)).unwrap();
            let changed = changed(&f, &self.clean());
            assert!((100..20_000).contains(&changed), "{name}: {changed}");
            f
        }

        fn is_button(&self, p: &Pointed) -> bool {
            let (x, y, w, h) = self.button;
            p.bbox.is_some_and(|b| {
                close(b.x, x) && close(b.y, y) && close(b.width, w) && close(b.height, h)
            })
        }
    }

    /// On a movie frame, noise shuts a fill in between the strokes of a label and joins
    /// letters into words. At 1 px per pt the letters touch on a clean frame too, and their
    /// strokes are as near a near-white page as its own noise. A point anywhere on the label
    /// still measures the button. Where it cannot, it gives no box or says that the points
    /// close to it give other boxes: never a sliver, the inside of a letter or a word as if
    /// it were the component, and never the page.
    #[test]
    fn every_point_on_a_label_measures_the_button_or_says_it_is_unsure() {
        for label in &LABELS {
            // The clean frame, which no noise is on: every point gives the button.
            let clean = label.clean();
            for (x, y) in label.grid() {
                let p = box_at(&clean, x, y, label.ppp);
                assert!(label.is_button(&p), "{} {x},{y}: {p:?}", label.name);
                // At 1 px per pt a letter can be a small part of its own for one pixel: the
                // box is then the one the pixels close to it give.
                let fine = |n: &Note| matches!(n, Note::OnMark | Note::Edge | Note::Voted(_));
                assert!(
                    p.notes.iter().all(fine),
                    "{} {x},{y}: {:?}",
                    label.name,
                    p.notes
                );
            }
            for encoder in ["x264", "vt"] {
                let f = label.encoded(encoder);
                let what = format!("{} {encoder}", label.name);
                let (mut right, mut total) = (0, 0);
                for (x, y) in label.grid() {
                    let p = box_at(&f, x, y, label.ppp);
                    let unsure = match p.bbox {
                        Some(_) => p
                            .notes
                            .iter()
                            .any(|n| matches!(n, Note::Edge | Note::Voted(_) | Note::Mark)),
                        None => p.notes.contains(&Note::Thin),
                    };
                    assert!(label.is_button(&p) || unsure, "{what} {x},{y}: {p:?}");
                    (right, total) = (right + usize::from(label.is_button(&p)), total + 1);
                }
                assert_eq!(total, 405);
                assert!(right >= 401, "{what}: {right} of 405 give the button");
            }
        }
    }

    #[test]
    fn the_limits_of_a_capture_with_no_size_in_points_come_from_the_shape_of_its_frame() {
        for (size, ppp) in [
            // iPhones, upright and on their side.
            ((750.0, 1334.0), 2.0),
            ((828.0, 1792.0), 2.0),
            ((1080.0, 2340.0), 3.0),
            ((1179.0, 2556.0), 3.0),
            ((1320.0, 2868.0), 3.0),
            ((2868.0, 1320.0), 3.0),
            // A saved image fit to 1600 px.
            ((736.0, 1600.0), 2.0),
            // iPads.
            ((1488.0, 2266.0), 2.0),
            ((1640.0, 2360.0), 2.0),
            ((2064.0, 2752.0), 2.0),
            ((1200.0, 1600.0), 1200.0 / 834.0),
            ((700.0, 700.0), 1.0),
        ] {
            assert_eq!(estimated_ppp(size), ppp, "{size:?}");
        }
    }

    /// A cabled iPhone capture has no size in points. Its limits are in pt all the same, and
    /// they take px per pt from the frame's shape: at 1 px per pt on a 2x or 3x frame, a point
    /// on a label gave one letter and a region cut a word in two. Sizes stay in px.
    #[test]
    fn a_capture_with_no_size_in_points_measures_a_label_as_one_with_them_does() {
        let mut bundle = mac_bundle();
        bundle.target.mode = CaptureMode::Device;
        bundle.device = Some(DeviceInfo {
            width: 1179,
            height: 2556,
            ..Default::default()
        });
        assert_eq!(width_pt(&bundle), None);
        let frames = [
            ("label-2x.png".to_owned(), 2u32),
            ("label-2x-x264.png".to_owned(), 2),
            ("label-2x-vt.png".to_owned(), 2),
            ("label-3x.png".to_owned(), 3),
        ];
        for (name, s) in frames {
            let f = match name.as_str() {
                "label-2x-x264.png" => LABELS[1].encoded("x264"),
                "label-2x-vt.png" => LABELS[1].encoded("vt"),
                _ => read_png(&fixture(&name)).unwrap(),
            };
            let (w, h) = (f64::from(f.width), f64::from(f.height));
            let (bx, by) = if s == 2 { (225, 600) } else { (364, 1200) };
            let button = (bx, by, 150 * s, 40 * s);
            let ask = |query: Query| {
                measure_picture(
                    1,
                    picture(f.clone(), Source::Movie, FULL),
                    FULL,
                    None,
                    &query,
                )
            };
            // On the stem of the "L", in the hole of the first "o" and on the "D".
            let points = match s {
                2 => [(297, 640), (320, 641), (383, 638)],
                _ => [(473, 1255), (508, 1261), (599, 1255)],
            };
            for (x, y) in points {
                let m = ask(Query::At(
                    (f64::from(x) + 0.5) / w,
                    (f64::from(y) + 0.5) / h,
                ));
                let what = format!("{name} {x},{y}: {:?}", m.warnings);
                assert_eq!(
                    (m.px_per_pt, m.limits_px_per_pt),
                    (None, Some(f64::from(s)))
                );
                let b = m.component.expect(&what);
                assert_eq!(b.pt, None);
                let px = px_box(
                    b.px.x as u32,
                    b.px.y as u32,
                    b.px.width as u32,
                    b.px.height as u32,
                );
                assert_within_1(px, button, &what);
                assert!(
                    m.warnings[0].starts_with("the capture's size in points is unknown: px only")
                        && m.warnings[0].contains(&format!("used {s} px per pt, an estimate")),
                    "{what}"
                );
                assert!(m.warnings[1].starts_with("the point was on text"), "{what}");
            }
            // A region over the label: the two words and the space between them, in px.
            let (x0, y0, x1, y1) = (bx + 20 * s, by + 8 * s, bx + 130 * s, by + 32 * s);
            let m = ask(Query::Region([
                f64::from(x0) / w,
                f64::from(y0) / h,
                f64::from(x1) / w,
                f64::from(y1) / h,
            ]));
            let (boxes, gaps) = (m.boxes.unwrap(), m.gaps.unwrap());
            assert_eq!(boxes.len(), 2, "{name}: {boxes:?}");
            assert!(boxes.iter().all(|b| b.pt.is_none()));
            let space = if s == 2 { 15 } else { 21 };
            assert!(
                gaps.len() == 1 && gaps[0].pt.is_none() && gaps[0].px.abs_diff(space) <= 2,
                "{name}: {gaps:?}"
            );
        }
    }
}

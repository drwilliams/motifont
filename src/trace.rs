//! Bitmap tracing: turns a black-and-white scan (e.g. a 600 dpi TIFF) into
//! smooth vector outlines, for artwork that only exists as a scan.
//!
//! Steps: threshold → trace pixel boundaries into closed loops → smooth the
//! staircase → find corners → least-squares fit of a few cubic Béziers
//! between corners (Schneider's algorithm).

use crate::pdf::{Artwork, FillRule, Shape};
use anyhow::{Context, Result};
use kurbo::{BezPath, Point, Vec2};
use std::path::Path;

#[derive(Clone, Debug)]
pub struct TraceOptions {
    /// Pixels darker than this (0–255) are ink.
    pub threshold: u8,
    /// Treat light pixels as ink instead.
    pub invert: bool,
    /// Drop specks smaller than this many square pixels.
    pub despeckle: f64,
    /// Allowed deviation from the pixel outline, in pixels.
    pub tolerance: f64,
    /// Turns sharper than this many degrees stay as corners.
    pub corner_angle: f64,
}

impl Default for TraceOptions {
    fn default() -> Self {
        TraceOptions { threshold: 128, invert: false, despeckle: 16.0, tolerance: 0.5, corner_angle: 50.0 }
    }
}

pub fn load(path: &Path, opts: &TraceOptions) -> Result<Artwork> {
    let img = image::open(path).with_context(|| format!("reading image {}", path.display()))?.to_luma_alpha8();
    let (w, h) = img.dimensions();
    let ink: Vec<bool> = img
        .pixels()
        .map(|p| {
            let [l, a] = p.0;
            let dark = (l < opts.threshold) != opts.invert;
            a >= 128 && dark
        })
        .collect();
    let mut art = Artwork::default();
    let outline = trace_bitmap(&ink, w as usize, h as usize, opts);
    if outline.elements().is_empty() {
        anyhow::bail!("no artwork found in {} (try --threshold or --invert)", path.display());
    }
    art.shapes.push(Shape { path: outline, rule: FillRule::EvenOdd, white: false });
    Ok(art)
}

/// Directions on the pixel-corner grid (y down): 0 = +x, 1 = +y, 2 = -x, 3 = -y.
const DX: [i64; 4] = [1, 0, -1, 0];
const DY: [i64; 4] = [0, 1, 0, -1];

pub fn trace_bitmap(ink: &[bool], w: usize, h: usize, opts: &TraceOptions) -> BezPath {
    let at = |x: i64, y: i64| x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h && ink[y as usize * w + x as usize];
    let vw = w + 1;
    // outgoing boundary edges per grid vertex, as a bitmask of directions
    let mut out = vec![0u8; vw * (h + 1)];
    let vid = |x: i64, y: i64| y as usize * vw + x as usize;
    for y in 0..h as i64 {
        for x in 0..w as i64 {
            if !at(x, y) {
                continue;
            }
            // Walk around each ink pixel clockwise (on screen); edges only where the neighbour is empty.
            if !at(x, y - 1) {
                out[vid(x, y)] |= 1 << 0;
            }
            if !at(x + 1, y) {
                out[vid(x + 1, y)] |= 1 << 1;
            }
            if !at(x, y + 1) {
                out[vid(x + 1, y + 1)] |= 1 << 2;
            }
            if !at(x - 1, y) {
                out[vid(x, y + 1)] |= 1 << 3;
            }
        }
    }

    let mut result = BezPath::new();
    for start in 0..out.len() {
        while out[start] != 0 {
            let (sx, sy) = ((start % vw) as i64, (start / vw) as i64);
            let mut d = out[start].trailing_zeros() as usize;
            let (mut x, mut y) = (sx, sy);
            let mut loop_pts: Vec<Point> = Vec::new();
            loop {
                out[vid(x, y)] &= !(1 << d);
                loop_pts.push(Point::new(x as f64, y as f64));
                x += DX[d];
                y += DY[d];
                if x == sx && y == sy {
                    break;
                }
                // prefer turning right, then straight, then left (keeps diagonal pixels separate)
                let m = out[vid(x, y)];
                let Some(nd) = [(d + 1) % 4, d, (d + 3) % 4].into_iter().find(|nd| m & (1 << nd) != 0) else {
                    break;
                };
                d = nd;
            }
            if polygon_area(&loop_pts).abs() < opts.despeckle {
                continue;
            }
            // smooth the staircase, flip to y-up
            let pts: Vec<Point> = smooth_loop(&loop_pts).into_iter().map(|p| Point::new(p.x, h as f64 - p.y)).collect();
            let corners = find_corners(&pts, opts.corner_angle);
            fit_loop(&pts, &corners, opts.tolerance, &mut result);
        }
    }
    result
}

fn polygon_area(p: &[Point]) -> f64 {
    let n = p.len();
    (0..n).map(|i| p[i].x * p[(i + 1) % n].y - p[(i + 1) % n].x * p[i].y).sum::<f64>() / 2.0
}

/// Replace the staircase by edge midpoints, then a light moving average.
fn smooth_loop(p: &[Point]) -> Vec<Point> {
    let n = p.len();
    let mids: Vec<Point> = (0..n).map(|i| p[i].midpoint(p[(i + 1) % n])).collect();
    let k = 1i64; // window radius
    (0..n as i64)
        .map(|i| {
            let mut acc = Vec2::ZERO;
            for j in -k..=k {
                acc += mids[((i + j).rem_euclid(n as i64)) as usize].to_vec2();
            }
            (acc / (2 * k + 1) as f64).to_point()
        })
        .collect()
}

/// Indices where the outline turns sharply. The turn is measured over a few
/// pixels each side (so pixel jaggies don't count) and only the sharpest point
/// of each bend is kept.
fn find_corners(p: &[Point], corner_angle: f64) -> Vec<bool> {
    let n = p.len();
    let mut corner = vec![false; n];
    if n < 12 {
        return corner;
    }
    let k = 4usize;
    let turn: Vec<f64> = (0..n)
        .map(|i| {
            let a = p[i] - p[(i + n - k) % n];
            let b = p[(i + k) % n] - p[i];
            let (la, lb) = (a.hypot(), b.hypot());
            if la == 0.0 || lb == 0.0 {
                return 0.0;
            }
            (a.dot(b) / (la * lb)).clamp(-1.0, 1.0).acos().to_degrees()
        })
        .collect();
    for i in 0..n {
        if turn[i] < corner_angle {
            continue;
        }
        let is_max = (1..=k).all(|j| turn[i] >= turn[(i + j) % n] && turn[i] > turn[(i + n - j) % n]);
        corner[i] = is_max;
    }
    corner
}

/// Fit a closed loop of dense points with as few cubic Béziers as possible,
/// keeping sharp corners (Schneider's "An Algorithm for Automatically Fitting
/// Digitized Curves", Graphics Gems 1990).
fn fit_loop(p: &[Point], corner: &[bool], tolerance: f64, out: &mut BezPath) {
    let n = p.len();
    let mut breaks: Vec<usize> = (0..n).filter(|i| corner[*i]).collect();
    let smooth_breaks = breaks.len() < 2;
    if smooth_breaks {
        let first = breaks.first().copied().unwrap_or(0);
        breaks = vec![first, (first + n / 2) % n];
        breaks.sort();
    }
    // tangent at a break: one-sided at corners, central where the loop is smooth
    let tangent_out = |i: usize| -> Vec2 {
        let v = if corner[i] { p[(i + 3) % n] - p[i] } else { p[(i + 1) % n] - p[(i + n - 1) % n] };
        unit(v)
    };
    let tangent_in = |i: usize| -> Vec2 {
        let v = if corner[i] { p[(i + n - 3) % n] - p[i] } else { p[(i + n - 1) % n] - p[(i + 1) % n] };
        unit(v)
    };
    out.move_to(p[breaks[0]]);
    for b in 0..breaks.len() {
        let (from, to) = (breaks[b], breaks[(b + 1) % breaks.len()]);
        let len = (to + n - from) % n;
        let len = if len == 0 { n } else { len };
        let run: Vec<Point> = (0..=len).map(|k| p[(from + k) % n]).collect();
        fit_cubic(&run, tangent_out(from), tangent_in(to), tolerance * tolerance, out, 0);
    }
    out.close_path();
}

fn unit(v: Vec2) -> Vec2 {
    let l = v.hypot();
    if l > 0.0 { v / l } else { Vec2::new(1.0, 0.0) }
}

fn fit_cubic(d: &[Point], t1: Vec2, t2: Vec2, err2: f64, out: &mut BezPath, depth: u32) {
    let n = d.len();
    if n == 2 || depth > 40 {
        let dist = (d[n - 1] - d[0]).hypot() / 3.0;
        out.curve_to(d[0] + t1 * dist, d[n - 1] + t2 * dist, d[n - 1]);
        return;
    }
    let mut u = chord_params(d);
    let mut bez = generate_bezier(d, &u, t1, t2);
    let (mut max, mut split) = max_error(d, &bez, &u);
    if max < err2 {
        out.curve_to(bez[1], bez[2], bez[3]);
        return;
    }
    if max < err2 * 4.0 {
        for _ in 0..4 {
            u = reparameterize(d, &u, &bez);
            bez = generate_bezier(d, &u, t1, t2);
            (max, split) = max_error(d, &bez, &u);
            if max < err2 {
                out.curve_to(bez[1], bez[2], bez[3]);
                return;
            }
        }
    }
    let split = split.clamp(1, n - 2);
    let tc = unit(d[split - 1] - d[split + 1]);
    fit_cubic(&d[..=split], t1, tc, err2, out, depth + 1);
    fit_cubic(&d[split..], -tc, t2, err2, out, depth + 1);
}

fn chord_params(d: &[Point]) -> Vec<f64> {
    let mut u = vec![0.0; d.len()];
    for i in 1..d.len() {
        u[i] = u[i - 1] + (d[i] - d[i - 1]).hypot();
    }
    let total = *u.last().unwrap();
    if total > 0.0 {
        for x in &mut u {
            *x /= total;
        }
    }
    u
}

fn bernstein(t: f64) -> [f64; 4] {
    let s = 1.0 - t;
    [s * s * s, 3.0 * t * s * s, 3.0 * t * t * s, t * t * t]
}

fn bez_eval(b: &[Point; 4], t: f64) -> Point {
    let w = bernstein(t);
    (b[0].to_vec2() * w[0] + b[1].to_vec2() * w[1] + b[2].to_vec2() * w[2] + b[3].to_vec2() * w[3]).to_point()
}

/// Least-squares control points for fixed end tangents.
fn generate_bezier(d: &[Point], u: &[f64], t1: Vec2, t2: Vec2) -> [Point; 4] {
    let (first, last) = (d[0], d[d.len() - 1]);
    let (mut c00, mut c01, mut c11, mut x0, mut x1) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (i, &t) in u.iter().enumerate() {
        let w = bernstein(t);
        let a1 = t1 * w[1];
        let a2 = t2 * w[2];
        c00 += a1.dot(a1);
        c01 += a1.dot(a2);
        c11 += a2.dot(a2);
        let tmp = d[i].to_vec2() - (first.to_vec2() * (w[0] + w[1]) + last.to_vec2() * (w[2] + w[3]));
        x0 += a1.dot(tmp);
        x1 += a2.dot(tmp);
    }
    let det = c00 * c11 - c01 * c01;
    let seg_len = (last - first).hypot();
    let eps = 1e-6 * seg_len;
    let (mut al, mut ar) = if det.abs() > 1e-12 { ((x0 * c11 - x1 * c01) / det, (c00 * x1 - c01 * x0) / det) } else { (0.0, 0.0) };
    if al < eps || ar < eps {
        al = seg_len / 3.0;
        ar = al;
    }
    [first, first + t1 * al, last + t2 * ar, last]
}

fn max_error(d: &[Point], b: &[Point; 4], u: &[f64]) -> (f64, usize) {
    let mut max = 0.0;
    let mut split = d.len() / 2;
    for i in 1..d.len() - 1 {
        let e = (bez_eval(b, u[i]) - d[i]).hypot2();
        if e >= max {
            max = e;
            split = i;
        }
    }
    (max, split)
}

/// One Newton–Raphson step per point to improve the parameter values.
fn reparameterize(d: &[Point], u: &[f64], b: &[Point; 4]) -> Vec<f64> {
    let q1 = [(b[1] - b[0]) * 3.0, (b[2] - b[1]) * 3.0, (b[3] - b[2]) * 3.0];
    let q2 = [(q1[1] - q1[0]) * 2.0, (q1[2] - q1[1]) * 2.0];
    d.iter()
        .zip(u)
        .map(|(p, &t)| {
            let s = 1.0 - t;
            let q = bez_eval(b, t) - *p;
            let d1 = q1[0] * (s * s) + q1[1] * (2.0 * s * t) + q1[2] * (t * t);
            let d2 = q2[0] * s + q2[1] * t;
            let den = d1.dot(d1) + q.dot(d2);
            if den.abs() < 1e-12 { t } else { (t - q.dot(d1) / den).clamp(0.0, 1.0) }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurbo::ParamCurve;

    /// Max distance from the traced outline to a circle of radius r centred at c.
    fn circle_error(path: &BezPath, c: Point, r: f64) -> f64 {
        let mut worst: f64 = 0.0;
        for seg in path.segments() {
            for k in 0..=20 {
                let p = seg.eval(k as f64 / 20.0);
                worst = worst.max(((p - c).hypot() - r).abs());
            }
        }
        worst
    }

    #[test]
    fn traces_circle_accurately() {
        let (w, h) = (300usize, 300usize);
        let (cx, cy, r) = (150.0, 150.0, 100.0);
        let ink: Vec<bool> = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f64 + 0.5, (i / w) as f64 + 0.5);
                ((x - cx).powi(2) + (y - cy).powi(2)).sqrt() <= r
            })
            .collect();
        let path = trace_bitmap(&ink, w, h, &TraceOptions::default());
        let err = circle_error(&path, Point::new(cx, h as f64 - cy), r);
        let n = path.elements().len();
        eprintln!("circle: max error {err:.3}px, {n} elements");
        assert!(err < 1.0, "max error {err}");
    }

    #[test]
    fn keeps_square_corners() {
        let (w, h) = (100usize, 100usize);
        let ink: Vec<bool> = (0..w * h).map(|i| (20..80).contains(&(i % w)) && (20..80).contains(&(i / w))).collect();
        let path = trace_bitmap(&ink, w, h, &TraceOptions::default());
        let bb = kurbo::Shape::bounding_box(&path);
        eprintln!("square bbox {bb:?}, {} elements", path.elements().len());
        assert!((bb.x0 - 20.0).abs() < 1.0 && (bb.x1 - 80.0).abs() < 1.0);
    }
}


//! Turns extracted PDF shapes into clean TrueType-ready glyph outlines:
//! correct contour directions, scaled and positioned into the em square,
//! cubic curves converted to quadratics.

use crate::pdf::{Artwork, FillRule};
use kurbo::{BezPath, CubicBez, PathEl, Point, Rect, Shape};

/// How the artwork is fitted into the glyph.
#[derive(Clone, Debug)]
pub struct Placement {
    /// Maximum artwork width in font units.
    pub max_width: f64,
    /// Maximum artwork height in font units.
    pub max_height: f64,
    /// Y position of the bottom of the artwork (0 = sits on the baseline).
    pub bottom: f64,
    /// Space left of the artwork.
    pub lsb: f64,
    /// Space right of the artwork.
    pub rsb: f64,
    /// Fixed advance width; when None it's lsb + artwork width + rsb.
    pub advance: Option<f64>,
}

impl Default for Placement {
    fn default() -> Self {
        Placement { max_width: 1000.0, max_height: 1000.0, bottom: 0.0, lsb: 0.0, rsb: 0.0, advance: None }
    }
}

pub struct GlyphOutline {
    /// Quadratic-only path in font units, TrueType contour directions.
    pub path: BezPath,
    pub advance: u16,
    /// More than one painted shape, so contours may overlap.
    pub overlaps: bool,
}

/// Split a path into its subpaths.
fn contours(path: &BezPath) -> Vec<BezPath> {
    let mut out = Vec::new();
    let mut cur = BezPath::new();
    for el in path.elements() {
        if matches!(el, PathEl::MoveTo(_)) && !cur.elements().is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(*el);
    }
    if !cur.elements().is_empty() {
        out.push(cur);
    }
    out
}

fn start_point(c: &BezPath) -> Point {
    match c.elements().first() {
        Some(PathEl::MoveTo(p)) => *p,
        _ => Point::ZERO,
    }
}

/// Classify each contour of one filled shape as outer boundary, hole, or
/// redundant (doesn't change the filled area), and orient it the TrueType
/// way: outer contours clockwise, holes counter-clockwise (y up). In kurbo's
/// convention clockwise-in-y-up means negative area.
fn orient_shape(path: &BezPath, rule: FillRule, out: &mut Vec<BezPath>) {
    let cs: Vec<BezPath> = contours(path).into_iter().filter(|c| c.area().abs() > 1e-6).collect();
    for (i, c) in cs.iter().enumerate() {
        let p = start_point(c);
        let others: i32 = cs.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, o)| o.winding(p)).sum();
        let own = if c.area() > 0.0 { 1 } else { -1 };
        let is_outer = match rule {
            FillRule::EvenOdd => {
                let depth: i32 = cs.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, o)| (o.winding(p) != 0) as i32).sum();
                depth % 2 == 0
            }
            FillRule::NonZero => {
                let outside = others;
                let inside = others + own;
                match (outside != 0, inside != 0) {
                    (false, true) => true,
                    (true, false) => false,
                    _ => continue, // contour lies inside filled area on both sides: drop it
                }
            }
        };
        let clockwise = c.area() < 0.0;
        if is_outer == clockwise {
            out.push(c.clone());
        } else {
            out.push(c.reverse_subpaths());
        }
    }
}

/// Merge all painted shapes, in paint order, into one set of non-overlapping
/// contours: dark shapes are unioned in, white (knockout) shapes are cut out.
/// Returns None if the boolean engine fails, so the caller can fall back.
fn merge(art: &Artwork) -> Option<Vec<BezPath>> {
    use linesweeper::{binary_op, BinaryOp, FillRule as LsRule};
    let mut acc: Option<BezPath> = None;
    // Group consecutive shapes of the same colour so we do as few operations as possible.
    let mut i = 0;
    while i < art.shapes.len() {
        let white = art.shapes[i].white;
        let mut group = BezPath::new();
        while i < art.shapes.len() && art.shapes[i].white == white {
            let mut cs = Vec::new();
            orient_shape(&art.shapes[i].path, art.shapes[i].rule, &mut cs);
            for c in cs {
                group.extend(c);
            }
            i += 1;
        }
        if group.elements().is_empty() {
            continue;
        }
        let result = match (&acc, white) {
            (None, true) => continue, // white on nothing
            (None, false) => binary_op(&group, &BezPath::new(), LsRule::NonZero, BinaryOp::Union),
            (Some(a), false) => binary_op(a, &group, LsRule::NonZero, BinaryOp::Union),
            (Some(a), true) => binary_op(a, &group, LsRule::NonZero, BinaryOp::Difference),
        }
        .ok()?;
        let mut merged = BezPath::new();
        for c in result.contours() {
            // TrueType: outer contours clockwise (negative kurbo area), holes counter-clockwise.
            let clockwise = c.path.area() < 0.0;
            if c.outer == clockwise {
                merged.extend(c.path.iter());
            } else {
                merged.extend(c.path.reverse_subpaths().iter());
            }
        }
        acc = Some(merged);
    }
    Some(contours(&acc?))
}

/// Simple fallback: orient each shape on its own and keep overlaps.
fn orient_all(art: &Artwork, warnings: &mut Vec<String>) -> Vec<BezPath> {
    let mut oriented = Vec::new();
    let white = art.shapes.iter().filter(|s| s.white).count();
    for s in art.shapes.iter().filter(|s| !s.white) {
        orient_shape(&s.path, s.rule, &mut oriented);
    }
    if white > 0 {
        warnings.push(format!(
            "{white} white (knockout) shape(s) were ignored; if the design relies on white shapes to cut holes, \
             merge them in Illustrator (Pathfinder > Minus Front) or use the bitmap workflow"
        ));
    }
    oriented
}

/// Produce the glyph outline for an artwork. Returns warnings alongside.
pub fn build(art: &Artwork, placement: &Placement) -> (GlyphOutline, Vec<String>) {
    let mut warnings = Vec::new();
    let (oriented, overlaps) = match merge(art) {
        Some(c) if !c.is_empty() => (c, false),
        _ => {
            if art.shapes.len() > 1 {
                warnings.push("could not merge overlapping shapes; contours are kept as drawn".into());
            }
            (orient_all(art, &mut warnings), art.shapes.len() > 1)
        }
    };

    let bbox = oriented.iter().map(|c| c.bounding_box()).reduce(|a, b| a.union(b)).unwrap_or(Rect::ZERO);
    let (w, h) = (bbox.width().max(1e-9), bbox.height().max(1e-9));
    let scale = (placement.max_width / w).min(placement.max_height / h);
    let tx = placement.lsb - bbox.x0 * scale;
    let ty = placement.bottom - bbox.y0 * scale;
    let xf = kurbo::Affine::new([scale, 0.0, 0.0, scale, tx, ty]);

    let mut path = BezPath::new();
    for c in &oriented {
        let mut c = c.clone();
        c.apply_affine(xf);
        // Contours smaller than ~1 unit² vanish when rounded to integers.
        if c.area().abs() < 1.0 {
            continue;
        }
        append_quadratic(&c, &mut path);
    }

    let advance = placement.advance.unwrap_or(placement.lsb + w * scale + placement.rsb);
    (GlyphOutline { path, advance: advance.round().clamp(0.0, 65535.0) as u16, overlaps }, warnings)
}

/// Append a contour converting cubics to quadratics (TrueType only has quadratics).
fn append_quadratic(c: &BezPath, out: &mut BezPath) {
    const ACCURACY: f64 = 0.2; // font units
    let mut cur = Point::ZERO;
    let mut start = Point::ZERO;
    for el in c.elements() {
        match *el {
            PathEl::MoveTo(p) => {
                out.move_to(p);
                cur = p;
                start = p;
            }
            PathEl::LineTo(p) => {
                if p != cur {
                    out.line_to(p);
                }
                cur = p;
            }
            PathEl::QuadTo(p1, p2) => {
                out.quad_to(p1, p2);
                cur = p2;
            }
            PathEl::CurveTo(p1, p2, p3) => {
                for (_, _, q) in CubicBez::new(cur, p1, p2, p3).to_quads(ACCURACY) {
                    out.quad_to(q.p1, q.p2);
                }
                cur = p3;
            }
            PathEl::ClosePath => {
                // drop an explicit final line back to the start; glyf contours close implicitly
                let els = out.elements();
                if cur == start
                    && matches!(els.last(), Some(PathEl::LineTo(_)))
                    && els.len() > 1
                    && !matches!(els[els.len() - 2], PathEl::MoveTo(_))
                {
                    out.pop();
                }
                out.close_path();
                cur = start;
            }
        }
    }
}

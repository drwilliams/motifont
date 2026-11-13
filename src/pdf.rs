//! Extracts filled vector artwork from a PDF page.
//!
//! This is a small PDF content-stream interpreter that only cares about
//! geometry: path construction, the transformation matrix, fills, strokes,
//! fill colour (to spot white "knockout" shapes) and Form XObjects.
//! Text and raster images are reported as warnings because they can't be
//! turned into outlines here.

use anyhow::{anyhow, bail, Context, Result};
use kurbo::{Affine, BezPath, Cap, Join, Point, Stroke, StrokeOpts};
use lopdf::{content::Content, Dictionary, Document, Object};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FillRule {
    NonZero,
    EvenOdd,
}

/// One painted shape, already in page coordinates (points, y up).
#[derive(Clone, Debug)]
pub struct Shape {
    pub path: BezPath,
    pub rule: FillRule,
    /// True when painted in white / no ink (a knockout).
    pub white: bool,
}

#[derive(Default, Debug)]
pub struct Artwork {
    pub shapes: Vec<Shape>,
    pub warnings: Vec<String>,
}

#[derive(Clone)]
struct GState {
    ctm: Affine,
    fill_white: bool,
    stroke_white: bool,
    line_width: f64,
    cap: Cap,
    join: Join,
    miter: f64,
}

impl Default for GState {
    fn default() -> Self {
        GState {
            ctm: Affine::IDENTITY,
            fill_white: false,
            stroke_white: false,
            line_width: 1.0,
            cap: Cap::Butt,
            join: Join::Miter,
            miter: 10.0,
        }
    }
}

pub fn load(path: &Path, page_number: u32) -> Result<Artwork> {
    let doc = Document::load(path).with_context(|| format!("reading PDF {}", path.display()))?;
    let pages = doc.get_pages();
    let page_id = *pages
        .get(&page_number)
        .ok_or_else(|| anyhow!("{} has no page {}", path.display(), page_number))?;
    let content = doc.get_page_content(page_id);
    let resources = page_resources(&doc, page_id);

    let mut art = Artwork::default();
    let mut interp = Interp { doc: &doc, art: &mut art, depth: 0 };
    interp.run(&content, resources.as_ref(), GState::default())?;
    if art.shapes.is_empty() {
        bail!(
            "no vector shapes found on page {} of {}{}",
            page_number,
            path.display(),
            if art.warnings.is_empty() { String::new() } else { format!(" ({})", art.warnings.join("; ")) }
        );
    }
    Ok(art)
}

fn page_resources(doc: &Document, page_id: lopdf::ObjectId) -> Option<Dictionary> {
    let (direct, ids) = doc.get_page_resources(page_id).ok()?;
    if let Some(d) = direct {
        return Some(d.clone());
    }
    ids.first().and_then(|id| doc.get_dictionary(*id).ok()).cloned()
}

struct Interp<'a> {
    doc: &'a Document,
    art: &'a mut Artwork,
    depth: u32,
}

fn num(o: &Object) -> f64 {
    match o {
        Object::Integer(i) => *i as f64,
        Object::Real(r) => *r as f64,
        _ => 0.0,
    }
}

fn nums(ops: &[Object]) -> Vec<f64> {
    ops.iter().map(num).collect()
}

/// Decide whether a colour (given as raw components) is effectively white.
fn is_white(comps: &[f64], cmyk_like: bool) -> bool {
    match comps.len() {
        1 if cmyk_like => comps[0] < 0.02, // separation tint: 0 = no ink
        1 => comps[0] > 0.98,
        3 => comps.iter().all(|c| *c > 0.98),
        4 => comps.iter().all(|c| *c < 0.02),
        _ => false,
    }
}

impl Interp<'_> {
    fn warn(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if !self.art.warnings.contains(&msg) {
            self.art.warnings.push(msg);
        }
    }

    fn run(&mut self, content: &[u8], resources: Option<&Dictionary>, start: GState) -> Result<()> {
        let content = Content::decode(content).context("decoding PDF content stream")?;
        let mut gs = start;
        let mut stack: Vec<GState> = Vec::new();
        let mut path = BezPath::new();
        let mut current = Point::ZERO;
        let mut subpath_start = Point::ZERO;
        // Colour spaces set with cs/CS: true when the space is "ink-like" (separation/DeviceN)
        let mut fill_ink_space = false;
        let mut stroke_ink_space = false;

        for op in &content.operations {
            let a = &op.operands;
            let n = nums(a);
            let get = |i: usize| n.get(i).copied().unwrap_or(0.0);
            match op.operator.as_str() {
                "q" => stack.push(gs.clone()),
                "Q" => {
                    if let Some(g) = stack.pop() {
                        gs = g
                    }
                }
                "cm" if n.len() == 6 => {
                    gs.ctm *= Affine::new([get(0), get(1), get(2), get(3), get(4), get(5)]);
                }
                "w" => gs.line_width = get(0),
                "J" => {
                    gs.cap = match get(0) as i32 {
                        1 => Cap::Round,
                        2 => Cap::Square,
                        _ => Cap::Butt,
                    }
                }
                "j" => {
                    gs.join = match get(0) as i32 {
                        1 => Join::Round,
                        2 => Join::Bevel,
                        _ => Join::Miter,
                    }
                }
                "M" => gs.miter = get(0),

                // colour
                "g" => gs.fill_white = is_white(&n, false),
                "G" => gs.stroke_white = is_white(&n, false),
                "rg" => gs.fill_white = is_white(&n, false),
                "RG" => gs.stroke_white = is_white(&n, false),
                "k" => gs.fill_white = is_white(&n, false),
                "K" => gs.stroke_white = is_white(&n, false),
                "cs" => fill_ink_space = !matches!(a.first(), Some(Object::Name(nm)) if nm == b"DeviceGray" || nm == b"DeviceRGB" || nm == b"CalRGB" || nm == b"CalGray"),
                "CS" => stroke_ink_space = !matches!(a.first(), Some(Object::Name(nm)) if nm == b"DeviceGray" || nm == b"DeviceRGB" || nm == b"CalRGB" || nm == b"CalGray"),
                "sc" | "scn" => {
                    let c: Vec<f64> = a.iter().filter(|o| matches!(o, Object::Integer(_) | Object::Real(_))).map(num).collect();
                    gs.fill_white = is_white(&c, fill_ink_space);
                }
                "SC" | "SCN" => {
                    let c: Vec<f64> = a.iter().filter(|o| matches!(o, Object::Integer(_) | Object::Real(_))).map(num).collect();
                    gs.stroke_white = is_white(&c, stroke_ink_space);
                }

                // path construction (coordinates are in user space; we apply the CTM at paint time)
                "m" => {
                    current = Point::new(get(0), get(1));
                    subpath_start = current;
                    path.move_to(current);
                }
                "l" => {
                    current = Point::new(get(0), get(1));
                    ensure_started(&mut path, subpath_start);
                    path.line_to(current);
                }
                "c" => {
                    ensure_started(&mut path, subpath_start);
                    current = Point::new(get(4), get(5));
                    path.curve_to(Point::new(get(0), get(1)), Point::new(get(2), get(3)), current);
                }
                "v" => {
                    ensure_started(&mut path, subpath_start);
                    let p1 = current;
                    current = Point::new(get(2), get(3));
                    path.curve_to(p1, Point::new(get(0), get(1)), current);
                }
                "y" => {
                    ensure_started(&mut path, subpath_start);
                    current = Point::new(get(2), get(3));
                    path.curve_to(Point::new(get(0), get(1)), current, current);
                }
                "h" => {
                    path.close_path();
                    current = subpath_start;
                }
                "re" => {
                    let (x, y, w, h) = (get(0), get(1), get(2), get(3));
                    path.move_to((x, y));
                    path.line_to((x + w, y));
                    path.line_to((x + w, y + h));
                    path.line_to((x, y + h));
                    path.close_path();
                    current = Point::new(x, y);
                    subpath_start = current;
                }

                // painting
                "f" | "F" | "f*" | "B" | "B*" | "b" | "b*" | "S" | "s" | "n" => {
                    let o = op.operator.as_str();
                    if matches!(o, "b" | "b*" | "s") {
                        path.close_path();
                    }
                    let fill = matches!(o, "f" | "F" | "f*" | "B" | "B*" | "b" | "b*");
                    let stroke = matches!(o, "B" | "B*" | "b" | "b*" | "S" | "s");
                    let rule = if o.ends_with('*') { FillRule::EvenOdd } else { FillRule::NonZero };
                    if fill && !path.elements().is_empty() {
                        let mut p = path.clone();
                        close_all(&mut p);
                        p.apply_affine(gs.ctm);
                        self.art.shapes.push(Shape { path: p, rule, white: gs.fill_white });
                    }
                    if stroke && !path.elements().is_empty() && gs.line_width > 0.0 {
                        self.stroke(&path, &gs);
                    }
                    path = BezPath::new();
                }
                "W" | "W*" => {} // clipping is ignored; artwork outside a clip is rare in motif files

                "Do" => {
                    if let Some(Object::Name(name)) = a.first() {
                        self.do_xobject(name, resources, &gs)?;
                    }
                }
                "BT" => self.warn("PDF contains live text; convert text to outlines in Illustrator (Type > Create Outlines)"),
                "BI" => self.warn("PDF contains an embedded raster image which is ignored; use the bitmap tracing workflow"),
                "sh" => self.warn("PDF contains a gradient/shading which is ignored"),
                _ => {}
            }
        }
        Ok(())
    }

    fn stroke(&mut self, path: &BezPath, gs: &GState) {
        // Stroke in user space, then transform. For non-uniform CTMs this is an
        // approximation, but artwork from Illustrator is uniformly scaled.
        let style = Stroke::new(gs.line_width)
            .with_caps(gs.cap)
            .with_join(gs.join)
            .with_miter_limit(gs.miter);
        let mut outline = kurbo::stroke(path.iter(), &style, &StrokeOpts::default(), 0.01);
        outline.apply_affine(gs.ctm);
        self.art.shapes.push(Shape { path: outline, rule: FillRule::NonZero, white: gs.stroke_white });
    }

    fn do_xobject(&mut self, name: &[u8], resources: Option<&Dictionary>, gs: &GState) -> Result<()> {
        let Some(res) = resources else { return Ok(()) };
        let Ok(xobjs) = res.get_deref(b"XObject", self.doc).and_then(Object::as_dict) else { return Ok(()) };
        let Ok(obj) = xobjs.get_deref(name, self.doc) else { return Ok(()) };
        let Ok(stream) = obj.as_stream() else { return Ok(()) };
        let subtype = stream.dict.get(b"Subtype").and_then(Object::as_name).unwrap_or(b"");
        match subtype {
            b"Form" => {
                if self.depth > 16 {
                    self.warn("Form XObjects nested too deeply; some artwork skipped");
                    return Ok(());
                }
                let mut child = gs.clone();
                if let Ok(m) = stream.dict.get(b"Matrix").and_then(Object::as_array) {
                    let m = nums(m);
                    if m.len() == 6 {
                        child.ctm *= Affine::new([m[0], m[1], m[2], m[3], m[4], m[5]]);
                    }
                }
                let data = stream.decompressed_content().unwrap_or_else(|_| stream.content.clone());
                let child_res = stream
                    .dict
                    .get_deref(b"Resources", self.doc)
                    .and_then(Object::as_dict)
                    .ok()
                    .cloned();
                self.depth += 1;
                let r = self.run(&data, child_res.as_ref().or(resources), child);
                self.depth -= 1;
                r?;
            }
            b"Image" => self.warn("PDF contains a raster image which is ignored; use the bitmap tracing workflow"),
            _ => {}
        }
        Ok(())
    }
}

fn ensure_started(path: &mut BezPath, start: Point) {
    if path.elements().is_empty() {
        path.move_to(start);
    }
}

/// Fills implicitly close every subpath.
fn close_all(path: &mut BezPath) {
    use kurbo::PathEl;
    let mut out = BezPath::new();
    let mut open = false;
    for el in path.elements() {
        match el {
            PathEl::MoveTo(_) => {
                if open {
                    out.close_path();
                }
                open = true;
            }
            PathEl::ClosePath => open = false,
            _ => {}
        }
        out.push(*el);
    }
    if open {
        out.close_path();
    }
    *path = out;
}

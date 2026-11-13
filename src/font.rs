//! Writes a minimal, valid TrueType font that holds the motif glyphs.
//! Defaults: 1000 UPM, ascender 750 / descender -250, Regular,
//! fsType 0 (no embedding restrictions).

use crate::outline::GlyphOutline;
use anyhow::{bail, Result};
use write_fonts::{
    tables::{
        cmap::Cmap,
        glyf::{GlyfLocaBuilder, SimpleGlyph},
        head::{Flags, Head, MacStyle},
        hhea::Hhea,
        hmtx::{Hmtx, LongMetric},
        maxp::Maxp,
        name::{Name, NameRecord},
        os2::{Os2, SelectionFlags},
        post::Post,
    },
    types::{FWord, Fixed, GlyphId, LongDateTime, NameId, Tag, UfWord},
    FontBuilder, OffsetMarker,
};

pub struct FontInfo {
    pub family: String,
    pub vendor: String,
    pub version: String,
}

pub struct GlyphEntry {
    pub chars: Vec<char>,
    pub outline: GlyphOutline,
}

const UPM: u16 = 1000;
const ASCENDER: i16 = 750;
const DESCENDER: i16 = -250;

fn glyph_name(c: char) -> String {
    if c.is_ascii_alphanumeric() {
        c.to_string()
    } else if (c as u32) <= 0xFFFF {
        format!("uni{:04X}", c as u32)
    } else {
        format!("u{:05X}", c as u32)
    }
}

/// PostScript names: printable ASCII, no spaces or []{}()<>/%, max 63 chars.
pub fn ps_name(family: &str) -> String {
    let s: String = family
        .chars()
        .filter(|c| c.is_ascii_graphic() && !"[](){}<>/%".contains(*c))
        .take(63)
        .collect();
    if s.is_empty() { "Motif".into() } else { s }
}

pub fn build(info: &FontInfo, entries: &[GlyphEntry]) -> Result<Vec<u8>> {
    // Glyph order: .notdef, then one glyph per mapped character (when a design
    // goes in both "A" and "a", each gets its own copy for maximum compatibility).
    let mut names: Vec<String> = vec![".notdef".into()];
    let mut glyphs: Vec<SimpleGlyph> = vec![SimpleGlyph::default()];
    let mut advances: Vec<u16> = vec![500];
    let mut mappings: Vec<(char, GlyphId)> = Vec::new();

    for e in entries {
        let simple = SimpleGlyph::from_bezpath(&e.outline.path).map_err(|e| anyhow::anyhow!("converting outline to TrueType glyph: {e:?}"))?;
        let mut simple = simple;
        simple.overlaps = e.outline.overlaps;
        for &c in &e.chars {
            if mappings.iter().any(|(m, _)| *m == c) {
                bail!("character '{c}' is assigned more than one design");
            }
            mappings.push((c, GlyphId::new(names.len() as u32)));
            names.push(glyph_name(c));
            glyphs.push(simple.clone());
            advances.push(e.outline.advance);
        }
    }

    let mut glyf_builder = GlyfLocaBuilder::new();
    let (mut x_min, mut y_min, mut x_max, mut y_max) = (i16::MAX, i16::MAX, i16::MIN, i16::MIN);
    let mut lsbs = Vec::new();
    let (mut max_points, mut max_contours) = (0u16, 0u16);
    let mut min_rsb = i16::MAX;
    let mut max_extent = i16::MIN;
    for (g, adv) in glyphs.iter().zip(&advances) {
        glyf_builder.add_glyph(g)?;
        if g.contours.is_empty() {
            lsbs.push(0);
            continue;
        }
        let b = g.bbox;
        x_min = x_min.min(b.x_min);
        y_min = y_min.min(b.y_min);
        x_max = x_max.max(b.x_max);
        y_max = y_max.max(b.y_max);
        lsbs.push(b.x_min);
        min_rsb = min_rsb.min(*adv as i16 - b.x_max);
        max_extent = max_extent.max(b.x_max);
        max_points = max_points.max(g.contours.iter().map(|c| c.len() as u16).sum());
        max_contours = max_contours.max(g.contours.len() as u16);
    }
    if x_min == i16::MAX {
        bail!("font has no outlines");
    }
    let (glyf, loca, loca_format) = glyf_builder.build();

    let now = chrono::Utc::now();
    let secs_1904 = now.timestamp() + 2_082_844_800;

    let head = Head {
        font_revision: Fixed::from_f64(1.0),
        flags: Flags::from_bits_truncate(0x000B),
        units_per_em: UPM,
        created: LongDateTime::new(secs_1904),
        modified: LongDateTime::new(secs_1904),
        x_min,
        y_min,
        x_max,
        y_max,
        mac_style: MacStyle::empty(),
        lowest_rec_ppem: 9,
        index_to_loc_format: loca_format as i16,
        ..Default::default()
    };

    let adv_max = *advances.iter().max().unwrap();
    let hhea = Hhea::new(
        FWord::new(ASCENDER),
        FWord::new(DESCENDER),
        FWord::new(0),
        UfWord::new(adv_max),
        FWord::new(lsbs.iter().skip(1).copied().min().unwrap_or(0)),
        FWord::new(min_rsb),
        FWord::new(max_extent),
        1,
        0,
        0,
        advances.len() as u16,
    );

    let hmtx = Hmtx::new(
        advances.iter().zip(&lsbs).map(|(a, l)| LongMetric::new(*a, *l)).collect(),
        vec![],
    );

    let maxp = Maxp {
        num_glyphs: names.len() as u16,
        max_points: Some(max_points),
        max_contours: Some(max_contours),
        max_composite_points: Some(0),
        max_composite_contours: Some(0),
        max_zones: Some(2),
        max_twilight_points: Some(0),
        max_storage: Some(0),
        max_function_defs: Some(0),
        max_instruction_defs: Some(0),
        max_stack_elements: Some(0),
        max_size_of_instructions: Some(0),
        max_component_elements: Some(0),
        max_component_depth: Some(0),
    };

    let chars: Vec<u32> = mappings.iter().map(|(c, _)| *c as u32).collect();
    let marked: Vec<u16> = advances.iter().skip(1).copied().collect();
    let avg = marked.iter().map(|a| *a as u32).sum::<u32>() / marked.len().max(1) as u32;
    let all_ascii = chars.iter().all(|c| *c < 0x80);
    let vendor: String = format!("{:<4}", info.vendor).chars().take(4).collect();
    let os2 = Os2 {
        x_avg_char_width: avg as i16,
        us_weight_class: 400,
        us_width_class: 5,
        fs_type: 0, // installable: no embedding restrictions
        y_subscript_x_size: 650,
        y_subscript_y_size: 600,
        y_subscript_y_offset: 75,
        y_superscript_x_size: 650,
        y_superscript_y_size: 600,
        y_superscript_y_offset: 350,
        y_strikeout_size: 50,
        y_strikeout_position: 300,
        ul_unicode_range_1: if all_ascii { 1 } else { 0 },
        ach_vend_id: Tag::new_checked(vendor.as_bytes()).unwrap_or(Tag::new(b"NONE")),
        fs_selection: SelectionFlags::REGULAR,
        us_first_char_index: (*chars.iter().min().unwrap()).min(0xFFFF) as u16,
        us_last_char_index: (*chars.iter().max().unwrap()).min(0xFFFF) as u16,
        s_typo_ascender: ASCENDER,
        s_typo_descender: DESCENDER,
        s_typo_line_gap: 0,
        // Windows clips anything outside win ascent/descent, so cover the artwork.
        us_win_ascent: (y_max.max(891)) as u16,
        us_win_descent: ((-y_min).max(147)) as u16,
        ul_code_page_range_1: Some(1),
        ul_code_page_range_2: Some(0),
        sx_height: Some(500),
        s_cap_height: Some(700),
        us_default_char: Some(0),
        us_break_char: Some(32),
        us_max_context: Some(0),
        ..Default::default()
    };

    let post = Post::new_v2(names.iter().map(|s| s.as_str()));
    let post = Post {
        underline_position: FWord::new(-75),
        underline_thickness: FWord::new(50),
        ..post
    };

    let cmap = Cmap::from_mappings(mappings.iter().copied()).map_err(|e| anyhow::anyhow!("cmap: {e:?}"))?;

    let ps = ps_name(&info.family);
    let unique = format!("{} Regular : {}", info.family, now.format("%Y-%m-%d"));
    let version = format!("Version {}", info.version);
    let entries: [(u16, &str); 6] = [
        (1, &info.family),
        (2, "Regular"),
        (3, &unique),
        (4, &info.family),
        (5, &version),
        (6, &ps),
    ];
    let mut records: Vec<NameRecord> = entries
        .iter()
        .map(|(id, s)| NameRecord::new(3, 1, 0x409, NameId::new(*id), OffsetMarker::new(s.to_string())))
        .collect();
    records.sort();
    let name = Name::new(records);

    let mut fb = FontBuilder::new();
    fb.add_table(&head)?
        .add_table(&hhea)?
        .add_table(&maxp)?
        .add_table(&os2)?
        .add_table(&hmtx)?
        .add_table(&cmap)?
        .add_table(&loca)?
        .add_table(&glyf)?
        .add_table(&name)?
        .add_table(&post)?;
    Ok(fb.build())
}

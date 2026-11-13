mod font;
mod outline;
mod pdf;
mod trace;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::path::{Path, PathBuf};

/// Build motif fonts (TrueType) from PDF artwork or scanned images.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build one font from one or more designs.
    ///
    /// Each DESIGN is `FILE` or `CHARS=FILE`; FILE is a PDF (optionally with `@PAGE`)
    /// or a bitmap image (TIFF, PNG, JPG, BMP, GIF) which is traced automatically.
    /// Examples:
    ///   motifont build Heart.pdf                       (design in A and a -> heart.ttf)
    ///   motifont build Aa=heart.pdf Bb=star.pdf -o set.ttf
    ///   motifont build Aa=sheet.pdf@1 Bb=sheet.pdf@2 -o set.ttf
    ///   motifont build Heart.tif                       (traced bitmap)
    #[command(verbatim_doc_comment)]
    Build {
        #[arg(required = true)]
        designs: Vec<String>,
        /// Output .ttf path (default: lowercase name of the first PDF).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Font family name (default: output file name without extension).
        #[arg(long)]
        family: Option<String>,
        #[command(flatten)]
        opts: Options,
    },
    /// Convert every PDF (or TIFF/PNG/... image) in a folder into its own font (design in A and a).
    Batch {
        input_dir: PathBuf,
        /// Folder to write the .ttf files to.
        #[arg(short, long)]
        output_dir: PathBuf,
        /// Characters that receive the design.
        #[arg(long, default_value = "Aa")]
        chars: String,
        /// Overwrite existing fonts.
        #[arg(long)]
        overwrite: bool,
        #[command(flatten)]
        opts: Options,
    },
}

#[derive(Args, Clone)]
struct Options {
    /// Maximum design width in font units (em = 1000).
    #[arg(long, default_value_t = 1000.0)]
    width: f64,
    /// Maximum design height in font units.
    #[arg(long, default_value_t = 1000.0)]
    height: f64,
    /// Y position of the bottom of the design (0 = on the baseline).
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    bottom: f64,
    /// Left side bearing (space before the design).
    #[arg(long, default_value_t = 0.0)]
    lsb: f64,
    /// Right side bearing (space after the design).
    #[arg(long, default_value_t = 0.0)]
    rsb: f64,
    /// Fixed advance width instead of lsb + design width + rsb.
    #[arg(long)]
    advance: Option<f64>,
    /// 4-letter OS/2 vendor ID (your foundry/company code).
    #[arg(long, default_value = "NONE")]
    vendor: String,
    /// Font version, e.g. 1.000
    #[arg(long, default_value = "1.000")]
    font_version: String,
    /// Bitmap tracing: pixels darker than this (0-255) count as ink.
    #[arg(long, default_value_t = 128, help_heading = "Bitmap tracing (TIFF/PNG/JPG/BMP/GIF)")]
    threshold: u8,
    /// Bitmap tracing: treat light pixels as ink (white-on-black artwork).
    #[arg(long, help_heading = "Bitmap tracing (TIFF/PNG/JPG/BMP/GIF)")]
    invert: bool,
    /// Bitmap tracing: ignore specks smaller than this many square pixels.
    #[arg(long, default_value_t = 16.0, help_heading = "Bitmap tracing (TIFF/PNG/JPG/BMP/GIF)")]
    despeckle: f64,
    /// Bitmap tracing: how closely to follow the pixels (smaller = tighter, more points).
    #[arg(long, default_value_t = 0.5, help_heading = "Bitmap tracing (TIFF/PNG/JPG/BMP/GIF)")]
    tolerance: f64,
    /// Bitmap tracing: turns sharper than this (degrees) stay as sharp corners.
    #[arg(long, default_value_t = 50.0, help_heading = "Bitmap tracing (TIFF/PNG/JPG/BMP/GIF)")]
    corner_angle: f64,
}

impl Options {
    fn trace(&self) -> trace::TraceOptions {
        trace::TraceOptions {
            threshold: self.threshold,
            invert: self.invert,
            despeckle: self.despeckle,
            tolerance: self.tolerance,
            corner_angle: self.corner_angle,
        }
    }

    fn placement(&self) -> outline::Placement {
        outline::Placement {
            max_width: self.width,
            max_height: self.height,
            bottom: self.bottom,
            lsb: self.lsb,
            rsb: self.rsb,
            advance: self.advance,
        }
    }
}

struct Design {
    chars: Vec<char>,
    file: PathBuf,
    page: u32,
}

fn parse_design(s: &str, default_chars: Option<&str>) -> Result<Design> {
    let (chars, rest) = match s.split_once('=') {
        Some((c, r)) if !c.is_empty() && !c.contains(['/', '\\']) => (c.to_string(), r),
        _ => match default_chars {
            Some(d) => (d.to_string(), s),
            None => bail!("design '{s}' needs characters, e.g. Bb={s}"),
        },
    };
    let (file, page) = match rest.rsplit_once('@') {
        Some((f, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (f, p.parse()?),
        _ => (rest, 1),
    };
    Ok(Design { chars: chars.chars().collect(), file: PathBuf::from(file), page })
}

fn make_font(designs: &[Design], family: &str, opts: &Options, out: &Path) -> Result<()> {
    let mut entries = Vec::new();
    for d in designs {
        let art = if is_image(&d.file) {
            trace::load(&d.file, &opts.trace())?
        } else {
            pdf::load(&d.file, d.page)?
        };
        let (outline, warns) = outline::build(&art, &opts.placement());
        for w in art.warnings.iter().chain(&warns) {
            eprintln!("  warning ({}): {w}", d.file.display());
        }
        entries.push(font::GlyphEntry { chars: d.chars.clone(), outline });
    }
    let info = font::FontInfo { family: family.to_string(), vendor: opts.vendor.clone(), version: opts.font_version.clone() };
    let bytes = font::build(&info, &entries)?;
    std::fs::write(out, bytes).with_context(|| format!("writing {}", out.display()))?;
    Ok(())
}

const IMAGE_EXTS: [&str; 7] = ["tif", "tiff", "png", "jpg", "jpeg", "bmp", "gif"];

fn is_image(p: &Path) -> bool {
    p.extension().is_some_and(|x| IMAGE_EXTS.iter().any(|e| x.eq_ignore_ascii_case(e)))
}

fn stem_lower(p: &Path) -> String {
    p.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_else(|| "motif".into())
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Build { designs, output, family, opts } => {
            let single = designs.len() == 1;
            let designs = designs
                .iter()
                .map(|s| parse_design(s, single.then_some("Aa")))
                .collect::<Result<Vec<_>>>()?;
            let output = output.unwrap_or_else(|| PathBuf::from(format!("{}.ttf", stem_lower(&designs[0].file))));
            let family = family.unwrap_or_else(|| stem_lower(&output));
            make_font(&designs, &family, &opts, &output)?;
            println!("wrote {}", output.display());
        }
        Cmd::Batch { input_dir, output_dir, chars, overwrite, opts } => {
            std::fs::create_dir_all(&output_dir)?;
            let mut pdfs: Vec<PathBuf> = std::fs::read_dir(&input_dir)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("pdf")) || is_image(p))
                .collect();
            pdfs.sort();
            let (mut ok, mut failed, mut skipped) = (0, 0, 0);
            for pdf in &pdfs {
                let family = stem_lower(pdf);
                let out = output_dir.join(format!("{family}.ttf"));
                if out.exists() && !overwrite {
                    skipped += 1;
                    continue;
                }
                let d = Design { chars: chars.chars().collect(), file: pdf.clone(), page: 1 };
                match make_font(&[d], &family, &opts, &out) {
                    Ok(()) => {
                        ok += 1;
                        println!("ok    {} -> {}", pdf.display(), out.display());
                    }
                    Err(e) => {
                        failed += 1;
                        eprintln!("FAIL  {}: {e:#}", pdf.display());
                    }
                }
            }
            println!("{ok} built, {failed} failed, {skipped} skipped (already exist)");
            if failed > 0 {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}

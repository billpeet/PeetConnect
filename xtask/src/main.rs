//! Repository chores, run as `cargo xtask <task>` (alias in `.cargo/config.toml`).
//!
//! - `icons`: render `assets/icon.svg` / `assets/icon-small.svg` into the multi-size
//!   `assets/peetconnect.ico` that `peet-winres` embeds in the Windows executables, plus
//!   `assets/icon-256.png` for places that want a bitmap.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use resvg::{tiny_skia, usvg};

/// Frames written to the .ico. Windows picks the closest match per DPI and context
/// (16/20/24/32 small icons, 40/48 Explorer, 64+ large views and Alt+Tab, 256 tiles).
const ICO_SIZES: [u32; 10] = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256];
/// Frames at or below this size use the flat artwork; shadow and sheen only blur there.
const SMALL_MAX: u32 = 32;

fn main() -> Result<()> {
    let task = std::env::args().nth(1);
    match task.as_deref() {
        Some("icons") => icons(),
        _ => bail!("usage: cargo xtask icons"),
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the repo root")
        .to_path_buf()
}

fn icons() -> Result<()> {
    let assets = repo_root().join("assets");
    let full = load_svg(&assets.join("icon.svg"))?;
    let small = load_svg(&assets.join("icon-small.svg"))?;

    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in ICO_SIZES {
        let tree = if size <= SMALL_MAX { &small } else { &full };
        let rgba = render(tree, size)?;
        let image = ico::IconImage::from_rgba_data(size, size, rgba);
        dir.add_entry(ico::IconDirEntry::encode(&image).context("encode icon frame")?);
    }
    let ico_path = assets.join("peetconnect.ico");
    let file =
        fs::File::create(&ico_path).with_context(|| format!("create {}", ico_path.display()))?;
    dir.write(file).context("write .ico")?;
    println!("wrote {} ({} frames)", ico_path.display(), ICO_SIZES.len());

    let png_path = assets.join("icon-256.png");
    let image = ico::IconImage::from_rgba_data(256, 256, render(&full, 256)?);
    let file =
        fs::File::create(&png_path).with_context(|| format!("create {}", png_path.display()))?;
    image.write_png(file).context("write png")?;
    println!("wrote {}", png_path.display());
    Ok(())
}

fn load_svg(path: &Path) -> Result<usvg::Tree> {
    let data = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    usvg::Tree::from_data(&data, &usvg::Options::default())
        .with_context(|| format!("parse {}", path.display()))
}

/// Rasterise `tree` to a `size` x `size` straight-alpha RGBA buffer.
fn render(tree: &usvg::Tree, size: u32) -> Result<Vec<u8>> {
    let mut pixmap = tiny_skia::Pixmap::new(size, size).context("allocate pixmap")?;
    let scale = size as f32 / tree.size().width();
    resvg::render(
        tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    // tiny-skia stores premultiplied alpha; .ico frames want straight alpha.
    Ok(pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect())
}

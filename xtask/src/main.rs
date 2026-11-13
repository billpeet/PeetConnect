//! Repository chores, run as `cargo xtask <task>` (alias in `.cargo/config.toml`).
//!
//! - `icons`: render `assets/icon.svg` / `assets/icon-small.svg` into the multi-size
//!   `assets/peetconnect.ico` that `peet-winres` embeds in the Windows executables, plus
//!   `assets/icon-256.png` for places that want a bitmap, and the tray state icons
//!   (`assets/tray-active.ico`, `tray-paused.ico`, `tray-offline.ico`) derived from the flat
//!   artwork.
//! - `dist`: build the release binaries, then stage, zip and build the installer
//!   (see `dist.rs`).

mod dist;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use resvg::{tiny_skia, usvg};

/// Frames written to the .ico. Windows picks the closest match per DPI and context
/// (16/20/24/32 small icons, 40/48 Explorer, 64+ large views and Alt+Tab, 256 tiles).
const ICO_SIZES: [u32; 10] = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256];
/// Frames at or below this size use the flat artwork; shadow and sheen only blur there.
const SMALL_MAX: u32 = 32;
/// Notification-area icons: 16 px at 100 % up to 32 px at 200 %, plus headroom.
const TRAY_SIZES: [u32; 6] = [16, 20, 24, 32, 40, 48];

/// Badge in the empty bottom-right corner (the P's stem is bottom-left): a dot with a white
/// ring so it separates from the red at 16 px.
const BADGE_ACTIVE: &str =
    r##"<circle cx="200" cy="200" r="50" fill="#22c55e" stroke="#ffffff" stroke-width="16"/>"##;
const BADGE_PAUSED: &str = r##"<circle cx="200" cy="200" r="50" fill="#f59e0b" stroke="#ffffff" stroke-width="16"/><rect x="180" y="178" width="14" height="44" rx="3" fill="#ffffff"/><rect x="206" y="178" width="14" height="44" rx="3" fill="#ffffff"/>"##;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("icons") => icons(),
        Some("dist") => dist::dist(&args[1..]),
        _ => bail!("usage: cargo xtask icons | dist [--no-build] [--no-installer]"),
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

    let small_text =
        fs::read_to_string(assets.join("icon-small.svg")).context("read icon-small.svg")?;
    let with_badge = |badge: &str| small_text.replace("</svg>", &format!("  {badge}\n</svg>"));
    // Offline: the same art in greys, so "host not running" reads as switched off.
    let grey = small_text
        .replace("#f05252", "#a1a1aa")
        .replace("#c81e1e", "#71717a")
        .replace("#6b1414", "#3f3f46");
    for (name, svg) in [
        ("tray-active.ico", with_badge(BADGE_ACTIVE)),
        ("tray-paused.ico", with_badge(BADGE_PAUSED)),
        ("tray-offline.ico", grey),
    ] {
        let tree = usvg::Tree::from_str(&svg, &usvg::Options::default())
            .with_context(|| format!("parse {name} artwork"))?;
        let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
        for size in TRAY_SIZES {
            let image = ico::IconImage::from_rgba_data(size, size, render(&tree, size)?);
            dir.add_entry(ico::IconDirEntry::encode(&image).context("encode icon frame")?);
        }
        let path = assets.join(name);
        let file = fs::File::create(&path).with_context(|| format!("create {}", path.display()))?;
        dir.write(file).context("write .ico")?;
        println!("wrote {}", path.display());
    }

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

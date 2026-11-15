//! Home-screen thumbnails: a small PNG of each host's primary display, saved while connected
//! (see `remote::session`) under `%LOCALAPPDATA%\PeetConnect\thumbs`, and decoded for GPUI on
//! demand.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use gpui::{RenderImage, Window};
use image::Frame;
use smallvec::SmallVec;

/// How often the cache looks at a file's timestamp for a newer thumbnail.
const RECHECK: Duration = Duration::from_secs(2);

/// Width the host scales thumbnails down to. Cards crop them to 16:10.
pub const WIDTH: u32 = 640;

/// Where `target`'s thumbnail lives. The name keeps only filename-safe characters.
pub fn path(target: &str) -> Option<PathBuf> {
    let name: String = target
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() {
        return None;
    }
    Some(
        dirs::data_local_dir()?
            .join("PeetConnect")
            .join("thumbs")
            .join(format!("{name}.png")),
    )
}

/// Writes a thumbnail, replacing any older one in one step so readers never see half a file.
pub fn save(path: &std::path::Path, png: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("png.tmp");
    std::fs::write(&tmp, png)?;
    std::fs::rename(&tmp, path)
}

/// Removes `target`'s thumbnail, if any.
pub fn remove(target: &str) {
    if let Some(path) = path(target) {
        let _ = std::fs::remove_file(path);
    }
}

struct Entry {
    modified: Option<SystemTime>,
    image: Option<Arc<RenderImage>>,
    checked: Instant,
}

/// Decoded thumbnails by target, reloaded when the file changes.
#[derive(Default)]
pub struct Cache {
    entries: HashMap<String, Entry>,
}

impl Cache {
    /// The thumbnail for `target`, if one has been saved.
    pub fn get(&mut self, target: &str, window: &mut Window) -> Option<Arc<RenderImage>> {
        let now = Instant::now();
        if let Some(entry) = self.entries.get(target) {
            if now.duration_since(entry.checked) < RECHECK {
                return entry.image.clone();
            }
        }
        let path = path(target)?;
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let entry = self.entries.entry(target.to_string()).or_insert(Entry {
            modified: None,
            image: None,
            checked: now,
        });
        entry.checked = now;
        if modified != entry.modified || (modified.is_some() && entry.image.is_none()) {
            entry.modified = modified;
            let fresh = modified.and_then(|_| load(&path));
            if let Some(old) = std::mem::replace(&mut entry.image, fresh) {
                let _ = window.drop_image(old);
            }
        }
        entry.image.clone()
    }

    /// Forgets `target`'s image (after its file was deleted).
    pub fn forget(&mut self, target: &str, window: &mut Window) {
        if let Some(Entry {
            image: Some(old), ..
        }) = self.entries.remove(target)
        {
            let _ = window.drop_image(old);
        }
    }
}

fn load(path: &std::path::Path) -> Option<Arc<RenderImage>> {
    let decoded = match image::open(path) {
        Ok(img) => img.into_rgba8(),
        Err(err) => {
            tracing::debug!(path = %path.display(), %err, "unreadable thumbnail");
            return None;
        }
    };
    // Cards show thumbnails at 16:10; crop the middle of the image to that shape so it can be
    // drawn edge to edge with rounded corners.
    let (w, h) = decoded.dimensions();
    let (cw, ch) = if u64::from(w) * 10 > u64::from(h) * 16 {
        (h * 16 / 10, h)
    } else {
        (w, w * 10 / 16)
    };
    let mut bgra =
        image::imageops::crop_imm(&decoded, (w - cw) / 2, (h - ch) / 2, cw.max(1), ch.max(1))
            .to_image();
    // GPUI's RenderImage takes BGRA bytes in an RGBA-typed buffer.
    for px in bgra.pixels_mut() {
        px.0.swap(0, 2);
    }
    Some(Arc::new(RenderImage::new(SmallVec::from_elem(
        Frame::new(bgra),
        1,
    ))))
}

#[cfg(test)]
mod tests {
    use super::path;

    #[test]
    fn file_names_are_safe() {
        let p = path("Study-PC.example.ts.net:7360").expect("path");
        assert_eq!(
            p.file_name().and_then(|n| n.to_str()),
            Some("study-pc.example.ts.net_7360.png")
        );
        assert!(path("  ").is_none());
    }
}

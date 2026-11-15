//! Shared look of the viewer: colours, embedded icons, tooltips and a few small widgets.

use std::borrow::Cow;

use gpui::{
    div, prelude::*, px, rgb, svg, AnyView, App, AssetSource, Context, Div, ElementId, FontWeight,
    Hsla, Rgba, SharedString, Stateful, Svg, Window,
};

/// Colours, as `0xRRGGBB`.
pub mod color {
    pub const BG: u32 = 0x14161a;
    pub const PANEL: u32 = 0x1a1d23;
    pub const CARD: u32 = 0x1d2027;
    pub const RAISE: u32 = 0x252a33;
    pub const FIELD: u32 = 0x1b1e24;
    pub const WELL: u32 = 0x0f1114;
    pub const BORDER: u32 = 0x2a2f38;
    pub const BORDER2: u32 = 0x343a46;
    pub const TEXT: u32 = 0xe8eaee;
    pub const TEXT2: u32 = 0xb6bbc6;
    pub const MUTED: u32 = 0x8a909c;
    pub const FAINT: u32 = 0x5c6370;
    pub const ACCENT: u32 = 0x4c7dff;
    pub const ACCENT_BG: u32 = 0x2f4f9e;
    pub const ACCENT_SOFT: u32 = 0x8fb0ff;
    pub const GREEN: u32 = 0x3dd68c;
    pub const AMBER: u32 = 0xf5b84a;
    pub const OFFLINE: u32 = 0x4a505b;
    pub const RED: u32 = 0xff8a8a;
    pub const RED_BG: u32 = 0x3a1e22;
    pub const RED_BORDER: u32 = 0x5a2a30;
    pub const ERROR: u32 = 0xff7b72;
    pub const STAR: u32 = 0xffc53d;
    pub const BAR: u32 = 0x16181d;
}

/// `rgb` with an alpha, for translucent fills.
pub fn rgba_a(hex: u32, alpha: f32) -> Hsla {
    let mut c: Rgba = rgb(hex);
    c.a = alpha;
    c.into()
}

/// Icons and the logo, compiled into the binary.
pub struct Assets;

macro_rules! embedded {
    ($path:expr, $($name:literal),* $(,)?) => {
        match $path {
            $(concat!("icons/", $name, ".svg") => Some(include_bytes!(concat!("../assets/icons/", $name, ".svg")).as_slice()),)*
            "brand/icon.png" => Some(include_bytes!("../../../assets/icon-256.png").as_slice()),
            _ => None,
        }
    };
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(embedded!(
            path,
            "app-window",
            "arrow-right",
            "check",
            "chevron-down",
            "ellipsis",
            "laptop",
            "layout-grid",
            "lock",
            "maximize",
            "minimize",
            "monitor",
            "network",
            "pencil",
            "power",
            "refresh",
            "scaling",
            "search",
            "server",
            "signal",
            "sliders",
            "smartphone",
            "star",
            "star-filled",
            "trash",
            "windows",
            "x",
        )
        .map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

/// An embedded icon by name (see `assets/icons`), `size` logical pixels square, drawn in
/// `color`.
pub fn icon(name: &'static str, size: f32, color: u32) -> Svg {
    svg()
        .path(format!("icons/{name}.svg"))
        .size(px(size))
        .flex_none()
        .text_color(rgb(color))
}

/// The icon for a Tailscale OS string.
pub fn os_icon(os: &str) -> &'static str {
    match os {
        "windows" => "windows",
        "android" | "iOS" | "ios" => "smartphone",
        "macOS" | "macos" => "laptop",
        "" => "monitor",
        _ => "server",
    }
}

/// A human name for a Tailscale OS string.
pub fn os_name(os: &str) -> String {
    match os {
        "windows" => "Windows".into(),
        "linux" => "Linux".into(),
        "android" => "Android".into(),
        "macOS" | "macos" => "macOS".into(),
        "iOS" | "ios" => "iOS".into(),
        "" => "Unknown".into(),
        other => other.into(),
    }
}

/// A small status dot.
pub fn dot(color: u32, size: f32) -> gpui::Div {
    div()
        .size(px(size))
        .flex_none()
        .rounded_full()
        .bg(rgb(color))
}

/// A keyboard-shortcut label.
pub fn kbd(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .px(px(5.0))
        .py(px(1.0))
        .rounded(px(4.0))
        .border_1()
        .border_b_2()
        .border_color(rgb(color::BORDER2))
        .bg(rgb(color::FIELD))
        .text_xs()
        .text_color(rgb(color::MUTED))
        .whitespace_nowrap()
        .child(text.into())
}

/// A thin vertical divider for toolbars.
pub fn separator() -> gpui::Div {
    div()
        .w(px(1.0))
        .h(px(18.0))
        .mx(px(6.0))
        .flex_none()
        .bg(rgb(color::BORDER2))
}

/// Text for a tooltip, with an optional shortcut shown beside it.
pub struct Tooltip {
    text: SharedString,
    shortcut: Option<SharedString>,
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py(px(4.0))
            .rounded_md()
            .bg(rgb(0x2b303a))
            .border_1()
            .border_color(rgb(0x3b4250))
            .shadow_md()
            .text_xs()
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgb(0xffffff))
            .child(self.text.clone())
            .children(self.shortcut.clone().map(kbd))
    }
}

/// A builder for `.tooltip(...)`.
pub fn tooltip(
    text: impl Into<SharedString>,
    shortcut: Option<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    move |_, cx| {
        let tip = Tooltip {
            text: text.clone(),
            shortcut: shortcut.clone(),
        };
        cx.new(|_| tip).into()
    }
}

/// A square icon-only button; `active` tints it as a toggle that is on.
pub fn icon_button(
    id: impl Into<ElementId>,
    name: &'static str,
    size: f32,
    active: bool,
) -> Stateful<Div> {
    let button = div()
        .id(id)
        .size(px(size + 14.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer();
    if active {
        button
            .bg(rgba_a(color::ACCENT, 0.18))
            .hover(|s| s.bg(rgba_a(color::ACCENT, 0.28)))
            .child(icon(name, size, color::ACCENT_SOFT))
    } else {
        button
            .hover(|s| s.bg(rgb(color::RAISE)))
            .child(icon(name, size, color::TEXT2))
    }
}

/// How a [`button`] looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Primary,
    Ghost,
    /// A toggle or selection that is on.
    Selected,
    Danger,
}

/// A text button with an optional leading icon.
pub fn button(
    id: impl Into<ElementId>,
    icon_name: Option<&'static str>,
    label: impl Into<SharedString>,
    kind: Kind,
) -> Stateful<Div> {
    let (bg, hover, fg, border) = match kind {
        Kind::Primary => (color::ACCENT, 0x5d8bff, 0xffffff, color::ACCENT),
        Kind::Ghost => (color::RAISE, 0x2e3440, color::TEXT, color::BORDER2),
        Kind::Selected => (color::ACCENT_BG, 0x3a5cb0, 0xffffff, 0x3d63c4),
        Kind::Danger => (color::RED_BG, 0x4a252b, color::RED, color::RED_BORDER),
    };
    div()
        .id(id)
        .h(px(28.0))
        .px(px(10.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(6.0))
        .rounded_md()
        .border_1()
        .border_color(rgb(border))
        .bg(rgb(bg))
        .hover(move |s| s.bg(rgb(hover)))
        .cursor_pointer()
        .whitespace_nowrap()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(fg))
        .children(icon_name.map(|n| icon(n, 14.0, fg)))
        .child(label.into())
}

/// "just now", "5 min ago", "3 h ago", "yesterday", "4 days ago", from Unix seconds.
pub fn ago(when: u64, now: u64) -> String {
    if when == 0 {
        return "never".into();
    }
    let secs = now.saturating_sub(when);
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86_400 => format!("{} h ago", secs / 3600),
        86_400..172_800 => "yesterday".into(),
        _ => format!("{} days ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::ago;

    #[test]
    fn ago_buckets() {
        let now = 1_000_000;
        assert_eq!(ago(0, now), "never");
        assert_eq!(ago(now - 10, now), "just now");
        assert_eq!(ago(now - 300, now), "5 min ago");
        assert_eq!(ago(now - 7200, now), "2 h ago");
        assert_eq!(ago(now - 90_000, now), "yesterday");
        assert_eq!(ago(now - 4 * 86_400, now), "4 days ago");
    }
}

//! Follow the Omarchy desktop theme.
//!
//! Omarchy renders `~/.config/omarchy/themed/grok.toml.tpl` into
//! `~/.local/state/omarchy/current/theme/grok.toml` on every theme change and then
//! swaps the whole `theme` directory in atomically.
//! With `[ui] theme = "omarchy"` the pager reads that file at startup, lays its
//! `[colors]` over the built-in theme that matches its `base` polarity, and re-reads
//! it whenever the `current` directory changes. Locating the state directory, reading
//! the rendered file and the inotify watch come from the `omarchy-theme` crate.
//! A running session therefore retints at the same moment as Ghostty and the other
//! native Omarchy apps, without a hook, a signal, or a poll.
//!
//! File shape (every key optional; unknown keys are ignored):
//!
//! ```toml
//! base = "dark"          # "light" picks GrokDay underneath, anything else GrokNight
//!
//! [colors]
//! bg_base = "#2e3440"    # any `Theme` color field, as #rrggbb
//! ```

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use omarchy_theme::{Omarchy, Subscription};
use ratatui::style::Color;

use super::{Theme, ThemeKind};

/// The `[ui].theme` value that turns this mode on.
pub const NAME: &str = "omarchy";
/// The file Omarchy renders from `~/.config/omarchy/themed/grok.toml.tpl`.
const THEME_FILE: &str = "grok.toml";

/// Set by [`enable`] when the config names [`NAME`]; never cleared outside tests.
static ENABLED: AtomicBool = AtomicBool::new(false);
/// Bumped on every palette change so render caches keyed on [`super::cache::stamp`] re-bake.
static GENERATION: AtomicU64 = AtomicU64::new(0);
static ACTIVE: RwLock<Option<Arc<Overlay>>> = RwLock::new(None);

struct Overlay {
    base: ThemeKind,
    colors: BTreeMap<String, Color>,
    /// `base` theme with `colors` applied, built once per reload.
    palette: Theme,
}

#[derive(Default, serde::Deserialize)]
struct File {
    base: Option<String>,
    #[serde(default)]
    colors: BTreeMap<String, String>,
}

#[must_use]
pub fn is_name(name: &str) -> bool {
    name.eq_ignore_ascii_case(NAME)
}

#[must_use]
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Omarchy's `current` state directory.
/// `GROK_OMARCHY_STATE_DIR` overrides it so a session can be pointed at a scratch copy.
fn omarchy() -> Option<Omarchy> {
    match std::env::var_os("GROK_OMARCHY_STATE_DIR") {
        Some(dir) => Some(Omarchy::at(dir)),
        None => Omarchy::detect(),
    }
}

/// Turn omarchy mode on and load the palette. Returns the base kind to render under.
pub fn enable() -> ThemeKind {
    ENABLED.store(true, Ordering::Relaxed);
    reload();
    base_kind()
}

/// Built-in kind underneath the overlay (`GrokNight` until a file with `base = "light"` loads).
#[must_use]
pub fn base_kind() -> ThemeKind {
    active().map_or(ThemeKind::GrokNight, |o| o.base)
}

#[must_use]
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// The overlaid palette, or `None` when omarchy mode is off or no file has loaded yet.
#[must_use]
pub fn palette() -> Option<Theme> {
    active().map(|o| o.palette)
}

fn active() -> Option<Arc<Overlay>> {
    ACTIVE.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Re-read the theme file. Returns `true` when the palette changed.
/// An unreadable or malformed file keeps the current palette: Omarchy deletes and
/// re-creates the `theme` directory, so a missing file is normal mid-swap.
pub fn reload() -> bool {
    if !is_enabled() {
        return false;
    }
    match omarchy().map(|o| o.read_theme_file(THEME_FILE)) {
        Some(Ok(text)) => apply_text(&text),
        Some(Err(error)) => {
            tracing::debug!(%error, "omarchy theme file unreadable; keeping palette");
            false
        }
        None => false,
    }
}

fn apply_text(text: &str) -> bool {
    let overlay = match parse(text) {
        Ok(overlay) => overlay,
        Err(error) => {
            tracing::warn!(%error, "omarchy theme file malformed; keeping palette");
            return false;
        }
    };
    let mut guard = ACTIVE.write().unwrap_or_else(|e| e.into_inner());
    if guard
        .as_ref()
        .is_some_and(|current| current.base == overlay.base && current.colors == overlay.colors)
    {
        return false;
    }
    *guard = Some(Arc::new(overlay));
    GENERATION.fetch_add(1, Ordering::Relaxed);
    true
}

fn parse(text: &str) -> Result<Overlay, toml::de::Error> {
    let file: File = toml::from_str(text)?;
    let base = match file.base.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("light") => ThemeKind::GrokDay,
        _ => ThemeKind::GrokNight,
    };
    let mut palette = match base {
        ThemeKind::GrokDay => Theme::grokday(),
        _ => Theme::groknight(),
    };
    let mut colors = BTreeMap::new();
    for (key, value) in file.colors {
        let Some(color) = parse_hex(&value) else {
            tracing::warn!(key = %key, value = %value, "omarchy theme: expected #rrggbb");
            continue;
        };
        match slot_mut(&mut palette, &key) {
            Some(slot) => {
                *slot = color;
                colors.insert(key, color);
            }
            None => tracing::debug!(key = %key, "omarchy theme: unknown color slot"),
        }
    }
    Ok(Overlay {
        base,
        colors,
        palette,
    })
}

fn parse_hex(value: &str) -> Option<Color> {
    let hex = value.trim().strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some(Color::Rgb(channel(0)?, channel(2)?, channel(4)?))
}

/// The `Theme` color field named `key`. Modifier fields are not addressable.
fn slot_mut<'a>(t: &'a mut Theme, key: &str) -> Option<&'a mut Color> {
    Some(match key {
        "bg_base" => &mut t.bg_base,
        "bg_light" => &mut t.bg_light,
        "bg_dark" => &mut t.bg_dark,
        "bg_highlight" => &mut t.bg_highlight,
        "bg_hover" => &mut t.bg_hover,
        "bg_terminal" => &mut t.bg_terminal,
        "accent_user" => &mut t.accent_user,
        "accent_assistant" => &mut t.accent_assistant,
        "accent_thinking" => &mut t.accent_thinking,
        "accent_tool" => &mut t.accent_tool,
        "accent_system" => &mut t.accent_system,
        "accent_error" => &mut t.accent_error,
        "accent_success" => &mut t.accent_success,
        "accent_running" => &mut t.accent_running,
        "accent_skill" => &mut t.accent_skill,
        "text_primary" => &mut t.text_primary,
        "text_secondary" => &mut t.text_secondary,
        "gray_dim" => &mut t.gray_dim,
        "gray" => &mut t.gray,
        "gray_bright" => &mut t.gray_bright,
        "command" => &mut t.command,
        "path" => &mut t.path,
        "running" => &mut t.running,
        "warning" => &mut t.warning,
        "fuzzy_accent" => &mut t.fuzzy_accent,
        "accent_plan" => &mut t.accent_plan,
        "accent_verify" => &mut t.accent_verify,
        "accent_remember" => &mut t.accent_remember,
        "selection_border" => &mut t.selection_border,
        "hover_border" => &mut t.hover_border,
        "prompt_border" => &mut t.prompt_border,
        "prompt_border_active" => &mut t.prompt_border_active,
        "accent_model" => &mut t.accent_model,
        "scrollbar_bg" => &mut t.scrollbar_bg,
        "scrollbar_fg" => &mut t.scrollbar_fg,
        "diff_delete_bg" => &mut t.diff_delete_bg,
        "diff_delete_fg" => &mut t.diff_delete_fg,
        "diff_insert_bg" => &mut t.diff_insert_bg,
        "diff_insert_fg" => &mut t.diff_insert_fg,
        "diff_equal_fg" => &mut t.diff_equal_fg,
        "diff_gutter_fg" => &mut t.diff_gutter_fg,
        "bg_visual" => &mut t.bg_visual,
        "paste_bg" => &mut t.paste_bg,
        "paste_fg" => &mut t.paste_fg,
        "paste_dim" => &mut t.paste_dim,
        "md_heading_h1" => &mut t.md_heading_h1,
        "md_heading_h2" => &mut t.md_heading_h2,
        "md_heading_h3" => &mut t.md_heading_h3,
        "md_heading_h4" => &mut t.md_heading_h4,
        "md_heading_h5" => &mut t.md_heading_h5,
        "md_heading_h6" => &mut t.md_heading_h6,
        "md_code" => &mut t.md_code,
        "md_task_checked" => &mut t.md_task_checked,
        "md_task_unchecked" => &mut t.md_task_unchecked,
        "md_muted" => &mut t.md_muted,
        "md_code_bg" => &mut t.md_code_bg,
        "md_text" => &mut t.md_text,
        "link_fg" => &mut t.link_fg,
        _ => return None,
    })
}

/// Wakes the event loop whenever Omarchy touches its `current` directory.
/// Every event is a wake-up; [`reload`] decides whether the palette really changed.
pub struct Watcher {
    subscription: Subscription,
}

impl Watcher {
    /// `None` unless omarchy mode is on and the state directory can be watched.
    /// The event loop then uses `std::future::pending()` so the `select!` branch never fires.
    #[must_use]
    pub fn start() -> Option<Self> {
        if !is_enabled() {
            return None;
        }
        let omarchy = omarchy()?;
        match omarchy.subscribe() {
            Ok(subscription) => {
                tracing::info!(dir = %omarchy.current_dir().display(), "watching omarchy theme directory");
                Some(Self { subscription })
            }
            Err(error) => {
                tracing::warn!(%error, "omarchy theme watch unavailable");
                None
            }
        }
    }

    pub async fn changed(&mut self) -> Result<(), tokio::sync::watch::error::RecvError> {
        self.subscription.changed().await
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_for_test() {
    ENABLED.store(false, Ordering::Relaxed);
    *ACTIVE.write().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_overlays_listed_slots_on_base_polarity() {
        let overlay =
            parse("base = \"light\"\n[colors]\nbg_base = \"#112233\"\nlink_fg = \"#ABCDEF\"\n")
                .unwrap();
        assert_eq!(overlay.base, ThemeKind::GrokDay);
        assert_eq!(overlay.palette.bg_base, Color::Rgb(0x11, 0x22, 0x33));
        assert_eq!(overlay.palette.link_fg, Color::Rgb(0xab, 0xcd, 0xef));
        assert_eq!(overlay.palette.text_primary, Theme::grokday().text_primary);
        assert_eq!(overlay.colors.len(), 2);
    }

    #[test]
    fn parse_defaults_to_dark_and_skips_bad_entries() {
        let overlay = parse(
            "[colors]\nnot_a_slot = \"#000000\"\nbg_base = \"red\"\naccent_user = \"#ff0000\"\n",
        )
        .unwrap();
        assert_eq!(overlay.base, ThemeKind::GrokNight);
        assert_eq!(overlay.palette.bg_base, Theme::groknight().bg_base);
        assert_eq!(overlay.palette.accent_user, Color::Rgb(0xff, 0, 0));
        assert_eq!(overlay.colors.len(), 1);
    }

    #[test]
    fn parse_rejects_invalid_toml() {
        assert!(parse("base = ").is_err());
    }

    #[test]
    fn apply_text_bumps_generation_only_on_change() {
        let _guard = super::super::cache::test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_for_test();
        let before = generation();

        assert!(apply_text("[colors]\nbg_base = \"#010203\"\n"));
        assert_eq!(generation(), before + 1);
        assert_eq!(palette().unwrap().bg_base, Color::Rgb(1, 2, 3));

        assert!(
            !apply_text("[colors]\nbg_base = \"#010203\"\n"),
            "identical content is not a change"
        );
        assert_eq!(generation(), before + 1);

        assert!(apply_text("base = \"light\"\n"));
        assert_eq!(base_kind(), ThemeKind::GrokDay);
        assert_eq!(generation(), before + 2);

        assert!(!apply_text("base = ["), "malformed text keeps the palette");
        assert_eq!(base_kind(), ThemeKind::GrokDay);
        reset_for_test();
    }
}

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// How small the notch may be drawn, as a multiple of its designed size. Below roughly 0.4 the
/// rings stop being readable at 100 % display scaling.
pub const SCALE_MIN: f64 = 0.40;
pub const SCALE_MAX: f64 = 1.00;

/// One half of the tray icon, or one ring on the notch: which provider. It shows that provider's
/// ring, so the tray and the notch can never disagree. (A `window` key from older builds is ignored.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraySlot {
    pub provider: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    /// "auto" | "zh" | "en" | "ja" | "ko" | "ru"
    #[serde(default = "default_lang")]
    pub lang: String,
    #[serde(default)]
    pub bar_x: Option<i32>,
    #[serde(default)]
    pub bar_y: Option<i32>,
    /// Logical width of the bar (wheel-adjustable, 220-520); None = default 360
    #[serde(default)]
    pub bar_w: Option<u32>,
    /// Allow dragging + wheel resizing (tray toggle, off by default to prevent accidental drags)
    #[serde(default)]
    pub drag_enabled: bool,
    /// Vertical position of the notch: the window centre as a fraction of the primary monitor's height (0 = top, 1 = bottom), default 0.5; saved after a drag
    #[serde(default = "default_notch_y")]
    pub notch_y: f64,
    /// Notch size as a multiple of the designed size (slider at the foot of the hover card).
    /// Only the pill is scaled — the hover card keeps its size, so the slider does not move
    /// while it is being dragged.
    #[serde(default = "default_scale")]
    pub scale: f64,
    /// What the tray icon draws: "off" (the plain mark, the previous behaviour and the default),
    /// "numbers" (up to two readings as digits) or "bars" (a column per reading).
    #[serde(default = "default_tray_mode")]
    pub tray_mode: String,
    /// Which providers the tray icon covers, in the order they are drawn. Ids match the page:
    /// "claude", "codex", "cursor", "gemini". Superseded by `tray_slots`; kept so an existing
    /// config still upgrades cleanly, and migrated in `load()`.
    #[serde(default = "default_tray_providers")]
    pub tray_providers: Vec<String>,
    /// What each part of the tray icon shows, in drawing order: the first entry is the top half of
    /// the digit layout, the second the bottom half, and the bar layout uses them all in order.
    #[serde(default)]
    pub tray_slots: Vec<TraySlot>,
    /// Which providers the notch itself shows, in order. Empty means every provider that has
    /// something to report — the original behaviour, and the default. Superseded by `notch_slots`,
    /// kept so an existing config migrates cleanly.
    #[serde(default)]
    pub notch_providers: Vec<String>,
    /// Which providers get a ring on the notch, in order. An empty list means every provider.
    #[serde(default)]
    pub notch_slots: Vec<TraySlot>,
    /// Antigravity's lane on the ring, as the Mac app's "Notch reads": "automatic", "5h" or "weekly"
    #[serde(default = "default_antigravity_limit")]
    pub antigravity_limit: String,
    /// The model family that choice looks at, as the Mac app's "Model data": "gemini" or "3p"
    #[serde(default = "default_antigravity_model")]
    pub antigravity_model: String,
    /// false = the pill is kept off the screen edge entirely; the tray icon is then the only way in
    #[serde(default = "yes")]
    pub notch_visible: bool,
    /// false = the tray icon is hidden. Refused while the notch is also hidden, because that would
    /// leave the app running with no way to reach it.
    #[serde(default = "yes")]
    pub tray_visible: bool,
    /// Admin keys for the pay-as-you-go cost cells (Settings → API spend). Stored as typed; an
    /// empty string means that cell is off. These are *admin* keys, not ordinary API keys.
    #[serde(default)]
    pub anthropic_admin_key: String,
    /// Monthly budget in USD the Anthropic API ring fills against; 0 = no budget, figure only
    #[serde(default)]
    pub anthropic_budget_usd: f64,
    #[serde(default)]
    pub openai_admin_key: String,
    #[serde(default)]
    pub openai_budget_usd: f64,
    /// xAI management key (console.x.ai → Management keys) and the team whose spend to read;
    /// an empty team id is looked up from the key itself
    #[serde(default)]
    pub xai_management_key: String,
    #[serde(default)]
    pub xai_team_id: String,
    #[serde(default)]
    pub xai_budget_usd: f64,
    /// Opacity of the pill's black background, 0.0 (transparent — rings float on the wallpaper)
    /// to 1.0 (solid, the original look). The rings, glyphs and figures are never faded.
    #[serde(default = "default_pill_alpha")]
    pub pill_alpha: f64,
    /// Colour of the figures under the rings: "auto" (white on a solid pill, dark once the pill
    /// is see-through), "light" or "dark". A see-through pill sits on whatever wallpaper is
    /// behind it, and no shadow makes white readable on a light one.
    #[serde(default = "default_pill_text")]
    pub pill_text: String,
}

fn default_pill_text() -> String {
    "auto".into()
}

fn default_pill_alpha() -> f64 {
    1.0
}

fn default_notch_y() -> f64 {
    0.5
}
fn default_scale() -> f64 {
    1.0
}
fn yes() -> bool {
    true
}
/// A fresh install shows the two readings straight away — a tray icon nobody knows to look for is
/// a feature nobody finds. An install that predates this setting is handled in `load()` instead:
/// it keeps the plain mark it already has, so upgrading never changes anyone's icon unasked.
fn default_tray_mode() -> String {
    "numbers".into()
}
fn default_tray_providers() -> Vec<String> {
    vec!["claude".into(), "codex".into()]
}
fn default_antigravity_limit() -> String {
    "automatic".into()
}
fn default_antigravity_model() -> String {
    "gemini".into()
}

fn default_port() -> u16 {
    48666
}
fn default_lang() -> String {
    "auto".into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: default_port(),
            lang: default_lang(),
            bar_x: None,
            bar_y: None,
            bar_w: None,
            drag_enabled: false,
            notch_y: default_notch_y(),
            scale: default_scale(),
            tray_mode: default_tray_mode(),
            tray_providers: default_tray_providers(),
            tray_slots: Vec::new(), // filled in by load(), from tray_providers
            notch_providers: Vec::new(), // empty = show them all
            notch_slots: Vec::new(),     // filled in by load(), from notch_providers
            antigravity_limit: default_antigravity_limit(),
            antigravity_model: default_antigravity_model(),
            notch_visible: true,
            tray_visible: true,
            anthropic_admin_key: String::new(),
            anthropic_budget_usd: 0.0,
            openai_admin_key: String::new(),
            openai_budget_usd: 0.0,
            xai_management_key: String::new(),
            xai_team_id: String::new(),
            xai_budget_usd: 0.0,
            pill_alpha: 1.0,
            pill_text: "auto".into(),
        }
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("codenotch")
        .join("config.json")
}

pub fn load() -> Config {
    let path = config_path();
    let raw = std::fs::read_to_string(&path).ok();
    let mut cfg: Config = raw
        .as_deref()
        .and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or_default();

    // Discoverability without surprising anyone. `default_tray_mode` gives a NEW install the
    // numbers icon, but serde applies that same default to an EXISTING config that simply predates
    // the setting — which would silently change the tray icon of everyone who upgrades. So an
    // existing file with no `tray_mode` key is pinned to the plain mark it already has; only a
    // machine with no config at all gets the new default.
    let upgrading = raw
        .as_deref()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok())
        .map(|v| v.get("tray_mode").is_none())
        .unwrap_or(false);
    if upgrading {
        cfg.tray_mode = "off".into();
    }

    // Migration: before slots existed the icon was a plain provider list, one reading each. That
    // is exactly a list of slots, so nobody's choice is lost and nobody has to reconfigure anything.
    if cfg.tray_slots.is_empty() {
        cfg.tray_slots = cfg
            .tray_providers
            .iter()
            .map(|p| TraySlot { provider: p.clone() })
            .collect();
    }

    // Same migration for the notch.
    if cfg.notch_slots.is_empty() {
        cfg.notch_slots = cfg
            .notch_providers
            .iter()
            .map(|p| TraySlot { provider: p.clone() })
            .collect();
    }

    // Both hidden would leave the app unreachable: no pill, no tray icon, no way to open settings.
    if !cfg.notch_visible && !cfg.tray_visible {
        cfg.tray_visible = true;
    }

    // A hand-edited file must not be able to produce an invisible window
    cfg.scale = cfg.scale.clamp(SCALE_MIN, SCALE_MAX);

    // Admin keys are stored DPAPI-protected; in memory they are plaintext. A plaintext value in
    // the file (older config, or pasted by hand) is accepted and protected on the next save.
    let had_plain = [&cfg.anthropic_admin_key, &cfg.openai_admin_key, &cfg.xai_management_key]
        .iter()
        .any(|k| !k.is_empty() && !crate::secret::is_protected(k));
    cfg.anthropic_admin_key = crate::secret::unprotect(&cfg.anthropic_admin_key);
    cfg.openai_admin_key = crate::secret::unprotect(&cfg.openai_admin_key);
    cfg.xai_management_key = crate::secret::unprotect(&cfg.xai_management_key);
    if had_plain {
        save(&cfg);
    }
    cfg
}

pub fn save(cfg: &Config) {
    let path = config_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut on_disk = cfg.clone();
    on_disk.anthropic_admin_key = crate::secret::protect(&cfg.anthropic_admin_key);
    on_disk.openai_admin_key = crate::secret::protect(&cfg.openai_admin_key);
    on_disk.xai_management_key = crate::secret::protect(&cfg.xai_management_key);
    let cfg = &on_disk;
    if let Ok(txt) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(path, txt);
    }
}

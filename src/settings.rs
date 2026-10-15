//! Persistent settings, stored as JSON in %APPDATA%\AutoClicker4dot20\settings.json.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fs, path::PathBuf};

pub const VK_RETURN: u32 = 0x0D;
pub const VK_LSHIFT: u32 = 0xA0;
pub const VK_F: u32 = 0x46;
pub const VK_S: u32 = 0x53;
pub const VK_W: u32 = 0x57;
pub const VK_F5: u32 = 0x74;
pub const VK_F6: u32 = 0x75;
pub const VK_F7: u32 = 0x76;
pub const VK_F8: u32 = 0x77;
pub const VK_F9: u32 = 0x78;
pub const VK_F10: u32 = 0x79;

/// A WARDOGS faction and where its JOIN button is on your screen.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Team {
    pub id: String,
    pub name: String,
    pub color: String,
    /// Screen position when it was set (used only by old saves without nx/ny).
    pub x: i32,
    pub y: i32,
    /// Position relative to the game window: offset from its centre in units
    /// of its height. Survives resolution and aspect-ratio changes.
    pub nx: f32,
    pub ny: f32,
    /// Game window height when the spot was saved (0 = old absolute save).
    pub ref_h: i32,
    pub calibrated: bool,
    /// Hex-encoded luminance patch around the button, used to check the team
    /// screen is open and to notice when you're in so clicking stops.
    pub patch: String,
    /// Selects this team and starts joining it.
    pub hotkey: u32,
}

impl Default for Team {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            color: String::new(),
            x: 0,
            y: 0,
            nx: 0.0,
            ny: 0.0,
            ref_h: 0,
            calibrated: false,
            patch: String::new(),
            hotkey: 0,
        }
    }
}

const TEAMS: [(&str, &str, &str, u32); 3] = [
    ("lonestar", "Lonestar", "#3B82F6", VK_F8),
    ("valkyra", "Valkyra", "#E5484D", VK_F9),
    ("manticore", "Manticore", "#46A758", VK_F10),
];

fn default_team(i: usize) -> Team {
    let (id, name, color, hotkey) = TEAMS[i];
    Team { id: id.into(), name: name.into(), color: color.into(), hotkey, ..Team::default() }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Keys {
    /// Start / stop joining the selected team.
    pub join: u32,
    /// Saves the cursor position while setting a button position.
    pub capture: u32,
}

impl Default for Keys {
    fn default() -> Self {
        Self { join: VK_F6, capture: VK_RETURN }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SprintMode {
    /// Auto-run: hold the forward AND sprint keys for you until you stop it.
    Run,
    /// Hold the sprint key while the forward key is held.
    Hold,
    /// Tap the sprint key once each time you start moving (toggle-sprint games).
    Tap,
    /// Keep the sprint key held the whole time auto sprint is on.
    Always,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct SprintSettings {
    pub enabled: bool,
    pub mode: SprintMode,
    pub sprint_key: u32,
    pub forward_key: u32,
    /// Pressing this (or the forward key) stops auto-run.
    pub back_key: u32,
    pub hotkey: u32,
}

impl Default for SprintSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: SprintMode::Run,
            sprint_key: VK_LSHIFT,
            forward_key: VK_W,
            back_key: VK_S,
            hotkey: VK_F7,
        }
    }
}

/// Auto revive: holds the revive key for you for a set time.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ReviveSettings {
    /// Starts (and cancels) the revive.
    pub hotkey: u32,
    /// The key the game wants held (F in WARDOGS).
    pub key: u32,
    pub hold_ms: u64,
    /// Let go as soon as you press a movement key.
    pub stop_on_move: bool,
}

impl Default for ReviveSettings {
    fn default() -> Self {
        // A bare-hands revive takes about 20 s; hold a little longer so it finishes.
        Self { hotkey: VK_F5, key: VK_F, hold_ms: 22_000, stop_on_move: true }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct GeneralSettings {
    pub always_on_top: bool,
    pub sound: bool,
    /// Name shown to your party. Empty = your Steam name.
    pub name: String,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self { always_on_top: false, sound: true, name: String::new() }
    }
}

/// Bumped when a saved setting's meaning changes; older files get migrated.
const SETTINGS_VERSION: u32 = 3;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    /// Files written before versioning load as 0.
    #[serde(default)]
    pub version: u32,
    pub teams: Vec<Team>,
    pub selected: String,
    pub keys: Keys,
    pub sprint: SprintSettings,
    pub revive: ReviveSettings,
    pub general: GeneralSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            teams: (0..TEAMS.len()).map(default_team).collect(),
            selected: TEAMS[0].0.into(),
            keys: Keys::default(),
            sprint: SprintSettings::default(),
            revive: ReviveSettings::default(),
            general: GeneralSettings::default(),
        }
    }
}

/// Plain single keys (sprint, forward, revive, capture) take no modifiers.
fn valid_key(code: u32, fallback: u32) -> u32 {
    if code == 0 || code > 0xFE || (1..=6).contains(&code) {
        fallback
    } else {
        code
    }
}

/// Hotkeys are `vk | mods << 8` (mods: 1 Ctrl, 2 Alt, 4 Shift, 8 Win); 0 = unbound.
pub fn valid_hotkey(code: u32) -> u32 {
    let (vk, mods) = (code & 0xFF, code >> 8);
    if vk == 0 || vk > 0xFE || mods > 0xF {
        0
    } else {
        code
    }
}

impl Settings {
    /// Always exactly the three WARDOGS factions, in order; keeps saved positions and keys.
    pub fn sanitize(&mut self) {
        let old = std::mem::take(&mut self.teams);
        self.teams = (0..TEAMS.len())
            .map(|i| {
                let mut t = default_team(i);
                if let Some(saved) = old.iter().find(|o| o.id == t.id) {
                    t.x = saved.x;
                    t.y = saved.y;
                    t.nx = saved.nx;
                    t.ny = saved.ny;
                    t.ref_h = saved.ref_h.max(0);
                    t.calibrated = saved.calibrated;
                    t.patch = saved.patch.clone();
                    t.hotkey = valid_hotkey(saved.hotkey);
                }
                t
            })
            .collect();
        if !self.teams.iter().any(|t| t.id == self.selected) {
            self.selected = self.teams[0].id.clone();
        }
        self.keys.join = valid_hotkey(self.keys.join);
        self.keys.capture = valid_key(self.keys.capture, VK_RETURN);

        if self.version < 2 {
            // v2: auto sprint became auto-run (holds forward + sprint for you).
            self.sprint.mode = SprintMode::Run;
        }
        if self.version < 3 && self.revive.hold_ms == 10_500 {
            // v3: the old 10.5 s default was too short for a bare-hands revive.
            self.revive.hold_ms = 22_000;
        }
        self.version = SETTINGS_VERSION;

        let s = &mut self.sprint;
        s.sprint_key = valid_key(s.sprint_key, VK_LSHIFT);
        s.forward_key = valid_key(s.forward_key, VK_W);
        s.back_key = valid_key(s.back_key, VK_S);
        s.hotkey = valid_hotkey(s.hotkey);

        let r = &mut self.revive;
        r.hotkey = valid_hotkey(r.hotkey);
        r.key = valid_key(r.key, VK_F);
        r.hold_ms = r.hold_ms.clamp(1_000, 60_000);

        self.general.name = self.general.name.trim().chars().take(24).collect();
    }
}

pub fn path() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("AutoClicker4dot20").join("settings.json")
}

pub fn load() -> Settings {
    let mut s = fs::read_to_string(path())
        .ok()
        .and_then(|text| serde_json::from_str::<Settings>(&text).ok())
        .unwrap_or_default();
    s.sanitize();
    s
}

pub fn save(s: &Settings) {
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(s) {
        // Write-then-rename so a crash mid-save never leaves a half-written file.
        let tmp = p.with_extension("json.tmp");
        if fs::write(&tmp, text).is_ok() {
            let _ = fs::rename(&tmp, &p);
        }
    }
}

/// JSON merge-patch: objects merge key by key, everything else (including arrays) replaces.
pub fn merge(target: &mut Value, patch: &Value) {
    if let Value::Object(patch_map) = patch {
        if !target.is_object() {
            *target = Value::Object(Default::default());
        }
        let map = target.as_object_mut().unwrap();
        for (k, v) in patch_map {
            merge(map.entry(k.clone()).or_insert(Value::Null), v);
        }
    } else {
        *target = patch.clone();
    }
}

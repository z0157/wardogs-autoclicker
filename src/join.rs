//! Team join: clicks your faction's JOIN button at a steady, human-mashing
//! pace until you're in, then stops by itself. It only ever clicks while the
//! team screen is open, so nothing here touches an actual match.

use crate::settings::Team;
use crate::{hooks, win, Core};
use serde_json::json;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Radius of the patch sampled around a JOIN button (31×31 px at the size it was saved).
pub const R: i32 = 15;
/// Steady 10 clicks a second: about what a person mashing the mouse manages.
pub const INTERVAL: Duration = Duration::from_millis(100);
/// Gives up after this long so it never runs unattended forever.
pub const MAX_TIME: Duration = Duration::from_secs(600);
/// The team screen has to be gone this long before we decide you're in.
const GONE_FOR: Duration = Duration::from_millis(2000);
/// This many of your own mouse movements away from the card = you took over.
const MOVES_TO_STOP: u32 = 3;

/// WARDOGS' SELECT FACTION panel, mapped from a 16:9 capture: each faction
/// card's emblem as (team, offset from window centre ÷ window height, x and y,
/// emblem hue in degrees). The panel is centred and scales with window
/// height, so this fits any resolution or aspect ratio.
pub const DEFAULT_LAYOUT: &[(&str, f32, f32, f32)] = &[
    ("lonestar", -0.1383, -0.0071, 202.0),
    ("valkyra", 0.0013, -0.0102, 5.0),
    ("manticore", 0.1397, -0.0062, 139.0),
];

/// Everything one join run needs, resolved for the current window size.
pub struct Plan {
    pub team: String,
    pub name: String,
    pub x: i32,
    pub y: i32,
    /// What the button looked like when it was saved, if it was saved on this PC.
    pub reference: Option<Vec<f32>>,
    /// Current window height ÷ height when saved.
    pub scale: f32,
    /// Game window client area (left, top, width, height).
    pub window: (i32, i32, i32, i32),
    /// Spot came from the built-in map (so the panel must be recognised to click).
    pub builtin: bool,
}

fn lum(c: [u8; 3]) -> f32 {
    0.299 * c[0] as f32 + 0.587 * c[1] as f32 + 0.114 * c[2] as f32
}

fn mean_std(v: &[f32]) -> (f32, f32) {
    let n = v.len().max(1) as f32;
    let m = v.iter().sum::<f32>() / n;
    let var = v.iter().map(|x| (x - m).powi(2)).sum::<f32>() / n;
    (m, var.sqrt())
}

/// Normalised cross-correlation: 1.0 = same shape regardless of brightness,
/// so a hover or greyed-out "team full" state still counts as the same button.
/// None if the reference has no detail to compare.
fn ncc(reference: &[f32], live: &[f32]) -> Option<f32> {
    if reference.is_empty() || reference.len() != live.len() {
        return None;
    }
    let (mr, sr) = mean_std(reference);
    let (ml, sl) = mean_std(live);
    if sr < 3.0 {
        return None;
    }
    if sl < 1.0 {
        return Some(0.0);
    }
    let n = reference.len() as f32;
    let cov = reference.iter().zip(live).map(|(a, b)| (a - mr) * (b - ml)).sum::<f32>() / n;
    Some(cov / (sr * sl))
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()).collect()
}

/// The patch around (x, y) at `scale` × the saved size, resampled to 31×31 luminance.
fn sample(x: i32, y: i32, scale: f32) -> Vec<f32> {
    let side = (2 * R + 1) as usize;
    if (scale - 1.0).abs() < 0.04 {
        return win::grab(x, y, R).into_iter().map(lum).collect();
    }
    let r2 = ((R as f32) * scale).round().max(1.0) as i32;
    let px = win::grab(x, y, r2);
    let w2 = (2 * r2 + 1) as usize;
    if px.len() != w2 * w2 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(side * side);
    for j in 0..side {
        let sy = (j * (w2 - 1) + (side - 1) / 2) / (side - 1);
        for i in 0..side {
            let sx = (i * (w2 - 1) + (side - 1) / 2) / (side - 1);
            out.push(lum(px[sy * w2 + sx]));
        }
    }
    out
}

/// Remembers where the JOIN button is (relative to the game window) and what
/// it looks like. Returns false if the spot has no detail to recognise.
pub fn calibrate(t: &mut Team, x: i32, y: i32, window: (i32, i32, i32, i32), px: &[[u8; 3]]) -> bool {
    let (left, top, w, h) = window;
    let luma: Vec<u8> = px.iter().map(|&c| lum(c).round() as u8).collect();
    t.x = x;
    t.y = y;
    t.nx = (x as f32 - (left as f32 + w as f32 / 2.0)) / h as f32;
    t.ny = (y as f32 - (top as f32 + h as f32 / 2.0)) / h as f32;
    t.ref_h = h;
    t.calibrated = true;
    t.patch = to_hex(&luma);
    mean_std(&luma.iter().map(|&b| b as f32).collect::<Vec<_>>()).1 >= 3.0
}

/// Screen point for a window-relative spot.
pub fn place(window: (i32, i32, i32, i32), nx: f32, ny: f32) -> (i32, i32) {
    let (left, top, w, h) = window;
    (
        (left as f32 + w as f32 / 2.0 + nx * h as f32).round() as i32,
        (top as f32 + h as f32 / 2.0 + ny * h as f32).round() as i32,
    )
}

/// Some(true/false) when we can tell whether the button is on screen; None if we can't.
fn button_visible(plan: &Plan) -> Option<bool> {
    let reference = plan.reference.as_ref()?;
    ncc(reference, &sample(plan.x, plan.y, plan.scale)).map(|score| score >= 0.45)
}

fn hue_of(c: [u8; 3]) -> Option<f32> {
    let (r, g, b) = (c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0);
    let max = r.max(g).max(b);
    let d = max - r.min(g).min(b);
    if max < 0.35 || d / max < 0.45 {
        return None; // too dark or too grey to call
    }
    let h = if max == r {
        60.0 * (((g - b) / d) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    Some(if h < 0.0 { h + 360.0 } else { h })
}

/// Share of pixels at this window-relative spot that are the emblem's colour.
fn emblem_match(window: (i32, i32, i32, i32), nx: f32, ny: f32, hue: f32) -> f32 {
    let (x, y) = place(window, nx, ny);
    let r = ((window.3 as f32) * 0.025).round().max(4.0) as i32;
    let px = win::grab(x, y, r);
    if px.is_empty() {
        return 0.0;
    }
    let hits = px
        .iter()
        .filter_map(|&c| hue_of(c))
        .filter(|h| {
            let d = (h - hue).abs() % 360.0;
            d.min(360.0 - d) <= 22.0
        })
        .count();
    hits as f32 / px.len() as f32
}

/// The whole SELECT FACTION panel (box, header, three cards) as left, top,
/// right, bottom offsets from the window centre in units of window height.
const PANEL_BOX: (f32, f32, f32, f32) = (-0.2153, -0.1139, 0.2171, 0.1157);
/// Greyscale picture of that panel, shrunk to 96×52. Matching its shape
/// rather than its colours means greyed-out "full" teams don't fool us.
const PANEL_TEMPLATE: &str = include_str!("panel_template.hex");
const TEMPLATE_W: usize = 96;
const TEMPLATE_H: usize = 52;
/// Shape match needed to call the panel open. Tested: real panel 1.0, all
/// three teams greyed + a hovered card ≈ 0.8, mid-match ≈ −0.26.
const PANEL_MATCH: f32 = 0.45;

fn panel_template() -> &'static [f32] {
    static T: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();
    T.get_or_init(|| from_hex(PANEL_TEMPLATE.trim()).into_iter().map(f32::from).collect())
}

/// The panel area on screen right now, shrunk to the template's size.
fn panel_luma(window: (i32, i32, i32, i32)) -> Vec<f32> {
    let (left, top, w, h) = window;
    let (cx, cy, hf) = (left as f32 + w as f32 / 2.0, top as f32 + h as f32 / 2.0, h as f32);
    let (x0, y0) = ((cx + PANEL_BOX.0 * hf).round() as i32, (cy + PANEL_BOX.1 * hf).round() as i32);
    let (x1, y1) = ((cx + PANEL_BOX.2 * hf).round() as i32, (cy + PANEL_BOX.3 * hf).round() as i32);
    let (pw, ph) = ((x1 - x0).max(1) as usize, (y1 - y0).max(1) as usize);
    let px = win::grab_rect(x0, y0, pw as i32, ph as i32);
    if px.len() != pw * ph {
        return Vec::new();
    }
    // Box-average each source block into one template cell.
    let mut sum = vec![0f32; TEMPLATE_W * TEMPLATE_H];
    let mut count = vec![0u32; TEMPLATE_W * TEMPLATE_H];
    for y in 0..ph {
        let ty = y * TEMPLATE_H / ph;
        for x in 0..pw {
            let i = ty * TEMPLATE_W + x * TEMPLATE_W / pw;
            sum[i] += lum(px[y * pw + x]);
            count[i] += 1;
        }
    }
    sum.iter().zip(&count).map(|(s, &n)| if n == 0 { 0.0 } else { s / n as f32 }).collect()
}

/// How closely the screen matches the SELECT FACTION panel's shape (−1…1).
fn panel_score(window: (i32, i32, i32, i32)) -> f32 {
    ncc(panel_template(), &panel_luma(window)).unwrap_or(0.0)
}

/// The SELECT FACTION panel is up when its shape clearly matches, or two
/// faction emblems show in colour, or the shape fairly matches AND at least
/// one emblem is in colour (full teams turn grey, so often only one is).
/// Measured: your real panel shape 0.41 + one colour emblem; loading screen
/// 0.03 / mid-match −0.26 with no colour emblems.
pub fn faction_panel_open(window: (i32, i32, i32, i32)) -> bool {
    let shape = panel_score(window);
    if shape >= PANEL_MATCH {
        return true;
    }
    let coloured = DEFAULT_LAYOUT.iter().filter(|&&(_, nx, ny, hue)| emblem_match(window, nx, ny, hue) >= 0.12).count();
    coloured >= 2 || (coloured >= 1 && shape >= 0.30)
}

pub fn panel_report(window: (i32, i32, i32, i32)) -> String {
    let emblems = DEFAULT_LAYOUT
        .iter()
        .map(|&(id, nx, ny, hue)| format!("{id}={:.2}", emblem_match(window, nx, ny, hue)))
        .collect::<Vec<_>>()
        .join(" ");
    format!("shape={:.2} {emblems}", panel_score(window))
}

/// Appends a line to join-log.txt next to the settings file (kept small).
pub fn log(line: &str) {
    use std::io::Write;
    let path = crate::settings::path().with_file_name("join-log.txt");
    if std::fs::metadata(&path).map(|m| m.len() > 256_000).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "[{:02}:{:02}:{:02} UTC] {line}", (secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    }
}

/// What tells us the team screen is still up during a run (decided at the start).
#[derive(Debug)]
enum Watch {
    /// The saved look of the button on this PC.
    Button,
    /// The SELECT FACTION panel, compared against a snapshot of it taken at the
    /// start on YOUR screen, so teams greying out as they fill barely matter.
    Panel(Vec<f32>),
    /// Nothing reliable: run until you stop it or time runs out.
    Nothing,
}

impl Watch {
    fn label(&self) -> &'static str {
        match self {
            Watch::Button => "saved button",
            Watch::Panel(_) => "faction panel",
            Watch::Nothing => "nothing",
        }
    }
}

fn screen_still_up(plan: &Plan, watch: &Watch) -> bool {
    match watch {
        Watch::Button => button_visible(plan) != Some(false),
        Watch::Panel(snapshot) => {
            ncc(snapshot, &panel_luma(plan.window)).is_some_and(|s| s >= 0.6) || faction_panel_open(plan.window)
        }
        Watch::Nothing => true,
    }
}

/// Is the team screen up? Checked in order of how sure we can be:
/// the saved look of the button, the faction panel's emblems, then (only for
/// spots that aren't built in) whether the mouse pointer is showing.
pub fn team_screen_open(plan: &Plan) -> bool {
    if let Some(visible) = button_visible(plan) {
        return visible;
    }
    if faction_panel_open(plan.window) {
        return true;
    }
    !plan.builtin && win::cursor_visible()
}

/// Clicks until joined, stopped, or something needs you. Returns why it ended.
pub fn run(core: &Arc<Core>, gen: u64, plan: &Plan) -> &'static str {
    let game = hooks::target_pid();
    let (x, y) = (plan.x, plan.y);

    // Never click into a live match: decide up front how we know the team screen is up.
    let watch = if button_visible(plan) == Some(true) {
        Watch::Button
    } else if faction_panel_open(plan.window) {
        Watch::Panel(panel_luma(plan.window))
    } else if !plan.builtin && plan.reference.is_none() && win::cursor_visible() {
        Watch::Nothing
    } else {
        log(&format!(
            "{}: not started, team screen not recognised at ({x},{y}) window {:?} panel [{}] cursor_visible={}",
            plan.team,
            plan.window,
            panel_report(plan.window),
            win::cursor_visible()
        ));
        return "closed";
    };
    log(&format!(
        "{}: start at ({x},{y}) window {:?} watching {} panel [{}]",
        plan.team,
        plan.window,
        watch.label(),
        panel_report(plan.window)
    ));

    // Hover properly first so the game registers the card under the cursor.
    win::set_cursor(x, y);
    sleep(Duration::from_millis(15));
    win::move_rel(1, 0);
    sleep(Duration::from_millis(8));
    win::move_rel(-1, 0);
    sleep(Duration::from_millis(8));
    win::set_cursor(x, y);
    sleep(Duration::from_millis(30));
    hooks::watch_mouse(true, x, y);

    win::time_begin();
    let start = Instant::now();
    let mut next = Instant::now();
    let mut last_check = Instant::now();
    let mut last_emit = Instant::now();
    let mut gone_since: Option<Instant> = None;
    let mut clicks = 0u64;

    let reason = loop {
        if core.join_gen.load(SeqCst) != gen {
            break "stopped";
        }
        if start.elapsed() > MAX_TIME {
            break "timeout";
        }
        if game == 0 || win::foreground_pid() != game {
            break "left";
        }
        if win::pid_at(x, y) != game {
            break "covered";
        }
        if hooks::mouse_moves_away() >= MOVES_TO_STOP {
            break "moved"; // you grabbed the mouse
        }

        // Put the cursor back on the card in case the game nudged it, then click.
        win::set_cursor(x, y);
        win::left_down();
        sleep(Duration::from_millis(12));
        win::left_up();
        clicks += 1;
        core.join_clicks.store(clicks, SeqCst);

        if last_check.elapsed() >= Duration::from_millis(250) {
            last_check = Instant::now();
            // The team screen has to stay gone a while: a "team full" flash or
            // a frame mid-transition shouldn't count as being in.
            if screen_still_up(plan, &watch) {
                gone_since = None;
            } else if gone_since.get_or_insert_with(Instant::now).elapsed() >= GONE_FOR {
                break "joined";
            }
        }

        if last_emit.elapsed() >= Duration::from_millis(200) {
            last_emit = Instant::now();
            core.emit("join", json!({ "active": true, "team": plan.team, "clicks": clicks }));
        }

        next += INTERVAL;
        let now = Instant::now();
        if now > next + INTERVAL {
            next = now;
        }
        if next > now {
            sleep(next - now);
        }
    };
    let moves = hooks::mouse_moves_away();
    hooks::watch_mouse(false, 0, 0);
    win::time_end();
    log(&format!(
        "{}: stopped ({reason}) after {clicks} clicks in {:.1}s, cursor at {:?}, your mouse moves away={moves}, panel [{}]",
        plan.team,
        start.elapsed().as_secs_f32(),
        win::cursor_pos(),
        panel_report(plan.window)
    ));
    reason
}

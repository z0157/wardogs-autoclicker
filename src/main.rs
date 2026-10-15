#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod hooks;
mod join;
mod party;
mod revive;
mod settings;
mod sprint;
mod steam;
mod win;

use serde::Serialize;
use serde_json::{json, Value};
use settings::Settings;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewWindow};

pub const WINDOW_TITLE: &str = "Wardogs AutoClicker 4.20";

#[derive(Clone, Debug)]
pub enum Capture {
    /// Saving where a team's JOIN button is; `then_join` starts joining right after.
    Position { team: String, then_join: bool },
    Key(String),
    /// Picking the game window by hand. `cancellable` is false until one is picked.
    Target { cancellable: bool },
}

/// The game window everything is locked to.
#[derive(Clone, Debug, Serialize)]
pub struct GameTarget {
    pub hwnd: isize,
    pub pid: u32,
    pub process: String,
    pub title: String,
    /// "front", "back" or "closed"
    pub state: String,
}

impl GameTarget {
    fn name(&self) -> String {
        if !self.title.is_empty() {
            self.title.clone()
        } else {
            self.process.trim_end_matches(".exe").to_string()
        }
    }
}

/// WARDOGS' own window (exe name, or the bare game title; not a browser tab about it).
fn is_wardogs(exe: &str, title: &str) -> bool {
    exe.to_ascii_lowercase().contains("wardogs") || title.eq_ignore_ascii_case("wardogs")
}

pub struct Core {
    pub app: AppHandle,
    settings: Mutex<Settings>,
    // team join
    pub join_gen: AtomicU64,
    join_active: AtomicBool,
    join_team: Mutex<Option<String>>,
    pub join_clicks: AtomicU64,
    /// Bumped to cancel a pending "follow the leader" join.
    follow_gen: AtomicU64,
    // party
    pub party: Mutex<Option<party::Party>>,
    pub party_gen: AtomicU64,
    // sprint & revive
    sprint_tx: mpsc::Sender<sprint::Msg>,
    pub sprint_holding: AtomicBool,
    pub revive_gen: AtomicU64,
    pub revive_active: AtomicBool,
    pub revive_key: Mutex<Option<u32>>,
    // capture & game window
    capture: Mutex<Option<Capture>>,
    capture_gen: AtomicU64,
    target: Mutex<Option<GameTarget>>,
    hidden_for_capture: AtomicBool,
}

impl Core {
    pub fn s(&self) -> MutexGuard<'_, Settings> {
        self.settings.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn snapshot(&self) -> Settings {
        self.s().clone()
    }

    fn target(&self) -> Option<GameTarget> {
        self.target.lock().unwrap().clone()
    }

    fn game_name(&self) -> String {
        self.target().map(|t| t.name()).unwrap_or_else(|| "WARDOGS".into())
    }

    /// Name shown to your party: yours if set, else your Steam name.
    pub fn player_name(&self) -> String {
        let set = self.s().general.name.clone();
        if !set.is_empty() {
            return set;
        }
        steam::persona_name().or_else(|| std::env::var("USERNAME").ok()).unwrap_or_else(|| "Player".into())
    }

    fn team_name(&self, id: &str) -> String {
        self.s().teams.iter().find(|t| t.id == id).map(|t| t.name.clone()).unwrap_or_else(|| id.to_string())
    }

    /// The built-in spot for a faction card on WARDOGS' SELECT FACTION panel.
    fn builtin_spot(id: &str) -> Option<(f32, f32)> {
        join::DEFAULT_LAYOUT.iter().find(|(t, ..)| *t == id).map(|&(_, nx, ny, _)| (nx, ny))
    }

    /// A spot the party leader saved and shared (followers only).
    fn leader_spot(&self, id: &str) -> Option<(f32, f32)> {
        self.party
            .lock()
            .unwrap()
            .as_ref()
            .filter(|p| !p.leader)
            .and_then(|p| p.layout.iter().find(|s| s.id == id).map(|s| (s.nx, s.ny)))
    }

    /// Where to click for a team on the current screen, or None if we don't know.
    /// Your own saved spot wins, then the built-in map, then your leader's.
    fn plan_for(&self, id: &str) -> Option<join::Plan> {
        let game = self.target()?;
        let window = win::client_rect(game.hwnd)?;
        let leader = self.leader_spot(id); // before locking settings (lock order: party, then settings)
        let s = self.s();
        let t = s.teams.iter().find(|t| t.id == id)?;
        let saved_look = || {
            let r = join::from_hex(&t.patch);
            (!r.is_empty()).then(|| r.into_iter().map(f32::from).collect::<Vec<f32>>())
        };
        let mut builtin = false;
        let (x, y, reference, scale) = if t.calibrated && t.ref_h > 0 {
            let (x, y) = join::place(window, t.nx, t.ny);
            (x, y, saved_look(), window.3 as f32 / t.ref_h as f32)
        } else if t.calibrated {
            (t.x, t.y, saved_look(), 1.0)
        } else if let Some((nx, ny)) = Self::builtin_spot(id) {
            builtin = true;
            let (x, y) = join::place(window, nx, ny);
            (x, y, None, 1.0)
        } else {
            let (nx, ny) = leader?;
            let (x, y) = join::place(window, nx, ny);
            (x, y, None, 1.0)
        };
        Some(join::Plan { team: t.id.clone(), name: t.name.clone(), x, y, reference, scale, window, builtin })
    }

    pub fn emit<P: serde::Serialize + Clone>(&self, event: &str, payload: P) {
        let _ = self.app.emit(event, payload);
    }

    pub fn toast(&self, msg: impl Into<String>) {
        self.emit("toast", msg.into());
    }

    pub fn chime(&self, on: bool) {
        if self.s().general.sound {
            win::chime(on);
        }
    }

    fn window(&self) -> Option<WebviewWindow> {
        self.app.get_webview_window("main")
    }

    fn minimize(&self) {
        if let Some(w) = self.window() {
            let _ = w.minimize();
        }
    }

    fn restore(&self) {
        if let Some(w) = self.window() {
            let _ = w.unminimize();
            let _ = w.show();
            let _ = w.set_focus();
        }
    }

    fn join_json(&self) -> Value {
        json!({
            "active": self.join_active.load(SeqCst),
            "team": *self.join_team.lock().unwrap(),
            "clicks": self.join_clicks.load(SeqCst),
        })
    }

    fn state_json(&self) -> Value {
        let ready: Vec<String> =
            join::DEFAULT_LAYOUT.iter().map(|(id, ..)| id.to_string()).collect();
        json!({
            "settings": self.snapshot(),
            "party": party::state_json(self),
            "shared_spots": ready,
            "steam_name": steam::persona_name(),
            "target": self.target(),
            "picking": matches!(*self.capture.lock().unwrap(), Some(Capture::Target { .. })),
            "join": self.join_json(),
            "holding": self.sprint_holding.load(SeqCst),
            "reviving": self.revive_active.load(SeqCst),
            "rate": 1000 / join::INTERVAL.as_millis() as u64,
            "settings_path": settings::path().to_string_lossy(),
        })
    }

    // ------------------------------------------------------------ settings

    fn after_change(&self, old: &Settings, new: &Settings, notify_ui: bool) {
        settings::save(new);
        sync_hooks(new);
        let _ = self.sprint_tx.send(sprint::Msg::Config(new.sprint.clone()));
        if old.general.always_on_top != new.general.always_on_top {
            if let Some(w) = self.window() {
                let _ = w.set_always_on_top(new.general.always_on_top);
            }
        }
        if old.selected != new.selected || old.teams.iter().zip(&new.teams).any(|(a, b)| a.nx != b.nx || a.ny != b.ny) {
            party::team_changed(self); // leader: share the pick / spots right away
        }
        if old.general.name != new.general.name {
            party::rename(self);
        }
        if notify_ui {
            self.emit("settings", new.clone());
        }
    }

    /// Backend-side change (hotkeys, positions…); pushes the result to the UI.
    pub fn mutate<R>(&self, f: impl FnOnce(&mut Settings) -> R) -> R {
        let (old, new, r) = {
            let mut g = self.s();
            let old = g.clone();
            let r = f(&mut g);
            g.sanitize();
            (old, g.clone(), r)
        };
        self.after_change(&old, &new, true);
        r
    }

    /// UI-side change: merges a partial JSON object into the current settings.
    fn patch(&self, patch: Value) -> Result<Settings, String> {
        let (old, new) = {
            let mut g = self.s();
            let mut v = serde_json::to_value(&*g).map_err(|e| e.to_string())?;
            settings::merge(&mut v, &patch);
            let mut s: Settings = serde_json::from_value(v).map_err(|e| e.to_string())?;
            s.sanitize();
            let old = std::mem::replace(&mut *g, s.clone());
            (old, s)
        };
        self.after_change(&old, &new, false);
        Ok(new)
    }

    // ------------------------------------------------------------ game window

    fn emit_target(&self) {
        self.emit("target", self.target());
    }

    /// Locks onto a game window.
    fn attach(&self, hwnd: isize, show_ui: bool) {
        let pid = win::pid_of(hwnd);
        let t = GameTarget {
            hwnd,
            pid,
            process: win::process_name(pid),
            title: win::window_title(hwnd),
            state: "front".into(),
        };
        let name = t.name();
        *self.target.lock().unwrap() = Some(t);
        hooks::set_target_pid(pid);
        self.emit("picker", json!({ "active": false }));
        self.emit_target();
        if show_ui {
            self.restore();
            self.toast(format!("Connected to {name}"));
        }
    }

    fn find_wardogs() -> Option<isize> {
        win::find_window(&is_wardogs)
    }

    /// Waits for WARDOGS (found automatically when it's running), or lets you
    /// click a window to use instead.
    fn begin_target_select(self: &Arc<Self>) {
        let cancellable = self.target().is_some();
        self.take_capture();
        self.stop_join();
        *self.capture.lock().unwrap() = Some(Capture::Target { cancellable });
        hooks::set_target_cancellable(cancellable);
        hooks::set_hover(false, 0);
        hooks::set_capture(hooks::CAP_TARGET);
        let gen = self.capture_gen.fetch_add(1, SeqCst) + 1;
        self.emit("picker", json!({ "active": true, "cancellable": cancellable }));
        if cancellable {
            self.restore();
        }

        // Keeps the hook's "can this window be picked?" answer fresh, feeds the
        // live readout, and connects by itself as soon as WARDOGS is running.
        let me = self.clone();
        thread::spawn(move || {
            let mut last: (isize, bool) = (-1, false);
            let mut last_search = Instant::now() - Duration::from_secs(5);
            while me.capture_gen.load(SeqCst) == gen && matches!(*me.capture.lock().unwrap(), Some(Capture::Target { .. })) {
                if !cancellable && last_search.elapsed() >= Duration::from_secs(1) {
                    last_search = Instant::now();
                    if let Some(h) = Self::find_wardogs() {
                        if matches!(me.take_capture(), Some(Capture::Target { .. })) {
                            me.attach(h, true);
                        }
                        break;
                    }
                }
                let (x, y) = win::cursor_pos();
                let root = win::root_at(x, y);
                let pid = win::pid_of(root);
                let own = pid == win::own_pid();
                let shell = root != 0 && win::is_shell_window(root);
                let ok = root != 0 && pid != 0 && !own && !shell;
                hooks::set_hover(ok, root);
                if (root, ok) != last {
                    last = (root, ok);
                    let process = if ok { win::process_name(pid) } else { String::new() };
                    let title = if ok { win::window_title(root) } else { String::new() };
                    me.emit("target-hover", json!({ "ok": ok, "own": own, "shell": shell, "process": process, "title": title }));
                }
                thread::sleep(Duration::from_millis(30));
            }
            hooks::set_hover(false, 0);
        });
    }

    fn finish_target(self: &Arc<Self>, hint_root: isize, x: i32, y: i32) {
        if !matches!(self.take_capture(), Some(Capture::Target { .. })) {
            return;
        }
        // Re-check at the exact click point in case the cursor moved since the last poll.
        let root = match win::root_at(x, y) {
            0 => hint_root,
            r => r,
        };
        let pid = win::pid_of(root);
        if root == 0 || pid == 0 || pid == win::own_pid() || win::is_shell_window(root) {
            self.toast("That's not a game window, click inside WARDOGS");
            self.begin_target_select();
            return;
        }
        self.attach(root, true);
    }

    /// Tracks whether the game is in front, and re-attaches automatically if
    /// it's closed and started again (same exe, new window).
    fn spawn_target_monitor(self: &Arc<Self>) {
        let me = self.clone();
        thread::spawn(move || {
            let mut last_search = Instant::now() - Duration::from_secs(5);
            loop {
                thread::sleep(Duration::from_millis(250));
                let Some(mut t) = me.target() else { continue };
                let alive = win::is_window(t.hwnd) && win::pid_of(t.hwnd) == t.pid && win::pid_alive(t.pid);
                if !alive && last_search.elapsed() >= Duration::from_secs(1) {
                    last_search = Instant::now();
                    let exe = t.process.clone();
                    if let Some(h) = win::find_window(&|p, _| p.eq_ignore_ascii_case(&exe)) {
                        t.hwnd = h;
                        t.pid = win::pid_of(h);
                        hooks::set_target_pid(t.pid);
                    }
                }
                let alive = win::is_window(t.hwnd) && win::pid_of(t.hwnd) == t.pid;
                let state = if !alive {
                    "closed"
                } else if win::foreground_pid() == t.pid {
                    "front"
                } else {
                    "back"
                };
                let title = if alive { win::window_title(t.hwnd) } else { t.title.clone() };
                let changed = state != t.state || (!title.is_empty() && title != t.title);
                if changed || !alive {
                    let mut g = me.target.lock().unwrap();
                    if let Some(cur) = g.as_mut() {
                        if cur.process == t.process {
                            cur.hwnd = t.hwnd;
                            cur.pid = t.pid;
                            cur.state = state.into();
                            if !title.is_empty() {
                                cur.title = title;
                            }
                        }
                    }
                }
                if changed {
                    me.emit_target();
                }
            }
        });
    }

    // ------------------------------------------------------------ team join

    fn stop_join(&self) {
        self.join_gen.fetch_add(1, SeqCst);
    }

    fn toggle_join(self: &Arc<Self>) {
        if self.join_active.load(SeqCst) {
            self.stop_join();
            party::leader_stop(self);
        } else {
            let id = self.s().selected.clone();
            self.start_join(&id, false);
        }
    }

    // ------------------------------------------------------------ following the party leader

    /// The leader picked a team: show it here too.
    fn follow_team(&self, team: &str) {
        if self.s().selected != team {
            let team = team.to_string();
            self.mutate(|s| s.selected = team);
        }
    }

    /// The leader pressed JOIN. Join the same team as soon as this player's team
    /// screen is open; if we can't recognise it on this PC, ask them to press the key.
    fn follow_join(self: &Arc<Self>, team: &str, leader: &str) {
        self.follow_team(team);
        let name = self.team_name(team);
        let key = settings_key_name(self.s().keys.join);
        let plan = self.plan_for(team);
        // Follow automatically only when we can recognise the team screen on this PC.
        if plan.as_ref().map_or(true, |p| p.reference.is_none() && !p.builtin) {
            self.chime(true);
            self.toast(if plan.is_some() {
                format!("{leader} is joining {name}. Press {key} on the team screen")
            } else {
                format!("{leader} is joining {name}. Set {name}'s JOIN spot to follow automatically")
            });
            return;
        }
        if self.join_active.load(SeqCst) {
            if self.join_team.lock().unwrap().as_deref() == Some(team) {
                return;
            }
            self.stop_join();
        }
        let fgen = self.follow_gen.fetch_add(1, SeqCst) + 1;
        self.chime(true);
        self.toast(format!("{leader} is joining {name}. You'll join as soon as your team screen is open"));
        let me = self.clone();
        let team = team.to_string();
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(60);
            while me.follow_gen.load(SeqCst) == fgen {
                if Instant::now() > deadline {
                    me.toast(format!("Didn't see your team screen. Press {key} to join {name}"));
                    return;
                }
                let in_front = me.target().is_some_and(|g| win::foreground_pid() == g.pid);
                if in_front && me.plan_for(&team).is_some_and(|p| join::team_screen_open(&p)) {
                    if !me.join_active.load(SeqCst) {
                        me.start_join(&team, true);
                    }
                    return;
                }
                thread::sleep(Duration::from_millis(250));
            }
        });
    }

    fn follow_stop(&self) {
        self.follow_gen.fetch_add(1, SeqCst);
        if self.join_active.load(SeqCst) {
            self.stop_join();
            self.toast("Your leader stopped joining");
        }
    }

    /// A team's own key: selects that team and joins it (or stops if it's already joining).
    fn join_team_key(self: &Arc<Self>, id: &str) {
        if self.join_active.load(SeqCst) {
            let current = self.join_team.lock().unwrap().clone();
            self.stop_join();
            if current.as_deref() == Some(id) {
                party::leader_stop(self);
                return;
            }
            // Switching teams: wait for the current run to wind down, then start.
            let me = self.clone();
            let id = id.to_string();
            thread::spawn(move || {
                for _ in 0..40 {
                    if !me.join_active.load(SeqCst) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                me.start_join(&id, false);
            });
            return;
        }
        self.start_join(id, false);
    }

    /// `from_party`: started because the party leader pressed JOIN (don't re-broadcast).
    fn start_join(self: &Arc<Self>, id: &str, from_party: bool) {
        let Some(game) = self.target() else {
            self.toast("Start WARDOGS first");
            return;
        };
        if self.s().selected != id {
            let id = id.to_string();
            self.mutate(|s| s.selected = id);
        }
        let Some(plan) = self.plan_for(id) else {
            if from_party {
                self.toast(format!("Set {}'s JOIN spot to follow your leader", self.team_name(id)));
            } else {
                // First time for this team: save where its JOIN button is, then go.
                let _ = self.start_position(id, true);
            }
            return;
        };
        if self.join_active.swap(true, SeqCst) {
            return;
        }
        let gen = self.join_gen.fetch_add(1, SeqCst) + 1;
        *self.join_team.lock().unwrap() = Some(plan.team.clone());
        self.join_clicks.store(0, SeqCst);
        self.emit("join", self.join_json());
        self.chime(true);
        if !from_party {
            party::leader_join(self, &plan.team); // only does anything if you lead a party
        }

        let me = self.clone();
        thread::spawn(move || {
            if win::foreground_pid() != game.pid {
                // Started from our window: bring the game forward first.
                me.minimize();
                win::bring_to_front(game.hwnd);
                thread::sleep(Duration::from_millis(350));
            }
            let reason = if me.join_gen.load(SeqCst) != gen {
                "stopped"
            } else if win::foreground_pid() != game.pid {
                "left"
            } else {
                join::run(&me, gen, &plan)
            };
            me.join_active.store(false, SeqCst);
            *me.join_team.lock().unwrap() = None;
            if reason != "stopped" {
                party::leader_done(&me);
            }
            let clicks = me.join_clicks.load(SeqCst);
            me.emit("join", json!({ "active": false, "team": plan.team, "clicks": clicks, "reason": reason }));
            me.chime(false);
            let game_name = game.name();
            let key = settings_key_name(me.s().keys.join);
            match reason {
                "joined" => me.toast(format!("Team screen closed, you're in {}", plan.name)),
                "moved" => me.toast("Stopped: you moved the mouse"),
                "left" => me.toast(format!("Stopped: {game_name} isn't in front")),
                "covered" => me.toast("Stopped: something is covering the team card"),
                "timeout" => me.toast("Stopped after 10 minutes"),
                "closed" => me.toast(format!("The team screen isn't open. Open it, then press {key}")),
                _ => {}
            }
        });
    }

    // ------------------------------------------------------------ sprint & revive

    fn toggle_sprint(&self) {
        let (on, run) = self.mutate(|s| {
            s.sprint.enabled = !s.sprint.enabled;
            (s.sprint.enabled, s.sprint.mode == settings::SprintMode::Run)
        });
        self.chime(on);
        self.toast(match (on, run) {
            (true, true) => "Auto-run on",
            (false, true) => "Auto-run off",
            (true, false) => "Auto sprint on",
            (false, false) => "Auto sprint off",
        });
    }

    /// Auto-run stopped itself (you moved, left the game, started a revive…).
    pub fn autorun_off(&self, why: &str) {
        let was_on = self.mutate(|s| std::mem::replace(&mut s.sprint.enabled, false));
        if was_on {
            self.chime(false);
            self.toast(format!("Auto-run off: {why}"));
        }
    }

    /// Starts the timed revive hold, or cancels it if one is running.
    fn toggle_revive(self: &Arc<Self>) {
        if self.revive_active.load(SeqCst) {
            self.revive_gen.fetch_add(1, SeqCst); // the running hold sees this and lets go
            return;
        }
        let Some(game) = self.target() else {
            self.toast("Start WARDOGS first");
            return;
        };
        // You can't revive while running: stop auto-run first.
        let running = {
            let s = self.s();
            s.sprint.enabled && s.sprint.mode == settings::SprintMode::Run
        };
        if running {
            self.autorun_off("reviving");
            thread::sleep(Duration::from_millis(60)); // let W + Shift come up before F goes down
        }
        let gen = self.revive_gen.fetch_add(1, SeqCst) + 1;
        self.revive_active.store(true, SeqCst);
        let me = self.clone();
        thread::spawn(move || {
            if win::foreground_pid() != game.pid {
                me.minimize();
                win::bring_to_front(game.hwnd);
                thread::sleep(Duration::from_millis(350));
            }
            if win::foreground_pid() != game.pid || me.revive_gen.load(SeqCst) != gen {
                me.revive_active.store(false, SeqCst);
                me.emit("revive", json!({ "active": false }));
                return;
            }
            revive::run(me, gen);
        });
    }

    // ------------------------------------------------------------ capture (positions / rebind)

    fn take_capture(&self) -> Option<Capture> {
        let c = self.capture.lock().unwrap().take();
        hooks::set_capture(hooks::CAP_NONE);
        if c.is_some() {
            self.emit("capture", json!({ "active": false }));
        }
        c
    }

    fn restore_after_capture(&self) {
        if self.hidden_for_capture.swap(false, SeqCst) {
            self.restore();
        }
    }

    fn cancel_capture(self: &Arc<Self>) {
        match self.take_capture() {
            Some(Capture::Target { cancellable: false }) => self.begin_target_select(),
            Some(Capture::Target { cancellable: true }) => {
                self.emit("picker", json!({ "active": false }));
                self.restore();
            }
            _ => self.restore_after_capture(),
        }
    }

    /// Asks you to click a team's JOIN button once so its position can be saved.
    fn start_position(self: &Arc<Self>, id: &str, then_join: bool) -> Result<(), String> {
        let Some(game) = self.target() else { return Err("Start WARDOGS first".into()) };
        let name = self.s().teams.iter().find(|t| t.id == id).map(|t| t.name.clone()).ok_or("Unknown team")?;
        self.take_capture();
        *self.capture.lock().unwrap() = Some(Capture::Position { team: id.to_string(), then_join });
        hooks::set_capture(hooks::CAP_POINT);
        let gen = self.capture_gen.fetch_add(1, SeqCst) + 1;
        self.emit(
            "capture",
            json!({
                "active": true,
                "kind": "point",
                "team": id,
                "title": format!("Set {name}'s JOIN button"),
                "text": format!("Open the team screen in {} and click {name}'s JOIN button once. That click is saved, not sent to the game.", game.name()),
            }),
        );
        self.hidden_for_capture.store(true, SeqCst);
        self.minimize();
        win::bring_to_front(game.hwnd);

        let me = self.clone();
        thread::spawn(move || {
            while me.capture_gen.load(SeqCst) == gen && matches!(*me.capture.lock().unwrap(), Some(Capture::Position { .. })) {
                let (x, y) = win::cursor_pos();
                me.emit("capture-live", json!({ "x": x, "y": y, "inside": win::pid_at(x, y) == game.pid }));
                thread::sleep(Duration::from_millis(60));
            }
        });
        Ok(())
    }

    fn finish_position(self: &Arc<Self>, x: i32, y: i32) {
        let Some(Capture::Position { team, then_join }) = self.take_capture() else { return };
        if win::pid_at(x, y) != hooks::target_pid() {
            self.toast(format!("That's not in {}, click the JOIN button inside the game", self.game_name()));
            let _ = self.start_position(&team, then_join);
            return;
        }
        let Some(window) = self.target().and_then(|g| win::client_rect(g.hwnd)) else {
            self.toast("Couldn't read the game window, try again");
            self.restore_after_capture();
            return;
        };
        // Sample before our window comes back and covers the spot.
        let px = win::grab(x, y, join::R);
        let (name, detailed) = self.mutate(|s| {
            s.teams
                .iter_mut()
                .find(|t| t.id == team)
                .map(|t| (t.name.clone(), join::calibrate(t, x, y, window, &px)))
                .unwrap_or_default()
        });
        if then_join {
            self.hidden_for_capture.store(false, SeqCst);
            self.start_join(&team, false);
        } else {
            self.toast(if detailed {
                format!("{name}'s JOIN button saved")
            } else {
                format!("{name} saved. Tip: click on the button's text so it can tell when you're in")
            });
            self.restore_after_capture();
        }
    }

    fn start_rebind(&self, action: String) {
        self.take_capture();
        let single = is_plain_key_action(&action);
        *self.capture.lock().unwrap() = Some(Capture::Key(action.clone()));
        hooks::set_capture(if single { hooks::CAP_KEY_SINGLE } else { hooks::CAP_KEY });
        self.emit(
            "capture",
            json!({
                "active": true,
                "kind": "key",
                "action": action,
                "text": if single { "Press the new key · Esc cancels" }
                        else { "Press a key or combo (Ctrl + F6, Mouse 4…) · Backspace clears · Esc cancels" },
            }),
        );
    }

    fn finish_key(&self, code: Option<u32>) {
        let Some(Capture::Key(action)) = self.take_capture() else { return };
        if let Err(e) = self.mutate(|s| assign_key(s, &action, code)) {
            self.toast(e);
        }
    }

    // ------------------------------------------------------------ hook events

    fn on_hook(self: &Arc<Self>, ev: hooks::HookEvent) {
        use hooks::HookEvent::*;
        match ev {
            Hotkey(code) => self.on_hotkey(code),
            ForwardDown => drop(self.sprint_tx.send(sprint::Msg::ForwardDown)),
            ForwardUp => drop(self.sprint_tx.send(sprint::Msg::ForwardUp)),
            BackDown => drop(self.sprint_tx.send(sprint::Msg::BackDown)),
            SprintKeyUp => drop(self.sprint_tx.send(sprint::Msg::SprintKeyUp)),
            CaptureKey(code) => self.finish_key(Some(code)),
            CaptureClear => self.finish_key(None),
            CapturePoint(x, y) => self.finish_position(x, y),
            CaptureTarget(root, x, y) => self.finish_target(root, x, y),
            CaptureCancel => self.cancel_capture(),
        }
    }

    fn on_hotkey(self: &Arc<Self>, code: u32) {
        let s = self.snapshot();
        if code == s.keys.join {
            self.toggle_join();
        } else if code == s.sprint.hotkey {
            self.toggle_sprint();
        } else if code == s.revive.hotkey {
            self.toggle_revive();
        } else if let Some(t) = s.teams.iter().find(|t| t.hotkey == code) {
            self.join_team_key(&t.id);
        }
    }

    /// Lets go of anything we're holding so nothing stays stuck after exit.
    fn shutdown(&self) {
        if self.party.lock().unwrap().is_some() {
            party::leave(self);
            thread::sleep(Duration::from_millis(400)); // let the "bye" go out
        }
        if self.join_active.load(SeqCst) {
            self.stop_join();
            win::left_up();
        }
        let _ = self.sprint_tx.send(sprint::Msg::Shutdown);
        if self.sprint_holding.swap(false, SeqCst) {
            win::key(self.s().sprint.sprint_key, true);
        }
        self.revive_gen.fetch_add(1, SeqCst);
        if let Some(k) = self.revive_key.lock().unwrap().take() {
            win::key(k, true);
        }
    }
}

/// Short key name for messages ("F6", "Ctrl + F6").
fn settings_key_name(code: u32) -> String {
    let (vk, mods) = (code & 0xFF, code >> 8);
    if vk == 0 {
        return "its key".into();
    }
    let mut name = String::new();
    for (bit, label) in [(1, "Ctrl + "), (2, "Alt + "), (4, "Shift + "), (8, "Win + ")] {
        if mods & bit != 0 {
            name.push_str(label);
        }
    }
    name.push_str(&match vk {
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x30..=0x39 | 0x41..=0x5A => (vk as u8 as char).to_string(),
        0x04 => "Middle click".into(),
        0x05 => "Mouse 4".into(),
        0x06 => "Mouse 5".into(),
        0x0D => "Enter".into(),
        0x20 => "Space".into(),
        _ => format!("key {vk}"),
    });
    name
}

fn sync_hooks(s: &Settings) {
    let mut codes = vec![s.keys.join, s.sprint.hotkey, s.revive.hotkey];
    codes.extend(s.teams.iter().map(|t| t.hotkey));
    hooks::configure(&codes, s.sprint.forward_key, s.sprint.back_key, s.sprint.sprint_key, s.keys.capture);
}

/// Keys used as-is (pressed or held for you) rather than as hotkeys.
fn is_plain_key_action(action: &str) -> bool {
    matches!(action, "capture" | "sprint_key" | "forward_key" | "back_key" | "revive_key")
}

fn hotkey_slot<'a>(s: &'a mut Settings, action: &str) -> Option<&'a mut u32> {
    match action {
        "join" => Some(&mut s.keys.join),
        "sprint" => Some(&mut s.sprint.hotkey),
        "revive" => Some(&mut s.revive.hotkey),
        a if a.starts_with("team:") => s.teams.iter_mut().find(|t| t.id == a[5..]).map(|t| &mut t.hotkey),
        _ => None,
    }
}

fn clear_hotkey(s: &mut Settings, code: u32) {
    for slot in [&mut s.keys.join, &mut s.sprint.hotkey, &mut s.revive.hotkey] {
        if *slot == code {
            *slot = 0;
        }
    }
    for t in &mut s.teams {
        if t.hotkey == code {
            t.hotkey = 0;
        }
    }
}

fn assign_key(s: &mut Settings, action: &str, code: Option<u32>) -> Result<(), String> {
    if is_plain_key_action(action) {
        let Some(vk) = code else { return Ok(()) };
        if (1..=6).contains(&vk) {
            return Err("Pick a keyboard key for this one".into());
        }
        match action {
            "sprint_key" => {
                clear_hotkey(s, vk);
                s.sprint.sprint_key = vk;
            }
            "forward_key" => {
                clear_hotkey(s, vk);
                s.sprint.forward_key = vk;
            }
            "back_key" => {
                clear_hotkey(s, vk);
                s.sprint.back_key = vk;
            }
            "revive_key" => s.revive.key = vk,
            _ => s.keys.capture = vk,
        }
        return Ok(());
    }
    let code = code.unwrap_or(0);
    if code != 0 && (code == s.sprint.forward_key || code == s.sprint.sprint_key || code == s.sprint.back_key) {
        return Err("That key is used by auto sprint".into());
    }
    if code != 0 {
        clear_hotkey(s, code); // one combo, one job
    }
    *hotkey_slot(s, action).ok_or("Unknown keybind")? = code;
    Ok(())
}

// ---------------------------------------------------------------- commands

#[tauri::command]
fn get_state(core: tauri::State<'_, Arc<Core>>) -> Value {
    core.state_json()
}

#[tauri::command]
fn patch_settings(core: tauri::State<'_, Arc<Core>>, patch: Value) -> Result<Settings, String> {
    core.patch(patch)
}

#[tauri::command]
fn toggle_join(core: tauri::State<'_, Arc<Core>>) {
    core.inner().toggle_join();
}

#[tauri::command]
fn party_create(core: tauri::State<'_, Arc<Core>>) -> String {
    party::create(core.inner())
}

#[tauri::command]
fn party_join(core: tauri::State<'_, Arc<Core>>, code: String) -> Result<String, String> {
    party::join(core.inner(), &code)
}

#[tauri::command]
fn party_leave(core: tauri::State<'_, Arc<Core>>) {
    party::leave(core.inner());
}

#[tauri::command]
fn party_make_leader(core: tauri::State<'_, Arc<Core>>, id: String) {
    party::make_leader(core.inner(), &id);
}

#[tauri::command]
fn set_position(core: tauri::State<'_, Arc<Core>>, id: String) -> Result<(), String> {
    core.inner().start_position(&id, false)
}

#[tauri::command]
fn select_target(core: tauri::State<'_, Arc<Core>>) {
    core.inner().begin_target_select();
}

#[tauri::command]
fn start_rebind(core: tauri::State<'_, Arc<Core>>, action: String) {
    core.start_rebind(action);
}

#[tauri::command]
fn cancel_capture(core: tauri::State<'_, Arc<Core>>) {
    core.inner().cancel_capture();
}

#[tauri::command]
fn win_minimize(window: WebviewWindow) {
    let _ = window.minimize();
}

#[tauri::command]
fn win_close(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
fn ui_ready(window: WebviewWindow) {
    let _ = window.show();
    let _ = window.set_focus();
}

fn main() {
    // Diagnostic: log how well the screen matches the SELECT FACTION panel, then exit.
    if std::env::args().any(|a| a == "--panel-check") {
        let report = match Core::find_wardogs().and_then(win::client_rect) {
            Some(window) => format!("panel check: window {window:?} {}", join::panel_report(window)),
            None => "panel check: WARDOGS window not found".to_string(),
        };
        join::log(&report);
        return;
    }
    if !win::single_instance(WINDOW_TITLE) {
        return;
    }
    if tauri::webview_version().is_err() {
        win::missing_webview2(WINDOW_TITLE);
        return;
    }

    let initial = settings::load();
    let (hook_tx, hook_rx) = mpsc::channel();
    let (sprint_tx, sprint_rx) = mpsc::channel();

    let app = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_state,
            patch_settings,
            toggle_join,
            set_position,
            party_create,
            party_join,
            party_leave,
            party_make_leader,
            select_target,
            start_rebind,
            cancel_capture,
            win_minimize,
            win_close,
            ui_ready
        ])
        .setup(move |app| {
            let core = Arc::new(Core {
                app: app.handle().clone(),
                settings: Mutex::new(initial),
                join_gen: AtomicU64::new(0),
                join_active: AtomicBool::new(false),
                join_team: Mutex::new(None),
                join_clicks: AtomicU64::new(0),
                follow_gen: AtomicU64::new(0),
                party: Mutex::new(None),
                party_gen: AtomicU64::new(0),
                sprint_tx,
                sprint_holding: AtomicBool::new(false),
                revive_gen: AtomicU64::new(0),
                revive_active: AtomicBool::new(false),
                revive_key: Mutex::new(None),
                capture: Mutex::new(None),
                capture_gen: AtomicU64::new(0),
                target: Mutex::new(None),
                hidden_for_capture: AtomicBool::new(false),
            });
            app.manage(core.clone());

            hooks::start(hook_tx);
            sync_hooks(&core.snapshot());
            sprint::spawn(core.clone(), sprint_rx);
            core.spawn_target_monitor();
            let dispatcher = core.clone();
            thread::Builder::new().name("dispatch".into()).spawn(move || {
                for ev in hook_rx {
                    dispatcher.on_hook(ev);
                }
            })?;

            // Connect to WARDOGS straight away if it's running; otherwise wait for it.
            match Core::find_wardogs() {
                Some(h) => core.attach(h, false),
                None => core.begin_target_select(),
            }

            if let Some(w) = app.get_webview_window("main") {
                if let Ok(h) = w.hwnd() {
                    win::round_corners(h.0 as isize);
                }
                let _ = w.set_always_on_top(core.snapshot().general.always_on_top);
                // Safety net: show the window even if the page never reports ready.
                let w2 = w.clone();
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(2500));
                    if !w2.is_visible().unwrap_or(true) {
                        let _ = w2.show();
                    }
                });
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to start Wardogs AutoClicker 4.20");

    app.run(|handle, event| {
        if let RunEvent::Exit = event {
            if let Some(core) = handle.try_state::<Arc<Core>>() {
                core.shutdown();
            }
        }
    });
}

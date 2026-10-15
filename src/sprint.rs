//! Auto sprint. In auto-run mode (the default) it holds the forward AND sprint
//! keys for you, so you sprint forward hands-free until you stop it: press the
//! toggle again, tap forward or back yourself, or leave the game.

use crate::settings::{SprintMode, SprintSettings};
use crate::{hooks, win, Core};
use serde_json::json;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

pub enum Msg {
    ForwardDown,
    ForwardUp,
    /// The back key was pressed (stops auto-run).
    BackDown,
    /// The user physically released the sprint key, which also releases our hold.
    SprintKeyUp,
    Config(SprintSettings),
    Shutdown,
}

struct Sprinter {
    core: Arc<Core>,
    cfg: SprintSettings,
    /// Keys we're holding down right now, in the order they were pressed.
    held: Vec<u32>,
    forward: bool,
}

impl Sprinter {
    /// Only ever act inside the game window.
    fn allowed(&self) -> bool {
        let target = hooks::target_pid();
        target != 0 && win::foreground_pid() == target
    }

    fn hold(&mut self, keys: &[u32]) {
        if !self.held.is_empty() {
            return;
        }
        for (i, &vk) in keys.iter().enumerate() {
            if i > 0 {
                std::thread::sleep(Duration::from_millis(30)); // forward first, then sprint
            }
            win::key(vk, false);
            self.held.push(vk);
        }
        self.publish();
    }

    fn release(&mut self) {
        if self.held.is_empty() {
            return;
        }
        for vk in self.held.drain(..).rev() {
            win::key(vk, true);
        }
        self.publish();
    }

    fn publish(&self) {
        let holding = !self.held.is_empty();
        self.core.sprint_holding.store(holding, SeqCst);
        self.core.emit("sprint", json!({ "holding": holding }));
    }

    /// Auto-run: you pressed forward or back yourself, or left the game.
    fn stop_run(&mut self, why: &str) {
        self.release();
        if self.cfg.enabled {
            self.cfg.enabled = false; // until the settings update comes back round
            self.core.autorun_off(why);
        }
    }

    fn reconcile(&mut self) {
        if self.cfg.mode == SprintMode::Run {
            let in_game = self.allowed();
            if self.cfg.enabled && in_game {
                self.hold(&[self.cfg.forward_key, self.cfg.sprint_key]);
            } else if !self.held.is_empty() && self.cfg.enabled && !in_game {
                // Don't resume running by surprise when you come back.
                self.stop_run("you left the game");
            } else {
                self.release();
            }
            return;
        }
        let want = self.cfg.enabled
            && match self.cfg.mode {
                SprintMode::Hold => self.forward,
                SprintMode::Always => true,
                SprintMode::Tap | SprintMode::Run => false,
            }
            && self.allowed();
        if want {
            self.hold(&[self.cfg.sprint_key]);
        } else {
            self.release();
        }
    }

    fn tap(&self) {
        std::thread::sleep(Duration::from_millis(15));
        win::key(self.cfg.sprint_key, false);
        std::thread::sleep(Duration::from_millis(45));
        win::key(self.cfg.sprint_key, true);
    }
}

pub fn spawn(core: Arc<Core>, rx: Receiver<Msg>) {
    let cfg = core.snapshot().sprint;
    let _ = std::thread::Builder::new().name("sprint".into()).spawn(move || {
        let mut sp = Sprinter { core, cfg, held: Vec::new(), forward: false };
        loop {
            match rx.recv_timeout(Duration::from_millis(150)) {
                Ok(Msg::ForwardDown) => {
                    sp.forward = true;
                    match sp.cfg.mode {
                        SprintMode::Run if sp.cfg.enabled && !sp.held.is_empty() => {
                            sp.stop_run(&format!("you pressed {}", key_label(sp.cfg.forward_key)));
                        }
                        SprintMode::Tap if sp.cfg.enabled && sp.allowed() => sp.tap(),
                        _ => {}
                    }
                }
                Ok(Msg::ForwardUp) => sp.forward = false,
                Ok(Msg::BackDown) => {
                    if sp.cfg.mode == SprintMode::Run && sp.cfg.enabled && !sp.held.is_empty() {
                        sp.stop_run(&format!("you pressed {}", key_label(sp.cfg.back_key)));
                    }
                }
                Ok(Msg::SprintKeyUp) => {
                    if !sp.held.is_empty() {
                        // Your own key-up also let go of ours: let it land, then hold again below.
                        sp.held.clear();
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
                Ok(Msg::Config(c)) => {
                    let changed_keys = c.sprint_key != sp.cfg.sprint_key || c.forward_key != sp.cfg.forward_key;
                    if changed_keys || c.mode != sp.cfg.mode || !c.enabled {
                        sp.release();
                    }
                    sp.cfg = c;
                }
                Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                    sp.release();
                    break;
                }
                // Periodic check guards against a missed key-up leaving a key stuck.
                Err(RecvTimeoutError::Timeout) => {
                    if sp.cfg.mode != SprintMode::Run {
                        sp.forward = win::key_is_down(sp.cfg.forward_key);
                    }
                }
            }
            sp.reconcile();
        }
    });
}

fn key_label(vk: u32) -> String {
    match vk {
        0x30..=0x39 | 0x41..=0x5A => (vk as u8 as char).to_string(),
        0x25 => "←".into(),
        0x26 => "↑".into(),
        0x27 => "→".into(),
        0x28 => "↓".into(),
        _ => "a movement key".into(),
    }
}

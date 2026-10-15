//! Auto revive: holds the revive key (F in WARDOGS) for a set time so you
//! don't have to keep your finger on it. Lets go early if you cancel, move,
//! press the key yourself, or leave the game.

use crate::{hooks, win, Core};
use serde_json::json;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

const VK_A: u32 = 0x41;
const VK_S: u32 = 0x53;
const VK_D: u32 = 0x44;

pub fn run(core: Arc<Core>, gen: u64) {
    let s = core.snapshot();
    let r = s.revive.clone();
    let game = hooks::target_pid();
    let total = Duration::from_millis(r.hold_ms);

    // Movement keys: a fresh press of any of them counts as "I moved".
    let moves: Vec<u32> = [s.sprint.forward_key, VK_A, VK_S, VK_D].into_iter().filter(|&k| k != r.key).collect();
    let mut was_down: Vec<bool> = moves.iter().map(|&k| win::key_is_down(k)).collect();

    win::key(r.key, false);
    *core.revive_key.lock().unwrap() = Some(r.key);
    core.emit("revive", json!({ "active": true, "total_ms": r.hold_ms }));
    core.chime(true);

    let start = Instant::now();
    let reason = loop {
        sleep(Duration::from_millis(25));
        if core.revive_gen.load(SeqCst) != gen {
            break "cancelled";
        }
        if start.elapsed() >= total {
            break "done";
        }
        if game == 0 || win::foreground_pid() != game {
            break "left";
        }
        if !win::key_is_down(r.key) {
            break "taken"; // you pressed and released it yourself
        }
        if r.stop_on_move {
            let now: Vec<bool> = moves.iter().map(|&k| win::key_is_down(k)).collect();
            let moved = now.iter().zip(&was_down).any(|(&n, &w)| n && !w);
            was_down = now;
            if moved {
                break "moved";
            }
        }
    };

    if let Some(k) = core.revive_key.lock().unwrap().take() {
        win::key(k, true);
    }
    core.revive_active.store(false, SeqCst);
    core.emit("revive", json!({ "active": false, "reason": reason }));
    core.chime(false);
    match reason {
        "done" => core.toast("Revive finished"),
        "moved" => core.toast("Revive stopped: you moved"),
        "left" => core.toast("Revive stopped: you left the game"),
        "cancelled" => core.toast("Revive cancelled"),
        _ => {}
    }
}

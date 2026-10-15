//! Global hotkeys, auto-sprint key tracking and click/key capture, all via
//! low-level hooks on a dedicated high-priority thread.
//!
//! Hooks see input even while a game has focus (RegisterHotKey often doesn't),
//! and can bind mouse side buttons and modifier combos. The callbacks only
//! touch atomics and post to a channel, so Windows never times them out and
//! drops the hook.

use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU16, AtomicU32, AtomicU8, Ordering::Relaxed};
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;
use std::time::Duration;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetCursorPos, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId,
    PeekMessageW, PostThreadMessageW, SetTimer, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT,
    LLKHF_INJECTED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOWS_HOOK_ID, WM_APP,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SYSKEYDOWN, WM_TIMER, WM_USER, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

pub enum HookEvent {
    /// A bound hotkey fired: `vk | mods << 8`.
    Hotkey(u32),
    ForwardDown,
    ForwardUp,
    BackDown,
    SprintKeyUp,
    CaptureKey(u32),
    CaptureClear,
    CapturePoint(i32, i32),
    /// The user clicked a window while picking the game: (root window, x, y).
    CaptureTarget(isize, i32, i32),
    CaptureCancel,
}

pub const CAP_NONE: u8 = 0;
/// Rebinding a hotkey: modifier combos allowed.
pub const CAP_KEY: u8 = 1;
pub const CAP_POINT: u8 = 2;
pub const CAP_TARGET: u8 = 3;
/// Rebinding a plain key (sprint / forward / revive / capture): no combos.
pub const CAP_KEY_SINGLE: u8 = 4;

pub const MOD_CTRL: u32 = 1;
pub const MOD_ALT: u32 = 2;
pub const MOD_SHIFT: u32 = 4;
pub const MOD_WIN: u32 = 8;

const VK_BACK: usize = 0x08;
const VK_ESCAPE: usize = 0x1B;
const MODIFIER_VKS: [usize; 8] = [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0x5B, 0x5C];
const WM_REFRESH: u32 = WM_APP + 1;
const LLMHF_ANY_INJECTED: u32 = 0x1 | 0x2;

struct Shared {
    capture: AtomicU8,
    forward: AtomicU32,
    back: AtomicU32,
    sprint: AtomicU32,
    capture_key: AtomicU32,
    /// Process the app is locked to; hotkeys only fire while it (or we) are in front.
    target_pid: AtomicU32,
    own_pid: AtomicU32,
    target_cancellable: AtomicBool,
    /// Whether the window under the cursor can be picked as the game (kept
    /// fresh by a polling thread so the hook itself never calls WindowFromPoint).
    hover_ok: AtomicBool,
    hover_root: AtomicIsize,
    pending_mod: AtomicU32,
    /// While joining: add up how far YOUR mouse moves (injected moves and the
    /// game repositioning the cursor don't count).
    watch_mouse: AtomicBool,
    mouse_travel: AtomicU32,
    anchor_x: AtomicI32,
    anchor_y: AtomicI32,
    /// Per virtual key: bit n set = bound with modifier set n.
    bound: [AtomicU16; 256],
    pressed: [AtomicBool; 256],
    swallow_up: [AtomicBool; 256],
}

static SH: Shared = Shared {
    capture: AtomicU8::new(CAP_NONE),
    forward: AtomicU32::new(0x57),
    back: AtomicU32::new(0x53),
    sprint: AtomicU32::new(0xA0),
    capture_key: AtomicU32::new(0x0D),
    target_pid: AtomicU32::new(0),
    own_pid: AtomicU32::new(0),
    target_cancellable: AtomicBool::new(false),
    hover_ok: AtomicBool::new(false),
    hover_root: AtomicIsize::new(0),
    pending_mod: AtomicU32::new(0),
    watch_mouse: AtomicBool::new(false),
    mouse_travel: AtomicU32::new(0),
    anchor_x: AtomicI32::new(0),
    anchor_y: AtomicI32::new(0),
    bound: [const { AtomicU16::new(0) }; 256],
    pressed: [const { AtomicBool::new(false) }; 256],
    swallow_up: [const { AtomicBool::new(false) }; 256],
};

static TX: OnceLock<Sender<HookEvent>> = OnceLock::new();
static THREAD: AtomicU32 = AtomicU32::new(0);
static KB_HOOK: AtomicIsize = AtomicIsize::new(0);
static MOUSE_HOOK: AtomicIsize = AtomicIsize::new(0);

fn emit(e: HookEvent) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(e);
    }
}

/// Atomically leaves capture `mode`; true if this call is the one that ended it.
fn end_capture(mode: u8) -> bool {
    SH.capture.compare_exchange(mode, CAP_NONE, Relaxed, Relaxed).is_ok()
}

fn mod_bit(vk: usize) -> u32 {
    match vk {
        0x10 | 0xA0 | 0xA1 => MOD_SHIFT,
        0x11 | 0xA2 | 0xA3 => MOD_CTRL,
        0x12 | 0xA4 | 0xA5 => MOD_ALT,
        0x5B | 0x5C => MOD_WIN,
        _ => 0,
    }
}

/// Modifiers physically held right now, ignoring `except` itself.
fn current_mods(except: usize) -> u32 {
    MODIFIER_VKS
        .iter()
        .filter(|&&vk| vk != except && SH.pressed[vk].load(Relaxed))
        .fold(0, |m, &vk| m | mod_bit(vk))
}

fn hotkeys_active() -> bool {
    let target = SH.target_pid.load(Relaxed);
    if target == 0 {
        return false; // nothing works until a game window is picked
    }
    let mut pid = 0u32;
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_null() {
            return false;
        }
        GetWindowThreadProcessId(fg, &mut pid);
    }
    pid == target || pid == SH.own_pid.load(Relaxed)
}

/// Bound combo for `vk` given the held modifiers. An exact match wins; a
/// plain binding still fires while modifiers are held (handy mid-game).
fn match_hotkey(vk: usize) -> Option<u32> {
    let mask = SH.bound[vk].load(Relaxed);
    if mask == 0 || !hotkeys_active() {
        return None;
    }
    let mods = current_mods(vk);
    if mask & (1 << mods) != 0 {
        Some(vk as u32 | mods << 8)
    } else if mask & 1 != 0 {
        Some(vk as u32)
    } else {
        None
    }
}

pub fn start(tx: Sender<HookEvent>) {
    let _ = TX.set(tx);
    SH.own_pid.store(unsafe { GetCurrentProcessId() }, Relaxed);
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let _ = std::thread::Builder::new().name("input-hooks".into()).spawn(move || unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        let mut msg: MSG = std::mem::zeroed();
        PeekMessageW(&mut msg, null_mut(), WM_USER, WM_USER, PM_NOREMOVE); // create the queue
        THREAD.store(GetCurrentThreadId(), Relaxed);
        install(WH_KEYBOARD_LL, &KB_HOOK);
        refresh_mouse();
        // Windows silently unhooks a callback that ever runs too long; re-arm periodically.
        SetTimer(null_mut(), 0, 30_000, None);
        let _ = ready_tx.send(());
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            match msg.message {
                WM_REFRESH => refresh_mouse(),
                WM_TIMER => {
                    install(WH_KEYBOARD_LL, &KB_HOOK);
                    if MOUSE_HOOK.load(Relaxed) != 0 {
                        install(WH_MOUSE_LL, &MOUSE_HOOK);
                    }
                    // Un-stick modifiers whose key-up we never saw (e.g. during a UAC prompt).
                    for vk in MODIFIER_VKS {
                        if (GetAsyncKeyState(vk as i32) as u16) & 0x8000 == 0 {
                            SH.pressed[vk].store(false, Relaxed);
                        }
                    }
                }
                _ => {
                    DispatchMessageW(&msg);
                }
            }
        }
    });
    let _ = ready_rx.recv_timeout(Duration::from_secs(2));
}

unsafe fn install(kind: WINDOWS_HOOK_ID, slot: &AtomicIsize) {
    let h = if kind == WH_KEYBOARD_LL {
        SetWindowsHookExW(kind, Some(kb_proc), GetModuleHandleW(null()), 0)
    } else {
        SetWindowsHookExW(kind, Some(mouse_proc), GetModuleHandleW(null()), 0)
    };
    if !h.is_null() {
        let old = slot.swap(h as isize, Relaxed);
        if old != 0 {
            UnhookWindowsHookEx(old as HHOOK);
        }
    }
}

unsafe fn uninstall(slot: &AtomicIsize) {
    let old = slot.swap(0, Relaxed);
    if old != 0 {
        UnhookWindowsHookEx(old as HHOOK);
    }
}

/// The mouse hook is only installed while it's needed (picking, calibrating,
/// or a mouse button is bound), so gameplay mouse input never passes through us.
fn refresh_mouse() {
    let need = SH.capture.load(Relaxed) != CAP_NONE
        || SH.watch_mouse.load(Relaxed)
        || (4..=6).any(|vk| SH.bound[vk].load(Relaxed) != 0);
    unsafe {
        if need && MOUSE_HOOK.load(Relaxed) == 0 {
            install(WH_MOUSE_LL, &MOUSE_HOOK);
        } else if !need {
            uninstall(&MOUSE_HOOK);
        }
    }
}

fn post_refresh() {
    let t = THREAD.load(Relaxed);
    if t != 0 {
        unsafe {
            PostThreadMessageW(t, WM_REFRESH, 0, 0);
        }
    }
}

/// Sets the hotkey combos plus the plain keys auto sprint and calibration watch.
pub fn configure(hotkeys: &[u32], forward: u32, back: u32, sprint: u32, capture_key: u32) {
    let mut want = [0u16; 256];
    for &code in hotkeys {
        let (vk, mods) = ((code & 0xFF) as usize, code >> 8);
        if vk > 0 && mods < 16 {
            want[vk] |= 1 << mods;
        }
    }
    for (i, w) in want.iter().enumerate() {
        SH.bound[i].store(*w, Relaxed);
    }
    SH.forward.store(forward, Relaxed);
    SH.back.store(back, Relaxed);
    SH.sprint.store(sprint, Relaxed);
    SH.capture_key.store(capture_key, Relaxed);
    post_refresh();
}

pub fn set_capture(mode: u8) {
    SH.pending_mod.store(0, Relaxed);
    SH.capture.store(mode, Relaxed);
    post_refresh();
}

pub fn set_target_cancellable(on: bool) {
    SH.target_cancellable.store(on, Relaxed);
}

pub fn set_target_pid(pid: u32) {
    SH.target_pid.store(pid, Relaxed);
}

pub fn target_pid() -> u32 {
    SH.target_pid.load(Relaxed)
}

/// Starts/stops watching for the user physically moving the mouse away from
/// (x, y). Only real mouse movement counts: our own clicks, and the game
/// repositioning its cursor, never reach this.
pub fn watch_mouse(on: bool, x: i32, y: i32) {
    SH.mouse_travel.store(0, Relaxed);
    SH.anchor_x.store(x, Relaxed);
    SH.anchor_y.store(y, Relaxed);
    SH.watch_mouse.store(on, Relaxed);
    post_refresh();
}

/// How many of the user's own mouse movements landed well away from the
/// anchor. A deliberate move produces a burst of these; sensor jitter doesn't.
pub fn mouse_moves_away() -> u32 {
    SH.mouse_travel.load(Relaxed)
}

pub fn set_hover(ok: bool, root: isize) {
    SH.hover_root.store(root, Relaxed);
    SH.hover_ok.store(ok, Relaxed);
}

unsafe extern "system" fn kb_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let k = &*(lp as *const KBDLLHOOKSTRUCT);
        if k.flags & LLKHF_INJECTED == 0 {
            let msg = wp as u32;
            let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            if on_key((k.vkCode & 0xFF) as usize, down) {
                return 1;
            }
        }
    }
    CallNextHookEx(null_mut(), code, wp, lp)
}

/// Returns true to swallow the key.
fn on_key(vk: usize, down: bool) -> bool {
    let was_down = SH.pressed[vk].swap(down, Relaxed);
    let fresh = down && !was_down;
    let vk32 = vk as u32;
    let cap = SH.capture.load(Relaxed);

    if !down {
        if vk32 == SH.forward.load(Relaxed) {
            emit(HookEvent::ForwardUp);
        }
        if vk32 == SH.sprint.load(Relaxed) {
            emit(HookEvent::SprintKeyUp);
        }
        // A modifier pressed and released on its own while binding becomes the hotkey.
        if cap == CAP_KEY && SH.pending_mod.load(Relaxed) == vk32 && end_capture(CAP_KEY) {
            SH.pending_mod.store(0, Relaxed);
            SH.swallow_up[vk].store(false, Relaxed);
            emit(HookEvent::CaptureKey(vk32 | current_mods(vk) << 8));
            return true;
        }
        return SH.swallow_up[vk].swap(false, Relaxed);
    }

    match cap {
        CAP_KEY | CAP_KEY_SINGLE => {
            if fresh {
                let mods = current_mods(vk);
                if vk == VK_ESCAPE && mods == 0 {
                    if end_capture(cap) {
                        emit(HookEvent::CaptureCancel);
                    }
                } else if vk == VK_BACK && mods == 0 && cap == CAP_KEY {
                    if end_capture(cap) {
                        emit(HookEvent::CaptureClear);
                    }
                } else if cap == CAP_KEY && mod_bit(vk) != 0 {
                    SH.pending_mod.store(vk32, Relaxed); // wait to see if a key joins it
                } else if end_capture(cap) {
                    SH.pending_mod.store(0, Relaxed);
                    emit(HookEvent::CaptureKey(if cap == CAP_KEY { vk32 | mods << 8 } else { vk32 }));
                }
            }
            SH.swallow_up[vk].store(true, Relaxed);
            return true;
        }
        CAP_POINT if vk32 == SH.capture_key.load(Relaxed) || vk == VK_ESCAPE => {
            if fresh && end_capture(CAP_POINT) {
                if vk == VK_ESCAPE {
                    emit(HookEvent::CaptureCancel);
                } else {
                    let mut p = POINT { x: 0, y: 0 };
                    unsafe {
                        GetCursorPos(&mut p);
                    }
                    emit(HookEvent::CapturePoint(p.x, p.y));
                }
            }
            SH.swallow_up[vk].store(true, Relaxed);
            return true;
        }
        CAP_TARGET if vk == VK_ESCAPE && SH.target_cancellable.load(Relaxed) => {
            if fresh && end_capture(CAP_TARGET) {
                emit(HookEvent::CaptureCancel);
            }
            SH.swallow_up[vk].store(true, Relaxed);
            return true;
        }
        _ => {}
    }

    if let Some(combo) = match_hotkey(vk) {
        if fresh {
            emit(HookEvent::Hotkey(combo));
        }
        SH.swallow_up[vk].store(true, Relaxed);
        return true;
    }
    if fresh && vk32 == SH.forward.load(Relaxed) {
        emit(HookEvent::ForwardDown);
    }
    if fresh && vk32 == SH.back.load(Relaxed) {
        emit(HookEvent::BackDown);
    }
    false
}

unsafe extern "system" fn mouse_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let msg = wp as u32;
        let m = &*(lp as *const MSLLHOOKSTRUCT);
        let physical = m.flags & LLMHF_ANY_INJECTED == 0;
        if msg == WM_MOUSEMOVE {
            if physical && SH.watch_mouse.load(Relaxed) {
                track_travel(m.pt.x, m.pt.y);
            }
        } else if physical && on_mouse(msg, m) {
            return 1;
        }
    }
    CallNextHookEx(null_mut(), code, wp, lp)
}

fn track_travel(x: i32, y: i32) {
    let d = (x - SH.anchor_x.load(Relaxed)).unsigned_abs() + (y - SH.anchor_y.load(Relaxed)).unsigned_abs();
    if d > 60 {
        SH.mouse_travel.fetch_add(1, Relaxed);
    }
}

fn on_mouse(msg: u32, m: &MSLLHOOKSTRUCT) -> bool {
    let (vk, down) = match msg {
        WM_LBUTTONDOWN => (1usize, true),
        WM_LBUTTONUP => (1, false),
        WM_RBUTTONDOWN => (2, true),
        WM_RBUTTONUP => (2, false),
        WM_MBUTTONDOWN => (4, true),
        WM_MBUTTONUP => (4, false),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let which = (m.mouseData >> 16) & 0xFFFF;
            (if which == 1 { 5 } else { 6 }, msg == WM_XBUTTONDOWN)
        }
        _ => return false,
    };
    if !down {
        return SH.swallow_up[vk].swap(false, Relaxed);
    }

    match SH.capture.load(Relaxed) {
        CAP_POINT if vk == 1 || vk == 2 => {
            if end_capture(CAP_POINT) {
                emit(if vk == 1 { HookEvent::CapturePoint(m.pt.x, m.pt.y) } else { HookEvent::CaptureCancel });
            }
            SH.swallow_up[vk].store(true, Relaxed);
            return true;
        }
        // Clicks on our own window or the taskbar pass through so you can get to the game.
        CAP_TARGET if vk == 1 && SH.hover_ok.load(Relaxed) => {
            if end_capture(CAP_TARGET) {
                emit(HookEvent::CaptureTarget(SH.hover_root.load(Relaxed), m.pt.x, m.pt.y));
            }
            SH.swallow_up[vk].store(true, Relaxed);
            return true;
        }
        CAP_KEY if vk >= 4 => {
            if end_capture(CAP_KEY) {
                emit(HookEvent::CaptureKey(vk as u32 | current_mods(0) << 8));
            }
            SH.swallow_up[vk].store(true, Relaxed);
            return true;
        }
        _ => {}
    }

    if vk >= 4 {
        if let Some(combo) = match_hotkey(vk) {
            emit(HookEvent::Hotkey(combo));
            SH.swallow_up[vk].store(true, Relaxed);
            return true;
        }
    }
    false
}

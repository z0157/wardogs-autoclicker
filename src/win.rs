//! Thin wrappers over the Win32 calls the app needs: left clicks, cursor,
//! screen sampling, window/process lookup and window chrome.

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, BOOL, HWND, LPARAM, POINT};
use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, SRCCOPY,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcessId, GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW,
    WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MapVirtualKeyW, SendInput, INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MAPVK_VK_TO_VSC_EX, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MOVE, MOUSEINPUT,
};
use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GetAncestor, GetClassNameW, GetClientRect, GetCursorInfo, GetCursorPos,
    GetForegroundWindow, GetWindow, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible,
    SetCursorPos, SetForegroundWindow, ShowWindow, WindowFromPoint, CURSORINFO, GA_ROOT, GW_OWNER, SW_RESTORE,
};

#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
    fn timeEndPeriod(period: u32) -> u32;
    fn PlaySoundW(sound: *const u16, module: *mut c_void, flags: u32) -> i32;
}

/// Marks every event we inject so it can be told apart from real input.
pub const INJECT_TAG: usize = 0x0AC4_2042;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------- input

fn send(i: &INPUT) {
    unsafe {
        SendInput(1, i, size_of::<INPUT>() as i32);
    }
}

fn mouse(flags: u32, dx: i32, dy: i32) {
    let mut i: INPUT = unsafe { zeroed() };
    i.r#type = INPUT_MOUSE;
    i.Anonymous.mi = MOUSEINPUT { dx, dy, mouseData: 0, dwFlags: flags, time: 0, dwExtraInfo: INJECT_TAG };
    send(&i);
}

/// Presses or releases a key by hardware scan code, which games reading
/// raw input pick up far more reliably than virtual-key events.
pub fn key(vk: u32, up: bool) {
    let sc = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC_EX) };
    let mut flags = if up { KEYEVENTF_KEYUP } else { 0 };
    let ki = if sc != 0 {
        flags |= KEYEVENTF_SCANCODE;
        if matches!(sc >> 8, 0xE0 | 0xE1) {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        KEYBDINPUT { wVk: 0, wScan: (sc & 0xFF) as u16, dwFlags: flags, time: 0, dwExtraInfo: INJECT_TAG }
    } else {
        KEYBDINPUT { wVk: vk as u16, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: INJECT_TAG }
    };
    let mut i: INPUT = unsafe { zeroed() };
    i.r#type = INPUT_KEYBOARD;
    i.Anonymous.ki = ki;
    send(&i);
}

pub fn key_is_down(vk: u32) -> bool {
    unsafe { (GetAsyncKeyState(vk as i32) as u16) & 0x8000 != 0 }
}

pub fn left_down() {
    mouse(MOUSEEVENTF_LEFTDOWN, 0, 0);
}

pub fn left_up() {
    mouse(MOUSEEVENTF_LEFTUP, 0, 0);
}

pub fn move_rel(dx: i32, dy: i32) {
    mouse(MOUSEEVENTF_MOVE, dx, dy);
}

pub fn cursor_pos() -> (i32, i32) {
    let mut p = POINT { x: 0, y: 0 };
    unsafe {
        GetCursorPos(&mut p);
    }
    (p.x, p.y)
}

pub fn set_cursor(x: i32, y: i32) {
    unsafe {
        SetCursorPos(x, y);
    }
}

/// Whether the mouse pointer is showing. Games hide it during play and show
/// it on menus like the team screen.
pub fn cursor_visible() -> bool {
    const CURSOR_SHOWING: u32 = 0x1;
    let mut ci: CURSORINFO = unsafe { zeroed() };
    ci.cbSize = size_of::<CURSORINFO>() as u32;
    unsafe { GetCursorInfo(&mut ci) != 0 && ci.flags & CURSOR_SHOWING != 0 }
}

/// A window's client area on screen: (left, top, width, height).
pub fn client_rect(h: isize) -> Option<(i32, i32, i32, i32)> {
    if !is_window(h) {
        return None;
    }
    unsafe {
        let mut r: RECT = zeroed();
        if GetClientRect(h as HWND, &mut r) == 0 || r.bottom <= 0 || r.right <= 0 {
            return None;
        }
        let mut origin = POINT { x: 0, y: 0 };
        ClientToScreen(h as HWND, &mut origin);
        Some((origin.x, origin.y, r.right, r.bottom))
    }
}

// ---------------------------------------------------------------- windows & processes
// Window handles travel between threads as isize.

pub fn own_pid() -> u32 {
    unsafe { GetCurrentProcessId() }
}

pub fn pid_of(h: isize) -> u32 {
    if h == 0 {
        return 0;
    }
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(h as HWND, &mut pid);
    }
    pid
}

/// Top-level window under a screen point (0 if none).
pub fn root_at(x: i32, y: i32) -> isize {
    unsafe {
        let h = WindowFromPoint(POINT { x, y });
        if h.is_null() {
            0
        } else {
            GetAncestor(h, GA_ROOT) as isize
        }
    }
}

pub fn pid_at(x: i32, y: i32) -> u32 {
    pid_of(root_at(x, y))
}

pub fn foreground_pid() -> u32 {
    pid_of(unsafe { GetForegroundWindow() } as isize)
}

pub fn is_window(h: isize) -> bool {
    h != 0 && unsafe { IsWindow(h as HWND) } != 0
}

pub fn window_title(h: isize) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(h as HWND, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize]).trim().to_string()
}

fn class_name(h: isize) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(h as HWND, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Taskbar, desktop, Start, Alt-Tab… things you click on the way to the game.
pub fn is_shell_window(h: isize) -> bool {
    matches!(
        class_name(h).as_str(),
        "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "Progman"
            | "WorkerW"
            | "NotifyIconOverflowWindow"
            | "TopLevelWindowForOverflowXamlIsland"
            | "Windows.UI.Core.CoreWindow"
            | "XamlExplorerHostIslandWindow"
            | "MultitaskingViewFrame"
            | "TaskListThumbnailWnd"
            | "ForegroundStaging"
    )
}

pub fn process_name(pid: u32) -> String {
    if pid == 0 {
        return String::new();
    }
    unsafe {
        let ph = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if ph.is_null() {
            return String::new();
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(ph, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(ph);
        if ok == 0 {
            return String::new();
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        full.rsplit('\\').next().unwrap_or_default().to_string()
    }
}

pub fn pid_alive(pid: u32) -> bool {
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        let ph = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if ph.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(ph, &mut code);
        CloseHandle(ph);
        ok != 0 && code == STILL_ACTIVE
    }
}

/// First visible top-level window (not ours) whose exe name and title pass `matches`.
pub fn find_window(matches: &dyn Fn(&str, &str) -> bool) -> Option<isize> {
    struct Search<'a> {
        matches: &'a dyn Fn(&str, &str) -> bool,
        own: u32,
        found: isize,
    }
    unsafe extern "system" fn each(h: HWND, lp: LPARAM) -> BOOL {
        let s = &mut *(lp as *mut Search);
        let hi = h as isize;
        if IsWindowVisible(h) == 0 || !GetWindow(h, GW_OWNER).is_null() {
            return 1;
        }
        let pid = pid_of(hi);
        if pid != s.own && (s.matches)(&process_name(pid), &window_title(hi)) {
            s.found = hi;
            return 0;
        }
        1
    }
    let mut s = Search { matches, own: own_pid(), found: 0 };
    unsafe {
        EnumWindows(Some(each), &mut s as *mut Search as LPARAM);
    }
    (s.found != 0).then_some(s.found)
}

/// Restores and activates a window. Works when we're the foreground app
/// (e.g. right after a click in our UI).
pub fn bring_to_front(h: isize) {
    if !is_window(h) {
        return;
    }
    unsafe {
        if IsIconic(h as HWND) != 0 {
            ShowWindow(h as HWND, SW_RESTORE);
        }
        SetForegroundWindow(h as HWND);
    }
}

// ---------------------------------------------------------------- screen sampling

/// Grabs the (2r+1)² pixels centred on (cx, cy) as RGB, row-major.
pub fn grab(cx: i32, cy: i32, r: i32) -> Vec<[u8; 3]> {
    grab_rect(cx - r, cy - r, 2 * r + 1, 2 * r + 1)
}

/// Grabs a w×h screen rectangle as RGB, row-major.
pub fn grab_rect(left: i32, top: i32, w: i32, h: i32) -> Vec<[u8; 3]> {
    if w <= 0 || h <= 0 {
        return Vec::new();
    }
    unsafe {
        let screen = GetDC(null_mut());
        if screen.is_null() {
            return Vec::new();
        }
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp);
        BitBlt(mem, 0, 0, w, h, screen, left, top, SRCCOPY);
        SelectObject(mem, old);

        let mut bi: BITMAPINFO = zeroed();
        bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -h; // top-down rows
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = 0; // BI_RGB
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(mem, bmp, 0, h as u32, buf.as_mut_ptr() as *mut c_void, &mut bi, DIB_RGB_COLORS);

        DeleteObject(bmp);
        DeleteDC(mem);
        ReleaseDC(null_mut(), screen);
        if lines == 0 {
            return Vec::new();
        }
        buf.chunks_exact(4).map(|p| [p[2], p[1], p[0]]).collect()
    }
}

// ---------------------------------------------------------------- timing

pub fn time_begin() {
    unsafe {
        timeBeginPeriod(1);
    }
}

pub fn time_end() {
    unsafe {
        timeEndPeriod(1);
    }
}

// ---------------------------------------------------------------- app-level helpers

/// Ensures only one copy runs. A second launch just brings the first one forward.
pub fn single_instance(title: &str) -> bool {
    const ERROR_ALREADY_EXISTS: u32 = 183;
    unsafe {
        let name = wide("Local\\AutoClicker4dot20.SingleInstance");
        let h = CreateMutexW(null(), 1, name.as_ptr());
        let existed = GetLastError() == ERROR_ALREADY_EXISTS || h.is_null();
        if !existed {
            return true; // keep the handle for the life of the process
        }
        let t = wide(title);
        let other = FindWindowW(null(), t.as_ptr());
        if !other.is_null() {
            if IsIconic(other) != 0 {
                ShowWindow(other, SW_RESTORE);
            }
            SetForegroundWindow(other);
        } else if !h.is_null() {
            // An older build may still be closing; wait briefly and take over.
            WaitForSingleObject(h, 3_000);
            return true;
        }
        false
    }
}

/// Shown instead of a silent crash when the WebView2 runtime (which draws the
/// window) is missing, e.g. on an older Windows 10 install.
pub fn missing_webview2(title: &str) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, IDYES, MB_ICONWARNING, MB_YESNO, SW_SHOWNORMAL};
    let text = wide(
        "This app needs Microsoft Edge WebView2, a free Microsoft component that \
         Windows 11 includes but some Windows 10 PCs are missing.\n\n\
         Open Microsoft's download page now? Install the \"Evergreen Bootstrapper\", \
         then start the app again.",
    );
    let caption = wide(title);
    unsafe {
        if MessageBoxW(null_mut(), text.as_ptr(), caption.as_ptr(), MB_YESNO | MB_ICONWARNING) == IDYES {
            let url = wide("https://developer.microsoft.com/microsoft-edge/webview2/");
            let open = wide("open");
            ShellExecuteW(null_mut(), open.as_ptr(), url.as_ptr(), null(), null(), SW_SHOWNORMAL);
        }
    }
}

pub fn round_corners(hwnd: isize) {
    let pref: u32 = 2; // DWMWCP_ROUND
    unsafe {
        DwmSetWindowAttribute(
            hwnd as HWND,
            DWMWA_WINDOW_CORNER_PREFERENCE as _,
            &pref as *const u32 as *const c_void,
            size_of::<u32>() as u32,
        );
    }
}

// ---------------------------------------------------------------- sound cues

fn tone(notes: &[(f32, f32)]) -> Vec<u8> {
    let rate = 44_100u32;
    let mut pcm: Vec<i16> = Vec::new();
    for &(freq, secs) in notes {
        let n = (rate as f32 * secs) as usize;
        for i in 0..n {
            let t = i as f32 / rate as f32;
            let attack = (i as f32 / (rate as f32 * 0.004)).min(1.0);
            let decay = (1.0 - i as f32 / n as f32).powf(2.2);
            let wave = (std::f32::consts::TAU * freq * t).sin() * 0.8 + (std::f32::consts::TAU * freq * 2.0 * t).sin() * 0.12;
            pcm.push((wave * attack * decay * 0.22 * i16::MAX as f32) as i16);
        }
    }
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Soft two-note chime: rising for "on", falling for "off".
pub fn chime(on: bool) {
    static ON: OnceLock<Vec<u8>> = OnceLock::new();
    static OFF: OnceLock<Vec<u8>> = OnceLock::new();
    let wav = if on {
        ON.get_or_init(|| tone(&[(880.0, 0.06), (1318.5, 0.09)]))
    } else {
        OFF.get_or_init(|| tone(&[(1318.5, 0.06), (880.0, 0.09)]))
    };
    const SND_ASYNC: u32 = 0x1;
    const SND_NODEFAULT: u32 = 0x2;
    const SND_MEMORY: u32 = 0x4;
    unsafe {
        PlaySoundW(wav.as_ptr() as *const u16, null_mut(), SND_ASYNC | SND_NODEFAULT | SND_MEMORY);
    }
}

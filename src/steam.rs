//! Your Steam display name, read from Steam's own local login file, so the
//! party shows the name your friends know you by.

use crate::win::wide;
use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::null_mut;
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};

fn steam_dir() -> Option<PathBuf> {
    let sub = wide(r"Software\Valve\Steam");
    let value = wide("SteamPath");
    let mut buf = [0u16; 520];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, sub.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, null_mut(), buf.as_mut_ptr() as *mut c_void, &mut size)
    };
    if rc != 0 {
        return None;
    }
    let len = (size as usize / 2).saturating_sub(1);
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

/// PersonaName of the most recent Steam login, if Steam is installed.
pub fn persona_name() -> Option<String> {
    let dir = steam_dir().unwrap_or_else(|| PathBuf::from(r"C:\Program Files (x86)\Steam"));
    let text = std::fs::read_to_string(dir.join("config").join("loginusers.vdf")).ok()?;
    let mut first: Option<String> = None;
    let mut name: Option<String> = None;
    let mut recent = false;
    for line in text.lines() {
        // Lines look like:  "PersonaName"		"YourName"
        let parts: Vec<&str> = line.split('"').collect();
        if parts.len() >= 4 {
            let (key, value) = (parts[1], parts[3]);
            if key.eq_ignore_ascii_case("PersonaName") && !value.trim().is_empty() {
                name = Some(value.trim().to_string());
                first.get_or_insert_with(|| value.trim().to_string());
            } else if key.eq_ignore_ascii_case("MostRecent") && value == "1" {
                recent = true;
            }
        }
        if line.trim() == "}" {
            if recent && name.is_some() {
                return name;
            }
            name = None;
            recent = false;
        }
    }
    first
}

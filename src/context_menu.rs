//! Explorer's "Search here with evbar" entry, registered per user under
//! `HKCU\Software\Classes` (no admin rights needed). It appears when
//! right-clicking a folder, a drive, or the empty space inside a folder; on
//! Windows 11 it is under "Show more options" (Shift+F10). Each runs
//! `evbar.exe --scope "<folder>"`, which hands the folder to the running evbar.

use std::ptr::null;

use evbar::wide;
use windows_sys::Win32::System::Registry::{RegDeleteTreeW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
use windows_sys::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK};

const TARGETS: [&str; 3] = ["Directory", "Directory\\Background", "Drive"];
const VERB: &str = "evbar";
const LABEL: &str = "Search here with evbar";

fn key(target: &str) -> String {
    format!("Software\\Classes\\{target}\\shell\\{VERB}")
}

unsafe fn set(subkey: &str, name: Option<&str>, value: &str) -> bool {
    let data = wide(value);
    let name = name.map(wide);
    RegSetKeyValueW(
        HKEY_CURRENT_USER,
        wide(subkey).as_ptr(),
        name.as_ref().map_or(null(), |n| n.as_ptr()),
        REG_SZ,
        data.as_ptr().cast(),
        (data.len() * 2) as u32,
    ) == 0
}

fn message(text: &str, ok: bool) {
    let icon = if ok { MB_ICONINFORMATION } else { MB_ICONWARNING };
    unsafe { MessageBoxW(std::ptr::null_mut(), wide(text).as_ptr(), wide("evbar").as_ptr(), MB_OK | icon) };
}

pub fn install() {
    let exe = match std::env::current_exe() {
        Ok(p) => p.display().to_string(),
        Err(e) => return message(&format!("Could not find evbar.exe: {e}"), false),
    };
    let command = format!("\"{exe}\" --scope \"%V\"");
    // Borrow Everything's icon when it is installed; evbar has none of its own.
    let icon = ["C:\\Program Files\\Everything\\Everything.exe", "C:\\Program Files (x86)\\Everything\\Everything.exe"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists());
    let mut ok = true;
    unsafe {
        for target in TARGETS {
            let k = key(target);
            ok &= set(&k, None, LABEL);
            if let Some(icon) = icon {
                ok &= set(&k, Some("Icon"), &format!("{icon},0"));
            }
            ok &= set(&format!("{k}\\command"), None, &command);
        }
        SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, null(), null());
    }
    if ok {
        message(
            &format!(
                "Added \"{LABEL}\" to Explorer's right-click menu for folders, drives, and folder backgrounds.\n\n\
                 On Windows 11 it is under \"Show more options\".\n\nIt runs: {command}"
            ),
            true,
        );
    } else {
        message("Could not write all registry entries for the context menu.", false);
    }
}

pub fn uninstall() {
    unsafe {
        for target in TARGETS {
            RegDeleteTreeW(HKEY_CURRENT_USER, wide(&key(target)).as_ptr());
        }
        SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, null(), null());
    }
    message(&format!("Removed \"{LABEL}\" from Explorer's right-click menu."), true);
}

/// Repairs a folder argument from Explorer. `"%V"` for a drive arrives as
/// `C:"` because Windows argument parsing treats `\"` as an escaped quote.
pub fn clean_scope_arg(raw: &str) -> String {
    let mut s = raw.trim().trim_matches('"').to_string();
    if s.ends_with(':') {
        s.push('\\');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::clean_scope_arg;

    #[test]
    fn repairs_drive_roots() {
        assert_eq!(clean_scope_arg("C:\""), "C:\\");
        assert_eq!(clean_scope_arg("C:\\"), "C:\\");
        assert_eq!(clean_scope_arg("C:\\Users\\nikhi\\Downloads"), "C:\\Users\\nikhi\\Downloads");
    }
}

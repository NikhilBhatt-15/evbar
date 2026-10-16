//! Asks Explorer which folder one of its windows is showing.
//!
//! Every Explorer window (every tab, on Windows 11) is listed in the shell's
//! `ShellWindows` collection. For each entry we get its `IShellBrowser`, keep
//! the one whose window is the active tab of the window we were given, and
//! read the folder from its view: `IShellView` → `IFolderView` →
//! `IPersistFolder2::GetCurFolder` → `SHGetPathFromIDListW`.
//!
//! Virtual locations (Home, This PC, Recycle Bin) have no file-system path
//! and yield `None`.

use windows::core::{Interface, VARIANT, w};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, IServiceProvider, CLSCTX_ALL};
use windows::Win32::UI::Shell::{
    IFolderView, IPersistFolder2, IShellBrowser, IShellWindows, SHGetNameFromIDList, SIGDN_FILESYSPATH, SID_STopLevelBrowser,
    ShellWindows,
};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, GetAncestor, GetClassNameW, GA_ROOT};

/// True for File Explorer's top-level window class.
pub fn is_explorer_window(hwnd: isize) -> bool {
    let mut class = [0u16; 32];
    let n = unsafe { GetClassNameW(HWND(hwnd as _), &mut class) };
    String::from_utf16_lossy(&class[..n.max(0) as usize]) == "CabinetWClass"
}

/// The file-system folder shown in the active tab of an Explorer window.
/// COM must already be initialized on the calling thread.
pub fn folder_of(explorer: isize) -> Option<String> {
    if !is_explorer_window(explorer) {
        return None;
    }
    let top = HWND(explorer as _);
    unsafe {
        // The selected tab is the first ShellTabWindowClass child.
        let active_tab = FindWindowExW(top, None, w!("ShellTabWindowClass"), None).ok();
        let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).ok()?;
        let mut fallback = None;
        for i in 0..windows.Count().ok()? {
            let Ok(item) = windows.Item(&VARIANT::from(i)) else { continue };
            let Ok(sp) = item.cast::<IServiceProvider>() else { continue };
            let Ok(browser) = sp.QueryService::<IShellBrowser>(&SID_STopLevelBrowser) else { continue };
            let Ok(hwnd) = browser.GetWindow() else { continue };
            if Some(hwnd) == active_tab {
                return path_of(&browser);
            }
            if fallback.is_none() && (hwnd == top || GetAncestor(hwnd, GA_ROOT) == top) {
                fallback = Some(browser);
            }
        }
        fallback.and_then(|b| path_of(&b))
    }
}

unsafe fn path_of(browser: &IShellBrowser) -> Option<String> {
    let view = browser.QueryActiveShellView().ok()?;
    let folder_view: IFolderView = view.cast().ok()?;
    let folder: IPersistFolder2 = folder_view.GetFolder().ok()?;
    let pidl = folder.GetCurFolder().ok()?;
    // SIGDN_FILESYSPATH fails for virtual folders, which is what we want.
    let name = SHGetNameFromIDList(pidl, SIGDN_FILESYSPATH);
    CoTaskMemFree(Some(pidl as *const _));
    let name = name.ok()?;
    let path = name.to_string().ok();
    CoTaskMemFree(Some(name.0 as *const _));
    path.filter(|p| !p.is_empty())
}

//! evbar: a compact, rounded search box that floats just above the Windows
//! taskbar tray and searches a running Voidtools Everything instance as you
//! type, over Everything's WM_COPYDATA IPC.
//!
//! Threads:
//! - UI thread: owns both windows and receives Everything's replies
//!   (WM_COPYDATA) directly on the search-box window.
//! - IPC worker: sends queries. It always sends the newest pending query and
//!   drops older ones, so typing never queues up work or blocks the UI.
//!
//! Replies are matched against the id of the newest query; anything else is
//! a stale answer and is ignored.

#![windows_subsystem = "windows"]

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::{Arc, Condvar, Mutex};

use evbar::ipc::{self, request, Item, Query};
use evbar::{highlight, hotkey, wide};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows_sys::Win32::System::Threading::{AttachThreadInput, CreateMutexW, GetCurrentThreadId};
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows_sys::Win32::UI::Controls::{ImageList_Draw, ILD_TRANSPARENT};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::{
    DefSubclassProc, SHAppBarMessage, SHGetFileInfoW, SetWindowSubclass, ShellExecuteW, ABE_BOTTOM, ABE_LEFT,
    ABE_RIGHT, ABE_TOP, ABM_GETTASKBARPOS, APPBARDATA, SHFILEINFOW, SHGFI_SMALLICON, SHGFI_SYSICONINDEX,
    SHGFI_USEFILEATTRIBUTES,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const BOX_CLASS: &str = "EvbarSearchBox";
const LIST_CLASS: &str = "EvbarResults";
const MUTEX_NAME: &str = "Local\\EvbarSingleInstance";

const WM_APP_NOT_RUNNING: u32 = WM_APP + 1;
const HOTKEY_ID: i32 = 1;
const ID_EDIT: usize = 100;
const ID_MENU_EXIT: usize = 1;

const MAX_RESULTS: u32 = 100;
const VISIBLE_ROWS: i32 = 9;
const REQUEST_FLAGS: u32 = request::FILE_NAME | request::PATH | request::HIGHLIGHTED_FILE_NAME;

// Layout in 96-DPI pixels.
const BOX_W: i32 = 300;
const BOX_H: i32 = 40;
const LIST_W: i32 = 560;
const ROW_H: i32 = 46;
const PAD: i32 = 6;
const FOOTER_H: i32 = 28;
const GAP: i32 = 8;

const EM_SETSEL: u32 = 0x00B1;
const EM_GETSEL: u32 = 0x00B0;
const EM_REPLACESEL: u32 = 0x00C2;
const EM_SETMARGINS: u32 = 0x00D3;
const EM_SETCUEBANNER: u32 = 0x1501;
const EN_SETFOCUS: u32 = 0x0100;
const EN_CHANGE: u32 = 0x0300;
const DWMWCP_ROUND: i32 = 2;
const MA_NOACTIVATE_: isize = 3;
const WM_MOUSELEAVE: u32 = 0x02A3;
const RENDER_TIMEOUT_TIMER: usize = 1;

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

#[derive(Clone, Copy)]
struct Theme {
    bg: COLORREF,
    border: COLORREF,
    text: COLORREF,
    dim: COLORREF,
    sel: COLORREF,
    accent: COLORREF,
}

impl Theme {
    fn current(force_light: Option<bool>) -> Theme {
        if force_light.unwrap_or_else(system_uses_light_theme) {
            Theme {
                bg: rgb(251, 251, 251),
                border: rgb(214, 214, 214),
                text: rgb(26, 26, 26),
                dim: rgb(96, 96, 96),
                sel: rgb(234, 234, 234),
                accent: rgb(0, 95, 184),
            }
        } else {
            Theme {
                bg: rgb(43, 43, 43),
                border: rgb(70, 70, 70),
                text: rgb(242, 242, 242),
                dim: rgb(166, 166, 166),
                sel: rgb(61, 61, 61),
                accent: rgb(96, 205, 255),
            }
        }
    }
}

fn system_uses_light_theme() -> bool {
    let key = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let value = wide("SystemUsesLightTheme");
    let mut data: u32 = 0;
    let mut len = size_of::<u32>() as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            &mut data as *mut u32 as *mut c_void,
            &mut len,
        )
    };
    rc == 0 && data != 0
}

/// The single-slot mailbox between the UI thread and the IPC worker.
struct Worker {
    slot: Mutex<Option<(u32, String)>>,
    wake: Condvar,
}

#[derive(PartialEq, Clone, Copy)]
enum Status {
    Idle,
    Ready,
    NotRunning,
}

struct Fonts {
    edit: HFONT,
    name: HFONT,
    name_bold: HFONT,
    small: HFONT,
    glyph: HFONT,
}

struct App {
    box_hwnd: HWND,
    edit: HWND,
    list: HWND,
    dpi: u32,
    theme: Theme,
    bg_brush: HBRUSH,
    fonts: Option<Fonts>,
    items: Vec<Item>,
    total: u32,
    status: Status,
    sel: i32,
    scroll: i32,
    hover: i32,
    tracking_mouse: bool,
    latest_id: u32,
    worker: Arc<Worker>,
    image_list: isize,
    icon_cache: HashMap<String, i32>,
    taskbar_created: u32,
    hidden_for_fullscreen: bool,
    force_light: Option<bool>,
    render: Option<RenderJob>,
    /// The hotkey as typed, e.g. "Win+Alt+Space", or None if none is active.
    hotkey: Option<String>,
}

/// `--render <query> <out.bmp>`: run one real query with the windows hidden,
/// paint the box and dropdown offscreen, write a bitmap, exit.
struct RenderJob {
    query: String,
    out: String,
}

struct Options {
    instance: Option<String>,
    force_light: Option<bool>,
    render: Option<RenderJob>,
    hotkey: String,
}

thread_local! {
    static APP: Cell<*mut App> = const { Cell::new(null_mut()) };
}

/// The UI state, owned by the UI thread for the life of the process.
fn app() -> &'static mut App {
    unsafe { &mut *APP.with(|a| a.get()) }
}

fn scale(v: i32) -> i32 {
    v * app().dpi as i32 / 96
}

fn main() {
    let opts = parse_args();
    unsafe {
        let mutex_name = wide(MUTEX_NAME);
        CreateMutexW(null(), 0, mutex_name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS && opts.render.is_none() {
            // Second launch: hand focus to the running instance.
            let class = wide(BOX_CLASS);
            let existing = FindWindowW(class.as_ptr(), null());
            if !existing.is_null() {
                AllowSetForegroundWindow(ASFW_ANY);
                PostMessageW(existing, WM_HOTKEY, HOTKEY_ID as usize, 0);
            }
            return;
        }
        CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32);
        run(opts);
    }
}

fn parse_args() -> Options {
    let args: Vec<String> = std::env::args().collect();
    let value = |flag: &str, n: usize| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + n).cloned())
    };
    Options {
        instance: value("--instance", 1),
        force_light: value("--theme", 1).map(|t| t == "light"),
        hotkey: value("--hotkey", 1).unwrap_or_else(|| hotkey::DEFAULT.to_string()),
        render: value("--render", 1)
            .zip(value("--render", 2))
            .map(|(query, out)| RenderJob { query, out }),
    }
}

unsafe fn run(opts: Options) {
    let instance = opts.instance;
    let worker = Arc::new(Worker {
        slot: Mutex::new(None),
        wake: Condvar::new(),
    });
    let state = Box::new(App {
        box_hwnd: null_mut(),
        edit: null_mut(),
        list: null_mut(),
        dpi: 96,
        theme: Theme::current(opts.force_light),
        bg_brush: null_mut(),
        fonts: None,
        items: Vec::new(),
        total: 0,
        status: Status::Idle,
        sel: 0,
        scroll: 0,
        hover: -1,
        tracking_mouse: false,
        latest_id: 0,
        worker: worker.clone(),
        image_list: 0,
        icon_cache: HashMap::new(),
        taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
        hidden_for_fullscreen: false,
        force_light: opts.force_light,
        render: opts.render,
        hotkey: None,
    });
    APP.with(|a| a.set(Box::into_raw(state)));
    let a = app();
    a.bg_brush = CreateSolidBrush(a.theme.bg);

    let hinst = GetModuleHandleW(null());
    let box_class = wide(BOX_CLASS);
    let list_class = wide(LIST_CLASS);
    let cursor = LoadCursorW(null_mut(), IDC_ARROW);
    for (class, proc_) in [
        (&box_class, box_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT),
        (&list_class, list_proc),
    ] {
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc_),
            hInstance: hinst,
            hCursor: cursor,
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        RegisterClassExW(&wc);
    }

    a.box_hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
        box_class.as_ptr(),
        wide("evbar").as_ptr(),
        WS_POPUP | WS_CLIPCHILDREN,
        0,
        0,
        scale(BOX_W),
        scale(BOX_H),
        null_mut(),
        null_mut(),
        hinst,
        null(),
    );
    a.edit = CreateWindowExW(
        0,
        wide("EDIT").as_ptr(),
        null(),
        WS_CHILD | WS_VISIBLE | ES_AUTOHSCROLL as u32,
        0,
        0,
        10,
        10,
        a.box_hwnd,
        ID_EDIT as HMENU,
        hinst,
        null(),
    );
    SetWindowSubclass(a.edit, Some(edit_proc), 1, 0);
    SendMessageW(a.edit, EM_SETCUEBANNER, 1, wide("Search Everything").as_ptr() as isize);
    a.list = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
        list_class.as_ptr(),
        null(),
        WS_POPUP,
        0,
        0,
        scale(LIST_W),
        scale(200),
        a.box_hwnd,
        null_mut(),
        hinst,
        null(),
    );

    // If evbar runs elevated and Everything does not, let its replies in.
    ChangeWindowMessageFilterEx(a.box_hwnd, ipc::WM_COPYDATA, MSGFLT_ALLOW, null_mut());
    for hwnd in [a.box_hwnd, a.list] {
        style_frame(hwnd);
    }
    place_windows();

    let reply_hwnd = a.box_hwnd as usize;
    std::thread::spawn(move || ipc_worker(worker, reply_hwnd, instance));

    if let Some(job) = &a.render {
        // Windows stay hidden; the query goes through the real IPC path.
        SetTimer(a.box_hwnd, RENDER_TIMEOUT_TIMER, 4000, None);
        SetWindowTextW(a.edit, wide(&job.query).as_ptr());
    } else {
        register_hotkey(&opts.hotkey);
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            null_mut(),
            Some(on_foreground_changed),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        ShowWindow(a.box_hwnd, SW_SHOWNOACTIVATE);
        // The hook only reports changes; check what is in front right now.
        on_foreground_changed(null_mut(), EVENT_SYSTEM_FOREGROUND, GetForegroundWindow(), 0, 0, 0, 0);
    }

    let mut msg: MSG = zeroed();
    while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

/// Registers the global hotkey. Another app may already own the combination
/// (RegisterHotKey is first come, first served), so say so instead of
/// failing silently.
unsafe fn register_hotkey(spec: &str) {
    let a = app();
    let problem = match hotkey::parse(spec) {
        Err(e) => Some(format!("The hotkey {e}.")),
        Ok(hk) => {
            if RegisterHotKey(a.box_hwnd, HOTKEY_ID, hk.modifiers | MOD_NOREPEAT, hk.vk) != 0 {
                a.hotkey = Some(spec.to_string());
                None
            } else {
                Some(format!("{spec} is already used by another app, so evbar can't use it."))
            }
        }
    };
    if let Some(problem) = problem {
        let text = format!(
            "{problem}\n\nClick the search box to use evbar, or restart it with a different hotkey, e.g.\n\n    evbar.exe --hotkey Ctrl+Shift+Space"
        );
        MessageBoxW(a.box_hwnd, wide(&text).as_ptr(), wide("evbar").as_ptr(), MB_OK | MB_ICONWARNING);
    }
}

/// Sends the newest pending query; older pending text is simply overwritten
/// in the slot and never sent.
fn ipc_worker(worker: Arc<Worker>, reply_hwnd: usize, instance: Option<String>) {
    loop {
        let (id, text) = {
            let mut slot = worker.slot.lock().unwrap();
            while slot.is_none() {
                slot = worker.wake.wait(slot).unwrap();
            }
            slot.take().unwrap()
        };
        let mut q = Query::new(text);
        q.max_results = MAX_RESULTS;
        q.request_flags = REQUEST_FLAGS;
        unsafe {
            let reply = reply_hwnd as HWND;
            let target = ipc::find_any(instance.as_deref());
            if target.is_null() || !ipc::send_query(target, reply, id, &q) {
                PostMessageW(reply, WM_APP_NOT_RUNNING, id as usize, 0);
            }
        }
    }
}

fn submit_query(text: String) {
    let a = app();
    a.latest_id = a.latest_id.wrapping_add(1).max(1);
    *a.worker.slot.lock().unwrap() = Some((a.latest_id, text));
    a.worker.wake.notify_one();
}

unsafe fn style_frame(hwnd: HWND) {
    let t = app().theme;
    let round = DWMWCP_ROUND;
    let hr = DwmSetWindowAttribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE as u32,
        &round as *const i32 as *const c_void,
        4,
    );
    if hr < 0 {
        // Pre-Windows 11: no DWM rounding, so clip to a rounded region.
        let mut rc: RECT = zeroed();
        GetWindowRect(hwnd, &mut rc);
        let r = scale(16);
        SetWindowRgn(hwnd, CreateRoundRectRgn(0, 0, rc.right - rc.left + 1, rc.bottom - rc.top + 1, r, r), 1);
    }
    DwmSetWindowAttribute(
        hwnd,
        DWMWA_BORDER_COLOR as u32,
        &t.border as *const u32 as *const c_void,
        4,
    );
}

unsafe fn make_font(px: i32, weight: i32, face: &str) -> HFONT {
    CreateFontW(
        -scale(px),
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET as u32,
        OUT_DEFAULT_PRECIS as u32,
        CLIP_DEFAULT_PRECIS as u32,
        CLEARTYPE_QUALITY as u32,
        0,
        wide(face).as_ptr(),
    )
}

unsafe fn rebuild_fonts() {
    let a = app();
    if let Some(f) = a.fonts.take() {
        for h in [f.edit, f.name, f.name_bold, f.small, f.glyph] {
            DeleteObject(h);
        }
    }
    a.fonts = Some(Fonts {
        edit: make_font(15, 400, "Segoe UI"),
        name: make_font(14, 400, "Segoe UI"),
        name_bold: make_font(14, 700, "Segoe UI"),
        small: make_font(12, 400, "Segoe UI"),
        glyph: make_font(15, 400, "Segoe MDL2 Assets"),
    });
    SendMessageW(a.edit, WM_SETFONT, a.fonts.as_ref().unwrap().edit as usize, 1);
}

/// Floats the box just above the tray (or beside a vertical taskbar) and
/// sizes the edit control inside it.
unsafe fn place_windows() {
    let a = app();
    let mut abd: APPBARDATA = zeroed();
    abd.cbSize = size_of::<APPBARDATA>() as u32;
    let have_taskbar = SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd) != 0;
    let tb = abd.rc;

    let monitor = MonitorFromRect(&tb, MONITOR_DEFAULTTOPRIMARY);
    let mut mi: MONITORINFO = zeroed();
    mi.cbSize = size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(monitor, &mut mi);
    let (work, mon) = (mi.rcWork, mi.rcMonitor);

    let (mut dx, mut dy) = (96u32, 96u32);
    if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) < 0 {
        dx = 96;
    }
    if dx != a.dpi || a.fonts.is_none() {
        a.dpi = dx;
        rebuild_fonts();
    }

    let (w, h, gap) = (scale(BOX_W), scale(BOX_H), scale(GAP));
    // Right edge of the notification area, if Explorer exposes it.
    let tray_right = {
        let tray = FindWindowExW(
            FindWindowW(wide("Shell_TrayWnd").as_ptr(), null()),
            null_mut(),
            wide("TrayNotifyWnd").as_ptr(),
            null(),
        );
        let mut rc: RECT = zeroed();
        if !tray.is_null() && GetWindowRect(tray, &mut rc) != 0 && rc.right > rc.left {
            rc.right.min(mon.right)
        } else {
            work.right
        }
    };
    let (x, y) = if !have_taskbar {
        (work.right - w - gap, work.bottom - h - gap)
    } else {
        match abd.uEdge {
            ABE_TOP => (tray_right - w - gap, tb.bottom.max(work.top) + gap),
            ABE_LEFT => (tb.right.max(work.left) + gap, work.bottom - h - gap),
            ABE_RIGHT => (tb.left.min(work.right) - w - gap, work.bottom - h - gap),
            ABE_BOTTOM | _ => (tray_right - w - gap, tb.top.min(work.bottom) - h - gap),
        }
    };
    SetWindowPos(a.box_hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE);

    let mut tm: TEXTMETRICW = zeroed();
    let dc = GetDC(a.edit);
    let old = SelectObject(dc, a.fonts.as_ref().unwrap().edit);
    GetTextMetricsW(dc, &mut tm);
    SelectObject(dc, old);
    ReleaseDC(a.edit, dc);
    let eh = tm.tmHeight + scale(2);
    let left = scale(38);
    SetWindowPos(a.edit, null_mut(), left, (h - eh) / 2, w - left - scale(12), eh, SWP_NOZORDER | SWP_NOACTIVATE);
    SendMessageW(a.edit, EM_SETMARGINS, 3, 0);
    if IsWindowVisible(a.list) != 0 {
        layout_list();
    }
}

fn list_rows() -> i32 {
    let a = app();
    (a.items.len() as i32).clamp(1, VISIBLE_ROWS)
}

/// Sizes the dropdown for the current results and opens it away from the
/// taskbar: upward when the box is in the lower half of its monitor.
unsafe fn layout_list() {
    let a = app();
    let mut br: RECT = zeroed();
    GetWindowRect(a.box_hwnd, &mut br);
    let monitor = MonitorFromWindow(a.box_hwnd, MONITOR_DEFAULTTONEAREST);
    let mut mi: MONITORINFO = zeroed();
    mi.cbSize = size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(monitor, &mut mi);
    let w = scale(LIST_W).max(br.right - br.left);
    let h = scale(PAD) * 2 + list_rows() * scale(ROW_H) + scale(FOOTER_H);
    let x = (br.right - w).max(mi.rcWork.left + scale(GAP));
    let upward = br.top > (mi.rcMonitor.top + mi.rcMonitor.bottom) / 2;
    let y = if upward { br.top - scale(GAP) - h } else { br.bottom + scale(GAP) };
    let show = if a.render.is_some() { 0 } else { SWP_SHOWWINDOW };
    SetWindowPos(a.list, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | show);
    InvalidateRect(a.list, null(), 0);
}

unsafe fn show_list(show: bool) {
    let a = app();
    if show {
        layout_list();
    } else {
        ShowWindow(a.list, SW_HIDE);
    }
}

fn edit_text() -> String {
    let a = app();
    unsafe {
        let len = GetWindowTextLengthW(a.edit);
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(a.edit, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

unsafe fn on_text_changed() {
    let a = app();
    let text = edit_text();
    if text.trim().is_empty() {
        a.items.clear();
        a.total = 0;
        a.status = Status::Idle;
        a.latest_id = a.latest_id.wrapping_add(1).max(1); // drop in-flight replies
        show_list(false);
        return;
    }
    // Keep the previous results on screen until the new reply lands: no flicker.
    submit_query(text);
}

unsafe fn on_reply(payload: &[u8]) {
    let a = app();
    let Some(list) = ipc::decode_list2(payload) else {
        return;
    };
    a.total = list.total;
    a.items = list.items;
    a.status = Status::Ready;
    a.sel = 0;
    a.scroll = 0;
    a.hover = -1;
    if a.render.is_some() {
        render_and_quit();
    } else if GetForegroundWindow() == a.box_hwnd {
        show_list(true);
    }
}

unsafe fn move_selection(delta: i32) {
    let a = app();
    if a.items.is_empty() {
        return;
    }
    let n = a.items.len() as i32;
    a.sel = (a.sel + delta).clamp(0, n - 1);
    if a.sel < a.scroll {
        a.scroll = a.sel;
    } else if a.sel >= a.scroll + VISIBLE_ROWS {
        a.scroll = a.sel - VISIBLE_ROWS + 1;
    }
    if IsWindowVisible(a.list) == 0 {
        show_list(true);
    }
    InvalidateRect(a.list, null(), 0);
}

unsafe fn open_item(index: i32, reveal: bool) {
    let a = app();
    let Some(item) = a.items.get(index as usize) else {
        return;
    };
    let full = item.full_path();
    if reveal {
        let args = wide(&format!("/select,\"{full}\""));
        ShellExecuteW(null_mut(), wide("open").as_ptr(), wide("explorer.exe").as_ptr(), args.as_ptr(), null(), SW_SHOWNORMAL);
    } else {
        let dir = wide(&item.path);
        ShellExecuteW(null_mut(), null(), wide(&full).as_ptr(), null(), dir.as_ptr(), SW_SHOWNORMAL);
    }
    show_list(false);
}

/// Like EverythingToolbar's ForciblySetForegroundWindow: try the polite way,
/// then borrow the foreground thread's input state.
unsafe fn force_foreground(hwnd: HWND) {
    if SetForegroundWindow(hwnd) != 0 {
        return;
    }
    let fg_thread = GetWindowThreadProcessId(GetForegroundWindow(), null_mut());
    let me = GetCurrentThreadId();
    if fg_thread != 0 && fg_thread != me {
        AttachThreadInput(me, fg_thread, 1);
        SetForegroundWindow(hwnd);
        SetActiveWindow(hwnd);
        AttachThreadInput(me, fg_thread, 0);
    }
}

unsafe fn activate() {
    let a = app();
    a.hidden_for_fullscreen = false;
    ShowWindow(a.box_hwnd, SW_SHOW);
    force_foreground(a.box_hwnd);
    SetFocus(a.edit);
    SendMessageW(a.edit, EM_SETSEL, 0, -1);
}

unsafe fn escape() {
    let a = app();
    if IsWindowVisible(a.list) != 0 {
        show_list(false);
    } else if GetWindowTextLengthW(a.edit) > 0 {
        SetWindowTextW(a.edit, wide("").as_ptr());
    } else {
        // Hand focus back to the taskbar, as EverythingToolbar does.
        let tray = FindWindowW(wide("Shell_TrayWnd").as_ptr(), null());
        if !tray.is_null() {
            force_foreground(tray);
        }
    }
}

/// Ctrl+Backspace: the classic EDIT control inserts 0x7F instead of
/// deleting a word.
unsafe fn delete_word_back(edit: HWND) {
    let mut start: u32 = 0;
    let mut end: u32 = 0;
    SendMessageW(edit, EM_GETSEL, &mut start as *mut u32 as usize, &mut end as *mut u32 as isize);
    let text: Vec<u16> = edit_text().encode_utf16().collect();
    let mut i = (end as usize).min(text.len());
    while i > 0 && text[i - 1] == b' ' as u16 {
        i -= 1;
    }
    while i > 0 && text[i - 1] != b' ' as u16 {
        i -= 1;
    }
    SendMessageW(edit, EM_SETSEL, i, end as isize);
    SendMessageW(edit, EM_REPLACESEL, 1, wide("").as_ptr() as isize);
}

unsafe extern "system" fn edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_KEYDOWN => {
            let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
            let handled = match wparam as u16 {
                VK_DOWN => {
                    move_selection(1);
                    true
                }
                VK_UP => {
                    move_selection(-1);
                    true
                }
                VK_NEXT => {
                    move_selection(VISIBLE_ROWS);
                    true
                }
                VK_PRIOR => {
                    move_selection(-VISIBLE_ROWS);
                    true
                }
                VK_RETURN => {
                    open_item(app().sel, ctrl);
                    true
                }
                VK_ESCAPE => {
                    escape();
                    true
                }
                _ => false,
            };
            if handled {
                return 0;
            }
        }
        WM_CHAR => match wparam {
            0x0D | 0x1B => return 0, // no beep for Enter / Escape
            0x7F => {
                delete_word_back(hwnd);
                return 0;
            }
            _ => {}
        },
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn box_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let a = app();
    match msg {
        ipc::WM_COPYDATA => {
            // Only the newest query's reply is accepted; older ones are stale.
            if let Some(payload) = ipc::reply_payload(msg, lparam, a.latest_id) {
                on_reply(payload);
            }
            1
        }
        WM_APP_NOT_RUNNING => {
            if wparam as u32 == a.latest_id {
                a.items.clear();
                a.total = 0;
                a.status = Status::NotRunning;
                if a.render.is_some() {
                    render_and_quit();
                } else if GetForegroundWindow() == a.box_hwnd {
                    show_list(true);
                }
            }
            0
        }
        WM_COMMAND => {
            let code = (wparam >> 16) as u32 & 0xFFFF;
            if (wparam & 0xFFFF) == ID_EDIT {
                if code == EN_CHANGE {
                    on_text_changed();
                } else if code == EN_SETFOCUS && (a.status != Status::Idle) {
                    show_list(true);
                }
            } else if (wparam & 0xFFFF) == ID_MENU_EXIT && lparam == 0 {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_HOTKEY => {
            if wparam as i32 == HOTKEY_ID {
                activate();
            }
            0
        }
        WM_ACTIVATE => {
            if (wparam & 0xFFFF) as u32 == WA_INACTIVE {
                show_list(false);
            } else {
                SetFocus(a.edit);
            }
            0
        }
        WM_LBUTTONDOWN => {
            SetFocus(a.edit);
            0
        }
        WM_CONTEXTMENU | WM_RBUTTONUP => {
            let menu = CreatePopupMenu();
            let label = match &a.hotkey {
                Some(h) => format!("{h} to focus"),
                None => "No hotkey (see --hotkey)".to_string(),
            };
            AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, wide(&label).as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            AppendMenuW(menu, MF_STRING, ID_MENU_EXIT, wide("Exit evbar").as_ptr());
            let mut pt: POINT = zeroed();
            GetCursorPos(&mut pt);
            SetForegroundWindow(hwnd);
            TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, hwnd, null());
            DestroyMenu(menu);
            0
        }
        WM_CTLCOLOREDIT => {
            let dc = wparam as HDC;
            SetTextColor(dc, a.theme.text);
            SetBkColor(dc, a.theme.bg);
            a.bg_brush as isize
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            paint_box(hwnd, dc);
            EndPaint(hwnd, &ps);
            0
        }
        WM_TIMER if wparam == RENDER_TIMEOUT_TIMER => {
            render_and_quit();
            0
        }
        WM_SETTINGCHANGE => {
            a.theme = Theme::current(a.force_light);
            DeleteObject(a.bg_brush);
            a.bg_brush = CreateSolidBrush(a.theme.bg);
            for h in [a.box_hwnd, a.list] {
                style_frame(h);
                InvalidateRect(h, null(), 1);
            }
            InvalidateRect(a.edit, null(), 1);
            place_windows();
            0
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            place_windows();
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ if msg == a.taskbar_created && msg != 0 => {
            place_windows(); // Explorer restarted
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn paint_box(hwnd: HWND, dc: HDC) {
    let a = app();
    let mut rc: RECT = zeroed();
    GetClientRect(hwnd, &mut rc);
    FillRect(dc, &rc, a.bg_brush);
    if let Some(f) = &a.fonts {
        SelectObject(dc, f.glyph);
        SetBkMode(dc, TRANSPARENT as _);
        SetTextColor(dc, a.theme.dim);
        let mut g = RECT {
            left: scale(10),
            top: 0,
            right: scale(36),
            bottom: rc.bottom,
        };
        let glyph: [u16; 1] = [0xE721]; // magnifier in Segoe MDL2 Assets
        DrawTextW(dc, glyph.as_ptr() as _, 1, &mut g, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    }
}

/// Paints the hidden box and dropdown into one offscreen bitmap, with the
/// rounded corners and border DWM would add, writes it as a .bmp, and exits.
unsafe fn render_and_quit() {
    let a = app();
    let Some(job) = a.render.take() else { return };
    layout_list();
    let (mut br, mut lr): (RECT, RECT) = (zeroed(), zeroed());
    GetWindowRect(a.box_hwnd, &mut br);
    GetWindowRect(a.list, &mut lr);
    let margin = scale(24);
    let ox = br.left.min(lr.left) - margin;
    let oy = br.top.min(lr.top) - margin;
    let w = br.right.max(lr.right) - ox + margin;
    let h = br.bottom.max(lr.bottom) - oy + margin;

    let screen = GetDC(null_mut());
    let dc = CreateCompatibleDC(screen);
    let bmp = CreateCompatibleBitmap(screen, w, h);
    ReleaseDC(null_mut(), screen);
    let old = SelectObject(dc, bmp);
    let backdrop = CreateSolidBrush(if a.theme.bg > rgb(128, 128, 128) { rgb(196, 208, 222) } else { rgb(24, 32, 44) });
    FillRect(dc, &RECT { left: 0, top: 0, right: w, bottom: h }, backdrop);
    DeleteObject(backdrop);

    let border = CreateSolidBrush(a.theme.border);
    let radius = scale(16);
    for (hwnd, r, is_box) in [(a.list, lr, false), (a.box_hwnd, br, true)] {
        let (x, y, cw, ch) = (r.left - ox, r.top - oy, r.right - r.left, r.bottom - r.top);
        let rgn = CreateRoundRectRgn(x, y, x + cw + 1, y + ch + 1, radius, radius);
        SelectClipRgn(dc, rgn);
        SetViewportOrgEx(dc, x, y, null_mut());
        if is_box {
            paint_box(hwnd, dc);
            let mut er: RECT = zeroed();
            GetWindowRect(a.edit, &mut er);
            SetViewportOrgEx(dc, x + er.left - r.left, y + er.top - r.top, null_mut());
            SendMessageW(a.edit, WM_PRINT, dc as usize, (PRF_CLIENT | PRF_ERASEBKGND) as isize);
        } else {
            paint_list(hwnd, dc);
        }
        SetViewportOrgEx(dc, 0, 0, null_mut());
        SelectClipRgn(dc, null_mut());
        FrameRgn(dc, rgn, border, 1, 1);
        DeleteObject(rgn);
    }
    DeleteObject(border);
    SelectObject(dc, old);
    save_bmp(dc, bmp, w, h, &job.out);
    let mut abd: APPBARDATA = zeroed();
    abd.cbSize = size_of::<APPBARDATA>() as u32;
    SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd);
    let rect = |r: RECT| format!("{},{} {}x{}", r.left, r.top, r.right - r.left, r.bottom - r.top);
    let _ = std::fs::write(
        format!("{}.txt", job.out),
        format!(
            "dpi={}\nbox={}\nlist={}\ntaskbar={} edge={}\nstatus={}\nitems={} total={}\n",
            a.dpi,
            rect(br),
            rect(lr),
            rect(abd.rc),
            abd.uEdge,
            match a.status {
                Status::Ready => "ready",
                Status::NotRunning => "not-running",
                Status::Idle => "idle",
            },
            a.items.len(),
            a.total
        ),
    );
    DeleteObject(bmp);
    DeleteDC(dc);
    PostQuitMessage(0);
}

unsafe fn save_bmp(dc: HDC, bmp: HBITMAP, w: i32, h: i32, path: &str) {
    let mut bi: BITMAPINFO = zeroed();
    bi.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w,
        biHeight: h, // bottom-up, as .bmp files store it
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..zeroed()
    };
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    GetDIBits(dc, bmp, 0, h as u32, pixels.as_mut_ptr() as *mut c_void, &mut bi, DIB_RGB_COLORS);
    let header_len = 14 + size_of::<BITMAPINFOHEADER>();
    let mut file = Vec::with_capacity(header_len + pixels.len());
    file.extend_from_slice(b"BM");
    file.extend_from_slice(&((header_len + pixels.len()) as u32).to_le_bytes());
    file.extend_from_slice(&0u32.to_le_bytes());
    file.extend_from_slice(&(header_len as u32).to_le_bytes());
    let ih = std::slice::from_raw_parts(&bi.bmiHeader as *const BITMAPINFOHEADER as *const u8, size_of::<BITMAPINFOHEADER>());
    file.extend_from_slice(ih);
    file.extend_from_slice(&pixels);
    let _ = std::fs::write(path, file);
}

/// Hide while a fullscreen app (game, video) is in front, like the real
/// taskbar does; come back when it leaves.
unsafe extern "system" fn on_foreground_changed(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    let a = app();
    if hwnd.is_null() || hwnd == a.box_hwnd || hwnd == a.list {
        return;
    }
    let fullscreen = {
        let mut class = [0u16; 32];
        let n = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
        let class = String::from_utf16_lossy(&class[..n.max(0) as usize]);
        let mut wr: RECT = zeroed();
        GetWindowRect(hwnd, &mut wr);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi);
        let m = mi.rcMonitor;
        class != "Progman"
            && class != "WorkerW"
            && wr.left <= m.left
            && wr.top <= m.top
            && wr.right >= m.right
            && wr.bottom >= m.bottom
    };
    if fullscreen && IsWindowVisible(a.box_hwnd) != 0 {
        a.hidden_for_fullscreen = true;
        show_list(false);
        ShowWindow(a.box_hwnd, SW_HIDE);
    } else if !fullscreen && a.hidden_for_fullscreen {
        a.hidden_for_fullscreen = false;
        ShowWindow(a.box_hwnd, SW_SHOWNOACTIVATE);
    }
}

fn row_at(y: i32) -> i32 {
    let a = app();
    let r = (y - scale(PAD)) / scale(ROW_H);
    if y < scale(PAD) || r >= list_rows() {
        return -1;
    }
    let i = a.scroll + r;
    if (i as usize) < a.items.len() {
        i
    } else {
        -1
    }
}

unsafe extern "system" fn list_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let a = app();
    let y = ((lparam >> 16) & 0xFFFF) as i16 as i32;
    match msg {
        WM_MOUSEACTIVATE => MA_NOACTIVATE_,
        WM_MOUSEMOVE => {
            if !a.tracking_mouse {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut tme);
                a.tracking_mouse = true;
            }
            let row = row_at(y);
            if row != a.hover {
                a.hover = row;
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_MOUSELEAVE => {
            a.tracking_mouse = false;
            a.hover = -1;
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_LBUTTONUP => {
            let row = row_at(y);
            if row >= 0 {
                a.sel = row;
                let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
                open_item(row, ctrl);
            }
            0
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam >> 16) & 0xFFFF) as i16 as i32;
            let max_scroll = (a.items.len() as i32 - VISIBLE_ROWS).max(0);
            a.scroll = (a.scroll - delta.signum() * 3).clamp(0, max_scroll);
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            paint_list(hwnd, dc);
            EndPaint(hwnd, &ps);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// System image list index for an item's icon, by extension. Uses
/// SHGFI_USEFILEATTRIBUTES so the shell never opens the file.
unsafe fn icon_index(item: &Item) -> i32 {
    let a = app();
    let key = if item.is_folder() {
        "<folder>".to_string()
    } else {
        match item.name.rfind('.') {
            Some(i) => item.name[i..].to_lowercase(),
            None => "<file>".to_string(),
        }
    };
    if let Some(&i) = a.icon_cache.get(&key) {
        return i;
    }
    let (probe, attrs) = if item.is_folder() {
        ("folder".to_string(), FILE_ATTRIBUTE_DIRECTORY)
    } else {
        (format!("file{}", if key.starts_with('.') { &key } else { "" }), FILE_ATTRIBUTE_NORMAL)
    };
    let mut sfi: SHFILEINFOW = zeroed();
    let himl = SHGetFileInfoW(
        wide(&probe).as_ptr(),
        attrs,
        &mut sfi,
        size_of::<SHFILEINFOW>() as u32,
        SHGFI_USEFILEATTRIBUTES | SHGFI_SYSICONINDEX | SHGFI_SMALLICON,
    );
    if himl != 0 {
        a.image_list = himl as isize;
    }
    a.icon_cache.insert(key, sfi.iIcon);
    sfi.iIcon
}

unsafe fn text_width(dc: HDC, s: &[u16]) -> i32 {
    let mut sz: SIZE = zeroed();
    GetTextExtentPoint32W(dc, s.as_ptr(), s.len() as i32, &mut sz);
    sz.cx
}

fn group_digits(n: u32) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

unsafe fn draw_text(dc: HDC, s: &str, rc: &mut RECT, flags: u32) {
    let w: Vec<u16> = s.encode_utf16().collect();
    DrawTextW(dc, w.as_ptr() as _, w.len() as i32, rc, flags | DT_SINGLELINE | DT_NOPREFIX);
}

/// Double-buffered paint of the result rows and footer.
unsafe fn paint_list(hwnd: HWND, target: HDC) {
    let a = app();
    let Some(f) = &a.fonts else { return };
    let t = a.theme;
    let mut rc: RECT = zeroed();
    GetClientRect(hwnd, &mut rc);
    let (w, h) = (rc.right, rc.bottom);
    let dc = CreateCompatibleDC(target);
    let bmp = CreateCompatibleBitmap(target, w, h);
    let old_bmp = SelectObject(dc, bmp);

    let bg = CreateSolidBrush(t.bg);
    FillRect(dc, &rc, bg);
    DeleteObject(bg);
    SetBkMode(dc, TRANSPARENT as _);

    let (pad, row_h) = (scale(PAD), scale(ROW_H));
    let icon = GetSystemMetrics(SM_CXSMICON);
    let text_x = pad + scale(12) + icon + scale(12);
    let text_right = w - pad - scale(12);

    if a.items.is_empty() {
        let (title, sub) = match a.status {
            Status::NotRunning => ("Everything is not running", "Start Voidtools Everything, then keep typing."),
            _ => ("No results", "Everything found nothing for this search."),
        };
        SelectObject(dc, f.name_bold);
        SetTextColor(dc, t.text);
        let mut r = RECT { left: pad + scale(14), top: pad + scale(5), right: text_right, bottom: pad + row_h / 2 + scale(2) };
        draw_text(dc, title, &mut r, DT_LEFT | DT_BOTTOM);
        SelectObject(dc, f.small);
        SetTextColor(dc, t.dim);
        let mut r = RECT { left: pad + scale(14), top: pad + row_h / 2 + scale(2), right: text_right, bottom: pad + row_h };
        draw_text(dc, sub, &mut r, DT_LEFT | DT_TOP | DT_END_ELLIPSIS);
    }

    let sel_brush = CreateSolidBrush(t.sel);
    let accent_brush = CreateSolidBrush(t.accent);
    let null_pen = GetStockObject(NULL_PEN);
    let old_pen = SelectObject(dc, null_pen);
    for r in 0..list_rows() {
        let i = a.scroll + r;
        let Some(item) = a.items.get(i as usize) else { break };
        let top = pad + r * row_h;
        let bottom = top + row_h;
        if i == a.sel || i == a.hover {
            let old_brush = SelectObject(dc, sel_brush);
            RoundRect(dc, pad, top, w - pad, bottom, scale(8), scale(8));
            SelectObject(dc, old_brush);
        }
        if i == a.sel {
            // Windows 11 style selection pill.
            let old_brush = SelectObject(dc, accent_brush);
            RoundRect(dc, pad + scale(2), top + scale(14), pad + scale(5), bottom - scale(14), scale(3), scale(3));
            SelectObject(dc, old_brush);
        }
        if a.image_list != 0 || icon_index(item) >= 0 {
            let idx = icon_index(item);
            ImageList_Draw(a.image_list as _, idx, dc, pad + scale(12), top + (row_h - icon) / 2, ILD_TRANSPARENT);
        }

        // Name line, with Everything's match highlighting.
        let name_top = top + scale(5);
        let name_bottom = top + row_h / 2 + scale(3);
        let runs = highlight::segments(item.highlighted_name.as_deref().unwrap_or(&item.name));
        let mut x = text_x;
        for (run, hit) in runs {
            let wrun: Vec<u16> = run.encode_utf16().collect();
            SelectObject(dc, if hit { f.name_bold } else { f.name });
            SetTextColor(dc, if hit { t.accent } else { t.text });
            let width = text_width(dc, &wrun);
            // The run that overflows ends the line with an ellipsis.
            let mut r = RECT { left: x, top: name_top, right: text_right, bottom: name_bottom };
            DrawTextW(dc, wrun.as_ptr() as _, wrun.len() as i32, &mut r, DT_LEFT | DT_BOTTOM | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
            x += width;
            if x >= text_right {
                break;
            }
        }

        SelectObject(dc, f.small);
        SetTextColor(dc, t.dim);
        let mut pr = RECT { left: text_x, top: name_bottom + scale(1), right: text_right, bottom: bottom - scale(3) };
        draw_text(dc, &item.path, &mut pr, DT_LEFT | DT_TOP | DT_PATH_ELLIPSIS | DT_END_ELLIPSIS);
    }
    SelectObject(dc, old_pen);
    DeleteObject(sel_brush);
    DeleteObject(accent_brush);

    // Footer: key hints and counts.
    SelectObject(dc, f.small);
    SetTextColor(dc, t.dim);
    let mut fr = RECT { left: pad + scale(12), top: h - scale(FOOTER_H), right: w - pad - scale(12), bottom: h - scale(4) };
    draw_text(dc, "Enter open   Ctrl+Enter show in folder   Esc close", &mut fr, DT_LEFT | DT_VCENTER);
    if a.status == Status::Ready {
        let counts = if a.total as usize > a.items.len() {
            format!("{} of {} results", group_digits(a.items.len() as u32), group_digits(a.total))
        } else {
            format!("{} results", group_digits(a.total))
        };
        draw_text(dc, &counts, &mut fr, DT_RIGHT | DT_VCENTER);
    }

    BitBlt(target, 0, 0, w, h, dc, 0, 0, SRCCOPY);
    SelectObject(dc, old_bmp);
    DeleteObject(bmp);
    DeleteDC(dc);
}

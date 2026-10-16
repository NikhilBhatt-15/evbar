//! A small stand-in for Everything's IPC window. It registers the same window
//! class, answers version queries, and answers `QUERY2W` requests the way
//! Everything does: it accepts the `WM_COPYDATA`, returns, then sends the
//! `LIST2` reply to the requested window with the requested reply id.
//!
//! Matching is deliberately simple (case-insensitive AND of space-separated
//! terms against the name, or path+name with `MATCH_PATH`). It exists so the
//! client can be tested without installing Everything.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::mpsc;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostMessageW, RegisterClassExW,
    SendMessageW, TranslateMessage, MSG, WM_APP, WNDCLASSEXW,
};

use crate::highlight;
use crate::ipc::{self, request, search, Item};
use crate::wide;

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub folder: bool,
    pub size: u64,
}

const WM_MOCK_FLUSH: u32 = WM_APP + 0x31;
const VERSION: (usize, usize, usize) = (1, 4, 1);

struct Pending {
    reply_hwnd: usize,
    reply_id: u32,
    payload: Vec<u8>,
}

thread_local! {
    static CORPUS: RefCell<Vec<Entry>> = const { RefCell::new(Vec::new()) };
    static PENDING: RefCell<Vec<Pending>> = const { RefCell::new(Vec::new()) };
}

/// Starts the mock on its own thread and returns its window handle (as
/// `usize`, so it can cross threads) once the window exists.
pub fn spawn(instance: Option<String>, corpus: Vec<Entry>) -> usize {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || unsafe {
        CORPUS.with(|c| *c.borrow_mut() = corpus);
        let hwnd = create_window(instance.as_deref());
        tx.send(hwnd as usize).unwrap();
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
    rx.recv().unwrap()
}

/// Runs the mock on the current thread until its message loop ends.
pub fn run(instance: Option<&str>, corpus: Vec<Entry>) {
    unsafe {
        CORPUS.with(|c| *c.borrow_mut() = corpus);
        create_window(instance);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe fn create_window(instance: Option<&str>) -> HWND {
    let class = wide(&ipc::window_class(instance));
    let hinst = GetModuleHandleW(std::ptr::null());
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: hinst,
        lpszClassName: class.as_ptr(),
        ..std::mem::zeroed()
    };
    RegisterClassExW(&wc);
    // A hidden top-level window: FindWindowW does not see message-only windows.
    CreateWindowExW(
        0,
        class.as_ptr(),
        class.as_ptr(),
        0,
        0,
        0,
        0,
        0,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        hinst,
        std::ptr::null(),
    )
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        ipc::EVERYTHING_WM_IPC => match wparam {
            ipc::IPC_GET_MAJOR_VERSION => VERSION.0 as isize,
            ipc::IPC_GET_MINOR_VERSION => VERSION.1 as isize,
            ipc::IPC_GET_REVISION => VERSION.2 as isize,
            _ => 0,
        },
        ipc::WM_COPYDATA => {
            let cds = &*(lparam as *const COPYDATASTRUCT);
            if cds.dwData != ipc::COPYDATA_QUERY2W || cds.lpData.is_null() {
                return 0;
            }
            let bytes = std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize);
            let Some((reply_hwnd, reply_id, query)) = ipc::decode_query2(bytes) else {
                return 0;
            };
            let payload = CORPUS.with(|c| answer(&c.borrow(), &query));
            PENDING.with(|p| {
                p.borrow_mut().push(Pending {
                    reply_hwnd: reply_hwnd as usize,
                    reply_id,
                    payload,
                })
            });
            // Reply after returning, like Everything does.
            PostMessageW(hwnd, WM_MOCK_FLUSH, 0, 0);
            1
        }
        WM_MOCK_FLUSH => {
            let pending = PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()));
            for r in pending {
                let cds = COPYDATASTRUCT {
                    dwData: r.reply_id as usize,
                    cbData: r.payload.len() as u32,
                    lpData: r.payload.as_ptr() as *mut c_void,
                };
                SendMessageW(
                    r.reply_hwnd as HWND,
                    ipc::WM_COPYDATA,
                    hwnd as usize,
                    &cds as *const COPYDATASTRUCT as isize,
                );
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Builds the `LIST2` reply for a query against the corpus.
pub fn answer(corpus: &[Entry], q: &ipc::Query) -> Vec<u8> {
    let case = q.search_flags & search::MATCH_CASE != 0;
    let fold = |s: &str| if case { s.to_string() } else { s.to_lowercase() };
    let terms: Vec<String> = q.text.split_whitespace().map(fold).collect();
    let mut hits: Vec<&Entry> = corpus
        .iter()
        .filter(|e| {
            let hay = if q.search_flags & search::MATCH_PATH != 0 {
                fold(&format!("{}\\{}", e.path, e.name))
            } else {
                fold(&e.name)
            };
            terms.iter().all(|t| hay.contains(t.as_str()))
        })
        .collect();
    hits.sort_by_key(|e| e.name.to_lowercase());

    let supported = request::FILE_NAME
        | request::PATH
        | request::FULL_PATH_AND_FILE_NAME
        | request::SIZE
        | request::DATE_MODIFIED
        | request::HIGHLIGHTED_FILE_NAME
        | request::HIGHLIGHTED_PATH;
    let flags = q.request_flags & supported;
    let items: Vec<Item> = hits
        .iter()
        .skip(q.offset as usize)
        .take(q.max_results as usize)
        .map(|e| Item {
            flags: if e.folder { ipc::ITEM_FOLDER } else { 0 },
            name: e.name.clone(),
            path: e.path.clone(),
            full_path: None,
            highlighted_name: Some(highlight::mark(&e.name, &terms)),
            highlighted_path: Some(highlight::mark(&e.path, &terms)),
            size: Some(e.size),
            date_modified: Some(0),
        })
        .collect();
    ipc::encode_list2(hits.len() as u32, q.offset, flags, q.sort, &items)
}

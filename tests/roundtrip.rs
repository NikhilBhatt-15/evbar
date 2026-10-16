//! End-to-end check of the IPC client against the mock Everything window:
//! real FindWindowW, real cross-thread WM_COPYDATA in both directions.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use evbar::ipc::{self, request, Query};
use evbar::mock::{self, Entry};
use evbar::{highlight, wide};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, MsgWaitForMultipleObjects, PeekMessageW,
    RegisterClassExW, TranslateMessage, HWND_MESSAGE, MSG, PM_REMOVE, QS_ALLINPUT, WNDCLASSEXW,
};

thread_local! {
    static REPLIES: RefCell<Vec<(usize, Vec<u8>)>> = const { RefCell::new(Vec::new()) };
}

unsafe extern "system" fn reply_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == ipc::WM_COPYDATA {
        let cds = &*(lparam as *const COPYDATASTRUCT);
        let data = std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize).to_vec();
        REPLIES.with(|r| r.borrow_mut().push((cds.dwData, data)));
        return 1;
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe fn reply_window() -> HWND {
    let class = wide("EvbarTestReply");
    let hinst = GetModuleHandleW(std::ptr::null());
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(reply_proc),
        hInstance: hinst,
        lpszClassName: class.as_ptr(),
        ..std::mem::zeroed()
    };
    RegisterClassExW(&wc);
    CreateWindowExW(
        0,
        class.as_ptr(),
        std::ptr::null(),
        0,
        0,
        0,
        0,
        0,
        HWND_MESSAGE,
        std::ptr::null_mut(),
        hinst,
        std::ptr::null(),
    )
}

fn pump_until(count: usize, timeout: Duration) -> Vec<(usize, Vec<u8>)> {
    let deadline = Instant::now() + timeout;
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while REPLIES.with(|r| r.borrow().len()) < count && Instant::now() < deadline {
            MsgWaitForMultipleObjects(0, std::ptr::null(), 0, 20, QS_ALLINPUT);
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
    REPLIES.with(|r| std::mem::take(&mut *r.borrow_mut()))
}

fn corpus() -> Vec<Entry> {
    let e = |name: &str, path: &str, folder: bool, size: u64| Entry {
        name: name.into(),
        path: path.into(),
        folder,
        size,
    };
    vec![
        e("notepad.exe", "C:\\Windows", false, 201_216),
        e("Notes", "D:\\", true, 0),
        e("readme.txt", "C:\\Projects\\notes-app", false, 1_024),
        e("calc.exe", "C:\\Windows\\System32", false, 45_056),
    ]
}

#[test]
fn query_and_reply_over_wm_copydata() {
    const INSTANCE: &str = "evbar-test";
    mock::spawn(Some(INSTANCE.into()), corpus());
    unsafe {
        let reply = reply_window();
        assert!(!reply.is_null());

        let target = ipc::find_any(Some(INSTANCE));
        assert!(!target.is_null(), "mock IPC window not found");
        assert_eq!(ipc::version(target), Some((1, 4, 1)));

        let mut q = Query::new("note");
        q.request_flags = request::FILE_NAME | request::PATH | request::SIZE | request::HIGHLIGHTED_FILE_NAME;
        assert!(ipc::send_query(target, reply, 7, &q));

        let replies = pump_until(1, Duration::from_secs(5));
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].0, 7, "reply id echoed as dwData");
        let list = ipc::decode_list2(&replies[0].1).expect("valid LIST2");
        assert_eq!(list.total, 2);
        let names: Vec<_> = list.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["notepad.exe", "Notes"]);
        assert_eq!(list.items[0].size, Some(201_216));
        assert!(list.items[1].is_folder());
        assert_eq!(list.items[0].full_path(), "C:\\Windows\\notepad.exe");
        let segs = highlight::segments(list.items[0].highlighted_name.as_deref().unwrap());
        assert_eq!(segs[0], ("note".to_string(), true));

        // Path matching widens the result set.
        let mut q = Query::new("notes");
        q.search_flags = ipc::search::MATCH_PATH;
        assert!(ipc::send_query(target, reply, 8, &q));
        // Several queries in flight: each reply carries its own id, which is
        // how the UI drops stale results.
        assert!(ipc::send_query(target, reply, 9, &Query::new("calc")));
        let replies = pump_until(2, Duration::from_secs(5));
        let ids: Vec<_> = replies.iter().map(|r| r.0).collect();
        assert_eq!(ids, [8, 9]);
        let by_path = ipc::decode_list2(&replies[0].1).unwrap();
        assert_eq!(by_path.total, 2);
        let calc = ipc::decode_list2(&replies[1].1).unwrap();
        assert_eq!(calc.items[0].name, "calc.exe");
    }
}

#[test]
fn missing_instance_is_reported() {
    assert!(ipc::find_window(Some("evbar-no-such-instance")).is_null());
}

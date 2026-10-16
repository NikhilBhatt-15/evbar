//! Prints the folder shown by every open File Explorer window, as evbar's
//! automatic folder scope would see it.
//!
//!     cargo run --example explorer_folders

use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::UI::WindowsAndMessaging::EnumWindows;

unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let list = &mut *(lparam as *mut Vec<isize>);
    if evbar::explorer::is_explorer_window(hwnd as isize) {
        list.push(hwnd as isize);
    }
    1
}

fn main() {
    unsafe {
        CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let mut found: Vec<isize> = Vec::new();
        EnumWindows(Some(collect), &mut found as *mut Vec<isize> as LPARAM);
        if found.is_empty() {
            println!("no File Explorer windows open");
        }
        for hwnd in found {
            let start = std::time::Instant::now();
            let folder = evbar::explorer::folder_of(hwnd);
            println!("{hwnd:#x}: {:?} ({} ms)", folder, start.elapsed().as_millis());
        }
    }
}

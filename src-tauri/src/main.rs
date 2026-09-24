#![cfg_attr(
    all(not(debug_assertions), feature = "gui"),
    windows_subsystem = "windows"
)]

fn main() {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with("-psn_"))
        .collect();
    if args.is_empty() {
        #[cfg(feature = "gui")]
        spack::gui::run();
        #[cfg(not(feature = "gui"))]
        spack::cli::run();
    } else {
        #[cfg(windows)]
        attach_parent_console();
        spack::cli::run();
    }
}

#[cfg(windows)]
fn attach_parent_console() {
    use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_OUTPUT_HANDLE,
    };
    unsafe {
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        if !out.is_null() && out != INVALID_HANDLE_VALUE {
            return;
        }
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        let conout: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
        for handle_id in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let h = CreateFileW(
                conout.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                INVALID_HANDLE_VALUE,
            );
            if !h.is_null() && h != INVALID_HANDLE_VALUE {
                SetStdHandle(handle_id, h);
            }
        }
    }
}

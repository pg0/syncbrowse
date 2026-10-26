//! Windows bits: recycle bin, default viewer, folder dialog.
use raw_window_handle::{DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle};
use std::num::NonZeroIsize;
use std::os::windows::ffi::OsStrExt;
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
use windows_sys::Win32::UI::Shell::{FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, SHFILEOPSTRUCTW, SHFileOperationW, ShellExecuteW};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, SW_SHOWNORMAL, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP};

pub fn attach_console() {
    unsafe { windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS) };
}

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain([0]).collect()
}

/// Network shares (NFS/SMB) have no recycle bin: a "trash" there deletes for good.
pub fn on_network(path: &str) -> bool {
    unsafe { GetDriveTypeW(wide(&crate::index::drive_root(path)).as_ptr()) == 4 } // DRIVE_REMOTE
}

pub fn trash(path: &str) -> Result<(), String> {
    let mut from = wide(path);
    from.push(0); // pFrom is a list that ends with a double NUL
    let mut op: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    op.wFunc = FO_DELETE;
    op.pFrom = from.as_ptr();
    op.fFlags = (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI) as _;
    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 || op.fAnyOperationsAborted != 0 || std::path::Path::new(path).exists() {
        return Err(format!("Delete failed: {path}"));
    }
    Ok(())
}

/// Default viewer for a picture, default browser for a URL.
pub fn open(target: &str) {
    unsafe {
        ShellExecuteW(null_mut(), wide("open").as_ptr(), wide(target).as_ptr(), null(), null(), SW_SHOWNORMAL);
    }
}

struct Owner(HWND);

impl HasWindowHandle for Owner {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let h = Win32WindowHandle::new(NonZeroIsize::new(self.0 as isize).ok_or(HandleError::Unavailable)?);
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(h)) })
    }
}

impl HasDisplayHandle for Owner {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::windows())
    }
}

/// Native folder dialog, owned by a hidden topmost window: without an owner it opens behind the browser.
pub fn pick_folder(start: &str) -> Option<String> {
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    let _one = ONE_AT_A_TIME.lock().ok()?;
    let class = wide("STATIC");
    let owner = unsafe {
        CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW, class.as_ptr(), null(), WS_POPUP, 0, 0, 0, 0,
            null_mut(), null_mut(), null_mut(), null())
    };
    let mut dialog = rfd::FileDialog::new().set_title("syncbrowse: choose folder").set_directory(start);
    if !owner.is_null() {
        dialog = dialog.set_parent(&Owner(owner));
    }
    let chosen = dialog.pick_folder();
    if !owner.is_null() {
        unsafe { DestroyWindow(owner) };
    }
    chosen.map(|p| p.to_string_lossy().into_owned())
}

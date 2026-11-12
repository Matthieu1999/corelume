//! Folders and the account name an elevated Corelume relies on, asked from Windows itself.
//! Environment variables such as SystemRoot, ProgramFiles, TEMP or USERNAME can be set by
//! any program running as the user, so a DLL or command found through them could run with
//! administrator rights.

use std::path::PathBuf;
use windows_sys::Win32::Security::Authentication::Identity::{GetUserNameExW, NameSamCompatible};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramFiles, SHGetKnownFolderPath};

fn from_wide(buf: &[u16]) -> String {
    String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())])
}

/// C:\Windows\System32.
pub fn system32() -> PathBuf {
    let mut buf = [0u16; 260];
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return PathBuf::from(r"C:\Windows\System32");
    }
    PathBuf::from(from_wide(&buf))
}

/// C:\Program Files (the 64-bit one), a per-machine folder only administrators can write to.
pub fn program_files() -> Option<PathBuf> {
    let mut raw = std::ptr::null_mut();
    let hr = unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFiles, 0, std::ptr::null_mut(), &mut raw) };
    let path = (hr >= 0 && !raw.is_null()).then(|| {
        let len = (0..).take_while(|&i| unsafe { *raw.add(i) } != 0).count();
        PathBuf::from(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(raw, len) }))
    });
    unsafe { CoTaskMemFree(raw.cast()) };
    path
}

/// "DOMAIN\user" of the account this process runs as, from its token.
pub fn user_name() -> Option<String> {
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    unsafe { GetUserNameExW(NameSamCompatible, buf.as_mut_ptr(), &mut len) }.then(|| from_wide(&buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_come_from_windows_not_the_environment() {
        assert!(system32().join("schtasks.exe").exists());
        let pf = program_files().unwrap();
        assert!(pf.is_absolute() && pf.exists());
        assert!(user_name().unwrap().contains('\\'));
    }
}

//! Elevated start without a UAC prompt, through two Task Scheduler entries that run
//! Corelume "with highest privileges":
//! - `Corelume` starts it at logon (`--autostart`); the tray's "Start with Windows" toggles it;
//! - `Corelume (open)` has no trigger: a non-elevated launch (Start menu) runs it and exits,
//!   so the dashboard always comes up elevated, with CPU temperatures.
//! Creating either needs admin: the installer calls `corelume.exe --install-tasks`.

use super::paths;
use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub const LOGON_TASK: &str = "Corelume";
pub const OPEN_TASK: &str = "Corelume (open)";
pub const AUTOSTART_ARG: &str = "--autostart";
/// Argument of the "open" task, so a non-elevated copy started by it never redirects again.
pub const FROM_TASK_ARG: &str = "--from-task";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const FILE_SHARE_READ: u32 = 1;

pub fn is_elevated() -> bool {
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len = 0u32;
    let ok = unsafe {
        GetTokenInformation(token, TokenElevation, (&raw mut elevation).cast(), size_of::<TOKEN_ELEVATION>() as u32, &mut len)
    };
    unsafe { CloseHandle(token) };
    ok != 0 && elevation.TokenIsElevated != 0
}

fn schtasks(args: &[&str]) -> bool {
    Command::new(paths::system32().join("schtasks.exe"))
        .args(args)
        .creation_flags(CREATE_NO_WINDOW) // Corelume is a GUI app: no console flash
        .output()
        .is_ok_and(|o| o.status.success())
}

pub fn exists(name: &str) -> bool {
    schtasks(&["/query", "/tn", name])
}

/// Starts the task; Task Scheduler runs it elevated without asking.
pub fn run(name: &str) -> bool {
    schtasks(&["/run", "/tn", name])
}

pub fn remove(name: &str) -> bool {
    schtasks(&["/delete", "/tn", name, "/f"])
}

/// Creates or replaces a task running this executable for the current user. The task starts
/// it elevated, so only a copy in Program Files may: there, only administrators can replace
/// the file. A copy elsewhere (a build folder) would hand admin rights to whoever edits it.
pub fn create(name: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    if !installed(&exe, paths::program_files().as_deref()) {
        return Err("only an installed Corelume (in Program Files) can create its scheduled tasks".into());
    }
    register(name, &task_xml(&exe.to_string_lossy(), &current_user()?, name == LOGON_TASK))
}

fn installed(exe: &Path, program_files: Option<&Path>) -> bool {
    program_files.is_some_and(|pf| exe.starts_with(pf))
}

// shortcut: the task belongs to the user running the installer; a standard user who
// types an admin's password at the UAC prompt gets tasks for that admin account.
fn current_user() -> Result<String, String> {
    paths::user_name().ok_or_else(|| "unknown user".to_string())
}

/// The XML goes next to the executable (Program Files once installed, not the user's TEMP),
/// in a new file locked against writes and deletes until schtasks has read it, so no other
/// program can swap the task it registers.
fn register(name: &str, xml: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("no folder for the task file")?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let path = dir.join(format!("corelume-task-{}-{stamp}.xml", std::process::id()));
    // schtasks reads task XML as UTF-16 LE with a byte-order mark.
    let bytes: Vec<u8> = std::iter::once(0xFEFFu16).chain(xml.encode_utf16()).flat_map(u16::to_le_bytes).collect();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_READ)
        .open(&path)
        .map_err(|e| format!("could not write the task file: {e}"))?;
    file.write_all(&bytes).map_err(|e| e.to_string())?;
    drop(file);
    // schtasks cannot read past our write handle; a read-only one still denies writes and deletes.
    let lock = std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(&path).map_err(|e| e.to_string())?;
    let ok = schtasks(&["/create", "/tn", name, "/xml", &path.to_string_lossy(), "/f"]);
    drop(lock);
    let _ = std::fs::remove_file(&path);
    ok.then_some(()).ok_or_else(|| format!("could not create the scheduled task \"{name}\" (needs admin)"))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Settings that matter: no 72 h time limit, keeps running on battery, normal priority
/// (Task Scheduler's default is below normal), and parallel runs so "open" works while
/// Corelume is already running (the new copy hands over to it and exits).
fn task_xml(exe: &str, user: &str, at_logon: bool) -> String {
    let trigger = if at_logon {
        // The delay lets the taskbar come up before Corelume adds its tray icon.
        format!("<LogonTrigger><Enabled>true</Enabled><UserId>{}</UserId><Delay>PT10S</Delay></LogonTrigger>", escape(user))
    } else {
        String::new()
    };
    let args = if at_logon { AUTOSTART_ARG } else { FROM_TASK_ARG };
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Corelume system monitor, elevated to read CPU temperatures.</Description></RegistrationInfo>
  <Triggers>{trigger}</Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>Parallel</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
    <IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>
  </Settings>
  <Actions Context="Author">
    <Exec><Command>{exe}</Command><Arguments>{args}</Arguments></Exec>
  </Actions>
</Task>"#,
        user = escape(user),
        exe = escape(exe),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logon_task_xml_has_trigger_arguments_and_escaping() {
        let xml = task_xml(r"C:\Program Files\A&B\corelume.exe", r"PC\Ann", true);
        assert!(xml.contains(r"<Command>C:\Program Files\A&amp;B\corelume.exe</Command>"));
        assert!(xml.contains(r"<LogonTrigger><Enabled>true</Enabled><UserId>PC\Ann</UserId>"));
        assert!(xml.contains("<Arguments>--autostart</Arguments>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
    }

    #[test]
    #[ignore = "creates then deletes a test task in Task Scheduler: cargo test -- --ignored"]
    fn task_scheduler_accepts_the_xml() {
        // Same XML at least privilege, which a non-elevated test is allowed to register.
        let name = "Corelume XML check";
        let xml = task_xml(r"C:\Windows\System32\notepad.exe", &current_user().unwrap(), true)
            .replace("HighestAvailable", "LeastPrivilege");
        register(name, &xml).unwrap();
        assert!(exists(name));
        assert!(remove(name));
        assert!(!exists(name));
    }

    #[test]
    fn only_a_copy_in_program_files_creates_tasks() {
        let pf = Path::new(r"C:\Program Files");
        assert!(installed(Path::new(r"C:\Program Files\Corelume\corelume.exe"), Some(pf)));
        assert!(!installed(Path::new(r"C:\Users\a\corelume\target\release\corelume.exe"), Some(pf)));
        assert!(!installed(Path::new(r"C:\Program Files (x86)\x.exe"), Some(pf)), "a prefix of the name is not the folder");
        assert!(!installed(Path::new(r"C:\Program Files\Corelume\corelume.exe"), None));
    }

    #[test]
    fn open_task_xml_has_no_trigger() {
        let xml = task_xml(r"C:\d.exe", r"PC\Ann", false);
        assert!(xml.contains("<Triggers></Triggers>"));
        assert!(xml.contains("<Arguments>--from-task</Arguments>"));
    }
}

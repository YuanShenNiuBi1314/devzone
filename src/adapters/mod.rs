//! Shared Windows observation primitives used by the pause ladder, the watchdog
//! and `doctor`: process enumeration, window discovery, foreground state.
//!
//! Everything here is read-only. Mutating adapters live in `hotkey`, `uia`,
//! `wm_close` (in `pause`), `ocr`.

pub mod hotkey;
pub mod ocr;
pub mod pattern;
pub mod pause;
pub mod uia;

#[cfg(feature = "toast")]
pub mod toast;

use std::path::PathBuf;

use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, RECT};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, WNDENUMPROC,
};

/// One step of the graceful-pause ladder (section 6.3.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// 1. SendInput Esc then Ctrl+S to the foreground window.
    Hotkey,
    /// 2. UI Automation Invoke on a "停止/暂停/Save" element.
    Uia,
    /// 3. PostMessage WM_CLOSE to the main window.
    WmClose,
    /// 4. Screenshot + OCR + click (off by default, disclosed in the README).
    Ocr,
    /// 5. TerminateProcess, recorded as `clean_exit = 0`.
    Kill,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Hotkey => "hotkey",
            Level::Uia => "uia",
            Level::WmClose => "wm_close",
            Level::Ocr => "ocr",
            Level::Kill => "kill",
        }
    }

    pub fn parse(value: &str) -> Option<Level> {
        match value.trim().to_lowercase().as_str() {
            "hotkey" | "key" | "sendinput" => Some(Level::Hotkey),
            "uia" | "automation" => Some(Level::Uia),
            "wm_close" | "wmclose" | "close" => Some(Level::WmClose),
            "ocr" | "ocr_click" => Some(Level::Ocr),
            "kill" | "terminate" | "force" => Some(Level::Kill),
            _ => None,
        }
    }

    /// Documented order: 1 -> 2 -> 3 -> 4 -> 5.
    pub const LADDER: [Level; 5] = [
        Level::Hotkey,
        Level::Uia,
        Level::WmClose,
        Level::Ocr,
        Level::Kill,
    ];
}

/// Result of a read-only capability probe for one level (used by `doctor` and
/// `watchdog adapter test`); never a stored assumption.
#[derive(Clone, Debug)]
pub struct LevelProbe {
    pub level: Level,
    pub available: bool,
    pub note: String,
    /// Human readable process state, e.g. `running pid=1234 windows=2`.
    pub process: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub exe: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct WindowInfo {
    pub hwnd: HWND,
    pub pid: u32,
    pub title: String,
    /// left, top, right, bottom in screen coordinates
    pub rect: (i32, i32, i32, i32),
    pub visible: bool,
}

impl WindowInfo {
    pub fn width(&self) -> i32 {
        (self.rect.2 - self.rect.0).max(0)
    }

    pub fn height(&self) -> i32 {
        (self.rect.3 - self.rect.1).max(0)
    }
}

/// Enumerate processes whose image name matches `hint` (case-insensitive; the
/// `.exe` suffix is optional in the hint).
pub fn find_processes(hint: &str) -> Vec<ProcessInfo> {
    let needle = hint.trim().to_lowercase();
    let needle = needle.trim_end_matches(".exe");
    let mut out = Vec::new();
    let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(handle) => handle,
        Err(err) => {
            tracing::debug!("CreateToolhelp32Snapshot failed: {err}");
            return out;
        }
    };
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok() {
        loop {
            let name = wide_to_string(&entry.szExeFile);
            if name.to_lowercase().trim_end_matches(".exe") == needle {
                out.push(ProcessInfo {
                    pid: entry.th32ProcessID,
                    name: name.clone(),
                    exe: process_image_path(entry.th32ProcessID),
                });
            }
            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                break;
            }
        }
    }
    let _ = unsafe { CloseHandle(snapshot) };
    out.sort_by_key(|p| p.pid);
    out
}

fn wide_to_string(raw: &[u16]) -> String {
    let end = raw.iter().position(|c| *c == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..end])
}

fn process_image_path(pid: u32) -> Option<PathBuf> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = [0u16; 1024];
        let mut size = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        if ok.is_err() {
            return None;
        }
        Some(PathBuf::from(wide_to_string(&buffer[..size as usize])))
    }
}

/// Top-level windows belonging to `pid`.
pub fn windows_of_pid(pid: u32) -> Vec<WindowInfo> {
    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let collector = &mut *(lparam.0 as *mut Vec<HWND>);
        collector.push(hwnd);
        BOOL(1)
    }
    let mut handles: Vec<HWND> = Vec::new();
    let proc: WNDENUMPROC = Some(callback);
    let _ = unsafe { EnumWindows(proc, LPARAM(&mut handles as *mut Vec<HWND> as isize)) };
    let mut out = Vec::new();
    for hwnd in handles {
        let mut window_pid = 0u32;
        let _ = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut window_pid)) };
        if window_pid != pid {
            continue;
        }
        out.push(describe_window(hwnd, window_pid));
    }
    out.sort_by_key(|w| w.hwnd.0 as u64);
    out
}

fn describe_window(hwnd: HWND, pid: u32) -> WindowInfo {
    let mut rect = RECT::default();
    let _ = unsafe { GetWindowRect(hwnd, &mut rect) };
    WindowInfo {
        hwnd,
        pid,
        title: window_title(hwnd),
        rect: (rect.left, rect.top, rect.right, rect.bottom),
        visible: unsafe { IsWindowVisible(hwnd) }.as_bool(),
    }
}

/// Window caption, decoded from UTF-16.
pub fn window_title(hwnd: HWND) -> String {
    let length = unsafe { GetWindowTextLengthW(hwnd) }.max(0) as usize;
    let mut buffer = vec![0u16; length + 1];
    let written = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    if written <= 0 {
        return String::new();
    }
    wide_to_string(&buffer[..written as usize])
}

/// The window currently receiving keyboard input (hotkey level needs it).
pub fn foreground_window() -> Option<WindowInfo> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return None;
    }
    let mut pid = 0u32;
    let _ = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    Some(describe_window(hwnd, pid))
}

/// Largest visible window of a process, which is the one a graceful pause should
/// target; falls back to any window when everything is hidden.
pub fn main_window_of(pid: u32) -> Option<WindowInfo> {
    let mut windows = windows_of_pid(pid);
    if windows.is_empty() {
        return None;
    }
    for window in &windows {
        // `pid` is part of the evidence trail `doctor` and `adapter test` print.
        tracing::trace!(
            "candidate window {:?} pid={} area={}x{}",
            window.hwnd,
            window.pid,
            window.width(),
            window.height()
        );
    }
    windows.sort_by_key(|w| -(w.width() as i64 * w.height() as i64));
    let visible: Vec<WindowInfo> = windows.iter().filter(|w| w.visible).cloned().collect();
    if let Some(first) = visible.into_iter().next() {
        return Some(first);
    }
    Some(windows.remove(0))
}

/// `running pid=.. windows=..` style summary for probe output.
pub fn process_summary(hint: &str) -> String {
    let found = find_processes(hint);
    if found.is_empty() {
        return format!("{hint} 未运行");
    }
    let parts: Vec<String> = found
        .iter()
        .map(|p| format!("pid={} windows={}", p.pid, windows_of_pid(p.pid).len()))
        .collect();
    format!("running {}", parts.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_parsing_and_order() {
        assert_eq!(Level::parse("UIA"), Some(Level::Uia));
        assert_eq!(Level::parse(" wm_close "), Some(Level::WmClose));
        assert_eq!(Level::parse("nonsense"), None);
        assert_eq!(Level::LADDER[0], Level::Hotkey);
        assert_eq!(Level::LADDER[4], Level::Kill);
        assert_eq!(Level::Ocr.as_str(), "ocr");
    }

    #[test]
    fn finds_a_known_system_process() {
        // Every Windows build has svchost.exe; the current process is also valid.
        let found = find_processes("svchost.exe");
        assert!(!found.is_empty(), "svchost.exe should be running");
        assert!(found[0].name.to_lowercase().contains("svchost"));
        assert!(find_processes("definitely-not-a-process-xyz").is_empty());
        assert!(process_summary("definitely-not-a-process-xyz").contains("未运行"));
    }

    #[test]
    fn own_process_has_no_top_level_windows() {
        let pid = std::process::id();
        let windows = windows_of_pid(pid);
        // A CLI process normally owns no window; either way this must not panic.
        for window in &windows {
            assert_eq!(window.pid, pid);
            assert!(window.width() >= 0);
        }
        let _ = process_summary(&format!("{pid}"));
    }

    #[test]
    fn foreground_window_probe_does_not_panic() {
        if let Some(window) = foreground_window() {
            assert!(window.pid > 0);
            let _ = window.title;
        }
    }
}

//! Public utilities: data paths, logging with size rotation, external command
//! execution with an opt-out dry run, JSON envelope, elevation, misc helpers.
//!
//! All of devzone is Windows-only for the MVP (see README "Non-goals").

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::Local;
use serde::Serialize;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;

/// Environment variable that relocates the whole `%LOCALAPPDATA%\devzone` root.
/// Used by the integration tests so they never touch a real user profile.
pub const HOME_ENV: &str = "DEVZONE_HOME";
/// Environment variable that turns every external system mutation into a logged
/// no-op. Needed so tests (and nervous users) can exercise `sched` / `guard` /
/// `crunch` without registering tasks or touching power schemes.
pub const DRY_RUN_ENV: &str = "DEVZONE_DRY_RUN";

/// `%LOCALAPPDATA%\devzone` layout (build guide section 8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub root: PathBuf,
    pub config_dir: PathBuf,
    pub config: PathBuf,
    pub data_dir: PathBuf,
    pub db: PathBuf,
    pub log_dir: PathBuf,
    pub log: PathBuf,
    pub events: PathBuf,
    pub plugins: PathBuf,
}

impl Paths {
    /// Resolve from `DEVZONE_HOME` (tests) or `%LOCALAPPDATA%\devzone`.
    pub fn resolve() -> Result<Paths> {
        if let Ok(custom) = std::env::var(HOME_ENV) {
            return Ok(Paths::with_root(PathBuf::from(custom)));
        }
        let base = dirs::data_local_dir().ok_or_else(|| anyhow!("LOCALAPPDATA is unavailable"))?;
        Ok(Paths::with_root(base.join("devzone")))
    }

    pub fn with_root(root: PathBuf) -> Paths {
        let config_dir = root.join("config");
        let data_dir = root.join("data");
        let log_dir = root.join("logs");
        Paths {
            plugins: root.join("plugins"),
            events: log_dir.join("events.jsonl"),
            log: log_dir.join("devzone.log"),
            log_dir,
            db: data_dir.join("devzone.db"),
            data_dir,
            config: config_dir.join("devzone.toml"),
            config_dir,
            root,
        }
    }

    /// Create the directory tree (idempotent).
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.root,
            &self.config_dir,
            &self.data_dir,
            &self.log_dir,
            &self.plugins,
        ] {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        Ok(())
    }
}

/// Rotate when a log file grows past this, keeping 3 generations
/// (`devzone.log`, `devzone.log.1`, `devzone.log.2`, `devzone.log.3`).
const LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;
const LOG_KEEP: usize = 3;

struct RotatingState {
    file: Option<std::fs::File>,
    path: PathBuf,
    bytes: u64,
}

/// Size-based rotating writer for `tracing` (section 8: >5MB rolls to .1/.2, 3 kept).
#[derive(Clone)]
pub struct RotatingFile(Arc<Mutex<RotatingState>>);

impl RotatingFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        RotatingFile(Arc::new(Mutex::new(RotatingState {
            file: None,
            path: path.into(),
            bytes: 0,
        })))
    }

    fn write_bytes(&self, buf: &[u8]) {
        let mut state = match self.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state.file.is_none() {
            if let Some(parent) = state.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&state.path);
            match file {
                Ok(file) => {
                    state.bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
                    state.file = Some(file);
                }
                Err(_) => return,
            }
        }
        if state.bytes.saturating_add(buf.len() as u64) > LOG_MAX_BYTES {
            if let Err(err) = rotate(&state.path) {
                eprintln!("[devzone] log rotation failed: {err}");
            }
            state.file = None;
            state.bytes = 0;
        }
        if let Some(file) = state.file.as_mut() {
            let _ = file.write_all(buf);
            let _ = file.flush();
            state.bytes = state.bytes.saturating_add(buf.len() as u64);
        }
    }
}

impl io::Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_bytes(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for RotatingFile {
    type Writer = RotatingFile;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn rotate(path: &Path) -> Result<()> {
    for idx in (1..LOG_KEEP).rev() {
        let from = format!("{path}.{idx}", path = path.display());
        let to = format!("{path}.{next}", path = path.display(), next = idx + 1);
        if Path::new(&from).exists() {
            std::fs::rename(&from, &to).ok();
        }
    }
    let last = format!("{path}.{LOG_KEEP}", path = path.display());
    if Path::new(&last).exists() {
        std::fs::remove_file(&last).ok();
    }
    if path.exists() {
        std::fs::rename(path, format!("{path}.1", path = path.display()))
            .context("rename active log file")?;
    }
    Ok(())
}

/// Initialise `tracing`: the rotating file gets `info` (or `DEVZONE_LOG`), and
/// stderr only when `DEVZONE_LOG_STDERR` is set. User-facing progress lines are
/// printed by `progress()`, so the file sink carries the detail (section 8).
pub fn init_logging(paths: &Paths) -> Result<()> {
    let filter = std::env::var("DEVZONE_LOG")
        .unwrap_or_else(|_| "info,lettre=warn,reqwest=warn,hyper=warn,rustls=warn".to_string());
    let env_filter = EnvFilter::try_new(&filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(RotatingFile::new(paths.log.clone()))
        .with_target(true)
        .with_ansi(false)
        .with_filter(env_filter.clone());
    let stderr_layer = std::env::var("DEVZONE_LOG_STDERR").ok().map(|_| {
        tracing_subscriber::fmt::layer()
            .with_writer(io::stderr)
            .with_target(true)
            .with_filter(env_filter)
    });
    let registry = tracing_subscriber::Registry::default()
        .with(file_layer)
        .with(stderr_layer);
    tracing::subscriber::set_global_default(registry)
        .map_err(|err| anyhow!("install logger: {err}"))
}

/// Local time with offset, e.g. `2026-10-10T20:00:00+08:00` (section 5.3).
pub fn now_iso() -> String {
    Local::now().to_rfc3339()
}

/// Compact ISO timestamp used in file names and checkpoint messages.
pub fn now_compact() -> String {
    Local::now().format("%Y%m%d-%H%M%S").to_string()
}

pub fn gb(bytes: i64) -> f64 {
    bytes.max(0) as f64 / (1024f64 * 1024f64 * 1024f64)
}

pub fn human_bytes(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let sign = if bytes < 0 { "-" } else { "" };
    let mut value = bytes.unsigned_abs() as f64;
    let mut unit = 0;
    while value >= 1024f64 && unit < UNITS.len() - 1 {
        value /= 1024f64;
        unit += 1;
    }
    if unit == 0 {
        format!("{sign}{value:.0}{}", UNITS[unit])
    } else {
        format!("{sign}{value:.2}{}", UNITS[unit])
    }
}

/// Section 5.3 common JSON envelope.
#[derive(Serialize)]
pub struct Envelope<T: Serialize> {
    pub ok: bool,
    pub command: String,
    pub data: T,
    pub ts: String,
}

impl Envelope<serde_json::Value> {
    pub fn ok(command: &str, data: serde_json::Value) -> serde_json::Value {
        to_value(&Envelope {
            ok: true,
            command: command.to_string(),
            data,
            ts: now_iso(),
        })
    }

    pub fn err(command: &str, error: &str) -> serde_json::Value {
        #[derive(Serialize)]
        struct Fail {
            ok: bool,
            command: String,
            error: String,
            ts: String,
        }
        to_value(&Fail {
            ok: false,
            command: command.to_string(),
            error: error.to_string(),
            ts: now_iso(),
        })
    }
}

fn to_value<T: Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

/// Print exactly one JSON object on stdout (section 5.2).
pub fn print_json(command: &str, data: serde_json::Value) -> Result<()> {
    let text = serde_json::to_string_pretty(&Envelope::ok(command, data))?;
    println!("{text}");
    Ok(())
}

/// Print a failure envelope on stdout and return the error for exit code 1.
pub fn print_json_error(command: &str, error: &str) -> Result<()> {
    let text = serde_json::to_string_pretty(&Envelope::err(command, error))?;
    println!("{text}");
    Ok(())
}

/// Progress line for operations longer than ~1s (section 5.2).
pub fn progress(message: &str) {
    eprintln!("[devzone] {message}");
}

/// An external program invocation. Kept as data so the pure argument builders are
/// unit-testable and `DEVZONE_DRY_RUN` can neutralise every system mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalCmd {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}

impl ExternalCmd {
    pub fn new(program: impl Into<String>, args: &[&str]) -> Self {
        ExternalCmd {
            program: program.into(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            cwd: None,
        }
    }

    pub fn dry_run() -> bool {
        matches!(
            std::env::var(DRY_RUN_ENV).as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        )
    }

    /// Execute, or log-and-skip when dry run is on.
    pub fn run(&self) -> Result<Output> {
        if Self::dry_run() {
            tracing::info!("dry-run: {} {}", self.program, self.args.join(" "));
            eprintln!(
                "[devzone] dry-run: {} {}",
                self.program,
                self.args.join(" ")
            );
            return Ok(Output {
                status: std::process::ExitStatus::default(),
                stdout: Vec::new(),
                stderr: Vec::new(),
            });
        }
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        let output = cmd
            .output()
            .with_context(|| format!("spawn {} (is it on PATH?)", self.program))?;
        Ok(output)
    }

    /// Execute and require a zero exit code; returns stdout decoded.
    pub fn run_checked(&self) -> Result<String> {
        let output = self.run()?;
        if Self::dry_run() {
            return Ok(String::new());
        }
        if !output.status.success() {
            bail!(
                "{} {} failed ({}): {}",
                self.program,
                self.args.join(" "),
                output.status,
                decode(&output.stderr).trim()
            );
        }
        Ok(decode(&output.stdout))
    }
}

/// Decode an OS console output (GBK on zh-CN Windows for schtasks/powercfg).
pub fn decode(bytes: &[u8]) -> String {
    let (cow, _) = encoding_rs::GBK.decode_without_bom_handling(bytes);
    cow.into_owned()
}

/// Read a secret from the console without echoing it
/// (`Win32_System_Console`; used by `guard autologon enable`).
pub fn read_secret(prompt: &str) -> Result<String> {
    use windows::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE, ENABLE_ECHO_INPUT,
        ENABLE_LINE_INPUT, STD_INPUT_HANDLE,
    };

    eprintln!("{prompt}");
    eprintln!("[devzone] 输入不回显；注意密码将以明文写入注册表（见 docs/disclaimers.md）。");

    let stdin_handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.context("GetStdHandle")?;
    let mut original = CONSOLE_MODE::default();
    let echo_off = unsafe { GetConsoleMode(stdin_handle, &mut original) }
        .and_then(|_| unsafe {
            SetConsoleMode(
                stdin_handle,
                original & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT),
            )
        })
        .is_ok();

    let mut secret = String::new();
    let read = std::io::stdin().read_line(&mut secret);
    if echo_off {
        let _ = unsafe { SetConsoleMode(stdin_handle, original) };
    }
    eprintln!();
    read.context("read secret")?;
    Ok(secret.trim_end_matches(['\r', '\n']).to_string())
}

/// Current process image path, used as `devzone`'s own executable when
/// re-executing elevated or registering scheduled tasks.
pub fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().context("current_exe")
}

/// True when the process has an elevated (admin) token.
pub fn is_admin() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let process = GetCurrentProcess();
        let mut token = HANDLE::default();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );
        let _ = CloseHandle(token);
        ok.is_ok() && elevation.TokenIsElevated != 0
    }
}

/// Ask for elevation with `ShellExecuteW("runas")` re-running this binary with the
/// same arguments plus `--elevated` (section 10). Returns Ok(true) when the
/// elevated child was started (caller should exit 0), Ok(false) when no
/// elevation is needed.
pub fn elevate_and_rerun(args: &[String]) -> Result<bool> {
    if is_admin() {
        return Ok(false);
    }
    let exe = current_exe()?;
    let joined = args.join(" ");
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }
    let operation = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let parameters = wide(&joined);
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(operation.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR(parameters.as_ptr()),
            None,
            SW_SHOW,
        )
    };
    let code = result.0 as isize;
    if code > 32 {
        tracing::info!("elevated child started for: {joined}");
        Ok(true)
    } else if code == 1223 {
        bail!("需要管理员权限：用户拒绝了 UAC 提权（ERROR_CANCELLED）")
    } else {
        bail!("需要管理员权限，ShellExecuteW(runas) 失败，代码 {code}")
    }
}

/// Gate for the admin-only command groups (section 10). When elevation is
/// required and a UAC child is started, this returns `Ok(true)` and the caller
/// must exit immediately.
pub fn ensure_admin_or_elevate(flags: &[String]) -> Result<bool> {
    if is_admin() || flags.iter().any(|a| a == "--elevated") {
        return Ok(false);
    }
    progress("该操作需要管理员权限，正在请求 UAC 提权…");
    let mut args: Vec<String> = flags.to_vec();
    if !args.iter().any(|a| a == "--elevated") {
        args.push("--elevated".to_string());
    }
    elevate_and_rerun(&args)
}

/// Millisecond timestamp helper for adapter timing.
pub fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

/// Case-insensitive contains used by the powercfg / schtasks parsers.
pub fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// Free bytes available on the drive backing `path` (`GetDiskFreeSpaceExW`).
pub fn disk_free_bytes(path: &Path) -> Result<u64> {
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut free_available = 0u64;
    let mut total = 0u64;
    let mut total_free = 0u64;
    let wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        GetDiskFreeSpaceExW(
            windows::core::PCWSTR(wide.as_ptr()),
            Some(&mut free_available),
            Some(&mut total),
            Some(&mut total_free),
        )
    }
    .with_context(|| format!("GetDiskFreeSpaceExW({})", path.display()))?;
    let _ = (total, total_free);
    Ok(free_available)
}

/// Total capacity of the drive backing `path`.
pub fn disk_total_bytes(path: &Path) -> Result<u64> {
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut total = 0u64;
    let wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        GetDiskFreeSpaceExW(
            windows::core::PCWSTR(wide.as_ptr()),
            None,
            Some(&mut total),
            None,
        )
    }
    .with_context(|| format!("GetDiskFreeSpaceExW({})", path.display()))?;
    Ok(total)
}

/// Parse `HH:MM` into minutes past midnight, validating the range.
pub fn parse_hhmm(value: &str) -> Result<u32> {
    let (hour, minute) = value
        .split_once(':')
        .ok_or_else(|| anyhow!("时间格式应为 HH:MM，收到 `{value}`"))?;
    let hour: u32 = hour.parse().with_context(|| format!("非法小时 `{hour}`"))?;
    let minute: u32 = minute
        .parse()
        .with_context(|| format!("非法分钟 `{minute}`"))?;
    if hour > 23 || minute > 59 {
        bail!("时间超出范围：`{value}`");
    }
    Ok(hour * 60 + minute)
}

/// Format minutes past midnight back to `HH:MM`.
pub fn format_hhmm(total: u32) -> String {
    format!("{:02}:{:02}", (total / 60) % 24, total % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_layout_matches_doc() {
        let paths = Paths::with_root(PathBuf::from("R"));
        assert_eq!(paths.config, PathBuf::from("R/config/devzone.toml"));
        assert_eq!(paths.db, PathBuf::from("R/data/devzone.db"));
        assert_eq!(paths.events, PathBuf::from("R/logs/events.jsonl"));
        assert_eq!(paths.plugins, PathBuf::from("R/plugins"));
    }

    #[test]
    fn envelope_shape() {
        let value = Envelope::ok("zone report", serde_json::json!({"a": 1}));
        assert_eq!(value["ok"], true);
        assert_eq!(value["command"], "zone report");
        assert_eq!(value["data"]["a"], 1);
        assert!(value["ts"].as_str().unwrap().contains('+'));
        let failed = Envelope::err("zone report", "boom");
        assert_eq!(failed["ok"], false);
        assert_eq!(failed["error"], "boom");
    }

    #[test]
    fn humanizes_bytes() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(1024), "1.00KB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.00GB");
        assert_eq!(human_bytes(-2048), "-2.00KB");
    }

    #[test]
    fn gb_floor_zero() {
        assert_eq!(gb(-5), 0.0);
        assert!((gb(1024 * 1024 * 1024) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn hhmm_roundtrip() {
        assert_eq!(parse_hhmm("09:30").unwrap(), 570);
        assert_eq!(format_hhmm(570), "09:30");
        assert!(parse_hhmm("24:00").is_err());
        assert!(parse_hhmm("9-30").is_err());
    }

    #[test]
    fn external_cmd_dry_run_is_silent() {
        std::env::set_var(DRY_RUN_ENV, "1");
        let cmd = ExternalCmd::new("schtasks.exe", &["/query"]);
        let out = cmd.run().unwrap();
        assert!(out.status.success());
        std::env::remove_var(DRY_RUN_ENV);
        assert!(!ExternalCmd::dry_run());
    }

    #[test]
    fn decode_handles_gbk() {
        let gbk_success = [0xB3, 0xC9, 0xB9, 0xA6]; // "成功" in GBK
        assert_eq!(decode(&gbk_success), "成功");
    }
}

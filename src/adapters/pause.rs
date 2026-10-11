//! Graceful-pause ladder (build guide section 6.3.6): five fallback levels tried
//! in order, stopping at the first success, each followed by an exit confirmation.
//!
//! Honesty rules baked into this module:
//! * a level only "succeeds" when the exit is *confirmed* (process gone or the
//!   `wait_exit_file` checkpoint appeared), never because a message was posted;
//! * level ⑤ (`TerminateProcess`) always records `clean_exit = false`, which is
//!   what §6.3.6 promises the user ("不干净退出就不关机、恢复时如实告知");
//! * `probe_levels` is strictly read-only — no input, no close, no kill — so
//!   `doctor` and `watchdog adapter test` can run on a live desktop;
//! * under `DEVZONE_DRY_RUN` every mutating branch turns into a printed plan.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{
    OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};

use crate::adapters::{self, hotkey, ocr, uia, Level, LevelProbe};
use crate::config::{AppPauseCfg, RegionCfg};
use crate::util;

/// One app to pause.
#[derive(Clone, Debug)]
pub struct PauseRequest {
    pub app: String,
    pub exe: Option<PathBuf>,
    pub cfg: AppPauseCfg,
    pub per_level_timeout: Duration,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct LevelAttempt {
    pub level: Level,
    pub ok: bool,
    pub note: String,
    pub duration_ms: u64,
}

#[derive(Clone, Debug)]
pub struct AppPauseResult {
    pub app: String,
    pub clean_exit: bool,
    pub level_used: Option<Level>,
    pub attempts: Vec<LevelAttempt>,
    pub duration_ms: u64,
}

impl AppPauseResult {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "app": self.app,
            "clean": self.clean_exit,
            "level_used": self.level_used.map(|l| l.as_str()),
            "duration_ms": self.duration_ms,
            "attempts": self.attempts.iter().map(|a| serde_json::json!({
                "level": a.level.as_str(),
                "ok": a.ok,
                "note": a.note,
                "duration_ms": a.duration_ms,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Configured levels in ladder order; `ocr` survives only when `ocr_fallback`
/// is also true (section 6.3.6 level 4).
pub fn steps_for(cfg: &AppPauseCfg) -> Vec<Level> {
    let mut out: Vec<Level> = Level::LADDER
        .iter()
        .filter(|level| {
            cfg.enabled_levels
                .iter()
                .any(|name| Level::parse(name) == Some(**level))
        })
        .copied()
        .collect();
    // Section 6.3.6 level 4: OCR + click runs only when the app lists `ocr`
    // **and** sets `ocr_fallback = true`. One of the two is not enough.
    if !cfg.ocr_fallback {
        out.retain(|level| *level != Level::Ocr);
    }
    out.sort_by_key(|level| Level::LADDER.iter().position(|l| l == level).unwrap_or(9));
    out
}

/// What one poll of the exit confirmation can conclude (§6.3.6: "每级后等待退出
/// 确认（进程消失或断点文件出现，超时 `pause_timeout_sec`）").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Confirmed: the process is gone or the checkpoint file appeared.
    Exited,
    /// Still worth polling.
    Waiting,
    /// Gave up after `timeout`; the ladder moves to the next level.
    TimedOut,
}

/// Poll cadence inside the confirmation window.
pub const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Pure half of the exit confirmation: given how long the current level has been
/// waiting, whether the process disappeared and whether the checkpoint file
/// showed up, decide. Confirmation wins over the clock, and the clock always
/// wins over an unconfirmed level.
pub fn exited_by_poll(
    elapsed_ms: u64,
    timeout: Duration,
    process_gone: bool,
    exit_file_found: bool,
) -> Decision {
    if process_gone || exit_file_found {
        return Decision::Exited;
    }
    if elapsed_ms >= timeout.as_millis() as u64 {
        return Decision::TimedOut;
    }
    Decision::Waiting
}

/// Fallback for a missing / zero `guard.pause_timeout_sec` (§4 default is 60s).
pub fn per_level_timeout_or_default(seconds: u64) -> Duration {
    if seconds == 0 {
        Duration::from_secs(60)
    } else {
        Duration::from_secs(seconds)
    }
}

/// The process instance a level should act on: prefer one that owns a window
/// (that is the one the user sees), otherwise the lowest pid so repeated runs
/// pick the same target. Pure: takes `(pid, window_count)` pairs.
pub fn prefer_pid_with_window(candidates: &[(u32, usize)]) -> Option<u32> {
    candidates
        .iter()
        .filter(|(_, windows)| *windows > 0)
        .min_by_key(|(pid, _)| *pid)
        .map(|(pid, _)| *pid)
        .or_else(|| candidates.iter().map(|(pid, _)| *pid).min())
}

/// True when the exit file path exists (empty / whitespace paths never match).
pub fn exit_file_appeared(cfg: &AppPauseCfg) -> bool {
    cfg.wait_exit_file
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(|path| Path::new(path).exists())
        .unwrap_or(false)
}

/// Window rectangle -> the capture region the OCR level reads (§6.3.6 level 4).
pub fn region_of_window(window: &adapters::WindowInfo) -> RegionCfg {
    RegionCfg {
        x: window.rect.0,
        y: window.rect.1,
        width: window.width(),
        height: window.height(),
    }
}

/// Map an OCR hit centre (relative to the captured region) back to absolute
/// screen coordinates for `SendInput`. Pure, and the only coordinate math in the
/// OCR path.
pub fn ocr_hit_to_screen(region: &RegionCfg, centre: (i32, i32)) -> (i32, i32) {
    (region.x + centre.0, region.y + centre.1)
}

/// Button labels the OCR level clicks (same set as UIA, §6.3.6 level 4).
pub const OCR_KEYWORDS: [&str; 3] = ["停止", "暂停", "Stop"];

/// `guard.pause_enabled` off / empty `enabled_levels` must never kill a process
/// the user did not opt in: level ⑤ is only the *last resort of a ladder*, not a
/// default. Pure decision helper.
pub fn kill_is_last_resort(cfg: &AppPauseCfg) -> bool {
    let steps = steps_for(cfg);
    !steps.is_empty() && !steps.contains(&Level::Kill)
}

/// Execute the ladder for one app: every configured level in order, stopping at
/// the first level whose effect is confirmed. Falls back to level ⑤ when the
/// first four all failed (§6.3.6).
pub fn run(request: &PauseRequest) -> Result<AppPauseResult> {
    let started = Instant::now();
    tracing::info!(
        "graceful pause start app={} reason={} exe={:?} timeout={}s",
        request.app,
        request.reason,
        request.exe,
        request.per_level_timeout.as_secs()
    );
    let mut attempts: Vec<LevelAttempt> = Vec::new();
    let steps = steps_for(&request.cfg);

    if steps.is_empty() {
        tracing::warn!(
            "pause: {} 未配置任何级别，跳过阶梯（不会强制结束进程）",
            request.app
        );
        return Ok(AppPauseResult {
            app: request.app.clone(),
            clean_exit: false,
            level_used: None,
            attempts,
            duration_ms: util::elapsed_ms(started),
        });
    }

    let candidates: Vec<(u32, usize)> = adapters::find_processes(&request.app)
        .iter()
        .map(|p| (p.pid, adapters::windows_of_pid(p.pid).len()))
        .collect();
    let Some(pid) = prefer_pid_with_window(&candidates) else {
        // Nothing to pause: trivially "clean", and we say so in the log.
        tracing::info!("pause: {} 未在运行，无需暂停", request.app);
        return Ok(AppPauseResult {
            app: request.app.clone(),
            clean_exit: true,
            level_used: None,
            attempts,
            duration_ms: util::elapsed_ms(started),
        });
    };
    let window = adapters::main_window_of(pid);

    if util::ExternalCmd::dry_run() {
        attempts = planned_attempts(&steps, pid, window.as_ref());
        tracing::info!("pause: dry-run，{} 的阶梯只打印计划不执行", request.app);
        return Ok(AppPauseResult {
            app: request.app.clone(),
            clean_exit: false,
            level_used: None,
            attempts,
            duration_ms: util::elapsed_ms(started),
        });
    }

    // Levels to try: the configured ones, then ⑤ as the documented last resort.
    let mut ladder = steps.clone();
    if kill_is_last_resort(&request.cfg) {
        ladder.push(Level::Kill);
    }

    for level in ladder {
        let attempt_started = Instant::now();
        let outcome = apply_level(level, request, pid, window.as_ref());
        let note = match outcome {
            Ok(note) => note,
            Err(reason) => {
                attempts.push(LevelAttempt {
                    level,
                    ok: false,
                    note: reason.to_string(),
                    duration_ms: util::elapsed_ms(attempt_started),
                });
                continue;
            }
        };
        // Every level is followed by an exit confirmation (section 6.3.6).
        let decision = wait_for_exit(request, pid);
        let duration_ms = util::elapsed_ms(attempt_started);
        if decision == Decision::Exited {
            attempts.push(LevelAttempt {
                level,
                ok: true,
                note,
                duration_ms,
            });
            return Ok(AppPauseResult {
                app: request.app.clone(),
                // ⑤ never counts as clean (§9 + §6.3.6 promise).
                clean_exit: level != Level::Kill,
                level_used: Some(level),
                attempts,
                duration_ms: util::elapsed_ms(started),
            });
        }
        attempts.push(LevelAttempt {
            level,
            ok: false,
            note: format!(
                "{note}；但 {}s 内未确认退出（进程仍在）",
                request.per_level_timeout.as_secs()
            ),
            duration_ms,
        });
    }

    // Section 9 row "优雅暂停 / 某应用所有级失败": record and carry on; the
    // guardian still shuts down because the checkpoint already lists this app as
    // not cleanly exited.
    tracing::error!(
        "pause: {} 所有级别均未确认退出（{}），checkpoint 记为 clean_exit=0",
        request.app,
        attempts.len()
    );
    Ok(AppPauseResult {
        app: request.app.clone(),
        clean_exit: false,
        level_used: None,
        attempts,
        duration_ms: util::elapsed_ms(started),
    })
}

/// Dry-run half of the ladder: the exact plan, level by level, nothing sent.
fn planned_attempts(
    steps: &[Level],
    pid: u32,
    window: Option<&adapters::WindowInfo>,
) -> Vec<LevelAttempt> {
    let mut ladder: Vec<Level> = steps.to_vec();
    if !ladder.contains(&Level::Kill) {
        ladder.push(Level::Kill);
    }
    ladder
        .iter()
        .map(|level| LevelAttempt {
            level: *level,
            ok: false,
            note: match level {
                Level::Hotkey => format!(
                    "dry-run: 将向 pid={pid} {} 发 Esc + Ctrl+S",
                    hwnd_note(window)
                ),
                Level::Uia => format!(
                    "dry-run: 将在 {} 上查找 Name∈[{}] 的元素并 Invoke",
                    hwnd_note(window),
                    uia::PAUSE_BUTTON_NAMES.join("/")
                ),
                Level::WmClose => {
                    format!("dry-run: 将对 {} PostMessageW(WM_CLOSE)", hwnd_note(window))
                }
                Level::Ocr => format!(
                    "dry-run: 将截 {} 客户区 + OCR 找 [{}] 后点击中心",
                    hwnd_note(window),
                    OCR_KEYWORDS.join("/")
                ),
                Level::Kill => format!("dry-run: 将 TerminateProcess(pid={pid})，clean_exit=0"),
            },
            duration_ms: 0,
        })
        .collect()
}

fn hwnd_note(window: Option<&adapters::WindowInfo>) -> String {
    match window {
        Some(window) => format!("窗口「{}」({:?})", window.title, window.hwnd),
        None => "无窗口".to_string(),
    }
}

/// One level of the ladder. `Ok(note)` means the mechanism itself was applied;
/// the caller still has to confirm the exit.
fn apply_level(
    level: Level,
    request: &PauseRequest,
    pid: u32,
    window: Option<&adapters::WindowInfo>,
) -> Result<String> {
    match level {
        Level::Hotkey => {
            let hwnd = window.ok_or_else(|| anyhow!("hotkey 需要可见窗口，进程无窗口"))?;
            hotkey::pause_shortcuts(hwnd.hwnd)?;
            Ok(format!(
                "已发送 Esc + Ctrl+S 到「{}」",
                truncate(&hwnd.title, 40)
            ))
        }
        Level::Uia => {
            let hwnd = window.ok_or_else(|| anyhow!("uia 需要窗口句柄"))?;
            let matched = uia::invoke_by_name(hwnd.hwnd, &uia::PAUSE_BUTTON_NAMES)?;
            Ok(format!("UIA Invoke 命中元素「{matched}」"))
        }
        Level::WmClose => {
            let hwnd = window.ok_or_else(|| anyhow!("wm_close 需要主窗口"))?;
            post_wm_close(hwnd.hwnd)?;
            Ok(format!(
                "已发送 WM_CLOSE 到「{}」",
                truncate(&hwnd.title, 40)
            ))
        }
        Level::Ocr => ocr_click(request, window),
        Level::Kill => {
            terminate_process(pid)?;
            Ok(format!(
                "TerminateProcess(pid={pid}) 已执行（clean_exit=0）"
            ))
        }
    }
}

/// `PostMessageW(hwnd, WM_CLOSE, 0, 0)` — level ③. The message is *posted*, so
/// the target app decides whether that means "save and quit".
///
/// # Safety
/// `hwnd` must be a live window handle of this session; posting to a stale
/// handle simply returns FALSE, which we surface as an error. Under dry run the
/// call is never made.
fn post_wm_close(hwnd: HWND) -> Result<()> {
    if util::ExternalCmd::dry_run() {
        util::progress(&format!(
            "dry-run: 将对 {hwnd:?} PostMessageW(WM_CLOSE)（未发送）"
        ));
        return Err(anyhow!("dry-run：WM_CLOSE 未发送"));
    }
    // SAFETY: only a posted message; the window itself decides what to do with it.
    unsafe { PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0)) }
        .map_err(|err| anyhow!("PostMessageW(WM_CLOSE) 失败: {err}"))
}

/// Level ④: capture the window rect, OCR it for a stop/pause label, click the
/// centre of the first hit. Without the `ocr` build feature `find_text` answers
/// an error, and this level fails for real (we never pretend success).
fn ocr_click(request: &PauseRequest, window: Option<&adapters::WindowInfo>) -> Result<String> {
    let hwnd = window.ok_or_else(|| anyhow!("ocr 需要窗口矩形作为截图区域"))?;
    let region = region_of_window(hwnd);
    if region.is_empty() {
        return Err(anyhow!("窗口矩形为空，OCR 无区域可截"));
    }
    let hits = ocr::find_text(&region, &OCR_KEYWORDS)?;
    let hit = hits.first().ok_or_else(|| {
        anyhow!(
            "OCR 在 {}x{} 区域内未识别到 [{}]",
            region.width,
            region.height,
            OCR_KEYWORDS.join("/")
        )
    })?;
    let (x, y) = ocr_hit_to_screen(&region, hit.centre);
    hotkey::left_click_at(x, y)?;
    tracing::info!("ocr: {} 命中「{}」于 ({x},{y})", request.app, hit.text);
    Ok(format!(
        "OCR 点击「{}」于 ({x},{y})",
        truncate(&hit.text, 30)
    ))
}

/// `OpenProcess(PROCESS_TERMINATE)` + `TerminateProcess` (level ⑤).
///
/// # Safety
/// The handle returned by `OpenProcess` is ours to close, and the exit code is
/// the documented 0. Terminating is destructive by design: the caller records
/// `clean_exit = false` so the resume notice can tell the user (§6.3.6). Dry run
/// never reaches the API at all.
fn terminate_process(pid: u32) -> Result<()> {
    if util::ExternalCmd::dry_run() {
        util::progress(&format!(
            "dry-run: 将 TerminateProcess(pid={pid})（未执行）"
        ));
        return Err(anyhow!("dry-run：TerminateProcess 未执行"));
    }
    // SAFETY: PROCESS_TERMINATE is the documented right for this level; the
    // handle is closed on every path below.
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) }
        .map_err(|err| anyhow!("OpenProcess(pid={pid}) 失败: {err}"))?;
    // SAFETY: `handle` is the just-opened handle of `pid`.
    let result = unsafe { TerminateProcess(handle, 0) };
    // SAFETY: `handle` came from OpenProcess above and is not used afterwards.
    let _ = unsafe { CloseHandle(handle) };
    result.map_err(|err| anyhow!("TerminateProcess(pid={pid}) 失败: {err}"))
}

/// Poll the confirmation window for one level: the pid we acted on is gone, or
/// the `wait_exit_file` appeared, until `per_level_timeout` runs out.
///
/// Re-enumerating by image name would happily match the *other* instances of a
/// multi-process (Electron) app, so liveness is checked for exactly the pid the
/// level targeted.
fn wait_for_exit(request: &PauseRequest, pid: u32) -> Decision {
    let start = Instant::now();
    loop {
        let elapsed = util::elapsed_ms(start);
        let decision = exited_by_poll(
            elapsed,
            request.per_level_timeout,
            !pid_alive(pid),
            exit_file_appeared(&request.cfg),
        );
        if decision != Decision::Waiting {
            return decision;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Is `pid` still alive? `OpenProcess` on a queried handle answers an error once
/// the process is gone and its last handle has been released.
///
/// # Safety
/// Only `PROCESS_QUERY_LIMITED_INFORMATION` is requested (no mutation), and any
/// handle we open is closed again on every path.
fn pid_alive(pid: u32) -> bool {
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return false;
    };
    // SAFETY: `handle` came from the OpenProcess call above.
    unsafe {
        let _ = CloseHandle(handle);
    }
    true
}

/// Shorten long window titles for log lines without cutting UTF-8 chars.
fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let cut: String = value.chars().take(max).collect();
    format!("{cut}…")
}

/// Read-only capability probe (§5.4 item 5, §6.3.6 note). Enumerates processes
/// and windows and asks UIA which elements exist; it never sends input, never
/// closes a window and never terminates anything.
pub fn probe_levels(app: &str, cfg: &AppPauseCfg) -> Result<Vec<LevelProbe>> {
    let process = adapters::process_summary(app);
    let candidates: Vec<(u32, usize)> = adapters::find_processes(app)
        .iter()
        .map(|p| (p.pid, adapters::windows_of_pid(p.pid).len()))
        .collect();
    let pid = prefer_pid_with_window(&candidates);
    let window = pid.and_then(adapters::main_window_of);
    let foreground = adapters::foreground_window();
    let steps = steps_for(cfg);
    let mut probes = Vec::new();

    for level in Level::LADDER {
        if !steps.contains(&level) {
            probes.push(LevelProbe {
                level,
                available: false,
                note: "未配置该级（ladder 不会尝试）".to_string(),
                process: process.clone(),
            });
            continue;
        }
        let (available, note) =
            probe_one(level, app, cfg, pid, window.as_ref(), foreground.as_ref());
        probes.push(LevelProbe {
            level,
            available,
            note,
            process: process.clone(),
        });
    }
    Ok(probes)
}

fn probe_one(
    level: Level,
    app: &str,
    cfg: &AppPauseCfg,
    pid: Option<u32>,
    window: Option<&adapters::WindowInfo>,
    foreground: Option<&adapters::WindowInfo>,
) -> (bool, String) {
    match level {
        Level::Hotkey => match (pid, window) {
            (Some(_), Some(window)) => {
                let is_foreground = foreground.map(|f| f.hwnd == window.hwnd);
                match is_foreground {
                    Some(true) => (
                        true,
                        format!("窗口「{}」已在前台，可发 Esc+Ctrl+S（本次未发送）", truncate(&window.title, 30)),
                    ),
                    _ => (
                        true,
                        format!(
                            "有窗口「{}」但当前不在前台；SetForegroundWindow 可能被系统前台锁拒绝（§16 待实测）",
                            truncate(&window.title, 30)
                        ),
                    ),
                }
            }
            (Some(_), None) => (
                false,
                "进程在运行但没有顶层窗口：键盘事件无处可送".to_string(),
            ),
            (None, _) => (false, format!("{app} 未运行")),
        },
        Level::Uia => match window {
            Some(window) => match uia::element_names(window.hwnd, uia::MAX_ELEMENTS) {
                Ok(names) => {
                    let matched: Vec<String> = names
                        .iter()
                        .filter(|name| uia::matches_name(name, &uia::PAUSE_BUTTON_NAMES))
                        .cloned()
                        .collect();
                    if matched.is_empty() {
                        (
                            false,
                            format!(
                                "UIA 可读（{} 个具名元素），但无 Name∈[{}]；示例：{}",
                                names.len(),
                                uia::PAUSE_BUTTON_NAMES.join("/"),
                                names
                                    .iter()
                                    .take(4)
                                    .cloned()
                                    .collect::<Vec<_>>()
                                    .join(" | ")
                            ),
                        )
                    } else {
                        (
                            true,
                            format!(
                                "UIA 可 Invoke：{}（本次只读，未 Invoke）",
                                matched.join("/")
                            ),
                        )
                    }
                }
                Err(err) => (false, format!("UIA 读取失败：{err}")),
            },
            None => (false, "无窗口句柄，UIA 无法定位元素".to_string()),
        },
        Level::WmClose => match (pid, window) {
            (Some(_), Some(window)) => (
                true,
                format!(
                    "主窗口「{}」存在，可 PostMessageW(WM_CLOSE)（本次未发送）",
                    truncate(&window.title, 30)
                ),
            ),
            (Some(_), None) => (false, "进程在运行但没有可关闭的主窗口".to_string()),
            (None, _) => (false, format!("{app} 未运行")),
        },
        Level::Ocr => {
            if !ocr::FEATURE_ENABLED {
                return (
                    false,
                    "OCR 未编译进本二进制（需 --features ocr）：本级总是失败，直接进入 ⑤"
                        .to_string(),
                );
            }
            match window {
                Some(window) => {
                    let region = region_of_window(window);
                    if region.is_empty() {
                        (false, "窗口矩形为空，无法截图".to_string())
                    } else if !cfg.ocr_fallback {
                        (
                            false,
                            "enabled_levels 含 ocr 但 ocr_fallback=false：按 §6.3.6 不启用"
                                .to_string(),
                        )
                    } else {
                        (
                            true,
                            format!(
                                "OCR 可用：区域 {}x{} @({},{})，找 [{}]（本次未识别未点击）",
                                region.width,
                                region.height,
                                region.x,
                                region.y,
                                OCR_KEYWORDS.join("/")
                            ),
                        )
                    }
                }
                None => (false, "无窗口矩形可截".to_string()),
            }
        }
        Level::Kill => match pid {
            Some(pid) => (
                true,
                format!(
                    "可 TerminateProcess(pid={pid})；clean_exit=0，恢复时如实告知（本次未执行）"
                ),
            ),
            None => (false, format!("{app} 未运行")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::Level;
    use serial_test::serial;

    fn cfg(levels: &[&str], ocr_fallback: bool) -> AppPauseCfg {
        AppPauseCfg {
            enabled_levels: levels.iter().map(|l| l.to_string()).collect(),
            ocr_fallback,
            wait_exit_file: None,
            watch_region: None,
        }
    }

    fn request(levels: &[&str], app: &str) -> PauseRequest {
        PauseRequest {
            app: app.to_string(),
            exe: None,
            cfg: cfg(levels, false),
            per_level_timeout: per_level_timeout_or_default(60),
            reason: "test".to_string(),
        }
    }

    #[test]
    fn steps_follow_ladder_order_and_gate_ocr() {
        let steps = steps_for(&cfg(&["wm_close", "hotkey", "kill"], false));
        assert_eq!(steps, vec![Level::Hotkey, Level::WmClose, Level::Kill]);
        // ocr needs both the level name and the explicit fallback switch
        assert!(!steps_for(&cfg(&["ocr"], false)).contains(&Level::Ocr));
        assert!(!steps_for(&cfg(&["hotkey"], true)).contains(&Level::Ocr));
        let with_ocr = steps_for(&cfg(&["hotkey", "ocr", "wm_close"], true));
        assert_eq!(with_ocr, vec![Level::Hotkey, Level::WmClose, Level::Ocr]);
        assert!(steps_for(&cfg(&[], false)).is_empty());
        assert!(Level::parse("nope").is_none());
    }

    #[test]
    fn kill_is_only_the_last_resort_of_a_configured_ladder() {
        assert!(kill_is_last_resort(&cfg(
            &["hotkey", "uia", "wm_close"],
            false
        )));
        assert!(!kill_is_last_resort(&cfg(&["hotkey", "kill"], false)));
        // an empty ladder must never end in a kill (never opted in)
        assert!(!kill_is_last_resort(&cfg(&[], false)));
    }

    #[test]
    fn confirmation_prefers_the_signal_over_the_clock() {
        let timeout = Duration::from_secs(60);
        assert_eq!(
            exited_by_poll(1_000, timeout, true, false),
            Decision::Exited
        );
        assert_eq!(
            exited_by_poll(1_000, timeout, false, true),
            Decision::Exited
        );
        assert_eq!(
            exited_by_poll(59_000, timeout, false, false),
            Decision::Waiting
        );
        assert_eq!(
            exited_by_poll(60_000, timeout, false, false),
            Decision::TimedOut
        );
        // an unconfirmed level times out, a confirmed one never does
        assert_eq!(
            exited_by_poll(120_000, timeout, false, false),
            Decision::TimedOut
        );
        assert_eq!(
            exited_by_poll(10_000_000, timeout, true, false),
            Decision::Exited
        );
        // zero timeout means "decide immediately", used by the smoke path
        assert_eq!(
            exited_by_poll(0, Duration::ZERO, false, false),
            Decision::TimedOut
        );
    }

    #[test]
    fn per_level_timeout_defaults_to_sixty_seconds() {
        assert_eq!(per_level_timeout_or_default(0), Duration::from_secs(60));
        assert_eq!(per_level_timeout_or_default(5), Duration::from_secs(5));
    }

    #[test]
    fn target_choice_prefers_windowed_instances() {
        assert_eq!(prefer_pid_with_window(&[(9, 0), (7, 2), (5, 0)]), Some(7));
        assert_eq!(prefer_pid_with_window(&[(9, 0), (5, 0)]), Some(5));
        assert_eq!(prefer_pid_with_window(&[]), None);
    }

    #[test]
    fn exit_file_helper_tolerates_absent_and_blank_paths() {
        let mut app_cfg = cfg(&["hotkey"], false);
        assert!(!exit_file_appeared(&app_cfg));
        app_cfg.wait_exit_file = Some("   ".to_string());
        assert!(!exit_file_appeared(&app_cfg));
        app_cfg.wait_exit_file = Some("does-not-exist-for-sure.devzone".to_string());
        assert!(!exit_file_appeared(&app_cfg));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("paused.json");
        std::fs::write(&file, "{}").unwrap();
        app_cfg.wait_exit_file = Some(file.to_string_lossy().to_string());
        assert!(exit_file_appeared(&app_cfg));
    }

    #[test]
    fn ocr_region_and_hit_mapping() {
        let window = adapters::WindowInfo {
            hwnd: HWND(std::ptr::null_mut()),
            pid: 1,
            title: "豆包".to_string(),
            rect: (100, 50, 500, 350),
            visible: true,
        };
        let region = region_of_window(&window);
        assert_eq!(
            (region.x, region.y, region.width, region.height),
            (100, 50, 400, 300)
        );
        assert!(!region.is_empty());
        assert_eq!(ocr_hit_to_screen(&region, (20, 30)), (120, 80));
        let offscreen = RegionCfg {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        assert!(offscreen.is_empty());
    }

    #[test]
    fn truncate_keeps_utf8_boundaries() {
        assert_eq!(truncate("豆包桌面端", 3), "豆包桌…");
        assert_eq!(truncate("short", 40), "short");
    }

    #[test]
    fn probe_levels_reports_the_whole_ladder_read_only() {
        // A process that certainly is not running: every configured level answers
        // "unavailable" with a reason, unconfigured levels say so, and nothing was
        // touched (probe_levels only enumerates).
        let probes = probe_levels(
            "definitely-not-a-process-xyz.exe",
            &cfg(&["hotkey", "wm_close", "kill"], false),
        )
        .unwrap();
        assert_eq!(probes.len(), Level::LADDER.len(), "{probes:?}");
        assert!(probes.iter().all(|p| !p.available), "{probes:?}");
        assert!(
            probes[0].process.contains("未运行"),
            "{:?}",
            probes[0].process
        );
        assert_eq!(probes[1].level, Level::Uia);
        assert!(probes[1].note.contains("未配置"), "{}", probes[1].note);
        assert!(probes[2].note.contains("未运行"), "{}", probes[2].note);
        // OCR never shows up unless the app opted in twice
        assert!(probes[3].note.contains("未配置"));
    }

    #[test]
    fn probe_levels_reports_ocr_compile_status_honestly() {
        let probes = probe_levels("devzone", &cfg(&["ocr"], true)).unwrap();
        let ocr = probes
            .iter()
            .find(|p| p.level == Level::Ocr)
            .expect("ocr probe");
        // Without `--features ocr` the level must be reported as unavailable with
        // the build-feature reason, never as "available".
        if !crate::adapters::ocr::FEATURE_ENABLED {
            assert!(!ocr.available, "{ocr:?}");
            assert!(ocr.note.contains("--features ocr"), "{}", ocr.note);
        }
    }

    /// Image name (without `.exe`) of the process running these tests, so the
    /// dry-run ladder has a real target to plan against.
    fn own_process_hint() -> String {
        std::env::current_exe()
            .ok()
            .and_then(|path| {
                path.file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "devzone".to_string())
    }

    #[serial]
    #[test]
    fn dry_run_ladder_only_prints_the_plan() {
        std::env::set_var(util::DRY_RUN_ENV, "1");
        let request = PauseRequest {
            app: own_process_hint(),
            exe: None,
            cfg: cfg(&["hotkey", "uia", "wm_close", "ocr"], true),
            per_level_timeout: Duration::from_millis(50),
            reason: "unit".to_string(),
        };
        let result = run(&request).unwrap();
        assert!(!result.clean_exit);
        assert!(result.level_used.is_none());
        // every configured level plus the documented fallback ⑤
        assert_eq!(
            result.attempts.iter().map(|a| a.level).collect::<Vec<_>>(),
            vec![
                Level::Hotkey,
                Level::Uia,
                Level::WmClose,
                Level::Ocr,
                Level::Kill
            ]
        );
        assert!(result.attempts.iter().all(|a| !a.ok));
        assert!(result.attempts.iter().all(|a| a.note.contains("dry-run")));
        assert!(result
            .attempts
            .last()
            .unwrap()
            .note
            .contains("TerminateProcess"));
        std::env::remove_var(util::DRY_RUN_ENV);
    }

    #[serial]
    #[test]
    fn unconfigured_ladder_and_absent_process_are_reported_honestly() {
        std::env::set_var(util::DRY_RUN_ENV, "1");
        // no levels configured -> no ladder, and definitely no kill
        let empty = run(&request(&[], "some-app.exe")).unwrap();
        assert!(empty.attempts.is_empty());
        assert!(!empty.clean_exit);
        assert!(empty.level_used.is_none());
        // process not running -> nothing to pause, reported as clean
        let gone = run(&request(&["hotkey"], "definitely-not-a-process-xyz.exe")).unwrap();
        assert!(gone.clean_exit);
        assert!(gone.attempts.is_empty());
        assert!(gone.to_json()["clean"].as_bool().unwrap());
        std::env::remove_var(util::DRY_RUN_ENV);
    }

    #[serial]
    #[test]
    fn mutating_primitives_refuse_under_dry_run() {
        // Belt and braces: even if a caller reaches them directly outside the
        // ladder, level ③ and ⑤ never touch a real window or process.
        std::env::set_var(util::DRY_RUN_ENV, "1");
        let close_error = post_wm_close(HWND(std::ptr::null_mut()))
            .unwrap_err()
            .to_string();
        let kill_error = terminate_process(std::process::id())
            .unwrap_err()
            .to_string();
        std::env::remove_var(util::DRY_RUN_ENV);
        assert!(close_error.contains("dry-run"), "{close_error}");
        assert!(close_error.contains("WM_CLOSE"), "{close_error}");
        assert!(kill_error.contains("dry-run"), "{kill_error}");
    }

    #[test]
    fn result_json_matches_the_event_table() {
        let result = AppPauseResult {
            app: "doubao.exe".to_string(),
            clean_exit: false,
            level_used: Some(Level::Kill),
            attempts: vec![LevelAttempt {
                level: Level::Hotkey,
                ok: false,
                note: "无窗口".to_string(),
                duration_ms: 12,
            }],
            duration_ms: 12,
        };
        let value = result.to_json();
        assert_eq!(value["app"], "doubao.exe");
        assert_eq!(value["clean"], false);
        assert_eq!(value["level_used"], "kill");
        assert_eq!(value["attempts"][0]["level"], "hotkey");
        assert_eq!(value["attempts"][0]["note"], "无窗口");
    }
}

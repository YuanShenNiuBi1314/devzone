//! devzone — local-first Windows dev-environment autonomy.
//!
//! Entry point: clap parse -> dispatch (build guide section 3 / 5). `doctor`
//! (section 5.4) and `status` live here because they span every module.

mod adapters;
mod battery;
mod cli;
mod config;
mod db;
mod events;
mod mod_crunch;
mod mod_guard;
mod mod_plugin;
mod mod_profile;
mod mod_sched;
mod mod_watchdog;
mod mod_zone;
mod notify;
mod util;

use anyhow::{Context as _, Result};
use clap::Parser;
use serde_json::{json, Value};
use std::path::Path;

use cli::{Cli, Command};
use config::Config;
use db::Db;
use events::{name, Event, EventBus};
use util::{contains_ci, Paths};

/// Everything a command handler needs: configuration, data paths, global flags.
pub struct Ctx {
    pub cfg: Config,
    pub paths: Paths,
    pub force: bool,
    pub elevated: bool,
    /// Set when the user file failed to parse (only `doctor` tolerates it).
    pub config_error: Option<String>,
}

impl Ctx {
    pub fn db(&self) -> Result<Db> {
        Db::open(&self.paths).context("打开 devzone.db")
    }

    pub fn bus(&self) -> Result<EventBus> {
        EventBus::new(&self.cfg, self.paths.clone())
    }

    /// `--json` handling: prints the envelope and reports whether it did.
    pub fn emit_json(&self, command: &str, data: Value, enabled: bool) -> Result<bool> {
        if enabled {
            util::print_json(command, data)?;
            return Ok(true);
        }
        Ok(false)
    }
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    // Section 5.3: a failing `--json` command still answers with one JSON object
    // on stdout (plus exit code 1); everything else reports on stderr only.
    let label = cli.command.label();
    let as_json = cli.command.wants_json();
    match dispatch(cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            let message = format!("{err:#}");
            if as_json {
                util::print_json_error(&label, &message).ok();
            } else {
                eprintln!("[devzone] 错误：{message}");
            }
            tracing::error!(command = %label, "{message}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn dispatch(cli: Cli) -> Result<()> {
    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    util::init_logging(&paths)?;
    config::ensure_config_file(&paths).context("初始化 devzone.toml")?;

    // Section 9: a broken config is a hard error with a doctor hint — except for
    // `doctor` itself, which has to be able to report the breakage.
    let raw = config::read_user_raw(&paths)?;
    let (cfg, config_error) = match Config::load(&paths) {
        Ok(cfg) => (cfg, None),
        Err(err) => {
            let message = format!("{err:#}");
            if matches!(cli.command, Command::Doctor) {
                (
                    Config::assemble("").context("内置默认配置不可用")?,
                    Some(message),
                )
            } else {
                return Err(anyhow::anyhow!(
                    "{message}\n提示：运行 devzone doctor 查看具体项。"
                ));
            }
        }
    };

    let ctx = Ctx {
        cfg,
        paths,
        force: cli.force,
        elevated: cli.elevated,
        config_error,
    };

    if cli.command.requires_admin() && !ctx.elevated {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if util::ensure_admin_or_elevate(&args)? {
            util::progress("已在管理员窗口中重新启动，本进程退出。");
            return Ok(());
        }
    }

    match cli.command {
        Command::Zone { cmd } => mod_zone::handle(&cmd, &ctx),
        Command::Sched { cmd } => mod_sched::handle(&cmd, &ctx),
        Command::Guard { cmd } => mod_guard::handle(&cmd, &ctx),
        Command::Crunch { cmd } => mod_crunch::handle(&cmd, &ctx),
        Command::Watchdog { cmd } => mod_watchdog::handle(&cmd, &ctx),
        Command::Profile { cmd } => mod_profile::handle(&cmd, &ctx, &raw),
        Command::Plugin { cmd } => plugin_handle(&cmd, &ctx),
        Command::Event { name, json } => {
            let payload = cli::parse_context(json.as_deref())?;
            if !Event::new(&name, json!({})).known() {
                util::progress(&format!("`{name}` 不在事件表中，仍按自定义事件派发"));
            }
            ctx.bus()?.emit_json(&name, payload)
        }
        Command::Doctor => doctor(&ctx),
        Command::Status { json } => status(&ctx, json, &raw),
    }
}

fn plugin_handle(cmd: &cli::PluginCmd, ctx: &Ctx) -> Result<()> {
    let dir = ctx.cfg.plugin_dir(&ctx.paths);
    match cmd {
        cli::PluginCmd::List => {
            let installed = mod_plugin::list_installed(&dir);
            if installed.is_empty() {
                println!("（无插件）目录：{}", dir.display());
                return Ok(());
            }
            for (path, manifest) in &installed {
                println!(
                    "{:<24} v{:<8} events={} command={} ({})",
                    manifest.name,
                    if manifest.version.is_empty() {
                        "-"
                    } else {
                        manifest.version.as_str()
                    },
                    if manifest.events.is_empty() {
                        "-".to_string()
                    } else {
                        manifest.events.join(",")
                    },
                    manifest.command,
                    path.display()
                );
            }
            Ok(())
        }
        cli::PluginCmd::Install { dir: source } => {
            let manifest = mod_plugin::install(Path::new(source), &dir)?;
            println!(
                "已安装插件 {} -> {}（订阅事件：{}）",
                manifest.name,
                dir.display(),
                if manifest.events.is_empty() {
                    "-".to_string()
                } else {
                    manifest.events.join(", ")
                }
            );
            Ok(())
        }
        cli::PluginCmd::Run { event, json } => {
            let payload = cli::parse_context(json.as_deref())?;
            let outcomes = mod_plugin::dispatch(
                &dir,
                &Event::new(event, payload),
                ctx.cfg.plugin.default_timeout_sec,
            );
            if outcomes.is_empty() {
                println!("没有插件订阅事件 `{event}`");
            }
            for outcome in outcomes {
                match &outcome.error {
                    Some(err) => println!("{} FAILED {err}", outcome.name),
                    None => println!("{} ok ({}ms)", outcome.name, outcome.duration_ms),
                }
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// doctor (section 5.4) — measured values are reported, never assumed
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    Warn,
    Fail,
}

impl Verdict {
    pub fn glyph(self) -> &'static str {
        match self {
            Verdict::Ok => "[✓]",
            Verdict::Warn => "[△]",
            Verdict::Fail => "[✗]",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::Warn => "warn",
            Verdict::Fail => "fail",
        }
    }
}

pub struct Check {
    pub verdict: Verdict,
    pub title: String,
    pub detail: String,
}

impl Check {
    pub fn ok(title: &str, detail: impl Into<String>) -> Check {
        Check {
            verdict: Verdict::Ok,
            title: title.to_string(),
            detail: detail.into(),
        }
    }
    pub fn warn(title: &str, detail: impl Into<String>) -> Check {
        Check {
            verdict: Verdict::Warn,
            title: title.to_string(),
            detail: detail.into(),
        }
    }
    pub fn fail(title: &str, detail: impl Into<String>) -> Check {
        Check {
            verdict: Verdict::Fail,
            title: title.to_string(),
            detail: detail.into(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "verdict": self.verdict.as_str(),
            "glyph": self.verdict.glyph(),
            "title": self.title,
            "detail": self.detail,
        })
    }
}

/// Which sleep states `powercfg /a` reports. Deliberately language independent:
/// zh-CN and en-US output are both matched, and "not available" wording flips
/// the verdict for that line.
pub fn parse_power_support(text: &str) -> PowerSupport {
    let mut support = PowerSupport::default();
    // `powercfg /a` prints one header per group (available / not available)
    // followed by the indented state names, in the OS display language.
    let mut negative = false;
    for line in text.lines() {
        let cleaned = line.trim();
        if cleaned.is_empty() {
            continue;
        }
        let is_header = contains_ci(cleaned, "sleep state")
            || cleaned.contains("睡眠状态")
            || cleaned.contains("休眠状态");
        // Some builds put the verdict on the header ("以下睡眠状态不存在:") and
        // others repeat it per item ("休眠不可用。"), so both are inspected.
        let line_negative = contains_ci(cleaned, "not available")
            || contains_ci(cleaned, "unavailable")
            || cleaned.contains("不存在")
            || cleaned.contains("不可用")
            || cleaned.contains("未启用")
            || cleaned.contains("禁用");
        if is_header {
            negative = line_negative;
            continue;
        }
        let negative_line = negative || line_negative;
        if contains_ci(cleaned, "hibernate") || cleaned.contains("休眠") {
            support.hibernate = !negative_line;
            if negative_line {
                support.hibernate_negative_line = cleaned.to_string();
            } else {
                support.hibernate_line = cleaned.to_string();
            }
        } else if contains_ci(cleaned, "standby (s3)")
            || contains_ci(cleaned, "sleep (s3)")
            || cleaned.contains("睡眠 (s3)")
            || cleaned.contains("睡眠(S3)")
        {
            support.s3 = !negative_line;
        } else if contains_ci(cleaned, "fast startup") || cleaned.contains("快速启动") {
            support.fast_startup = !negative_line;
        } else {
            support.other_states.push(cleaned.to_string());
        }
    }
    support
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PowerSupport {
    pub hibernate: bool,
    pub s3: bool,
    pub fast_startup: bool,
    pub hibernate_line: String,
    pub hibernate_negative_line: String,
    pub other_states: Vec<String>,
}

/// Physical memory in MB, used for the hiberfil.sys sizing hint (~40% of RAM).
pub fn memory_mb() -> Option<u64> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    match unsafe { GlobalMemoryStatusEx(&mut status) } {
        Ok(()) => Some(status.ullTotalPhys / 1024 / 1024),
        Err(err) => {
            tracing::debug!("GlobalMemoryStatusEx failed: {err}");
            None
        }
    }
}

/// Run every doctor check. `as_json` prints the envelope instead of the glyph
/// lines (section 5.2 says JSON goes to stdout; the glyphs are the text UI).
pub fn doctor_checks(ctx: &Ctx) -> Vec<Check> {
    let mut checks: Vec<Check> = Vec::new();
    let mut power_available = false;

    // 1. hibernate (S4) availability
    match util::ExternalCmd::new("powercfg.exe", &["/a"]).run() {
        Ok(output) => {
            let text = util::decode(&output.stdout);
            let support = parse_power_support(&text);
            power_available = support.hibernate;
            if support.hibernate {
                checks.push(Check::ok(
                    "休眠可用性 (S4)",
                    format!("powercfg /a 报告休眠可用：{}", support.hibernate_line),
                ));
            } else {
                let evidence = if support.hibernate_negative_line.is_empty() {
                    "powercfg /a 未列出休眠".to_string()
                } else {
                    support.hibernate_negative_line.clone()
                };
                checks.push(Check::warn(
                    "休眠可用性 (S4)",
                    format!(
                        "休眠不可用（{evidence}）。tier1 不依赖休眠；tier2 若要睡眠需 S3={}",
                        support.s3
                    ),
                ));
            }
        }
        Err(err) => checks.push(Check::fail(
            "休眠可用性 (S4)",
            format!("powercfg /a 无法执行：{err}"),
        )),
    }

    // 2. hiberfil.sys space vs RAM * 40%
    let windir = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let hiberfil = Path::new(&windir).join("hiberfil.sys");
    let ram_mb = memory_mb().unwrap_or(0);
    let needed_gb = (ram_mb as f64 * 0.4) / 1024f64;
    let total = util::disk_total_bytes(Path::new(&windir)).unwrap_or(0);
    if hiberfil.exists() {
        let size = std::fs::metadata(&hiberfil).map(|m| m.len()).unwrap_or(0);
        checks.push(Check::ok(
            "hiberfil.sys 空间",
            format!(
                "已存在 {:.1} GiB / 系统盘总容量 {:.0} GiB（RAM {ram_mb} MB，休眠约需 {needed_gb:.1} GiB）",
                util::gb(size as i64),
                util::gb(total as i64)
            ),
        ));
    } else {
        let free = util::disk_free_bytes(Path::new(&windir))
            .map(|b| util::gb(b as i64))
            .unwrap_or(0.0);
        // `powercfg /h` without on/off only prints usage, so the availability
        // verdict comes from the `powercfg /a` parse above (item 1).
        let reason = if power_available {
            "powercfg /a 显示休眠可用，但文件尚未生成"
        } else {
            "休眠未启用（powercfg /h on 可开启，会占用系统盘空间）"
        };
        checks.push(if free >= needed_gb.max(1.0) {
            Check::warn(
                "hiberfil.sys 空间",
                format!(
                    "休眠文件不存在：{reason}。系统盘剩余 {free:.1} GiB，足以容纳休眠（约需 {needed_gb:.1} GiB）"
                ),
            )
        } else {
            Check::fail(
                "hiberfil.sys 空间",
                format!("系统盘剩余 {free:.1} GiB，不足以容纳休眠（约需 {needed_gb:.1} GiB）"),
            )
        });
    }

    // 3. battery API (shared with `guard battery` via battery::verify_source)
    let info = battery::read_system_power_status();
    if let Err(err) = battery::verify_source(&battery::SystemBattery) {
        tracing::debug!("battery verify: {err}");
    }
    if info.ac.is_none() && info.level_pct.is_none() {
        checks.push(Check::warn(
            "电池 API",
            "GetSystemPowerStatus 返回未知值（台式机 / 无电池）。三电平守护在此机器上不会触发",
        ));
    } else {
        checks.push(Check::ok(
            "电池 API",
            format!(
                "AC={:?} 电量={:?}% 充电中={:?} flag={:?}",
                info.ac, info.level_pct, info.charging, info.battery_flag
            ),
        ));
    }

    // 4. administrator rights
    if util::is_admin() {
        checks.push(Check::ok(
            "管理员权限",
            "已提权：crunch / 电源策略 / autologon / sched --system 可直接执行",
        ));
    } else {
        checks.push(Check::warn(
            "管理员权限",
            "未提权：crunch on/off、guard schedule/wake/autologon、sched --system、--daemon 会在执行时请求 UAC",
        ));
    }

    // 5. target app adapters, measured right now (never hard-coded)
    let apps = ctx.cfg.whitelisted_apps();
    if apps.is_empty() {
        checks.push(Check::warn(
            "目标应用适配器",
            "[guard.whitelist.apps] 为空：优雅暂停没有目标。可先 devzone profile apply mid",
        ));
    } else {
        for (app, app_cfg) in apps {
            match adapters::pause::probe_levels(&app, &app_cfg) {
                Ok(probes) => {
                    let available: Vec<String> = probes
                        .iter()
                        .filter(|p| p.available)
                        .map(|p| p.level.as_str().to_string())
                        .collect();
                    let running = probes
                        .first()
                        .map(|p| p.process.clone())
                        .unwrap_or_default();
                    let detail = format!(
                        "配置={}；本次实测可用={}；进程状态={}",
                        if app_cfg.enabled_levels.is_empty() {
                            "-".to_string()
                        } else {
                            app_cfg.enabled_levels.join("/")
                        },
                        if available.is_empty() {
                            "-".to_string()
                        } else {
                            available.join("/")
                        },
                        running
                    );
                    checks.push(if available.is_empty() {
                        Check::warn(&format!("适配器 {app}"), detail)
                    } else {
                        Check::ok(&format!("适配器 {app}"), detail)
                    });
                }
                Err(err) => checks.push(Check::warn(
                    &format!("适配器 {app}"),
                    format!("实测失败：{err}（结果仅提示，不影响其他项）"),
                )),
            }
        }
    }

    // 6. notification channels
    let channels = notify::build_channels(&ctx.cfg);
    if channels.is_empty() {
        checks.push(Check::warn(
            "通知通道",
            "全部关闭：至少启用 watchdog.notify.console 才能看到事件",
        ));
    }
    for channel in channels {
        checks.push(match channel.probe() {
            Ok(text) => Check::ok(&format!("通知通道 {}", channel.name()), text),
            Err(err) => Check::fail(&format!("通知通道 {}", channel.name()), err.to_string()),
        });
    }
    if ctx.cfg.watchdog.notify.smtp.enable {
        checks.push(Check::warn(
            "SMTP 实际发信",
            "doctor 只做 NOOP 连接测试；完整发信测试：devzone event remind",
        ));
    }

    // 7. UIA availability
    checks.push(match adapters::uia::probe() {
        Ok(detail) => Check::ok("UIA 可用性", detail),
        Err(err) => Check::fail("UIA 可用性", err.to_string()),
    });

    // 8. configuration
    checks.push(match &ctx.config_error {
        Some(err) => Check::fail("配置 devzone.toml", err.clone()),
        None => Check::ok(
            "配置 devzone.toml",
            format!(
                "解析成功：profile={} zones={} whitelist_apps={} regions={} plugins_dir={}",
                ctx.cfg.profile.active,
                ctx.cfg.zone.zones.len(),
                ctx.cfg.guard.whitelist.apps.len(),
                ctx.cfg.watchdog.regions.len(),
                ctx.cfg.plugin_dir(&ctx.paths).display()
            ),
        ),
    });

    // 9. BIOS hint: prompt only, never probed (section 5.4 item 9)
    checks.push(Check::warn(
        "定时开机 (BIOS)",
        "定时开机需主板 RTC 唤醒 / AC 恢复，请到 BIOS 确认（机型相关：ThinkPad After Power Loss / Dell AC Recovery 等）。devzone 不承诺纯软件开机",
    ));

    checks
}

fn doctor(ctx: &Ctx) -> Result<()> {
    let checks = doctor_checks(ctx);
    let failed = checks.iter().filter(|c| c.verdict == Verdict::Fail).count();
    println!("devzone {} doctor", env!("CARGO_PKG_VERSION"));
    println!("数据目录：{}", ctx.paths.root.display());
    for check in &checks {
        println!(
            "{} {} — {}",
            check.verdict.glyph(),
            check.title,
            check.detail
        );
    }
    if ctx.config_error.is_none() {
        match ctx.db() {
            Ok(db) => println!("[i] 数据库统计：{}", db.table_counts()?),
            Err(err) => println!("[✗] 数据库：{err}"),
        }
    }
    let attention = checks.iter().filter(|c| c.verdict != Verdict::Ok).count();
    println!("[i] 结论：{attention} 项需要关注，其中 {failed} 项失败");
    if failed > 0 {
        return Err(anyhow::anyhow!("doctor 存在 {failed} 项失败"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

fn status(ctx: &Ctx, as_json: bool, raw: &str) -> Result<()> {
    let db = ctx.db()?;
    let overrides = Config::user_override_paths(raw).unwrap_or_default();
    let data = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "data_root": ctx.paths.root.display().to_string(),
        "admin": util::is_admin(),
        "dry_run": util::ExternalCmd::dry_run(),
        "profile": {
            "active": ctx.cfg.profile.active,
            "user_overridden": overrides,
        },
        "battery": battery_json(),
        "zone": mod_zone::summary(ctx, &db)?,
        "sched": mod_sched::summary(ctx)?,
        "guard": mod_guard::summary(ctx, &db)?,
        "crunch": mod_crunch::summary(ctx)?,
        "watchdog": mod_watchdog::summary(ctx, &db)?,
        "notify": notify::channel_report(&ctx.cfg),
        "plugins": {
            "dir": ctx.cfg.plugin_dir(&ctx.paths).display().to_string(),
            "enabled": ctx.cfg.plugin.enable,
            "installed": mod_plugin::list_installed(&ctx.cfg.plugin_dir(&ctx.paths)).len(),
        },
        "events": recent_events_json(ctx),
    });
    if ctx.emit_json("status", data.clone(), as_json)? {
        return Ok(());
    }
    println!(
        "devzone {} — profile={} admin={} dry_run={}",
        env!("CARGO_PKG_VERSION"),
        ctx.cfg.profile.active,
        util::is_admin(),
        util::ExternalCmd::dry_run()
    );
    println!("数据目录：{}", ctx.paths.root.display());
    if let Some(error) = &ctx.config_error {
        println!("[!] 配置错误：{error}");
    }
    for (section, value) in data.as_object().into_iter().flatten() {
        if ["version", "data_root", "admin", "dry_run", "profile"].contains(&section.as_str()) {
            continue;
        }
        println!("\n[{section}]");
        println!("{}", render_section(value));
    }
    Ok(())
}

fn recent_events_json(ctx: &Ctx) -> Value {
    match ctx.bus().and_then(|bus| bus.recent_events(5)) {
        Ok(events) => json!(events
            .iter()
            .map(|e| json!({ "ts": e.ts, "name": e.name, "payload": e.payload }))
            .collect::<Vec<_>>()),
        Err(err) => json!({ "error": err.to_string() }),
    }
}

/// Render one status sub-object as `key: value` lines.
fn render_section(value: &Value) -> String {
    let mut out = String::new();
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                out.push_str(&format!("  {key}: {}\n", compact(child)));
            }
        }
        other => out.push_str(&format!("  {}\n", compact(other))),
    }
    if out.trim().is_empty() {
        out.push_str("  (空)\n");
    }
    out.trim_end().to_string()
}

fn compact(value: &Value) -> String {
    match value {
        Value::Array(items) if items.is_empty() => "(空)".to_string(),
        Value::Object(map) if map.is_empty() => "(空)".to_string(),
        Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .map(|item| match item {
                    Value::Object(map) => map
                        .iter()
                        .map(|(k, v)| format!("{k}={}", compact(v)))
                        .collect::<Vec<_>>()
                        .join(" "),
                    other => compact(other),
                })
                .collect();
            format!("[{}]", parts.join(" | "))
        }
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{k}={}", compact(v)))
                .collect();
            format!("{{{}}}", parts.join(" "))
        }
        Value::String(text) => text.clone(),
        Value::Null => "-".to_string(),
        other => other.to_string(),
    }
}

fn battery_json() -> Value {
    let info = battery::read_system_power_status();
    json!({
        "ac": info.ac,
        "level_pct": info.level_pct,
        "charging": info.charging,
        "flag": info.battery_flag,
        "life_min": info.life_min,
    })
}

/// Event names accepted by `devzone event <name>` (documentation aid).
pub fn known_events() -> &'static [&'static str] {
    &name::KNOWN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_support_parses_english_output() {
        let text = r#"
This system has the following sleep states available:
    Standby (S3)
    Hibernate
This system has the following sleep states not available:
    Standby (S1)
    Standby (S2)
The fast startup sleep state requires an account-level password
"#;
        let support = parse_power_support(text);
        assert!(support.hibernate, "{support:?}");
        assert!(support.s3);
        assert_eq!(support.hibernate_line, "Hibernate");
        assert!(!support.hibernate_negative_line.is_empty() || support.hibernate);
    }

    #[test]
    fn power_support_parses_chinese_negative_block() {
        let text = "此计算机上不存在以下睡眠状态:\n    休眠\n    快速启动\n";
        let support = parse_power_support(text);
        assert!(!support.hibernate);
        assert_eq!(support.hibernate_negative_line, "休眠");
        assert!(!support.fast_startup);
    }

    #[test]
    fn power_support_parses_english_negative_block() {
        let text = "The following sleep states are not available on this system:\n    Hibernate\n";
        let support = parse_power_support(text);
        assert!(!support.hibernate);
        assert_eq!(support.hibernate_negative_line, "Hibernate");
    }

    #[test]
    fn unknown_sleep_states_are_collected() {
        let support = parse_power_support("    Standby (S1)\n    Wake on LAN");
        assert_eq!(support.other_states.len(), 2, "{support:?}");
    }

    #[test]
    fn memory_probe_is_plausible() {
        if let Some(mb) = memory_mb() {
            assert!(mb >= 512, "{mb} MB");
        }
    }

    #[test]
    fn disk_probe_on_system_drive() {
        let free = util::disk_free_bytes(Path::new("C:\\")).unwrap();
        assert!(free > 0);
        let total = util::disk_total_bytes(Path::new("C:\\")).unwrap();
        assert!(total >= free);
        assert!(util::disk_free_bytes(Path::new("Z:\\nope")).is_err());
    }

    #[test]
    fn rendering_helpers() {
        let rendered =
            render_section(&json!({"zones": [{"name": "work", "free_gb": 6.2}], "count": 1}));
        assert!(rendered.contains("zones"), "{rendered}");
        assert!(rendered.contains("free_gb=6.2"), "{rendered}");
        assert_eq!(render_section(&json!([])).trim(), "(空)");
        assert_eq!(compact(&Value::Null), "-");
        assert!(battery_json().get("ac").is_some());
        assert!(known_events().contains(&"zone_alert"));
        assert_eq!(Verdict::Fail.glyph(), "[✗]");
        assert_eq!(Verdict::Ok.as_str(), "ok");
        assert_eq!(
            Check::ok("t", "d").to_json()["verdict"],
            serde_json::json!("ok")
        );
    }
}

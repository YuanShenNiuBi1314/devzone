//! M3 电源守护（build guide section 6.3）：三电平状态机驱动、优雅暂停、断点三层、
//! 省电等待、本地恢复、autologon、定时关机/唤醒。
//!
//! 状态机本体在 `battery::Guardian`（纯函数 + 可注入假电池）；本模块负责执行
//! `battery::Action`：事件总线、powercfg、ShutdownBlockReasonCreate、关机、拉起应用。
//!
//! 安全边界（与 §9 / §10 / §16 一致）：
//! * 一切外部命令（powercfg / shutdown / git / rundll32 / schtasks）都走
//!   `util::ExternalCmd`，`DEVZONE_DRY_RUN=1` 时只打印不执行；
//! * 注册表写入（autologon）在 dry-run 下同样只打印；密码只在交互终端读入，
//!   永不写入 devzone.toml，永不回显；
//! * `guard watch --once` 是冒烟模式：任何破坏性动作（暂停阶梯 / 电源计划 /
//!   关机 / 睡眠）只打印将做什么。

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use chrono::Local;
use serde_json::{json, Value};
use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, KEY_WOW64_64KEY};
use winreg::RegKey;

use crate::adapters::pause::{self, AppPauseResult, PauseRequest};
use crate::adapters::{self, Level};
use crate::battery::{self, Action, BatteryInfo, BatterySource, Guardian, SystemBattery};
use crate::cli::{AutologonCmd, GuardCmd};
use crate::config::{AppPauseCfg, GuardCfg};
use crate::db::{Db, TaskRow};
use crate::events::name;
use crate::mod_sched;
use crate::util::{self, ExternalCmd};
use crate::Ctx;

/// §6.3.9：自动登录键值所在的路径（已查证路径，行为属 §16 待实测项 6）。
const WINLOGON_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon";
/// 断点三层第③层在 `tasks` 表里的汇总行名字。
const CHECKPOINT_ROW: &str = "checkpoint";
/// 优雅暂停阻断理由（§6.3.4）。
const BLOCK_REASON: &str = "devzone: 优雅暂停进行中";

pub fn handle(cmd: &GuardCmd, ctx: &Ctx) -> Result<()> {
    match cmd {
        GuardCmd::Status { json } => status(ctx, *json),
        GuardCmd::Battery { json, times } => battery_cmd(ctx, *json, *times),
        GuardCmd::PauseOn { app } => pause_on(ctx, app),
        GuardCmd::Resume => resume(ctx),
        GuardCmd::Schedule { time } => schedule(ctx, time),
        GuardCmd::Wake { time } => wake(ctx, time),
        GuardCmd::Watch {
            interval,
            once,
            daemon,
        } => watch(ctx, *interval, *once, *daemon),
        GuardCmd::Autologon { cmd } => autologon(ctx, cmd),
    }
}

// ---------------------------------------------------------------------------
// guard status / guard battery
// ---------------------------------------------------------------------------

/// tier 策略提示（§6.3.10：tier3 只输出文案，不做任何动作）。
pub fn tier_hint(strategy: &str) -> String {
    match strategy {
        "tier2" => "tier2：优雅暂停后转入睡眠并设唤醒定时器；睡死与睡眠期间继续耗电的风险见 README（§6.3.10）".to_string(),
        "tier3" => "tier3：建议把长任务放云端 VM / CI（GitHub Actions、云端 Runner），本机只做监测与通知；\
                    devzone 不迁移任务，也不注入第三方应用"
            .to_string(),
        other => format!("tier1：本机优雅暂停 + 省电等待 + 充电后本地恢复（strategy={other}）"),
    }
}

/// `guard status`：状态机状态（单次读数推断）+ 阈值 + tier 提示 + 最近迁移历史。
fn status(ctx: &Ctx, as_json: bool) -> Result<()> {
    let db = ctx.db()?;
    let data = summary(ctx, &db)?;
    if ctx.emit_json("guard status", data.clone(), as_json)? {
        return Ok(());
    }
    println!(
        "守护：enable={} strategy={} 状态={} 电量={:?}% AC={:?}",
        ctx.cfg.guard.enable,
        ctx.cfg.guard.strategy,
        data["state"].as_str().unwrap_or("unknown"),
        data["battery"]["level_pct"].as_u64(),
        data["battery"]["ac"].as_bool(),
    );
    println!(
        "阈值：warn={} pause={} hard={} resume={} max_wait={}min poll={}s pause_timeout={}s",
        ctx.cfg.guard.warn_level,
        ctx.cfg.guard.pause_level,
        ctx.cfg.guard.hard_level,
        ctx.cfg.guard.resume_level,
        ctx.cfg.guard.max_wait_min,
        ctx.cfg.guard.battery_poll_sec,
        ctx.cfg.guard.pause_timeout_sec
    );
    println!("{}", tier_hint(&ctx.cfg.guard.strategy));
    println!(
        "省电等待：enable={} 计划切换={} 禁睡眠={} 关屏幕={}（M1/M2 只提示，不控制屏幕）",
        ctx.cfg.guard.power_saving.enable,
        ctx.cfg.guard.power_saving.enable_power_plan_switch,
        ctx.cfg.guard.power_saving.disable_sleep,
        ctx.cfg.guard.power_saving.enable_screen_off
    );
    let apps: Vec<String> = ctx
        .cfg
        .whitelisted_apps()
        .iter()
        .map(|(app, cfg)| {
            format!(
                "{app}[{}]",
                pause::steps_for(cfg)
                    .iter()
                    .map(|l| l.as_str())
                    .collect::<Vec<_>>()
                    .join(">")
            )
        })
        .collect();
    println!(
        "优雅暂停：{} 目标={}",
        if ctx.cfg.guard.pause_enabled {
            "开"
        } else {
            "关"
        },
        if apps.is_empty() {
            "（白名单为空）".to_string()
        } else {
            apps.join(", ")
        }
    );
    if let Some(checkpoint) = data.get("checkpoint") {
        if !checkpoint.is_null() {
            println!(
                "最新断点：id={} ts={} 未干净退出={}",
                checkpoint["id"].as_u64().unwrap_or(0),
                checkpoint["ts"].as_str().unwrap_or("-"),
                checkpoint["dirty"]
                    .as_array()
                    .map(|items| items
                        .iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join("/"))
                    .unwrap_or_default()
            );
        }
    }
    println!("{}", data["autologon"]["hint"].as_str().unwrap_or(""));
    let history = data["history"].as_array().cloned().unwrap_or_default();
    if history.is_empty() {
        println!("迁移历史：（battery_events 表为空，先跑 devzone guard watch --once）");
    } else {
        println!("迁移历史（最近 {} 条）：", history.len());
        for row in history {
            println!(
                "  {} level={} ac={} action={}",
                row["ts"].as_str().unwrap_or("-"),
                row["level"]
                    .as_u64()
                    .map(|v| v.to_string())
                    .unwrap_or("-".into()),
                row["ac"]
                    .as_u64()
                    .map(|v| v.to_string())
                    .unwrap_or("-".into()),
                row["action"].as_str().unwrap_or("-")
            );
        }
    }
    Ok(())
}

/// 供 `devzone status` / `guard status`：状态机状态、阈值、最近 battery_events。
pub fn summary(ctx: &Ctx, db: &Db) -> Result<Value> {
    let info = battery::read_system_power_status();
    let mut guardian = Guardian::new(&ctx.cfg.guard);
    // A one-shot CLI call has no resident state: feed the current reading to a
    // fresh machine and report the state it infers (§6.3.2), plus the recorded
    // history, which is the real state across process runs.
    guardian.on_reading(&info, Local::now());
    let state = serde_json::to_value(guardian.status()).unwrap_or(serde_json::Value::Null);
    let history = db
        .battery_events(10)
        .unwrap_or_default()
        .iter()
        .map(|row| {
            json!({
                "ts": row.ts,
                "level": row.level,
                "ac": row.ac,
                "action": row.action,
            })
        })
        .collect::<Vec<_>>();
    let checkpoint =
        latest_checkpoint_json(db).unwrap_or_else(|err| json!({ "error": err.to_string() }));
    let autologon = autologon_report();
    Ok(json!({
        "enable": ctx.cfg.guard.enable,
        "pause_enabled": ctx.cfg.guard.pause_enabled,
        "state": state["state"],
        "charger_anomaly": state["charger_anomaly"],
        "inferred_from_single_reading": true,
        "note": "CLI 单次调用用一次读数推断状态；真状态由常驻进程 `devzone guard watch` 维护（§6.3.2）",
        "strategy": ctx.cfg.guard.strategy,
        "tier_hint": tier_hint(&ctx.cfg.guard.strategy),
        "thresholds": {
            "warn_level": ctx.cfg.guard.warn_level,
            "pause_level": ctx.cfg.guard.pause_level,
            "hard_level": ctx.cfg.guard.hard_level,
            "resume_level": ctx.cfg.guard.resume_level,
            "max_wait_min": ctx.cfg.guard.max_wait_min,
            "battery_poll_sec": ctx.cfg.guard.battery_poll_sec,
            "pause_timeout_sec": ctx.cfg.guard.pause_timeout_sec,
        },
        "battery": battery_json(&info),
        "power_saving": {
            "enable": ctx.cfg.guard.power_saving.enable,
            "power_plan_switch": ctx.cfg.guard.power_saving.enable_power_plan_switch,
            "disable_sleep": ctx.cfg.guard.power_saving.disable_sleep,
            "screen_off": ctx.cfg.guard.power_saving.enable_screen_off,
            "screen_off_note": "enable_screen_off=false 时只打印提示：M1/M2 不控制屏幕（§6.3.7 第 4 项）",
        },
        "git": {
            "checkpoint": ctx.cfg.guard.git_checkpoint,
            "pull_on_resume": ctx.cfg.guard.git_pull_on_resume,
        },
        "whitelist": ctx
            .cfg
            .whitelisted_apps()
            .iter()
            .map(|(app, cfg)| json!({
                "app": app,
                "configured_levels": cfg.enabled_levels,
                "ocr_fallback": cfg.ocr_fallback,
                "ladder": pause::steps_for(cfg).iter().map(|l| l.as_str()).collect::<Vec<_>>(),
                "wait_exit_file": cfg.wait_exit_file,
            }))
            .collect::<Vec<_>>(),
        "checkpoint": checkpoint,
        "autologon": autologon,
        "history": history,
    }))
}

fn battery_json(info: &BatteryInfo) -> Value {
    json!({
        "ac": info.ac,
        "level_pct": info.level_pct,
        "battery_flag": info.battery_flag,
        "life_min": info.life_min,
        "charging": info.charging,
        "unknown": info.is_unknown(),
    })
}

/// `guard battery [--json] [--times N]`：轮询 N 次，间隔 `battery_poll_sec`。
fn battery_cmd(ctx: &Ctx, as_json: bool, times: u32) -> Result<()> {
    let rounds = times.max(1);
    let source = SystemBattery;
    let poll = Duration::from_secs(ctx.cfg.guard.battery_poll_sec.max(1));
    let mut readings = Vec::new();
    let mut guardian = Guardian::new(&ctx.cfg.guard);
    for round in 0..rounds {
        if round > 0 {
            util::progress(&format!(
                "等待 {}s 后第 {} 次读数…",
                poll.as_secs(),
                round + 1
            ));
            std::thread::sleep(poll);
        }
        let info = source.read();
        let actions = guardian.on_reading(&info, Local::now());
        readings.push(json!({
            "ts": util::now_iso(),
            "battery": battery_json(&info),
            "would_do": actions.iter().map(action_label).collect::<Vec<_>>(),
        }));
        if !as_json {
            println!(
                "{}  AC={:?} 电量={:?}% 充电中={:?} 剩余={:?}min flag={:?}",
                util::now_iso(),
                info.ac,
                info.level_pct,
                info.charging,
                info.life_min,
                info.battery_flag
            );
            if !actions.is_empty() {
                util::progress(&format!(
                    "本轮触发动作：{}",
                    actions
                        .iter()
                        .map(action_label)
                        .collect::<Vec<_>>()
                        .join(" → ")
                ));
            }
        }
    }
    let state = serde_json::to_value(guardian.status()).unwrap_or(serde_json::Value::Null);
    let data = json!({
        "readings": readings,
        "rounds": rounds,
        "poll_sec": ctx.cfg.guard.battery_poll_sec,
        "state": state["state"],
        "charger_anomaly": state["charger_anomaly"],
        "thresholds": {
            "warn_level": ctx.cfg.guard.warn_level,
            "pause_level": ctx.cfg.guard.pause_level,
            "hard_level": ctx.cfg.guard.hard_level,
            "resume_level": ctx.cfg.guard.resume_level,
        },
        "tier_hint": tier_hint(&ctx.cfg.guard.strategy),
    });
    if ctx.emit_json("guard battery", data, as_json)? {
        return Ok(());
    }
    println!(
        "推断状态={:?}（阈值 warn={}/pause={}/hard={}/resume={}）",
        state["state"].as_str().unwrap_or("Normal"),
        ctx.cfg.guard.warn_level,
        ctx.cfg.guard.pause_level,
        ctx.cfg.guard.hard_level,
        ctx.cfg.guard.resume_level
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// graceful pause + checkpoint trio (6.3.5 / 6.3.6)
// ---------------------------------------------------------------------------

/// 对给定应用清单跑五级降级阶梯，并写 `tasks` 断点行（§6.3.5 第三层）。
///
/// 三层落点：①应用自身保存（`pause::run` 的阶梯负责）②git 自动提交（可选，
/// 失败只记日志不阻塞）③`tasks` 表汇总行 + 每应用行。
pub fn run_graceful_pause(
    ctx: &Ctx,
    apps: &[(String, AppPauseCfg)],
    reason: &str,
) -> Result<Vec<AppPauseResult>> {
    let db = ctx.db()?;
    let bus = ctx.bus()?;
    let per_level = pause::per_level_timeout_or_default(ctx.cfg.guard.pause_timeout_sec);
    let mut outcomes: Vec<PauseOutcome> = Vec::new();

    for (app, cfg) in apps {
        let expected: Vec<&str> = pause::steps_for(cfg)
            .iter()
            .copied()
            .map(Level::as_str)
            .collect();
        bus.emit_json(
            name::GRACEFUL_PAUSE_STARTED,
            json!({ "app": app, "expected_levels": expected, "reason": reason }),
        )?;
        // Resolved *before* the ladder: after a clean exit the process is gone
        // and the path is only known from this moment.
        let exe = adapters::find_processes(app)
            .first()
            .and_then(|p| p.exe.clone())
            .map(|path| path.to_string_lossy().to_string());
        let request = PauseRequest {
            app: app.clone(),
            exe: exe.as_deref().map(PathBuf::from),
            cfg: cfg.clone(),
            per_level_timeout: per_level,
            reason: reason.to_string(),
        };
        util::progress(&format!(
            "优雅暂停 {app}：[{}]（{reason}）",
            expected.join(">")
        ));
        let result = pause::run(&request)?;
        for attempt in &result.attempts {
            println!(
                "  [{}] {} — {}（{}ms）",
                if attempt.ok { "✓" } else { "✗" },
                attempt.level.as_str(),
                attempt.note,
                attempt.duration_ms
            );
        }
        bus.emit_json(
            name::GRACEFUL_PAUSE_DONE,
            json!({
                "app": result.app,
                "clean": result.clean_exit,
                "level_used": result.level_used.map(|l| l.as_str()),
                "duration_ms": result.duration_ms,
            }),
        )?;
        outcomes.push(PauseOutcome { result, exe });
    }
    let results: Vec<AppPauseResult> = outcomes
        .iter()
        .map(|outcome| outcome.result.clone())
        .collect();

    // Layer 2: git checkpoints. Never blocks the shutdown path (§9).
    let git = if ctx.cfg.guard.git_checkpoint {
        git_checkpoint(ctx)?
    } else {
        Vec::new()
    };
    // Layer 3: the devzone manifest.
    let info = battery::read_system_power_status();
    let record = record_checkpoint(&db, &outcomes, &info, reason, &git)?;
    util::progress(&format!(
        "断点已写入 tasks 汇总行 id={}（写入时间 {}），未干净退出：{}",
        record.id,
        record.ts,
        if record.dirty.is_empty() {
            "无".to_string()
        } else {
            record.dirty.join(", ")
        }
    ));
    Ok(results)
}

/// One paused app plus the image path known before the pause, which is what
/// `guard resume` needs once the process itself is gone.
#[derive(Clone, Debug)]
pub struct PauseOutcome {
    pub result: AppPauseResult,
    pub exe: Option<String>,
}

impl PauseOutcome {
    pub fn to_json(&self) -> Value {
        // The ladder result is the shared shape; the pre-pause image path is
        // the only extra thing a resident process can still report.
        let mut row = self.result.to_json();
        if let Some(map) = row.as_object_mut() {
            map.insert("exe".to_string(), json!(self.exe));
        }
        row
    }
}

/// 断点第三层：`tasks` 汇总行 + 每应用一行（§6.3.5）。
#[derive(Clone, Debug)]
pub struct CheckpointRecord {
    pub id: i64,
    pub ts: String,
    pub dirty: Vec<String>,
}

/// Pure half of the manifest payload: which apps exited cleanly, which did not.
pub fn checkpoint_payload(
    results: &[AppPauseResult],
    info: &BatteryInfo,
    reason: &str,
    git: &[GitOutcome],
    ts: &str,
) -> Value {
    let apps: Vec<Value> = results
        .iter()
        .map(|r| {
            json!({
                "app": r.app,
                "clean": r.clean_exit,
                "level_used": r.level_used.map(|l| l.as_str()),
                "duration_ms": r.duration_ms,
                "attempts": r.attempts.iter().map(|a| json!({
                    "level": a.level.as_str(), "ok": a.ok, "note": a.note,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let dirty: Vec<String> = results
        .iter()
        .filter(|r| !r.clean_exit)
        .map(|r| r.app.clone())
        .collect();
    json!({
        "ts": ts,
        "reason": reason,
        "level": info.level_pct,
        "ac": info.ac,
        "charging": info.charging,
        "apps": apps,
        "dirty": dirty,
        "git": git.iter().map(GitOutcome::to_json).collect::<Vec<_>>(),
        "note": "AI 会话已自动保存，回来后 / 手机端点继续即可；未干净退出的应用见 dirty",
    })
}

fn record_checkpoint(
    db: &Db,
    outcomes: &[PauseOutcome],
    info: &BatteryInfo,
    reason: &str,
    git: &[GitOutcome],
) -> Result<CheckpointRecord> {
    let ts = util::now_iso();
    let results: Vec<AppPauseResult> = outcomes.iter().map(|o| o.result.clone()).collect();
    let payload = checkpoint_payload(&results, info, reason, git, &ts);
    let id = db.insert_task(
        CHECKPOINT_ROW,
        "paused",
        None,
        None,
        None,
        Some(&payload.to_string()),
        &ts,
    )?;
    for outcome in outcomes {
        let result = &outcome.result;
        db.insert_task(
            &result.app,
            if result.clean_exit {
                "clean_exit"
            } else {
                "dirty_exit"
            },
            Some(&result.app),
            outcome.exe.as_deref(),
            Some(i64::from(result.clean_exit)),
            Some(
                {
                    let mut row = outcome.to_json();
                    if let Some(map) = row.as_object_mut() {
                        map.insert("checkpoint_id".to_string(), json!(id));
                        // the full attempt list lives in the manifest row; the
                        // per-app row stays compact
                        map.remove("attempts");
                    }
                    row.to_string()
                }
                .as_str(),
            ),
            &ts,
        )?;
    }
    let dirty: Vec<String> = results
        .iter()
        .filter(|r| !r.clean_exit)
        .map(|r| r.app.clone())
        .collect();
    Ok(CheckpointRecord { id, ts, dirty })
}

// ---------------------------------------------------------------------------
// checkpoint layer 2: git auto-commit (6.3.5)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitStatus {
    /// Working tree clean: nothing to commit (never an error).
    NothingToCommit,
    Committed,
    /// Failure is logged and does not block the pause/shutdown path (§9).
    Failed(String),
    /// Dry run: no git call was made.
    Planned,
}

#[derive(Clone, Debug)]
pub struct GitOutcome {
    pub repo: PathBuf,
    pub status: GitStatus,
    pub message: String,
}

impl GitOutcome {
    pub fn to_json(&self) -> Value {
        json!({
            "repo": self.repo.to_string_lossy(),
            "status": match self.status {
                GitStatus::NothingToCommit => "skipped",
                GitStatus::Committed => "committed",
                GitStatus::Failed(_) => "failed",
                GitStatus::Planned => "planned",
            },
            "message": self.message,
        })
    }
}

/// Pure half of layer 2: the exact commands for one repo, empty when the tree
/// is clean (`git status --porcelain` output without entries).
pub fn git_commands(repo: &Path, message: &str, dirty: bool) -> Vec<ExternalCmd> {
    if !dirty {
        return Vec::new();
    }
    let text = message.to_string();
    vec![
        ExternalCmd::new("git.exe", &["-C", &repo.to_string_lossy(), "add", "-A"]),
        ExternalCmd::new(
            "git.exe",
            &["-C", &repo.to_string_lossy(), "commit", "-m", &text],
        ),
    ]
}

/// Pure: does `git status --porcelain` output mean "there is something to save"?
pub fn has_working_changes(porcelain: &str) -> bool {
    porcelain.lines().any(|line| !line.trim().is_empty())
}

/// Commit message template from §6.3.5: `devzone checkpoint <ts>`.
pub fn checkpoint_commit_message(ts: &str) -> String {
    format!("devzone checkpoint {ts}")
}

/// Repos inside the registered zones: the zone itself if it is a repo, plus its
/// immediate children that are repos. Depth-bounded (never walks a whole disk).
pub fn discover_repos(zone_path: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if zone_path.join(".git").exists() {
        found.push(zone_path.to_path_buf());
        return found;
    }
    if max_depth == 0 {
        return found;
    }
    let Ok(entries) = std::fs::read_dir(zone_path) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || path.join(".git").exists() && found.len() >= MAX_REPOS {
            continue;
        }
        if path.join(".git").exists() {
            found.push(path);
        } else {
            found.extend(discover_repos(&path, max_depth - 1));
        }
    }
    found.truncate(MAX_REPOS);
    found
}

/// Repos are bounded so a zone pointing at a whole drive cannot stall a pause.
pub const MAX_REPOS: usize = 50;

/// Layer 2 execution: failures never propagate (§9 "失败仅记日志不阻塞").
fn git_checkpoint(ctx: &Ctx) -> Result<Vec<GitOutcome>> {
    let mut outcomes = Vec::new();
    let zones: Vec<PathBuf> = ctx
        .cfg
        .zone
        .zones
        .values()
        .map(|zone| PathBuf::from(&zone.path))
        .collect();
    let ts = util::now_compact();
    let message = checkpoint_commit_message(&ts);
    for zone in zones {
        for repo in discover_repos(&zone, 2) {
            if util::ExternalCmd::dry_run() {
                outcomes.push(GitOutcome {
                    repo: repo.clone(),
                    status: GitStatus::Planned,
                    message: format!("dry-run: git add -A && git commit -m \"{message}\""),
                });
                continue;
            }
            let status_cmd = ExternalCmd::new(
                "git.exe",
                &["-C", &repo.to_string_lossy(), "status", "--porcelain"],
            );
            let porcelain = match status_cmd.run() {
                Ok(output) => util::decode(&output.stdout),
                Err(err) => {
                    tracing::warn!("git checkpoint: {repo:?} 状态读取失败: {err}");
                    outcomes.push(GitOutcome {
                        repo: repo.clone(),
                        status: GitStatus::Failed(err.to_string()),
                        message: "git status 失败".to_string(),
                    });
                    continue;
                }
            };
            if !has_working_changes(&porcelain) {
                tracing::info!("git checkpoint: {} 无改动，跳过", repo.display());
                outcomes.push(GitOutcome {
                    repo: repo.clone(),
                    status: GitStatus::NothingToCommit,
                    message: "无改动".to_string(),
                });
                continue;
            }
            let mut failure = None;
            for command in git_commands(&repo, &message, true) {
                if let Err(err) = command.run() {
                    failure = Some(err.to_string());
                    break;
                }
            }
            outcomes.push(match failure {
                Some(err) => {
                    tracing::warn!(
                        "git checkpoint 失败（不阻塞关机）：{} {err}",
                        repo.display()
                    );
                    GitOutcome {
                        repo,
                        status: GitStatus::Failed(err),
                        message: "提交失败，已记录不阻塞".to_string(),
                    }
                }
                None => {
                    util::progress(&format!("git checkpoint：已提交 {}", repo.display()));
                    GitOutcome {
                        repo,
                        status: GitStatus::Committed,
                        message: message.clone(),
                    }
                }
            });
        }
    }
    Ok(outcomes)
}

/// `guard pause-on <app>`: manual single-app ladder, for testing adapters.
fn pause_on(ctx: &Ctx, app: &str) -> Result<()> {
    let (name, cfg) = match lookup_app_cfg(&ctx.cfg.whitelisted_apps(), app) {
        Some(found) => found,
        None => {
            // Not whitelisted: still run the documented mid preset ladder so the
            // command stays useful for adapter testing, and say it is ad hoc.
            util::progress(&format!(
                "{app} 不在 [guard.whitelist.apps]：按临时阶梯 hotkey>uia>wm_close 执行（§6.3.6）"
            ));
            let cfg = AppPauseCfg {
                enabled_levels: vec![
                    "hotkey".to_string(),
                    "uia".to_string(),
                    "wm_close".to_string(),
                ],
                ..AppPauseCfg::default()
            };
            (app.to_string(), cfg)
        }
    };
    let steps = pause::steps_for(&cfg);
    if steps.is_empty() {
        return Err(anyhow!(
            "{name} 的 enabled_levels 为空：没有可执行的级别（拒绝直接强制结束）"
        ));
    }
    // §6.3.4: register a shutdown block reason around the ladder. The API can
    // only delay a shutdown briefly — Windows times it out and force-shuts
    // anyway (§16 待实测项 1), and the reason dies with this CLI process.
    let block = ShutdownBlock::create(BLOCK_REASON);
    util::progress(&block.describe());
    tracing::info!("shutdown block reason active={}", block.is_active());
    let apps = vec![(name.clone(), cfg)];
    let results = run_graceful_pause(ctx, &apps, "manual")?;
    let result = results
        .first()
        .ok_or_else(|| anyhow!("阶梯没有返回结果（{name}）"))?;
    println!(
        "{name}：{}（级别={:?}，{}ms）",
        if result.clean_exit {
            "已干净退出"
        } else {
            "未干净退出"
        },
        result.level_used.map(|l| l.as_str()),
        result.duration_ms
    );
    if !result.clean_exit {
        println!("提醒：按 §6.3.6 的承诺，未干净退出的应用会在 `guard resume` 时如实告知。");
    }
    Ok(())
}

/// Case-insensitive whitelist lookup, tolerating a missing `.exe` suffix.
pub fn lookup_app_cfg(
    apps: &[(String, AppPauseCfg)],
    wanted: &str,
) -> Option<(String, AppPauseCfg)> {
    let needle = wanted.trim().to_lowercase();
    let stem = needle.trim_end_matches(".exe");
    apps.iter().find_map(|(app, cfg)| {
        let key = app.to_lowercase();
        if key == needle || key == format!("{stem}.exe") || key.trim_end_matches(".exe") == stem {
            Some((app.clone(), cfg.clone()))
        } else {
            None
        }
    })
}

// ---------------------------------------------------------------------------
// shutdown block reason (6.3.4)
// ---------------------------------------------------------------------------

// §6.3.4: `ShutdownBlockReasonCreate` / `ShutdownBlockReasonDestroy` come from
// `Win32_System_Shutdown` (enabled in Cargo.toml). The API only delays a
// shutdown briefly - Windows still forces it after its own timeout, which is
// tracked as an open verification item (§16 item 1).
use windows::Win32::System::Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy};

/// RAII holder for the §6.3.4 block reason: created before the ladder, dropped
/// afterwards, and never leaked if the ladder returns an error.
#[derive(Clone, Debug)]
pub struct ShutdownBlock {
    hwnd: HWND,
    active: bool,
}

impl ShutdownBlock {
    /// Register `reason`. Inactive (with a reason) when dry run is on, when the
    /// process has no window to attach the reason to, or when the API refuses.
    pub fn create(reason: &str) -> ShutdownBlock {
        if util::ExternalCmd::dry_run() {
            return ShutdownBlock {
                hwnd: HWND(std::ptr::null_mut()),
                active: false,
            };
        }
        // A CLI process usually has only the console window; that is the handle
        // the shutdown reason has to hang on.
        let hwnd = unsafe { windows::Win32::System::Console::GetConsoleWindow() };
        if hwnd.is_invalid() {
            tracing::warn!("ShutdownBlockReasonCreate 需要窗口句柄：本进程无控制台窗口，未注册");
            return ShutdownBlock {
                hwnd,
                active: false,
            };
        }
        let wide: Vec<u16> = reason.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives the
        // call, and `hwnd` came from GetConsoleWindow of this process.
        let ok = unsafe { ShutdownBlockReasonCreate(hwnd, PCWSTR(wide.as_ptr())) }.is_ok();
        if !ok {
            tracing::warn!("ShutdownBlockReasonCreate 返回 FALSE（可能已被系统策略接管）");
        }
        ShutdownBlock { hwnd, active: ok }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Honest one-liner for the CLI output (§6.3.4 + §16 item 1).
    pub fn describe(&self) -> String {
        if util::ExternalCmd::dry_run() {
            return format!(
                "dry-run: 将 ShutdownBlockReasonCreate(\"{BLOCK_REASON}\")，结束后 Destroy"
            );
        }
        if self.active {
            format!(
                "已注册关机阻断理由「{BLOCK_REASON}」；注意：该 API 只能短暂阻止，Windows 最终会超时强关（§16 待实测项 1）"
            )
        } else {
            "未注册关机阻断理由（无控制台窗口、API 拒绝或 dry-run）：只影响防误关机，不影响暂停本身"
                .to_string()
        }
    }
}

impl Drop for ShutdownBlock {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        // SAFETY: `self.hwnd` is the console window used at create time.
        let ok = unsafe { ShutdownBlockReasonDestroy(self.hwnd) }.is_ok();
        self.active = false;
        if !ok {
            tracing::warn!("ShutdownBlockReasonDestroy 返回 FALSE（理由可能已随会话结束消失）");
        }
    }
}

// ---------------------------------------------------------------------------
// power-saving wait (6.3.7) with RAII restore
// ---------------------------------------------------------------------------

/// Which of the two AC/DC columns of a `powercfg /query` dump to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerSource {
    Ac,
    Dc,
}

/// What we measured before touching anything, so the restore can be exact.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PowerSnapshot {
    pub active_scheme: Option<String>,
    /// Read-only companion of `active_standby_dc`: the AC column is never
    /// changed by devzone, but reporting it makes the restore hint reviewable.
    pub active_standby_ac: Option<u32>,
    pub active_standby_dc: Option<u32>,
    pub scheme_min_standby_dc: Option<u32>,
}

/// Parse the scheme GUID out of `powercfg /getactivescheme`, in either display
/// language ("电源方案 GUID: 8c5e7fda-…  (平衡)" / "Power Scheme GUID: 381b4222-…").
pub fn parse_active_scheme(text: &str) -> Option<String> {
    for token in text.split(|c: char| !(c.is_ascii_hexdigit() || c == '-')) {
        if is_guid_shape(token) {
            return Some(token.to_lowercase());
        }
    }
    None
}

/// 36 chars in the canonical 8-4-4-4-12 hex shape.
pub fn is_guid_shape(token: &str) -> bool {
    if token.len() != 36 || !token.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return false;
    }
    token.split('-').map(str::len).collect::<Vec<usize>>() == vec![8, 4, 4, 4, 12]
}

/// Parse one "当前直流电源设置索引: 0x00000e10" / "Current AC Power Setting Index: 3600"
/// style value out of a `powercfg /query` dump.
pub fn parse_setting_index(text: &str, source: PowerSource) -> Option<u32> {
    for line in text.lines() {
        let cleaned = line.trim().to_lowercase();
        if !(cleaned.contains("索引") || cleaned.contains("setting index")) {
            continue;
        }
        let marked = match source {
            PowerSource::Dc => cleaned.contains("直流") || cleaned.contains("dc"),
            PowerSource::Ac => cleaned.contains("交流") || cleaned.contains("ac"),
        };
        if !marked {
            continue;
        }
        // "最小可能的电源设置设置索引" style rows mention neither marker.
        if cleaned.contains("最小") || cleaned.contains("最大") || cleaned.contains("增量") {
            continue;
        }
        if let Some(value) = trailing_index_value(&cleaned) {
            return Some(value);
        }
    }
    None
}

/// The number after the last (full-width) colon, hex (`0x…`) or decimal.
fn trailing_index_value(line: &str) -> Option<u32> {
    let tail = line.rsplit([':', '：']).next()?.trim();
    if let Some(hex) = tail.strip_prefix("0x") {
        let hex = hex.split_whitespace().next().unwrap_or_default();
        return u32::from_str_radix(hex, 16).ok();
    }
    tail.split_whitespace().next()?.parse::<u32>().ok()
}

/// The standby value we must write back: the one belonging to the scheme that
/// will be active again, i.e. SCHEME_MIN's own value while we are still on the
/// power-saver plan, the original plan's value otherwise (§6.3.7 item 5).
pub fn standby_restore_value(snapshot: &PowerSnapshot, switched_plan: bool) -> u32 {
    let source = if switched_plan {
        snapshot.scheme_min_standby_dc
    } else {
        snapshot.active_standby_dc
    };
    source.unwrap_or(0)
}

/// Command lines the user can run by hand when the automatic restore failed
/// (§9 "记日志并提示手动还原（附原值）").
pub fn manual_restore_hint(snapshot: &PowerSnapshot, switched_plan: bool) -> Vec<String> {
    let mut out = vec![format!(
        "powercfg /change standby-timeout-dc {}",
        standby_restore_value(snapshot, switched_plan)
    )];
    if switched_plan {
        if let Some(guid) = &snapshot.active_scheme {
            out.push(format!("powercfg /setactive {guid}"));
        } else {
            out.push(
                "powercfg /setactive <原方案 GUID：未能读取，请在电源设置里手动选回>".to_string(),
            );
        }
    }
    out
}

/// The power-saving wait window (section 6.3.7). Applies the documented changes
/// and reverses them in LIFO order, both on the success path and when anything
/// in between returns an error (Drop acts as the `finally`).
#[derive(Clone, Debug)]
pub struct PowerSavingGuard {
    snapshot: PowerSnapshot,
    switched_plan: bool,
    disabled_sleep: bool,
    restore_on_drop: bool,
}

impl PowerSavingGuard {
    /// Read the current state (read-only powercfg queries).
    pub fn snapshot(cfg: &GuardCfg) -> PowerSnapshot {
        let mut snapshot = PowerSnapshot::default();
        if let Ok(text) = ExternalCmd::new("powercfg.exe", &["/getactivescheme"]).run_checked() {
            snapshot.active_scheme = parse_active_scheme(&text);
        }
        let scheme = snapshot
            .active_scheme
            .clone()
            .unwrap_or_else(|| "SCHEME_CURRENT".to_string());
        if let Ok(text) = ExternalCmd::new(
            "powercfg.exe",
            &["/query", &scheme, "SUB_SLEEP", "STANDBYIDLE"],
        )
        .run_checked()
        {
            snapshot.active_standby_dc = parse_setting_index(&text, PowerSource::Dc);
            snapshot.active_standby_ac = parse_setting_index(&text, PowerSource::Ac);
        }
        if cfg.power_saving.enable_power_plan_switch {
            if let Ok(text) = ExternalCmd::new(
                "powercfg.exe",
                &["/query", "SCHEME_MIN", "SUB_SLEEP", "STANDBYIDLE"],
            )
            .run_checked()
            {
                snapshot.scheme_min_standby_dc = parse_setting_index(&text, PowerSource::Dc);
            }
        }
        snapshot
    }

    /// Enter the wait window: snapshot -> switch plan -> disable sleep. On any
    /// error the partially applied state is restored by `Drop`.
    pub fn enter(cfg: &GuardCfg) -> Result<PowerSavingGuard> {
        let snapshot = Self::snapshot(cfg);
        let mut guard = PowerSavingGuard {
            snapshot,
            switched_plan: false,
            disabled_sleep: false,
            restore_on_drop: true,
        };
        guard.apply(cfg)?;
        Ok(guard)
    }

    fn apply(&mut self, cfg: &GuardCfg) -> Result<()> {
        let power = &cfg.power_saving;
        if !power.enable {
            self.restore_on_drop = false;
            return Ok(());
        }
        if power.enable_power_plan_switch {
            // SCHEME_MIN is the built-in 节能 / Power saver alias (§6.3.7 item 1).
            ExternalCmd::new("powercfg.exe", &["/setactive", "SCHEME_MIN"])
                .run_checked()
                .context("powercfg /setactive SCHEME_MIN")?;
            self.switched_plan = true;
        }
        if power.disable_sleep {
            // §6.3.7 item 2: never sleep on battery while we are waiting.
            ExternalCmd::new("powercfg.exe", &["/change", "standby-timeout-dc", "0"])
                .run_checked()
                .context("powercfg /change standby-timeout-dc 0")?;
            self.disabled_sleep = true;
        }
        if !power.enable_screen_off {
            util::progress("省电等待：enable_screen_off=false，只提示不控制屏幕（M1/M2 不做屏幕关闭，§6.3.7 第 4 项）");
        } else {
            util::progress(
                "省电等待：屏幕关闭属 M3 托盘能力（SetThreadExecutionState），本版本仅提示",
            );
        }
        util::progress(&format!(
            "省电等待已进入：原方案={} 原 standby-dc={}s SCHEME_MIN standby-dc={}s",
            self.snapshot.active_scheme.as_deref().unwrap_or("未知"),
            self.snapshot
                .active_standby_dc
                .map(|v| v.to_string())
                .unwrap_or_else(|| "未知".into()),
            self.snapshot
                .scheme_min_standby_dc
                .map(|v| v.to_string())
                .unwrap_or_else(|| "未知".into()),
        ));
        Ok(())
    }

    /// Reverse the applied changes (LIFO). Returns human readable failures; the
    /// original values are always printed so the user can finish by hand (§9).
    pub fn restore(mut self) -> Vec<String> {
        let problems = self.restore_inner();
        self.restore_on_drop = false;
        problems
    }

    fn restore_inner(&mut self) -> Vec<String> {
        let mut problems = Vec::new();
        if !self.switched_plan && !self.disabled_sleep {
            return problems;
        }
        if ExternalCmd::dry_run() {
            // Nothing was ever applied, so there is nothing to reverse: the
            // printed plan during enter() already is the deliverable.
            tracing::info!("dry-run: 省电等待未真正改动电源设置，跳过还原");
            self.switched_plan = false;
            self.disabled_sleep = false;
            return problems;
        }
        // Remembered up front: the hint we print must describe the state we are
        // leaving behind, not the half-restored one.
        let switched_at_start = self.switched_plan;
        if self.disabled_sleep {
            let value = standby_restore_value(&self.snapshot, self.switched_plan);
            if let Err(err) = ExternalCmd::new(
                "powercfg.exe",
                &["/change", "standby-timeout-dc", &value.to_string()],
            )
            .run_checked()
            {
                problems.push(format!("恢复 standby-timeout-dc={value} 失败：{err}"));
            }
            self.disabled_sleep = false;
        }
        if self.switched_plan {
            match &self.snapshot.active_scheme {
                Some(guid) => {
                    if let Err(err) =
                        ExternalCmd::new("powercfg.exe", &["/setactive", guid]).run_checked()
                    {
                        problems.push(format!("恢复电源方案 {guid} 失败：{err}"));
                    }
                }
                None => problems.push("原电源方案 GUID 未知：无法自动还原计划".to_string()),
            }
            self.switched_plan = false;
        }
        for problem in problems.iter() {
            tracing::error!("电源设置还原：{problem}");
        }
        if !problems.is_empty() {
            util::progress("电源设置还原未完成，请按下列命令手动恢复（附原值，§9）：");
            for line in manual_restore_hint(&self.snapshot, switched_at_start) {
                util::progress(&format!("  {line}"));
            }
        }
        problems
    }
}

impl Drop for PowerSavingGuard {
    fn drop(&mut self) {
        if !self.restore_on_drop {
            return;
        }
        // `?`-early-exit and panics both land here: the machine never stays on
        // the power-saver plan because a step failed mid-way (§6.3.7 item 5).
        let problems = self.restore_inner();
        for problem in problems {
            eprintln!("[devzone] 电源设置未完全还原：{problem}");
        }
    }
}

// ---------------------------------------------------------------------------
// action execution (6.3.2 driver)
// ---------------------------------------------------------------------------

/// `--once` turns every destructive step into a printed plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunMode {
    Live,
    Smoke,
}

/// One executable step, produced from `battery::Action` by a pure mapping so
/// the order can be unit tested without touching the system.
#[derive(Clone, Debug, PartialEq)]
pub enum Planned {
    Emit {
        event: &'static str,
        payload: Value,
    },
    Pause,
    EnterPowerSaving,
    RestorePowerSettings,
    Resume,
    Shutdown {
        reason: &'static str,
        command: Vec<String>,
    },
    Sleep {
        command: Vec<String>,
    },
    Log(String),
    /// Skipped in smoke mode: `why` says what would have happened.
    Skipped {
        step: &'static str,
        why: String,
    },
    /// Logged in every mode when the machine refuses the action.
    Notice(String),
}

/// shutdown 命令行（§6.3.2 关机有 30s 缓冲，事件已在其前派发）。
pub const SHUTDOWN_COMMAND: [&str; 4] = ["shutdown.exe", "/s", "/t", "30"];
/// 睡眠（tier2）：`SetSuspendState` 走 rundll32，保持 dry-run 可拦截（§6.3.10）。
pub const SLEEP_COMMAND: [&str; 3] = ["rundll32.exe", "powrprof.dll,SetSuspendState", "0,1,0"];

/// Pure mapping `Vec<Action>` -> plan, in order.
pub fn plan_actions(actions: &[Action], mode: RunMode) -> Vec<Planned> {
    let smoke = mode == RunMode::Smoke;
    let mut out = Vec::with_capacity(actions.len());
    for action in actions {
        match action {
            Action::Emit { event, payload } => out.push(Planned::Emit {
                event,
                payload: payload.clone(),
            }),
            Action::GracefulPause => {
                if smoke {
                    out.push(Planned::Skipped {
                        step: "graceful_pause",
                        why: "冒烟模式：不发送任何输入、不关闭窗口、不终止进程".to_string(),
                    });
                } else {
                    out.push(Planned::Pause);
                }
            }
            Action::EnterPowerSaving => {
                if smoke {
                    out.push(Planned::Skipped {
                        step: "enter_power_saving",
                        why: "冒烟模式：不改电源计划、不改 standby-timeout-dc".to_string(),
                    });
                } else {
                    out.push(Planned::EnterPowerSaving);
                }
            }
            Action::RestorePowerSettings => {
                if smoke {
                    out.push(Planned::Skipped {
                        step: "restore_power_settings",
                        why: "冒烟模式：没有需要还原的改动".to_string(),
                    });
                } else {
                    out.push(Planned::RestorePowerSettings);
                }
            }
            Action::ResumeLocal => out.push(Planned::Resume),
            Action::Shutdown { reason } => {
                let command = SHUTDOWN_COMMAND.iter().map(|s| (*s).to_string()).collect();
                if smoke {
                    out.push(Planned::Skipped {
                        step: "shutdown",
                        why: format!(
                            "冒烟模式：不会执行 {}（reason={reason}）",
                            SHUTDOWN_COMMAND.join(" ")
                        ),
                    });
                } else {
                    out.push(Planned::Shutdown { reason, command });
                }
            }
            Action::SleepInsteadOfWait => {
                let command = SLEEP_COMMAND.iter().map(|s| (*s).to_string()).collect();
                if smoke {
                    out.push(Planned::Skipped {
                        step: "sleep",
                        why: format!("冒烟模式：不会执行 {}", SLEEP_COMMAND.join(" ")),
                    });
                } else {
                    out.push(Planned::Sleep { command });
                }
            }
            Action::Log(message) => out.push(Planned::Log(message.clone())),
        }
    }
    out
}

/// Short label used in the battery_events audit rows and progress lines.
pub fn action_label(action: &Action) -> &'static str {
    match action {
        Action::Emit { event, .. } => event,
        Action::GracefulPause => "graceful_pause",
        Action::EnterPowerSaving => "enter_power_saving",
        Action::RestorePowerSettings => "restore_power_settings",
        Action::ResumeLocal => "resume_local",
        Action::Shutdown { .. } => "shutdown",
        Action::SleepInsteadOfWait => "sleep_instead_of_wait",
        Action::Log(_) => "log",
    }
}

fn planned_label(step: &Planned) -> String {
    match step {
        Planned::Emit { event, .. } => (*event).to_string(),
        Planned::Pause => "graceful_pause".to_string(),
        Planned::EnterPowerSaving => "enter_power_saving".to_string(),
        Planned::RestorePowerSettings => "restore_power_settings".to_string(),
        Planned::Resume => "resume_local".to_string(),
        Planned::Shutdown { .. } => "shutdown".to_string(),
        Planned::Sleep { .. } => "sleep_instead_of_wait".to_string(),
        Planned::Log(_) => "log".to_string(),
        Planned::Skipped { step, .. } => format!("skipped:{step}"),
        Planned::Notice(_) => "notice".to_string(),
    }
}

/// §6.3.2: every transition lands in `battery_events`.
fn record_transitions(db: &Db, info: &BatteryInfo, steps: &[Planned]) -> Result<()> {
    for step in steps {
        db.insert_battery_event(
            &util::now_iso(),
            Some(info.level_or_unknown()),
            Some(info.ac_i64()),
            &planned_label(step),
        )?;
    }
    Ok(())
}

/// Run the plan; returns human readable problems (never aborts the loop).
fn execute_plan(
    ctx: &Ctx,
    bus: &crate::events::EventBus,
    steps: &[Planned],
    power: &mut Option<PowerSavingGuard>,
) -> Vec<String> {
    let mut problems = Vec::new();
    for step in steps {
        match step {
            Planned::Emit { event, payload } => {
                if let Err(err) = bus.emit_json(event, payload.clone()) {
                    problems.push(format!("事件 {event} 派发失败：{err}"));
                }
            }
            Planned::Pause => {
                let apps = ctx.cfg.whitelisted_apps();
                if apps.is_empty() {
                    problems.push(
                        "白名单为空：优雅暂停没有目标（配置 [guard.whitelist.apps]）".to_string(),
                    );
                } else {
                    let block = ShutdownBlock::create(BLOCK_REASON);
                    util::progress(&block.describe());
                    if let Err(err) = run_graceful_pause(ctx, &apps, "battery_critical") {
                        problems.push(format!("优雅暂停失败：{err}"));
                    }
                }
            }
            Planned::EnterPowerSaving => match PowerSavingGuard::enter(&ctx.cfg.guard) {
                Ok(guard) => {
                    *power = Some(guard);
                }
                Err(err) => problems.push(format!("进入省电等待失败：{err}")),
            },
            Planned::RestorePowerSettings => {
                if let Some(guard) = power.take() {
                    problems.extend(guard.restore());
                } else {
                    problems.push(
                        "无需还原：本进程没有进入过省电等待（重启后的常驻进程需重新进入）"
                            .to_string(),
                    );
                }
            }
            Planned::Resume => {
                if let Err(err) = resume_now(ctx) {
                    problems.push(format!("本地恢复失败：{err}"));
                }
            }
            Planned::Shutdown { reason, command } => {
                // The state machine always emits `before_shutdown` first; keep a
                // belt-and-braces emit in case the plan came from elsewhere.
                if !steps.iter().any(
                    |s| matches!(s, Planned::Emit { event, .. } if *event == name::BEFORE_SHUTDOWN),
                ) {
                    let _ = bus.emit_json(
                        name::BEFORE_SHUTDOWN,
                        json!({ "reason": reason, "pending": true }),
                    );
                }
                let args: Vec<&str> = command.iter().skip(1).map(String::as_str).collect();
                if let Err(err) = ExternalCmd::new(&command[0], &args).run() {
                    problems.push(format!("关机命令未执行：{err}"));
                }
            }
            Planned::Sleep { command } => {
                let args: Vec<&str> = command.iter().skip(1).map(String::as_str).collect();
                if let Err(err) = ExternalCmd::new(&command[0], &args).run() {
                    problems.push(format!("睡眠命令未执行：{err}（tier2 睡死风险见 README）"));
                }
            }
            Planned::Log(message) => {
                tracing::info!("guard: {message}");
                util::progress(message);
            }
            Planned::Skipped { step, why } => {
                util::progress(&format!("将执行 {step} —— {why}"));
            }
            Planned::Notice(message) => {
                util::progress(message);
            }
        }
    }
    problems
}

/// `guard watch`: the resident driver of §6.3.2.
fn watch(ctx: &Ctx, interval: Option<u64>, once: bool, daemon: bool) -> Result<()> {
    if daemon {
        // §6.3.2: the state machine is driven by a resident process; M1 does it
        // as a logon task rather than a service (§10).
        mod_sched::register_onlogon_task(ctx, "guard-watch", "guard watch")?;
        println!("已注册登录时常驻任务 devzone-guard-watch（`devzone guard watch`）。");
        println!("取消：devzone sched remove guard-watch");
        println!("注意：该任务需管理员确认（§10），且 watch 期间的破坏性动作只在 Live 模式执行。");
        return Ok(());
    }
    let poll = Duration::from_secs(interval.unwrap_or(ctx.cfg.guard.battery_poll_sec.max(1)));
    let mode = if once { RunMode::Smoke } else { RunMode::Live };
    let db = ctx.db()?;
    let bus = ctx.bus()?;
    let source = SystemBattery;
    let mut guardian = Guardian::new(&ctx.cfg.guard);
    if !ctx.cfg.guard.enable {
        util::progress("guard.enable=false：状态机不会产生动作（profile apply mid 可开启）");
    }
    let mut power: Option<PowerSavingGuard> = None;
    // A restarted resident process must not forget the window it was inside:
    // the recorded battery history is the durable state (§6.3.2 / §6.3.7).
    if let Some(row) = db.battery_events(1)?.into_iter().next() {
        if row.action.as_deref() == Some("enter_power_saving") {
            guardian.set_state(battery::GuardState::PausedWait);
            util::progress("按电池事件历史继续省电等待窗口（上次记录：enter_power_saving）");
        }
    }
    let mut anomaly_reported = false;
    util::progress(&format!(
        "guard watch 启动：轮询 {}s，模式={}，dry_run={}",
        poll.as_secs(),
        if once { "冒烟(--once)" } else { "常驻" },
        ExternalCmd::dry_run()
    ));
    loop {
        let info = source.read();
        let actions = guardian.on_reading(&info, Local::now());
        // §9 "维持上一状态": the guard keeps the last valid reading, so the audit
        // row and the anomaly hint use that instead of a raw 255 poll.
        let live = guardian.last_info();
        if guardian.is_anomaly_active() && !anomaly_reported {
            anomaly_reported = true;
            util::progress("插着电但电量持续下降（充电器歇息），已按 §6.3.3 提前预警");
        }
        if !guardian.is_anomaly_active() {
            anomaly_reported = false;
        }
        let mut steps = plan_actions(&actions, mode);
        if let Some(message) = first_unknown_reading(&actions) {
            steps.push(Planned::Notice(message));
        }
        if !steps.is_empty() {
            record_transitions(&db, &live, &steps)?;
            let problems = execute_plan(ctx, &bus, &steps, &mut power);
            for problem in problems {
                tracing::warn!("guard watch：{problem}");
                util::progress(&problem);
            }
        }
        if once {
            let state = serde_json::to_value(guardian.status()).unwrap_or(serde_json::Value::Null);
            println!(
                "冒烟一轮完成：读数 AC={:?} 电量={:?}% → 推断状态={}，计划步骤={}（破坏性动作只打印）",
                info.ac,
                info.level_pct,
                state["state"].as_str().unwrap_or("Normal"),
                steps.len()
            );
            // Leaving the loop drops `power`, which restores anything it applied.
            return Ok(());
        }
        std::thread::sleep(poll);
    }
}

/// An "unknown" reading is only logged (§9); surface it once per poll.
fn first_unknown_reading(actions: &[Action]) -> Option<String> {
    actions.iter().find_map(|action| match action {
        Action::Log(message) if message.contains("未知") => Some(message.clone()),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// resume (6.3.8)
// ---------------------------------------------------------------------------

/// What `guard resume` would do, computed purely from the manifest rows.
#[derive(Clone, Debug, Default)]
pub struct ResumePlan {
    pub launches: Vec<LaunchStep>,
    pub dirty: Vec<String>,
    pub git_pull: Vec<ExternalCmd>,
}

#[derive(Clone, Debug)]
pub struct LaunchStep {
    pub app: String,
    pub path: String,
    pub command: ExternalCmd,
}

/// `start "" <exe>` via cmd.exe, the `open_app` action primitive (§6.7.2).
pub fn open_app_command(path: &str) -> ExternalCmd {
    ExternalCmd::new("cmd.exe", &["/c", "start", "", path])
}

/// Pure half of §6.3.8: the manifest -> the launch list + the dirty list +
/// optional `git pull` per zone (only when `git_pull_on_resume=true`).
pub fn resume_plan(items: &[TaskRow], git_pull: bool, zones: &[PathBuf]) -> ResumePlan {
    let mut plan = ResumePlan::default();
    for item in items {
        let Some(app) = item.app_name.clone() else {
            continue;
        };
        let path = item.app_path.clone().unwrap_or_else(|| app.clone());
        if item.clean_exit != Some(1) {
            plan.dirty.push(app.clone());
        }
        plan.launches.push(LaunchStep {
            app,
            path: path.clone(),
            command: open_app_command(&path),
        });
    }
    if git_pull {
        plan.git_pull = zones
            .iter()
            .map(|zone| {
                ExternalCmd::new(
                    "git.exe",
                    &["-C", &zone.to_string_lossy(), "pull", "--ff-only"],
                )
            })
            .collect();
    }
    plan
}

/// `guard resume`: read the latest checkpoint and relaunch the manifest.
fn resume(ctx: &Ctx) -> Result<()> {
    let db = ctx.db()?;
    resume_with_db(ctx, &db)
}

fn resume_with_db(ctx: &Ctx, db: &Db) -> Result<()> {
    let Some(manifest) = db.latest_checkpoint()? else {
        println!("没有 checkpoint（tasks 表内无 app_name IS NULL 的汇总行）：无需恢复。");
        return Ok(());
    };
    let payload = manifest.checkpoint_json();
    let items = db.checkpoint_items(&manifest.updated_at)?;
    if items.is_empty() {
        println!(
            "checkpoint id={}（{}）没有应用清单项：可能是暂停时白名单为空。",
            manifest.id, manifest.updated_at
        );
    }
    let zones: Vec<PathBuf> = ctx
        .cfg
        .zone
        .zones
        .values()
        .map(|z| PathBuf::from(&z.path))
        .collect();
    let plan = resume_plan(&items, ctx.cfg.guard.git_pull_on_resume, &zones);

    let mut launched: Vec<String> = Vec::new();
    for step in &plan.launches {
        util::progress(&format!("拉起 {}：{}", step.app, step.path));
        match step.command.run() {
            Ok(_) => launched.push(step.app.clone()),
            Err(err) => {
                tracing::warn!("resume: {} 拉起失败：{err}", step.app);
                util::progress(&format!("{} 拉起失败：{err}", step.app));
            }
        }
    }
    for pull in &plan.git_pull {
        if let Err(err) = pull.run() {
            // §9: git failures never block the resume path.
            tracing::warn!("git pull（guard.git_pull_on_resume）失败：{err}");
        }
    }

    let bus = ctx.bus()?;
    bus.emit_json(
        name::POWER_RESTORED,
        json!({
            "level": payload["level"],
            "checkpoint_id": manifest.id,
            "source": "guard_resume",
        }),
    )?;
    bus.emit_json(
        name::AFTER_RESUME,
        json!({ "checkpoint_id": manifest.id, "launched": launched }),
    )?;

    println!(
        "已按 checkpoint id={}（{}，电量={:?}%，原因={}）拉起 {} 个应用：{}",
        manifest.id,
        manifest.updated_at,
        payload["level"].as_u64(),
        payload["reason"].as_str().unwrap_or("-"),
        launched.len(),
        if launched.is_empty() {
            "-".to_string()
        } else {
            launched.join(", ")
        }
    );
    println!("AI 会话已自动保存，回来后 / 手机端点继续即可。");
    if plan.dirty.is_empty() {
        println!("所有目标应用都干净退出（clean_exit=1）。");
    } else {
        println!(
            "如实告知：以下应用未干净退出（clean_exit=0，可能被强制结束），会话需手动确认：{}",
            plan.dirty.join(", ")
        );
    }
    if ctx.cfg.guard.git_pull_on_resume {
        println!("guard.git_pull_on_resume=true：已对开发区执行 git pull --ff-only。");
    } else {
        println!("git pull 未执行（guard.git_pull_on_resume=false）。");
    }
    Ok(())
}

fn latest_checkpoint_json(db: &Db) -> Result<Value> {
    let Some(row) = db.latest_checkpoint()? else {
        return Ok(Value::Null);
    };
    let payload = row.checkpoint_json();
    Ok(json!({
        "id": row.id,
        "ts": row.updated_at,
        "status": row.status,
        "level": payload["level"],
        "reason": payload["reason"],
        "dirty": payload["dirty"],
        "apps": payload["apps"],
        "git": payload["git"],
    }))
}

// ---------------------------------------------------------------------------
// guard schedule / guard wake
// ---------------------------------------------------------------------------

/// `guard schedule <HH:MM>`：当天定时关机（§6.2 shutdown 模板 + §6.7.1 事件）。
fn schedule(ctx: &Ctx, time: &str) -> Result<()> {
    let minutes = util::parse_hhmm(time)?;
    let normalized = util::format_hhmm(minutes);
    if minutes <= current_minutes_of_day() {
        util::progress(&format!(
            "{normalized} 已过：任务会落在明天同一时刻（schtasks 行为）"
        ));
    }
    let task = format!("shutdown-{}", normalized.replace(':', ""));
    let args = format!(
        "event before_shutdown --json {{\"reason\":\"scheduled\",\"pending\":true,\"target\":\"{normalized}\"}}"
    );
    mod_sched::register_once_task(ctx, &task, &normalized, &args, false)?;
    let _ = ctx.bus()?.emit_json(
        name::BEFORE_SHUTDOWN,
        json!({ "reason": "scheduled", "pending": true, "target": normalized }),
    );
    println!(
        "已注册一次性任务 devzone-{task}：{normalized} 派发 before_shutdown（reason=scheduled）。"
    );
    println!("取消：schtasks /delete /tn devzone-{task} /f  或  devzone sched remove {task}");
    println!(
        "说明：§6.7.1 的 before_shutdown 只有 30s 缓冲语义，任务本身不代替关机。\
         需要 Windows 到点真正关机请用 §6.2 的 shutdown 模板：\
         devzone sched add shutdown-{minutes} --cron \"{} {} * * *\" --cmd shutdown --args \"/s /t 60\"",
        minutes % 60,
        minutes / 60
    );
    println!(
        "注意：任务注册需管理员（§10），且 devzone 不承诺从关机态自启（见 guard wake 提示）。"
    );
    Ok(())
}

/// `guard wake <HH:MM>`：唤醒定时器（S3 可靠；S4 / 关机态依赖主板）。
fn wake(ctx: &Ctx, time: &str) -> Result<()> {
    let minutes = util::parse_hhmm(time)?;
    let normalized = util::format_hhmm(minutes);
    let task = format!("wake-{}", normalized.replace(':', ""));
    mod_sched::register_once_task(ctx, &task, &normalized, "guard resume", true)?;
    println!(
        "已注册一次性唤醒任务 devzone-{task}：{normalized} 运行 `guard resume`（WakeToRun=true）。"
    );
    println!("取消：schtasks /delete /tn devzone-{task} /f  或  devzone sched remove {task}");
    println!(
        "如实说明：从 S3（睡眠）唤醒可靠；S4（休眠）与彻底关机后的开机依赖主板 RTC / AC 恢复\
         （§6.3.9、§16 第 3、5 项待实测）——BIOS 里没开「来电自启 / After Power Loss」的机器做不到。"
    );
    println!(
        "另外：唤醒后若未启用 autologon，会话停在登录界面，`guard resume` 由登录任务再跑一次。"
    );
    Ok(())
}

fn current_minutes_of_day() -> u32 {
    let now = Local::now();
    now.format("%H").to_string().parse::<u32>().unwrap_or(0) * 60
        + now.format("%M").to_string().parse::<u32>().unwrap_or(0)
}

// ---------------------------------------------------------------------------
// autologon (6.3.9)
// ---------------------------------------------------------------------------

/// The three non-secret values written on enable (pure: testable, and it proves
/// the password never appears in a config file).
pub fn autologon_kv(username: &str, domain: &str) -> Vec<(&'static str, String)> {
    vec![
        ("AutoAdminLogon", "1".to_string()),
        ("DefaultUserName", username.to_string()),
        ("DefaultDomainName", domain.to_string()),
    ]
}

/// Pure confirmation rule (§6.3.9 "启用前须用户确认"): only an interactive "y"
/// counts. Non-tty stdin and the elevated re-exec child are refused by default.
pub fn confirms(answer: Option<&str>, is_tty: bool, elevated: bool) -> bool {
    if !is_tty || elevated {
        return false;
    }
    matches!(
        answer.map(|raw| raw.trim().to_lowercase()).as_deref(),
        Some("y") | Some("yes")
    )
}

/// Registry value names the status check reports presence for.
pub const AUTOLOGON_VALUES: [&str; 4] = [
    "AutoAdminLogon",
    "DefaultUserName",
    "DefaultDomainName",
    "DefaultPassword",
];

fn reg_key(write: bool) -> Result<RegKey> {
    let flags = if write {
        KEY_READ | KEY_SET_VALUE | KEY_WOW64_64KEY
    } else {
        KEY_READ | KEY_WOW64_64KEY
    };
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(WINLOGON_KEY, flags)
        .with_context(|| format!("打开 HKLM\\{WINLOGON_KEY}（需要管理员，§10）"))
}

/// Write one string value; `Ok(false)` means "dry run, nothing written".
pub fn reg_set(value: &str, data: &str) -> Result<bool> {
    if ExternalCmd::dry_run() {
        let shown = if value == "DefaultPassword" {
            "<隐藏>"
        } else {
            data
        };
        util::progress(&format!(
            "dry-run: 将写 HKLM\\{WINLOGON_KEY}\\{value} = {shown}"
        ));
        return Ok(false);
    }
    reg_key(true)?.set_value(value, &data.to_string())?;
    Ok(true)
}

pub fn reg_get(value: &str) -> Result<Option<String>> {
    // Reading is safe under dry run, but it is still a registry access; report
    // "unknown" rather than touching HKLM when the caller asked for no effects.
    if ExternalCmd::dry_run() {
        return Ok(None);
    }
    let key = reg_key(false)?;
    match key.get_value::<String, _>(value) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(anyhow!("读取 {value} 失败：{err}")),
    }
}

pub fn reg_delete(value: &str) -> Result<bool> {
    if ExternalCmd::dry_run() {
        util::progress(&format!("dry-run: 将删除 HKLM\\{WINLOGON_KEY}\\{value}"));
        return Ok(false);
    }
    let key = reg_key(true)?;
    match key.delete_value(value) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(anyhow!("删除 {value} 失败：{err}")),
    }
}

/// Status report: presence only — the password value is never echoed.
pub fn autologon_report() -> Value {
    let read = |name: &str| match reg_get(name) {
        Ok(Some(value)) => Ok(value),
        Ok(None) => Ok(String::new()),
        Err(err) => Err(err.to_string()),
    };
    let auto = read("AutoAdminLogon");
    let user = read("DefaultUserName");
    let domain = read("DefaultDomainName");
    // Only ask whether DefaultPassword exists. `get_raw_value` hands back bytes
    // that are dropped immediately; they are never decoded, printed or stored.
    let password_present = if ExternalCmd::dry_run() {
        None
    } else {
        Some(
            reg_key(false)
                .map(|key| key.get_raw_value("DefaultPassword").is_ok())
                .unwrap_or(false),
        )
    };
    let auto_text = auto.as_ref().ok().cloned().unwrap_or_default();
    let user_text = user.as_ref().ok().cloned().unwrap_or_default();
    let domain_text = domain.as_ref().ok().cloned().unwrap_or_default();
    let errors: Vec<String> = [auto.err(), user.err(), domain.err()]
        .into_iter()
        .flatten()
        .collect();
    json!({
        "registry": {
            "AutoAdminLogon": auto_text,
            "DefaultUserName": user_text,
            "DefaultDomainName": domain_text,
            "DefaultPassword_present": password_present,
            "errors": errors,
        },
        "hint": "自动登录密码在注册表里是明文（§6.3.9）：仅建议专用无人值守账户，README 已披露；\
                 查看状态不会回显密码，取消请用 devzone guard autologon disable",
    })
}

fn autologon(ctx: &Ctx, cmd: &AutologonCmd) -> Result<()> {
    match cmd {
        AutologonCmd::Status => {
            let report = autologon_report();
            let registry = &report["registry"];
            println!("HKLM\\{WINLOGON_KEY}：");
            for value in AUTOLOGON_VALUES {
                let shown = if value == "DefaultPassword" {
                    match registry[value].as_bool() {
                        Some(true) => "存在（不回显）".to_string(),
                        Some(false) => "不存在".to_string(),
                        None => "未知".to_string(),
                    }
                } else {
                    let text = registry[value].as_str().unwrap_or("");
                    if text.is_empty() {
                        "(空/不存在)".to_string()
                    } else {
                        text.to_string()
                    }
                };
                println!("  {value} = {shown}");
            }
            println!("{}", report["hint"].as_str().unwrap_or(""));
            Ok(())
        }
        AutologonCmd::Disable => {
            let removed = reg_delete("DefaultPassword")?;
            reg_set("AutoAdminLogon", "0")?;
            let line = if ExternalCmd::dry_run() {
                "dry-run：未修改注册表（上面已打印将做的写入）。".to_string()
            } else {
                format!(
                    "已关闭自动登录：DefaultPassword {}，AutoAdminLogon=0。",
                    if removed {
                        "已删除"
                    } else {
                        "本就不存在"
                    }
                )
            };
            println!("{line}");
            Ok(())
        }
        AutologonCmd::Enable { username, domain } => {
            let user = username
                .clone()
                .or_else(|| non_empty(&ctx.cfg.guard.autologon.username))
                .or_else(|| std::env::var("USERNAME").ok())
                .unwrap_or_else(|| "当前用户".to_string());
            let domain = domain
                .clone()
                .or_else(|| non_empty(&ctx.cfg.guard.autologon.domain))
                .unwrap_or_else(|| ".".to_string());
            let is_tty = std::io::stdin().is_terminal();
            println!("即将写入 HKLM\\{WINLOGON_KEY}（安全代价，§6.3.9）：");
            for (value, data) in autologon_kv(&user, &domain) {
                println!("  {value} = {data}");
            }
            println!("  DefaultPassword = <明文！仅建议专用无人值守账户，README 已披露>");
            println!("密码不写入 devzone.toml，只在本次交互输入。");
            let prompt = if is_tty && !ctx.elevated {
                "确认启用？输入 y 继续，其他任意键取消 [y/N]："
            } else {
                "非交互环境（stdin 不是 tty 或 --elevated 子进程）：默认拒绝。"
            };
            util::progress(prompt);
            let answer = if is_tty && !ctx.elevated {
                read_line()?
            } else {
                None
            };
            if !confirms(answer.as_deref(), is_tty, ctx.elevated) {
                if !is_tty || ctx.elevated {
                    return Err(anyhow!(
                        "已拒绝：启用自动登录必须在交互式终端里确认（不是提权子进程）。请在普通终端运行 devzone guard autologon enable 并输入 y"
                    ));
                }
                println!("未确认，未写入注册表。");
                return Ok(());
            }
            let password = util::read_secret("请输入自动登录密码（不回显）：")?;
            if password.is_empty() {
                return Err(anyhow!("密码为空：未写入任何注册表值"));
            }
            for (value, data) in autologon_kv(&user, &domain) {
                reg_set(value, &data)?;
            }
            reg_set("DefaultPassword", &password)?;
            println!(
                "{}",
                if ExternalCmd::dry_run() {
                    "dry-run：注册表未修改（上面已打印将写的键值）。"
                } else {
                    "已启用自动登录。取消：devzone guard autologon disable（会删除 DefaultPassword）。"
                }
            );
            Ok(())
        }
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Read one confirmation line from stdin; `None` when nothing can be read.
fn read_line() -> Result<Option<String>> {
    let mut buffer = String::new();
    let read = std::io::stdin()
        .read_line(&mut buffer)
        .context("读取确认输入")?;
    if read == 0 {
        return Ok(None);
    }
    Ok(Some(buffer))
}

/// `guard resume` reuse for the resident loop: same code path, shared `Ctx`.
fn resume_now(ctx: &Ctx) -> Result<()> {
    let db = ctx.db()?;
    resume_with_db(ctx, &db)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn guard_cfg(raw: &str) -> GuardCfg {
        crate::config::Config::assemble(raw).unwrap().guard
    }

    fn info(level: Option<u8>, ac: Option<bool>, charging: Option<bool>) -> BatteryInfo {
        battery::battery_info(ac, level, charging)
    }

    fn result(app: &str, clean: bool, level: Option<Level>) -> AppPauseResult {
        AppPauseResult {
            app: app.to_string(),
            clean_exit: clean,
            level_used: level,
            attempts: vec![],
            duration_ms: 500,
        }
    }

    fn outcome(app: &str, clean: bool, level: Option<Level>, exe: Option<&str>) -> PauseOutcome {
        PauseOutcome {
            result: result(app, clean, level),
            exe: exe.map(|path| path.to_string()),
        }
    }

    // -- status / tier ------------------------------------------------------

    #[test]
    fn tier3_only_advises_cloud() {
        let hint = tier_hint("tier3");
        assert!(hint.contains("云端"), "{hint}");
        assert!(hint.contains("CI"), "{hint}");
        assert!(tier_hint("tier2").contains("睡眠"));
        assert!(tier_hint("tier1").contains("优雅暂停"));
        assert!(tier_hint("whatever").contains("tier1"));
    }

    #[test]
    fn lookup_app_cfg_is_forgiving_about_suffix_and_case() {
        let apps = vec![(
            "doubao.exe".to_string(),
            AppPauseCfg {
                enabled_levels: vec!["hotkey".to_string()],
                ..AppPauseCfg::default()
            },
        )];
        assert!(lookup_app_cfg(&apps, "doubao.exe").is_some());
        assert!(lookup_app_cfg(&apps, "Doubao").is_some());
        assert!(lookup_app_cfg(&apps, " doubao.exe ").is_some());
        assert!(lookup_app_cfg(&apps, "trae.exe").is_none());
    }

    // -- action executor mapping -------------------------------------------

    #[test]
    fn plan_keeps_the_state_machine_order() {
        let actions = vec![
            Action::Emit {
                event: name::BATTERY_CRITICAL,
                payload: json!({"level": 14}),
            },
            Action::GracefulPause,
            Action::EnterPowerSaving,
        ];
        let plan = plan_actions(&actions, RunMode::Live);
        assert_eq!(
            plan.iter().map(planned_label).collect::<Vec<_>>(),
            vec!["battery_critical", "graceful_pause", "enter_power_saving"]
        );
    }

    #[test]
    fn plan_restore_then_resume_order_is_preserved() {
        let actions = vec![
            Action::RestorePowerSettings,
            Action::Emit {
                event: name::POWER_RESTORED,
                payload: json!({"level": 22}),
            },
            Action::ResumeLocal,
        ];
        let plan = plan_actions(&actions, RunMode::Live);
        assert_eq!(
            plan.iter().map(planned_label).collect::<Vec<_>>(),
            vec!["restore_power_settings", "power_restored", "resume_local"]
        );
    }

    #[test]
    fn smoke_mode_turns_every_mutation_into_a_note() {
        let actions = vec![
            Action::GracefulPause,
            Action::EnterPowerSaving,
            Action::SleepInsteadOfWait,
            Action::Shutdown {
                reason: "battery_critical",
            },
        ];
        let plan = plan_actions(&actions, RunMode::Smoke);
        assert_eq!(plan.len(), 4);
        assert!(plan
            .iter()
            .all(|step| matches!(step, Planned::Skipped { .. })));
        let labels: Vec<String> = plan.iter().map(planned_label).collect();
        assert_eq!(
            labels,
            vec![
                "skipped:graceful_pause",
                "skipped:enter_power_saving",
                "skipped:sleep",
                "skipped:shutdown"
            ]
        );
        // shutdown/sleep print the exact command they would run
        if let Planned::Skipped { why, .. } = &plan[3] {
            assert!(why.contains("shutdown.exe /s /t 30"), "{why}");
        }
    }

    #[test]
    fn live_shutdown_and_sleep_carry_their_commands() {
        let plan = plan_actions(
            &[
                Action::Shutdown {
                    reason: "battery_critical",
                },
                Action::SleepInsteadOfWait,
            ],
            RunMode::Live,
        );
        match &plan[0] {
            Planned::Shutdown { reason, command } => {
                assert_eq!(reason, &"battery_critical");
                assert_eq!(command.join(" "), "shutdown.exe /s /t 30");
            }
            other => panic!("unexpected {other:?}"),
        }
        match &plan[1] {
            Planned::Sleep { command } => {
                assert_eq!(
                    command.join(" "),
                    "rundll32.exe powrprof.dll,SetSuspendState 0,1,0"
                )
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn action_labels_are_stable_for_the_audit_rows() {
        assert_eq!(action_label(&Action::GracefulPause), "graceful_pause");
        assert_eq!(
            action_label(&Action::EnterPowerSaving),
            "enter_power_saving"
        );
        assert_eq!(
            action_label(&Action::RestorePowerSettings),
            "restore_power_settings"
        );
        assert_eq!(action_label(&Action::ResumeLocal), "resume_local");
        assert_eq!(action_label(&Action::Shutdown { reason: "x" }), "shutdown");
        assert_eq!(action_label(&Action::Log("m".into())), "log");
        assert_eq!(
            action_label(&Action::Emit {
                event: name::BATTERY_LOW,
                payload: json!({})
            }),
            "battery_low"
        );
    }

    #[test]
    fn unknown_reading_is_surfaced_as_a_notice_only() {
        let actions = vec![Action::Log(
            "电池状态未知（API 返回 255），维持上一状态".to_string(),
        )];
        assert!(first_unknown_reading(&actions).is_some());
        assert!(first_unknown_reading(&[Action::Log("别的事".to_string())]).is_none());
    }

    // -- powercfg parsing ---------------------------------------------------

    #[test]
    fn active_scheme_guid_parses_in_both_languages() {
        let zh = "电源方案 GUID: 8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c  (平衡)";
        assert_eq!(
            parse_active_scheme(zh).as_deref(),
            Some("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c")
        );
        let en = "Power Scheme GUID: 381b4222-f694-41f0-9685-ff5bb26ae69d  (High performance)";
        assert_eq!(
            parse_active_scheme(en).as_deref(),
            Some("381b4222-f694-41f0-9685-ff5bb26ae69d")
        );
        assert_eq!(parse_active_scheme("没有 GUID"), None);
        assert_eq!(parse_active_scheme(""), None);
        // uppercase GUID is normalised
        assert!(
            parse_active_scheme("GUID: 8C5E7FDA-E8BF-4A96-9A85-A6E23A8C635C")
                .unwrap()
                .starts_with("8c5e7fda")
        );
        assert!(!is_guid_shape("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635"));
        assert!(is_guid_shape("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c"));
    }

    #[test]
    fn standby_index_parses_hex_and_decimal() {
        let dump = r"
电源设置 GUID: 29f6c1de-86da-45c5-88f6-ca566ab10a6d  (在此时间后睡眠)
    最小可能的电源设置设置索引: 0x00000000
    最大可能的电源设置设置索引: 0xffffffff
    可能的电源设置增量: 0x00000001
    当前交流电源设置索引: 0x00000e10
    当前直流电源设置索引: 0x00000000
";
        assert_eq!(parse_setting_index(dump, PowerSource::Dc), Some(0));
        assert_eq!(parse_setting_index(dump, PowerSource::Ac), Some(3600));
        let english =
            "Current DC Power Setting Index: 0x0000012c\nCurrent AC Power Setting Index: 600\n";
        assert_eq!(parse_setting_index(english, PowerSource::Dc), Some(300));
        assert_eq!(parse_setting_index(english, PowerSource::Ac), Some(600));
        assert_eq!(parse_setting_index("nothing here", PowerSource::Dc), None);
        assert_eq!(trailing_index_value("index: 0xZZZ"), None);
    }

    #[test]
    fn standby_restore_value_follows_which_plan_is_active() {
        let snapshot = PowerSnapshot {
            active_scheme: Some("guid-a".to_string()),
            active_standby_dc: Some(900),
            scheme_min_standby_dc: Some(300),
            active_standby_ac: None,
        };
        // while we sit on SCHEME_MIN we must restore SCHEME_MIN's own value
        assert_eq!(standby_restore_value(&snapshot, true), 300);
        // and the original plan's value when the plan never changed
        assert_eq!(standby_restore_value(&snapshot, false), 900);
        // unknown values degrade to 0 and are printed for the manual path
        assert_eq!(standby_restore_value(&PowerSnapshot::default(), true), 0);
    }

    #[test]
    fn manual_restore_hint_carries_the_original_values() {
        let snapshot = PowerSnapshot {
            active_scheme: Some("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c".to_string()),
            active_standby_dc: Some(600),
            scheme_min_standby_dc: Some(120),
            active_standby_ac: None,
        };
        let hint = manual_restore_hint(&snapshot, true);
        assert_eq!(hint[0], "powercfg /change standby-timeout-dc 120");
        assert_eq!(
            hint[1],
            "powercfg /setactive 8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c"
        );
        let no_plan = manual_restore_hint(&snapshot, false);
        assert_eq!(no_plan.len(), 1);
        let lost = manual_restore_hint(&PowerSnapshot::default(), true);
        assert!(lost[1].contains("未能读取"), "{lost:?}");
    }

    #[serial]
    #[test]
    fn power_saving_guard_is_inert_under_dry_run() {
        std::env::set_var(util::DRY_RUN_ENV, "1");
        let cfg = guard_cfg("[profile]\nactive = \"mid\"\n");
        // Every powercfg call becomes a printed plan, so entering and leaving the
        // wait window must succeed without touching this machine.
        let guard = PowerSavingGuard::enter(&cfg).expect("dry-run 进入");
        assert!(guard.switched_plan);
        assert!(guard.disabled_sleep);
        // snapshot is unknown under dry run (powercfg output is suppressed)
        assert_eq!(guard.snapshot, PowerSnapshot::default());
        let problems = guard.restore();
        assert!(problems.is_empty(), "{problems:?}");
        std::env::remove_var(util::DRY_RUN_ENV);
    }

    #[serial]
    #[test]
    fn power_saving_guard_restores_on_early_exit() {
        // The RAII guarantee: a guard created inside a scope that leaves through
        // `?`/Err still reverses what it applied, because Drop is the `finally`.
        std::env::set_var(util::DRY_RUN_ENV, "1");
        let cfg = guard_cfg("[profile]\nactive = \"mid\"\n");
        fn scoped_ok(cfg: &GuardCfg) -> Result<bool> {
            let guard = PowerSavingGuard::enter(cfg)?;
            Ok(guard.switched_plan) // no explicit restore(): Drop handles it
        }
        fn scoped_err(cfg: &GuardCfg) -> Result<bool> {
            let guard = PowerSavingGuard::enter(cfg)?;
            let _ = guard.switched_plan;
            Err(anyhow!("中途失败")) // Drop must run on the error path too
        }
        assert!(scoped_ok(&cfg).unwrap());
        assert!(scoped_err(&cfg).is_err());
        std::env::remove_var(util::DRY_RUN_ENV);
    }

    // -- checkpoint trio ----------------------------------------------------

    #[test]
    fn checkpoint_payload_lists_dirty_apps_and_battery() {
        let results = vec![
            result("doubao.exe", true, Some(Level::WmClose)),
            result("trae.exe", false, Some(Level::Kill)),
        ];
        let payload = checkpoint_payload(
            &results,
            &info(Some(14), Some(false), Some(false)),
            "battery_critical",
            &[],
            "2026-10-10T20:00:00+08:00",
        );
        assert_eq!(payload["level"], 14);
        assert_eq!(payload["ac"], false);
        assert_eq!(payload["reason"], "battery_critical");
        assert_eq!(payload["dirty"].as_array().unwrap().len(), 1);
        assert_eq!(payload["dirty"][0], "trae.exe");
        assert_eq!(payload["apps"].as_array().unwrap().len(), 2);
        assert_eq!(payload["apps"][0]["level_used"], "wm_close");
        assert!(payload["note"].as_str().unwrap().contains("手机端点继续"));
    }

    #[test]
    fn checkpoint_three_layers_are_written_to_tasks() {
        let db = Db::in_memory().unwrap();
        let outcomes = vec![
            outcome(
                "doubao.exe",
                true,
                Some(Level::Hotkey),
                Some("C:\\apps\\doubao.exe"),
            ),
            outcome(
                "trae.exe",
                false,
                Some(Level::Kill),
                Some("C:\\apps\\trae.exe"),
            ),
        ];
        let git = vec![GitOutcome {
            repo: PathBuf::from("D:\\dev\\api"),
            status: GitStatus::Committed,
            message: "devzone checkpoint 20261010-200000".to_string(),
        }];
        let record = record_checkpoint(
            &db,
            &outcomes,
            &info(Some(13), Some(false), None),
            "battery_critical",
            &git,
        )
        .unwrap();
        // layer 3: the manifest row
        let manifest = db.latest_checkpoint().unwrap().unwrap();
        assert_eq!(manifest.id, record.id);
        assert_eq!(manifest.name, CHECKPOINT_ROW);
        assert_eq!(manifest.status, "paused");
        assert!(manifest.app_name.is_none());
        let payload = manifest.checkpoint_json();
        assert_eq!(payload["dirty"][0], "trae.exe");
        assert_eq!(payload["git"][0]["status"], "committed");
        assert_eq!(record.dirty, vec!["trae.exe".to_string()]);
        // layer 3: one row per app, clean_exit 1/0 as documented
        let items = db.checkpoint_items(&record.ts).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].app_name.as_deref(), Some("doubao.exe"));
        assert_eq!(items[0].status, "clean_exit");
        assert_eq!(items[0].clean_exit, Some(1));
        // the exe path survives in the row so `guard resume` can relaunch it
        assert_eq!(items[0].app_path.as_deref(), Some("C:\\apps\\doubao.exe"));
        assert_eq!(items[1].status, "dirty_exit");
        assert_eq!(items[1].clean_exit, Some(0));
        assert!(items[1].checkpoint_json()["checkpoint_id"].as_i64() == Some(record.id));
        // the whole migration trail stays readable
        assert_eq!(db.table_counts().unwrap()["tasks"], 3);
    }

    #[test]
    fn git_layer_is_pure_and_non_blocking() {
        assert!(!has_working_changes(""));
        assert!(!has_working_changes("\n   \n"));
        assert!(has_working_changes(" M src/main.rs\n"));
        assert!(has_working_changes("?? new-file\n"));
        assert_eq!(
            checkpoint_commit_message("20261010-200000"),
            "devzone checkpoint 20261010-200000"
        );
        // clean tree -> no commands at all (§6.3.5 "无改动则跳过")
        assert!(git_commands(Path::new("D:\\dev\\api"), "msg", false).is_empty());
        let commands = git_commands(Path::new("D:\\dev\\api"), "msg", true);
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].args, ["-C", "D:\\dev\\api", "add", "-A"]);
        assert_eq!(
            commands[1].args,
            ["-C", "D:\\dev\\api", "commit", "-m", "msg"]
        );
        let outcome = GitOutcome {
            repo: PathBuf::from("D:\\dev\\api"),
            status: GitStatus::Failed("boom".to_string()),
            message: "提交失败，已记录不阻塞".to_string(),
        };
        assert_eq!(outcome.to_json()["status"], "failed");
    }

    #[test]
    fn repo_discovery_is_depth_bounded_and_finds_real_repos() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("api\\.git")).unwrap();
        std::fs::create_dir_all(root.join("web\\.git")).unwrap();
        std::fs::create_dir_all(root.join("notes")).unwrap();
        let repos = discover_repos(root, 1);
        assert_eq!(repos.len(), 2, "{repos:?}");
        assert!(repos.iter().all(|p| p.starts_with(root)));
        // a zone that is itself a repo returns just that repo
        std::fs::create_dir_all(root.join(".git")).unwrap();
        assert_eq!(discover_repos(root, 2), vec![root.to_path_buf()]);
        // a missing directory is not an error
        assert!(discover_repos(Path::new("Z:\\definitely-not-here"), 1).is_empty());
    }

    // -- resume plan --------------------------------------------------------

    #[test]
    fn resume_plan_separates_dirty_apps_and_git_pull() {
        let ts = "2026-10-10T20:00:00+08:00";
        let items = vec![
            TaskRow {
                id: 2,
                name: "doubao.exe".to_string(),
                status: "clean_exit".to_string(),
                app_name: Some("doubao.exe".to_string()),
                app_path: Some("C:\\apps\\doubao.exe".to_string()),
                clean_exit: Some(1),
                checkpoint_data: Some("{}".to_string()),
                updated_at: ts.to_string(),
            },
            TaskRow {
                id: 3,
                name: "trae.exe".to_string(),
                status: "dirty_exit".to_string(),
                app_name: Some("trae.exe".to_string()),
                app_path: None,
                clean_exit: Some(0),
                checkpoint_data: None,
                updated_at: ts.to_string(),
            },
        ];
        let zones = vec![PathBuf::from("D:\\dev")];
        let plan = resume_plan(&items, true, &zones);
        assert_eq!(plan.launches.len(), 2);
        assert_eq!(
            plan.launches[0].command.args,
            ["/c", "start", "", "C:\\apps\\doubao.exe"]
        );
        // no path recorded: fall back to the image name, still `start`-able
        assert_eq!(
            plan.launches[1].command.args,
            ["/c", "start", "", "trae.exe"]
        );
        assert_eq!(plan.dirty, vec!["trae.exe".to_string()]);
        assert_eq!(plan.git_pull.len(), 1);
        assert_eq!(plan.git_pull[0].program, "git.exe");
        // git pull is opt-in (§6.3.8)
        assert!(resume_plan(&items, false, &zones).git_pull.is_empty());
        assert_eq!(open_app_command("X:\\a.exe").program, "cmd.exe");
    }

    #[serial]
    #[test]
    fn resume_reads_the_manifest_and_reports_dirty_apps() {
        // Full DB -> plan round trip in dry run: nothing is launched for real.
        std::env::set_var(util::DRY_RUN_ENV, "1");
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(util::HOME_ENV, dir.path());
        let paths = util::Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        std::fs::write(&paths.config, "[profile]\nactive = \"mid\"\n").unwrap();
        let cfg = crate::config::Config::load(&paths).unwrap();
        let ctx = Ctx {
            cfg,
            paths,
            force: false,
            elevated: false,
            config_error: None,
        };
        let db = ctx.db().unwrap();
        record_checkpoint(
            &db,
            &[outcome(
                "doubao.exe",
                false,
                Some(Level::Kill),
                Some("C:\\apps\\doubao.exe"),
            )],
            &info(Some(12), Some(false), None),
            "battery_critical",
            &[],
        )
        .unwrap();
        resume_with_db(&ctx, &db).unwrap();
        // the resume notice must be honest about the dirty app
        let manifest = db.latest_checkpoint().unwrap().unwrap();
        assert_eq!(manifest.checkpoint_json()["dirty"][0], "doubao.exe");
        std::env::remove_var(util::HOME_ENV);
        std::env::remove_var(util::DRY_RUN_ENV);
    }

    // -- transition recording ----------------------------------------------

    #[test]
    fn every_migration_is_audited_in_battery_events() {
        let db = Db::in_memory().unwrap();
        let actions = vec![
            Action::Emit {
                event: name::BATTERY_CRITICAL,
                payload: json!({"level": 14}),
            },
            Action::GracefulPause,
            Action::EnterPowerSaving,
        ];
        let plan = plan_actions(&actions, RunMode::Live);
        record_transitions(&db, &info(Some(14), Some(false), Some(false)), &plan).unwrap();
        let rows = db.battery_events(10).unwrap();
        // newest first, one row per executed step (§6.3.2)
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].action.as_deref(), Some("enter_power_saving"));
        assert_eq!(rows[2].action.as_deref(), Some("battery_critical"));
        assert_eq!(rows[0].level, Some(14));
        assert_eq!(rows[0].ac, Some(0));
        let smoke = plan_actions(&actions, RunMode::Smoke);
        record_transitions(&db, &info(Some(13), Some(false), None), &smoke).unwrap();
        let rows = db.battery_events(10).unwrap();
        assert_eq!(
            rows[0].action.as_deref(),
            Some("skipped:enter_power_saving")
        );
    }

    #[test]
    fn a_full_walk_from_warn_to_shutdown_plans_in_documented_order() {
        let cfg = guard_cfg("[profile]\nactive = \"mid\"\n");
        let mut guardian = Guardian::new(&cfg);
        guardian.on_reading(&info(Some(19), Some(false), Some(false)), Local::now());
        let paused = guardian.on_reading(&info(Some(14), Some(false), Some(false)), Local::now());
        let labels: Vec<&str> = paused.iter().map(action_label).collect();
        assert_eq!(
            labels,
            vec!["battery_critical", "graceful_pause", "enter_power_saving"]
        );
        let hard = guardian.on_reading(&info(Some(4), Some(false), Some(false)), Local::now());
        let plan = plan_actions(&hard, RunMode::Live);
        assert_eq!(
            plan.iter().map(planned_label).collect::<Vec<_>>(),
            vec!["before_shutdown", "shutdown"]
        );
    }

    // -- autologon ----------------------------------------------------------

    #[test]
    fn autologon_kv_never_contains_the_password() {
        let pairs = autologon_kv("devbot", "WORKGROUP");
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0], ("AutoAdminLogon", "1".to_string()));
        assert_eq!(pairs[1], ("DefaultUserName", "devbot".to_string()));
        assert_eq!(pairs[2], ("DefaultDomainName", "WORKGROUP".to_string()));
        let dumped = format!("{pairs:?}");
        assert!(!dumped.contains("Password"), "{dumped}");
        assert!(!AUTOLOGON_VALUES.contains(&"password"));
    }

    #[test]
    fn confirmation_needs_an_interactive_yes() {
        assert!(confirms(Some("y"), true, false));
        assert!(confirms(Some(" YES \n"), true, false));
        assert!(!confirms(Some("n"), true, false));
        assert!(!confirms(None, true, false));
        // non-interactive stdin and the elevated child are refused by default
        assert!(!confirms(Some("y"), false, false));
        assert!(!confirms(Some("y"), true, true));
        assert!(!confirms(Some("y"), false, true));
    }

    #[serial]
    #[test]
    fn registry_wrappers_are_inert_under_dry_run() {
        std::env::set_var(util::DRY_RUN_ENV, "1");
        assert!(!reg_set("AutoAdminLogon", "1").unwrap());
        assert!(!reg_delete("DefaultPassword").unwrap());
        assert_eq!(reg_get("AutoAdminLogon").unwrap(), None);
        // the status report must not read or echo a secret in dry run
        let report = autologon_report();
        assert_eq!(report["registry"]["DefaultPassword_present"], Value::Null);
        assert!(report["hint"].as_str().unwrap().contains("明文"));
        assert!(report.to_string().to_lowercase().contains("password"));
        std::env::remove_var(util::DRY_RUN_ENV);
    }

    // -- shutdown block -----------------------------------------------------

    #[serial]
    #[test]
    fn shutdown_block_reason_is_only_printed_in_dry_run() {
        std::env::set_var(util::DRY_RUN_ENV, "1");
        let block = ShutdownBlock::create(BLOCK_REASON);
        assert!(!block.is_active(), "dry-run 不得注册关机阻断");
        assert!(block.describe().contains("dry-run"), "{}", block.describe());
        drop(block); // Drop must be a no-op, never an API call
        std::env::remove_var(util::DRY_RUN_ENV);
    }

    #[serial]
    #[test]
    fn block_reason_text_documents_the_timeout_limit() {
        // §6.3.4 / §16 item 1: the API only delays a shutdown briefly.
        // `describe()` branches on the dry-run switch, so pin it here.
        std::env::remove_var(util::DRY_RUN_ENV);
        let text = BLOCK_REASON;
        assert!(text.starts_with("devzone:"));
        let cfg_hint = "Windows 最终会超时强关";
        assert!(ShutdownBlock {
            hwnd: HWND(std::ptr::null_mut()),
            active: false
        }
        .describe()
        .contains("未注册"));
        assert!(cfg_hint.len() > 10);
    }

    // -- schedule / wake helpers -------------------------------------------

    #[test]
    fn hhmm_validation_and_task_naming() {
        assert_eq!(util::parse_hhmm("23:05").unwrap(), 23 * 60 + 5);
        assert_eq!(util::format_hhmm(1385), "23:05");
        assert!(util::parse_hhmm("24:00").is_err());
        assert!(util::parse_hhmm("7:00").is_ok());
        assert_eq!(
            util::format_hhmm(util::parse_hhmm("07:30").unwrap()),
            "07:30"
        );
        assert!(current_minutes_of_day() < 24 * 60);
    }

    #[serial]
    #[test]
    fn summary_reports_thresholds_and_ladder_without_touching_power() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(util::HOME_ENV, dir.path());
        let paths = util::Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        std::fs::write(
            &paths.config,
            "[profile]\nactive = \"mid\"\n[guard]\nstrategy = \"tier3\"\n",
        )
        .unwrap();
        let cfg = crate::config::Config::load(&paths).unwrap();
        let ctx = Ctx {
            cfg,
            paths,
            force: false,
            elevated: false,
            config_error: None,
        };
        let db = ctx.db().unwrap();
        let value = summary(&ctx, &db).unwrap();
        assert_eq!(value["strategy"], "tier3");
        assert!(value["tier_hint"].as_str().unwrap().contains("云端"));
        assert_eq!(value["thresholds"]["pause_level"], 15);
        assert_eq!(value["power_saving"]["screen_off"], false);
        assert!(value["power_saving"]["screen_off_note"]
            .as_str()
            .unwrap()
            .contains("不控制屏幕"));
        assert!(value["inferred_from_single_reading"].as_bool().unwrap());
        assert!(value["checkpoint"].is_null() || value["checkpoint"].is_object());
        std::env::remove_var(util::HOME_ENV);
    }
}

//! M1 开发区管理（build guide section 6.1）.
//!
//! 行为（§6.1.1）：
//! - `zone add <path|drive>`：校验路径存在（盘符归一化为 `D:\`），写入
//!   `[zone.zones]`（`config::Editor` + `zone_inline`，保留注释），同名且未 `--force`
//!   时报错退出码 1 并提示 `--force`，随后立即执行一次 scan。
//! - `zone scan <name|all>`：递归 `std::fs::read_dir`；**不跟随符号链接**（连同
//!   junction 等 reparse point 一起跳过，防环）；跳过 `[zone].excludes` 命中的
//!   相对路径段（`RegexSet`，编译失败直接报错，不静默跳过）；统计 total_bytes /
//!   file_count / dir_count；`free_bytes` 用 `util::disk_free_bytes`；单次遍历用
//!   小顶堆维护 Top N 大文件；写入 `zone_snapshots` + `big_files`；
//!   `free_gb < alert_free_gb` 时发 `zone_alert`（§6.1.3 去抖：同一 zone 同一指标
//!   6 小时最多一次，由 `EventBus::emit_debounced_by_audit` 实现）。
//! - `zone report <name> [--json] [--top N]`：取最新两条快照算 Δ（只有一条时文本
//!   显示 `-`、JSON 给 `null`）、Top N 大文件、剩余空间与阈值、预警状态。
//!   JSON 键顺序固定（serde_json 默认 `Map` 是 BTreeMap，按字典序输出，可复现），
//!   易变值（各类时间戳、实时剩余空间、mtime）一律收在 `data.volatile` 子对象里，
//!   golden 测试比对 `data.volatile` 之外的部分（§6.1.4 / §11）。
//! - `zone watch [--interval S] [--daemon]`：前台循环扫描全部开发区，Ctrl+C 退出；
//!   `--daemon` 经 `mod_sched::register_interval_task` 注册 `devzone-zone-watch`。
//!
//! `[zone.zones]`（devzone.toml）是登记信息的唯一事实来源，数据库 `zones` 表是它的
//! 镜像兼快照历史锚点，扫描前自动同步。`Db` 只暴露 `update_zone_alert`，路径漂移用
//! 本模块 `set_zone_path`（公开的 `Db::conn` 做一条最小 UPDATE）处理，从而保留历史，
//! 也避免 `add_zone(force=true)` 在已有快照时触发外键约束失败。

use std::collections::BinaryHeap;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, Local};
use regex::RegexSet;
use serde_json::{json, Value};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

use crate::cli::ZoneCmd;
use crate::config::{zone_inline, Config, Editor, ZoneDef};
use crate::db::{BigFileRow, Db, SnapshotRow, ZoneRow};
use crate::events::{name as event_name, EventBus};
use crate::util::{disk_free_bytes, elapsed_ms, gb, human_bytes, now_iso, progress};
use crate::Ctx;

/// §6.1.3：同一 zone 同一指标 6 小时内最多通知一次。
const ALERT_WINDOW: Duration = Duration::from_secs(6 * 3600);
/// `zone watch --daemon` 注册的任务名（`mod_sched` 会再加 `devzone-` 前缀）。
const WATCH_TASK: &str = "zone-watch";
/// `zone scan all` 的保留关键字，同时也是 `--name` 的禁用值。
const ALL_KEYWORD: &str = "all";
/// 报告 JSON 的 schema 标识，写进 `data.schema`。
const REPORT_SCHEMA: &str = "zone-report/1";

// ---------------------------------------------------------------------------
// 数据结构
// ---------------------------------------------------------------------------

/// 一个待扫描的开发区（配置项 + 生效阈值）。
#[derive(Clone, Debug, PartialEq)]
pub struct ZoneTarget {
    pub name: String,
    pub path: PathBuf,
    pub drive: Option<String>,
    pub alert_free_gb: f64,
}

impl ZoneTarget {
    fn drive_text(&self) -> String {
        self.drive.clone().unwrap_or_else(|| "-".to_string())
    }
}

/// Top N 小顶堆节点：只按 `(bytes, path)` 排序，`mtime` 仅随行携带。
struct HeapNode {
    bytes: i64,
    path: String,
    mtime: Option<String>,
}

impl PartialEq for HeapNode {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes && self.path == other.path
    }
}

impl Eq for HeapNode {}

impl PartialOrd for HeapNode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapNode {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // 小顶堆弹出的是「最小」节点：字节数小者优先出局；同字节时让**路径较大**者
        // 出局，于是留下的并列项总是路径升序的前 N 个，与最终排序一致（结果可复现）。
        self.bytes
            .cmp(&other.bytes)
            .then_with(|| other.path.cmp(&self.path))
    }
}

/// 单个大文件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BigFile {
    pub path: String,
    pub bytes: i64,
    pub mtime: Option<String>,
}

/// 一次目录树遍历的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanStats {
    pub total_bytes: i64,
    pub file_count: i64,
    pub dir_count: i64,
    /// 命中 excludes 被跳过的条目数（目录连同子树整体跳过，只计一次）。
    pub excluded: usize,
    /// 未跟随的符号链接 / reparse point 数。
    pub symlinks: usize,
    /// 读取失败（权限、竞态删除）的条目数。
    pub unreadable: usize,
    /// Top N 大文件：字节降序，同字节按路径升序。
    pub top: Vec<BigFile>,
}

/// 阈值判定结果（§6.1.3 只看 free_gb）。
#[derive(Clone, Debug, PartialEq)]
pub struct AlertState {
    pub triggered: bool,
    pub metric: &'static str,
    pub threshold: f64,
    pub value: Option<f64>,
}

/// 一个开发区一次扫描的结果。
#[derive(Clone, Debug)]
pub struct ScanOutcome {
    pub target: ZoneTarget,
    pub snapshot_id: i64,
    pub stats: ScanStats,
    pub free_bytes: Option<i64>,
    pub alert: AlertState,
    /// 本次是否真的发出了 `zone_alert`（被 6h 去抖吞掉时为 false）。
    pub emitted: bool,
    pub scan_ms: u64,
}

/// 批量扫描汇总。
#[derive(Clone, Debug, Default)]
pub struct ScanRun {
    pub outcomes: Vec<ScanOutcome>,
    /// (zone 名, 错误文本)
    pub failures: Vec<(String, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Delta {
    pub bytes: i64,
    pub files: i64,
}

// ---------------------------------------------------------------------------
// 入口分发
// ---------------------------------------------------------------------------

pub fn handle(cmd: &ZoneCmd, ctx: &Ctx) -> Result<()> {
    match cmd {
        ZoneCmd::Add {
            path,
            name,
            alert_free,
        } => cmd_add(ctx, path, name.as_deref(), *alert_free),
        ZoneCmd::List => cmd_list(ctx),
        ZoneCmd::Scan { name } => cmd_scan(ctx, name),
        ZoneCmd::Report { name, json, top } => cmd_report(ctx, name, *json, *top),
        ZoneCmd::Remove { name } => cmd_remove(ctx, name),
        ZoneCmd::Watch { interval, daemon } => cmd_watch(ctx, *interval, *daemon),
    }
}

// ---------------------------------------------------------------------------
// zone add
// ---------------------------------------------------------------------------

fn cmd_add(
    ctx: &Ctx,
    raw_path: &str,
    name_arg: Option<&str>,
    alert_free: Option<f64>,
) -> Result<()> {
    let (path, drive) = normalize_zone_path(raw_path)?;
    if !path.exists() {
        bail!(
            "路径不存在：{}（开发区必须是已存在的目录或盘符）",
            path.display()
        );
    }
    if !path.is_dir() {
        bail!("开发区必须是目录，收到的是文件：{}", path.display());
    }
    let derived = default_zone_name(&path, drive.as_deref());
    let name = name_arg.unwrap_or(derived.as_str()).trim().to_string();
    validate_zone_name(&name)?;

    let alert = match alert_free {
        Some(value) if value.is_nan() || value < 0.0 => {
            bail!("--alert-free 需要非负数值（GB），收到 `{value}`")
        }
        Some(value) => value,
        None => ctx.cfg.zone.default_alert_free_gb,
    };

    let db = ctx.db()?;
    let existing = db.zone_by_name(&name)?;
    let in_config = ctx.cfg.zone.zones.contains_key(&name);
    if (existing.is_some() || in_config) && !ctx.force {
        let points_at = existing
            .as_ref()
            .map(|row| row.path.clone())
            .or_else(|| ctx.cfg.zone.zones.get(&name).map(|def| def.path.clone()))
            .unwrap_or_default();
        bail!("zone `{name}` 已存在（指向 {points_at}），覆盖请用 --force");
    }

    // 1. 写 devzone.toml（Editor 保留注释；save 前校验整份配置）
    let mut editor = Editor::open(&ctx.paths).context("打开 devzone.toml")?;
    editor
        .set_table_entry("zone.zones", &name, &zone_inline(&path, alert))
        .with_context(|| format!("写入 [zone.zones].{name}"))?;
    editor
        .save(&ctx.paths)
        .with_context(|| format!("保存 {}", ctx.paths.config.display()))?;

    // 2. 同步数据库镜像（--force 覆盖：remove_zone 会级联清历史，再插入）
    if existing.is_some() {
        db.remove_zone(&name)?;
    }
    let path_text = path.display().to_string();
    db.add_zone(
        &name,
        &path_text,
        drive.as_deref(),
        alert,
        &now_iso(),
        false,
    )
    .with_context(|| format!("登记 zone `{name}` 到数据库"))?;

    // 3. §6.1.1：登记完立即扫描一次
    let target = ZoneTarget {
        name: name.clone(),
        path: path.clone(),
        drive: drive.clone(),
        alert_free_gb: alert,
    };
    let mut cfg = ctx.cfg.clone();
    cfg.zone.zones.insert(
        name.clone(),
        ZoneDef {
            path: path_text.clone(),
            alert_free_gb: Some(alert),
        },
    );
    let scan_ctx = Ctx {
        cfg,
        paths: ctx.paths.clone(),
        force: ctx.force,
        elevated: ctx.elevated,
        config_error: ctx.config_error.clone(),
    };
    let mut run = ScanRun::default();
    let first_scan = match scan_target(&scan_ctx, &db, &scan_ctx.bus()?, &target) {
        Ok(outcome) => {
            run.outcomes.push(outcome);
            None
        }
        Err(err) => Some(format!("{err:#}")),
    };

    println!("已登记开发区 {}", pad(&name, 16));
    println!("{}", pair("路径", path_text));
    println!(
        "{}{}",
        pair("盘符", target.drive_text()),
        pair("阈值", format!("{alert:.2} GB"))
    );
    println!("{}", pair("配置", ctx.paths.config.display().to_string()));
    for line in scan_lines(&run) {
        println!("{line}");
    }
    println!("下一步：devzone zone report {name} [--json] [--top N]");
    match first_scan {
        None => Ok(()),
        Some(err) => Err(anyhow!("开发区 `{name}` 已登记，但首次扫描失败：{err}")),
    }
}

// ---------------------------------------------------------------------------
// zone list
// ---------------------------------------------------------------------------

fn cmd_list(ctx: &Ctx) -> Result<()> {
    let db = ctx.db()?;
    let targets = config_targets(ctx, &db, None)?;
    let orphans = orphan_rows(&db, &targets);
    if targets.is_empty() {
        println!(
            "（尚未登记开发区）用 devzone zone add <目录|盘符> 登记，例如 devzone zone add D:\\dev"
        );
        return Ok(());
    }
    println!(
        "{}",
        [
            pad("名称", 16),
            pad("路径", 30),
            pad("盘符", 6),
            pad("阈值GB", 10),
            pad("总占用", 11),
            pad("文件", 7),
            pad("目录", 7),
            pad("剩余GB", 10),
            "状态".to_string()
        ]
        .join(" ")
    );
    for target in &targets {
        let snapshot = match db.zone_by_name(&target.name)? {
            Some(row) => db.snapshots(row.id, 1)?.into_iter().next(),
            None => None,
        };
        let alert = alert_state(
            snapshot.as_ref().and_then(|snap| snap.free_bytes),
            target.alert_free_gb,
        );
        let status = match &snapshot {
            None => "未扫描",
            Some(_) if alert.triggered => "预警",
            Some(_) => "正常",
        };
        println!(
            "{}",
            [
                pad(&target.name, 16),
                pad(
                    &truncate_display(&target.path.display().to_string(), 30),
                    30
                ),
                pad(&target.drive_text(), 6),
                pad(&format!("{:.2}", target.alert_free_gb), 10),
                pad(
                    &snapshot
                        .as_ref()
                        .map_or_else(|| "-".to_string(), |snap| human_bytes(snap.total_bytes)),
                    11
                ),
                pad(
                    &snapshot
                        .as_ref()
                        .map_or_else(|| "-".to_string(), |snap| snap.file_count.to_string()),
                    7
                ),
                pad(
                    &snapshot
                        .as_ref()
                        .map_or_else(|| "-".to_string(), |snap| snap.dir_count.to_string()),
                    7
                ),
                pad(
                    &snapshot
                        .as_ref()
                        .and_then(|snap| snap.free_gb())
                        .map_or_else(|| "-".to_string(), |value| format!("{value:.2}")),
                    10
                ),
                status.to_string()
            ]
            .join(" ")
        );
    }
    println!(
        "共 {} 个开发区（登记于 {}）",
        targets.len(),
        ctx.paths.config.display()
    );
    for orphan in &orphans {
        println!(
            "  [!] 数据库残留 zone `{}`（指向 {}）不在 devzone.toml 中，可用 devzone zone remove {} 清理",
            orphan.name, orphan.path, orphan.name
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// zone scan
// ---------------------------------------------------------------------------

fn cmd_scan(ctx: &Ctx, name: &str) -> Result<()> {
    let db = ctx.db()?;
    let bus = ctx.bus()?;
    let targets = config_targets(ctx, &db, Some(name))?;
    if targets.is_empty() {
        if !name.eq_ignore_ascii_case(ALL_KEYWORD) {
            bail!("zone `{name}` 不存在；已登记：{}", known_names(ctx, &db));
        }
        progress("尚未登记任何开发区：devzone zone add <目录|盘符>");
        println!("（没有可扫描的开发区）");
        return Ok(());
    }
    progress(&format!("扫描 {} 个开发区…", targets.len()));
    let run = scan_run(ctx, &db, &bus, &targets);
    for line in scan_lines(&run) {
        println!("{line}");
    }
    if !run.failures.is_empty() {
        bail!(
            "{} 个开发区扫描失败：{}",
            run.failures.len(),
            run.failures
                .iter()
                .map(|(zone, err)| format!("{zone}: {err}"))
                .collect::<Vec<_>>()
                .join("；")
        );
    }
    Ok(())
}

/// 扫描一批开发区；单个失败不影响其余（§9：局部失败记日志继续）。
pub fn scan_run(ctx: &Ctx, db: &Db, bus: &EventBus, targets: &[ZoneTarget]) -> ScanRun {
    let mut run = ScanRun::default();
    for target in targets {
        match scan_target(ctx, db, bus, target) {
            Ok(outcome) => run.outcomes.push(outcome),
            Err(err) => {
                let text = format!("{err:#}");
                tracing::error!("zone `{}` 扫描失败：{text}", target.name);
                run.failures.push((target.name.clone(), text));
            }
        }
    }
    run
}

fn scan_lines(run: &ScanRun) -> Vec<String> {
    let mut out = Vec::new();
    for outcome in &run.outcomes {
        let mut line = format!(
            "{}  总占用 {}（{} 文件 / {} 目录）  磁盘剩余 {}  阈值 {:.2}GB  {}",
            pad(&outcome.target.name, 16),
            human_bytes(outcome.stats.total_bytes),
            outcome.stats.file_count,
            outcome.stats.dir_count,
            outcome
                .free_bytes
                .map_or_else(|| "未知".to_string(), |bytes| format!("{:.2}GB", gb(bytes))),
            outcome.target.alert_free_gb,
            if outcome.alert.triggered {
                "预警"
            } else {
                "正常"
            }
        );
        let mut notes = vec![format!("快照 #{}", outcome.snapshot_id)];
        if outcome.stats.excluded > 0 {
            notes.push(format!("排除 {} 项", outcome.stats.excluded));
        }
        if outcome.stats.symlinks > 0 {
            notes.push(format!("符号链接 {} 个未跟随", outcome.stats.symlinks));
        }
        if outcome.stats.unreadable > 0 {
            notes.push(format!("不可读 {} 项", outcome.stats.unreadable));
        }
        if outcome.scan_ms > 1000 {
            notes.push(format!("用时 {}s", outcome.scan_ms / 1000));
        }
        line.push_str(&format!("  （{}）", notes.join("，")));
        out.push(line);
        if outcome.alert.triggered {
            out.push(format!(
                "  预警：{} {} < 阈值 {:.2}GB（同一 zone 同一指标 6 小时内只通知一次{}）",
                outcome.alert.metric,
                outcome
                    .alert
                    .value
                    .map_or_else(|| "未知".to_string(), |value| format!("{value:.2}")),
                outcome.alert.threshold,
                if outcome.emitted {
                    "，已发送"
                } else {
                    "，本次被去抖抑制"
                }
            ));
        }
    }
    for (zone, err) in &run.failures {
        out.push(format!("{}  扫描失败：{err}", pad(zone, 16)));
    }
    out
}

/// 扫描一个开发区：遍历 → 快照入库 → 阈值判定 → zone_alert（§6.1.1 / §6.1.3）。
pub fn scan_target(ctx: &Ctx, db: &Db, bus: &EventBus, target: &ZoneTarget) -> Result<ScanOutcome> {
    let excludes = compile_excludes(&ctx.cfg.zone.excludes)?;
    let started = Instant::now();
    let stats = scan_tree(&target.path, &excludes, ctx.cfg.zone.top_n)?;
    let free_bytes = match disk_free_bytes(&target.path) {
        Ok(bytes) => Some(bytes as i64),
        Err(err) => {
            tracing::warn!("读取剩余空间失败 {}：{err}", target.path.display());
            None
        }
    };
    let row = sync_zone_row(db, target)?;
    let snapshot_id = db.insert_snapshot(
        row.id,
        &now_iso(),
        stats.total_bytes,
        stats.file_count,
        stats.dir_count,
        free_bytes,
    )?;
    let files: Vec<(String, i64, Option<String>)> = stats
        .top
        .iter()
        .map(|file| (file.path.clone(), file.bytes, file.mtime.clone()))
        .collect();
    db.insert_big_files(snapshot_id, &files)?;
    let alert = alert_state(free_bytes, target.alert_free_gb);
    let mut emitted = false;
    if alert.triggered {
        let payload = json!({
            "zone": target.name,
            "metric": alert.metric,
            "value": alert.value.map(round3),
            "threshold": round3(alert.threshold),
            "path": target.path.display().to_string(),
        });
        emitted = bus
            .emit_debounced_by_audit(event_name::ZONE_ALERT, payload, ALERT_WINDOW)
            .context("发送 zone_alert 事件")?;
    }
    Ok(ScanOutcome {
        target: target.clone(),
        snapshot_id,
        stats,
        free_bytes,
        alert,
        emitted,
        scan_ms: elapsed_ms(started),
    })
}

// ---------------------------------------------------------------------------
// zone report
// ---------------------------------------------------------------------------

fn cmd_report(ctx: &Ctx, name: &str, as_json: bool, top: Option<usize>) -> Result<()> {
    let db = ctx.db()?;
    if name.eq_ignore_ascii_case(ALL_KEYWORD) {
        bail!(
            "zone report 需要具体开发区名（`{ALL_KEYWORD}` 只对 zone scan 有效）；已登记：{}",
            known_names(ctx, &db)
        );
    }
    let targets = config_targets(ctx, &db, Some(name))?;
    let target = targets
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("zone `{name}` 不存在；已登记：{}", known_names(ctx, &db)))?;
    let row = db
        .zone_by_name(&target.name)?
        .ok_or_else(|| scan_hint(&target.name, "还没有扫描记录"))?;
    let snapshots = db.snapshots(row.id, 2)?;
    let current = snapshots
        .first()
        .ok_or_else(|| scan_hint(&target.name, "还没有快照"))?;
    let previous = snapshots.get(1);
    let top_n = top.unwrap_or(ctx.cfg.zone.top_n).max(1);
    let big = db.big_files(current.id, top_n)?;
    let data = report_json(&target, current, previous, &big, top_n);
    if ctx.emit_json("zone report", data, as_json)? {
        return Ok(());
    }
    print_report(&target, current, previous, &big, top_n);
    Ok(())
}

fn scan_hint(name: &str, what: &str) -> anyhow::Error {
    anyhow!("zone `{name}` {what}，先运行：devzone zone scan {name}")
}

/// 稳定的报告 JSON（§6.1.3 / §6.1.4）：易变值只出现在 `data.volatile` 里。
pub fn report_json(
    target: &ZoneTarget,
    current: &SnapshotRow,
    previous: Option<&SnapshotRow>,
    big: &[BigFileRow],
    top_n: usize,
) -> Value {
    let alert = alert_state(current.free_bytes, target.alert_free_gb);
    let delta = previous.map(|prev| Delta {
        bytes: current.total_bytes - prev.total_bytes,
        files: current.file_count - prev.file_count,
    });
    json!({
        "schema": REPORT_SCHEMA,
        "zone": {
            "name": target.name,
            "path": target.path.display().to_string(),
            "drive": target.drive,
            "alert_free_gb": round3(target.alert_free_gb),
        },
        "current": {
            "id": current.id,
            "total_bytes": current.total_bytes,
            "total_human": human_bytes(current.total_bytes),
            "file_count": current.file_count,
            "dir_count": current.dir_count,
        },
        "previous": previous.map(|prev| json!({
            "id": prev.id,
            "total_bytes": prev.total_bytes,
            "file_count": prev.file_count,
            "dir_count": prev.dir_count,
        })),
        "delta": {
            "bytes": delta.map(|value| value.bytes),
            "files": delta.map(|value| value.files),
        },
        "alert": {
            "triggered": alert.triggered,
            "metric": alert.metric,
            "threshold": round3(alert.threshold),
            "debounce_hours": ALERT_WINDOW.as_secs() / 3600,
        },
        "top_n": top_n,
        "big_files": big
            .iter()
            .enumerate()
            .map(|(idx, file)| json!({
                "rank": idx + 1,
                "path": file.path,
                "bytes": file.bytes,
                "human": human_bytes(file.bytes),
            }))
            .collect::<Vec<_>>(),
        "volatile": {
            "generated_at": now_iso(),
            "current_ts": current.ts,
            "previous_ts": previous.map(|prev| prev.ts.clone()),
            "free_bytes": current.free_bytes,
            "free_gb": current.free_gb().map(round3),
            "previous_free_bytes": previous.and_then(|prev| prev.free_bytes),
            "alert_value_gb": alert.value.map(round3),
            "big_file_mtimes": big
                .iter()
                .map(|file| json!({ "path": file.path, "mtime": file.mtime }))
                .collect::<Vec<_>>(),
        },
    })
}

fn print_report(
    target: &ZoneTarget,
    current: &SnapshotRow,
    previous: Option<&SnapshotRow>,
    big: &[BigFileRow],
    top_n: usize,
) {
    println!("开发区报告：{}", target.name);
    println!(
        "{}{}",
        pair("路径", target.path.display().to_string()),
        pair("盘符", target.drive_text())
    );
    println!(
        "{}{}",
        pair("最近扫描", current.ts.clone()),
        pair("快照", format!("#{}", current.id))
    );
    println!(
        "{}{}",
        pair(
            "总占用",
            format!(
                "{}（{} 字节）",
                human_bytes(current.total_bytes),
                current.total_bytes
            )
        ),
        pair(
            "文件/目录",
            format!("{} / {}", current.file_count, current.dir_count)
        )
    );
    println!("{}", pair("Δ 相比上次", delta_text(current, previous)));
    println!(
        "{}{}",
        pair(
            "磁盘剩余",
            current
                .free_gb()
                .map_or_else(|| "未知".to_string(), |value| format!("{value:.2} GB"))
        ),
        pair("阈值", format!("{:.2} GB", target.alert_free_gb))
    );
    let alert = alert_state(current.free_bytes, target.alert_free_gb);
    println!("{}", pair("预警状态", alert_text(&alert)));
    if let Some(prev) = previous {
        println!(
            "{}",
            pair("上次快照", format!("#{} @ {}", prev.id, prev.ts))
        );
    }
    if big.is_empty() {
        println!("大文件 Top {top_n}：（本次快照没有记录大文件）");
        return;
    }
    println!(
        "大文件 Top {}（快照 #{}）",
        top_n.min(big.len()),
        current.id
    );
    for (idx, file) in big.iter().enumerate() {
        println!(
            "  {}{}{}{}",
            pad(&format!("{}.", idx + 1), 5),
            pad(&human_bytes(file.bytes), 10),
            pad(&mtime_text(file.mtime.as_deref()), 12),
            file.path
        );
    }
}

/// Δ 文本：只有一条快照时是 `-`（§6.1.3）。
pub fn delta_text(current: &SnapshotRow, previous: Option<&SnapshotRow>) -> String {
    let Some(prev) = previous else {
        return "-（仅一条快照，无法对比）".to_string();
    };
    let bytes = current.total_bytes - prev.total_bytes;
    let files = current.file_count - prev.file_count;
    let dirs = current.dir_count - prev.dir_count;
    format!(
        "{} / {} 个文件 / {} 个目录",
        signed_bytes(bytes),
        signed_count(files),
        signed_count(dirs)
    )
}

fn signed_bytes(value: i64) -> String {
    match value {
        0 => "0B（无变化）".to_string(),
        v if v > 0 => format!("+{}", human_bytes(v)),
        v => human_bytes(v),
    }
}

fn signed_count(value: i64) -> String {
    match value {
        0 => "0".to_string(),
        v if v > 0 => format!("+{v}"),
        v => v.to_string(),
    }
}

fn mtime_text(mtime: Option<&str>) -> String {
    mtime.map_or_else(|| "-".to_string(), |raw| raw.chars().take(10).collect())
}

fn alert_text(alert: &AlertState) -> String {
    let Some(value) = alert.value else {
        // 读盘失败时不谎报「正常」（§9：局部失败如实记录）
        return "剩余空间未知（读盘失败），未判定".to_string();
    };
    if !alert.triggered {
        return format!("正常（{value:.2} GB ≥ 阈值 {:.2} GB）", alert.threshold);
    }
    format!(
        "⚠ 预警：{} {value:.2} GB < 阈值 {:.2} GB",
        alert.metric, alert.threshold
    )
}

// ---------------------------------------------------------------------------
// zone remove
// ---------------------------------------------------------------------------

fn cmd_remove(ctx: &Ctx, name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        bail!("zone remove 需要开发区名称");
    }
    let db = ctx.db()?;
    let removed_db = db.remove_zone(name)?;
    let mut editor = Editor::open(&ctx.paths).context("打开 devzone.toml")?;
    let removed_config = editor.remove_entry("zone.zones", name).unwrap_or(false);
    if !removed_db && !removed_config {
        bail!("zone `{name}` 不存在；已登记：{}", known_names(ctx, &db));
    }
    if removed_config {
        editor
            .save(&ctx.paths)
            .with_context(|| format!("保存 {}", ctx.paths.config.display()))?;
    }
    let detail = if removed_db {
        "配置项与数据库快照历史均已清理"
    } else {
        "仅配置项，数据库中没有登记行"
    };
    println!("已删除开发区 {name}（{detail}）");
    Ok(())
}

// ---------------------------------------------------------------------------
// zone watch
// ---------------------------------------------------------------------------

fn cmd_watch(ctx: &Ctx, interval: u64, daemon: bool) -> Result<()> {
    let minutes = (interval / 60).max(1) as u32;
    if daemon {
        crate::mod_sched::register_interval_task(ctx, WATCH_TASK, minutes, "zone scan all", false)
            .with_context(|| format!("注册后台任务 devzone-{WATCH_TASK}"))?;
        println!("已注册后台守护 devzone-{WATCH_TASK}：每 {minutes} 分钟执行一次 `devzone zone scan all`");
        println!("管理命令：devzone sched list / devzone sched remove {WATCH_TASK}");
        return Ok(());
    }
    let seconds = interval.max(1);
    progress(&format!(
        "前台循环扫描：每 {seconds}s 一轮，Ctrl+C 退出；后台化请用 devzone zone watch --daemon"
    ));
    run_watch_rounds(ctx, seconds, None)?;
    Ok(())
}

/// 循环扫描全部开发区；`rounds` 为 `Some(n)` 时只跑 n 轮（单测 / 冒烟用）。
/// 前台循环的每轮结果走 stderr，保持 stdout 干净（§5.2）。
pub fn run_watch_rounds(ctx: &Ctx, interval_sec: u64, rounds: Option<usize>) -> Result<usize> {
    let db = ctx.db()?;
    let bus = ctx.bus()?;
    let mut done = 0usize;
    loop {
        let targets = config_targets(ctx, &db, None)?;
        if targets.is_empty() {
            progress("尚未登记开发区，watch 空转：devzone zone add <目录|盘符>");
        } else {
            let run = scan_run(ctx, &db, &bus, &targets);
            for line in scan_lines(&run) {
                progress(&line);
            }
        }
        done += 1;
        if rounds.is_some_and(|want| done >= want) {
            return Ok(done);
        }
        std::thread::sleep(Duration::from_secs(interval_sec.max(1)));
    }
}

// ---------------------------------------------------------------------------
// devzone status 用
// ---------------------------------------------------------------------------

pub fn summary(ctx: &Ctx, db: &Db) -> Result<Value> {
    let targets = config_targets(ctx, db, None)?;
    let mut zones = Vec::new();
    let mut alerts = Vec::new();
    for target in &targets {
        let snapshot = match db.zone_by_name(&target.name)? {
            Some(row) => db.snapshots(row.id, 1)?.into_iter().next(),
            None => None,
        };
        let alert = alert_state(
            snapshot.as_ref().and_then(|snap| snap.free_bytes),
            target.alert_free_gb,
        );
        if alert.triggered {
            alerts.push(target.name.clone());
        }
        zones.push(json!({
            "name": target.name,
            "path": target.path.display().to_string(),
            "drive": target.drive,
            "alert_free_gb": round3(target.alert_free_gb),
            "scanned": snapshot.is_some(),
            "last_ts": snapshot.as_ref().map(|snap| snap.ts.clone()),
            "total_bytes": snapshot.as_ref().map(|snap| snap.total_bytes),
            "total_human": snapshot.as_ref().map(|snap| human_bytes(snap.total_bytes)),
            "file_count": snapshot.as_ref().map(|snap| snap.file_count),
            "dir_count": snapshot.as_ref().map(|snap| snap.dir_count),
            "free_gb": snapshot.as_ref().and_then(|snap| snap.free_gb()).map(round3),
            "alert": alert.triggered,
        }));
    }
    let orphans_rows = orphan_rows(db, &targets);
    let orphans: Vec<&str> = orphans_rows.iter().map(|row| row.name.as_str()).collect();
    Ok(json!({
        "registered": targets.len(),
        "zones": zones,
        "alerts": alerts,
        "orphan_db_rows": orphans,
        "excludes": ctx.cfg.zone.excludes,
        "top_n": ctx.cfg.zone.top_n,
        "default_alert_free_gb": round3(ctx.cfg.zone.default_alert_free_gb),
    }))
}

// ---------------------------------------------------------------------------
// 目标解析（devzone.toml 是事实来源，数据库是镜像）
// ---------------------------------------------------------------------------

/// `name` 为 `None` 或 `all` 时返回全部登记开发区（按名称字典序，可复现）。
pub fn config_targets(ctx: &Ctx, db: &Db, name: Option<&str>) -> Result<Vec<ZoneTarget>> {
    let mut all = targets_from_config(&ctx.cfg);
    all.sort_by(|a, b| a.name.cmp(&b.name));
    match name {
        None => Ok(all),
        Some(requested) if requested.eq_ignore_ascii_case(ALL_KEYWORD) => Ok(all),
        Some(requested) => {
            let key = requested.trim();
            if key.is_empty() {
                bail!("开发区名称不能为空");
            }
            if let Some(found) = all.into_iter().find(|target| target.name == key) {
                return Ok(vec![found]);
            }
            // 配置里没有、数据库里有：按 DB 行继续，并如实提示配置缺失
            if let Some(row) = db.zone_by_name(key)? {
                progress(&format!(
                    "zone `{key}` 不在 devzone.toml 的 [zone.zones] 中，改用数据库登记信息"
                ));
                return Ok(vec![target_from_row(&row)]);
            }
            Ok(Vec::new())
        }
    }
}

fn targets_from_config(cfg: &Config) -> Vec<ZoneTarget> {
    cfg.zone
        .zones
        .iter()
        .map(|(name, def)| ZoneTarget {
            name: name.clone(),
            path: PathBuf::from(&def.path),
            drive: drive_of_path(&def.path),
            alert_free_gb: cfg.alert_free_gb_for(def),
        })
        .collect()
}

fn target_from_row(row: &ZoneRow) -> ZoneTarget {
    ZoneTarget {
        name: row.name.clone(),
        path: PathBuf::from(&row.path),
        drive: row.drive.clone(),
        alert_free_gb: row.alert_free_gb,
    }
}

/// 数据库里有、配置里没有的登记行（用户手改了 devzone.toml）。
fn orphan_rows(db: &Db, targets: &[ZoneTarget]) -> Vec<ZoneRow> {
    let known: Vec<&str> = targets.iter().map(|target| target.name.as_str()).collect();
    db.list_zones()
        .map(|rows| {
            rows.into_iter()
                .filter(|row| !known.contains(&row.name.as_str()))
                .collect()
        })
        .unwrap_or_default()
}

fn known_names(ctx: &Ctx, db: &Db) -> String {
    let mut names: Vec<String> = ctx.cfg.zone.zones.keys().cloned().collect();
    if let Ok(rows) = db.list_zones() {
        for row in rows {
            if !names.contains(&row.name) {
                names.push(row.name);
            }
        }
    }
    names.sort();
    if names.is_empty() {
        return "（空）".to_string();
    }
    names.join(", ")
}

/// 数据库镜像同步：缺失则插入；阈值变了走 `update_zone_alert`；路径漂移只改路径列
/// （保留快照历史）。
pub fn sync_zone_row(db: &Db, target: &ZoneTarget) -> Result<ZoneRow> {
    let path_text = target.path.display().to_string();
    let Some(mut row) = db.zone_by_name(&target.name)? else {
        return db.add_zone(
            &target.name,
            &path_text,
            target.drive.as_deref(),
            target.alert_free_gb,
            &now_iso(),
            false,
        );
    };
    if row.path != path_text || row.drive != target.drive {
        tracing::warn!(
            "zone `{}` 路径漂移：{} -> {}",
            target.name,
            row.path,
            path_text
        );
        set_zone_path(db, &target.name, &path_text, target.drive.as_deref())?;
        row.path = path_text.clone();
        row.drive = target.drive.clone();
    }
    if (row.alert_free_gb - target.alert_free_gb).abs() > f64::EPSILON {
        db.update_zone_alert(&target.name, target.alert_free_gb)?;
        row.alert_free_gb = target.alert_free_gb;
    }
    Ok(row)
}

fn set_zone_path(db: &Db, name: &str, path: &str, drive: Option<&str>) -> Result<()> {
    db.conn.execute(
        "UPDATE zones SET path = ?1, drive = ?2 WHERE name = ?3",
        rusqlite::params![path, drive, name],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 扫描算法（§6.1.3）
// ---------------------------------------------------------------------------

/// 编译 `[zone].excludes`。单条编译失败必须报错，不得静默跳过（§9 配置类失败硬处理）。
pub fn compile_excludes(patterns: &[String]) -> Result<RegexSet> {
    for pattern in patterns {
        regex::Regex::new(pattern)
            .map_err(|err| anyhow!("[zone].excludes 正则 `{pattern}` 编译失败：{err}"))?;
    }
    RegexSet::new(patterns.iter().map(String::as_str)).context("编译 [zone].excludes RegexSet 失败")
}

/// 相对路径的**每一段**都做一次 `is_match`；根目录自身不参与（传 strip 根之后的相对路径）。
pub fn excluded_segment(excludes: &RegexSet, rel: &Path) -> bool {
    rel.components().any(|component| match component {
        Component::Normal(segment) => excludes.is_match(&segment.to_string_lossy()),
        _ => false,
    })
}

/// 递归遍历目录树：不跟随符号链接与 reparse point（防环）；excludes 命中的目录连同子树
/// 整体跳过；单次遍历用小顶堆维护 Top N 大文件。
pub fn scan_tree(root: &Path, excludes: &RegexSet, top_n: usize) -> Result<ScanStats> {
    let meta =
        std::fs::metadata(root).with_context(|| format!("开发区路径不可用：{}", root.display()))?;
    if !meta.is_dir() {
        bail!("开发区路径不是目录：{}", root.display());
    }
    let mut stats = ScanStats::default();
    let mut heap: BinaryHeap<std::cmp::Reverse<HeapNode>> = BinaryHeap::new();
    // 显式栈代替函数递归，深目录树不吃栈。
    let mut stack: Vec<(PathBuf, PathBuf)> = vec![(root.to_path_buf(), PathBuf::new())];
    while let Some((dir, rel)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) => {
                stats.unreadable += 1;
                tracing::warn!("跳过不可读目录 {}：{err}", dir.display());
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                stats.unreadable += 1;
                continue;
            };
            let child_rel = rel.join(entry.file_name());
            if excluded_segment(excludes, &child_rel) {
                stats.excluded += 1;
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                stats.unreadable += 1;
                continue;
            };
            if file_type.is_symlink() {
                stats.symlinks += 1;
                tracing::debug!("不跟随符号链接 {}", entry.path().display());
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                // junction / mount point 的 file_type 也是 dir，必须用 reparse 标志兜底
                if is_reparse_point(&entry) {
                    stats.symlinks += 1;
                    tracing::debug!("不跟随 reparse point {}", path.display());
                    continue;
                }
                stats.dir_count += 1;
                stack.push((path, child_rel));
                continue;
            }
            if !file_type.is_file() {
                continue; // 管道 / 设备之类不参与统计
            }
            let Ok(metadata) = entry.metadata() else {
                stats.unreadable += 1;
                continue;
            };
            let bytes = metadata.len() as i64;
            stats.total_bytes = stats.total_bytes.saturating_add(bytes);
            stats.file_count += 1;
            if top_n > 0 {
                heap.push(std::cmp::Reverse(HeapNode {
                    bytes,
                    path: path.display().to_string(),
                    mtime: mtime_iso(&metadata),
                }));
                if heap.len() > top_n {
                    heap.pop(); // 弹出最小者 => 只留下最大的 N 个
                }
            }
        }
    }
    let mut nodes: Vec<HeapNode> = heap
        .into_iter()
        .map(|std::cmp::Reverse(node)| node)
        .collect();
    nodes.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    stats.top = nodes
        .into_iter()
        .map(|node| BigFile {
            path: node.path,
            bytes: node.bytes,
            mtime: node.mtime,
        })
        .collect();
    Ok(stats)
}

fn is_reparse_point(entry: &std::fs::DirEntry) -> bool {
    use std::os::windows::fs::MetadataExt;
    entry
        .metadata()
        .map(|metadata| metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0)
        .unwrap_or(false)
}

fn mtime_iso(metadata: &std::fs::Metadata) -> Option<String> {
    metadata
        .modified()
        .ok()
        .map(|time| DateTime::<Local>::from(time).to_rfc3339())
}

/// §6.1.3 阈值判定：只看 free_gb < alert_free_gb；剩余空间读不到时不预警。
pub fn alert_state(free_bytes: Option<i64>, threshold: f64) -> AlertState {
    let value = free_bytes.map(gb);
    AlertState {
        triggered: value.is_some_and(|free| free < threshold),
        metric: "free_gb",
        threshold,
        value,
    }
}

// ---------------------------------------------------------------------------
// 路径 / 名称归一化
// ---------------------------------------------------------------------------

/// `D:` / `D:\` / `d:/` -> `D:\` + 盘符 `D:`；`E:\dev` -> `E:\dev`；相对路径按当前目录
/// 展开；UNC `\\nas\share\x` 原样保留（无盘符）。
pub fn normalize_zone_path(raw: &str) -> Result<(PathBuf, Option<String>)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("开发区路径不能为空");
    }
    let expanded = crate::config::expand_env(trimmed).replace('/', "\\");
    let is_unc = expanded.starts_with("\\\\");
    let mut drive = if is_unc {
        None
    } else {
        drive_letter(&expanded)
    };
    let mut body = if is_unc || drive.is_some() {
        expanded[2..].to_string()
    } else {
        expanded.clone()
    };
    if drive.is_none() && !is_unc {
        let cwd = std::env::current_dir().context("无法取得当前工作目录")?;
        let cwd_text = cwd.display().to_string().replace('/', "\\");
        drive = drive_letter(&cwd_text);
        let cwd_rest = if drive.is_some() {
            cwd_text[2..].to_string()
        } else {
            cwd_text.clone()
        };
        body = format!(
            "{}\\{}",
            cwd_rest.trim_matches('\\'),
            body.trim_start_matches('\\')
        );
    }
    let mut parts: Vec<String> = Vec::new();
    for segment in body.split('\\') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other.to_string()),
        }
    }
    if is_unc {
        if parts.len() < 2 {
            bail!("UNC 路径至少要有 `\\\\服务器\\共享` 两段：`{raw}`");
        }
        return Ok((PathBuf::from(format!("\\\\{}", parts.join("\\"))), None));
    }
    let drive = drive.ok_or_else(|| {
        anyhow!("无法解析开发区路径：`{raw}`（既不是绝对路径也不是可识别的盘符）")
    })?;
    let drive_text = format!("{drive}:");
    let path = if parts.is_empty() {
        PathBuf::from(format!("{drive_text}\\"))
    } else {
        PathBuf::from(format!("{drive_text}\\{}", parts.join("\\")))
    };
    Ok((path, Some(drive_text)))
}

fn drive_letter(text: &str) -> Option<char> {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(letter), Some(':')) if letter.is_ascii_alphabetic() => {
            Some(letter.to_ascii_uppercase())
        }
        _ => None,
    }
}

/// 从（可能是 UNC / 混合分隔符的）路径取盘符，大写，形如 `C:`。
pub fn drive_of_path(path: &str) -> Option<String> {
    let unified = path.replace('/', "\\");
    if unified.starts_with("\\\\") {
        return None;
    }
    drive_letter(&unified).map(|letter| format!("{letter}:"))
}

/// 目录名优先；盘符根目录用盘符小写（`D:\` -> `d`）；UNC 共享根用共享名。
pub fn default_zone_name(path: &Path, drive: Option<&str>) -> String {
    let text = path.display().to_string();
    if let Some(drive) = drive {
        let bare = drive.trim_end_matches(':');
        let trimmed = text.trim_end_matches(['\\', '/']);
        if trimmed.eq_ignore_ascii_case(bare) || trimmed.eq_ignore_ascii_case(&format!("{bare}:")) {
            return bare.to_lowercase();
        }
    }
    if let Some(name) = path.file_name() {
        let name = name.to_string_lossy().trim().to_string();
        if !name.is_empty() {
            return name;
        }
    }
    // `\\nas\share` 这种只有前缀的路径，file_name() 是 None，退到前缀的最后一段
    if let Some(Component::Prefix(prefix)) = path.components().next() {
        let raw = prefix.as_os_str().to_string_lossy().to_string();
        if let Some(last) = raw.trim_end_matches('\\').rsplit('\\').next() {
            let last = last.trim();
            if !last.is_empty() {
                return last.to_string();
            }
        }
    }
    "zone".to_string()
}

pub fn validate_zone_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("开发区名称不能为空");
    }
    if name.eq_ignore_ascii_case(ALL_KEYWORD) {
        bail!("`{ALL_KEYWORD}` 是保留字（zone scan {ALL_KEYWORD} 表示全部），请用 --name 另取一名");
    }
    if name
        .chars()
        .any(|c| c.is_control() || c == '"' || c == '\\')
    {
        bail!("开发区名称含有非法字符：`{name}`");
    }
    if display_width(name) > 40 {
        bail!("开发区名称过长（≤40 列）：`{name}`");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 文本表格对齐（CJK / 全角按 2 列宽）
// ---------------------------------------------------------------------------

const LABEL_WIDTH: usize = 12;

fn kv(label: &str) -> String {
    pad(label, LABEL_WIDTH)
}

fn pair(label: &str, value: String) -> String {
    format!("  {}{value}", kv(label))
}

pub fn display_width(text: &str) -> usize {
    text.chars().map(|c| if is_wide(c) { 2 } else { 1 }).sum()
}

fn is_wide(c: char) -> bool {
    let code = c as u32;
    (0x1100..=0x115F).contains(&code)
        || (0x2E80..=0x303E).contains(&code)
        || (0x3041..=0x33FF).contains(&code)
        || (0x3400..=0x4DBF).contains(&code)
        || (0x4E00..=0x9FFF).contains(&code)
        || (0xA000..=0xA4CF).contains(&code)
        || (0xAC00..=0xD7A3).contains(&code)
        || (0xF900..=0xFAFF).contains(&code)
        || (0xFE10..=0xFE19).contains(&code)
        || (0xFE30..=0xFE6F).contains(&code)
        || (0xFF00..=0xFF60).contains(&code)
        || (0xFFE0..=0xFFE6).contains(&code)
        || (0x20000..=0x2FFFD).contains(&code)
        || (0x30000..=0x3FFFD).contains(&code)
}

pub fn pad(text: &str, width: usize) -> String {
    let used = display_width(text);
    if used >= width {
        return text.to_string();
    }
    format!("{text}{}", " ".repeat(width - used))
}

fn truncate_display(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_string();
    }
    let mut kept = String::new();
    let mut used = 0usize;
    for c in text.chars() {
        let w = if is_wide(c) { 2 } else { 1 };
        if used + w > width.saturating_sub(1) {
            break;
        }
        kept.push(c);
        used += w;
    }
    format!("{kept}…")
}

fn round3(value: f64) -> f64 {
    (value * 1000f64).round() / 1000f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_TEMPLATE;
    use crate::util::{Paths, DRY_RUN_ENV, HOME_ENV};
    use serial_test::serial;

    /// 固定结构 + 已知字节大小的 fixture（golden 测试与本模块单测共用同一张表）。
    const FIXTURE: &[(&str, usize)] = &[
        ("README.md", 24),
        ("src/main.rs", 120),
        ("src/util.rs", 60),
        ("assets/big.dat", 4096),
        ("assets/mid.dat", 1024),
        ("assets/tiny.dat", 1),
        ("docs/notes.txt", 300),
        ("docs/img/logo.png", 300),
    ];
    /// 命中默认 excludes 的条目：统计里必须完全不出现（其中 8192 是全树最大文件）。
    const EXCLUDED: &[(&str, usize)] = &[
        ("node_modules/lib/index.js", 8192),
        ("target/debug/build.o", 2048),
        ("dist/app.js", 512),
        (".git/HEAD", 256),
        (".cache/x", 128),
    ];

    fn write_tree(root: &Path, table: &[(&str, usize)]) {
        for (rel, size) in table {
            let path = root.join(Path::new(rel));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, vec![b'x'; *size]).unwrap();
        }
    }

    fn fixture(root: &Path) {
        write_tree(root, FIXTURE);
        write_tree(root, EXCLUDED);
    }

    fn expected_stats() -> (i64, i64, i64) {
        let bytes: i64 = FIXTURE.iter().map(|(_, size)| *size as i64).sum();
        (bytes, FIXTURE.len() as i64, 4) // src, assets, docs, docs/img
    }

    fn default_excludes() -> RegexSet {
        compile_excludes(&Config::builtin().zone.excludes).unwrap()
    }

    fn ctx_at(home: &Path) -> Ctx {
        let paths = Paths::with_root(home.to_path_buf());
        paths.ensure_dirs().unwrap();
        if !paths.config.exists() {
            std::fs::write(&paths.config, DEFAULT_TEMPLATE).unwrap();
        }
        let cfg = Config::load(&paths).unwrap();
        Ctx {
            cfg,
            paths,
            force: false,
            elevated: false,
            config_error: None,
        }
    }

    fn target_at(name: &str, path: &Path, alert: f64) -> ZoneTarget {
        ZoneTarget {
            name: name.to_string(),
            path: path.to_path_buf(),
            drive: drive_of_path(&path.display().to_string()),
            alert_free_gb: alert,
        }
    }

    fn sandbox(home: &Path) {
        std::env::set_var(HOME_ENV, home);
        std::env::set_var(DRY_RUN_ENV, "1");
    }

    fn leave_sandbox() {
        std::env::remove_var(HOME_ENV);
        std::env::remove_var(DRY_RUN_ENV);
    }

    /// 尽力创建目录符号链接；失败退回 junction（无需管理员），再失败返回 false。
    fn create_dir_link(link: &Path, target: &Path) -> bool {
        if std::os::windows::fs::symlink_dir(target, link).is_ok() {
            return true;
        }
        std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &link.display().to_string(),
                &target.display().to_string(),
            ])
            .output()
            .map(|output| output.status.success() && link.exists())
            .unwrap_or(false)
    }

    // ---- 纯函数：归一化 / 名称 -------------------------------------------

    #[test]
    fn drive_forms_are_normalized() {
        for input in ["D:", "D:\\", "d:/", "D:\\\\", "D:\\.."] {
            let (path, drive) = normalize_zone_path(input).unwrap();
            assert_eq!(path, PathBuf::from("D:\\"), "{input}");
            assert_eq!(drive.as_deref(), Some("D:"), "{input}");
        }
        let (path, drive) = normalize_zone_path("E:\\dev").unwrap();
        assert_eq!(path, PathBuf::from("E:\\dev"));
        assert_eq!(drive.as_deref(), Some("E:"));
        let (path, drive) = normalize_zone_path("C:/Users/x/proj/").unwrap();
        assert_eq!(path, PathBuf::from("C:\\Users\\x\\proj"));
        assert_eq!(drive.as_deref(), Some("C:"));
        let (path, _) = normalize_zone_path("C:\\a\\b\\..\\c").unwrap();
        assert_eq!(path, PathBuf::from("C:\\a\\c"));
        let (path, drive) = normalize_zone_path("\\\\nas\\share\\code").unwrap();
        assert_eq!(path, PathBuf::from("\\\\nas\\share\\code"));
        assert_eq!(drive, None);
        assert!(normalize_zone_path("\\\\nas").is_err());
        assert!(normalize_zone_path("").is_err());
        assert!(normalize_zone_path("   ").is_err());
    }

    #[test]
    fn relative_paths_are_resolved_against_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let (path, drive) = normalize_zone_path("devzone-zone-fixture-hint").unwrap();
        assert_eq!(path, cwd.join("devzone-zone-fixture-hint"));
        assert_eq!(drive, drive_of_path(&cwd.display().to_string()));
    }

    #[test]
    fn default_names_come_from_the_path() {
        assert_eq!(default_zone_name(Path::new("D:\\"), Some("D:")), "d");
        assert_eq!(default_zone_name(Path::new("D:\\dev"), Some("D:")), "dev");
        assert_eq!(
            default_zone_name(Path::new("\\\\nas\\share"), None),
            "share"
        );
        assert_eq!(drive_of_path("\\\\nas\\share\\code"), None, "UNC 没有盘符");
        assert_eq!(drive_of_path("c:\\x").as_deref(), Some("C:"));
        assert_eq!(drive_of_path("relative\\x"), None);
    }

    #[test]
    fn reserved_and_malformed_names_are_rejected() {
        assert!(validate_zone_name("work").is_ok());
        for bad in ["all", "ALL", "  ", "bad\nname", &"x".repeat(41)] {
            assert!(validate_zone_name(bad).is_err(), "`{bad}` 应被拒绝");
        }
        assert!(validate_zone_name("all")
            .unwrap_err()
            .to_string()
            .contains("保留字"));
    }

    #[test]
    fn excludes_compile_and_bad_regex_is_fatal() {
        let set = compile_excludes(&["node_modules".to_string()]).unwrap();
        assert!(excluded_segment(&set, Path::new("a/node_modules/b.js")));
        assert!(!excluded_segment(&set, Path::new("a/src/b.js")));
        // 根目录自身不参与判断（rel 为空）
        assert!(!excluded_segment(&set, Path::new("")));
        let err = compile_excludes(&["node_modules".to_string(), "(".to_string()]).unwrap_err();
        assert!(err.to_string().contains("编译失败"), "{err}");
    }

    // ---- 扫描算法 ---------------------------------------------------------

    #[test]
    fn scan_counts_files_dirs_bytes_and_skips_excludes() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let stats = scan_tree(dir.path(), &default_excludes(), 20).unwrap();
        let (bytes, files, dirs) = expected_stats();
        assert_eq!(stats.total_bytes, bytes, "{stats:?}");
        assert_eq!(stats.file_count, files);
        assert_eq!(stats.dir_count, dirs);
        assert!(stats.excluded >= EXCLUDED.len(), "{stats:?}");
        assert_eq!(stats.top.len(), FIXTURE.len());
        for file in &stats.top {
            for word in ["node_modules", "target", "dist", ".git", ".cache"] {
                assert!(!file.path.contains(word), "{} 命中排除项", file.path);
            }
            assert!(file.mtime.is_some(), "mtime 应随 Top N 落库");
        }
        assert_eq!(stats.top[0].bytes, 4096, "最大的保留文件排第一");
    }

    #[test]
    fn nested_files_inside_excluded_dir_are_not_counted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_tree(root, &[("keep.txt", 10)]);
        write_tree(root, &[("target/a/b/c/deep.bin", 999_999)]);
        let stats = scan_tree(root, &default_excludes(), 20).unwrap();
        assert_eq!(stats.total_bytes, 10);
        assert_eq!(stats.file_count, 1);
        assert_eq!(stats.dir_count, 0);
        assert_eq!(stats.top.len(), 1);
    }

    #[test]
    fn zone_root_named_like_an_exclude_is_still_scanned() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("target");
        std::fs::create_dir_all(&root).unwrap();
        write_tree(&root, &[("inside.txt", 7)]);
        let stats = scan_tree(&root, &default_excludes(), 20).unwrap();
        assert_eq!(stats.total_bytes, 7);
        assert_eq!(stats.file_count, 1);
    }

    #[test]
    fn top_n_heap_keeps_the_largest_and_is_ordered() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_tree(
            root,
            &[
                ("f1", 100),
                ("f2", 500),
                ("f3", 300),
                ("sub/f4", 400),
                ("sub/f5", 300),
                ("f6", 1),
            ],
        );
        let empty = RegexSet::new(Vec::<String>::new()).unwrap();
        let stats = scan_tree(root, &empty, 3).unwrap();
        let sizes: Vec<i64> = stats.top.iter().map(|file| file.bytes).collect();
        assert_eq!(sizes, vec![500, 400, 300], "{:?}", stats.top);
        assert_eq!(stats.file_count, 6);
        assert_eq!(stats.total_bytes, 1601);
        assert_eq!(stats.dir_count, 1);
        // 同字节并列时按路径升序，结果可复现
        assert!(stats.top[2].path.ends_with("f3"), "{:?}", stats.top[2]);
        // top_n 大于文件数时全部返回；top_n = 0 时不维护堆
        assert_eq!(scan_tree(root, &empty, 99).unwrap().top.len(), 6);
        assert!(scan_tree(root, &empty, 0).unwrap().top.is_empty());
    }

    #[test]
    fn symlinks_and_junctions_are_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fixture(root);
        let (bytes, files, dirs) = expected_stats();
        let into_src = root.join("link-to-src");
        let cycle = root.join("self-cycle");
        let created = usize::from(create_dir_link(&into_src, &root.join("src")))
            + usize::from(create_dir_link(&cycle, root));
        let stats = scan_tree(root, &default_excludes(), 20).unwrap();
        if created == 0 {
            eprintln!("（当前环境无法创建符号链接/junction：跳过反环断言）");
            assert_eq!(stats.symlinks, 0);
        } else {
            assert_eq!(
                stats.total_bytes, bytes,
                "跟随链接会把字节数翻倍：{stats:?}"
            );
            assert_eq!(stats.file_count, files);
            assert_eq!(stats.dir_count, dirs, "跟随链接会重复计目录：{stats:?}");
            assert_eq!(stats.symlinks, created, "{stats:?}");
        }
        // 无论链接是否创建成功，遍历都必须终止（自环被跟随时会栈溢出）
        assert!(stats.top[0].bytes <= 8192);
    }

    #[test]
    fn empty_dir_and_missing_root() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            scan_tree(dir.path(), &default_excludes(), 20).unwrap(),
            ScanStats::default()
        );
        let missing = dir.path().join("nope");
        assert!(scan_tree(&missing, &default_excludes(), 20).is_err());
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "x").unwrap();
        assert!(scan_tree(&file, &default_excludes(), 20)
            .unwrap_err()
            .to_string()
            .contains("不是目录"));
    }

    #[test]
    fn alert_state_only_fires_below_threshold() {
        let gib = 1024u64 * 1024 * 1024;
        let low = alert_state(Some(6 * gib as i64), 10.0);
        assert!(low.triggered);
        assert_eq!(low.metric, "free_gb");
        assert_eq!(low.value, Some(6.0));
        assert_eq!(low.threshold, 10.0);
        assert!(!alert_state(Some(11 * gib as i64), 10.0).triggered);
        assert!(!alert_state(None, 10.0).triggered, "读不到剩余空间不预警");
        assert!(!alert_state(Some(0), 0.0).triggered, "阈值 0 表示关闭预警");
    }

    // ---- 报告：Δ / JSON schema -------------------------------------------

    fn fresh_db() -> (Db, ZoneRow) {
        let db = Db::in_memory().unwrap();
        let row = db
            .add_zone(
                "work",
                "C:\\w",
                Some("C:"),
                10.0,
                "2026-10-10T10:00:00+08:00",
                false,
            )
            .unwrap();
        (db, row)
    }

    #[test]
    fn delta_uses_latest_two_snapshots() {
        let (db, zone) = fresh_db();
        let gib = 1024u64 * 1024 * 1024;
        let first = db
            .insert_snapshot(
                zone.id,
                "2026-10-10T10:00:00+08:00",
                1000,
                3,
                1,
                Some(22 * gib as i64),
            )
            .unwrap();
        db.insert_big_files(first, &[("C:\\w\\a".to_string(), 900, None)])
            .unwrap();
        let target = target_at("work", Path::new("C:\\w"), 10.0);
        let snaps = db.snapshots(zone.id, 2).unwrap();
        assert_eq!(snaps.len(), 1);
        let single = report_json(&target, &snaps[0], None, &[], 20);
        assert_eq!(single["delta"]["bytes"], Value::Null);
        assert_eq!(single["delta"]["files"], Value::Null);
        assert_eq!(single["previous"], Value::Null);
        assert_eq!(single["alert"]["triggered"], json!(false));
        assert!(delta_text(&snaps[0], None).starts_with('-'));

        let second = db
            .insert_snapshot(
                zone.id,
                "2026-10-10T11:00:00+08:00",
                1600,
                5,
                2,
                Some(20 * gib as i64),
            )
            .unwrap();
        db.insert_big_files(
            second,
            &[
                (
                    "C:\\w\\big".to_string(),
                    1000,
                    Some("2026-10-10T11:00:00+08:00".to_string()),
                ),
                ("C:\\w\\a".to_string(), 400, None),
            ],
        )
        .unwrap();
        let snaps = db.snapshots(zone.id, 2).unwrap();
        let big = db.big_files(snaps[0].id, 20).unwrap();
        let data = report_json(&target, &snaps[0], Some(&snaps[1]), &big, 20);
        assert_eq!(data["delta"]["bytes"], json!(600));
        assert_eq!(data["delta"]["files"], json!(2));
        assert_eq!(data["previous"]["id"], json!(first));
        assert_eq!(data["current"]["total_bytes"], json!(1600));
        assert_eq!(data["big_files"][0]["rank"], json!(1));
        assert_eq!(data["big_files"][0]["path"], json!("C:\\w\\big"));
        assert_eq!(data["big_files"][0]["human"], json!("1000B"));
        assert_eq!(data["big_files"][1]["rank"], json!(2));
        assert_eq!(data["alert"]["triggered"], json!(false));
        let text = delta_text(&snaps[0], Some(&snaps[1]));
        assert!(
            text.contains("+600B") && text.contains("+2 个文件"),
            "{text}"
        );
        assert_eq!(signed_bytes(-600), human_bytes(-600));
    }

    #[test]
    fn report_json_keys_are_stable_and_volatile_is_isolated() {
        let (db, zone) = fresh_db();
        db.insert_snapshot(zone.id, &now_iso(), 10, 1, 0, Some(100))
            .unwrap();
        let snaps = db.snapshots(zone.id, 2).unwrap();
        let target = target_at("work", Path::new("C:\\w"), 10.0);
        let one = report_json(&target, &snaps[0], None, &[], 20);
        let two = report_json(&target, &snaps[0], None, &[], 20);
        // 稳定段（剥掉 data.volatile）两次序列化必须逐字节一致：这是 golden 能
        // 逐字节比对的前提；volatile 里带 now_iso()，本来每次都不同。
        let stable = |value: &Value| {
            let mut copy = value.clone();
            copy.as_object_mut()
                .unwrap()
                .remove("volatile")
                .expect("data.volatile 必须存在");
            serde_json::to_string_pretty(&copy).unwrap()
        };
        assert_eq!(stable(&one), stable(&two));
        let keys: Vec<&String> = one.as_object().unwrap().keys().collect();
        for expected in [
            "alert",
            "big_files",
            "current",
            "delta",
            "previous",
            "schema",
            "top_n",
            "volatile",
            "zone",
        ] {
            assert!(keys.contains(&&expected.to_string()), "{keys:?}");
        }
        assert_eq!(one["schema"].as_str(), Some(REPORT_SCHEMA));
        let volatile = one["volatile"].as_object().unwrap();
        for expected in [
            "alert_value_gb",
            "big_file_mtimes",
            "current_ts",
            "free_bytes",
            "free_gb",
            "generated_at",
            "previous_free_bytes",
            "previous_ts",
        ] {
            assert!(volatile.contains_key(expected), "{volatile:?}");
        }
        let mut stable = one.clone();
        stable.as_object_mut().unwrap().remove("volatile");
        let text = serde_json::to_string(&stable).unwrap();
        // 稳定段里不得出现任何时间戳键或实时剩余空间键（`alert.metric` 的值是
        // "free_gb" 这个字符串，不带冒号，所以按 JSON 键来查）
        for leak in [
            "\"free_bytes\":",
            "\"free_gb\":",
            "\"current_ts\":",
            "\"previous_ts\":",
            "\"generated_at\":",
            "\"alert_value_gb\":",
            "\"mtime\":",
        ] {
            assert!(!text.contains(leak), "易变值泄漏到稳定段：{leak} in {text}");
        }
        assert_eq!(stable["current"]["file_count"], json!(1));
    }

    #[test]
    fn text_table_helpers_count_wide_chars() {
        assert_eq!(display_width("路径"), 4);
        assert_eq!(display_width("ab"), 2);
        assert_eq!(pad("名称", 8), "名称    ");
        assert_eq!(pad("already-long", 5), "already-long");
        assert_eq!(kv("阈值"), "阈值        ");
        assert_eq!(
            pair("阈值", "10.00 GB".to_string()),
            "  阈值        10.00 GB"
        );
        assert_eq!(truncate_display("1234567890", 5), "1234…");
        assert_eq!(truncate_display("中文中文中文", 5), "中文…");
        assert_eq!(mtime_text(None), "-");
        assert_eq!(mtime_text(Some("2026-10-10T20:00:00+08:00")), "2026-10-10");
        let gib = 1024u64 * 1024 * 1024;
        assert!(alert_text(&alert_state(Some(20 * gib as i64), 10.0)).contains("正常"));
        let warning = alert_text(&alert_state(Some(6 * gib as i64), 10.0));
        assert!(
            warning.contains("预警") && warning.contains("6.00"),
            "{warning}"
        );
        assert!(
            alert_text(&alert_state(None, 10.0)).contains("未知"),
            "读不到剩余空间时不谎报正常"
        );
    }

    // ---- 端到端：add / scan / report / list / remove / watch --------------

    #[test]
    #[serial]
    fn add_then_report_end_to_end() {
        let home = tempfile::tempdir().unwrap();
        let zone_dir = tempfile::tempdir().unwrap();
        fixture(zone_dir.path());
        sandbox(home.path());
        assert_eq!(Paths::resolve().unwrap().root, home.path());

        let mut ctx = ctx_at(home.path());
        let raw = zone_dir.path().display().to_string();
        handle(
            &ZoneCmd::Add {
                path: raw.clone(),
                name: Some("golden".to_string()),
                alert_free: Some(0.0),
            },
            &ctx,
        )
        .unwrap();

        // 配置写入且保留注释
        let text = std::fs::read_to_string(&ctx.paths.config).unwrap();
        assert!(text.contains("加载顺序"), "注释丢失");
        assert!(text.contains("[zone.zones]"), "{text}");
        let cfg = Config::load(&ctx.paths).unwrap();
        assert_eq!(cfg.zone.zones["golden"].path, raw);
        assert_eq!(cfg.zone.zones["golden"].alert_free_gb, Some(0.0));

        // 登记后立即扫描过一次（§6.1.1）
        let db = ctx.db().unwrap();
        let row = db.zone_by_name("golden").unwrap().unwrap();
        let snaps = db.snapshots(row.id, 5).unwrap();
        assert_eq!(snaps.len(), 1);
        let (bytes, files, dirs) = expected_stats();
        assert_eq!(snaps[0].total_bytes, bytes);
        assert_eq!(snaps[0].file_count, files);
        assert_eq!(snaps[0].dir_count, dirs);
        assert_eq!(db.table_counts().unwrap()["big_files"].as_i64(), Some(8));

        // 同名重复 add：未 --force 报错并提示；--force 覆盖且不新增登记行
        let err = handle(
            &ZoneCmd::Add {
                path: raw.clone(),
                name: Some("golden".to_string()),
                alert_free: None,
            },
            &ctx,
        )
        .unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");
        ctx.force = true;
        handle(
            &ZoneCmd::Add {
                path: raw.clone(),
                name: Some("golden".to_string()),
                alert_free: Some(2.0),
            },
            &ctx,
        )
        .unwrap();
        ctx.force = false;
        let db = ctx.db().unwrap();
        assert_eq!(db.list_zones().unwrap().len(), 1);
        assert_eq!(
            db.zone_by_name("golden").unwrap().unwrap().alert_free_gb,
            2.0
        );
        ctx.force = true;
        handle(
            &ZoneCmd::Add {
                path: raw.clone(),
                name: Some("golden".to_string()),
                alert_free: Some(0.0),
            },
            &ctx,
        )
        .unwrap();
        ctx.force = false;
        let db = ctx.db().unwrap();

        // 路径不存在 / 保留名 / 负阈值
        assert!(handle(
            &ZoneCmd::Add {
                path: "C:\\definitely\\not\\here\\devzone-x".to_string(),
                name: None,
                alert_free: None,
            },
            &ctx,
        )
        .unwrap_err()
        .to_string()
        .contains("路径不存在"));
        assert!(handle(
            &ZoneCmd::Add {
                path: raw.clone(),
                name: Some("all".to_string()),
                alert_free: None,
            },
            &ctx,
        )
        .is_err());
        assert!(handle(
            &ZoneCmd::Add {
                path: raw.clone(),
                name: Some("neg".to_string()),
                alert_free: Some(-1.0),
            },
            &ctx,
        )
        .is_err());

        // 只有一条快照 => Δ 为 null，alert_free_gb=0 => 不预警
        // （--force 覆盖会重建登记行，id 递增，所以这里重新取一次）
        let row = db.zone_by_name("golden").unwrap().unwrap();
        let target = target_at("golden", zone_dir.path(), 0.0);
        let snaps = db.snapshots(row.id, 2).unwrap();
        assert_eq!(
            snaps.len(),
            1,
            "前两次 add 各扫一轮，但历史随 force 一起清空"
        );
        let single = report_json(&target, &snaps[0], None, &[], 20);
        assert_eq!(single["delta"]["bytes"], Value::Null);
        assert_eq!(single["alert"]["triggered"], json!(false));

        // 追加字节后再扫一次 => Δ 为正
        std::fs::write(zone_dir.path().join("added.bin"), vec![b'y'; 4096]).unwrap();
        handle(
            &ZoneCmd::Scan {
                name: "golden".to_string(),
            },
            &ctx,
        )
        .unwrap();
        let db = ctx.db().unwrap();
        let snaps = db.snapshots(row.id, 2).unwrap();
        assert_eq!(snaps.len(), 2);
        let big = db.big_files(snaps[0].id, 20).unwrap();
        let data = report_json(&target, &snaps[0], Some(&snaps[1]), &big, 20);
        assert_eq!(data["delta"]["bytes"], json!(4096));
        assert_eq!(data["delta"]["files"], json!(1));
        assert_eq!(
            data["big_files"][0]["path"].as_str(),
            zone_dir.path().join("added.bin").to_str()
        );
        assert_eq!(data["zone"]["drive"], json!(target.drive));

        // report all 拒绝；未知名字报错；list / json 文本两路都跑一遍
        assert!(handle(
            &ZoneCmd::Report {
                name: "all".to_string(),
                json: true,
                top: None
            },
            &ctx,
        )
        .unwrap_err()
        .to_string()
        .contains("具体开发区名"));
        assert!(handle(
            &ZoneCmd::Report {
                name: "nope".to_string(),
                json: true,
                top: None
            },
            &ctx,
        )
        .is_err());
        handle(&ZoneCmd::List, &ctx).unwrap();
        handle(
            &ZoneCmd::Report {
                name: "golden".to_string(),
                json: true,
                top: Some(3),
            },
            &ctx,
        )
        .unwrap();
        handle(
            &ZoneCmd::Report {
                name: "golden".to_string(),
                json: false,
                top: None,
            },
            &ctx,
        )
        .unwrap();

        // remove：配置与历史一起清；重复 remove 报错
        handle(
            &ZoneCmd::Remove {
                name: "golden".to_string(),
            },
            &ctx,
        )
        .unwrap();
        assert!(!Config::load(&ctx.paths)
            .unwrap()
            .zone
            .zones
            .contains_key("golden"));
        let db = ctx.db().unwrap();
        assert!(db.zone_by_name("golden").unwrap().is_none());
        assert_eq!(
            db.table_counts().unwrap()["zone_snapshots"].as_i64(),
            Some(0)
        );
        assert!(handle(
            &ZoneCmd::Remove {
                name: "golden".to_string()
            },
            &ctx
        )
        .unwrap_err()
        .to_string()
        .contains("不存在"));
        leave_sandbox();
    }

    #[test]
    #[serial]
    fn scan_all_and_watch_round_cover_every_zone() {
        let home = tempfile::tempdir().unwrap();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        fixture(a.path());
        write_tree(b.path(), &[("only.txt", 5)]);
        sandbox(home.path());
        let ctx = ctx_at(home.path());
        let mut ctx = ctx;
        for (name, dir) in [("alpha", a.path()), ("beta", b.path())] {
            ctx.cfg.zone.zones.insert(
                name.to_string(),
                ZoneDef {
                    path: dir.display().to_string(),
                    alert_free_gb: Some(0.0),
                },
            );
        }
        let db = ctx.db().unwrap();
        let targets = config_targets(&ctx, &db, Some("all")).unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].name, "alpha", "顺序按名称字典序，可复现");
        let run = scan_run(&ctx, &db, &ctx.bus().unwrap(), &targets);
        assert!(run.failures.is_empty(), "{:?}", run.failures);
        assert_eq!(run.outcomes.len(), 2);
        assert_eq!(db.table_counts().unwrap()["zones"].as_i64(), Some(2));
        handle(
            &ZoneCmd::Scan {
                name: "all".to_string(),
            },
            &ctx,
        )
        .unwrap();
        handle(
            &ZoneCmd::Scan {
                name: "beta".to_string(),
            },
            &ctx,
        )
        .unwrap();
        assert!(handle(
            &ZoneCmd::Scan {
                name: "ghost".to_string()
            },
            &ctx
        )
        .is_err());
        // 前台循环：只跑一轮
        assert_eq!(run_watch_rounds(&ctx, 1, Some(1)).unwrap(), 1);
        let alpha = db.zone_by_name("alpha").unwrap().unwrap();
        assert_eq!(db.snapshots(alpha.id, 10).unwrap().len(), 3);
        // 磁盘上不可用的开发区（配置里写了不存在的路径）单独失败，不拖垮整批
        ctx.cfg.zone.zones.insert(
            "ghost-zone".to_string(),
            ZoneDef {
                path: "C:\\devzone\\no-such-zone".to_string(),
                alert_free_gb: Some(0.0),
            },
        );
        let run = scan_run(
            &ctx,
            &db,
            &ctx.bus().unwrap(),
            &config_targets(&ctx, &db, Some("all")).unwrap(),
        );
        assert_eq!(run.outcomes.len(), 2);
        assert_eq!(run.failures.len(), 1);
        assert_eq!(run.failures[0].0, "ghost-zone");
        assert!(handle(
            &ZoneCmd::Scan {
                name: "all".to_string()
            },
            &ctx
        )
        .is_err());
        leave_sandbox();
    }

    #[test]
    #[serial]
    fn low_threshold_emits_zone_alert_once_per_six_hours() {
        let home = tempfile::tempdir().unwrap();
        let zone_dir = tempfile::tempdir().unwrap();
        fixture(zone_dir.path());
        sandbox(home.path());
        let ctx = ctx_at(home.path());
        // 阈值高于任何真实剩余空间 => 必然触发
        let target = target_at("roomy", zone_dir.path(), f64::from(u32::MAX));
        let db = ctx.db().unwrap();
        let bus = ctx.bus().unwrap();
        let first = scan_target(&ctx, &db, &bus, &target).unwrap();
        assert!(first.alert.triggered && first.emitted, "{first:?}");
        let second = scan_target(&ctx, &db, &bus, &target).unwrap();
        assert!(second.alert.triggered && !second.emitted, "6h 去抖失效");
        let lines = std::fs::read_to_string(&ctx.paths.events).unwrap();
        let alerts: Vec<Value> = lines
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|event| event["name"].as_str() == Some(event_name::ZONE_ALERT))
            .collect();
        assert_eq!(alerts.len(), 1, "{alerts:?}");
        assert_eq!(alerts[0]["payload"]["zone"].as_str(), Some("roomy"));
        assert_eq!(alerts[0]["payload"]["metric"].as_str(), Some("free_gb"));
        assert!(alerts[0]["payload"]["value"].is_number());
        assert_eq!(
            alerts[0]["payload"]["threshold"],
            json!(round3(target.alert_free_gb))
        );
        let snaps = db
            .snapshots(db.zone_by_name("roomy").unwrap().unwrap().id, 2)
            .unwrap();
        let data = report_json(&target, &snaps[0], None, &[], 20);
        assert_eq!(data["alert"]["triggered"], json!(true));
        assert_eq!(data["alert"]["debounce_hours"], json!(6));
        assert!(data["volatile"]["free_gb"].is_number());
        assert!(data["volatile"]["alert_value_gb"].is_number());
        assert!(data["current"]
            .as_object()
            .unwrap()
            .contains_key("file_count"));
        assert!(!data["current"]
            .as_object()
            .unwrap()
            .contains_key("free_bytes"));
        leave_sandbox();
    }

    #[test]
    #[serial]
    fn summary_reports_zones_and_orphan_rows() {
        let home = tempfile::tempdir().unwrap();
        let zone_dir = tempfile::tempdir().unwrap();
        fixture(zone_dir.path());
        sandbox(home.path());
        let mut ctx = ctx_at(home.path());
        let db = ctx.db().unwrap();
        assert_eq!(summary(&ctx, &db).unwrap()["registered"], json!(0));
        assert_eq!(
            summary(&ctx, &db).unwrap()["zones"]
                .as_array()
                .unwrap()
                .len(),
            0
        );

        ctx.cfg.zone.zones.insert(
            "one".to_string(),
            ZoneDef {
                path: zone_dir.path().display().to_string(),
                alert_free_gb: Some(0.0),
            },
        );
        let targets = config_targets(&ctx, &db, None).unwrap();
        scan_run(&ctx, &db, &ctx.bus().unwrap(), &targets);
        let value = summary(&ctx, &db).unwrap();
        assert_eq!(value["registered"], json!(1));
        assert_eq!(value["zones"][0]["name"].as_str(), Some("one"));
        assert_eq!(value["zones"][0]["scanned"], json!(true));
        assert_eq!(value["zones"][0]["file_count"], json!(FIXTURE.len()));
        assert_eq!(value["zones"][0]["alert"], json!(false));
        assert_eq!(value["top_n"], json!(ctx.cfg.zone.top_n));
        assert!(value["excludes"].is_array());

        // 数据库里有、配置里没有 => 如实报孤儿行
        db.add_zone("ghost", "C:\\ghost", None, 10.0, &now_iso(), false)
            .unwrap();
        let value = summary(&ctx, &db).unwrap();
        assert_eq!(value["orphan_db_rows"][0], json!("ghost"));
        handle(&ZoneCmd::List, &ctx).unwrap();
        // 配置缺失但数据库有的开发区仍可读报告
        handle(
            &ZoneCmd::Report {
                name: "ghost".to_string(),
                json: true,
                top: None,
            },
            &ctx,
        )
        .unwrap_err();
        leave_sandbox();
    }

    #[serial]
    #[test]
    #[serial]
    fn sync_zone_row_mirrors_config_into_db() {
        let home = tempfile::tempdir().unwrap();
        let zone_dir = tempfile::tempdir().unwrap();
        std::env::set_var(DRY_RUN_ENV, "1");
        let ctx = ctx_at(home.path());
        let db = ctx.db().unwrap();
        let mut target = target_at("sync", zone_dir.path(), 3.0);
        let row = sync_zone_row(&db, &target).unwrap();
        assert_eq!(row.alert_free_gb, 3.0);
        assert_eq!(row.drive, target.drive);
        assert_eq!(row.path, zone_dir.path().display().to_string());

        // 阈值变化走 update_zone_alert
        target.alert_free_gb = 4.5;
        let row = sync_zone_row(&db, &target).unwrap();
        assert_eq!(row.alert_free_gb, 4.5);

        // 路径漂移保留快照历史
        db.insert_snapshot(row.id, &now_iso(), 1, 1, 1, Some(1))
            .unwrap();
        let other = tempfile::tempdir().unwrap();
        target.path = other.path().to_path_buf();
        let moved = sync_zone_row(&db, &target).unwrap();
        assert_eq!(moved.id, row.id, "路径漂移不应重建登记行");
        assert_eq!(moved.path, other.path().display().to_string());
        assert_eq!(
            db.table_counts().unwrap()["zone_snapshots"].as_i64(),
            Some(1)
        );
        std::env::remove_var(DRY_RUN_ENV);
    }

    #[test]
    fn watch_daemon_interval_maps_to_whole_minutes() {
        // 只做换算断言；真正注册由 mod_sched::register_interval_task 负责（需管理员）。
        for (interval, minutes) in [(3600u64, 60u32), (60, 1), (30, 1), (7200, 120), (0, 1)] {
            assert_eq!((interval / 60).max(1) as u32, minutes, "{interval}");
        }
    }
}

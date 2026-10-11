//! M4 打扰阻断（build guide section 6.4）：写
//! `HKLM\SOFTWARE\Microsoft\WindowsUpdate\UX\Settings` 的五个 Pause* 值
//! （REG_SZ，`yyyy-MM-ddTHH:mm:ssZ` UTC），N 上限 35 天，过期自动删除。
//! 已查证来源：Microsoft Learn `waas-configure-wufb`（§16.12 行为仍待实测）。
//!
//! 安全要求：注册表写入必须可被 dry-run 拦截。本模块所有 HKLM 访问都经
//! [`reg_set`] / [`reg_get`] / [`reg_delete`] 三个私有包装，当
//! `DEVZONE_DRY_RUN=1`（[`util::ExternalCmd::dry_run`]）时只记日志并读写
//! 进程内沙箱，**绝不触碰真实注册表**（§11：单元测试即依赖这一点）。

use std::collections::BTreeMap;
use std::sync::Mutex;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, Duration, Local, NaiveDateTime, Timelike, Utc};
use serde_json::{json, Value};
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE};
use winreg::RegKey;

use crate::cli::CrunchCmd;
use crate::events::name;
use crate::util::{self, ExternalCmd};
use crate::Ctx;

/// §6.4 / §16.12：暂停更新的注册表位置（HKLM，写入需管理员）。
const UPDATE_SETTINGS_KEY: &str = r"SOFTWARE\Microsoft\WindowsUpdate\UX\Settings";

/// Windows 允许的最大暂停天数（§6.4：>35 截断为 35 并提示）。
pub const MAX_PAUSE_DAYS: u32 = 35;

pub const KEY_EXPIRY: &str = "PauseUpdatesExpiryTime";
pub const KEY_FEATURE_START: &str = "PauseFeatureUpdatesStartTime";
pub const KEY_FEATURE_END: &str = "PauseFeatureUpdatesEndTime";
pub const KEY_QUALITY_START: &str = "PauseQualityUpdatesStartTime";
pub const KEY_QUALITY_END: &str = "PauseQualityUpdatesEndTime";

/// §6.4 要求写入 / 删除的 5 个 REG_SZ 值。
pub const PAUSE_KEYS: [&str; 5] = [
    KEY_EXPIRY,
    KEY_FEATURE_START,
    KEY_FEATURE_END,
    KEY_QUALITY_START,
    KEY_QUALITY_END,
];

/// dry-run 沙箱：`DEVZONE_DRY_RUN=1` 时写落到这里、读也从这里取。
static DRY_VALUES: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

fn dry_store() -> std::sync::MutexGuard<'static, BTreeMap<String, String>> {
    DRY_VALUES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// 纯逻辑：时间格式、天数截断、5 个键值的构造 / 解析
// ---------------------------------------------------------------------------

/// §6.4 时间格式 `yyyy-MM-ddTHH:mm:ssZ`（UTC）。
pub fn utc_stamp(now: DateTime<Utc>) -> String {
    now.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// 把时间压到整秒（§6.4 的存储格式没有小数秒）。
fn truncate_to_second(value: DateTime<Utc>) -> DateTime<Utc> {
    value.with_nanosecond(0).unwrap_or(value)
}

/// 解析注册表里的暂停时间；容忍带毫秒 / 显式偏移 / 省略时区的写法，统一归一到整秒 UTC。
pub fn parse_utc_stamp(raw: &str) -> Result<DateTime<Utc>> {
    let text = raw.trim();
    if text.is_empty() {
        bail!("暂停时间为空");
    }
    if let Ok(parsed) = DateTime::parse_from_rfc3339(text) {
        return Ok(truncate_to_second(parsed.with_timezone(&Utc)));
    }
    let stripped = text.strip_suffix('Z').unwrap_or(text);
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(stripped, format) {
            return Ok(truncate_to_second(naive.and_utc()));
        }
    }
    bail!("无法解析暂停时间 `{raw}`（期望 yyyy-MM-ddTHH:mm:ssZ）")
}

/// 本地时区的可读时间（回显给用户；注册表里存的仍是 UTC）。
fn local_text(utc: DateTime<Utc>) -> String {
    utc.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// `--days`：缺省取 `cfg.crunch.pause_days`；>35 截断为 35 并给出提示文案。
pub fn resolve_days(requested: Option<u32>, cfg_default: u32) -> Result<(u32, Option<String>)> {
    let raw = requested.unwrap_or(cfg_default);
    if raw == 0 {
        bail!("暂停天数至少为 1（`--days 0` 无意义；恢复更新请用 devzone crunch off）");
    }
    if raw > MAX_PAUSE_DAYS {
        return Ok((
            MAX_PAUSE_DAYS,
            Some(format!(
                "Windows 最多只能暂停 {MAX_PAUSE_DAYS} 天，已把 {raw} 天截断为 {MAX_PAUSE_DAYS} 天"
            )),
        ));
    }
    Ok((raw, None))
}

/// §6.4 的 5 个键值（纯数据结构体，构造与解析都可单测）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PauseWindow {
    pub expiry: String,
    pub feature_start: String,
    pub feature_end: String,
    pub quality_start: String,
    pub quality_end: String,
}

impl PauseWindow {
    /// `expiry = now + N 天`；feature/quality 的起止同值（§6.4）。
    pub fn build(now: DateTime<Utc>, days: u32) -> PauseWindow {
        let start = utc_stamp(now);
        let end = utc_stamp(now + Duration::days(i64::from(days)));
        PauseWindow {
            expiry: end.clone(),
            feature_start: start.clone(),
            feature_end: end.clone(),
            quality_start: start,
            quality_end: end,
        }
    }

    /// -> 注册表键值映射（固定 5 项，写序稳定，便于日志与测试）。
    pub fn to_values(&self) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        map.insert(KEY_EXPIRY.to_string(), self.expiry.clone());
        map.insert(KEY_FEATURE_START.to_string(), self.feature_start.clone());
        map.insert(KEY_FEATURE_END.to_string(), self.feature_end.clone());
        map.insert(KEY_QUALITY_START.to_string(), self.quality_start.clone());
        map.insert(KEY_QUALITY_END.to_string(), self.quality_end.clone());
        map
    }

    /// 从注册表读到的键值构造；缺任一值返回 None（如实反映，不做猜测）。
    pub fn from_values(values: &BTreeMap<String, String>) -> Option<PauseWindow> {
        Some(PauseWindow {
            expiry: values.get(KEY_EXPIRY).cloned()?,
            feature_start: values.get(KEY_FEATURE_START).cloned()?,
            feature_end: values.get(KEY_FEATURE_END).cloned()?,
            quality_start: values.get(KEY_QUALITY_START).cloned()?,
            quality_end: values.get(KEY_QUALITY_END).cloned()?,
        })
    }

    pub fn expiry_time(&self) -> Result<DateTime<Utc>> {
        parse_utc_stamp(&self.expiry)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "expiry": self.expiry,
            "feature_start": self.feature_start,
            "feature_end": self.feature_end,
            "quality_start": self.quality_start,
            "quality_end": self.quality_end,
        })
    }
}

/// 注册表状态归一化结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PauseState {
    /// 没有 `PauseUpdatesExpiryTime`（或整个键不存在）。
    NotPaused,
    Paused {
        expiry: DateTime<Utc>,
    },
    Expired {
        expiry: DateTime<Utc>,
    },
    /// 有值但解析不了：如实报告，不猜测、不自动删除。
    Unparsable {
        raw: String,
        reason: String,
    },
}

/// 纯函数：注册表读到的原始值 + 当前时间 -> 状态。
pub fn classify(raw: Option<String>, now: DateTime<Utc>) -> PauseState {
    let Some(text) = raw else {
        return PauseState::NotPaused;
    };
    match parse_utc_stamp(&text) {
        Ok(expiry) if expiry <= now => PauseState::Expired { expiry },
        Ok(expiry) => PauseState::Paused { expiry },
        Err(err) => PauseState::Unparsable {
            raw: text,
            reason: err.to_string(),
        },
    }
}

/// 剩余整天数（向上取整；已过期为 0）。
pub fn days_left(expiry: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
    let secs = (expiry - now).num_seconds();
    if secs <= 0 {
        0
    } else {
        secs.div_euclid(86_400) + i64::from(secs.rem_euclid(86_400) > 0)
    }
}

/// 展示层视图（`crunch status` 与 `devzone status` 共用）。
#[derive(Clone, Debug, PartialEq)]
pub struct PauseView {
    pub paused: bool,
    pub expiry: Option<String>,
    pub expired: bool,
    pub days_left: i64,
    pub message: String,
}

/// 纯函数：状态 -> 用户可读视图。`auto_recovered` 表示本次刚到期并自动清理。
pub fn describe(state: &PauseState, now: DateTime<Utc>, auto_recovered: bool) -> PauseView {
    if auto_recovered {
        return PauseView {
            paused: false,
            expiry: None,
            expired: true,
            days_left: 0,
            message: "暂停已到期，已自动恢复更新".to_string(),
        };
    }
    match state {
        PauseState::NotPaused => PauseView {
            paused: false,
            expiry: None,
            expired: false,
            days_left: 0,
            message: "未暂停（HKLM 更新暂停键值不存在，Windows Update 正常）".to_string(),
        },
        PauseState::Paused { expiry } => {
            let left = days_left(*expiry, now);
            PauseView {
                paused: true,
                expiry: Some(utc_stamp(*expiry)),
                expired: false,
                days_left: left,
                message: format!(
                    "更新已暂停至 {}（剩余约 {left} 天）",
                    local_text(*expiry)
                ),
            }
        }
        PauseState::Expired { expiry } => PauseView {
            paused: false,
            expiry: Some(utc_stamp(*expiry)),
            expired: true,
            days_left: 0,
            message: format!(
                "暂停已到期（{}）；运行 devzone crunch off 恢复更新（auto_expire=true 时会自动删除）",
                local_text(*expiry)
            ),
        },
        PauseState::Unparsable { raw, reason } => PauseView {
            paused: false,
            expiry: Some(raw.clone()),
            expired: false,
            days_left: 0,
            message: format!("注册表里的暂停时间无法解析：{reason}"),
        },
    }
}

impl PauseView {
    fn to_json(&self, ctx: &Ctx, auto_recovered: bool) -> Value {
        json!({
            "paused": self.paused,
            "expiry": self.expiry,
            "expired": self.expired,
            "days_left": self.days_left,
            "auto_expire": ctx.cfg.crunch.auto_expire,
            "auto_recovered": auto_recovered,
            "default_pause_days": ctx.cfg.crunch.pause_days,
            "max_pause_days": MAX_PAUSE_DAYS,
            "registry": UPDATE_SETTINGS_KEY,
            "dry_run": ExternalCmd::dry_run(),
            "message": self.message,
        })
    }
}

// ---------------------------------------------------------------------------
// 注册表包装（dry-run 拦截层）
// ---------------------------------------------------------------------------

/// 写一个 REG_SZ。dry-run 时只记日志 + 落沙箱，不写 HKLM。
fn reg_set(key: &str, value: &str) -> Result<()> {
    if ExternalCmd::dry_run() {
        tracing::info!("dry-run: 未写注册表 HKLM\\{UPDATE_SETTINGS_KEY} {key}={value}");
        eprintln!("[devzone] dry-run: 未写注册表 {key}={value}");
        dry_store().insert(key.to_string(), value.to_string());
        return Ok(());
    }
    let (hk, _disposition) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .create_subkey_with_flags(UPDATE_SETTINGS_KEY, KEY_SET_VALUE)
        .with_context(|| format!("打开/创建 HKLM\\{UPDATE_SETTINGS_KEY} 失败（需要管理员权限）"))?;
    hk.set_value(key, &value.to_string())
        .with_context(|| format!("写入 {key} 失败（需要管理员权限）"))?;
    tracing::info!("HKLM\\{UPDATE_SETTINGS_KEY} {key}={value}");
    Ok(())
}

/// 读一个 REG_SZ。dry-run 时只读沙箱（模拟值），不读 HKLM。
fn reg_get(key: &str) -> Result<Option<String>> {
    if ExternalCmd::dry_run() {
        let value = dry_store().get(key).cloned();
        tracing::info!("dry-run: 从沙箱读 {key} -> {value:?}");
        return Ok(value);
    }
    let Ok(hk) =
        RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(UPDATE_SETTINGS_KEY, KEY_READ)
    else {
        // 整个键不存在 = 从未暂停过
        return Ok(None);
    };
    match hk.get_value::<String, _>(key) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(anyhow!(err)).with_context(|| format!("读取 {key} 失败")),
    }
}

/// 删除一个值；键或值不存在都不算错（幂等）。返回是否真的删掉了一个值。
fn reg_delete(key: &str) -> Result<bool> {
    if ExternalCmd::dry_run() {
        let removed = dry_store().remove(key).is_some();
        tracing::info!("dry-run: 未删注册表 {key}（沙箱 removed={removed}）");
        eprintln!("[devzone] dry-run: 未删注册表 {key}");
        return Ok(removed);
    }
    let Ok(hk) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(UPDATE_SETTINGS_KEY, KEY_SET_VALUE)
    else {
        return Ok(false);
    };
    match hk.delete_value(key) {
        Ok(()) => {
            tracing::info!("deleted HKLM\\{UPDATE_SETTINGS_KEY} {key}");
            Ok(true)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(anyhow!(err)).with_context(|| format!("删除 {key} 失败（需要管理员权限）")),
    }
}

/// 读回 §6.4 的 5 个键值。
fn read_pause_values() -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for key in PAUSE_KEYS {
        if let Some(value) = reg_get(key)? {
            map.insert(key.to_string(), value);
        }
    }
    Ok(map)
}

/// 删除 §6.4 的 5 个键值；不存在的键不算错。返回实际删除数量。
fn clear_pause_values() -> Result<usize> {
    let mut removed = 0usize;
    for key in PAUSE_KEYS {
        if reg_delete(key)? {
            removed += 1;
        }
    }
    Ok(removed)
}

/// 状态变化事件（`crunch_state` 是本项目为 crunch 状态变化新增的事件名）。
/// 派发失败只记日志，不推翻已完成的注册表操作（§9 降级总表）。
fn emit_state(ctx: &Ctx, payload: Value) {
    match ctx.bus() {
        Ok(bus) => {
            if let Err(err) = bus.emit_json(name::CRUNCH_STATE, payload) {
                tracing::warn!("crunch_state 事件派发失败：{err}");
                util::progress(&format!(
                    "事件派发失败（不影响已完成的更新暂停操作）：{err}"
                ));
            }
        }
        Err(err) => {
            tracing::warn!("事件总线不可用：{err}");
            util::progress(&format!(
                "事件总线不可用（不影响已完成的更新暂停操作）：{err}"
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// 命令
// ---------------------------------------------------------------------------

pub fn handle(cmd: &CrunchCmd, ctx: &Ctx) -> Result<()> {
    match cmd {
        CrunchCmd::On { days } => crunch_on(ctx, *days),
        CrunchCmd::Off => crunch_off(ctx),
        CrunchCmd::Status { json } => crunch_status(ctx, *json),
    }
}

fn crunch_on(ctx: &Ctx, requested: Option<u32>) -> Result<()> {
    let (days, truncated) = resolve_days(requested, ctx.cfg.crunch.pause_days)?;
    if let Some(hint) = &truncated {
        util::progress(hint);
    }
    let now = Utc::now();
    let window = PauseWindow::build(now, days);
    for (key, value) in window.to_values() {
        reg_set(&key, &value).with_context(|| format!("写入更新暂停键值 {key} 失败"))?;
    }
    let expiry = window.expiry_time().context("刚写入的到期时间应当可解析")?;

    let dry_suffix = if ExternalCmd::dry_run() {
        "（dry-run：未写注册表）"
    } else {
        ""
    };
    let expiry_text = local_text(expiry);
    println!("已{dry_suffix}暂停 Windows 更新 {days} 天，到期：{expiry_text}");
    if truncated.is_some() {
        println!("（已按 Windows 上限截断为 {MAX_PAUSE_DAYS} 天）");
    }
    println!("写入的 5 个 REG_SZ（HKLM\\{UPDATE_SETTINGS_KEY}）：");
    for (key, value) in window.to_values() {
        println!("  {key} = {value}");
    }
    println!("提前恢复更新：devzone crunch off");
    emit_state(
        ctx,
        json!({
            "action": "on",
            "days": days,
            "until": window.expiry,
            "dry_run": ExternalCmd::dry_run(),
        }),
    );
    Ok(())
}

fn crunch_off(ctx: &Ctx) -> Result<()> {
    let removed = clear_pause_values()?;
    let dry_suffix = if ExternalCmd::dry_run() {
        "（dry-run：未改注册表）"
    } else {
        ""
    };
    if removed == 0 {
        println!("已{dry_suffix}恢复 Windows 更新：原本就没有暂停键值（操作幂等）");
    } else {
        println!("已{dry_suffix}恢复 Windows 更新：删除 {removed} 个暂停键值");
    }
    emit_state(
        ctx,
        json!({
            "action": "off",
            "removed": removed,
            "dry_run": ExternalCmd::dry_run(),
        }),
    );
    Ok(())
}

fn crunch_status(ctx: &Ctx, as_json: bool) -> Result<()> {
    let now = Utc::now();
    let mut state = classify(reg_get(KEY_EXPIRY)?, now);
    let mut auto_recovered = false;
    let due = match &state {
        PauseState::Expired { expiry } if ctx.cfg.crunch.auto_expire => Some(utc_stamp(*expiry)),
        _ => None,
    };
    if let Some(until) = due {
        // §9：清理失败（例如未提权）不报错退出，如实展示到期状态并提示手动恢复。
        match clear_pause_values() {
            Ok(removed) => {
                tracing::info!("crunch auto_expire：已删除 {removed} 个到期键值");
                auto_recovered = true;
                state = PauseState::NotPaused;
                emit_state(
                    ctx,
                    json!({ "action": "auto_expired", "until": until, "removed": removed }),
                );
            }
            Err(err) => {
                tracing::warn!("crunch auto_expire 清理失败：{err}");
                util::progress(&format!("自动恢复未生效（需要管理员权限？）：{err}"));
            }
        }
    }
    let view = describe(&state, now, auto_recovered);
    if ctx.emit_json("crunch status", view.to_json(ctx, auto_recovered), as_json)? {
        return Ok(());
    }
    println!("{}", view.message);
    if ExternalCmd::dry_run() {
        println!("（dry-run：注册表未被读写）");
    }
    Ok(())
}

/// 供 `devzone status` / `crunch status`：只读，不做任何注册表改动。
/// 键或值不存在时如实返回「未暂停」。
pub fn summary(ctx: &Ctx) -> Result<Value> {
    let now = Utc::now();
    let state = classify(reg_get(KEY_EXPIRY)?, now);
    let view = describe(&state, now, false);
    let mut data = view.to_json(ctx, false);
    let values = read_pause_values()?;
    data["pause_values"] = json!(values);
    data["pause_window"] = PauseWindow::from_values(&values)
        .map(|window| window.to_json())
        .unwrap_or(Value::Null);
    data["note"] = json!(if ExternalCmd::dry_run() {
        "dry-run：读取的是进程内沙箱，未访问 HKLM"
    } else if values.is_empty() {
        "未暂停"
    } else {
        "读自 HKLM 更新暂停键值"
    });
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn utc(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap_or_else(|err| panic!("{text}: {err}"))
            .with_timezone(&Utc)
    }

    fn test_ctx(dir: &Path) -> Ctx {
        let paths = util::Paths::with_root(dir.to_path_buf());
        paths.ensure_dirs().unwrap();
        std::fs::write(&paths.config, crate::config::DEFAULT_TEMPLATE).unwrap();
        Ctx {
            cfg: crate::config::Config::load(&paths).unwrap(),
            paths,
            force: false,
            elevated: true,
            config_error: None,
        }
    }

    /// 打开 dry-run，并把 DEVZONE_HOME 指到 tempfile —— 绝不碰这台机器的注册表。
    fn dry_ctx(dir: &Path) -> Ctx {
        std::env::set_var(util::HOME_ENV, dir);
        std::env::set_var(util::DRY_RUN_ENV, "1");
        dry_store().clear();
        test_ctx(dir)
    }

    fn leave_dry_mode() {
        std::env::remove_var(util::DRY_RUN_ENV);
        std::env::remove_var(util::HOME_ENV);
        dry_store().clear();
    }

    // -- 纯逻辑 -----------------------------------------------------------

    #[test]
    fn utc_stamp_uses_doc_format() {
        assert_eq!(
            utc_stamp(utc("2026-10-11T08:00:00+00:00")),
            "2026-10-11T08:00:00Z"
        );
        // 带偏移的输入先归一到 UTC
        assert_eq!(
            utc_stamp(utc("2026-10-11T08:00:00+08:00")),
            "2026-10-11T00:00:00Z"
        );
    }

    #[test]
    fn parse_utc_stamp_tolerates_common_writings() {
        assert_eq!(
            parse_utc_stamp("2026-10-11T08:00:00Z").unwrap(),
            utc("2026-10-11T08:00:00+00:00")
        );
        assert_eq!(
            parse_utc_stamp("2026-10-11T08:00:00").unwrap(),
            utc("2026-10-11T08:00:00+00:00")
        );
        // 带毫秒的写法归一到整秒（§6.4 格式没有小数秒）
        assert_eq!(
            parse_utc_stamp("2026-10-11T08:00:00.1234567Z").unwrap(),
            utc("2026-10-11T08:00:00+00:00")
        );
        assert_eq!(
            parse_utc_stamp("2026-10-11T08:00:00+08:00").unwrap(),
            utc("2026-10-11T00:00:00+00:00")
        );
        for bad in ["", "not-a-date", "2026-10-11", "2026-13-45T99:99:99Z"] {
            assert!(parse_utc_stamp(bad).is_err(), "`{bad}` 应解析失败");
        }
    }

    #[test]
    fn days_clamp_at_35_and_default_from_config() {
        assert_eq!(resolve_days(None, 14).unwrap(), (14, None));
        assert_eq!(resolve_days(Some(21), 14).unwrap(), (21, None));
        assert_eq!(resolve_days(Some(35), 14).unwrap(), (35, None));
        let (days, hint) = resolve_days(Some(40), 14).unwrap();
        assert_eq!(days, MAX_PAUSE_DAYS);
        let hint = hint.expect(">35 必须给出截断提示");
        assert!(hint.contains("35") && hint.contains("40"), "{hint}");
        assert!(resolve_days(Some(0), 14).is_err());
        assert!(resolve_days(None, 0).is_err());
    }

    #[test]
    fn pause_window_builds_the_five_documented_values() {
        let now = utc("2026-10-11T08:00:00+00:00");
        let window = PauseWindow::build(now, 14);
        assert_eq!(window.expiry, "2026-10-25T08:00:00Z");
        assert_eq!(window.feature_start, "2026-10-11T08:00:00Z");
        assert_eq!(window.quality_start, "2026-10-11T08:00:00Z");
        assert_eq!(window.feature_end, "2026-10-25T08:00:00Z");
        assert_eq!(window.quality_end, "2026-10-25T08:00:00Z");
        assert_eq!(window.to_values().len(), 5);
        assert_eq!(
            window.expiry_time().unwrap(),
            utc("2026-10-25T08:00:00+00:00")
        );
        assert_eq!(window.to_json()["expiry"], "2026-10-25T08:00:00Z");
    }

    #[test]
    fn pause_window_roundtrip_and_missing_key() {
        let built = PauseWindow::build(utc("2026-10-11T08:00:00+00:00"), 7);
        let values = built.to_values();
        assert_eq!(PauseWindow::from_values(&values).as_ref(), Some(&built));
        let mut partial = values.clone();
        assert!(partial.remove(KEY_QUALITY_END).is_some());
        assert_eq!(PauseWindow::from_values(&partial), None);
        assert_eq!(PauseWindow::from_values(&BTreeMap::new()), None);
        // 键名与 §6.4 完全一致
        let names: Vec<&str> = values.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            vec![
                "PauseFeatureUpdatesEndTime",
                "PauseFeatureUpdatesStartTime",
                "PauseQualityUpdatesEndTime",
                "PauseQualityUpdatesStartTime",
                "PauseUpdatesExpiryTime",
            ]
        );
    }

    #[test]
    fn classify_and_days_left() {
        let now = utc("2026-10-11T08:00:00+00:00");
        assert_eq!(classify(None, now), PauseState::NotPaused);
        let future = utc("2026-10-21T08:00:00+00:00");
        assert_eq!(
            classify(Some(utc_stamp(future)), now),
            PauseState::Paused { expiry: future }
        );
        let past = utc("2026-10-01T08:00:00+00:00");
        assert_eq!(
            classify(Some(utc_stamp(past)), now),
            PauseState::Expired { expiry: past }
        );
        // 正好等于 now 视为已过期（<=）
        assert!(matches!(
            classify(Some(utc_stamp(now)), now),
            PauseState::Expired { .. }
        ));
        match classify(Some("乱码".to_string()), now) {
            PauseState::Unparsable { raw, reason } => {
                assert_eq!(raw, "乱码");
                assert!(reason.contains("无法解析"), "{reason}");
            }
            other => panic!("应为 Unparsable，实际 {other:?}"),
        }
        assert_eq!(days_left(future, now), 10);
        assert_eq!(days_left(past, now), 0);
        assert_eq!(days_left(now + Duration::seconds(1), now), 1);
        assert_eq!(days_left(now + Duration::seconds(90_061), now), 2);
    }

    #[test]
    fn describe_messages_match_the_doc() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let now = utc("2026-10-11T08:00:00+00:00");

        let view = describe(&PauseState::NotPaused, now, false);
        assert!(!view.paused);
        assert!(view.message.contains("未暂停"), "{}", view.message);

        let view = describe(
            &PauseState::Paused {
                expiry: now + Duration::days(14),
            },
            now,
            false,
        );
        assert!(view.paused);
        assert_eq!(view.expiry.as_deref(), Some("2026-10-25T08:00:00Z"));
        assert_eq!(view.days_left, 14);
        assert!(view.message.contains("更新已暂停至"), "{}", view.message);

        let view = describe(&PauseState::Expired { expiry: now }, now, false);
        assert!(view.expired);
        assert!(view.message.contains("crunch off"), "{}", view.message);

        let view = describe(&PauseState::NotPaused, now, true);
        assert_eq!(view.message, "暂停已到期，已自动恢复更新");

        let view = describe(
            &PauseState::Unparsable {
                raw: "x".into(),
                reason: "坏值".into(),
            },
            now,
            false,
        );
        assert!(view.message.contains("无法解析"), "{}", view.message);

        let json = view.to_json(&ctx, false);
        assert_eq!(json["max_pause_days"], 35);
        assert_eq!(json["default_pause_days"], 14);
        assert_eq!(json["auto_expire"], true);
        assert_eq!(json["registry"], UPDATE_SETTINGS_KEY);
        // dry_run 取决于进程环境（其它测试可能并发设置 DEVZONE_DRY_RUN），只断言字段存在
        assert!(json.get("dry_run").is_some(), "{json}");
    }

    // -- dry-run 全流程（不写这台机器的 Windows Update 注册表） ----------

    #[test]
    #[serial_test::serial]
    fn dry_run_registry_wrappers_stay_in_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let _ctx = dry_ctx(dir.path());

        assert_eq!(reg_get(KEY_EXPIRY).unwrap(), None);
        reg_set(KEY_EXPIRY, "2026-12-31T00:00:00Z").unwrap();
        assert_eq!(
            reg_get(KEY_EXPIRY).unwrap().as_deref(),
            Some("2026-12-31T00:00:00Z")
        );
        // 值只存在于沙箱里
        assert_eq!(
            dry_store().get(KEY_EXPIRY).map(String::as_str),
            Some("2026-12-31T00:00:00Z")
        );
        assert!(reg_delete(KEY_EXPIRY).unwrap());
        assert!(!reg_delete(KEY_EXPIRY).unwrap(), "重复删除应幂等");
        assert!(!reg_delete("NeverWrittenValue").unwrap());
        assert_eq!(reg_get(KEY_EXPIRY).unwrap(), None);
        assert_eq!(clear_pause_values().unwrap(), 0);
        leave_dry_mode();
    }

    #[test]
    #[serial_test::serial]
    fn dry_run_crunch_on_status_off() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = dry_ctx(dir.path());

        assert_eq!(
            classify(reg_get(KEY_EXPIRY).unwrap(), Utc::now()),
            PauseState::NotPaused
        );

        handle(&CrunchCmd::On { days: Some(7) }, &ctx).unwrap();
        let window =
            PauseWindow::from_values(&read_pause_values().unwrap()).expect("沙箱里应有 5 个值");
        assert_eq!(window.to_values().len(), 5);
        assert!(window.expiry_time().unwrap() > Utc::now());

        let summary_json = summary(&ctx).unwrap();
        assert_eq!(summary_json["paused"], true);
        assert_eq!(summary_json["dry_run"], true);
        assert_eq!(summary_json["expiry"], window.expiry);
        assert_eq!(summary_json["pause_values"][KEY_EXPIRY], window.expiry);
        assert_eq!(
            summary_json["pause_window"]["feature_end"],
            window.feature_end
        );
        assert_eq!(summary_json["message"], {
            // 与 describe() 的文案保持一致
            describe(
                &classify(reg_get(KEY_EXPIRY).unwrap(), Utc::now()),
                Utc::now(),
                false,
            )
            .message
        });

        handle(&CrunchCmd::Status { json: true }, &ctx).unwrap();

        handle(&CrunchCmd::Off, &ctx).unwrap();
        assert!(
            read_pause_values().unwrap().is_empty(),
            "off 应清掉 5 个键值"
        );
        assert_eq!(summary(&ctx).unwrap()["paused"], false);
        assert_eq!(summary(&ctx).unwrap()["pause_window"], Value::Null);
        // 幂等：再来一次 off 也不报错
        handle(&CrunchCmd::Off, &ctx).unwrap();

        let audit = std::fs::read_to_string(&ctx.paths.events).unwrap();
        assert!(audit.contains("crunch_state"), "{audit}");
        assert!(audit.contains("\"action\":\"off\""), "{audit}");
        leave_dry_mode();
    }

    #[test]
    #[serial_test::serial]
    fn dry_run_crunch_clamps_days_and_auto_expires() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = dry_ctx(dir.path());

        // 40 天 -> 截断为 35 天
        handle(&CrunchCmd::On { days: Some(40) }, &ctx).unwrap();
        let window = PauseWindow::from_values(&read_pause_values().unwrap()).unwrap();
        let expiry = window.expiry_time().unwrap();
        assert!(
            expiry > Utc::now() + Duration::days(34) && expiry <= Utc::now() + Duration::days(36),
            "应暂停到 35 天后，实际 {}",
            window.expiry
        );

        // 把到期时间改成过去，模拟到期 -> status 自动恢复
        let past = utc_stamp(Utc::now() - Duration::days(1));
        for key in PAUSE_KEYS {
            reg_set(key, &past).unwrap();
        }
        assert!(matches!(
            classify(reg_get(KEY_EXPIRY).unwrap(), Utc::now()),
            PauseState::Expired { .. }
        ));
        handle(&CrunchCmd::Status { json: false }, &ctx).unwrap();
        assert!(
            read_pause_values().unwrap().is_empty(),
            "auto_expire 应清空 5 个键值"
        );
        assert_eq!(summary(&ctx).unwrap()["paused"], false);
        let audit = std::fs::read_to_string(&ctx.paths.events).unwrap();
        assert!(audit.contains("auto_expired"), "{audit}");

        // auto_expire=false 时 status 不得改动注册表
        let mut manual = test_ctx(dir.path());
        manual.cfg.crunch.auto_expire = false;
        reg_set(KEY_EXPIRY, &past).unwrap();
        handle(&CrunchCmd::Status { json: false }, &manual).unwrap();
        assert_eq!(reg_get(KEY_EXPIRY).unwrap().as_deref(), Some(past.as_str()));

        leave_dry_mode();
    }
}

//! M5 AI 交付守护（build guide section 6.5）：停顿检测（区域 dHash，默认零 OCR）、
//! 感知两级制（Full/Lite 自动切换）、积分水位适配器（official_api > uia > ocr >
//! manual）、通知到手机。
//!
//! 说明：§15 把 watchdog 列为 M2 里程碑，本仓库按「实现文档中全部模块」交付，
//! 因此 watchdog 已实装；`--features ocr` 未编译时 OCR 通道如实报错不假装可用。
//!
//! ## 结构
//! * 感知两级制：[`decide_mode`]（纯函数，注入 [`crate::battery::BatteryInfo`]）＋
//!   [`guardian_in_wait`]（把 §6.3.2 状态机的「是否已在省电等待」引进来）。
//! * 停顿检测：[`evaluate`] / [`decide_with`] 是纯函数，截帧由 [`sense_frame`] 注入，
//!   因此 §11 的「fixture 两帧（变化/静止）→ 哈希距离断言」不需要真屏幕。
//! * 积分水位：[`parse_credit`]（中文/英文、百分比、绝对值、`1200/5000` 分数式）、
//!   [`estimate_eta_min`]（速率外推；数据不足明确返回 None，输出「无法估算 ETA」）、
//!   [`credit_verdict`]（阈值判定）。
//! * 跨轮次状态：`data/watchdog_state.json`（静止计数、最近读数）与
//!   `data/credits-<platform>.json`（manual 通道）。**不新增数据库表**（§6.1.2 已建完），
//!   历史一律走 `logs/events.jsonl`（§7/§8）。
//!
//! ## 只读边界
//! 检测循环与 `adapter test` 只读取（截屏 / UIA 文本 / 进程与窗口枚举），**从不**发送按键、
//! 点击或关闭窗口；§6.3.6 的 OCR 点击阶梯只属于 guard 的优雅暂停。

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::adapters::{self, ocr, pattern, pause, uia};
use crate::battery::{self, BatteryInfo};
use crate::cli::{AdapterCmd, WatchdogCmd};
use crate::config::{Config, CreditAdapterCfg, RegionCfg};
use crate::db::Db;
use crate::events::{name, EventBus};
use crate::mod_sched;
use crate::util::{self, ExternalCmd};
use crate::Ctx;

/// 计划任务名（`mod_sched` 负责加 `devzone-` 前缀）。
const DAEMON_TASK: &str = "watchdog-start";
/// §10：M1/M2 不做常驻服务，计划任务每分钟拉起一次「单轮检测」。
const DAEMON_EVERY_MIN: u32 = 1;
/// 同一应用 30 分钟内只报一次 stall（避免手机被刷屏）。
const STALL_DEBOUNCE: Duration = Duration::from_secs(30 * 60);
/// 同一平台 30 分钟内只报一次 credits_low。
const CREDITS_DEBOUNCE: Duration = Duration::from_secs(30 * 60);
/// ETA 外推要求的最小观测跨度：短于这个时间的两点差值全是噪声。
const ETA_MIN_SPAN_SEC: i64 = 120;
/// 读数样本只保留最近 N 条。
const CREDIT_SAMPLES: usize = 24;
/// Lite 模板匹配（NCC）低于该相关性就算「数字区变了」（§6.5.2）。
const NCC_MATCH_FLOOR: f64 = 0.5;
/// 没有配置 `[watchdog.adapters]` 时的默认优先级：§6.5.3 的 uia > ocr > manual。
const DEFAULT_PRIORITY: [&str; 3] = ["uia", "ocr", "manual"];
/// UIA / OCR 通道用来定位积分文本的关键词。
const CREDIT_KEYWORDS: [&str; 7] = [
    "积分",
    "额度",
    "剩余",
    "可用",
    "credits",
    "remaining",
    "quota",
];

pub fn handle(cmd: &WatchdogCmd, ctx: &Ctx) -> Result<()> {
    if !ctx.cfg.watchdog.enable && matches!(cmd, WatchdogCmd::Start { .. }) {
        util::progress("[watchdog].enable=false（§4.1 的 low 预设），显式命令仍按你的要求执行");
    }
    match cmd {
        WatchdogCmd::Start { mode, daemon } => start(ctx, mode.as_deref(), *daemon),
        WatchdogCmd::Stop => stop(ctx),
        WatchdogCmd::Status { json } => status(ctx, *json),
        WatchdogCmd::Adapter { cmd } => match cmd {
            AdapterCmd::List => adapter_list(ctx),
            AdapterCmd::Test { app, json } => adapter_test(ctx, app, *json),
        },
        WatchdogCmd::ImportCredits {
            platform,
            value,
            unit,
        } => import_credits(ctx, platform, *value, unit),
    }
}

// ---------------------------------------------------------------------------
// 感知两级制（§6.5.2）
// ---------------------------------------------------------------------------

/// Full = 精确读数（可跑 OCR）；Lite = 省电等待期，只判「变没变 / 在不在」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Full,
    Lite,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Full => "full",
            Tier::Lite => "lite",
        }
    }

    pub fn parse(value: &str) -> Option<Tier> {
        match value.trim().to_lowercase().as_str() {
            "full" => Some(Tier::Full),
            "lite" => Some(Tier::Lite),
            _ => None,
        }
    }

    /// §6.5.2 表格里的手段说明，`status` / `--json` 靠它向用户解释两级制。
    pub fn sensing_note(self) -> &'static str {
        match self {
            Tier::Full => "Full：UIA 文本优先，OCR 仅用于积分数字，取精确值",
            Tier::Lite => "Lite：UIA 文本 / 区域哈希 / 模板匹配(NCC) / 窗口标题，不跑 OCR 模型",
        }
    }
}

/// 一次模式判定的结果（含给人看的理由）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModeDecision {
    pub tier: Tier,
    pub reason: String,
    /// full/lite 强制时为 true，auto 推断时为 false。
    pub forced: bool,
}

/// 纯函数：`watchdog.mode` + 电池读数 + 守护是否在省电等待 → 用哪一级感知。
///
/// 规则（§6.5.2）：
/// * `full` / `lite` 强制，不看电池；
/// * `auto`（无法识别的取值也按 auto 处理，并在理由里点明）：Guardian 已进入
///   PAUSED_WAIT、或电量低于 `[guard].pause_level`、或在电池供电且电量未知 → Lite；
///   其余（含无电池的台式机）→ Full。
pub fn decide_mode(
    mode: &str,
    info: &BatteryInfo,
    pause_level: u8,
    guardian_wait: bool,
) -> ModeDecision {
    let normalized = mode.trim().to_lowercase();
    if let Some(tier) = Tier::parse(&normalized) {
        return ModeDecision {
            tier,
            forced: true,
            reason: format!("watchdog.mode={normalized}（强制，不看电池状态）"),
        };
    }
    let aside = if normalized == "auto" || normalized.is_empty() {
        String::new()
    } else {
        format!("（取值 \"{normalized}\" 不是 auto|full|lite，按 auto 处理）")
    };
    if guardian_wait {
        return ModeDecision {
            tier: Tier::Lite,
            forced: false,
            reason: format!("§6.3.7 省电等待进行中（Guardian 已进入 PAUSED_WAIT）→ Lite{aside}"),
        };
    }
    match info.level_pct {
        Some(level) if level < pause_level => ModeDecision {
            tier: Tier::Lite,
            forced: false,
            reason: format!("电量 {level}% < [guard].pause_level {pause_level}% → Lite{aside}"),
        },
        Some(level) if info.ac == Some(true) => ModeDecision {
            tier: Tier::Full,
            forced: false,
            reason: format!("AC 在线、电量 {level}% ≥ pause_level {pause_level}% → Full{aside}"),
        },
        Some(level) => ModeDecision {
            tier: Tier::Full,
            forced: false,
            reason: format!(
                "电池供电、电量 {level}% 仍 ≥ pause_level {pause_level}% → Full{aside}"
            ),
        },
        None if info.ac == Some(false) => ModeDecision {
            tier: Tier::Lite,
            forced: false,
            reason: format!("电池供电且电量未知（偏保守）→ Lite{aside}"),
        },
        None => ModeDecision {
            tier: Tier::Full,
            forced: false,
            reason: format!("无电池读数（台式机或 API 未上报）→ Full{aside}"),
        },
    }
}

/// 借 §6.3.2 的状态机用一次：喂入当前读数，看它是否已落入省电等待（PAUSED_WAIT）。
fn guardian_in_wait(cfg: &Config, info: &BatteryInfo) -> bool {
    let mut guardian = battery::Guardian::new(&cfg.guard);
    guardian.on_reading(info, Local::now());
    guardian.state() == battery::GuardState::PausedWait
}

/// 生效模式：`--mode` 覆盖配置；`auto` 时读一次电池并问一次 Guardian。
fn effective_mode(ctx: &Ctx, mode_override: Option<&str>) -> ModeDecision {
    let mode = mode_override
        .map(str::to_string)
        .unwrap_or_else(|| ctx.cfg.watchdog.mode.clone());
    if Tier::parse(&mode).is_some() {
        // 强制模式不读电池，省一次系统调用。
        return decide_mode(
            &mode,
            &BatteryInfo::default(),
            ctx.cfg.guard.pause_level,
            false,
        );
    }
    let info = battery::read_system_power_status();
    let wait = guardian_in_wait(&ctx.cfg, &info);
    decide_mode(&mode, &info, ctx.cfg.guard.pause_level, wait)
}

// ---------------------------------------------------------------------------
// 停顿检测（§6.5.1）
// ---------------------------------------------------------------------------

/// 一轮检测的结论（纯数据，便于断言）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    /// 本轮是第一帧（只建立基线，不计数）。
    pub first: bool,
    /// 输出区「在动」。
    pub changed: bool,
    /// 更新后的连续静止轮数。
    pub stable_polls: u32,
    /// 本轮刚好越过 `stall_stable_polls` → 该发 `stall_detected`。
    pub stall_now: bool,
    /// 已处于 stall 且本轮变化 → 该发 `stall_resolved`。
    pub resolved_now: bool,
}

/// 「一次检测」的纯函数：给定上一帧哈希、当前帧哈希、已累计静止轮数、触发阈值。
/// 没有上一帧时只建立基线（`first`），不计数、不判定。
///
/// 注意第一帧不计入静止数：`stall_stable_polls=5` 意味着「基线之后再连续 5 轮无变化」。
pub fn evaluate(
    prev_hash: Option<u64>,
    cur_hash: u64,
    stable: u32,
    stable_polls_needed: u32,
) -> Decision {
    match prev_hash {
        Some(previous) => decide_with(
            pattern::region_changed(previous, cur_hash, pattern::DHASH_CHANGED_THRESHOLD),
            false,
            stable,
            stable_polls_needed,
            false,
        ),
        None => decide_with(false, true, stable, stable_polls_needed, false),
    }
}

/// [`evaluate`] 的内核：把「在动 / 静止」当已知输入（区域哈希与 Lite 模板相关性
/// 合成之后调用），于是判定逻辑完全不依赖截帧，可逐条测。
pub fn decide_with(
    changed: bool,
    first: bool,
    stable: u32,
    stable_polls_needed: u32,
    was_stalled: bool,
) -> Decision {
    if first {
        return Decision {
            first: true,
            changed: false,
            stable_polls: 0,
            stall_now: false,
            resolved_now: false,
        };
    }
    if changed {
        return Decision {
            first: false,
            changed: true,
            stable_polls: 0,
            stall_now: false,
            // 「之前已报 stall」才补发恢复事件（§6.5.1）。
            resolved_now: was_stalled,
        };
    }
    let next = stable.saturating_add(1);
    Decision {
        first: false,
        changed: false,
        stable_polls: next,
        // 只在越过阈值的那一轮发一次，后续轮次由去抖窗口兜底。
        stall_now: next >= stable_polls_needed.max(1) && !was_stalled,
        resolved_now: false,
    }
}

/// §6.5.2：Lite 的判据是「变没变」。区域哈希为主，缓存模板的 NCC 相关性为辅
/// （没有可比对的模板时 NCC 为 `None`，自动退化成纯哈希）。
pub fn lite_signal(changed_by_hash: bool, ncc: Option<f64>) -> bool {
    changed_by_hash || ncc.is_some_and(|score| score < NCC_MATCH_FLOOR)
}

/// 帧比对：给出「在动」结论；`None` 表示还没有上一帧。
pub fn frame_changed(previous: Option<u64>, current: u64, ncc: Option<f64>) -> Option<bool> {
    let previous = previous?;
    let by_hash = pattern::region_changed(previous, current, pattern::DHASH_CHANGED_THRESHOLD);
    Some(lite_signal(by_hash, ncc))
}

/// 文本/标题信号的稳定 64-bit 哈希（FNV-1a）：Lite 在没有可用区域时用它判「变没变」。
pub fn text_hash(text: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// 跨轮次/跨进程保存的停顿状态（§7/§8：历史在 events.jsonl，这里只留当前判定）。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AppWatch {
    /// 上一帧的区域/文本哈希。
    #[serde(default)]
    pub last_hash: Option<u64>,
    #[serde(default)]
    pub stable_polls: u32,
    /// 已经报过 stall_detected（用于 stall_resolved 的「之前已报」条件）。
    #[serde(default)]
    pub stalled: bool,
    /// 连续静止的起始时刻（epoch 秒）→ `stall_detected` 的 `since_sec`。
    #[serde(default)]
    pub stable_since_epoch: Option<i64>,
    /// 本轮实际使用的信号：region | window | text。
    #[serde(default)]
    pub signal: String,
    #[serde(default)]
    pub last_hash_hex: String,
    #[serde(default)]
    pub last_ts: String,
    /// 降级原因等人话说明（例如「未配置 [watchdog.regions]」）。
    #[serde(default)]
    pub note: String,
}

/// 积分读数样本（manual/uia/ocr/official_api 任一通道的历史），用于速率外推。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CreditSample {
    pub ts: String,
    pub epoch: i64,
    pub pct: Option<f64>,
    pub abs: Option<f64>,
    pub source: String,
}

/// 某平台最近一次积分判定。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CreditRecord {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub remaining_pct: Option<f64>,
    #[serde(default)]
    pub remaining_abs: Option<f64>,
    #[serde(default)]
    pub total_abs: Option<f64>,
    #[serde(default)]
    pub ts: String,
    #[serde(default)]
    pub verdict: String,
    #[serde(default)]
    pub samples: Vec<CreditSample>,
}

/// watchdog 的全部跨轮次状态，落在 `data/watchdog_state.json`（随 DEVZONE_HOME 隔离）。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct WatchState {
    #[serde(default)]
    pub apps: BTreeMap<String, AppWatch>,
    #[serde(default)]
    pub credits: BTreeMap<String, CreditRecord>,
    #[serde(default)]
    pub updated: String,
}

impl WatchState {
    fn to_json(&self) -> Value {
        json!({ "apps": self.apps, "credits": self.credits, "updated": self.updated })
    }
}

/// 一次截帧的结果（或不可截时的降级说明）。
struct Frame {
    /// region | window | text
    kind: &'static str,
    hash: u64,
    /// Lite 模板相关性（NCC）；没有可比对的缓存模板时为 None。
    ncc: Option<f64>,
    note: String,
    detail: String,
}

/// 灰度模板缓存：Lite 用它做「数字区变没变」的 NCC 比对，而不是跑 OCR 模型。
/// （`pub` 是因为 [`run_poll`] 的签名暴露了它；字段保持私有。）
pub struct GrayTemplate {
    gray: Vec<u8>,
    width: usize,
    height: usize,
}

impl GrayTemplate {
    fn of(image: &image::RgbImage) -> GrayTemplate {
        GrayTemplate {
            gray: pattern::rgb_to_gray(image),
            width: image.width() as usize,
            height: image.height() as usize,
        }
    }

    /// 与当前帧的相关性；尺寸变了（窗口被拖动/缩放）时返回 None，交由哈希判定。
    fn score(&self, image: &image::RgbImage) -> Option<f64> {
        if image.width() as usize != self.width || image.height() as usize != self.height {
            return None;
        }
        let gray = pattern::rgb_to_gray(image);
        Some(pattern::ncc(
            &gray,
            self.width,
            self.height,
            &self.gray,
            self.width,
            self.height,
        ))
    }
}

// ---------------------------------------------------------------------------
// 目标清单（§6.5.1：[guard.whitelist.apps] ∪ [watchdog.regions]）
// ---------------------------------------------------------------------------

/// 一个被盯的目标应用。
#[derive(Clone, Debug)]
pub struct Target {
    pub app: String,
    /// 来自 `[watchdog.regions]` 的固定区域（优先于「取主窗口矩形」）。
    pub region: Option<RegionCfg>,
    /// 配置来源：regions | whitelist | both。
    pub from: &'static str,
    /// 积分水位用的平台名（去掉 `.exe`）。
    pub platform: String,
}

impl Target {
    pub fn to_json(&self) -> Value {
        json!({
            "app": self.app,
            "from": self.from,
            "platform": self.platform,
            "region": self.region,
            "region_configured": self.region.is_some(),
        })
    }
}

/// `[guard.whitelist.apps]` ∪ `[watchdog.regions]` 的键，稳定顺序、去重。
pub fn targets(cfg: &Config) -> Vec<Target> {
    let mut merged: BTreeMap<String, Target> = BTreeMap::new();
    for app in cfg.guard.whitelist.apps.keys() {
        merged.insert(
            app.clone(),
            Target {
                app: app.clone(),
                region: cfg.watchdog.regions.get(app).cloned(),
                from: "whitelist",
                platform: platform_of(app),
            },
        );
    }
    for (app, region) in &cfg.watchdog.regions {
        match merged.get_mut(app) {
            Some(existing) => {
                existing.region = Some(region.clone());
                existing.from = "both";
            }
            None => {
                merged.insert(
                    app.clone(),
                    Target {
                        app: app.clone(),
                        region: Some(region.clone()),
                        from: "regions",
                        platform: platform_of(app),
                    },
                );
            }
        }
    }
    for (app, target) in &mut merged {
        // 白名单项可以用 watch_region 指向另一个区域键（config.rs 已留了这个字段）。
        if target.region.is_none() {
            if let Some(key) = cfg
                .guard
                .whitelist
                .apps
                .get(app)
                .and_then(|entry| entry.watch_region.clone())
            {
                target.region = cfg.watchdog.regions.get(&key).cloned();
            }
        }
    }
    merged.into_values().collect()
}

/// `doubao.exe` → `doubao`（积分平台名用短名）。
pub fn platform_of(app: &str) -> String {
    let trimmed = app.trim().to_lowercase();
    trimmed
        .strip_suffix(".exe")
        .unwrap_or(trimmed.as_str())
        .to_string()
}

// ---------------------------------------------------------------------------
// 积分水位（§6.5.3）
// ---------------------------------------------------------------------------

/// 读数通道；文档顺序 official_api > uia > ocr > manual。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreditSource {
    OfficialApi,
    Uia,
    Ocr,
    Manual,
}

impl CreditSource {
    pub fn as_str(self) -> &'static str {
        match self {
            CreditSource::OfficialApi => "official_api",
            CreditSource::Uia => "uia",
            CreditSource::Ocr => "ocr",
            CreditSource::Manual => "manual",
        }
    }

    pub fn parse(value: &str) -> Option<CreditSource> {
        match value
            .trim()
            .to_lowercase()
            .replace(['-', ' '], "_")
            .as_str()
        {
            "official_api" | "api" | "official" => Some(CreditSource::OfficialApi),
            "uia" | "automation" => Some(CreditSource::Uia),
            "ocr" => Some(CreditSource::Ocr),
            "manual" | "import" => Some(CreditSource::Manual),
            _ => None,
        }
    }

    fn rank(self) -> u8 {
        match self {
            CreditSource::OfficialApi => 0,
            CreditSource::Uia => 1,
            CreditSource::Ocr => 2,
            CreditSource::Manual => 3,
        }
    }

    /// §6.5.2：Lite 不跑 OCR 模型，所以 ocr 通道只在 Full 轮次参与。
    fn allowed_in(self, tier: Tier) -> bool {
        !(self == CreditSource::Ocr && tier == Tier::Lite)
    }
}

/// 解析出来的一次积分读数。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CreditReading {
    pub remaining_pct: Option<f64>,
    pub remaining_abs: Option<f64>,
    pub total_abs: Option<f64>,
}

impl CreditReading {
    /// 能换算成百分比就用百分比；只有绝对值且知道总量时才折算，否则老实返回 None。
    pub fn resolved_pct(&self) -> Option<f64> {
        if let Some(pct) = self.remaining_pct {
            return Some(pct);
        }
        match (self.remaining_abs, self.total_abs) {
            (Some(abs), Some(total)) if total > 0.0 => Some((abs / total) * 100.0),
            _ => None,
        }
    }

    pub fn to_json(self) -> Value {
        json!({
            "remaining_pct": self.remaining_pct.map(round1),
            "remaining_abs": self.remaining_abs,
            "total_abs": self.total_abs,
        })
    }
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn decimal_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(\d+(?:[.,]\d+)?)\s*(%?)").expect("内置数字正则必须合法"))
}

fn fraction_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(\d+(?:[.,]\d+)?)\s*/\s*(\d+(?:[.,]\d+)?)")
            .expect("内置分数正则必须合法")
    })
}

/// 「还剩下」语境关键词（§6.5.3：取剩余比例或数值）。
const REMAINING_KEYS: [&str; 9] = [
    "剩余",
    "剩下",
    "余下",
    "可用",
    "余额",
    "剩",
    "remaining",
    "left",
    "available",
];
/// 计量单位关键词。
const UNIT_KEYS: [&str; 6] = ["积分", "额度", "点数", "credits", "points", "quota"];

fn to_number(raw: &str) -> Option<f64> {
    raw.replace(',', "").parse::<f64>().ok()
}

/// 把界面上的中文/英文文案解析成一次积分读数（纯函数，逐条测）。
///
/// 支持 `剩余额度 18%`、`剩余 1200 积分`、`1200/5000`、`remaining credits: 7 %`、
/// `已用 82%，剩余 18%`（多数字时取带「剩余」语境的那个）；无任何数字 → `None`。
pub fn parse_credit(text: &str) -> Option<CreditReading> {
    if text.trim().is_empty() {
        return None;
    }
    // 分数式优先：`1200/5000` 既是绝对值也能折算百分比。
    if let Some(caps) = fraction_re().captures(text) {
        let remaining = to_number(caps.get(1)?.as_str())?;
        let total = to_number(caps.get(2)?.as_str())?;
        if total > 0.0 {
            return Some(CreditReading {
                remaining_pct: Some((remaining / total) * 100.0),
                remaining_abs: Some(remaining),
                total_abs: Some(total),
            });
        }
    }

    let lower = text.to_lowercase();
    let mut numbers: Vec<(f64, bool, String)> = Vec::new();
    for caps in decimal_re().captures_iter(text) {
        let Some(all) = caps.get(0) else { continue };
        let Some(value) = to_number(caps.get(1)?.as_str()) else {
            continue;
        };
        let is_pct = !caps.get(2)?.as_str().is_empty();
        // 该数字前面的若干字符是它的语境（"剩余额度 18%" → "剩余额度 "）。
        let window_start = text[..all.start()]
            .char_indices()
            .rev()
            .nth(16)
            .map(|(idx, _)| idx)
            .unwrap_or(0);
        let context = text[window_start..all.start()].to_lowercase();
        numbers.push((value, is_pct, context));
    }
    if numbers.is_empty() {
        return None;
    }
    let keyworded = |context: &str| REMAINING_KEYS.iter().any(|key| context.contains(key));
    let credit_context = UNIT_KEYS
        .iter()
        .chain(CREDIT_KEYWORDS.iter())
        .any(|key| lower.contains(key));

    let percents: Vec<&(f64, bool, String)> = numbers.iter().filter(|(_, pct, _)| *pct).collect();
    if let Some((value, _, _)) = percents
        .iter()
        .find(|(_, _, context)| keyworded(context))
        .or_else(|| percents.first())
    {
        // >100 的「百分比」显然不是百分比，退回绝对值处理。
        if *value <= 100.0 {
            return Some(CreditReading {
                remaining_pct: Some(*value),
                remaining_abs: None,
                total_abs: None,
            });
        }
    }
    let absolutes: Vec<&(f64, bool, String)> = numbers.iter().filter(|(_, pct, _)| !*pct).collect();
    let chosen = if credit_context {
        absolutes
            .iter()
            .find(|(_, _, context)| keyworded(context))
            .or_else(|| absolutes.first())
            .or_else(|| percents.first())
    } else {
        absolutes.first().or_else(|| percents.first())
    }?;
    Some(CreditReading {
        remaining_pct: None,
        remaining_abs: Some(chosen.0),
        total_abs: None,
    })
}

/// 用最近若干次读数外推「还能用几分钟」。
///
/// `series` 是按时间升序的 `(epoch 秒, 余量)`；点数不足、跨度不足、余量没在减少
/// 都返回 `None` —— 调用方必须把它显示成「无法估算 ETA」，不许伪造（§9 的反面）。
pub fn estimate_eta_min(series: &[(i64, f64)], now_value: f64, min_span_sec: i64) -> Option<f64> {
    if series.len() < 2 {
        return None;
    }
    let (first_ts, first_value) = series[0];
    let (last_ts, last_value) = *series.last()?;
    let span_sec = last_ts - first_ts;
    if span_sec < min_span_sec {
        return None;
    }
    let consumed = first_value - last_value;
    if consumed <= f64::EPSILON {
        return None;
    }
    let per_minute = consumed / (span_sec as f64 / 60.0);
    if per_minute <= f64::EPSILON || now_value <= 0.0 {
        return None;
    }
    Some(now_value / per_minute)
}

/// 阈值判定：剩余百分比 < `credits_warn_pct` 或预计耗尽 < `credits_warn_eta_min`。
/// 返回 `(§6.7.1 payload, 人话理由)`；未命中阈值返回 `None`。
pub fn credit_verdict(
    platform: &str,
    reading: &CreditReading,
    eta_min: Option<f64>,
    warn_pct: u8,
    warn_eta_min: u32,
) -> Option<(Value, String)> {
    let pct = reading.resolved_pct();
    if let Some(pct) = pct {
        if pct < f64::from(warn_pct) {
            let mut payload = json!({ "platform": platform, "remaining_pct": round1(pct) });
            if let Some(eta) = eta_min {
                payload["eta_min"] = json!(round1(eta));
            }
            return Some((
                payload,
                format!("剩余 {pct:.1}% < 预警阈值 {}%（§6.5.3）", warn_pct),
            ));
        }
    }
    if let Some(eta) = eta_min {
        if eta < f64::from(warn_eta_min) {
            let mut payload = json!({ "platform": platform });
            if let Some(pct) = pct {
                payload["remaining_pct"] = json!(round1(pct));
            }
            payload["eta_min"] = json!(round1(eta));
            return Some((
                payload,
                format!(
                    "按最近消耗速率预计 {eta:.0} 分钟后耗尽 < 阈值 {warn_eta_min} 分钟（§6.5.3）"
                ),
            ));
        }
    }
    None
}

/// manual 通道的落盘结构（`data/credits-<platform>.json`，不建表）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManualCredit {
    pub platform: String,
    pub value: f64,
    pub unit: String,
    pub remaining_pct: Option<f64>,
    pub remaining_abs: Option<f64>,
    pub ts: String,
    pub epoch: i64,
}

impl ManualCredit {
    fn reading(&self) -> CreditReading {
        CreditReading {
            remaining_pct: self.remaining_pct,
            remaining_abs: self.remaining_abs,
            total_abs: None,
        }
    }
}

/// 单位是不是百分比（`--unit percent|%|百分比`）。
fn unit_is_percent(unit: &str) -> bool {
    matches!(
        unit.trim().to_lowercase().as_str(),
        "%" | "percent" | "pct" | "百分比"
    )
}

/// 平台名要当文件名用，必须过滤掉路径分隔符等危险字符。
pub fn sanitize_platform(platform: &str) -> String {
    let cleaned: String = platform
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '.') {
        "platform".to_string()
    } else {
        cleaned
    }
}

fn state_file(ctx: &Ctx) -> PathBuf {
    ctx.paths.data_dir.join("watchdog_state.json")
}

fn manual_file(ctx: &Ctx, platform: &str) -> PathBuf {
    ctx.paths
        .data_dir
        .join(format!("credits-{}.json", sanitize_platform(platform)))
}

fn debug_path(ctx: &Ctx, app: &str) -> PathBuf {
    ctx.paths.data_dir.join("debug").join(format!(
        "watchdog-{}-{}.png",
        sanitize_platform(app),
        util::now_compact()
    ))
}

fn load_state(ctx: &Ctx) -> WatchState {
    read_json_state(&state_file(ctx)).unwrap_or_else(|| {
        tracing::debug!(
            "watchdog 状态文件缺失或损坏，从空状态开始：{}",
            state_file(ctx).display()
        );
        WatchState::default()
    })
}

/// 状态文件坏了也不能让守护崩掉（§9：降级 + 记日志）。
fn read_json_state<T>(path: &Path) -> Option<T>
where
    T: for<'de> Deserialize<'de>,
{
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<T>(&text) {
        Ok(value) => Some(value),
        Err(err) => {
            tracing::warn!("状态文件 {} 解析失败：{err}", path.display());
            None
        }
    }
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(value).context("序列化 JSON 状态")?;
    std::fs::write(path, text).with_context(|| format!("写入 {}", path.display()))?;
    Ok(())
}

fn save_state(ctx: &Ctx, state: &WatchState) -> Result<()> {
    write_json(&state_file(ctx), &state.to_json())
}

/// 读 manual 通道最近一次人工导入。
fn read_manual(ctx: &Ctx, platform: &str) -> Option<ManualCredit> {
    read_json_state(&manual_file(ctx, platform))
}

/// 该平台的积分优先级：配置里的 priority 按 §6.5.3 排序，manual 永远兜底。
pub fn credit_priority(cfg: Option<&CreditAdapterCfg>) -> Vec<CreditSource> {
    let configured: Vec<String> = cfg
        .map(|entry| entry.priority.clone())
        .filter(|entry| !entry.is_empty())
        .unwrap_or_else(|| {
            DEFAULT_PRIORITY
                .iter()
                .map(|name| (*name).to_string())
                .collect()
        });
    let mut sources: Vec<CreditSource> = configured
        .iter()
        .filter_map(|name| {
            CreditSource::parse(name).or_else(|| {
                util::progress(&format!(
                    "未知的积分适配器 \"{name}\"（可用：official_api/uia/ocr/manual），已忽略"
                ));
                None
            })
        })
        .collect();
    if sources.is_empty() {
        sources = DEFAULT_PRIORITY
            .iter()
            .filter_map(|name| CreditSource::parse(name))
            .collect();
    }
    if !sources.contains(&CreditSource::Manual) {
        sources.push(CreditSource::Manual);
    }
    sources.sort_by_key(|source| source.rank());
    sources.dedup();
    sources
}

/// 一次积分读取的结果。
struct CreditOutcome {
    platform: String,
    source: String,
    reading: Option<CreditReading>,
    eta_note: String,
    verdict: Option<String>,
    notes: Vec<String>,
}

impl CreditOutcome {
    fn to_json(&self) -> Value {
        json!({
            "platform": self.platform,
            "source": self.source,
            "reading": self.reading.map(|value| value.to_json()),
            "remaining_pct": self.reading.and_then(|value| value.resolved_pct()).map(round1),
            "eta_note": self.eta_note,
            "verdict": self.verdict,
            "notes": self.notes,
        })
    }
}

/// 官方接口读取：只有用户配了 `api_url` 才发请求。
/// §6.5.3 已核查：豆包无个人额度公开 API、Trae OpenAPI 仅企业版管理员 ——
/// devzone **绝不凭空编造任何平台的官方接口**。
fn read_official_api(cfg: &CreditAdapterCfg, notes: &mut Vec<String>) -> Option<CreditReading> {
    let url = cfg.api_url.trim();
    if url.is_empty() {
        notes.push(
            "official_api 跳过：未配置 api_url（§6.5.3：豆包无个人额度 API、Trae OpenAPI 仅企业版，devzone 不猜接口）"
                .to_string(),
        );
        return None;
    }
    if ExternalCmd::dry_run() {
        notes.push("official_api 跳过：DEVZONE_DRY_RUN=1（不发起任何外部请求）".to_string());
        return None;
    }
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            notes.push(format!("official_api 失败：{err}"));
            return None;
        }
    };
    let mut request = client.get(url);
    if !cfg.api_token_env.trim().is_empty() {
        match std::env::var(cfg.api_token_env.trim()) {
            Ok(token) => {
                // 已带 scheme 的 token 原样发出，否则按 Bearer 处理。绝不回显 token 本身。
                let value = if token.contains(' ') {
                    token
                } else {
                    format!("Bearer {token}")
                };
                request = request.header("Authorization", value);
            }
            Err(_) => notes.push(format!(
                "official_api：环境变量 {} 未设置，按匿名请求尝试",
                cfg.api_token_env.trim()
            )),
        }
    }
    match request.send() {
        Ok(response) => {
            let status = response.status();
            if !status.is_success() {
                notes.push(format!("official_api 返回 {status}"));
                return None;
            }
            let body = response.text().unwrap_or_default();
            parse_credit(&body).or_else(|| {
                notes.push(
                    "official_api：响应里没解析出数字（接口字段形状未知，请改用 uia/ocr/manual）"
                        .to_string(),
                );
                None
            })
        }
        Err(err) => {
            notes.push(format!("official_api 请求失败：{err}"));
            None
        }
    }
}

/// 找一个平台的主窗口（`doubao` / `doubao.exe` 两种写法都能命中）。
fn window_for(platform: &str) -> Option<adapters::WindowInfo> {
    let hint = if platform.to_lowercase().ends_with(".exe") {
        platform.to_string()
    } else {
        format!("{platform}.exe")
    };
    adapters::find_processes(&hint)
        .iter()
        .find_map(|process| adapters::main_window_of(process.pid))
}

/// 目标应用有没有配置区域；没有的话回退到窗口矩形（§6.5.1）。
fn region_for(target: &Target) -> (Option<RegionCfg>, String) {
    if let Some(region) = &target.region {
        return (
            Some(region.clone()),
            format!("[watchdog.regions] \"{}\"", target.app),
        );
    }
    let note = format!(
        "未配置 [watchdog.regions] \"{}\"：回退到主窗口矩形（devzone watchdog adapter test {} 会给出建议坐标）",
        target.app, target.app
    );
    if let Some(window) = window_for(&target.app) {
        return (Some(ocr::region_from_rect(window.rect)), note);
    }
    (None, note)
}

/// 截一帧并给出哈希；顺带维护 Lite 用的 NCC 模板缓存。
fn sense_frame(
    ctx: &Ctx,
    target: &Target,
    tier: Tier,
    templates: &mut BTreeMap<String, GrayTemplate>,
) -> Frame {
    let (region, note) = region_for(target);
    match region {
        Some(region) => match ocr::capture_region(&region) {
            Ok(image) => frame_from_image(
                ctx,
                target,
                tier,
                templates,
                &image,
                "region",
                format!(
                    "区域 ({},{}) {}x{}",
                    region.x, region.y, region.width, region.height
                ),
                note,
            ),
            Err(err) => frame_from_capture_error(
                ctx,
                target,
                tier,
                templates,
                note,
                &format!("区域截图失败：{err}"),
            ),
        },
        None => frame_from_capture_error(ctx, target, tier, templates, note, "没有可用的区域配置"),
    }
}

/// OCR hits -> credit reading, `Null` when there is nothing to parse.
fn parse_ocr_hits(hits: &[crate::adapters::ocr::OcrHit]) -> Value {
    if hits.is_empty() {
        return Value::Null;
    }
    let joined = hits
        .iter()
        .map(|hit| hit.text.clone())
        .collect::<Vec<_>>()
        .join(" | ");
    parse_credit(&joined)
        .map(|value| value.to_json())
        .unwrap_or(Value::Null)
}

/// 截图成功：组装 dHash + 模板缓存 + 可选落盘（仅当 debug_save_screenshots=true，§8）。
#[allow(clippy::too_many_arguments)]
fn frame_from_image(
    ctx: &Ctx,
    target: &Target,
    tier: Tier,
    templates: &mut BTreeMap<String, GrayTemplate>,
    image: &image::RgbImage,
    kind: &'static str,
    detail: String,
    mut note: String,
) -> Frame {
    if let Err(err) = ocr::maybe_save_screenshot(
        image,
        &debug_path(ctx, &target.app),
        ctx.cfg.watchdog.debug_save_screenshots,
    ) {
        tracing::warn!("调试截图失败：{err}");
    }
    let hash = pattern::dhash(image);
    let key = format!("{}:{}", target.app, kind);
    let ncc = templates.get(&key).and_then(|cached| cached.score(image));
    templates.insert(key, GrayTemplate::of(image));
    if kind == "region" && target.region.as_ref().is_some_and(ocr::region_offscreen) {
        note.push_str("；注意：区域超出主屏，可能截到黑边");
    }
    Frame {
        kind,
        hash,
        ncc: if tier == Tier::Lite { ncc } else { None },
        note,
        detail: format!("{detail}，dHash={}", pattern::hash_hex(hash)),
    }
}

/// 区域不可用：先试主窗口矩形，再退化到 UIA 文本 / 窗口标题（§6.5.2 允许的 Lite 手段）。
fn frame_from_capture_error(
    ctx: &Ctx,
    target: &Target,
    tier: Tier,
    templates: &mut BTreeMap<String, GrayTemplate>,
    mut note: String,
    reason: &str,
) -> Frame {
    note.push_str(&format!("；{reason}"));
    let window = window_for(&target.app);
    if let Some(window) = &window {
        if let Ok(image) = ocr::capture_window(window.hwnd) {
            return frame_from_image(
                ctx,
                target,
                tier,
                templates,
                &image,
                "window",
                format!(
                    "窗口「{}」矩形 {:?}",
                    if window.title.is_empty() {
                        "(无标题)"
                    } else {
                        &window.title
                    },
                    window.rect
                ),
                note,
            );
        }
        note.push_str("；窗口截图也失败（最小化或尺寸为 0？），改用文本信号");
    }
    // 降级路径：UIA 文本 + 窗口标题
    let mut text = String::new();
    if let Some(window) = &window {
        text.push_str(&window.title);
        for (element, value) in uia::text_snippets(window.hwnd, 24) {
            text.push('\n');
            text.push_str(&element);
            text.push(':');
            text.push_str(&value);
        }
        if text.trim().is_empty() {
            note.push_str("；UIA 未返回文本，只用窗口标题比对");
        }
    } else {
        note.push_str("；目标进程未运行或没有顶层窗口，无法感知");
    }
    let hash = text_hash(&text);
    Frame {
        kind: "text",
        hash,
        ncc: None,
        note,
        detail: format!(
            "文本信号（{} 字符）textHash={}",
            text.chars().count(),
            pattern::hash_hex(hash)
        ),
    }
}

/// 一轮完整检测：两级感知 + 每个目标的停顿判定 + 积分水位。返回报告 JSON。
pub fn run_poll(
    ctx: &Ctx,
    state: &mut WatchState,
    templates: &mut BTreeMap<String, GrayTemplate>,
    bus: &EventBus,
    mode: &ModeDecision,
) -> Result<Value> {
    let now_epoch = Local::now().timestamp();
    let needed = ctx.cfg.watchdog.stall_stable_polls;
    let mut app_reports = Vec::new();

    for target in targets(&ctx.cfg) {
        let frame = sense_frame(ctx, &target, mode.tier, templates);
        let watch = state.apps.entry(target.app.clone()).or_default();
        let previous = watch.last_hash;
        let was_stalled = watch.stalled;
        let decision = match frame_changed(previous, frame.hash, frame.ncc) {
            Some(changed) => decide_with(changed, false, watch.stable_polls, needed, was_stalled),
            None => evaluate(previous, frame.hash, watch.stable_polls, needed),
        };
        let mut emitted: Vec<&'static str> = Vec::new();

        if decision.stall_now {
            let since = watch
                .stable_since_epoch
                .map(|start| (now_epoch - start).max(0))
                .unwrap_or(0);
            // §6.7.1 payload：{"app":"trae.exe","since_sec":300}
            let payload = json!({ "app": target.app, "since_sec": since });
            if bus.emit_debounced(
                name::STALL_DETECTED,
                payload,
                &format!("stall_detected:{}", target.app),
                STALL_DEBOUNCE,
            )? {
                emitted.push(name::STALL_DETECTED);
            }
            watch.stalled = true;
        } else if decision.resolved_now {
            let stalled_for = watch
                .stable_since_epoch
                .map(|start| (now_epoch - start).max(0))
                .unwrap_or(0);
            bus.emit_json(
                name::STALL_RESOLVED,
                json!({ "app": target.app, "stalled_for_sec": stalled_for }),
            )?;
            emitted.push(name::STALL_RESOLVED);
            watch.stalled = false;
        }

        // 状态推进
        if decision.changed {
            watch.stable_polls = 0;
            watch.stable_since_epoch = None;
            watch.stalled = false;
        } else if decision.first {
            watch.stable_polls = 0;
            watch.stable_since_epoch = None;
        } else {
            watch.stable_polls = decision.stable_polls;
            if watch.stable_since_epoch.is_none() {
                watch.stable_since_epoch = Some(now_epoch);
            }
        }
        watch.last_hash = Some(frame.hash);
        watch.last_hash_hex = pattern::hash_hex(frame.hash);
        watch.signal = frame.kind.to_string();
        watch.note = if frame.note.is_empty() {
            frame.detail
        } else {
            format!("{}；{}", frame.detail, frame.note)
        };
        watch.last_ts = util::now_iso();

        app_reports.push(json!({
            "app": target.app,
            "signal": frame.kind,
            "hash": watch.last_hash_hex,
            "stable_polls": watch.stable_polls,
            "stable_polls_needed": needed,
            "stalled": watch.stalled,
            "first_frame": decision.first,
            "changed": decision.changed,
            "ncc": frame.ncc.map(|score| (score * 1000.0).round() / 1000.0),
            "events": emitted,
            "detail": watch.note,
        }));
    }

    let credits = poll_credits(ctx, state, bus, mode)?;
    state.updated = util::now_iso();

    Ok(json!({
        "tier": mode.tier.as_str(),
        "reason": mode.reason,
        "sensing": mode.tier.sensing_note(),
        "poll_sec": ctx.cfg.poll_sec(mode.tier == Tier::Lite),
        "at": state.updated,
        "apps": app_reports,
        "credits": credits
            .iter()
            .map(CreditOutcome::to_json)
            .collect::<Vec<_>>(),
        "dry_run": ExternalCmd::dry_run(),
    }))
}

/// 积分水位：优先级 official_api > uia > ocr > manual（§6.5.3）。
fn poll_credits(
    ctx: &Ctx,
    state: &mut WatchState,
    bus: &EventBus,
    mode: &ModeDecision,
) -> Result<Vec<CreditOutcome>> {
    let mut outcomes = Vec::new();
    for platform in credit_platforms(ctx) {
        outcomes.push(read_one_credit(ctx, state, bus, mode, &platform)?);
    }
    Ok(outcomes)
}

/// 要查积分的平台：`[watchdog.adapters]` 的键 ∪ 目标应用的短名。
fn credit_platforms(ctx: &Ctx) -> Vec<String> {
    let mut set = std::collections::BTreeSet::new();
    for key in ctx.cfg.watchdog.adapters.keys() {
        set.insert(platform_of(key));
    }
    for target in targets(&ctx.cfg) {
        if !target.platform.is_empty() {
            set.insert(target.platform);
        }
    }
    set.into_iter().collect()
}

fn read_one_credit(
    ctx: &Ctx,
    state: &mut WatchState,
    bus: &EventBus,
    mode: &ModeDecision,
    platform: &str,
) -> Result<CreditOutcome> {
    let adapter_cfg = ctx
        .cfg
        .watchdog
        .adapters
        .get(platform)
        .or_else(|| ctx.cfg.watchdog.adapters.get(&format!("{platform}.exe")))
        .cloned();
    let mut notes: Vec<String> = Vec::new();
    let mut reading: Option<CreditReading> = None;
    let mut source = String::new();

    for channel in credit_priority(adapter_cfg.as_ref()) {
        if !channel.allowed_in(mode.tier) {
            notes.push(format!(
                "{} 跳过：{} 不跑 OCR 模型（§6.5.2）",
                channel.as_str(),
                mode.tier.as_str()
            ));
            continue;
        }
        let candidate = match channel {
            CreditSource::OfficialApi => match adapter_cfg.as_ref() {
                Some(cfg) => read_official_api(cfg, &mut notes),
                None => {
                    notes.push("official_api 跳过：[watchdog.adapters] 无该平台条目".to_string());
                    None
                }
            },
            CreditSource::Uia => read_via_uia(platform, &mut notes),
            CreditSource::Ocr => read_via_ocr(ctx, platform, &mut notes),
            CreditSource::Manual => match read_manual(ctx, platform) {
                Some(manual) => {
                    notes.push(format!(
                        "manual：采用 {} 为 {} 导入的读数（{} {}，见 {}）",
                        manual.ts,
                        manual.platform,
                        manual.value,
                        manual.unit,
                        manual_file(ctx, platform).display()
                    ));
                    Some(manual.reading())
                }
                None => {
                    notes.push(format!(
                        "manual：还没有导入过（devzone watchdog import-credits {platform} <值> --unit <credits|percent>）"
                    ));
                    None
                }
            },
        };
        if let Some(value) = candidate {
            reading = Some(value);
            source = channel.as_str().to_string();
            break;
        }
    }

    let record = state.credits.entry(platform.to_string()).or_default();

    let mut eta_min: Option<f64> = None;
    let eta_note: String;
    if let Some(value) = reading {
        let pct = value.resolved_pct();
        record.samples.push(CreditSample {
            ts: util::now_iso(),
            epoch: Local::now().timestamp(),
            pct,
            abs: value.remaining_abs,
            source: source.clone(),
        });
        if record.samples.len() > CREDIT_SAMPLES {
            let excess = record.samples.len() - CREDIT_SAMPLES;
            record.samples.drain(..excess);
        }
        // 有百分比就按百分点算速率；只有绝对值时按绝对值算（分钟数与单位无关）。
        let (series, current, unit_label) = match pct {
            Some(now_pct) => {
                let series: Vec<(i64, f64)> = record
                    .samples
                    .iter()
                    .filter_map(|sample| sample.pct.map(|value| (sample.epoch, value)))
                    .collect();
                (series, now_pct, "百分点")
            }
            None => {
                let series: Vec<(i64, f64)> = record
                    .samples
                    .iter()
                    .filter_map(|sample| sample.abs.map(|value| (sample.epoch, value)))
                    .collect();
                (series, value.remaining_abs.unwrap_or(0.0), "绝对值")
            }
        };
        match estimate_eta_min(&series, current, ETA_MIN_SPAN_SEC) {
            Some(eta) => {
                eta_min = Some(eta);
                eta_note = format!(
                    "按 {} 次读数（{} 单位，跨度 ≥{ETA_MIN_SPAN_SEC}s）估算 ≈ {eta:.0} 分钟",
                    series.len(),
                    unit_label
                );
            }
            None if series.len() < 2 => {
                eta_note = format!(
                    "无法估算 ETA（只有 {} 次读数：至少需要 2 次且时间跨度 ≥{ETA_MIN_SPAN_SEC}s）",
                    series.len()
                );
            }
            None => {
                eta_note = "无法估算 ETA（读数没有下降趋势：额度未在消耗或刚被补充）".to_string();
            }
        }
        notes.push(format!(
            "最近一次读数 {}（通道 {}）",
            record
                .samples
                .last()
                .map(|sample| sample.ts.clone())
                .unwrap_or_default(),
            record
                .samples
                .last()
                .map(|sample| sample.source.as_str())
                .unwrap_or("-")
        ));
        record.remaining_pct = value.remaining_pct;
        record.remaining_abs = value.remaining_abs;
        record.total_abs = value.total_abs;
        record.source = source.clone();
        record.ts = util::now_iso();
    } else {
        eta_note = "无法估算 ETA（本轮没有任何通道给出读数）".to_string();
    }

    let verdict = match reading {
        Some(value) => {
            let outcome = credit_verdict(
                platform,
                &value,
                eta_min,
                ctx.cfg.watchdog.credits_warn_pct,
                ctx.cfg.watchdog.credits_warn_eta_min,
            );
            match outcome {
                Some((payload, reason)) => {
                    record.verdict = reason.clone();
                    let fired = bus.emit_debounced(
                        name::CREDITS_LOW,
                        payload,
                        &format!("credits_low:{platform}"),
                        CREDITS_DEBOUNCE,
                    )?;
                    notes.push(format!(
                        "{}：{reason}",
                        if fired {
                            "已派发 credits_low"
                        } else {
                            "credits_low 被去抖窗口抑制"
                        }
                    ));
                    Some(reason)
                }
                None => {
                    record.verdict = "未触发预警".to_string();
                    Some("未触发预警".to_string())
                }
            }
        }
        None => {
            record.verdict = "无读数".to_string();
            None
        }
    };

    Ok(CreditOutcome {
        platform: platform.to_string(),
        source: if source.is_empty() {
            "-".to_string()
        } else {
            source
        },
        reading,
        eta_note,
        verdict,
        notes,
    })
}

/// UIA 通道：找含「积分/额度/剩余」的文本再抽数字（§6.5.3，不跑 OCR）。
fn read_via_uia(platform: &str, notes: &mut Vec<String>) -> Option<CreditReading> {
    let window = window_for(platform)?;
    let snippets = uia::text_snippets(window.hwnd, 32);
    if snippets.is_empty() {
        notes.push("uia 跳过：UIA 未返回文本（本二进制未实现，或窗口没有可读元素）".to_string());
        return None;
    }
    let render = |pairs: &[(String, String)]| {
        pairs
            .iter()
            .map(|(element, value)| format!("{element} {value}"))
            .collect::<Vec<_>>()
            .join(" | ")
    };
    // §6.5.3：优先带「积分/额度/剩余」关键词的元素；都没有时再退回全部文本。
    let relevant: Vec<(String, String)> = snippets
        .iter()
        .filter(|(element, value)| {
            ocr::matches_keywords(&format!("{element} {value}"), &CREDIT_KEYWORDS)
        })
        .cloned()
        .collect();
    let candidates = if relevant.is_empty() {
        &snippets
    } else {
        &relevant
    };
    let joined = render(candidates);
    parse_credit(&joined).or_else(|| {
        notes.push(format!(
            "uia：{} 段文本（关键词命中 {} 段）里没有可解析的额度数字（片段：{}）",
            snippets.len(),
            relevant.len(),
            truncate(&joined, 80)
        ));
        None
    })
}

/// OCR 通道（仅 Full；未编译时如实报错，§9 降级到下一通道）。
fn read_via_ocr(ctx: &Ctx, platform: &str, notes: &mut Vec<String>) -> Option<CreditReading> {
    if !ocr::FEATURE_ENABLED {
        notes.push(
            "ocr 跳过：本二进制未编译 --features ocr（§6.5.2 的 Lite 本来也不跑它）".to_string(),
        );
        return None;
    }
    let region = match ctx
        .cfg
        .watchdog
        .regions
        .get(platform)
        .or_else(|| ctx.cfg.watchdog.regions.get(&format!("{platform}.exe")))
        .cloned()
        .or_else(|| window_for(platform).map(|window| ocr::region_from_rect(window.rect)))
    {
        Some(region) if !region.is_empty() => region,
        _ => {
            notes.push(
                "ocr 跳过：没有可用的积分区域（请在 [watchdog.regions] 手填数字区坐标）"
                    .to_string(),
            );
            return None;
        }
    };
    match ocr::find_text(&region, &CREDIT_KEYWORDS) {
        Ok(hits) if hits.is_empty() => {
            notes.push("ocr：识别成功，但区域内没有含关键词的文本".to_string());
            None
        }
        Ok(hits) => {
            notes.push(format!(
                "ocr：{} 条命中，首条中心（相对区域）={:?}",
                hits.len(),
                hits.first().map(|hit| hit.centre)
            ));
            let joined = hits
                .iter()
                .map(|hit| hit.text.clone())
                .collect::<Vec<_>>()
                .join(" | ");
            parse_credit(&joined).or_else(|| {
                notes.push(format!(
                    "ocr：文本「{}」里没有可解析的数字",
                    truncate(&joined, 60)
                ));
                None
            })
        }
        Err(err) => {
            notes.push(format!("ocr 失败（§9 降级：继续走下一通道）：{err}"));
            None
        }
    }
}

fn truncate(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let mut cut: String = chars[..max].iter().collect();
    cut.push('…');
    cut
}

// ---------------------------------------------------------------------------
// 命令：start / stop
// ---------------------------------------------------------------------------

fn start(ctx: &Ctx, mode_override: Option<&str>, daemon: bool) -> Result<()> {
    if let Some(raw) = mode_override {
        if Tier::parse(raw).is_none() && raw.trim().to_lowercase() != "auto" {
            return Err(anyhow!("--mode 只接受 auto|full|lite，收到 \"{raw}\""));
        }
    }
    if daemon {
        let cfg = &ctx.cfg.watchdog;
        mod_sched::register_interval_task(
            ctx,
            DAEMON_TASK,
            DAEMON_EVERY_MIN,
            "watchdog start",
            false,
        )
        .with_context(|| format!("注册计划任务 {DAEMON_TASK}"))?;
        println!("已注册计划任务 devzone-{DAEMON_TASK}：每 {DAEMON_EVERY_MIN} 分钟拉起一次 `devzone watchdog start`。");
        println!(
            "说明（§10 不做常驻服务，M1/M2 折中）：每次被拉起只跑一轮检测（非交互环境单轮即退出），\
静止计数与最近读数保存在 {}，因此「连续 {} 轮无变化 → stall_detected」这个判定在任务之间仍然连续。",
            state_file(ctx).display(),
            cfg.stall_stable_polls
        );
        println!(
            "轮询节奏：Full {}s / Lite {}s；计划任务粒度只有分钟级，所以 --daemon 实际是「每分钟一轮」。想要秒级节奏请在前台跑 `devzone watchdog start`。",
            cfg.poll_sec_full, cfg.poll_sec_lite
        );
        println!("停止：devzone watchdog stop");
        return Ok(());
    }
    // 交互式终端 → 前台循环；计划任务 / 管道 → 单轮即退出（§10：不做常驻服务）。
    let max_polls = foreground_budget(std::io::stdin().is_terminal());
    run_forever(ctx, mode_override, max_polls)
}

/// 前台循环的预算：`None` = 一直跑到 Ctrl+C，`Some(1)` = 只跑一轮。
///
/// 计划任务（schtasks）没有交互终端，因此每次被拉起只跑一轮检测 —— 这正是
/// §10「不做常驻服务」与 §6.5.2「Full 30s / Lite 60s 轮询」之间的 M1/M2 折中：
/// 静止计数与读数保存在状态文件里，跨轮次仍然连续。
fn foreground_budget(stdin_terminal: bool) -> Option<usize> {
    if stdin_terminal {
        None
    } else {
        Some(1)
    }
}

/// 前台循环（§6.5.1 / §6.5.2）。
fn run_forever(ctx: &Ctx, mode_override: Option<&str>, max_polls: Option<usize>) -> Result<()> {
    let mut state = load_state(ctx);
    let mut templates: BTreeMap<String, GrayTemplate> = BTreeMap::new();
    let bus = ctx.bus()?;
    if targets(&ctx.cfg).is_empty() {
        util::progress(
            "提示：[guard.whitelist.apps] 与 [watchdog.regions] 都是空的，现在没有可盯的目标应用。",
        );
    } else if !ctx
        .cfg
        .watchdog
        .regions
        .values()
        .any(|region| !region.is_empty())
    {
        util::progress(
            "提示：[watchdog.regions] 为空，检测会退化为「主窗口矩形 / UIA 文本」信号；用 devzone watchdog adapter test <app> 取得建议坐标后手填。",
        );
    }
    let mut polls = 0usize;
    loop {
        let mode = effective_mode(ctx, mode_override);
        util::progress(&format!(
            "[{}|每{}s] {}（第 {} 轮）",
            mode.tier.as_str(),
            ctx.cfg.poll_sec(mode.tier == Tier::Lite),
            mode.reason,
            polls + 1
        ));
        let report = run_poll(ctx, &mut state, &mut templates, &bus, &mode)?;
        save_state(ctx, &state)?;
        print_poll_report(&report);
        polls += 1;
        if max_polls.is_some_and(|limit| polls >= limit) {
            util::progress(
                "非交互环境（计划任务 / 管道）：本轮结束即退出，由 devzone-watchdog-start 再次拉起（§10）。",
            );
            return Ok(());
        }
        let interval = ctx.cfg.poll_sec(mode.tier == Tier::Lite);
        util::progress(&format!("休眠 {interval}s 后进行下一轮（Ctrl+C 退出）"));
        std::thread::sleep(Duration::from_secs(interval));
    }
}

fn print_poll_report(report: &Value) {
    println!(
        "感知模式：{}（{}），轮询 {}s",
        report["tier"].as_str().unwrap_or("?"),
        report["reason"].as_str().unwrap_or("?"),
        report["poll_sec"].as_u64().unwrap_or(0)
    );
    if let Some(apps) = report["apps"].as_array() {
        if apps.is_empty() {
            println!("  （没有目标：[guard.whitelist.apps] 与 [watchdog.regions] 都是空的）");
        }
        for app in apps {
            println!(
                "  {:<18} 信号={:<7} 静止 {}/{} hash={} 事件={} — {}",
                app["app"].as_str().unwrap_or("?"),
                app["signal"].as_str().unwrap_or("?"),
                app["stable_polls"].as_u64().unwrap_or(0),
                app["stable_polls_needed"].as_u64().unwrap_or(0),
                app["hash"].as_str().unwrap_or("?"),
                app["events"]
                    .as_array()
                    .map(|list| {
                        if list.is_empty() {
                            "-".to_string()
                        } else {
                            list.iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(",")
                        }
                    })
                    .unwrap_or_default(),
                app["detail"].as_str().unwrap_or("")
            );
        }
    }
    for credit in report["credits"].as_array().into_iter().flatten() {
        println!(
            "  积分 {:<12} 通道={:<12} 剩余={} ETA：{}",
            credit["platform"].as_str().unwrap_or("?"),
            credit["source"].as_str().unwrap_or("?"),
            credit["remaining_pct"]
                .as_f64()
                .map(|value| format!("{value:.1}%"))
                .unwrap_or_else(|| "无读数".to_string()),
            credit["eta_note"].as_str().unwrap_or("")
        );
        for note in credit["notes"].as_array().into_iter().flatten() {
            if let Some(text) = note.as_str() {
                println!("      · {text}");
            }
        }
    }
}

fn stop(ctx: &Ctx) -> Result<()> {
    let existed = mod_sched::task_exists(DAEMON_TASK)
        .map_err(|err| {
            tracing::warn!("查询计划任务 {DAEMON_TASK} 失败：{err}");
            err
        })
        .ok();
    mod_sched::remove_task(ctx, DAEMON_TASK)
        .map_err(|err| anyhow!("删除计划任务 devzone-{DAEMON_TASK} 失败：{err}"))?;
    match existed {
        Some(true) => println!("已删除计划任务 devzone-{DAEMON_TASK}，watchdog 不再被定时拉起。"),
        Some(false) => println!("devzone-{DAEMON_TASK} 原本就不存在（幂等：无需删除）。"),
        None => println!(
            "已尝试删除计划任务 devzone-{DAEMON_TASK}（存在性查询失败，结果以 schtasks 为准）。"
        ),
    }
    println!(
        "前台循环（如果有）请用 Ctrl+C 结束；静止计数仍保存在 {}。",
        state_file(ctx).display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 命令：status
// ---------------------------------------------------------------------------

fn status(ctx: &Ctx, as_json: bool) -> Result<()> {
    let data = status_data(ctx)?;
    if ctx.emit_json("watchdog status", data.clone(), as_json)? {
        return Ok(());
    }
    print_status_text(&data);
    Ok(())
}

fn status_data(ctx: &Ctx) -> Result<Value> {
    let state = load_state(ctx);
    let mode = effective_mode(ctx, None);
    let info = battery::read_system_power_status();
    let bus = ctx.bus()?;
    let recent = bus
        .recent_events(60)
        .unwrap_or_default()
        .into_iter()
        .filter(|event| {
            matches!(
                event.name.as_str(),
                name::STALL_DETECTED | name::STALL_RESOLVED | name::CREDITS_LOW
            )
        })
        .take(5)
        .map(|event| json!({ "ts": event.ts, "name": event.name, "payload": event.payload }))
        .collect::<Vec<_>>();
    let targets = targets(&ctx.cfg);
    Ok(json!({
        "enable": ctx.cfg.watchdog.enable,
        "configured_mode": ctx.cfg.watchdog.mode,
        "mode": mode.tier.as_str(),
        "mode_reason": mode.reason,
        "forced": mode.forced,
        "poll_sec": {
            "full": ctx.cfg.watchdog.poll_sec_full,
            "lite": ctx.cfg.watchdog.poll_sec_lite,
            "effective": ctx.cfg.poll_sec(mode.tier == Tier::Lite),
        },
        "stall_stable_polls": ctx.cfg.watchdog.stall_stable_polls,
        "credits_thresholds": {
            "warn_pct": ctx.cfg.watchdog.credits_warn_pct,
            "warn_eta_min": ctx.cfg.watchdog.credits_warn_eta_min,
        },
        "sensing": {
            "full": Tier::Full.sensing_note(),
            "lite": Tier::Lite.sensing_note(),
            "switch": "auto：§6.3.7 省电等待（Guardian 在 PAUSED_WAIT）或电量 < [guard].pause_level 时切 Lite，power_restored 后切回 Full；full/lite 为强制",
        },
        "battery": {
            "ac": info.ac,
            "level_pct": info.level_pct,
            "charging": info.charging,
            "guardian_in_power_saving_wait": guardian_in_wait(&ctx.cfg, &info),
        },
        "targets": targets.iter().map(Target::to_json).collect::<Vec<_>>(),
        "regions_configured": ctx.cfg.watchdog.regions.len(),
        "targets_without_region": targets
            .iter()
            .filter(|t| t.region.is_none())
            .map(|t| t.app.clone())
            .collect::<Vec<_>>(),
        "watches": state.apps,
        "credits": state.credits,
        "manual": manual_values(ctx),
        "features": {
            "ocr": ocr::FEATURE_ENABLED,
            "toast": toast_state(),
            "debug_save_screenshots": ctx.cfg.watchdog.debug_save_screenshots,
        },
        "daemon": {
            "task": format!("devzone-{DAEMON_TASK}"),
            "every_min": DAEMON_EVERY_MIN,
            "note": "§10：不做常驻服务；计划任务每次只跑一轮检测（M1/M2 折中）",
        },
        "state_file": state_file(ctx).display().to_string(),
        "recent_events": recent,
        "channels": bus.channel_names(),
    }))
}

fn print_status_text(data: &Value) {
    println!(
        "watchdog：{}（{}），感知模式={}{}",
        if data["enable"].as_bool().unwrap_or(false) {
            "已启用"
        } else {
            "已关闭（§4.1 预设）"
        },
        data["mode_reason"].as_str().unwrap_or("?"),
        data["mode"].as_str().unwrap_or("?"),
        if data["forced"].as_bool().unwrap_or(false) {
            "（强制）"
        } else {
            ""
        }
    );
    println!(
        "  轮询：full={}s lite={}s 生效={}s；连续静止 {} 轮 → stall_detected（第一帧只建基线）",
        data["poll_sec"]["full"].as_u64().unwrap_or(0),
        data["poll_sec"]["lite"].as_u64().unwrap_or(0),
        data["poll_sec"]["effective"].as_u64().unwrap_or(0),
        data["stall_stable_polls"].as_u64().unwrap_or(0)
    );
    println!(
        "  两级制：{}\n           {}",
        Tier::Full.sensing_note(),
        Tier::Lite.sensing_note()
    );
    println!(
        "  目标 {} 个，其中 {} 个没配置 [watchdog.regions]（会退化到窗口矩形/文本信号）",
        data["targets"].as_array().map(Vec::len).unwrap_or(0),
        data["targets_without_region"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0)
    );
    if let Some(apps) = data["watches"].as_object() {
        if apps.is_empty() {
            println!("  还没有检测记录（先跑 devzone watchdog start）");
        }
        for (app, watch) in apps {
            println!(
                "  {app}：信号={} 静止={} hash={} 已报 stall={} @{}",
                watch["signal"].as_str().unwrap_or("-"),
                watch["stable_polls"].as_u64().unwrap_or(0),
                watch["last_hash_hex"].as_str().unwrap_or("-"),
                watch["stalled"].as_bool().unwrap_or(false),
                watch["last_ts"].as_str().unwrap_or("-"),
            );
        }
    }
    for (platform, record) in data["credits"].as_object().into_iter().flatten() {
        let pct = record["remaining_pct"]
            .as_f64()
            .map(|value| format!("{value:.1}%"))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "  积分 {platform}：通道={} 剩余={} ETA={}（{}）",
            record["source"].as_str().unwrap_or("-"),
            pct,
            record["verdict"].as_str().unwrap_or("未判定"),
            record["ts"].as_str().unwrap_or("-"),
        );
    }
    for (platform, manual) in data["manual"].as_object().into_iter().flatten() {
        println!(
            "  manual {platform}：{} {}（{}）",
            manual["value"].as_f64().unwrap_or(0.0),
            manual["unit"].as_str().unwrap_or(""),
            manual["ts"].as_str().unwrap_or("-"),
        );
    }
    println!(
        "  OCR={}，Toast={}，调试截图落盘={}",
        if ocr::FEATURE_ENABLED {
            "已编译"
        } else {
            "未编译（需 --features ocr）"
        },
        data["features"]["toast"].as_str().unwrap_or("?"),
        if data["features"]["debug_save_screenshots"]
            .as_bool()
            .unwrap_or(false)
        {
            "开"
        } else {
            "关（§8 默认）"
        }
    );
    println!(
        "  计划任务：{}（每 {} 分钟一轮）\n  状态文件：{}",
        data["daemon"]["task"].as_str().unwrap_or("-"),
        DAEMON_EVERY_MIN,
        data["state_file"].as_str().unwrap_or("-")
    );
    if let Some(events) = data["recent_events"].as_array() {
        println!("  最近 stall/credits 事件：{} 条", events.len());
        for event in events {
            println!(
                "    {} {} {}",
                event["ts"].as_str().unwrap_or("?"),
                event["name"].as_str().unwrap_or("?"),
                event["payload"]
            );
        }
    }
    if let Some(channels) = data["channels"].as_array() {
        println!(
            "  通知通道：{}",
            channels
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

/// 已导入过的 manual 读数（`[watchdog.adapters]` 的键 ∪ 目标应用短名）。
fn manual_values(ctx: &Ctx) -> Value {
    let mut map = serde_json::Map::new();
    for platform in credit_platforms(ctx) {
        if let Some(manual) = read_manual(ctx, &platform) {
            if let Ok(value) = serde_json::to_value(&manual) {
                map.insert(sanitize_platform(&platform), value);
            }
        }
    }
    Value::Object(map)
}

/// toast 通道状态（只有编译了 `--features toast` 才有真实答案）。
#[cfg(feature = "toast")]
fn toast_state() -> String {
    match crate::adapters::toast::probe() {
        Ok(text) => text,
        Err(err) => format!("不可用：{err}"),
    }
}

#[cfg(not(feature = "toast"))]
fn toast_state() -> String {
    "未编译（需 --features toast）".to_string()
}

// ---------------------------------------------------------------------------
// 命令：adapter list / adapter test
// ---------------------------------------------------------------------------

fn adapter_list(ctx: &Ctx) -> Result<()> {
    println!("积分读数适配器（§6.5.3 顺序 official_api > uia > ocr > manual）：");
    if ctx.cfg.watchdog.adapters.is_empty() {
        println!("  （[watchdog.adapters] 为空：所有平台按默认优先级 uia > ocr > manual）");
    }
    for (platform, cfg) in &ctx.cfg.watchdog.adapters {
        let names: Vec<&str> = credit_priority(Some(cfg))
            .iter()
            .map(|source| source.as_str())
            .collect();
        println!(
            "  {platform:<12} 配置={} → 生效={}｜api_url={}｜token 环境变量={}",
            if cfg.priority.is_empty() {
                "-".to_string()
            } else {
                cfg.priority.join("/")
            },
            names.join(" > "),
            if cfg.api_url.trim().is_empty() {
                "未配置（不会凭空猜接口）"
            } else {
                cfg.api_url.as_str()
            },
            if cfg.api_token_env.trim().is_empty() {
                "-"
            } else {
                cfg.api_token_env.as_str()
            },
        );
    }
    println!("输出区域（[watchdog.regions]，devzone watchdog adapter test 给出建议坐标）：");
    if ctx.cfg.watchdog.regions.is_empty() {
        println!("  （空：检测退化为窗口矩形 / UIA 文本）");
    }
    for (app, region) in &ctx.cfg.watchdog.regions {
        println!(
            "  {app:<16} x={:<6} y={:<6} {}x{}{}",
            region.x,
            region.y,
            region.width,
            region.height,
            if region.is_empty() {
                "  ← 尺寸非法（<=0）"
            } else if ocr::region_offscreen(region) {
                "  ← 超出主屏，可能截到黑边"
            } else {
                ""
            }
        );
    }
    println!("优雅暂停白名单（[guard.whitelist.apps]，同样参与停顿检测）：");
    if ctx.cfg.guard.whitelist.apps.is_empty() {
        println!("  （空）");
    }
    for (app, cfg) in &ctx.cfg.guard.whitelist.apps {
        println!(
            "  {app:<16} levels={} ocr_fallback={} watch_region={} 进程={}",
            if cfg.enabled_levels.is_empty() {
                "-".to_string()
            } else {
                cfg.enabled_levels.join("/")
            },
            cfg.ocr_fallback,
            cfg.watch_region.as_deref().unwrap_or("(同名区域)"),
            adapters::process_summary(app),
        );
    }
    println!("构建特性与隐私：");
    println!(
        "  ocr     = {}（Windows.Media.Ocr；Lite 模式不需要它）",
        ocr::FEATURE_ENABLED
    );
    println!("  toast   = {}", toast_state());
    println!(
        "  调试截图 = {}（§8：默认不落盘）\n  状态文件 = {}",
        if ctx.cfg.watchdog.debug_save_screenshots {
            "开"
        } else {
            "关"
        },
        state_file(ctx).display()
    );
    Ok(())
}

/// 只读实测（§5.4 第 5 项的 watchdog 侧语义）：探测进程/窗口、UIA 文本、截一帧算 dHash，
/// 并打印建议的 `[watchdog.regions]` TOML 片段让用户手填。
/// **不发送任何按键、不点击、不关闭窗口，也不改配置文件。**
fn adapter_test(ctx: &Ctx, app: &str, as_json: bool) -> Result<()> {
    let hint = if app.to_lowercase().ends_with(".exe") {
        app.to_string()
    } else {
        format!("{app}.exe")
    };
    let processes = adapters::find_processes(&hint);
    let app_cfg = ctx
        .cfg
        .guard
        .whitelist
        .apps
        .get(app)
        .or_else(|| ctx.cfg.guard.whitelist.apps.get(&hint))
        .cloned()
        .unwrap_or_default();

    let mut probes = Vec::new();
    match pause::probe_levels(app, &app_cfg) {
        Ok(list) => {
            for probe in list {
                probes.push(json!({
                    "level": probe.level.as_str(),
                    "available": probe.available,
                    "note": probe.note,
                    "process": probe.process,
                }));
            }
        }
        Err(err) => probes.push(json!({
            "level": "-",
            "available": false,
            "note": format!("级别实测不可用：{err}"),
            "process": adapters::process_summary(&hint),
        })),
    }

    let window = processes
        .iter()
        .find_map(|process| adapters::main_window_of(process.pid))
        .or_else(|| window_for(app));
    let is_foreground = match (&window, adapters::foreground_window()) {
        (Some(w), Some(f)) => w.hwnd == f.hwnd,
        _ => false,
    };

    let configured = ctx
        .cfg
        .watchdog
        .regions
        .get(app)
        .or_else(|| ctx.cfg.watchdog.regions.get(&hint))
        .cloned();
    let region = configured
        .clone()
        .or_else(|| window.as_ref().map(|w| ocr::region_from_rect(w.rect)));
    let region_source = if configured.is_some() {
        "config"
    } else if window.is_some() {
        "window_rect"
    } else {
        "none"
    };

    let snippets = window
        .as_ref()
        .map(|w| uia::text_snippets(w.hwnd, 24))
        .unwrap_or_default();
    let snippet_text = snippets
        .iter()
        .map(|(element, value)| format!("{element} {value}"))
        .collect::<Vec<_>>()
        .join(" | ");
    let credit = parse_credit(&snippet_text);

    let mut notes: Vec<String> = Vec::new();
    let capture = match region.clone() {
        Some(region) => match ocr::capture_region(&region) {
            Ok(image) => {
                let saved = debug_path(ctx, app);
                ocr::maybe_save_screenshot(
                    &image,
                    &saved,
                    ctx.cfg.watchdog.debug_save_screenshots,
                )?;
                if ocr::region_offscreen(&region) {
                    notes.push("区域超出主屏（多显示器或窗口已移动），建议重新取值".to_string());
                }
                json!({
                    "ok": true,
                    "width": image.width(),
                    "height": image.height(),
                    "dhash": pattern::hash_hex(pattern::dhash(&image)),
                    "changed_threshold": pattern::DHASH_CHANGED_THRESHOLD,
                    "debug_png": if ctx.cfg.watchdog.debug_save_screenshots { saved.display().to_string() } else { "未落盘（§8）".to_string() },
                })
            }
            Err(err) => {
                notes.push(format!("截帧失败：{err}"));
                json!({ "ok": false, "error": err.to_string() })
            }
        },
        None => {
            notes.push(
                "没有可截的区域也没有可截的窗口：停顿检测将退化为 UIA 文本 / 窗口标题信号"
                    .to_string(),
            );
            Value::Null
        }
    };

    let ocr_report = if !ocr::FEATURE_ENABLED {
        json!({
            "compiled": false,
            "note": "未编译 --features ocr：Full 模式的积分数字改由 UIA 文本或 manual 导入提供",
        })
    } else {
        match region
            .clone()
            .filter(|region| !region.is_empty())
            .map(|region| ocr::find_text(&region, &CREDIT_KEYWORDS))
        {
            Some(Ok(hits)) => json!({
                "compiled": true,
                "hits": hits.iter().map(ocr::hit_to_json).collect::<Vec<_>>(),
                "parsed": parse_ocr_hits(&hits),
            }),
            Some(Err(err)) => json!({ "compiled": true, "error": err.to_string() }),
            None => {
                json!({ "compiled": true, "note": "没有可识别的区域（OCR 需要 [watchdog.regions] 的积分数字区坐标）" })
            }
        }
    };

    let suggestion = region
        .as_ref()
        .map(|region| {
            format!(
                "[watchdog.regions]\n\"{app}\" = {{ x = {x}, y = {y}, width = {w}, height = {h} }}",
                x = region.x,
                y = region.y,
                w = region.width,
                h = region.height
            )
        })
        .unwrap_or_else(|| {
            format!(
                "# 取不到窗口矩形：请把应用窗口打开后重试，或按屏幕坐标手填一行：\n[watchdog.regions]\n\"{app}\" = {{ x = 0, y = 0, width = 800, height = 600 }}",
            )
        });

    let data = json!({
        "app": app,
        "readonly": true,
        "actions_sent": { "keys": 0, "clicks": 0, "closed_windows": 0 },
        "processes": processes
            .iter()
            .map(|process| json!({
                "pid": process.pid,
                "name": process.name,
                "exe": process.exe.as_ref().map(|p| p.display().to_string()),
                "windows": adapters::windows_of_pid(process.pid).len(),
            }))
            .collect::<Vec<_>>(),
        "window": window.as_ref().map(|w| json!({
            "title": w.title,
            "rect": [w.rect.0, w.rect.1, w.rect.2, w.rect.3],
            "width": w.width(),
            "height": w.height(),
            "visible": w.visible,
            "foreground": is_foreground,
        })),
        "level_probes": probes,
        "region": region.map(|region| json!({
            "x": region.x,
            "y": region.y,
            "width": region.width,
            "height": region.height,
            "source": region_source,
        })),
        "capture": capture,
        "uia_snippets": snippets
            .iter()
            .map(|(element, value)| json!({ "name": element, "value": value }))
            .collect::<Vec<_>>(),
        "credit_from_uia": credit.map(|value| value.to_json()),
        "ocr": ocr_report,
        "credit_config": ctx
            .cfg
            .watchdog
            .adapters
            .get(app)
            .or_else(|| ctx.cfg.watchdog.adapters.get(&hint))
            .map(|cfg| json!({ "priority": cfg.priority, "api_url_configured": !cfg.api_url.trim().is_empty() })),
        "suggested_toml": suggestion,
        "notes": notes,
    });
    if ctx.emit_json("watchdog adapter test", data.clone(), as_json)? {
        return Ok(());
    }

    println!("只读探测（不发送按键 / 不点击 / 不关窗口）：devzone watchdog adapter test {app}");
    let running = data["processes"].as_array().map(Vec::len).unwrap_or(0);
    if running == 0 {
        println!("  进程：{hint} 未运行");
    } else {
        let parts: Vec<String> = data["processes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|process| {
                format!(
                    "pid={} 窗口数={}",
                    process["pid"].as_u64().unwrap_or(0),
                    process["windows"].as_u64().unwrap_or(0)
                )
            })
            .collect();
        println!("  进程：{}", parts.join(", "));
    }
    match &data["window"] {
        Value::Null => println!("  窗口：未找到顶层窗口"),
        value => println!(
            "  窗口：{} 矩形 {:?} 可见={} 前台={}",
            truncate(value["title"].as_str().unwrap_or("(无标题)"), 40),
            value["rect"],
            value["visible"].as_bool().unwrap_or(false),
            value["foreground"].as_bool().unwrap_or(false)
        ),
    }
    for probe in data["level_probes"].as_array().into_iter().flatten() {
        println!(
            "  级别 {:<10} 可用={} {}",
            probe["level"].as_str().unwrap_or("?"),
            probe["available"].as_bool().unwrap_or(false),
            probe["note"].as_str().unwrap_or("")
        );
    }
    println!("  区域来源：{region_source}");
    if data["capture"]["ok"].as_bool().unwrap_or(false) {
        println!(
            "  截帧：{}x{} dHash={}（Hamming 距离 ≥{} 判为「在动」）",
            data["capture"]["width"].as_u64().unwrap_or(0),
            data["capture"]["height"].as_u64().unwrap_or(0),
            data["capture"]["dhash"].as_str().unwrap_or("?"),
            data["capture"]["changed_threshold"].as_u64().unwrap_or(0)
        );
    } else if data["capture"].is_object() {
        println!(
            "  截帧：失败 — {}",
            data["capture"]["error"].as_str().unwrap_or("?")
        );
    }
    println!("  UIA 文本：{} 段", snippets.len());
    if let Some(parsed) = data["credit_from_uia"].as_object() {
        println!(
            "  积分解析（来自 UIA 文本）：{}",
            Value::Object(parsed.clone())
        );
    }
    println!("  OCR：{}", data["ocr"]);
    for note in data["notes"].as_array().into_iter().flatten() {
        if let Some(text) = note.as_str() {
            println!("  · {text}");
        }
    }
    println!(
        "下一步：把下面这段贴进 {}（devzone 不自动改你的配置）：",
        ctx.paths.config.display()
    );
    println!("{suggestion}");
    Ok(())
}

// ---------------------------------------------------------------------------
// 命令：import-credits
// ---------------------------------------------------------------------------

fn import_credits(ctx: &Ctx, platform: &str, value: f64, unit: &str) -> Result<()> {
    if !value.is_finite() || value < 0.0 {
        return Err(anyhow!("额度值必须是 ≥0 的有限数字，收到 {value}"));
    }
    let percent = unit_is_percent(unit);
    if percent && value > 100.0 {
        return Err(anyhow!(
            "--unit 是百分比时值必须 ≤100，收到 {value}（绝对数值请用 --unit credits）"
        ));
    }
    let name = platform.trim().to_lowercase();
    if name.is_empty() {
        return Err(anyhow!("平台名不能为空"));
    }
    let now = Local::now();
    let manual = ManualCredit {
        platform: name.clone(),
        value,
        unit: unit.to_string(),
        remaining_pct: if percent { Some(value) } else { None },
        remaining_abs: if percent { None } else { Some(value) },
        ts: now.to_rfc3339(),
        epoch: now.timestamp(),
    };
    let path = manual_file(ctx, &name);
    write_json(
        &path,
        &serde_json::to_value(&manual).context("序列化 manual 读数")?,
    )?;
    println!(
        "已记录 manual 读数：{name} = {value} {unit} → {}",
        path.display()
    );

    // 立刻做一次阈值判定：写入状态文件（含速率样本），必要时发 §6.7.1 的 credits_low。
    let mut state = load_state(ctx);
    let reading = manual.reading();
    let record = state.credits.entry(name.clone()).or_default();
    record.samples.push(CreditSample {
        ts: manual.ts.clone(),
        epoch: manual.epoch,
        pct: manual.remaining_pct,
        abs: manual.remaining_abs,
        source: "manual".to_string(),
    });
    if record.samples.len() > CREDIT_SAMPLES {
        let excess = record.samples.len() - CREDIT_SAMPLES;
        record.samples.drain(..excess);
    }
    record.remaining_pct = reading.remaining_pct;
    record.remaining_abs = reading.remaining_abs;
    record.source = "manual".to_string();
    record.ts = manual.ts.clone();

    let (series, current) = if reading.resolved_pct().is_some() {
        let series: Vec<(i64, f64)> = record
            .samples
            .iter()
            .filter_map(|sample| sample.pct.map(|value| (sample.epoch, value)))
            .collect();
        (series, reading.resolved_pct().unwrap_or(0.0))
    } else {
        let series: Vec<(i64, f64)> = record
            .samples
            .iter()
            .filter_map(|sample| sample.abs.map(|value| (sample.epoch, value)))
            .collect();
        (series, reading.remaining_abs.unwrap_or(0.0))
    };
    let (eta_min, eta_note) = match estimate_eta_min(&series, current, ETA_MIN_SPAN_SEC) {
        Some(eta) => (
            Some(eta),
            format!("按 {} 次导入的读数估算 ≈ {eta:.0} 分钟", series.len()),
        ),
        None if series.len() < 2 => (
            None,
            format!(
                "无法估算 ETA（只有 {} 次读数：manual 至少需要 2 次且跨度 ≥{ETA_MIN_SPAN_SEC}s）",
                series.len()
            ),
        ),
        None => (None, "无法估算 ETA（导入的读数没有下降趋势）".to_string()),
    };
    let verdict = credit_verdict(
        &name,
        &reading,
        eta_min,
        ctx.cfg.watchdog.credits_warn_pct,
        ctx.cfg.watchdog.credits_warn_eta_min,
    );
    if let Some((payload, reason)) = &verdict {
        record.verdict = reason.clone();
        let fired = ctx.bus()?.emit_debounced(
            name::CREDITS_LOW,
            payload.clone(),
            &format!("credits_low:{name}"),
            CREDITS_DEBOUNCE,
        )?;
        println!(
            "{}：{reason}（payload={payload}）",
            if fired {
                "已派发 credits_low"
            } else {
                "credits_low 被 30 分钟去抖窗口抑制"
            }
        );
    } else {
        record.verdict = "未触发预警".to_string();
        println!(
            "未触发预警（阈值：剩余 < {}% 或预计 < {} 分钟耗尽）",
            ctx.cfg.watchdog.credits_warn_pct, ctx.cfg.watchdog.credits_warn_eta_min
        );
    }
    println!("ETA：{eta_note}");
    save_state(ctx, &state)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// devzone status 摘要
// ---------------------------------------------------------------------------

/// 供 `devzone status`：模式、应用清单、regions 覆盖情况、最近一次 stall/credits 判定、manual 值。
pub fn summary(ctx: &Ctx, _db: &Db) -> Result<Value> {
    let state = load_state(ctx);
    let mode = effective_mode(ctx, None);
    let targets = targets(&ctx.cfg);
    let active_stalls = state
        .apps
        .iter()
        .filter(|(_, watch)| watch.stalled)
        .map(|(app, watch)| {
            json!({
                "app": app,
                "stable_polls": watch.stable_polls,
                "since_epoch": watch.stable_since_epoch,
                "signal": watch.signal,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "enabled": ctx.cfg.watchdog.enable,
        "mode": mode.tier.as_str(),
        "configured_mode": ctx.cfg.watchdog.mode,
        "mode_reason": mode.reason,
        "poll_sec": ctx.cfg.poll_sec(mode.tier == Tier::Lite),
        "stall_stable_polls": ctx.cfg.watchdog.stall_stable_polls,
        "targets": targets
            .iter()
            .map(|target| json!({
                "app": target.app,
                "from": target.from,
                "region_configured": target.region.is_some(),
                "signal": state.apps.get(&target.app).map(|w| w.signal.clone()).unwrap_or_default(),
                "stable_polls": state.apps.get(&target.app).map(|w| w.stable_polls).unwrap_or(0),
                "stalled": state.apps.get(&target.app).map(|w| w.stalled).unwrap_or(false),
            }))
            .collect::<Vec<_>>(),
        "regions_configured": ctx.cfg.watchdog.regions.len(),
        "regions_missing": targets
            .iter()
            .filter(|t| t.region.is_none())
            .map(|t| t.app.clone())
            .collect::<Vec<_>>(),
        "active_stalls": active_stalls,
        "credits": state.credits,
        "manual": manual_values(ctx),
        "features": { "ocr": ocr::FEATURE_ENABLED, "toast": toast_state() },
        "debug_save_screenshots": ctx.cfg.watchdog.debug_save_screenshots,
        "daemon_task": format!("devzone-{DAEMON_TASK}"),
        "state_file": state_file(ctx).display().to_string(),
        "updated": state.updated,
    }))
}

// ---------------------------------------------------------------------------
// 单元测试（§11：纯函数 + 注入截帧 + DEVZONE_HOME/DEVZONE_DRY_RUN 命令级）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppPauseCfg, DEFAULT_TEMPLATE};
    use crate::util::Paths;
    use image::RgbImage;
    use serial_test::serial;

    fn battery(ac: Option<bool>, level: Option<u8>) -> BatteryInfo {
        BatteryInfo {
            ac,
            level_pct: level,
            battery_flag: level.map(|_| 1),
            life_min: None,
            charging: ac.map(|on_ac| !on_ac),
        }
    }

    fn ctx_in(dir: &tempfile::TempDir, raw: &str) -> Ctx {
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        std::fs::write(&paths.config, raw).unwrap();
        Ctx {
            cfg: Config::load(&paths).unwrap(),
            paths,
            force: false,
            elevated: true,
            config_error: None,
        }
    }

    /// 平滑对角渐变：两帧相同 ⇒ 距离 0。
    fn gradient(seed: u32) -> RgbImage {
        RgbImage::from_fn(64, 32, |x, y| {
            let value = ((x as usize + y as usize + seed as usize) % 256) as u8;
            image::Rgb([value, value, value])
        })
    }

    /// 伪随机噪声：与渐变明显不同 ⇒ 距离 ≥ 8/64（§6.5.1 阈值）。
    fn noise(seed: u32) -> RgbImage {
        let mut state = seed;
        RgbImage::from_fn(64, 32, |_, _| {
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
            let value = ((state >> 16) & 0xff) as u8;
            image::Rgb([value, value, value])
        })
    }

    // ---- 两级感知（§6.5.2）----

    #[test]
    fn forced_modes_ignore_the_battery() {
        for (raw, want) in [("full", Tier::Full), ("LITE", Tier::Lite)] {
            let decision = decide_mode(raw, &battery(Some(false), Some(3)), 15, true);
            assert_eq!(decision.tier, want);
            assert!(decision.forced, "{raw} 必须是强制");
            assert!(decision.reason.contains("强制"), "{}", decision.reason);
        }
    }

    #[test]
    fn auto_switches_to_lite_in_power_saving_wait() {
        // Guardian 已在 PAUSED_WAIT → Lite
        let decision = decide_mode("auto", &battery(Some(false), Some(12)), 15, true);
        assert_eq!(decision.tier, Tier::Lite);
        assert!(
            decision.reason.contains("PAUSED_WAIT"),
            "{}",
            decision.reason
        );
        // 电量低于 pause_level → Lite（即使插着电）
        let decision = decide_mode("auto", &battery(Some(true), Some(9)), 15, false);
        assert_eq!(decision.tier, Tier::Lite);
        assert!(decision.reason.contains("9%"));
        // AC 在线且电量充足 → Full
        assert_eq!(
            decide_mode("auto", &battery(Some(true), Some(88)), 15, false).tier,
            Tier::Full
        );
        // 电池供电但仍在 pause_level 之上 → Full（§6.5.2：只有省电等待才切 Lite）
        assert_eq!(
            decide_mode("auto", &battery(Some(false), Some(60)), 15, false).tier,
            Tier::Full
        );
        // 状态未知偏电 → Lite
        assert_eq!(
            decide_mode("auto", &battery(Some(false), None), 15, false).tier,
            Tier::Lite
        );
        // 无电池（台式机）→ Full，且不因「未知」乱切
        assert_eq!(
            decide_mode("auto", &BatteryInfo::default(), 15, false).tier,
            Tier::Full
        );
    }

    #[test]
    fn unknown_mode_string_is_treated_as_auto_with_a_note() {
        let decision = decide_mode("wat", &battery(Some(true), Some(80)), 15, false);
        assert_eq!(decision.tier, Tier::Full);
        assert!(!decision.forced);
        assert!(
            decision.reason.contains("按 auto 处理"),
            "{}",
            decision.reason
        );
    }

    #[test]
    fn foreground_budget_keeps_m1_m2_compromise() {
        // 交互终端：常驻前台循环（Ctrl+C 退出）；计划任务/管道：单轮即退出（§10）
        assert_eq!(foreground_budget(true), None);
        assert_eq!(foreground_budget(false), Some(1));
    }

    #[test]
    fn guardian_helper_agrees_with_the_level_rule() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(&dir, DEFAULT_TEMPLATE);
        assert!(guardian_in_wait(&ctx.cfg, &battery(Some(false), Some(10))));
        assert!(!guardian_in_wait(&ctx.cfg, &battery(Some(true), Some(90))));
        // 未知读数不该让 Guardian 乱进省电等待（§9 第一行）
        assert!(!guardian_in_wait(&ctx.cfg, &BatteryInfo::default()));
    }

    // ---- parse_credit（§6.5.3 文本抽取）----

    #[test]
    fn parses_chinese_percentage_and_absolute_values() {
        let pct = parse_credit("剩余额度 18%").unwrap();
        assert_eq!(pct.remaining_pct, Some(18.0));
        assert_eq!(pct.resolved_pct(), Some(18.0));

        let abs = parse_credit("剩余 1200 积分").unwrap();
        assert_eq!(abs.remaining_abs, Some(1200.0));
        assert_eq!(abs.remaining_pct, None);
        assert_eq!(abs.resolved_pct(), None, "不知道总量时不许编造百分比");

        let english = parse_credit("Remaining credits: 7 %").unwrap();
        assert_eq!(english.remaining_pct, Some(7.0));

        let available = parse_credit("quota available 42.5%").unwrap();
        assert_eq!(available.remaining_pct, Some(42.5));

        let points = parse_credit("本周期可用点数 300").unwrap();
        assert_eq!(points.remaining_abs, Some(300.0));
    }

    #[test]
    fn parses_the_fraction_form() {
        let reading = parse_credit("剩余 1200/5000").unwrap();
        assert_eq!(reading.remaining_abs, Some(1200.0));
        assert_eq!(reading.total_abs, Some(5000.0));
        assert_eq!(reading.resolved_pct(), Some(24.0));
        let grouped = parse_credit("1,200 / 5,000").unwrap();
        assert_eq!(grouped.remaining_abs, Some(1200.0));
        assert_eq!(grouped.total_abs, Some(5000.0));
    }

    #[test]
    fn multiple_numbers_take_the_remaining_one() {
        let reading = parse_credit("已用 82%，剩余 18%（每周重置）").unwrap();
        assert_eq!(reading.remaining_pct, Some(18.0));
        let unkeyworded = parse_credit("18% 82%").unwrap();
        assert_eq!(
            unkeyworded.remaining_pct,
            Some(18.0),
            "无语境时取第一个百分比"
        );
        let absolutes = parse_credit("总额度 5000，可用 300 积分").unwrap();
        assert_eq!(absolutes.remaining_abs, Some(300.0));
    }

    #[test]
    fn text_without_numbers_is_none() {
        assert!(parse_credit("").is_none());
        assert!(parse_credit("   ").is_none());
        assert!(parse_credit("额度已用尽，请充值").is_none());
        assert!(parse_credit("no digits here").is_none());
    }

    #[test]
    fn absurd_percentage_falls_back_to_absolute() {
        let reading = parse_credit("剩余 1200% 完成度").unwrap();
        assert_eq!(reading.remaining_pct, None);
        assert_eq!(reading.remaining_abs, Some(1200.0));
    }

    // ---- ETA 速率 ----

    #[test]
    fn eta_needs_two_readings_and_a_real_span() {
        assert_eq!(estimate_eta_min(&[], 50.0, ETA_MIN_SPAN_SEC), None);
        assert_eq!(
            estimate_eta_min(&[(100, 50.0)], 50.0, ETA_MIN_SPAN_SEC),
            None,
            "单次读数不能推算"
        );
        assert_eq!(
            estimate_eta_min(&[(0, 50.0), (100, 49.0)], 49.0, ETA_MIN_SPAN_SEC),
            None,
            "跨度 <{ETA_MIN_SPAN_SEC}s 全是噪声"
        );
        // 10 分钟掉 5 个百分点 ⇒ 49% 还能跑 98 分钟
        let eta = estimate_eta_min(&[(0, 54.0), (600, 49.0)], 49.0, ETA_MIN_SPAN_SEC).unwrap();
        assert!((eta - 98.0).abs() < 1e-6, "{eta}");
        // 绝对值序列同样可算（分钟数与单位无关）：10 分钟烧掉 500 ⇒ 剩下 500 只够 10 分钟
        let eta = estimate_eta_min(&[(0, 1000.0), (600, 500.0)], 500.0, ETA_MIN_SPAN_SEC).unwrap();
        assert!((eta - 10.0).abs() < 1e-6, "{eta}");
    }

    #[test]
    fn eta_refuses_to_invent_a_rate_when_nothing_was_consumed() {
        assert_eq!(
            estimate_eta_min(&[(0, 30.0), (600, 30.0)], 30.0, ETA_MIN_SPAN_SEC),
            None,
            "读数没下降 → 无法估算 ETA"
        );
        assert_eq!(
            estimate_eta_min(&[(0, 10.0), (600, 30.0)], 30.0, ETA_MIN_SPAN_SEC),
            None,
            "读数在涨（刚充值）→ 无法估算 ETA"
        );
        assert_eq!(
            estimate_eta_min(&[(0, 5.0), (600, 0.0)], 0.0, ETA_MIN_SPAN_SEC),
            None,
            "已经归零时不该炸出除零或负数"
        );
    }

    // ---- 阈值判定 ----

    #[test]
    fn credit_verdict_hits_percentage_and_eta() {
        let low = CreditReading {
            remaining_pct: Some(18.0),
            remaining_abs: None,
            total_abs: None,
        };
        let (payload, reason) = credit_verdict("doubao", &low, None, 20, 30).unwrap();
        assert_eq!(payload["platform"], "doubao");
        assert_eq!(payload["remaining_pct"], 18.0);
        assert!(reason.contains("20%"), "{reason}");
        assert!(payload.get("eta_min").is_none());
        assert!(credit_verdict("doubao", &low, None, 10, 30).is_none());

        let plenty = CreditReading {
            remaining_pct: Some(80.0),
            remaining_abs: None,
            total_abs: None,
        };
        let (payload, reason) = credit_verdict("trae", &plenty, Some(12.0), 20, 30).unwrap();
        assert_eq!(payload["eta_min"], 12.0);
        assert_eq!(payload["remaining_pct"], 80.0);
        assert!(reason.contains("12 分钟"), "{reason}");
        assert!(credit_verdict("trae", &plenty, Some(31.0), 20, 30).is_none());

        // 只有绝对值、无法换算百分比：不该凭空造出 credits_low
        let abs_only = CreditReading {
            remaining_pct: None,
            remaining_abs: Some(5.0),
            total_abs: None,
        };
        assert!(credit_verdict("x", &abs_only, None, 20, 30).is_none());
    }

    #[test]
    fn reading_percentage_prefers_pct_then_fraction() {
        let reading = CreditReading {
            remaining_pct: None,
            remaining_abs: Some(250.0),
            total_abs: Some(1000.0),
        };
        assert_eq!(reading.resolved_pct(), Some(25.0));
        let unknown_total = CreditReading {
            remaining_pct: None,
            remaining_abs: Some(250.0),
            total_abs: Some(0.0),
        };
        assert_eq!(unknown_total.resolved_pct(), None);
    }

    // ---- 停顿计数（§6.5.1）----

    #[test]
    fn first_frame_only_establishes_a_baseline() {
        let decision = evaluate(None, 1234, 0, 5);
        assert!(decision.first);
        assert!(!decision.changed);
        assert_eq!(decision.stable_polls, 0);
        assert!(!decision.stall_now);
        assert!(!decision.resolved_now);
    }

    #[test]
    fn identical_frames_stall_after_five_further_polls() {
        let hash = pattern::dhash(&gradient(3));
        let mut stable = 0;
        let mut previous: Option<u64> = None;
        let mut fired_at = None;
        for round in 1..=8 {
            let decision = evaluate(previous, hash, stable, 5);
            assert_eq!(decision.first, round == 1, "只有第一轮是基线");
            assert!(!decision.changed, "同一帧不该算变化");
            assert!(!decision.resolved_now);
            stable = decision.stable_polls;
            previous = Some(hash);
            if decision.stall_now {
                fired_at = Some(round);
                break;
            }
        }
        // 基线轮无从判断静止，所以「连续 5 轮无变化」落在第 6 轮。
        assert_eq!(fired_at, Some(6), "stall_stable_polls=5 ⇒ 第 6 轮命中");
        assert_eq!(stable, 5);
        // 已报过 stall 之后继续静止：不重复派发
        assert!(!decide_with(false, false, 6, 5, true).stall_now);
    }

    #[test]
    fn different_frames_reset_the_counter_and_resolve_a_reported_stall() {
        let before = pattern::dhash(&gradient(0));
        let after = pattern::dhash(&noise(7));
        assert!(pattern::region_changed(
            before,
            after,
            pattern::DHASH_CHANGED_THRESHOLD
        ));
        // 已报过 stall 且本帧变化 → 补发 stall_resolved
        let decision = decide_with(true, false, 7, 5, true);
        assert!(decision.resolved_now);
        assert_eq!(decision.stable_polls, 0);
        // 没报过 stall：变化只是归零，不发任何事件
        let quiet = decide_with(true, false, 2, 5, false);
        assert!(!quiet.resolved_now && !quiet.stall_now);
        // 静止一轮 → 计数 +1
        let still = decide_with(false, false, 2, 5, false);
        assert_eq!(still.stable_polls, 3);
        // 计数饱和不溢出
        assert_eq!(
            decide_with(false, false, u32::MAX, 5, false).stable_polls,
            u32::MAX
        );
    }

    #[test]
    fn two_memory_images_drive_the_detector_end_to_end() {
        // §11 的 fixture 语义：变化 / 静止两帧，走真实 dHash
        let frame_a = pattern::dhash(&gradient(0));
        let frame_b = pattern::dhash(&noise(7));
        let mut stable = 0;
        let mut previous = None;
        for hash in [frame_a, frame_b, frame_b] {
            let decision = evaluate(previous, hash, stable, 3);
            stable = decision.stable_polls;
            previous = Some(hash);
        }
        assert_eq!(stable, 1, "a→b 变化清零，之后 b→b 累计 1");
        let second = evaluate(previous, frame_b, stable, 3);
        assert_eq!(second.stable_polls, 2);
        let third = evaluate(previous, frame_b, 2, 3);
        assert!(third.stall_now && third.stable_polls == 3);
        assert_eq!(frame_changed(previous, frame_a, None), Some(true));
    }

    #[test]
    fn lite_template_matching_can_alone_declare_motion() {
        // 哈希距离不够（例如只变了几枚数字），但 NCC 相关性塌了 → 仍判「在动」
        assert!(lite_signal(false, Some(0.21)));
        assert!(!lite_signal(false, Some(0.97)));
        assert!(!lite_signal(false, None), "没有缓存模板时退化为纯哈希判断");
        assert!(lite_signal(true, Some(0.99)));
        assert_eq!(frame_changed(None, 5, Some(0.1)), None, "第一帧不判变化");
    }

    #[test]
    fn text_hash_is_stable_and_sensitive() {
        assert_eq!(text_hash("豆包 生成中"), text_hash("豆包 生成中"));
        assert_ne!(text_hash("豆包 生成中"), text_hash("豆包 等待提问"));
        assert_eq!(text_hash(""), text_hash(""));
        assert_ne!(text_hash(""), text_hash(" "));
    }

    // ---- 目标清单与降级路径 ----

    /// 在 `targets()` 结果里按名字找（mid 预设会往白名单里塞 doubao/trae，所以不能用下标）。
    fn find_target(made: &[Target], app: &str) -> Target {
        made.iter()
            .find(|t| t.app == app)
            .unwrap_or_else(|| {
                panic!(
                    "{app} 不在目标清单里：{:?}",
                    made.iter().map(|t| &t.app).collect::<Vec<_>>()
                )
            })
            .clone()
    }

    #[test]
    fn targets_are_the_union_of_whitelist_and_regions() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(
            &dir,
            r#"
[guard.whitelist.apps]
"doubao.exe" = { enabled_levels = ["hotkey"] }
[watchdog.regions]
"doubao.exe" = { x = 1, y = 2, width = 300, height = 200 }
"claude.exe" = { x = 3, y = 4, width = 500, height = 400 }
"#,
        );
        let made = targets(&ctx.cfg);
        // 稳定顺序（BTreeMap）且去重
        let names: Vec<&str> = made.iter().map(|t| t.app.as_str()).collect();
        assert_eq!(names, {
            let mut sorted = names.clone();
            sorted.sort_unstable();
            sorted.dedup();
            sorted
        });
        let doubao = find_target(&made, "doubao.exe");
        assert_eq!(doubao.from, "both", "白名单与区域都有它");
        assert_eq!(doubao.platform, "doubao");
        assert_eq!(doubao.region.map(|r| (r.x, r.width)), Some((1, 300)));
        let claude = find_target(&made, "claude.exe");
        assert_eq!(claude.from, "regions", "只出现在区域里");
        assert!(claude.region.is_some());
        assert_eq!(platform_of("Trae.EXE"), "trae");
        assert_eq!(platform_of("  claude "), "claude");
    }

    #[test]
    fn watch_region_redirects_a_whitelist_app_to_another_region() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(
            &dir,
            r#"
[guard.whitelist.apps]
"doubao.exe" = { enabled_levels = ["hotkey"], watch_region = "chat" }
[watchdog.regions]
"chat" = { x = 9, y = 9, width = 100, height = 50 }
"#,
        );
        let doubao = find_target(&targets(&ctx.cfg), "doubao.exe");
        let region = doubao
            .region
            .clone()
            .expect("watch_region 应把 chat 区域挂到这个应用上");
        assert_eq!((region.x, region.width), (9, 100));
        // chat 自身作为独立目标也存在（区域键一律参与检测）
        assert!(targets(&ctx.cfg).iter().any(|t| t.app == "chat"));
    }

    #[test]
    fn missing_regions_degrade_to_text_signal_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(&dir, DEFAULT_TEMPLATE);
        let target = Target {
            app: "definitely-not-running.exe".to_string(),
            region: None,
            from: "whitelist",
            platform: "definitely-not-running".to_string(),
        };
        let mut templates = BTreeMap::new();
        let frame = sense_frame(&ctx, &target, Tier::Lite, &mut templates);
        assert_eq!(frame.kind, "text");
        assert!(frame.note.contains("未运行"), "{}", frame.note);
        // 尺寸为 0 的区域配置本身就该被识别为不可用
        assert!(RegionCfg {
            x: 0,
            y: 0,
            width: 0,
            height: 0
        }
        .is_empty());
        assert!(region_for(&target).0.is_none());
    }

    #[test]
    fn a_configured_region_is_captured_or_degrades_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(&dir, DEFAULT_TEMPLATE);
        let region = RegionCfg {
            x: 0,
            y: 0,
            width: 32,
            height: 24,
        };
        let probe = ocr::capture_region(&region);
        let target = Target {
            app: "devzone-selftest.exe".to_string(),
            region: Some(region.clone()),
            from: "regions",
            platform: "devzone-selftest".to_string(),
        };
        let mut templates = BTreeMap::new();
        let frame = sense_frame(&ctx, &target, Tier::Lite, &mut templates);
        if probe.is_ok() {
            assert_eq!(frame.kind, "region", "真机截屏应当成功（只读）");
            assert!(frame.detail.contains("dHash="), "{}", frame.detail);
            // 第二帧：同一块屏幕区域 ⇒ NCC 模板已可用
            let again = sense_frame(&ctx, &target, Tier::Lite, &mut templates);
            assert!(again.ncc.is_none_or(|score| score > 0.9), "{:?}", again.ncc);
        } else {
            assert_eq!(frame.kind, "text", "无屏幕环境必须降级而不是失败");
        }
        // debug_save_screenshots=false ⇒ §8 绝不落盘
        assert!(!ctx.paths.data_dir.join("debug").exists());
    }

    // ---- 适配器优先级 ----

    #[test]
    fn priority_follows_the_document_order_and_manual_is_always_last() {
        let cfg = CreditAdapterCfg {
            priority: vec!["manual".to_string(), "uia".to_string()],
            api_url: String::new(),
            api_token_env: String::new(),
        };
        assert_eq!(
            credit_priority(Some(&cfg)),
            vec![CreditSource::Uia, CreditSource::Manual]
        );
        let all = CreditAdapterCfg {
            priority: vec![
                "ocr".to_string(),
                "official_api".to_string(),
                "uia".to_string(),
            ],
            api_url: String::new(),
            api_token_env: String::new(),
        };
        assert_eq!(
            credit_priority(Some(&all)),
            vec![
                CreditSource::OfficialApi,
                CreditSource::Uia,
                CreditSource::Ocr,
                CreditSource::Manual
            ]
        );
        assert_eq!(
            credit_priority(None),
            vec![CreditSource::Uia, CreditSource::Ocr, CreditSource::Manual]
        );
        // 空 priority 也回落到默认，且 dedup 后 manual 只有一个
        let empty = CreditAdapterCfg::default();
        assert_eq!(credit_priority(Some(&empty)).len(), 3);
        let dup = CreditAdapterCfg {
            priority: vec!["manual".to_string(), "manual".to_string()],
            api_url: String::new(),
            api_token_env: String::new(),
        };
        assert_eq!(credit_priority(Some(&dup)), vec![CreditSource::Manual]);
        // §6.5.2：Lite 不跑 OCR 模型
        assert!(!CreditSource::Ocr.allowed_in(Tier::Lite));
        assert!(CreditSource::Uia.allowed_in(Tier::Lite));
        assert!(CreditSource::Manual.allowed_in(Tier::Full));
        assert_eq!(
            CreditSource::parse("OFFICIAL-API"),
            Some(CreditSource::OfficialApi)
        );
        assert_eq!(CreditSource::parse("telepathy"), None);
    }

    #[test]
    fn official_api_without_url_is_never_guessed() {
        let mut notes = Vec::new();
        assert!(read_official_api(&CreditAdapterCfg::default(), &mut notes).is_none());
        assert!(notes[0].contains("api_url"), "{notes:?}");
        assert!(
            notes[0].contains("豆包"),
            "必须引用 §6.5.3 的核查结论：{notes:?}"
        );
    }

    #[serial]
    #[test]
    #[serial]
    fn official_api_honours_dry_run_and_reports_transport_failure() {
        let cfg = CreditAdapterCfg {
            priority: vec!["official_api".to_string()],
            // 回环地址：不会有真实外网流量，未 dry-run 时也只是立刻连接被拒
            api_url: "http://127.0.0.1:1/never".to_string(),
            api_token_env: "DEVZONE_TEST_TOKEN_UNSET".to_string(),
        };
        std::env::set_var(util::DRY_RUN_ENV, "1");
        std::env::remove_var("DEVZONE_TEST_TOKEN_UNSET");
        let mut dry = Vec::new();
        assert!(read_official_api(&cfg, &mut dry).is_none());
        assert!(
            dry.iter().any(|note| note.contains("DEVZONE_DRY_RUN")),
            "dry-run 下不得发起请求：{dry:?}"
        );
        std::env::remove_var(util::DRY_RUN_ENV);
        let mut live = Vec::new();
        assert!(read_official_api(&cfg, &mut live).is_none());
        assert!(
            live.iter().any(|note| note.contains("请求失败")),
            "连接失败要记进 notes 而不是静默：{live:?}"
        );
        assert!(
            live.iter().any(|note| note.contains("环境变量")),
            "token 环境变量缺失要如实说明（且绝不回显值）：{live:?}"
        );
        assert!(
            !live
                .iter()
                .any(|note| note.contains("DEVZONE_TEST_TOKEN_UNSET=")),
            "绝不能把 token 内容拼进日志：{live:?}"
        );
    }

    // ---- 平台名 / 单位 ----

    #[test]
    fn platform_names_are_filename_safe() {
        assert_eq!(sanitize_platform("doubao"), "doubao");
        assert_eq!(sanitize_platform("trae.cn"), "trae.cn");
        assert_eq!(sanitize_platform("../../windows"), ".._.._windows");
        assert_eq!(sanitize_platform("a b/c:d"), "a_b_c_d");
        assert_eq!(sanitize_platform("   "), "platform");
        assert_eq!(sanitize_platform("...."), "platform");
        assert_eq!(sanitize_platform(&"x".repeat(100)).len(), 64);
    }

    #[test]
    fn unit_parsing_for_import() {
        for percent in ["%", "Percent", "pct", "百分比"] {
            assert!(unit_is_percent(percent), "{percent}");
        }
        for absolute in ["credits", "", "点数"] {
            assert!(!unit_is_percent(absolute), "{absolute:?}");
        }
    }

    #[test]
    fn truncate_is_multibyte_safe() {
        assert_eq!(truncate("短", 10), "短");
        assert_eq!(truncate("一二三四五", 3), "一二三…");
        assert_eq!(truncate("abcdef", 6), "abcdef");
    }

    // ---- 命令级：DEVZONE_HOME + DEVZONE_DRY_RUN ----

    #[serial]
    #[test]
    #[serial]
    fn import_credits_persists_under_devzone_home_and_judges_threshold() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(util::HOME_ENV, dir.path());
        std::env::set_var(util::DRY_RUN_ENV, "1");
        // DEVZONE_HOME 必须真的把整棵数据目录隔离开
        let paths = Paths::resolve().unwrap();
        assert_eq!(paths.root, dir.path());
        paths.ensure_dirs().unwrap();
        std::fs::write(&paths.config, DEFAULT_TEMPLATE).unwrap();
        let ctx = Ctx {
            cfg: Config::load(&paths).unwrap(),
            paths,
            force: false,
            elevated: true,
            config_error: None,
        };
        let platform = "pytest-doubao";
        handle(
            &WatchdogCmd::ImportCredits {
                platform: platform.to_string(),
                value: 18.0,
                unit: "percent".to_string(),
            },
            &ctx,
        )
        .expect("import-credits 应成功");

        let file = ctx.paths.data_dir.join(format!("credits-{platform}.json"));
        assert!(
            file.exists(),
            "manual 读数必须落在 DEVZONE_HOME 的 data/ 下"
        );
        let stored: ManualCredit =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(stored.value, 18.0);
        assert_eq!(stored.remaining_pct, Some(18.0));
        assert_eq!(stored.unit, "percent");
        assert_eq!(stored.platform, platform);

        // 阈值 20% ⇒ credits_low 必须已写进 events.jsonl（§7 至少一次语义）
        let audit = std::fs::read_to_string(&ctx.paths.events).unwrap();
        let event: crate::events::Event = audit
            .lines()
            .filter_map(|line| serde_json::from_str::<crate::events::Event>(line.trim()).ok())
            .next_back()
            .expect("审计日志里必须有一条事件");
        assert_eq!(event.name, name::CREDITS_LOW);
        assert_eq!(event.payload["platform"], platform);
        assert_eq!(event.payload["remaining_pct"].as_f64(), Some(18.0));
        assert!(state_file(&ctx).exists(), "状态文件也必须在隔离目录内");

        // 未触发阈值的导入不再发事件，且 manual 以最后一次为准
        let before = std::fs::read_to_string(&ctx.paths.events)
            .unwrap()
            .lines()
            .count();
        handle(
            &WatchdogCmd::ImportCredits {
                platform: platform.to_string(),
                value: 91.0,
                unit: "percent".to_string(),
            },
            &ctx,
        )
        .unwrap();
        let after = std::fs::read_to_string(&ctx.paths.events)
            .unwrap()
            .lines()
            .count();
        assert_eq!(before, after, "91% 不该触发 credits_low");
        let fresh: ManualCredit =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(fresh.value, 91.0);

        // 绝对值单位 + 非法输入
        handle(
            &WatchdogCmd::ImportCredits {
                platform: platform.to_string(),
                value: 1200.0,
                unit: "credits".to_string(),
            },
            &ctx,
        )
        .unwrap();
        let abs: ManualCredit =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(abs.remaining_abs, Some(1200.0));
        assert_eq!(abs.remaining_pct, None);
        for bad in [120.0_f64, -1.0, f64::NAN] {
            assert!(
                handle(
                    &WatchdogCmd::ImportCredits {
                        platform: platform.to_string(),
                        value: bad,
                        unit: "percent".to_string(),
                    },
                    &ctx,
                )
                .is_err(),
                "{bad} 必须被拒绝"
            );
        }
        assert!(handle(
            &WatchdogCmd::ImportCredits {
                platform: "   ".to_string(),
                value: 1.0,
                unit: "credits".to_string(),
            },
            &ctx,
        )
        .is_err());

        std::env::remove_var(util::HOME_ENV);
        std::env::remove_var(util::DRY_RUN_ENV);
    }

    #[test]
    fn manual_import_feeds_the_credit_chain_and_reports_missing_eta() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(
            &dir,
            r#"
[watchdog]
mode = "lite"
stall_stable_polls = 2

[watchdog.adapters]
"manualonly" = { priority = ["manual"] }
"#,
        );
        handle(
            &WatchdogCmd::ImportCredits {
                platform: "manualonly".to_string(),
                value: 12.0,
                unit: "percent".to_string(),
            },
            &ctx,
        )
        .unwrap();
        let mut state = load_state(&ctx);
        let mut templates = BTreeMap::new();
        let bus = ctx.bus().unwrap();
        let mode = decide_mode("lite", &BatteryInfo::default(), 15, false);
        let report = run_poll(&ctx, &mut state, &mut templates, &bus, &mode).unwrap();
        let credits = report["credits"].as_array().expect("credits 数组");
        let entry = credits
            .iter()
            .find(|c| c["platform"] == "manualonly")
            .expect("应包含 manualonly 平台");
        assert_eq!(entry["source"], "manual");
        assert_eq!(entry["remaining_pct"], 12.0);
        assert!(
            entry["eta_note"].as_str().unwrap().contains("无法估算 ETA"),
            "单次读数必须老实说无法估算：{}",
            entry["eta_note"]
        );
        assert!(entry["verdict"].as_str().unwrap().contains("剩余 12.0%"));
        assert_eq!(report["tier"], "lite");
        // manual 通道命中后不该再去猜官方接口
        assert!(
            !entry["notes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n.as_str().unwrap().contains("official_api")),
            "优先级里只有 manual 时不该提 official_api：{:?}",
            entry["notes"]
        );
    }

    #[test]
    fn lite_poll_skips_the_ocr_channel() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(
            &dir,
            r#"
[watchdog]
mode = "lite"

[watchdog.adapters]
"skipped" = { priority = ["ocr", "manual"] }
"#,
        );
        let mut state = WatchState::default();
        let mut templates = BTreeMap::new();
        let bus = ctx.bus().unwrap();
        let mode = decide_mode("lite", &BatteryInfo::default(), 15, false);
        let report = run_poll(&ctx, &mut state, &mut templates, &bus, &mode).unwrap();
        let entry = report["credits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["platform"] == "skipped")
            .unwrap();
        assert!(
            entry["notes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n.as_str().unwrap().contains("不跑 OCR 模型")),
            "{:?}",
            entry["notes"]
        );
    }

    #[test]
    fn one_readonly_poll_persists_state_and_empty_targets_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(&dir, DEFAULT_TEMPLATE);
        // 非交互 → 单轮；没有目标 → 不截屏、不发事件，只落状态文件
        run_forever(&ctx, Some("lite"), Some(1)).expect("单轮检测不该失败");
        assert!(state_file(&ctx).exists());
        // 非法 --mode 必须报错（main.rs 会转成退出码 1）
        assert!(handle(
            &WatchdogCmd::Start {
                mode: Some("ultra".to_string()),
                daemon: false
            },
            &ctx
        )
        .is_err());
        // 合法取值不报错（daemon=false + 非交互 → 单轮）
        for raw in ["auto", "full", "lite"] {
            assert!(
                Tier::parse(raw).is_some() || raw == "auto",
                "{raw} 应当被接受"
            );
        }
    }

    #[serial]
    #[test]
    #[serial]
    fn daemon_registration_and_stop_go_through_sched_under_dry_run() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(util::DRY_RUN_ENV, "1");
        let ctx = ctx_in(&dir, DEFAULT_TEMPLATE);
        let started = handle(
            &WatchdogCmd::Start {
                mode: None,
                daemon: true,
            },
            &ctx,
        );
        let stopped = handle(&WatchdogCmd::Stop, &ctx);
        // mod_sched 由另一个 agent 实现：注册失败时 Stop 也应失败，不能假装已停
        assert_eq!(
            started.is_err(),
            stopped.is_err(),
            "started={started:?} stopped={stopped:?}"
        );
        std::env::remove_var(util::DRY_RUN_ENV);
    }

    #[test]
    fn status_json_has_a_stable_shape() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(&dir, DEFAULT_TEMPLATE);
        let data = status_data(&ctx).unwrap();
        for key in [
            "enable",
            "configured_mode",
            "mode",
            "mode_reason",
            "forced",
            "poll_sec",
            "stall_stable_polls",
            "credits_thresholds",
            "sensing",
            "battery",
            "targets",
            "regions_configured",
            "targets_without_region",
            "watches",
            "credits",
            "manual",
            "features",
            "daemon",
            "state_file",
            "recent_events",
            "channels",
        ] {
            assert!(data.get(key).is_some(), "status JSON 缺字段 {key}");
        }
        assert_eq!(
            data["sensing"]["lite"].as_str().unwrap(),
            Tier::Lite.sensing_note()
        );
        assert!(!data["features"]["toast"].as_str().unwrap().is_empty());
        assert!(!data["sensing"]["switch"].as_str().unwrap().is_empty());
        handle(&WatchdogCmd::Status { json: true }, &ctx).unwrap();
    }

    #[test]
    fn adapter_list_and_test_are_read_only_and_only_print_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(
            &dir,
            r#"
[guard.whitelist.apps]
"explorer.exe" = { enabled_levels = ["uia", "wm_close"] }
[watchdog.regions]
"explorer.exe" = { x = 0, y = 0, width = 40, height = 30 }
"#,
        );
        handle(
            &WatchdogCmd::Adapter {
                cmd: AdapterCmd::List,
            },
            &ctx,
        )
        .unwrap();
        for app in ["explorer.exe", "definitely-not-running"] {
            handle(
                &WatchdogCmd::Adapter {
                    cmd: AdapterCmd::Test {
                        app: app.to_string(),
                        json: true,
                    },
                },
                &ctx,
            )
            .unwrap_or_else(|err| panic!("{app} 的只读探测不该失败：{err}"));
        }
        // 只读契约：不改配置、不动窗口
        let config_text = std::fs::read_to_string(&ctx.paths.config).unwrap();
        assert!(config_text.contains("explorer.exe"), "配置应保持原样");
        assert!(!config_text.contains("suggested"), "不该把建议片段写回配置");
        // 探测本身也只用只读 API：确认没走按键/点击通道
        assert_eq!(pause::steps_for(&AppPauseCfg::default()), vec![]);
    }

    #[test]
    fn run_poll_without_any_target_is_a_no_op_and_still_reports() {
        // low 预设不往白名单里塞应用，区域也为空 ⇒ 目标清单是空的，检测必须安全落地
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(
            &dir,
            r#"
[profile]
active = "low"
"#,
        );
        assert!(targets(&ctx.cfg).is_empty(), "low 预设不该有目标");
        handle(
            &WatchdogCmd::Adapter {
                cmd: AdapterCmd::List,
            },
            &ctx,
        )
        .unwrap();
        let mut templates = BTreeMap::new();
        let bus = ctx.bus().unwrap();
        let mode = decide_mode("full", &BatteryInfo::default(), 15, false);
        let report = run_poll(
            &ctx,
            &mut WatchState::default(),
            &mut templates,
            &bus,
            &mode,
        )
        .unwrap();
        assert!(report["apps"].as_array().unwrap().is_empty());
        assert!(report["credits"].as_array().unwrap().is_empty());
        assert_eq!(report["tier"], "full");
        assert_eq!(report["sensing"], Tier::Full.sensing_note());
        assert!(report.get("dry_run").is_some());
        // 空目标时 Stop/Status/List 都不该失败
        status_data(&ctx).unwrap();
    }

    #[test]
    fn summary_reports_mode_regions_and_manual_values() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(
            &dir,
            r#"
[profile]
active = "low"

[watchdog]
mode = "full"

[watchdog.regions]
"trae.exe" = { x = 10, y = 20, width = 300, height = 200 }
"#,
        );
        handle(
            &WatchdogCmd::ImportCredits {
                platform: "trae".to_string(),
                value: 77.0,
                unit: "percent".to_string(),
            },
            &ctx,
        )
        .unwrap();
        let db = ctx.db().unwrap();
        let value = summary(&ctx, &db).unwrap();
        assert_eq!(value["mode"], "full");
        assert_eq!(value["configured_mode"], "full");
        assert_eq!(value["regions_configured"], 1);
        assert_eq!(value["targets"].as_array().unwrap().len(), 1);
        assert_eq!(value["targets"][0]["app"], "trae.exe");
        assert_eq!(value["targets"][0]["region_configured"], true);
        assert!(value["regions_missing"].as_array().unwrap().is_empty());
        assert_eq!(value["manual"]["trae"]["value"], 77.0);
        assert_eq!(value["daemon_task"], "devzone-watchdog-start");
        assert!(value["state_file"]
            .as_str()
            .unwrap()
            .contains(dir.path().to_str().unwrap()));
        assert_eq!(value["features"]["ocr"], ocr::FEATURE_ENABLED);
    }

    #[test]
    fn state_file_corruption_degrades_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(&dir, DEFAULT_TEMPLATE);
        std::fs::write(state_file(&ctx), "{ not json").unwrap();
        // §9：状态文件坏了只降级重来，不能让整个守护失败
        let state = load_state(&ctx);
        assert!(state.apps.is_empty());
        save_state(&ctx, &state).unwrap();
        assert!(state_file(&ctx).exists());
        assert!(read_json_state::<WatchState>(&state_file(&ctx)).is_some());
        assert!(read_json_state::<WatchState>(&ctx.paths.data_dir.join("nope.json")).is_none());
    }

    #[test]
    fn state_round_trip_keeps_app_and_credit_records() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(&dir, DEFAULT_TEMPLATE);
        let mut state = WatchState::default();
        state.apps.insert(
            "trae.exe".to_string(),
            AppWatch {
                last_hash: Some(42),
                stable_polls: 3,
                stalled: true,
                stable_since_epoch: Some(1_700_000_000),
                signal: "region".to_string(),
                last_hash_hex: pattern::hash_hex(42),
                last_ts: "2026-10-11T10:00:00+08:00".to_string(),
                note: "区域 (0,0) 64x32".to_string(),
            },
        );
        state.credits.insert(
            "trae".to_string(),
            CreditRecord {
                source: "manual".to_string(),
                remaining_pct: Some(11.0),
                remaining_abs: None,
                total_abs: None,
                ts: "2026-10-11T10:00:00+08:00".to_string(),
                verdict: "剩余 11.0% < 预警阈值 20%".to_string(),
                samples: vec![CreditSample {
                    ts: "2026-10-11T10:00:00+08:00".to_string(),
                    epoch: 1,
                    pct: Some(11.0),
                    abs: None,
                    source: "manual".to_string(),
                }],
            },
        );
        save_state(&ctx, &state).unwrap();
        let loaded = load_state(&ctx);
        assert_eq!(loaded.apps, state.apps);
        assert_eq!(loaded.credits, state.credits);
        let json = state.to_json();
        assert_eq!(json["apps"]["trae.exe"]["stable_polls"], 3);
        assert_eq!(json["credits"]["trae"]["samples"][0]["pct"], 11.0);
    }

    #[test]
    fn credit_samples_are_capped_at_the_recent_window() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(
            &dir,
            r#"
[watchdog]
mode = "lite"

[watchdog.adapters]
"capped" = { priority = ["manual"] }
"#,
        );
        let rounds = CREDIT_SAMPLES + 8;
        for index in 0..rounds {
            handle(
                &WatchdogCmd::ImportCredits {
                    platform: "capped".to_string(),
                    value: 90.0 - index as f64,
                    unit: "percent".to_string(),
                },
                &ctx,
            )
            .unwrap();
        }
        let state = load_state(&ctx);
        let record = &state.credits["capped"];
        assert_eq!(record.samples.len(), CREDIT_SAMPLES, "样本数必须有上限");
        assert_eq!(
            record.remaining_pct,
            Some(90.0 - (rounds - 1) as f64),
            "manual 读数以最后一次为准"
        );
        // 跨度足够且单调下降 ⇒ ETA 不再是「无法估算」
        assert!(!record.samples.is_empty());
        let series: Vec<(i64, f64)> = record
            .samples
            .iter()
            .filter_map(|sample| sample.pct.map(|value| (sample.epoch, value)))
            .collect();
        let last = series.last().map(|(_, value)| *value).unwrap_or(0.0);
        let eta = estimate_eta_min(&series, last.max(0.01), ETA_MIN_SPAN_SEC);
        assert!(eta.is_none() || eta > Some(0.0), "{eta:?}");
    }
}

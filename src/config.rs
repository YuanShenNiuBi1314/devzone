//! Configuration: `%LOCALAPPDATA%\devzone\config\devzone.toml`.
//!
//! Loading order (build guide section 4): builtin defaults -> profile preset ->
//! user file (per-key override). `devzone profile apply` only writes the
//! `[profile]` section, so user overrides are never flattened.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context as _, Result};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::util::Paths;

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("读取配置文件失败 {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("解析 devzone.toml 失败: {0}")]
    Parse(String),
    #[error("非法配置项: {0}")]
    Invalid(String),
    #[error("未知预设 `{0}`（可选：low | mid | pro）")]
    UnknownProfile(String),
}

/// Everything devzone reads from configuration.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub zone: ZoneCfg,
    pub sched: SchedCfg,
    pub guard: GuardCfg,
    pub crunch: CrunchCfg,
    pub watchdog: WatchdogCfg,
    pub profile: ProfileCfg,
    pub plugin: PluginCfg,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ZoneCfg {
    pub default_alert_free_gb: f64,
    pub top_n: usize,
    pub excludes: Vec<String>,
    pub zones: BTreeMap<String, ZoneDef>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ZoneDef {
    pub path: String,
    pub alert_free_gb: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SchedCfg {
    pub templates: BTreeMap<String, SchedTemplate>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SchedTemplate {
    pub cron: String,
    pub cmd: String,
    pub args: String,
    pub system: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct GuardCfg {
    pub warn_level: u8,
    pub pause_level: u8,
    pub hard_level: u8,
    pub resume_level: u8,
    pub max_wait_min: u32,
    pub battery_poll_sec: u64,
    pub charger_anomaly_min: u32,
    pub pause_timeout_sec: u64,
    /// tier1 | tier2 | tier3 (section 6.3.2 / 6.3.10).
    pub strategy: String,
    /// Master switch for the three-level battery guardian (preset table 4.1).
    pub enable: bool,
    /// Whether the graceful-pause ladder runs before power saving / shutdown.
    pub pause_enabled: bool,
    /// Layer 2 of the checkpoint trio: `git add -A && git commit` per zone repo.
    pub git_checkpoint: bool,
    pub git_pull_on_resume: bool,
    pub power_saving: PowerSavingCfg,
    pub whitelist: WhitelistCfg,
    pub autologon: AutologonCfg,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PowerSavingCfg {
    /// M3 only; M1/M2 just prints the hint (section 6.3.7 item 4).
    pub enable_screen_off: bool,
    pub enable_power_plan_switch: bool,
    pub disable_sleep: bool,
    /// Master switch for the power-saving wait window.
    pub enable: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WhitelistCfg {
    pub apps: BTreeMap<String, AppPauseCfg>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppPauseCfg {
    /// Subset of: hotkey | uia | wm_close | ocr | kill.
    pub enabled_levels: Vec<String>,
    /// Level 4 (OCR + click) stays off unless both this and `ocr` in
    /// `enabled_levels` are set (section 6.3.6).
    pub ocr_fallback: bool,
    /// Optional checkpoint file whose appearance confirms a clean exit.
    pub wait_exit_file: Option<String>,
    /// `watchdog` output region used for stall detection, key `app` of
    /// `[watchdog.regions]`. Defaults to the app key itself.
    pub watch_region: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AutologonCfg {
    /// Default off: the password is stored in clear text in the registry.
    pub enable: bool,
    pub username: String,
    pub domain: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CrunchCfg {
    pub pause_days: u32,
    pub auto_expire: bool,
    /// Master switch used by the presets (section 4.1); `crunch on/off` writes
    /// the registry, this only records whether the user opted in at all.
    pub enable: bool,
    /// Reserved for the M3 tray build (do-not-disturb); no effect until then.
    pub dnd_enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WatchdogCfg {
    /// auto | full | lite (section 6.5.2).
    pub mode: String,
    pub poll_sec_full: u64,
    pub poll_sec_lite: u64,
    pub stall_stable_polls: u32,
    pub credits_warn_pct: u8,
    pub credits_warn_eta_min: u32,
    /// Privacy switch: screenshots are never written to disk unless enabled.
    pub debug_save_screenshots: bool,
    pub enable: bool,
    pub regions: BTreeMap<String, RegionCfg>,
    pub adapters: BTreeMap<String, CreditAdapterCfg>,
    pub notify: NotifyCfg,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RegionCfg {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl RegionCfg {
    pub fn is_empty(&self) -> bool {
        self.width <= 0 || self.height <= 0
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CreditAdapterCfg {
    /// official_api | uia | ocr | manual, tried in order (section 6.5.3).
    pub priority: Vec<String>,
    /// Used by the `official_api` adapter only; never invented by devzone.
    pub api_url: String,
    pub api_token_env: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct NotifyCfg {
    pub console: bool,
    /// Requires the `toast` build feature (M2).
    pub toast: bool,
    pub smtp: SmtpCfg,
    pub feishu: FeishuCfg,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SmtpCfg {
    pub enable: bool,
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub username: String,
    pub password: String,
    pub from: String,
    pub to: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FeishuCfg {
    pub enable: bool,
    pub webhook_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProfileCfg {
    pub active: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PluginCfg {
    pub dir: String,
    pub enable: bool,
    pub default_timeout_sec: u64,
}

impl Default for PowerSavingCfg {
    fn default() -> Self {
        PowerSavingCfg {
            enable_screen_off: false,
            enable_power_plan_switch: true,
            disable_sleep: true,
            enable: true,
        }
    }
}

impl Default for NotifyCfg {
    fn default() -> Self {
        NotifyCfg {
            console: true,
            toast: false,
            smtp: SmtpCfg::default(),
            feishu: FeishuCfg::default(),
        }
    }
}

impl Default for SmtpCfg {
    fn default() -> Self {
        SmtpCfg {
            enable: false,
            host: String::new(),
            port: 465,
            tls: true,
            username: String::new(),
            password: String::new(),
            from: String::new(),
            to: Vec::new(),
        }
    }
}

impl Default for ZoneCfg {
    fn default() -> Self {
        ZoneCfg {
            default_alert_free_gb: 10.0,
            top_n: 20,
            excludes: ["node_modules", "target", ".git", "dist", ".cache"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            zones: BTreeMap::new(),
        }
    }
}

impl Default for SchedCfg {
    fn default() -> Self {
        let mut templates = BTreeMap::new();
        for (name, cron, cmd, args, system) in [
            ("shutdown", "30 9 * * *", "shutdown", "/s /t 60", true),
            ("reminder", "0 * * * *", "devzone", "event remind", false),
            ("cleanup", "0 9 1 * *", "devzone", "zone scan all", false),
        ] {
            templates.insert(
                name.to_string(),
                SchedTemplate {
                    cron: cron.to_string(),
                    cmd: cmd.to_string(),
                    args: args.to_string(),
                    system,
                },
            );
        }
        SchedCfg { templates }
    }
}

impl Default for GuardCfg {
    fn default() -> Self {
        GuardCfg {
            warn_level: 20,
            pause_level: 15,
            hard_level: 5,
            resume_level: 20,
            max_wait_min: 45,
            battery_poll_sec: 60,
            charger_anomaly_min: 10,
            pause_timeout_sec: 60,
            strategy: "tier1".to_string(),
            enable: true,
            pause_enabled: true,
            git_checkpoint: false,
            git_pull_on_resume: false,
            power_saving: PowerSavingCfg {
                enable_screen_off: false,
                enable_power_plan_switch: true,
                disable_sleep: true,
                enable: true,
            },
            whitelist: WhitelistCfg {
                apps: BTreeMap::new(),
            },
            autologon: AutologonCfg {
                enable: false,
                username: String::new(),
                domain: String::new(),
            },
        }
    }
}

impl Default for CrunchCfg {
    fn default() -> Self {
        CrunchCfg {
            pause_days: 14,
            auto_expire: true,
            enable: false,
            dnd_enabled: false,
        }
    }
}

impl Default for WatchdogCfg {
    fn default() -> Self {
        WatchdogCfg {
            mode: "auto".to_string(),
            poll_sec_full: 30,
            poll_sec_lite: 60,
            stall_stable_polls: 5,
            credits_warn_pct: 20,
            credits_warn_eta_min: 30,
            debug_save_screenshots: false,
            enable: true,
            regions: BTreeMap::new(),
            adapters: BTreeMap::new(),
            notify: NotifyCfg {
                console: true,
                toast: false,
                smtp: SmtpCfg {
                    enable: false,
                    host: String::new(),
                    port: 465,
                    tls: true,
                    username: String::new(),
                    password: String::new(),
                    from: String::new(),
                    to: Vec::new(),
                },
                feishu: FeishuCfg {
                    enable: false,
                    webhook_url: String::new(),
                },
            },
        }
    }
}

impl Default for ProfileCfg {
    fn default() -> Self {
        ProfileCfg {
            active: "mid".to_string(),
        }
    }
}

impl Default for PluginCfg {
    fn default() -> Self {
        PluginCfg {
            dir: "%LOCALAPPDATA%/devzone/plugins".to_string(),
            enable: true,
            default_timeout_sec: 30,
        }
    }
}

/// The documented schema, with comments, written on first run.
pub const DEFAULT_TEMPLATE: &str = r#"# devzone.toml — 由 devzone 生成的首启文件
# 加载顺序（build guide §4）：内置默认 -> [profile].active 预设 -> 本文件逐项覆盖。
# 因此本文件默认只激活 [profile].active；其余键一律以注释形式给出（§4 的完整
# schema 与默认值）。把某行取消注释即成为「用户自定义项」，会压过预设。
# devzone profile show 会逐项标出来源（default / preset / user）。

[profile]
active = "mid"               # low | mid | pro

# ---- M1 开发区管理 ----------------------------------------------------------
# [zone]
# default_alert_free_gb = 10   # 剩余空间低于该值触发 zone_alert
# top_n = 20                   # 大文件 Top N
# excludes = ["node_modules", "target", ".git", "dist", ".cache"]
# [zone.zones]                 # devzone zone add 写入
# "work" = { path = "D:\dev", alert_free_gb = 5.0 }

# ---- M2 定时任务中心 --------------------------------------------------------
# [sched]
# [sched.templates]            # 内置 shutdown / reminder / cleanup

# ---- M3 电源守护 ------------------------------------------------------------
# [guard]
# warn_level = 20
# pause_level = 15
# hard_level = 5
# resume_level = 20
# max_wait_min = 45
# battery_poll_sec = 60
# charger_anomaly_min = 10
# pause_timeout_sec = 60
# strategy = "tier1"           # tier1 | tier2 | tier3
# enable = true
# pause_enabled = true
# git_checkpoint = false
# git_pull_on_resume = false
# [guard.power_saving]
# enable = true
# enable_screen_off = false    # M3 实现，M1/M2 仅提示
# enable_power_plan_switch = true
# disable_sleep = true
# [guard.whitelist]
# [guard.whitelist.apps]
# "doubao.exe" = { enabled_levels = ["hotkey", "uia", "wm_close"] }
# "trae.exe"   = { enabled_levels = ["hotkey", "uia", "wm_close"], ocr_fallback = false }
# [guard.autologon]
# enable = false               # 默认关闭：密码明文写入注册表
# username = ""
# domain = ""
# password 不写入 devzone.toml；启用时交互输入一次并写入注册表

# ---- M4 打扰阻断 ------------------------------------------------------------
# [crunch]
# pause_days = 14              # 上限 35 天（Windows 限制）
# auto_expire = true
# enable = false
# dnd_enabled = false          # 勿扰：M3 托盘实现，此处仅预留

# ---- M5 AI 交付守护 ---------------------------------------------------------
# [watchdog]
# mode = "auto"                # auto | full | lite
# poll_sec_full = 30
# poll_sec_lite = 60
# stall_stable_polls = 5
# credits_warn_pct = 20
# credits_warn_eta_min = 30
# debug_save_screenshots = false
# enable = true
# [watchdog.regions]           # devzone watchdog adapter test 会打印建议片段
# "trae.exe" = { x = 400, y = 120, width = 900, height = 620 }
# [watchdog.adapters]          # official_api > uia > ocr > manual
# "doubao" = { priority = ["uia", "manual"] }
# "trae"   = { priority = ["uia", "manual"] }
# [watchdog.notify]
# console = true
# toast = false
# [watchdog.notify.smtp]
# enable = false
# host = "smtp.qq.com"
# port = 465
# tls = true
# username = ""
# password = ""
# from = ""
# to = []
# [watchdog.notify.feishu]
# enable = false
# webhook_url = ""

# ---- M7 插件 ----------------------------------------------------------------
# [plugin]
# dir = "%LOCALAPPDATA%/devzone/plugins"
# enable = true
# default_timeout_sec = 30
"#;

/// Values applied by a preset on top of the builtin defaults (section 4.1).
/// Each entry is a dotted path inside the config tree.
pub fn preset_values(name: &str) -> Result<Vec<(&'static str, serde_json::Value)>> {
    let patches = match name {
        "low" => vec![
            ("guard.enable", false.into()),
            ("guard.pause_enabled", false.into()),
            ("guard.power_saving.enable", false.into()),
            ("guard.autologon.enable", false.into()),
            ("crunch.enable", false.into()),
            ("watchdog.enable", false.into()),
            ("watchdog.mode", "lite".into()),
            ("plugin.enable", false.into()),
            ("watchdog.notify.console", true.into()),
            ("watchdog.notify.smtp.enable", false.into()),
            ("watchdog.notify.feishu.enable", false.into()),
        ],
        "mid" => vec![
            ("guard.enable", true.into()),
            ("guard.warn_level", 20u8.into()),
            ("guard.pause_level", 15u8.into()),
            ("guard.hard_level", 5u8.into()),
            ("guard.resume_level", 20u8.into()),
            ("guard.pause_enabled", true.into()),
            ("guard.strategy", "tier1".into()),
            (
                "guard.whitelist.apps",
                preset_whitelist(&["hotkey", "uia", "wm_close"]),
            ),
            ("guard.power_saving.enable", true.into()),
            ("crunch.enable", true.into()),
            ("crunch.pause_days", 14u32.into()),
            ("watchdog.enable", true.into()),
            ("watchdog.mode", "auto".into()),
            ("plugin.enable", true.into()),
            ("watchdog.notify.console", true.into()),
            ("watchdog.notify.toast", false.into()),
            ("guard.autologon.enable", false.into()),
        ],
        "pro" => vec![
            ("guard.enable", true.into()),
            ("guard.warn_level", 20u8.into()),
            ("guard.pause_level", 15u8.into()),
            ("guard.hard_level", 5u8.into()),
            ("guard.resume_level", 20u8.into()),
            ("guard.pause_enabled", true.into()),
            (
                "guard.whitelist.apps",
                preset_whitelist(&["hotkey", "uia", "wm_close", "ocr"]),
            ),
            ("guard.power_saving.enable", true.into()),
            ("guard.git_checkpoint", true.into()),
            ("crunch.enable", true.into()),
            ("crunch.pause_days", 21u32.into()),
            ("watchdog.enable", true.into()),
            ("watchdog.mode", "full".into()),
            ("watchdog.poll_sec_full", 15u64.into()),
            ("plugin.enable", true.into()),
            ("watchdog.notify.console", true.into()),
            ("watchdog.notify.toast", true.into()),
            ("guard.autologon.enable", false.into()),
        ],
        other => return Err(ConfigError::UnknownProfile(other.to_string()).into()),
    };
    Ok(patches)
}

fn preset_whitelist(levels: &[&str]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for app in ["doubao.exe", "trae.exe"] {
        map.insert(
            app.to_string(),
            serde_json::json!({
                "enabled_levels": levels,
                "ocr_fallback": false,
            }),
        );
    }
    serde_json::Value::Object(map)
}

impl Config {
    /// Builtin defaults only (no preset, no user file).
    pub fn builtin() -> Config {
        Config {
            zone: ZoneCfg::default(),
            sched: SchedCfg::default(),
            guard: GuardCfg::default(),
            crunch: CrunchCfg::default(),
            watchdog: WatchdogCfg::default(),
            profile: ProfileCfg::default(),
            plugin: PluginCfg::default(),
        }
    }

    /// builtin -> preset -> user file, then validate.
    pub fn load(paths: &Paths) -> Result<Config> {
        let user_raw = if paths.config.exists() {
            std::fs::read_to_string(&paths.config)
                .map_err(|source| ConfigError::Io {
                    path: paths.config.display().to_string(),
                    source,
                })
                .map_err(anyhow::Error::from)?
        } else {
            String::new()
        };
        Self::assemble(&user_raw).with_context(|| format!("配置 {}", paths.config.display()))
    }

    /// Same merge for a raw TOML string; unit-testable without touching disk.
    pub fn assemble(user_raw: &str) -> Result<Config> {
        let mut tree = json_of(&Config::builtin())?;
        let user: serde_json::Value = if user_raw.trim().is_empty() {
            serde_json::json!({})
        } else {
            let toml_value: toml::Value =
                toml::from_str(user_raw).map_err(|err| ConfigError::Parse(err.to_string()))?;
            json_of(&toml_value)?
        };
        let active = dig_str(&user, "profile.active")
            .or_else(|| dig_str(&tree, "profile.active"))
            .unwrap_or("mid")
            .to_string();
        // 1. preset layer (only keys the user file does not set are relevant,
        //    but the merge order below already gives the user file priority).
        let mut preset_tree = serde_json::json!({});
        for (dotted, value) in preset_values(&active)? {
            insert_dotted(&mut preset_tree, dotted, value);
        }
        merge(&mut tree, &preset_tree);
        merge(&mut tree, &user);
        let mut cfg: Config = serde_json::from_value(tree)
            .map_err(|err| ConfigError::Parse(format!("类型不匹配：{err}")))?;
        cfg.profile.active = active;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Keys explicitly present in the user file, as dotted paths. `profile show`
    /// marks these as user overrides (section 6.6).
    pub fn user_override_paths(user_raw: &str) -> Result<Vec<String>> {
        if user_raw.trim().is_empty() {
            return Ok(Vec::new());
        }
        let toml_value: toml::Value =
            toml::from_str(user_raw).map_err(|err| ConfigError::Parse(err.to_string()))?;
        let json = json_of(&toml_value)?;
        let mut out = Vec::new();
        collect_paths(&json, "", &mut out);
        out.sort();
        Ok(out)
    }

    pub fn validate(&self) -> Result<()> {
        let mut problems = Vec::new();
        let g = &self.guard;
        if !matches!(g.strategy.as_str(), "tier1" | "tier2" | "tier3") {
            problems.push(format!("guard.strategy = `{}` 非法", g.strategy));
        }
        if g.hard_level > g.pause_level || g.pause_level > g.warn_level {
            problems.push("guard 阈值需满足 hard_level <= pause_level <= warn_level".to_string());
        }
        if g.resume_level < g.pause_level {
            problems.push("guard.resume_level 不应低于 pause_level（会来回抖动）".to_string());
        }
        if g.battery_poll_sec == 0 {
            problems.push("guard.battery_poll_sec 必须 > 0".to_string());
        }
        if !matches!(self.watchdog.mode.as_str(), "auto" | "full" | "lite") {
            problems.push(format!("watchdog.mode = `{}` 非法", self.watchdog.mode));
        }
        if self.crunch.pause_days == 0 || self.crunch.pause_days > 35 {
            problems.push(format!(
                "crunch.pause_days = {} 超出 1..=35",
                self.crunch.pause_days
            ));
        }
        if self.zone.top_n == 0 {
            problems.push("zone.top_n 必须 > 0".to_string());
        }
        for (name, zone) in &self.zone.zones {
            if zone.path.trim().is_empty() {
                problems.push(format!("zone.zones.{name}.path 为空"));
            }
        }
        let smtp = &self.watchdog.notify.smtp;
        if smtp.enable && (smtp.host.trim().is_empty() || smtp.to.is_empty()) {
            problems.push("watchdog.notify.smtp.enable=true 需要 host 与 to".to_string());
        }
        let feishu = &self.watchdog.notify.feishu;
        if feishu.enable && feishu.webhook_url.trim().is_empty() {
            problems.push("watchdog.notify.feishu.enable=true 需要 webhook_url".to_string());
        }
        if !problems.is_empty() {
            return Err(ConfigError::Invalid(problems.join("；")).into());
        }
        Ok(())
    }

    /// Effective alert threshold for a zone.
    pub fn alert_free_gb_for(&self, zone: &ZoneDef) -> f64 {
        zone.alert_free_gb
            .unwrap_or(self.zone.default_alert_free_gb)
    }

    /// `[plugin].dir` with `%VAR%` expanded; falls back to the data layout.
    ///
    /// When the data root is relocated through `DEVZONE_HOME` (tests, portable
    /// setups) the documented default follows that root instead of pointing at
    /// the real `%LOCALAPPDATA%`, so no test can ever execute a plugin that
    /// belongs to the logged-in user.
    pub fn plugin_dir(&self, paths: &Paths) -> PathBuf {
        let expanded = expand_env(&self.plugin.dir);
        if expanded.trim().is_empty() {
            return paths.plugins.clone();
        }
        if std::env::var_os(crate::util::HOME_ENV).is_some()
            && self.plugin.dir.trim() == PluginCfg::default().dir
        {
            return paths.plugins.clone();
        }
        PathBuf::from(expanded)
    }

    /// Poll interval for the current sensing mode (section 6.5.2).
    pub fn poll_sec(&self, lite: bool) -> u64 {
        if lite {
            self.watchdog.poll_sec_lite
        } else {
            self.watchdog.poll_sec_full
        }
    }

    /// Whitelisted apps in deterministic order.
    pub fn whitelisted_apps(&self) -> Vec<(String, AppPauseCfg)> {
        self.guard
            .whitelist
            .apps
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

/// Expand `%LOCALAPPDATA%\devzone` style paths.
pub fn expand_env(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        if value.as_bytes()[idx] == b'%' {
            if let Some(end) = value[idx + 1..].find('%') {
                let name = &value[idx + 1..idx + 1 + end];
                let replacement = match std::env::var(name) {
                    Ok(val) => val,
                    Err(_) => match name {
                        "LOCALAPPDATA" => dirs::data_local_dir()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                        "USERPROFILE" => dirs::home_dir()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                        _ => format!("%{name}%"),
                    },
                };
                out.push_str(&replacement);
                idx += end + 2;
                continue;
            }
        }
        out.push(value.chars().nth(idx).unwrap_or(' '));
        idx += 1;
    }
    out
}

fn json_of<T: Serialize>(value: &T) -> Result<serde_json::Value> {
    serde_json::to_value(value).context("serialize config tree")
}

/// Deep merge `from` into `to` (objects merged recursively, everything else replaced).
fn merge(to: &mut serde_json::Value, from: &serde_json::Value) {
    match (to, from) {
        (serde_json::Value::Object(base), serde_json::Value::Object(extra)) => {
            for (key, value) in extra {
                let entry = base
                    .entry(key.clone())
                    .or_insert_with(|| serde_json::Value::Object(Default::default()));
                if value.is_object() && entry.is_object() {
                    merge(entry, value);
                } else {
                    *entry = value.clone();
                }
            }
        }
        (target, other) => *target = other.clone(),
    }
}

fn insert_dotted(tree: &mut serde_json::Value, dotted: &str, value: serde_json::Value) {
    let mut current = tree;
    let parts: Vec<&str> = dotted.split('.').collect();
    for part in &parts[..parts.len() - 1] {
        current = current
            .as_object_mut()
            .expect("object while inserting preset path")
            .entry(*part)
            .or_insert_with(|| serde_json::json!({}));
    }
    if value.is_object() {
        let entry = current
            .as_object_mut()
            .expect("object while inserting preset path")
            .entry(*parts.last().expect("non-empty dotted path"))
            .or_insert_with(|| serde_json::json!({}));
        if entry.is_object() {
            merge(entry, &value);
            return;
        }
    }
    current
        .as_object_mut()
        .expect("object while inserting preset path")
        .insert(
            (*parts.last().expect("non-empty dotted path")).to_string(),
            value,
        );
}

fn dig_str<'a>(value: &'a serde_json::Value, dotted: &str) -> Option<&'a str> {
    let mut current = value;
    for part in dotted.split('.') {
        current = current.get(part)?;
    }
    current.as_str()
}

fn collect_paths(value: &serde_json::Value, prefix: &str, out: &mut Vec<String>) {
    if let serde_json::Value::Object(map) = value {
        for (key, child) in map {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            if child.is_object() && !child.as_object().map(|m| m.is_empty()).unwrap_or(true) {
                collect_paths(child, &path, out);
            } else {
                out.push(path);
            }
        }
    }
}

/// Read the raw user file (empty string when absent).
pub fn read_user_raw(paths: &Paths) -> Result<String> {
    if !paths.config.exists() {
        return Ok(String::new());
    }
    std::fs::read_to_string(&paths.config)
        .map_err(|source| ConfigError::Io {
            path: paths.config.display().to_string(),
            source,
        })
        .map_err(anyhow::Error::from)
}

/// Write the documented template if no config file exists yet.
pub fn ensure_config_file(paths: &Paths) -> Result<bool> {
    paths.ensure_dirs()?;
    if paths.config.exists() {
        return Ok(false);
    }
    std::fs::write(&paths.config, DEFAULT_TEMPLATE)
        .with_context(|| format!("write {}", paths.config.display()))?;
    Ok(true)
}

/// Surgical edits on the user file via `toml_edit`, preserving comments
/// and every key the CLI does not own.
pub struct Editor {
    text: String,
}

impl Editor {
    pub fn open(paths: &Paths) -> Result<Editor> {
        let text = if paths.config.exists() {
            std::fs::read_to_string(&paths.config)?
        } else {
            DEFAULT_TEMPLATE.to_string()
        };
        Ok(Editor { text })
    }

    pub fn set(&mut self, table: &str, key: &str, value: &str) -> Result<()> {
        let mut doc: toml_edit::DocumentMut = self
            .text
            .parse()
            .map_err(|err: toml_edit::TomlError| ConfigError::Parse(err.to_string()))?;
        let parsed: toml_edit::Value = value
            .parse()
            .map_err(|err: toml_edit::TomlError| anyhow!("非法 TOML 值 `{value}`: {err}"))?;
        let root = doc.as_table_mut();
        let target: &mut toml_edit::Table = if table.is_empty() {
            root
        } else {
            ensure_table(root, table)?
        };
        target.insert(key, toml_edit::Item::Value(parsed));
        self.text = doc.to_string();
        Ok(())
    }

    /// `set_table("zone.zones", "work", "{ path = \"D:\\\dev\", alert_free_gb = 5.0 }")`
    pub fn set_table_entry(&mut self, table: &str, key: &str, inline: &str) -> Result<()> {
        let mut doc: toml_edit::DocumentMut = self
            .text
            .parse()
            .map_err(|err: toml_edit::TomlError| ConfigError::Parse(err.to_string()))?;
        let value: toml_edit::Value = inline
            .parse()
            .map_err(|err: toml_edit::TomlError| anyhow!("非法 TOML 内联表 `{inline}`: {err}"))?;
        let root = doc.as_table_mut();
        let target: &mut toml_edit::Table = if table.is_empty() {
            root
        } else {
            ensure_table(root, table)?
        };
        target.insert(key, toml_edit::Item::Value(value));
        self.text = doc.to_string();
        Ok(())
    }

    pub fn remove_entry(&mut self, table: &str, key: &str) -> Result<bool> {
        let mut doc: toml_edit::DocumentMut = self
            .text
            .parse()
            .map_err(|err: toml_edit::TomlError| ConfigError::Parse(err.to_string()))?;
        let root = doc.as_table_mut();
        let target: &mut toml_edit::Table = if table.is_empty() {
            root
        } else {
            match ensure_table(root, table) {
                Ok(t) => t,
                Err(_) => return Ok(false),
            }
        };
        let existed = target.remove(key).is_some();
        self.text = doc.to_string();
        Ok(existed)
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        paths.ensure_dirs()?;
        // Validate before writing: a broken devzone.toml must never be produced.
        let parsed: toml::Value = toml::from_str(&self.text)
            .map_err(|err: toml::de::Error| ConfigError::Parse(err.to_string()))?;
        let json = json_of(&parsed)?;
        let probe: Config = serde_json::from_value(json)
            .map_err(|err| ConfigError::Parse(format!("类型不匹配：{err}")))?;
        probe.validate()?;
        std::fs::write(&paths.config, &self.text)
            .with_context(|| format!("write {}", paths.config.display()))?;
        Ok(())
    }
}

fn ensure_table<'a>(
    root: &'a mut toml_edit::Table,
    dotted: &str,
) -> Result<&'a mut toml_edit::Table> {
    descend(root, &dotted.split('.').collect::<Vec<_>>())
}

/// Recursively create/return the table addressed by a dotted path.
fn descend<'a>(
    table: &'a mut toml_edit::Table,
    parts: &[&str],
) -> Result<&'a mut toml_edit::Table> {
    if parts.is_empty() {
        return Ok(table);
    }
    if table.get(parts[0]).is_none() {
        let mut created = toml_edit::Table::new();
        created.set_implicit(true);
        table.insert(parts[0], toml_edit::Item::Table(created));
    }
    let entry = table.get_mut(parts[0]).expect("table just inserted");
    let sub = entry
        .as_table_mut()
        .ok_or_else(|| anyhow!("配置路径中的 `{}` 不是表，无法写入", parts[0]))?;
    descend(sub, &parts[1..])
}

/// Format a `ZoneDef` as a TOML inline table (path escaping included).
pub fn zone_inline(path: &Path, alert_free_gb: f64) -> String {
    let escaped = path.display().to_string().replace('\\', "\\\\");
    format!("{{ path = \"{escaped}\", alert_free_gb = {alert_free_gb} }}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn template_parses_and_validates() {
        let cfg = Config::assemble(DEFAULT_TEMPLATE).unwrap();
        assert_eq!(cfg.zone.default_alert_free_gb, 10.0);
        assert_eq!(cfg.guard.warn_level, 20);
        assert_eq!(cfg.crunch.pause_days, 14);
        assert_eq!(cfg.watchdog.notify.smtp.port, 465);
        assert_eq!(cfg.plugin.dir, "%LOCALAPPDATA%/devzone/plugins");
    }

    #[test]
    fn low_preset_switches_everything_off() {
        let cfg = Config::assemble("[profile]\nactive = \"low\"\n").unwrap();
        assert!(!cfg.guard.enable);
        assert!(!cfg.guard.pause_enabled);
        assert!(!cfg.guard.power_saving.enable);
        assert!(!cfg.crunch.enable);
        assert!(!cfg.watchdog.enable);
        assert!(!cfg.plugin.enable);
    }

    #[test]
    fn user_file_beats_preset() {
        let cfg = Config::assemble(
            "[profile]\nactive = \"mid\"\n[guard]\npause_level = 8\n[crunch]\npause_days = 30\n",
        )
        .unwrap();
        assert_eq!(cfg.guard.pause_level, 8);
        assert_eq!(cfg.crunch.pause_days, 30);
        // preset key the user did not touch keeps the preset value
        assert_eq!(cfg.guard.warn_level, 20);
        assert_eq!(cfg.guard.strategy, "tier1");
    }

    #[test]
    fn mid_preset_fills_whitelist_apps() {
        let cfg = Config::assemble("[profile]\nactive = \"mid\"\n").unwrap();
        let apps = cfg.guard.whitelist.apps;
        assert!(apps.contains_key("doubao.exe"));
        assert_eq!(
            apps["trae.exe"].enabled_levels,
            vec!["hotkey", "uia", "wm_close"]
        );
        assert!(!apps["trae.exe"].ocr_fallback);
    }

    #[test]
    fn pro_preset_uses_full_mode_and_ocr_level() {
        let cfg = Config::assemble("[profile]\nactive = \"pro\"\n").unwrap();
        assert_eq!(cfg.watchdog.mode, "full");
        assert_eq!(cfg.crunch.pause_days, 21);
        assert!(cfg.guard.git_checkpoint);
        assert!(cfg.guard.whitelist.apps["doubao.exe"]
            .enabled_levels
            .contains(&"ocr".to_string()));
    }

    #[test]
    fn rejects_bad_values() {
        let err = Config::assemble("[guard]\nwarn_level = 5\npause_level = 15\n").unwrap_err();
        assert!(
            err.to_string().contains("hard_level <= pause_level"),
            "{err}"
        );
        let err = Config::assemble("[profile]\nactive = \"ultra\"\n").unwrap_err();
        assert!(err.to_string().contains("未知预设"), "{err}");
        let err = Config::assemble("[crunch]\npause_days = 90\n").unwrap_err();
        assert!(err.to_string().contains("1..=35"), "{err}");
        let err = Config::assemble("[watchdog]\nmode = \"turbo\"\n").unwrap_err();
        assert!(err.to_string().contains("watchdog.mode"), "{err}");
        let err = Config::assemble("[watchdog.notify.feishu]\nenable = true\n").unwrap_err();
        assert!(err.to_string().contains("webhook_url"), "{err}");
        assert!(Config::assemble("[profile]\nactive = 3\n").is_err());
    }

    #[test]
    fn override_paths_are_reported() {
        let paths = Config::user_override_paths("[guard]\npause_level = 12\n").unwrap();
        assert_eq!(paths, vec!["guard.pause_level".to_string()]);
        let paths = Config::user_override_paths("[watchdog.notify.smtp]\nhost = \"smtp.qq.com\"\n")
            .unwrap();
        assert_eq!(paths, vec!["watchdog.notify.smtp.host".to_string()]);
        assert!(Config::user_override_paths("").unwrap().is_empty());
    }

    #[serial]
    #[test]
    fn env_expansion() {
        std::env::set_var("DEVZONE_TEST_VAR", "X:\\y");
        assert_eq!(expand_env("%DEVZONE_TEST_VAR%\\plugins"), "X:\\y\\plugins");
        assert_eq!(expand_env("plain"), "plain");
        assert_eq!(expand_env("%DEVZONE_MISSING%"), "%DEVZONE_MISSING%");
        std::env::remove_var("DEVZONE_TEST_VAR");
        assert!(expand_env("%LOCALAPPDATA%/devzone/plugins").contains("devzone"));
    }

    #[test]
    fn zone_inline_escapes_backslashes() {
        let inline = zone_inline(Path::new("D:\\dev"), 5.0);
        assert_eq!(inline, "{ path = \"D:\\\\dev\", alert_free_gb = 5 }");
        let cfg = Config::assemble(&format!("[zone.zones]\nwork = {inline}\n")).unwrap();
        assert_eq!(cfg.zone.zones["work"].path, "D:\\dev");
    }

    #[test]
    fn alert_threshold_falls_back_to_default() {
        let mut cfg = Config::builtin();
        let zone = ZoneDef {
            path: "D:\\x".into(),
            alert_free_gb: None,
        };
        assert!((cfg.alert_free_gb_for(&zone) - 10.0).abs() < f64::EPSILON);
        cfg.zone.default_alert_free_gb = 2.0;
        assert!((cfg.alert_free_gb_for(&zone) - 2.0).abs() < f64::EPSILON);
    }

    #[serial]
    #[test]
    fn relocated_data_root_pulls_plugins_inside_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        let cfg = Config::builtin();
        std::env::set_var(crate::util::HOME_ENV, dir.path());
        assert_eq!(cfg.plugin_dir(&paths), paths.plugins);
        // an explicit directory from the user is still honoured
        let mut custom = cfg.clone();
        custom.plugin.dir = "D:\\my-plugins".to_string();
        assert_eq!(custom.plugin_dir(&paths), PathBuf::from("D:\\my-plugins"));
        std::env::remove_var(crate::util::HOME_ENV);
        assert!(cfg.plugin_dir(&paths).to_string_lossy().contains("devzone"));
    }

    #[test]
    fn poll_interval_per_mode() {
        let cfg = Config::builtin();
        assert_eq!(cfg.poll_sec(true), 60);
        assert_eq!(cfg.poll_sec(false), 30);
    }

    #[test]
    fn editor_roundtrip_preserves_comments() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        std::fs::write(&paths.config, DEFAULT_TEMPLATE).unwrap();
        let mut editor = Editor::open(&paths).unwrap();
        editor.set("profile", "active", "\"low\"").unwrap();
        editor
            .set_table_entry(
                "zone.zones",
                "work",
                &zone_inline(Path::new("D:\\dev"), 5.0),
            )
            .unwrap();
        editor.save(&paths).unwrap();
        let text = std::fs::read_to_string(&paths.config).unwrap();
        assert!(text.contains("加载顺序"), "comments lost");
        let cfg = Config::load(&paths).unwrap();
        assert_eq!(cfg.profile.active, "low");
        assert_eq!(cfg.zone.zones["work"].path, "D:\\dev");
        assert!(editor.remove_entry("zone.zones", "work").unwrap());
        assert!(!editor.remove_entry("zone.zones", "work").unwrap());
    }

    #[test]
    fn editor_refuses_broken_config() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        std::fs::write(&paths.config, DEFAULT_TEMPLATE).unwrap();
        let mut editor = Editor::open(&paths).unwrap();
        editor.set("crunch", "pause_days", "99").unwrap();
        assert!(editor.save(&paths).is_err());
    }

    #[test]
    fn ensure_config_file_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        assert!(ensure_config_file(&paths).unwrap());
        assert!(!ensure_config_file(&paths).unwrap());
        assert!(paths.config.exists());
        assert!(Config::load(&paths).is_ok());
    }
}

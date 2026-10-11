//! CLI command tree, exactly as specified in build guide section 5.1.

use clap::{Parser, Subcommand};

/// DevZone —— 本地优先的 Windows 开发环境自治工具。
///
/// 划定开发区 → 自动盯占用 → 守住电源与更新 → 盯 AI 交付会话 → 通知到手机。
#[derive(Parser, Debug)]
#[command(name = "devzone", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
    /// 覆盖写操作（重复 add 同名项时用 --force 替换）
    #[arg(long, global = true)]
    pub force: bool,
    /// 内部标志：已是提权后的子进程，不再二次提权
    #[arg(long, global = true, hide = true)]
    pub elevated: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// 开发区管理：目录占用、大文件、快照对比、空间预警
    Zone {
        #[command(subcommand)]
        cmd: ZoneCmd,
    },
    /// 定时任务中心（封装 schtasks，任务名统一加 devzone- 前缀）
    Sched {
        #[command(subcommand)]
        cmd: SchedCmd,
    },
    /// 电源守护：三电平电量状态机、优雅暂停、省电等待、恢复
    Guard {
        #[command(subcommand)]
        cmd: GuardCmd,
    },
    /// 打扰阻断（交付模式）：限时暂停 Windows 更新
    Crunch {
        #[command(subcommand)]
        cmd: CrunchCmd,
    },
    /// AI 交付守护：停顿检测、积分水位、通知
    Watchdog {
        #[command(subcommand)]
        cmd: WatchdogCmd,
    },
    /// 分层预设 low / mid / pro
    Profile {
        #[command(subcommand)]
        cmd: ProfileCmd,
    },
    /// 插件管理
    Plugin {
        #[command(subcommand)]
        cmd: PluginCmd,
    },
    /// 触发一个事件（写审计、发通知、调用订阅插件）
    Event {
        /// 事件名，见 docs 事件表
        name: String,
        /// 事件上下文 JSON（字符串或 @文件）
        #[arg(long, value_name = "CONTEXT")]
        json: Option<String>,
    },
    /// 环境自检（9 项，逐项 [✓]/[△]/[✗]）
    Doctor,
    /// 当前状态总览
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ZoneCmd {
    /// 登记一个开发区（路径或盘符），写入配置并立即扫描一次
    Add {
        /// 目录或盘符，如 D:\\dev 或 E:
        path: String,
        #[arg(long)]
        name: Option<String>,
        /// 该开发区的剩余空间预警阈值（GB）
        #[arg(long = "alert-free", value_name = "GB")]
        alert_free: Option<f64>,
    },
    /// 列出已登记的开发区
    List,
    /// 扫描开发区并写入快照
    Scan {
        /// 开发区名，或 all 扫描全部
        name: String,
    },
    /// 输出当前指标与最近一次快照的差值
    Report {
        name: String,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        top: Option<usize>,
    },
    /// 删除一个开发区（含其快照历史）
    Remove { name: String },
    /// 循环扫描并在阈值命中时通知
    Watch {
        #[arg(long, value_name = "SEC", default_value_t = 3600)]
        interval: u64,
        /// 注册为计划任务后台运行
        #[arg(long)]
        daemon: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum SchedCmd {
    /// 新建计划任务；--cron 支持子集（见 docs）
    Add {
        name: String,
        #[arg(long)]
        cron: String,
        #[arg(long)]
        cmd: String,
        #[arg(long)]
        args: Option<String>,
        /// 以 SYSTEM / 最高权限运行（需管理员）
        #[arg(long)]
        system: bool,
    },
    /// 从模板库创建一个任务
    Template {
        /// shutdown | reminder | cleanup
        name: String,
        /// 实例名（计划任务名后缀）
        instance: Option<String>,
    },
    /// 列出 devzone- 前缀的任务
    List,
    /// 删除任务
    Remove { name: String },
    /// 立即运行一次
    Run { name: String },
}

#[derive(Subcommand, Debug)]
pub enum GuardCmd {
    /// 守护状态总览（状态机、阈值、tier 策略提示）
    Status {
        #[arg(long)]
        json: bool,
    },
    /// 读一次电池状态
    Battery {
        #[arg(long)]
        json: bool,
        /// 轮询 N 次（每次间隔 battery_poll_sec 秒）
        #[arg(long, default_value_t = 1)]
        times: u32,
    },
    /// 手动对某个应用执行优雅暂停阶梯（测试用）
    PauseOn {
        /// 进程名，如 doubao.exe
        app: String,
    },
    /// 手动触发恢复：读最新 checkpoint 并拉起清单
    Resume,
    /// 定时关机（当天 HH:MM）
    Schedule { time: String },
    /// 唤醒定时器（S3 可靠；S4 依赖主板，见 doctor 提示）
    Wake { time: String },
    /// 前台运行状态机（§6.3.2 的常驻驱动；--daemon 注册为计划任务）
    Watch {
        #[arg(long, value_name = "SEC")]
        interval: Option<u64>,
        /// 只跑一轮（冒烟测试用）
        #[arg(long)]
        once: bool,
        /// 注册登录时运行的守护任务
        #[arg(long)]
        daemon: bool,
    },
    /// 无人值守自动登录（默认关闭，密码明文写注册表）
    Autologon {
        #[command(subcommand)]
        cmd: AutologonCmd,
    },
}

#[derive(Subcommand, Debug)]
pub enum AutologonCmd {
    /// 交互输入密码并写入 Winlogon 注册表（需管理员）
    Enable {
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        domain: Option<String>,
    },
    /// 删除 DefaultPassword 并关闭 AutoAdminLogon
    Disable,
    /// 读取当前注册表状态（不回显密码）
    Status,
}

#[derive(Subcommand, Debug)]
pub enum CrunchCmd {
    /// 暂停更新 N 天（上限 35，需管理员）
    On {
        #[arg(long)]
        days: Option<u32>,
    },
    /// 删除暂停键值，恢复更新（需管理员）
    Off,
    /// 读取当前暂停状态；已过期且 auto_expire=true 时自动恢复
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum WatchdogCmd {
    /// 启动守护（前台循环；--daemon 注册计划任务）
    Start {
        /// 覆盖配置里的感知模式 auto|full|lite
        #[arg(long)]
        mode: Option<String>,
        #[arg(long)]
        daemon: bool,
    },
    /// 停止守护（删除其计划任务）
    Stop,
    /// 当前状态：模式、目标应用、最近事件
    Status {
        #[arg(long)]
        json: bool,
    },
    /// 适配器
    Adapter {
        #[command(subcommand)]
        cmd: AdapterCmd,
    },
    /// 手动导入某平台的剩余额度（manual 通道，始终可用）
    ImportCredits {
        platform: String,
        /// 百分比（0-100）或绝对数值
        value: f64,
        /// 数值单位说明，写入审计日志
        #[arg(long, default_value = "credits")]
        unit: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum AdapterCmd {
    /// 列出配置的感知适配器与可用级别
    List,
    /// 实测某应用当前可用级别（只读，不做真实点击）
    Test {
        app: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ProfileCmd {
    /// 套用预设（只写 [profile].active，不覆盖用户自定义项）
    Apply {
        /// low | mid | pro
        name: String,
    },
    /// 显示生效配置，并标出被用户覆盖的项
    Show {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum PluginCmd {
    /// 列出已安装插件及订阅事件
    List,
    /// 从目录安装插件（校验 plugin.toml）
    Install { dir: String },
    /// 直接派发一个事件给插件（同 `devzone event`）
    Run {
        event: String,
        #[arg(long, value_name = "CONTEXT")]
        json: Option<String>,
    },
}

impl Command {
    /// True when this invocation asked for machine-readable stdout.
    pub fn wants_json(&self) -> bool {
        match self {
            Command::Zone { cmd } => matches!(cmd, ZoneCmd::Report { json: true, .. }),
            Command::Guard { cmd } => {
                matches!(
                    cmd,
                    GuardCmd::Status { json: true } | GuardCmd::Battery { json: true, .. }
                ) || matches!(
                    cmd,
                    GuardCmd::Autologon {
                        cmd: AutologonCmd::Status
                    }
                )
            }
            Command::Crunch { cmd } => matches!(cmd, CrunchCmd::Status { json: true }),
            Command::Watchdog { cmd } => matches!(
                cmd,
                WatchdogCmd::Status { json: true }
                    | WatchdogCmd::Adapter {
                        cmd: AdapterCmd::Test { json: true, .. }
                    }
            ),
            Command::Profile { cmd } => matches!(cmd, ProfileCmd::Show { json: true }),
            Command::Status { json } => *json,
            _ => false,
        }
    }

    /// Label used in the JSON envelope (`zone report`, `guard battery`, ...).
    pub fn label(&self) -> String {
        match self {
            Command::Zone { cmd } => format!("zone {}", zone_tail(cmd)),
            Command::Sched { cmd } => format!("sched {}", sched_tail(cmd)),
            Command::Guard { cmd } => format!("guard {}", guard_tail(cmd)),
            Command::Crunch { cmd } => format!("crunch {}", crunch_tail(cmd)),
            Command::Watchdog { cmd } => format!("watchdog {}", watchdog_tail(cmd)),
            Command::Profile { cmd } => format!("profile {}", profile_tail(cmd)),
            Command::Plugin { cmd } => format!("plugin {}", plugin_tail(cmd)),
            Command::Event { .. } => "event".to_string(),
            Command::Doctor => "doctor".to_string(),
            Command::Status { .. } => "status".to_string(),
        }
    }

    /// Sub-commands that mutate the system and therefore need elevation (§10:
    /// crunch, guard power/autologon, sched --system, daemon task registration).
    pub fn requires_admin(&self) -> bool {
        match self {
            Command::Crunch { cmd } => matches!(cmd, CrunchCmd::On { .. } | CrunchCmd::Off),
            Command::Sched {
                cmd: SchedCmd::Add { system: true, .. },
            } => true,
            Command::Guard { cmd } => matches!(
                cmd,
                GuardCmd::Schedule { .. }
                    | GuardCmd::Wake { .. }
                    | GuardCmd::Autologon {
                        cmd: AutologonCmd::Enable { .. } | AutologonCmd::Disable
                    }
                    | GuardCmd::Watch { daemon: true, .. }
            ),
            Command::Zone { cmd } => matches!(cmd, ZoneCmd::Watch { daemon: true, .. }),
            Command::Watchdog { cmd } => matches!(cmd, WatchdogCmd::Start { daemon: true, .. }),
            _ => false,
        }
    }
}

fn zone_tail(cmd: &ZoneCmd) -> String {
    match cmd {
        ZoneCmd::Add { .. } => "add".into(),
        ZoneCmd::List => "list".into(),
        ZoneCmd::Scan { .. } => "scan".into(),
        ZoneCmd::Report { .. } => "report".into(),
        ZoneCmd::Remove { .. } => "remove".into(),
        ZoneCmd::Watch { .. } => "watch".into(),
    }
}

fn sched_tail(cmd: &SchedCmd) -> String {
    match cmd {
        SchedCmd::Add { .. } => "add".into(),
        SchedCmd::Template { .. } => "template".into(),
        SchedCmd::List => "list".into(),
        SchedCmd::Remove { .. } => "remove".into(),
        SchedCmd::Run { .. } => "run".into(),
    }
}

fn guard_tail(cmd: &GuardCmd) -> String {
    match cmd {
        GuardCmd::Status { .. } => "status".into(),
        GuardCmd::Battery { .. } => "battery".into(),
        GuardCmd::PauseOn { .. } => "pause-on".into(),
        GuardCmd::Resume => "resume".into(),
        GuardCmd::Schedule { .. } => "schedule".into(),
        GuardCmd::Wake { .. } => "wake".into(),
        GuardCmd::Watch { .. } => "watch".into(),
        GuardCmd::Autologon { cmd } => format!("autologon {}", autologon_tail(cmd)),
    }
}

fn autologon_tail(cmd: &AutologonCmd) -> String {
    match cmd {
        AutologonCmd::Enable { .. } => "enable".into(),
        AutologonCmd::Disable => "disable".into(),
        AutologonCmd::Status => "status".into(),
    }
}

fn crunch_tail(cmd: &CrunchCmd) -> String {
    match cmd {
        CrunchCmd::On { .. } => "on".into(),
        CrunchCmd::Off => "off".into(),
        CrunchCmd::Status { .. } => "status".into(),
    }
}

fn watchdog_tail(cmd: &WatchdogCmd) -> String {
    match cmd {
        WatchdogCmd::Start { .. } => "start".into(),
        WatchdogCmd::Stop => "stop".into(),
        WatchdogCmd::Status { .. } => "status".into(),
        WatchdogCmd::ImportCredits { .. } => "import-credits".into(),
        WatchdogCmd::Adapter { cmd } => format!("adapter {}", adapter_tail(cmd)),
    }
}

fn adapter_tail(cmd: &AdapterCmd) -> String {
    match cmd {
        AdapterCmd::List => "list".into(),
        AdapterCmd::Test { .. } => "test".into(),
    }
}

fn profile_tail(cmd: &ProfileCmd) -> String {
    match cmd {
        ProfileCmd::Apply { .. } => "apply".into(),
        ProfileCmd::Show { .. } => "show".into(),
    }
}

fn plugin_tail(cmd: &PluginCmd) -> String {
    match cmd {
        PluginCmd::List => "list".into(),
        PluginCmd::Install { .. } => "install".into(),
        PluginCmd::Run { .. } => "run".into(),
    }
}

/// Parse the `--json <context>` argument: either inline JSON or `@path`.
pub fn parse_context(value: Option<&str>) -> anyhow::Result<serde_json::Value> {
    let Some(raw) = value else {
        return Ok(serde_json::json!({}));
    };
    let text = match raw.strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|err| anyhow::anyhow!("读取上下文文件 {path} 失败：{err}"))?,
        None => raw.to_string(),
    };
    serde_json::from_str(&text).map_err(|err| anyhow::anyhow!("--json 上下文不是合法 JSON：{err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap_or_else(|err| panic!("{err}"))
    }

    #[test]
    fn tree_matches_doc() {
        let cli = parse(&["devzone", "zone", "report", "work", "--json", "--top", "5"]);
        assert!(matches!(
            cli.command,
            Command::Zone {
                cmd: ZoneCmd::Report { .. }
            }
        ));
        assert_eq!(cli.command.label(), "zone report");
        parse(&[
            "devzone",
            "sched",
            "add",
            "nightly",
            "--cron",
            "30 9 * * *",
            "--cmd",
            "shutdown",
            "--args",
            "/s /t 60",
            "--system",
        ]);
        parse(&["devzone", "guard", "pause-on", "doubao.exe"]);
        parse(&["devzone", "guard", "wake", "07:30"]);
        parse(&["devzone", "crunch", "on", "--days", "14"]);
        parse(&["devzone", "watchdog", "adapter", "test", "trae.exe"]);
        parse(&["devzone", "watchdog", "import-credits", "doubao", "18"]);
        parse(&["devzone", "profile", "apply", "mid"]);
        parse(&[
            "devzone",
            "plugin",
            "run",
            "power_restored",
            "--json",
            "{\"level\":22}",
        ]);
        parse(&[
            "devzone",
            "event",
            "zone_alert",
            "--json",
            "{\"zone\":\"work\"}",
        ]);
        parse(&["devzone", "doctor"]);
        parse(&["devzone", "status", "--json"]);
    }

    #[test]
    fn defaults_match_the_spec() {
        let cli = parse(&["devzone", "zone", "watch"]);
        if let Command::Zone {
            cmd: ZoneCmd::Watch { interval, daemon },
        } = &cli.command
        {
            assert_eq!(*interval, 3600);
            assert!(!daemon);
        } else {
            panic!("wrong command");
        }
    }

    #[test]
    fn unknown_command_is_usage_error() {
        let err = Cli::try_parse_from(["devzone", "nope"]).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn admin_requirements_table() {
        assert!(parse(&["devzone", "crunch", "on"]).command.requires_admin());
        assert!(!parse(&["devzone", "crunch", "status"])
            .command
            .requires_admin());
        assert!(parse(&["devzone", "guard", "schedule", "23:00"])
            .command
            .requires_admin());
        assert!(parse(&["devzone", "guard", "autologon", "enable"])
            .command
            .requires_admin());
        assert!(!parse(&["devzone", "guard", "autologon", "status"])
            .command
            .requires_admin());
        assert!(!parse(&["devzone", "sched", "remove", "x"])
            .command
            .requires_admin());
        assert!(parse(&[
            "devzone",
            "sched",
            "add",
            "x",
            "--cron",
            "* * * * *",
            "--cmd",
            "whoami",
            "--system"
        ])
        .command
        .requires_admin());
        assert!(!parse(&[
            "devzone",
            "sched",
            "add",
            "x",
            "--cron",
            "* * * * *",
            "--cmd",
            "whoami"
        ])
        .command
        .requires_admin());
        assert!(parse(&["devzone", "zone", "watch", "--daemon"])
            .command
            .requires_admin());
        assert!(!parse(&["devzone", "zone", "list"]).command.requires_admin());
    }

    #[test]
    fn wants_json_follows_the_documented_flags() {
        assert!(parse(&["devzone", "zone", "report", "w", "--json"])
            .command
            .wants_json());
        assert!(!parse(&["devzone", "zone", "list"]).command.wants_json());
        assert!(parse(&["devzone", "status", "--json"]).command.wants_json());
        assert!(parse(&["devzone", "guard", "battery", "--json"])
            .command
            .wants_json());
        assert!(
            parse(&["devzone", "watchdog", "adapter", "test", "trae.exe", "--json"])
                .command
                .wants_json()
        );
        assert_eq!(
            parse(&["devzone", "watchdog", "adapter", "test", "x"])
                .command
                .label(),
            "watchdog adapter test"
        );
    }

    #[test]
    fn context_parsing_accepts_inline_and_file() {
        assert_eq!(parse_context(None).unwrap(), serde_json::json!({}));
        assert_eq!(parse_context(Some("{\"a\":1}")).unwrap()["a"], 1);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ctx.json");
        std::fs::write(&path, "{\"zone\":\"work\"}").unwrap();
        let parsed = parse_context(Some(&format!("@{}", path.display()))).unwrap();
        assert_eq!(parsed["zone"], "work");
        assert!(parse_context(Some("not json")).is_err());
    }
}

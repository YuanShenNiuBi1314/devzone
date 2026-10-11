//! M2 定时任务中心（build guide section 6.2）：封装 `schtasks.exe`。
//!
//! 实现要点：
//! - 任务名统一 `devzone-<name>`（幂等：重复 `add` 同名任务时未 `--force` 报错并提示，
//!   `--force` 先 delete 再 create）。
//! - `--system` 加 `/ru SYSTEM /rl HIGHEST`（需管理员，由 main.rs 的 requires_admin 触发 UAC）。
//! - cron 子集严格按 §6.2 映射表（字段仅支持 `*`、数字、`*/N`，其余报「暂不支持」）。
//! - `list` 走 `schtasks /query /fo CSV /v`，GBK 解码后按 CSV 解析并只保留 `devzone-` 前缀。
//! - 所有系统改动都经 [`util::ExternalCmd`]，因此 `DEVZONE_DRY_RUN=1` 时只打印不执行
//!   （§11 的测试安全阀）；参数一律以数组传递，绝不拼 shell 字符串。

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{Local, Timelike};
use serde_json::{json, Value};

use crate::cli::SchedCmd;
use crate::util::{self, ExternalCmd};
use crate::Ctx;

/// §6.2：任务名统一前缀，避免与其它软件的计划任务冲突。
pub const TASK_PREFIX: &str = "devzone-";
/// 外部命令本体（Windows 自带）。
const SCHTASKS: &str = "schtasks.exe";

/// `devzone-` 前缀的任务条目（`sched list` 与 `devzone status` 共用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskEntry {
    /// 完整任务名，含 `devzone-` 前缀。
    pub name: String,
    pub status: String,
    pub schedule: String,
    pub next_run: String,
    pub command: String,
    pub system: bool,
}

impl TaskEntry {
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "status": self.status,
            "schedule": self.schedule,
            "next_run": self.next_run,
            "command": self.command,
            "system": self.system,
        })
    }
}

// ---------------------------------------------------------------------------
// cron 子集 -> schtasks 参数（§6.2 映射表，纯函数）
// ---------------------------------------------------------------------------

/// 单个 cron 字段允许的形状：`*`、`*/N`、数字。其它形状（列表 / 区间 / 组合）不支持。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Star,
    Every(u32),
    Val(u32),
}

/// 解析后的 5 字段 cron（分 时 日 月 周）。
#[derive(Clone, Debug, PartialEq, Eq)]
struct Cron {
    raw: String,
    minute: Field,
    hour: Field,
    dom: Field,
    month: Field,
    dow: Field,
}

fn unsupported(cron: &str, reason: &str) -> anyhow::Error {
    anyhow!(
        "暂不支持的 cron 表达式：`{cron}`（{reason}）\n\
         提示：§6.2 子集里每个字段只能是 `*`、数字或 `*/N`，例如 `*/15 * * * *`、`30 9 * * *`、`0 9 * * 1`"
    )
}

impl Cron {
    fn err(&self, reason: &str) -> anyhow::Error {
        unsupported(&self.raw, reason)
    }

    /// §6.2 映射表（逐行单元测试）。
    fn to_schtasks(&self) -> Result<Vec<String>> {
        if self.month != Field::Star {
            return Err(self.err("月份字段只支持 `*`"));
        }
        let args = match (self.minute, self.hour, self.dom, self.dow) {
            // `* * * * *` -> /sc MINUTE /mo 1
            (Field::Star, Field::Star, Field::Star, Field::Star) => minute_mo(1),
            // `*/15 * * * *` -> /sc MINUTE /mo 15
            (Field::Every(step), Field::Star, Field::Star, Field::Star) => minute_mo(step),
            // `0 * * * *` -> /sc HOURLY（起算点取当前小时，见 hourly_start）
            (Field::Val(_), Field::Star, Field::Star, Field::Star) => {
                vec!["/sc".to_string(), "HOURLY".to_string()]
            }
            // `0 */2 * * *` -> /sc HOURLY /mo 2
            (Field::Val(_), Field::Every(step), Field::Star, Field::Star) => vec![
                "/sc".to_string(),
                "HOURLY".to_string(),
                "/mo".to_string(),
                step.to_string(),
            ],
            // `30 9 * * *` -> /sc DAILY /st 09:30
            (Field::Val(minute), Field::Val(hour), Field::Star, Field::Star) => vec![
                "/sc".to_string(),
                "DAILY".to_string(),
                "/st".to_string(),
                hhmm(hour, minute),
            ],
            // `0 9 * * 1` -> /sc WEEKLY /d MON /st 09:00
            (Field::Val(minute), Field::Val(hour), Field::Star, Field::Val(day)) => {
                let name = weekday_name(day).ok_or_else(|| self.err("周字段只支持 0-7"))?;
                vec![
                    "/sc".to_string(),
                    "WEEKLY".to_string(),
                    "/d".to_string(),
                    name.to_string(),
                    "/st".to_string(),
                    hhmm(hour, minute),
                ]
            }
            // `0 9 1 * *` -> /sc MONTHLY /d 1 /st 09:00
            (Field::Val(minute), Field::Val(hour), Field::Val(day), Field::Star) => {
                if day == 0 {
                    return Err(self.err("日期字段不能是 0"));
                }
                vec![
                    "/sc".to_string(),
                    "MONTHLY".to_string(),
                    "/d".to_string(),
                    day.to_string(),
                    "/st".to_string(),
                    hhmm(hour, minute),
                ]
            }
            _ => return Err(self.err("字段组合超出支持范围（每个维度只支持单个非 `*` 字段）")),
        };
        Ok(args)
    }

    /// 「起算点取当前小时」：HOURLY 形状在注册时刻补 `/st HH:mm`，其余为空。
    fn hourly_start(&self, now_hour: u32) -> Vec<String> {
        match (self.minute, self.hour, self.dom, self.dow) {
            (Field::Val(minute), Field::Star, Field::Star, Field::Star)
            | (Field::Val(minute), Field::Every(_), Field::Star, Field::Star) => {
                vec!["/st".to_string(), hhmm(now_hour.min(23), minute)]
            }
            _ => Vec::new(),
        }
    }
}

fn minute_mo(step: u32) -> Vec<String> {
    vec![
        "/sc".to_string(),
        "MINUTE".to_string(),
        "/mo".to_string(),
        step.max(1).to_string(),
    ]
}

fn hhmm(hour: u32, minute: u32) -> String {
    format!("{hour:02}:{minute:02}")
}

fn weekday_name(day: u32) -> Option<&'static str> {
    match day {
        0 | 7 => Some("SUN"),
        1 => Some("MON"),
        2 => Some("TUE"),
        3 => Some("WED"),
        4 => Some("THU"),
        5 => Some("FRI"),
        6 => Some("SAT"),
        _ => None,
    }
}

fn parse_field(raw: &str, max: u32, label: &str, cron: &str) -> Result<Field> {
    if raw == "*" {
        return Ok(Field::Star);
    }
    if let Some(step) = raw.strip_prefix("*/") {
        let value: u32 = step
            .parse()
            .map_err(|_| unsupported(cron, &format!("{label}字段 `{raw}` 的步长不是数字")))?;
        if value == 0 || value > max {
            return Err(unsupported(
                cron,
                &format!("{label}字段 `{raw}` 的步长需落在 1..={max}"),
            ));
        }
        return Ok(Field::Every(value));
    }
    if raw.contains([',', '-', '/', 'L', 'W', '?', '#']) {
        return Err(unsupported(
            cron,
            &format!("{label}字段 `{raw}`（列表 / 区间 / 组合写法暂不支持）"),
        ));
    }
    let value: u32 = raw
        .parse()
        .map_err(|_| unsupported(cron, &format!("{label}字段 `{raw}` 不是数字、`*` 或 `*/N`")))?;
    if value > max {
        return Err(unsupported(
            cron,
            &format!("{label}字段 `{raw}` 超出 0..={max}"),
        ));
    }
    Ok(Field::Val(value))
}

fn parse_cron(cron: &str) -> Result<Cron> {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(unsupported(
            cron,
            &format!("需要 5 个字段「分 时 日 月 周」，实际 {} 个", fields.len()),
        ));
    }
    Ok(Cron {
        raw: cron.trim().to_string(),
        minute: parse_field(fields[0], 59, "分", cron)?,
        hour: parse_field(fields[1], 23, "时", cron)?,
        dom: parse_field(fields[2], 31, "日", cron)?,
        month: parse_field(fields[3], 12, "月", cron)?,
        dow: parse_field(fields[4], 7, "周", cron)?,
    })
}

/// cron 子集 -> schtasks 参数片段（纯函数，逐条 §6.2 表测试）。
pub fn cron_to_schtasks(cron: &str) -> Result<Vec<String>> {
    parse_cron(cron)?.to_schtasks()
}

/// §6.2 表里 HOURLY 行的「起算点取当前小时」：注册时刻补 `/st HH:mm`，其余形状返回空。
/// （`cron` 应先经 [`cron_to_schtasks`] 校验；此处只在已校验通过时取分钟字段）
pub fn hourly_start_args(cron: &str, now_hour: u32) -> Vec<String> {
    parse_cron(cron)
        .map(|parsed| parsed.hourly_start(now_hour))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 任务名 / 命令行引用（纯函数）
// ---------------------------------------------------------------------------

/// `devzone-<name>`；已带前缀的输入保持幂等（`sched remove devzone-foo` 也可）。
pub fn task_full_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("任务名不能为空");
    }
    if trimmed.contains(['\\', '/', ':', '"', '|', '<', '>']) {
        bail!("任务名 `{trimmed}` 含非法字符（不能包含 \\ / : \" | < >）");
    }
    let bare = trimmed.strip_prefix(TASK_PREFIX).unwrap_or(trimmed).trim();
    if bare.is_empty() {
        bail!("任务名 `{trimmed}` 只有前缀，缺少名字");
    }
    Ok(format!("{TASK_PREFIX}{bare}"))
}

/// schtasks `/tr` 的命令串：exe 路径含空格时需要内层引号
/// （外层引号交给参数数组，由 CreateProcess 还原，不需要手工拼 shell）。
pub fn quote_task_command(exe: &Path, args: &str) -> String {
    let exe_text = exe.to_string_lossy();
    let head = if exe_text.contains(' ') {
        format!("\"{exe_text}\"")
    } else {
        exe_text.to_string()
    };
    let args = args.trim();
    if args.is_empty() {
        head
    } else {
        format!("{head} {args}")
    }
}

/// `--cmd` / `--args` -> `/tr` 值。`devzone` 自身解析为当前 exe 的绝对路径
/// （计划任务的运行环境里 PATH 不一定有 devzone）。
pub fn task_command_line(cmd: &str, args: Option<&str>, current_exe: &Path) -> String {
    let trimmed = cmd.trim();
    let exe = if runs_devzone_self(trimmed) {
        current_exe.to_path_buf()
    } else {
        PathBuf::from(trimmed)
    };
    quote_task_command(&exe, args.unwrap_or(""))
}

fn runs_devzone_self(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    matches!(lower.as_str(), "devzone" | "devzone.exe")
        || Path::new(&lower)
            .file_stem()
            .map(|stem| stem == "devzone")
            .unwrap_or(false)
}

/// 组装 `schtasks /create` 参数（纯函数）。`/f` 让覆盖式创建成立。
pub fn create_task_args(
    full_name: &str,
    tr: &str,
    schedule: &[String],
    system: bool,
) -> Vec<String> {
    let mut args = vec![
        "/create".to_string(),
        "/tn".to_string(),
        full_name.to_string(),
        "/tr".to_string(),
        tr.to_string(),
    ];
    args.extend_from_slice(schedule);
    args.push("/f".to_string());
    if system {
        args.extend([
            "/ru".to_string(),
            "SYSTEM".to_string(),
            "/rl".to_string(),
            "HIGHEST".to_string(),
        ]);
    }
    args
}

/// 从 schtasks 参数里还原一句人类可读的计划（回显与 JSON 用）。
pub fn describe_schedule(args: &[String]) -> String {
    let flag = |name: &str| {
        args.iter()
            .position(|a| a.as_str() == name)
            .and_then(|idx| args.get(idx + 1))
            .map(String::as_str)
    };
    let sc = flag("/sc").unwrap_or("-");
    let mut text = match sc {
        "MINUTE" => format!("每 {} 分钟", flag("/mo").unwrap_or("1")),
        "HOURLY" => format!("每 {} 小时", flag("/mo").unwrap_or("1")),
        "DAILY" => "每天".to_string(),
        "WEEKLY" => format!("每周 {}", flag("/d").unwrap_or("-")),
        "MONTHLY" => format!("每月 {} 号", flag("/d").unwrap_or("-")),
        "ONCE" => "一次性".to_string(),
        "ONLOGON" => "登录时".to_string(),
        "ONSTART" => "开机时".to_string(),
        other => other.to_string(),
    };
    if let Some(start) = flag("/st") {
        text.push(' ');
        text.push_str(start);
    }
    text
}

// ---------------------------------------------------------------------------
// schtasks /query /fo CSV /v 解析（纯函数 + 小 CSV 解析器）
// ---------------------------------------------------------------------------

/// 单行 CSV 字段解析：双引号包裹、引号内逗号不切分、`""` 表示一个字面量引号。
pub fn parse_csv_line(line: &str) -> Vec<String> {
    if line.trim().is_empty() {
        return Vec::new();
    }
    parse_csv_rows(&format!("{line}\n"))
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// CSV -> 记录表（RFC4180 子集：引号内可含换行；`\r\n` 与 `\n` 都接受）。
pub fn parse_csv_rows(text: &str) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            ',' => row.push(std::mem::take(&mut field)),
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            other => field.push(other),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

fn norm_header(value: &str) -> String {
    value.trim().trim_start_matches('\u{feff}').to_lowercase()
}

fn find_column(headers: &[String], candidates: &[&str]) -> Option<usize> {
    headers
        .iter()
        .position(|header| candidates.contains(&norm_header(header).as_str()))
}

/// `schtasks /fo CSV /v` 的列位置。zh-CN 与 en-US 表头都认，认不出来时退回约定位置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CsvColumns {
    pub name: usize,
    pub status: Option<usize>,
    pub next_run: Option<usize>,
    pub command: Option<usize>,
    pub run_as: Option<usize>,
    pub schedule_type: Option<usize>,
    pub start_time: Option<usize>,
    pub repeat_every: Option<usize>,
}

impl CsvColumns {
    pub fn resolve(headers: &[String]) -> CsvColumns {
        CsvColumns {
            name: find_column(
                headers,
                &[
                    "taskname",
                    "task name",
                    "task",
                    "任务名",
                    "任务名称",
                    "名称",
                ],
            )
            .unwrap_or(1),
            status: find_column(headers, &["status", "状态"]),
            next_run: find_column(headers, &["next run time", "下次运行时间", "下次计划时间"]),
            command: find_column(headers, &["task to run", "要运行的任务", "运行的任务"]),
            run_as: find_column(
                headers,
                &["run as user", "以用户身份运行", "作为用户运行", "运行用户"],
            ),
            schedule_type: find_column(headers, &["schedule type", "计划类型", "计划任务类型"]),
            start_time: find_column(headers, &["start time", "开始时间"]),
            repeat_every: find_column(headers, &["repeat: every", "重复: 每隔", "重复：每隔"]),
        }
    }

    fn cell<'a>(&self, row: &'a [String], idx: Option<usize>) -> &'a str {
        let Some(index) = idx else {
            return "";
        };
        row.get(index).map(String::as_str).unwrap_or("")
    }
}

/// CSV 文本（已 GBK 解码）-> 仅 `devzone-` 前缀的任务条目。
pub fn parse_task_rows(csv: &str) -> Vec<TaskEntry> {
    // 表头永远不跨行，用单行解析器；正文可能有跨行字段（注释），用整表解析器。
    let (header_line, body) = match csv.find('\n') {
        Some(idx) => (&csv[..idx], &csv[idx..]),
        None => (csv, ""),
    };
    if header_line.trim().is_empty() {
        return Vec::new();
    }
    let columns = CsvColumns::resolve(&parse_csv_line(header_line));
    parse_csv_rows(body)
        .iter()
        .filter_map(|row| task_entry_from_row(row, &columns))
        .collect()
}

fn task_entry_from_row(row: &[String], columns: &CsvColumns) -> Option<TaskEntry> {
    let raw = columns.cell(row, Some(columns.name));
    // 任务名可能是 `\Folder\devzone-x`，取最后一段。
    let full = raw.rsplit('\\').next().unwrap_or("").trim().to_string();
    full.strip_prefix(TASK_PREFIX)?;
    let run_as = columns.cell(row, columns.run_as).to_lowercase();
    Some(TaskEntry {
        name: full,
        status: normalize_status(columns.cell(row, columns.status)),
        schedule: compose_schedule(
            columns.cell(row, columns.schedule_type),
            columns.cell(row, columns.start_time),
            columns.cell(row, columns.repeat_every),
        ),
        next_run: display_cell(columns.cell(row, columns.next_run)),
        command: columns.cell(row, columns.command).trim().to_string(),
        system: run_as.contains("system"),
    })
}

/// 空 / `N/A` / `Disabled` 之类的占位值视为无效。
fn useful(value: &str) -> bool {
    let text = value.trim();
    !text.is_empty()
        && !text.eq_ignore_ascii_case("n/a")
        && !text.eq_ignore_ascii_case("disabled")
        && !text.eq_ignore_ascii_case("none")
}

fn display_cell(value: &str) -> String {
    if useful(value) {
        value.trim().to_string()
    } else {
        "-".to_string()
    }
}

/// 状态归一化（en-US / zh-CN 都能识别），识别不了就原样小写返回。
pub fn normalize_status(raw: &str) -> String {
    match raw.trim().to_lowercase().as_str() {
        "" => "unknown".to_string(),
        "ready" | "就绪" => "ready".to_string(),
        "running" | "正在运行" | "运行中" => "running".to_string(),
        "disabled" | "已禁用" => "disabled".to_string(),
        other => other.to_string(),
    }
}

/// `09:30:00` -> `09:30`，其它保持原样。
pub fn short_time(value: &str) -> String {
    let text = value.trim();
    let bits: Vec<&str> = text.split(':').collect();
    if bits.len() == 3 && bits.iter().all(|bit| !bit.is_empty() && bit.len() <= 2) {
        return format!("{}:{}", bits[0], bits[1]);
    }
    text.to_string()
}

/// 「计划类型 + 起算时间 + 重复间隔」拼成一句可读计划。
pub fn compose_schedule(kind: &str, start_time: &str, repeat_every: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if useful(kind) {
        parts.push(kind.trim().to_string());
    }
    if useful(start_time) {
        parts.push(format!("@{}", short_time(start_time)));
    }
    if useful(repeat_every) {
        parts.push(format!("每 {}", short_time(repeat_every)));
    }
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(" ")
    }
}

// ---------------------------------------------------------------------------
// 一次性任务的 XML 定义（WakeToRun 只有走 XML 才能表达）
// ---------------------------------------------------------------------------

fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// 生成 `schtasks /create /xml` 用的任务定义（纯函数 + 单元测试）。
///
/// * `date` = `yyyy-MM-dd`，`hhmm_value` = `HH:MM`：当天一次性触发，`EndBoundary` 收在同一分钟
/// * `wake_to_run` -> `<WakeToRun>true</WakeToRun>`，从睡眠中唤醒执行（§6.3.9 `guard wake`）
/// * `system` -> `<Principals>` 以 S-1-5-18 / 最高权限运行
pub fn once_task_xml(
    exe: &Path,
    args: &str,
    date: &str,
    hhmm_value: &str,
    wake_to_run: bool,
    system: bool,
) -> String {
    let start = format!("{date}T{hhmm_value}:00");
    let end = format!("{date}T{hhmm_value}:59");
    let wake = if wake_to_run { "true" } else { "false" };
    let principal = if system {
        "      <UserId>S-1-5-18</UserId>\n      <LogonType>ServiceAccount</LogonType>\n      <RunLevel>HighestAvailable</RunLevel>"
    } else {
        "      <UserId>%USERDOMAIN%\\%USERNAME%</UserId>\n      <LogonType>InteractiveToken</LogonType>\n      <RunLevel>Limited</RunLevel>"
    };
    let command = xml_escape(&exe.to_string_lossy());
    let arguments = xml_escape(args.trim());
    let arguments_line = if arguments.is_empty() {
        String::new()
    } else {
        format!("      <Arguments>{arguments}</Arguments>\n")
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>devzone</Author>
    <Source>devzone sched</Source>
    <Description>devzone 一次性任务（唤醒={wake}）</Description>
  </RegistrationInfo>
  <Triggers>
    <TimeTrigger>
      <StartBoundary>{start}</StartBoundary>
      <EndBoundary>{end}</EndBoundary>
      <Enabled>true</Enabled>
    </TimeTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
{principal}
    </Principal>
  </Principals>
  <Settings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>{wake}</WakeToRun>
    <ExecutionTimeLimit>PT1H</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
{arguments_line}    </Exec>
  </Actions>
</Task>
"#
    )
}

// ---------------------------------------------------------------------------
// schtasks 执行层（全部走 ExternalCmd，dry-run 时只打印）
// ---------------------------------------------------------------------------

fn sc(args: &[&str]) -> Vec<String> {
    args.iter().map(|item| (*item).to_string()).collect()
}

/// 跑一条 schtasks 命令并交出原始 `Output`（存在性检查看的是退出码）。
fn spawn_schtasks(args: &[String]) -> Result<std::process::Output> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    ExternalCmd::new(SCHTASKS, &refs)
        .run()
        .with_context(|| format!("执行 {SCHTASKS} {}", args.join(" ")))
}

/// 跑一条 schtasks 命令，返回 GBK 解码后的 stdout。dry-run 时 ExternalCmd 只打印不执行。
fn run_schtasks(args: &[String]) -> Result<String> {
    let output = spawn_schtasks(args)?;
    if ExternalCmd::dry_run() {
        return Ok(String::new());
    }
    let stdout = util::decode(&output.stdout);
    if output.status.success() {
        return Ok(stdout);
    }
    let stderr = util::decode(&output.stderr).trim().to_string();
    let detail = if stderr.is_empty() {
        stdout.trim().to_string()
    } else {
        stderr
    };
    if util::contains_ci(&detail, "access is denied")
        || util::contains_ci(&detail, "mandatory privilege")
        || detail.contains("拒绝访问")
        || detail.contains("客户端没有所需的特权")
    {
        bail!(
            "需要管理员权限：schtasks {} 失败（{detail}）",
            args.join(" ")
        );
    }
    bail!(
        "schtasks {} 失败（{}）：{detail}",
        args.join(" "),
        output.status
    )
}

fn create_task(full: &str, tr: &str, schedule: &[String], system: bool) -> Result<()> {
    let args = create_task_args(full, tr, schedule, system);
    run_schtasks(&args)?;
    tracing::info!("registered scheduled task {full}");
    Ok(())
}

fn delete_task(full: &str) -> Result<()> {
    run_schtasks(&sc(&["/delete", "/tn", full, "/f"]))
        .with_context(|| format!("删除任务 {full}"))?;
    tracing::info!("deleted scheduled task {full}");
    Ok(())
}

/// 是否存在 `devzone-<name>` 任务（短名与全名都接受）。
pub fn task_exists(name: &str) -> Result<bool> {
    let full = task_full_name(name)?;
    if ExternalCmd::dry_run() {
        tracing::info!("dry-run: 跳过 `schtasks /query /tn {full}` 存在性检查");
        return Ok(false);
    }
    let output = spawn_schtasks(&sc(&["/query", "/tn", &full]))?;
    Ok(output.status.success())
}

/// 解析 `schtasks /query /fo CSV /v`（GBK 解码后）并过滤 `devzone-` 前缀。
pub fn list_devzone_tasks() -> Result<Vec<TaskEntry>> {
    let csv = run_schtasks(&sc(&["/query", "/fo", "CSV", "/v"]))?;
    Ok(parse_task_rows(&csv))
}

// ---------------------------------------------------------------------------
// 供其它模块复用（zone/guard/watchdog 的 --daemon、§6.3.8 登录自启）。
//
// 这三个注册函数只被并行开发中的 mod_zone / mod_guard / mod_watchdog 调用，
// 本模块内部不引用，故显式允许 dead_code（CI 跑 `clippy -- -D warnings`）。
// ---------------------------------------------------------------------------

/// 供 `zone watch --daemon`、`guard watch --daemon`、`watchdog start --daemon` 复用：
/// 每 N 分钟以 devzone 自身跑一次 `devzone_args`。覆盖式注册（幂等）。
#[allow(dead_code)]
pub fn register_interval_task(
    _ctx: &Ctx,
    name: &str,
    every_min: u32,
    devzone_args: &str,
    system: bool,
) -> Result<()> {
    let full = task_full_name(name)?;
    let tr = task_command_line("devzone", Some(devzone_args), &util::current_exe()?);
    let schedule = minute_mo(every_min.max(1));
    create_task(&full, &tr, &schedule, system)?;
    println!(
        "已注册后台任务 {full}：{} -> {tr}{}",
        describe_schedule(&schedule),
        if ExternalCmd::dry_run() {
            "（dry-run）"
        } else {
            ""
        }
    );
    Ok(())
}

/// `guard resume` / 常驻守护的登录时自启（§6.3.8 优先 onlogon）。
#[allow(dead_code)]
pub fn register_onlogon_task(_ctx: &Ctx, name: &str, devzone_args: &str) -> Result<()> {
    let full = task_full_name(name)?;
    let tr = task_command_line("devzone", Some(devzone_args), &util::current_exe()?);
    let schedule = sc(&["/sc", "ONLOGON"]);
    create_task(&full, &tr, &schedule, false)?;
    println!(
        "已注册登录自启任务 {full}：{tr}{}",
        if ExternalCmd::dry_run() {
            "（dry-run）"
        } else {
            ""
        }
    );
    Ok(())
}

/// 当天一次性任务（`guard schedule` / `guard wake`）。
///
/// `wake_to_run = true` 时走 XML 注册（命令行没有唤醒开关）；临时 XML 写在
/// `ctx.paths.root` 下，注册结束后删除。
#[allow(dead_code)]
pub fn register_once_task(
    ctx: &Ctx,
    name: &str,
    hhmm_value: &str,
    devzone_args: &str,
    wake_to_run: bool,
) -> Result<()> {
    let full = task_full_name(name)?;
    util::parse_hhmm(hhmm_value)
        .with_context(|| format!("非法时间 `{hhmm_value}`（应为 HH:MM）"))?;
    let exe = util::current_exe()?;
    let today = Local::now().format("%Y-%m-%d").to_string();
    let dry_suffix = if ExternalCmd::dry_run() {
        "（dry-run）"
    } else {
        ""
    };
    if wake_to_run {
        ctx.paths.ensure_dirs()?;
        let xml = once_task_xml(&exe, devzone_args, &today, hhmm_value, true, false);
        let xml_path = ctx.paths.root.join(format!("task-{full}.xml"));
        std::fs::write(&xml_path, xml.as_bytes())
            .with_context(|| format!("写入临时任务定义 {}", xml_path.display()))?;
        let registered = run_schtasks(&sc(&[
            "/create",
            "/tn",
            &full,
            "/xml",
            &xml_path.to_string_lossy(),
            "/f",
        ]));
        // 注册定义已被任务库复制走，临时文件无论如何都要清理。
        if let Err(err) = std::fs::remove_file(&xml_path) {
            tracing::warn!("删除临时任务定义 {} 失败：{err}", xml_path.display());
        }
        registered?;
        println!("已注册一次性任务 {full}：{today} {hhmm_value} 唤醒运行（WakeToRun）-> {devzone_args}{dry_suffix}");
    } else {
        let tr = quote_task_command(&exe, devzone_args);
        let schedule = sc(&["/sc", "ONCE", "/st", hhmm_value]);
        create_task(&full, &tr, &schedule, false)?;
        println!("已注册一次性任务 {full}：{today} {hhmm_value} -> {tr}{dry_suffix}");
    }
    Ok(())
}

/// 删除任务（不存在时幂等跳过）。
pub fn remove_task(_ctx: &Ctx, name: &str) -> Result<()> {
    let full = task_full_name(name)?;
    if ExternalCmd::dry_run() {
        util::progress("dry-run：跳过存在性检查，只打印删除命令");
    } else if !task_exists(&full)? {
        println!("任务 `{full}` 不存在（已跳过，操作幂等）");
        return Ok(());
    }
    delete_task(&full)?;
    println!("已删除计划任务 {full}");
    Ok(())
}

// ---------------------------------------------------------------------------
// 命令分发
// ---------------------------------------------------------------------------

pub fn handle(cmd: &SchedCmd, ctx: &Ctx) -> Result<()> {
    match cmd {
        SchedCmd::Add {
            name,
            cron,
            cmd: program,
            args,
            system,
        } => add_task(ctx, name, cron, program, args.as_deref(), *system),
        SchedCmd::Template { name, instance } => add_from_template(ctx, name, instance.as_deref()),
        SchedCmd::List => list_tasks(),
        SchedCmd::Remove { name } => remove_task(ctx, name),
        SchedCmd::Run { name } => run_task(name),
    }
}

fn add_task(
    ctx: &Ctx,
    name: &str,
    cron: &str,
    program: &str,
    args: Option<&str>,
    system: bool,
) -> Result<()> {
    let full = task_full_name(name)?;
    // 先解析 cron：参数非法就不该去动计划任务库。
    let mut schedule = cron_to_schtasks(cron)?;
    schedule.extend(hourly_start_args(cron, Local::now().hour()));
    let tr = task_command_line(program, args, &util::current_exe()?);

    if task_exists(&full)? {
        if !ctx.force {
            bail!("任务 `{full}` 已存在；如需覆盖请加 --force（会先删除再创建）");
        }
        util::progress(&format!("--force：先删除已存在的任务 {full}"));
        delete_task(&full)?;
    }
    create_task(&full, &tr, &schedule, system)?;

    let dry_suffix = if ExternalCmd::dry_run() {
        "（dry-run：未真正注册）"
    } else {
        ""
    };
    println!("已创建计划任务 {full}{dry_suffix}");
    println!(
        "  计划：{}（cron `{cron}` -> {}）",
        describe_schedule(&schedule),
        schedule.join(" ")
    );
    println!("  命令：{tr}");
    if system {
        println!("  身份：SYSTEM / 最高权限（需管理员）");
    }
    Ok(())
}

fn add_from_template(ctx: &Ctx, template: &str, instance: Option<&str>) -> Result<()> {
    let Some(tpl) = ctx.cfg.sched.templates.get(template).cloned() else {
        let mut names: Vec<String> = ctx.cfg.sched.templates.keys().cloned().collect();
        names.sort();
        let available = if names.is_empty() {
            "-".to_string()
        } else {
            names.join(", ")
        };
        bail!("未知模板 `{template}`；可用：{available}（见 [sched.templates]）");
    };
    let name = match instance {
        Some(extra) if !extra.trim().is_empty() => format!("{template}-{}", extra.trim()),
        _ => template.to_string(),
    };
    let args = if tpl.args.trim().is_empty() {
        None
    } else {
        Some(tpl.args.as_str())
    };
    println!(
        "模板 `{template}`：cron={} cmd={} args={}",
        tpl.cron, tpl.cmd, tpl.args
    );
    add_task(ctx, &name, &tpl.cron, &tpl.cmd, args, tpl.system)
}

fn list_tasks() -> Result<()> {
    util::progress("查询计划任务（schtasks /query /fo CSV /v）…");
    let tasks = list_devzone_tasks()?;
    if tasks.is_empty() {
        println!("（无 {TASK_PREFIX} 前缀的计划任务）");
        if ExternalCmd::dry_run() {
            println!("提示：dry-run 下 schtasks /query 不会真正执行。");
        }
        return Ok(());
    }
    println!(
        "{:<26} {:<8} {:<22} {:<19} {:<7} 命令",
        "任务名", "状态", "计划", "下次运行", "身份"
    );
    for entry in &tasks {
        println!(
            "{:<26} {:<8} {:<22} {:<19} {:<7} {}",
            entry.name,
            entry.status,
            entry.schedule,
            entry.next_run,
            if entry.system { "SYSTEM" } else { "用户" },
            entry.command
        );
    }
    println!("共 {} 个 devzone 任务", tasks.len());
    Ok(())
}

fn run_task(name: &str) -> Result<()> {
    let full = task_full_name(name)?;
    if !ExternalCmd::dry_run() && !task_exists(&full)? {
        bail!("任务 `{full}` 不存在；先运行 devzone sched add {name} --cron ... --cmd ... 创建");
    }
    run_schtasks(&sc(&["/run", "/tn", &full]))?;
    println!("已触发运行：{full}");
    Ok(())
}

/// 供 `devzone status` 使用：devzone 任务清单 + 模板库。
pub fn summary(ctx: &Ctx) -> Result<Value> {
    let (tasks, error) = match list_devzone_tasks() {
        Ok(entries) => (entries, String::new()),
        Err(err) => {
            tracing::warn!("sched summary 查询失败：{err}");
            (Vec::new(), err.to_string())
        }
    };
    Ok(json!({
        "prefix": TASK_PREFIX,
        "count": tasks.len(),
        "dry_run": ExternalCmd::dry_run(),
        "templates": ctx.cfg.sched.templates.keys().cloned().collect::<Vec<String>>(),
        "tasks": tasks.iter().map(TaskEntry::to_json).collect::<Vec<Value>>(),
        "error": error,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    // -- §6.2 映射表：6 行示例逐条断言 ------------------------------------

    #[test]
    fn cron_table_every_minute() {
        assert_eq!(
            cron_to_schtasks("* * * * *").unwrap(),
            strings(&["/sc", "MINUTE", "/mo", "1"])
        );
    }

    #[test]
    fn cron_table_step_minutes() {
        assert_eq!(
            cron_to_schtasks("*/15 * * * *").unwrap(),
            strings(&["/sc", "MINUTE", "/mo", "15"])
        );
    }

    #[test]
    fn cron_table_hourly() {
        assert_eq!(
            cron_to_schtasks("0 * * * *").unwrap(),
            strings(&["/sc", "HOURLY"])
        );
    }

    #[test]
    fn cron_table_daily() {
        assert_eq!(
            cron_to_schtasks("30 9 * * *").unwrap(),
            strings(&["/sc", "DAILY", "/st", "09:30"])
        );
    }

    #[test]
    fn cron_table_weekly() {
        assert_eq!(
            cron_to_schtasks("0 9 * * 1").unwrap(),
            strings(&["/sc", "WEEKLY", "/d", "MON", "/st", "09:00"])
        );
    }

    #[test]
    fn cron_table_monthly() {
        assert_eq!(
            cron_to_schtasks("0 9 1 * *").unwrap(),
            strings(&["/sc", "MONTHLY", "/d", "1", "/st", "09:00"])
        );
    }

    // -- 不支持的表达式 ---------------------------------------------------

    #[test]
    fn cron_rejects_lists_ranges_and_combinations() {
        for bad in [
            "0 9 * * 1,3",    // 周字段列表
            "*/5 9-17 * * *", // 时字段区间
            "0 9 * */2 *",    // 月字段步进
            "0 9 1 * 1",      // 日 + 周组合
            "0 9 1 1 1",      // 多字段组合
            "0 9 * * * *",    // 6 字段
            "*/0 * * * *",    // 步长 0
            "60 * * * *",     // 超出范围
            "sun 9 * * *",    // 非数字
            "",               // 空
        ] {
            let err = cron_to_schtasks(bad).unwrap_err().to_string();
            assert!(
                err.starts_with("暂不支持的 cron 表达式："),
                "`{bad}` 的错误信息不对：{err}"
            );
        }
    }

    #[test]
    fn cron_weekly_accepts_sunday_both_forms() {
        for day in ["0", "7"] {
            let cron = format!("0 9 * * {day}");
            assert_eq!(
                cron_to_schtasks(&cron).unwrap(),
                strings(&["/sc", "WEEKLY", "/d", "SUN", "/st", "09:00"]),
                "{cron}"
            );
        }
    }

    #[test]
    fn hourly_start_uses_current_hour_only_for_hourly() {
        let parsed = parse_cron("0 * * * *").unwrap();
        assert_eq!(parsed.hourly_start(9), strings(&["/st", "09:00"]));
        let half_past = parse_cron("30 * * * *").unwrap();
        assert_eq!(half_past.hourly_start(23), strings(&["/st", "23:30"]));
        let every_two = parse_cron("15 */2 * * *").unwrap();
        assert_eq!(
            every_two.to_schtasks().unwrap(),
            strings(&["/sc", "HOURLY", "/mo", "2"])
        );
        assert_eq!(every_two.hourly_start(1), strings(&["/st", "01:15"]));
        assert!(parse_cron("30 9 * * *").unwrap().hourly_start(9).is_empty());
    }

    #[test]
    fn describe_schedule_reads_back_the_args() {
        assert_eq!(
            describe_schedule(&strings(&["/sc", "MINUTE", "/mo", "15"])),
            "每 15 分钟"
        );
        assert_eq!(
            describe_schedule(&strings(&["/sc", "DAILY", "/st", "09:30"])),
            "每天 09:30"
        );
        assert_eq!(
            describe_schedule(&strings(&["/sc", "WEEKLY", "/d", "MON", "/st", "09:00"])),
            "每周 MON 09:00"
        );
        assert_eq!(describe_schedule(&strings(&["/sc", "ONLOGON"])), "登录时");
        assert_eq!(describe_schedule(&[]), "-");
    }

    // -- 任务名与命令行引用 -----------------------------------------------

    #[test]
    fn task_full_name_is_prefix_idempotent() {
        assert_eq!(task_full_name("nightly").unwrap(), "devzone-nightly");
        assert_eq!(
            task_full_name("devzone-nightly").unwrap(),
            "devzone-nightly"
        );
        assert_eq!(
            task_full_name(" zone watch ").unwrap(),
            "devzone-zone watch"
        );
        assert!(task_full_name("").is_err());
        assert!(task_full_name("devzone-").is_err());
        assert!(task_full_name(r"a\b").is_err());
        assert!(task_full_name("c:").is_err());
    }

    #[test]
    fn quote_task_command_wraps_paths_with_spaces() {
        let spaced = Path::new(r"C:\Program Files\devzone\devzone.exe");
        assert_eq!(
            quote_task_command(spaced, "zone scan all"),
            r#""C:\Program Files\devzone\devzone.exe" zone scan all"#
        );
        assert_eq!(
            quote_task_command(spaced, ""),
            r#""C:\Program Files\devzone\devzone.exe""#
        );
        let plain = Path::new(r"X").join("devzone.exe");
        assert_eq!(quote_task_command(&plain, "-a"), r"X\devzone.exe -a");
        assert_eq!(quote_task_command(&plain, ""), r"X\devzone.exe");
    }

    #[test]
    fn task_command_line_resolves_devzone_to_current_exe() {
        let exe = Path::new(r"C:\tools\dev zone\devzone.exe");
        assert_eq!(
            task_command_line("devzone", Some("event remind"), exe),
            r#""C:\tools\dev zone\devzone.exe" event remind"#
        );
        assert_eq!(
            task_command_line("DevZone.exe", Some("zone scan all"), exe),
            r#""C:\tools\dev zone\devzone.exe" zone scan all"#
        );
        // 其它程序原样保留（shutdown.exe 在 System32，计划任务可直接解析）
        assert_eq!(
            task_command_line("shutdown", Some("/s /t 60"), exe),
            "shutdown /s /t 60"
        );
        assert_eq!(task_command_line("whoami", None, exe), "whoami");
    }

    #[test]
    fn create_task_args_uses_array_and_adds_system_flags() {
        let schedule = strings(&["/sc", "MINUTE", "/mo", "1"]);
        let plain = create_task_args("devzone-x", "whoami", &schedule, false);
        assert_eq!(
            plain,
            strings(&[
                "/create",
                "/tn",
                "devzone-x",
                "/tr",
                "whoami",
                "/sc",
                "MINUTE",
                "/mo",
                "1",
                "/f"
            ])
        );
        let elevated = create_task_args("devzone-x", "whoami", &schedule, true);
        assert!(elevated.iter().any(|a| a == "SYSTEM"));
        assert!(elevated.iter().any(|a| a == "HIGHEST"));
        assert!(elevated.iter().any(|a| a == "/ru"));
        assert!(elevated.iter().any(|a| a == "/rl"));
    }

    // -- CSV 解析 ---------------------------------------------------------

    #[test]
    fn csv_line_parser_handles_quoted_commas_and_escapes() {
        let line = "\"a\",\"b,c\",\"say \"\"hi\"\"\",\"\"";
        assert_eq!(
            parse_csv_line(line),
            strings(&["a", "b,c", "say \"hi\"", ""])
        );
        assert_eq!(
            parse_csv_line("HostName,TaskName"),
            strings(&["HostName", "TaskName"])
        );
        assert!(parse_csv_line("").is_empty());
    }

    #[test]
    fn csv_parser_keeps_newlines_inside_quotes() {
        let rows = parse_csv_rows("\"x\",\"multi\nline\",\"y\"\r\n\"1\",\"2\",\"3\"\n");
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0][1], "multi\nline");
        assert_eq!(rows[1], strings(&["1", "2", "3"]));
    }

    #[test]
    fn columns_resolve_english_and_chinese_headers() {
        let en = parse_csv_line(
            r#""HostName","TaskName","Next Run Time","Status","Task To Run","Run As User","Schedule Type","Start Time","Repeat: Every""#,
        );
        let en_cols = CsvColumns::resolve(&en);
        assert_eq!(en_cols.name, 1);
        assert_eq!(en_cols.status, Some(3));
        assert_eq!(en_cols.next_run, Some(2));
        assert_eq!(en_cols.command, Some(4));
        assert_eq!(en_cols.run_as, Some(5));
        assert_eq!(en_cols.schedule_type, Some(6));
        assert_eq!(en_cols.repeat_every, Some(8));

        let zh = parse_csv_line(
            r#""主机名","任务名","下次运行时间","状态","要运行的任务","以用户身份运行","计划类型","开始时间""#,
        );
        let zh_cols = CsvColumns::resolve(&zh);
        assert_eq!(zh_cols.name, 1);
        assert_eq!(zh_cols.status, Some(3));
        assert_eq!(zh_cols.next_run, Some(2));
        assert_eq!(zh_cols.command, Some(4));
        assert_eq!(zh_cols.run_as, Some(5));
        assert_eq!(zh_cols.schedule_type, Some(6));

        // 未知表头退回约定位置，不 panic。
        let unknown = CsvColumns::resolve(&strings(&["?", "?", "?"]));
        assert_eq!(unknown.name, 1);
        assert_eq!(unknown.status, None);
    }

    #[test]
    fn parse_task_rows_keeps_only_devzone_prefix() {
        let csv = r#""HostName","TaskName","Next Run Time","Status","Task To Run","Run As User","Schedule Type","Start Time","Repeat: Every"
"PC","\devzone-nightly","2026-10-12 09:30:00","Ready","""C:\Program Files\devzone\devzone.exe"" zone scan all","zhang","Daily","09:30:00","Disabled"
"PC","\devzone-wake","N/A","Disabled","C:\Windows\System32\shutdown.exe -s -t 60","SYSTEM","One Time Only","23:00:00","Disabled"
"PC","\OtherTool","2026-10-12 01:00:00","Ready","cmd /c echo","SYSTEM","Hourly","01:00:00","Disabled"
"#;
        let tasks = parse_task_rows(csv);
        assert_eq!(tasks.len(), 2, "{tasks:?}");
        assert_eq!(tasks[0].name, "devzone-nightly");
        assert_eq!(tasks[0].status, "ready");
        assert_eq!(tasks[0].schedule, "Daily @09:30");
        assert_eq!(tasks[0].next_run, "2026-10-12 09:30:00");
        assert_eq!(
            tasks[0].command,
            r#""C:\Program Files\devzone\devzone.exe" zone scan all"#
        );
        assert!(!tasks[0].system);
        assert_eq!(tasks[1].name, "devzone-wake");
        assert_eq!(tasks[1].status, "disabled");
        assert_eq!(tasks[1].next_run, "-");
        assert!(tasks[1].system);
        assert_eq!(tasks[1].schedule, "One Time Only @23:00");
        assert_eq!(tasks[0].to_json()["name"], "devzone-nightly");
    }

    #[test]
    fn parse_task_rows_accepts_chinese_status_and_folder_names() {
        let csv = r#""主机名","任务名","下次运行时间","状态","要运行的任务","以用户身份运行","计划类型","开始时间"
"电脑","\Folder\devzone-guard","2026-10-11 20:00:00","正在运行","devzone guard watch","zhang","每小时","20:00:00"
"#;
        let tasks = parse_task_rows(csv);
        assert_eq!(tasks.len(), 1, "{tasks:?}");
        assert_eq!(tasks[0].name, "devzone-guard");
        assert_eq!(tasks[0].status, "running");
        assert_eq!(tasks[0].schedule, "每小时 @20:00");
        assert!(parse_task_rows("").is_empty());
        assert!(parse_task_rows("\"HostName\",\"TaskName\"").is_empty());
    }

    #[test]
    fn schedule_text_helpers() {
        assert_eq!(short_time("09:30:00"), "09:30");
        assert_eq!(short_time("00:15"), "00:15");
        assert_eq!(short_time("Disabled"), "Disabled");
        assert_eq!(compose_schedule("", "", ""), "-");
        assert_eq!(compose_schedule("Daily", "N/A", "Disabled"), "Daily");
        assert_eq!(
            compose_schedule("Minutes", "N/A", "00:15"),
            "Minutes 每 00:15"
        );
        assert_eq!(normalize_status("就绪"), "ready");
        assert_eq!(normalize_status("Running"), "running");
        assert_eq!(normalize_status(""), "unknown");
        assert_eq!(normalize_status("某种状态"), "某种状态");
    }

    // -- 一次性任务 XML --------------------------------------------------

    #[test]
    fn once_task_xml_carries_wake_to_run_and_hhmm() {
        let xml = once_task_xml(
            Path::new(r"C:\Program Files\devzone\devzone.exe"),
            "guard wake-run",
            "2026-10-11",
            "09:30",
            true,
            false,
        );
        assert!(xml.contains("<WakeToRun>true</WakeToRun>"), "{xml}");
        assert!(
            xml.contains("<StartBoundary>2026-10-11T09:30:00</StartBoundary>"),
            "{xml}"
        );
        assert!(
            xml.contains("<EndBoundary>2026-10-11T09:30:59</EndBoundary>"),
            "{xml}"
        );
        assert!(xml.contains("<TimeTrigger>"), "{xml}");
        assert!(
            xml.contains(r"<Command>C:\Program Files\devzone\devzone.exe</Command>"),
            "{xml}"
        );
        assert!(
            xml.contains("<Arguments>guard wake-run</Arguments>"),
            "{xml}"
        );
        assert!(
            xml.contains("<LogonType>InteractiveToken</LogonType>"),
            "{xml}"
        );
        assert!(!xml.contains("S-1-5-18"), "{xml}");
    }

    #[test]
    fn once_task_xml_system_principal_and_escaping() {
        let xml = once_task_xml(
            Path::new(r"C:\dev a\devzone.exe"),
            "event remind --json {\"a\":1} & b",
            "2026-10-11",
            "07:05",
            false,
            true,
        );
        assert!(xml.contains("<WakeToRun>false</WakeToRun>"), "{xml}");
        assert!(xml.contains("S-1-5-18"), "{xml}");
        assert!(
            xml.contains("<RunLevel>HighestAvailable</RunLevel>"),
            "{xml}"
        );
        assert!(
            xml.contains("<Arguments>event remind --json {&quot;a&quot;:1} &amp; b</Arguments>"),
            "{xml}"
        );
        assert!(xml.contains("T07:05:00"), "{xml}");

        let bare = once_task_xml(
            Path::new(r"C:\devzone.exe"),
            "",
            "2026-10-11",
            "23:59",
            true,
            false,
        );
        assert!(!bare.contains("<Arguments>"), "{bare}");
        assert!(
            bare.contains(r"<Command>C:\devzone.exe</Command>"),
            "{bare}"
        );
    }

    #[test]
    fn xml_escape_covers_the_five_entities() {
        assert_eq!(
            xml_escape(r#"<a href="x">&'o"</a>"#),
            "&lt;a href=&quot;x&quot;&gt;&amp;&apos;o&quot;&lt;/a&gt;"
        );
    }

    // -- dry-run 下的注册流程（绝不真机注册） ----------------------------

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

    #[serial]
    #[test]
    #[serial_test::serial]
    fn dry_run_add_and_remove_are_logged_not_registered() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(util::HOME_ENV, dir.path());
        std::env::set_var(util::DRY_RUN_ENV, "1");
        let ctx = test_ctx(dir.path());

        let add = SchedCmd::Add {
            name: "nightly".to_string(),
            cron: "30 9 * * *".to_string(),
            cmd: "devzone".to_string(),
            args: Some("zone scan all".to_string()),
            system: false,
        };
        handle(&add, &ctx).unwrap();

        // --force 覆盖（先 delete 再 create）在 dry-run 下同样只打印
        let mut forced = test_ctx(dir.path());
        forced.force = true;
        let add_system = SchedCmd::Add {
            name: "nightly".to_string(),
            cron: "0 9 * * 1".to_string(),
            cmd: "shutdown".to_string(),
            args: Some("/s /t 60".to_string()),
            system: true,
        };
        handle(&add_system, &forced).unwrap();

        assert!(
            list_devzone_tasks().unwrap().is_empty(),
            "dry-run 不应查到任务"
        );
        assert!(!task_exists("nightly").unwrap());
        handle(
            &SchedCmd::Remove {
                name: "nightly".to_string(),
            },
            &ctx,
        )
        .unwrap();
        handle(
            &SchedCmd::Run {
                name: "nightly".to_string(),
            },
            &ctx,
        )
        .unwrap();
        assert!(handle(&SchedCmd::List, &ctx).is_ok());

        std::env::remove_var(util::DRY_RUN_ENV);
        std::env::remove_var(util::HOME_ENV);
    }

    #[serial]
    #[test]
    #[serial_test::serial]
    fn dry_run_template_and_daemon_registrations() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(util::HOME_ENV, dir.path());
        std::env::set_var(util::DRY_RUN_ENV, "1");
        let ctx = test_ctx(dir.path());

        handle(
            &SchedCmd::Template {
                name: "reminder".to_string(),
                instance: Some("standup".to_string()),
            },
            &ctx,
        )
        .unwrap();
        let unknown = handle(
            &SchedCmd::Template {
                name: "nope".to_string(),
                instance: None,
            },
            &ctx,
        )
        .unwrap_err()
        .to_string();
        assert!(unknown.contains("未知模板"), "{unknown}");
        assert!(
            unknown.contains("shutdown"),
            "错误里应列出可用模板：{unknown}"
        );

        register_interval_task(&ctx, "zone-watch", 30, "zone scan all", false).unwrap();
        register_onlogon_task(&ctx, "resume", "guard resume").unwrap();
        register_once_task(&ctx, "wake", "07:30", "guard wake-run", true).unwrap();
        register_once_task(&ctx, "shutdown", "23:00", "guard schedule 23:00", false).unwrap();

        // 临时 XML 必须已清理
        let leftovers: Vec<String> = std::fs::read_dir(&ctx.paths.root)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".xml"))
            .collect();
        assert!(leftovers.is_empty(), "临时 XML 未删除：{leftovers:?}");

        let summary = summary(&ctx).unwrap();
        assert_eq!(summary["prefix"], TASK_PREFIX);
        assert_eq!(summary["count"], 0);
        assert_eq!(summary["dry_run"], true);
        assert_eq!(summary["error"], "");
        assert!(summary["templates"].as_array().unwrap().len() >= 3);

        // 非法时间 / 非法 cron 走明确错误
        let bad_time = register_once_task(&ctx, "wake", "25:00", "guard wake-run", false)
            .unwrap_err()
            .to_string();
        assert!(bad_time.contains("HH:MM"), "{bad_time}");
        let bad_cron = handle(
            &SchedCmd::Add {
                name: "x".to_string(),
                cron: "0 9 * * 1,3".to_string(),
                cmd: "whoami".to_string(),
                args: None,
                system: false,
            },
            &ctx,
        )
        .unwrap_err()
        .to_string();
        assert!(bad_cron.contains("暂不支持的 cron 表达式"), "{bad_cron}");

        std::env::remove_var(util::DRY_RUN_ENV);
        std::env::remove_var(util::HOME_ENV);
    }
}

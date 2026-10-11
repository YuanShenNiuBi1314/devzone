# devzone

[![CI](https://github.com/YuanShenNiuBi1314/devzone/badge.svg?branch=main)](https://github.com/YuanShenNiuBi1314/devzone/actions/workflows/ci.yml)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![platform: windows](https://img.shields.io/badge/platform-Windows--only-lightgrey)](https://github.com/YuanShenNiuBi1314/devzone)

> **The "UPS" for your unattended AI coding tasks.** When the battery is about to die, devzone gracefully pauses your AI sessions (saves first — hotkey → UI Automation → WM_CLOSE), writes a checkpoint, waits in power-saving mode, and auto-resumes when power returns. It also watches your AI agent for stalls and credit burn, and pings your phone when something needs you.

[English](#english) · [中文版](#中文版)

## English

### Why devzone

AI agents now run unattended for hours — generating, refactoring, burning credits. Three things routinely kill the night:

1. **The battery dies** and the session is cut mid-write (corrupted files, lost context, wasted credits).
2. **The agent stalls or asks a question** at 2 AM while you are out — and keeps burning credits doing nothing.
3. **Windows Update reboots** in the middle of a crunch delivery.

devzone is a local-first Rust CLI for Windows that guards all three, with **no cloud, no telemetry, no third-party manipulation** — it only reads and reminds (and, on battery emergency, asks apps to save and quit through their own graceful-close paths).

### What it does

- **Battery Guardian** — a 3-level state machine (warn 20% → graceful pause 15% → hard shutdown 5%). On the way down it runs a *graceful pause ladder* (SendInput hotkeys → UI Automation → WM_CLOSE → optional OCR fallback, default off → force-kill only as last resort, with an honest `clean_exit=false` record), writes a 3-layer checkpoint (app save → optional git commit → devzone task manifest), enters power-saving wait (eco power plan, no sleep, Lite sensing), then **auto-resumes the manifest when power returns**. It also detects a "resting charger" (AC online but the level keeps falling).
- **AI Delivery Watchdog** — stall detection via output-region pixel-hash (zero OCR by default), Full/Lite sensing tiers (Lite during power-saving: no OCR at all), credit monitoring through `official API > UIA > OCR > manual import`, and phone notifications via SMTP / Feishu / Windows Toast. Reminders tell you exactly which app to open on your phone and tap *continue*.
- **Update Crunch** — pause Windows Update for up to 35 days (`HKLM\SOFTWARE\Microsoft\WindowsUpdate\UX\Settings`), auto-resume when expired.
- **Dev Zone Watch** — register folders ("dev zones"), track size / file counts / big files / free space, alert on thresholds.
- **Scheduler** — cron-subset scheduled tasks via `schtasks`, plus a template library (shutdown / reminder / cleanup).
- **Profiles & Plugins** — one-command presets (`low` / `mid` / `pro`), plus an out-of-process plugin bus (12 events, JSON via stdin, any language). GUI-injection auto-continue is *not* built in — only official-platform channels; plugins are the extension point.

### Quick start

```powershell
cargo build --release
.\target\release\devzone.exe doctor          # 9-point environment check
.\target\release\devzone.exe profile apply mid
```

### Status

- **M1** (zone / sched / guard tier1 / crunch / profile / plugins / doctor): implemented — 270 tests green, CI on Windows.
- **M2** (watchdog): core implemented; end-to-end sensing still needs per-app verification (see roadmap below).
- **M3** (tray GUI): not implemented yet.

The full Chinese README below has the complete command table, state-machine details, disclaimers and honest implementation-diff notes.

---
## 中文版

本地优先的 Windows 开发环境自治工具：划定开发区 → 自动盯占用 → 守住电源与更新 → 盯 AI 交付会话（停顿 / 提问 / 积分预警）→ 通知到手机，人在外面也能随时接管。

- 仓库：<https://github.com/YuanShenNiuBi1314/devzone>
- 许可：MIT（[LICENSE](LICENSE)）
- 平台：仅 Windows 11（MVP 不做跨平台）
- 所有数据留在本机 `%LOCALAPPDATA%\devzone\`，无云端同步、无遥测上报

## 截图

截图放在 [`docs/screenshots/`](docs/screenshots/)。M1 阶段是 CLI 输出截图（`doctor`、`zone report`、`profile show`、`guard battery`）。

> 当前状态：该目录只有占位说明，还没有截图文件 —— 截图需要真机跑一遍才能贴，不放假图。

- 真实输出：[docs/screenshots/cli-output.md](docs/screenshots/cli-output.md)（`devzone doctor` + `devzone status`，v0.1.0，Windows 11，已脱敏）。
## 功能特性

六大模块 + 插件接口，全部本地运行。

### M1 开发区管理（`zone`）

- `zone add <路径|盘符>`：登记开发区，写入 `[zone.zones]` 并立刻扫描一次
- 递归统计总字节 / 文件数 / 目录数；符号链接不跟随；`[zone].excludes` 命中的路径段跳过
- 大文件 Top N（单次遍历维护小顶堆），盘符剩余空间用 `GetDiskFreeSpaceExW`
- 快照对比：`report` 给出与上一次快照的 Δ bytes / Δ files
- 阈值预警：剩余空间低于 `alert_free_gb` 触发 `zone_alert`（同一 zone 同一指标 6h 去抖）
- `zone watch`：前台循环扫描；`--daemon` 注册为计划任务

### M2 定时任务中心（`sched`）

- 封装 `schtasks.exe`，任务名统一加 `devzone-` 前缀，避免撞名
- cron 子集映射（`*`、数字、`*/N`）：分钟 / 小时 / 每天 / 每周 / 每月；其余表达式报「暂不支持」
- `--system` 以 `/ru SYSTEM /rl HIGHEST` 注册（需管理员）
- 内置模板库：`shutdown` / `reminder` / `cleanup`
- `sched list` 解析 `schtasks /query /fo CSV /v`，只显示 devzone 自己的任务

### M3 电源守护（`guard`）

- 三电平电量状态机（预警 / 优雅暂停 / 硬临界）+ 充电器歇息检测 + 断点三层
- 优雅暂停五级降级阶梯，省电等待，充电恢复后本地自动恢复
- `guard schedule` 定时关机、`guard wake` 唤醒定时器（不承诺关机态开机）
- `guard autologon`（默认关闭，密码明文写注册表，见免责声明）

### M4 打扰阻断（`crunch`）

- 交付模式下限时暂停 Windows 更新（写 `HKLM\SOFTWARE\Microsoft\WindowsUpdate\UX\Settings` 五个 `Pause*` 值）
- 天数上限 35（Windows 限制），`auto_expire = true` 时到期自动删除并提示
- `crunch status` 读取当前暂停状态；勿扰模式属 M3，本版本只留配置项

### M5 AI 交付守护（`watchdog`）

- 停顿检测：截取目标应用输出区 → 8×8 缩放 → dHash，连续 N 次无变化触发 `stall_detected`（默认零 OCR）
- 感知两级制：Full（精确读数，30s）/ Lite（只判「变没变」，60s，不跑 OCR 模型）；`mode = auto` 时进入省电等待自动切 Lite
- 积分水位：适配器优先级 `official_api > uia > ocr > manual`；`watchdog import-credits` 是始终可用的手动通道
- 通知：Console（必选）、SMTP、飞书 Webhook；`--features toast` 可启用 Windows Toast，失败自动降级 Console
- 平台现状（文档 §6.5.3 已核查，2026-10）：豆包无个人额度公开 API，Trae OpenAPI 仅企业版管理员 → 个人场景走 `uia / ocr / manual`

### M6 分层预设（`profile`）

- 内置三档：`low`（守护全关，只保留基础提示）/ `mid`（一键全开，核心人群）/ `pro`（逐项可配，含 git 断点与 OCR 级别）
- `profile apply` **只写 `[profile].active` 一行**：预设层在内存里生效（内置默认 → 预设 → 用户文件逐项覆盖），绝不摊平覆盖你的自定义段
- `profile show` 逐项标注来源：`default` / `preset(<active>)` / `user`，并列出被你的文件覆盖掉的预设值

### M7 插件接口（`plugin` / `event`）

- 事件表（§6.7.1 的 11 项）：`battery_low`、`battery_critical`、`graceful_pause_started`、`graceful_pause_done`、`before_shutdown`、`power_restored`、`after_resume`、`stall_detected`、`credits_low`、`zone_alert`、`remind`；实现里另加一个 `crunch_state`（暂停更新状态变化，见「实现差异说明」第 8 条）
- 插件 = 一个目录 + `plugin.toml` + 任意语言的命令；devzone 把完整 payload JSON 写进它的 stdin
- 约定：stdout 忽略；退出码 0 成功，非 0 或超时（默认 30s）记为失败并 kill，但不影响主流程
- 事件同时写入 `logs/events.jsonl`（审计），写成功才算派发完成（至少一次语义）
- 示例插件：[`docs/plugins/auto-continue-example/`](docs/plugins/auto-continue-example/) —— **只演示官方 CLI/API 通道**

## 安装与快速开始

前提：Windows 11、Rust 工具链（edition 2021）。

```powershell
git clone https://github.com/YuanShenNiuBi1314/devzone.git
cd devzone
cargo build --release
# 产物：target\release\devzone.exe（把它加进 PATH，或直接 .\target\release\devzone.exe）
```

上手三步：

```powershell
devzone doctor                 # 环境自检（9 项，逐项 [✓]/[△]/[✗]）
devzone profile apply mid      # 一键把守护全开（只改 [profile].active，不动你的自定义项）
devzone zone add D:\dev --name work   # 登记开发区并立即扫描
```

再看一眼状态：

```powershell
devzone status                 # 各模块总览
devzone profile show           # 生效配置 + 逐项来源（default / preset(mid) / user）
devzone guard battery          # 读一次电池（AC / 电量 / 是否充电）
```

想配到手机上（不配也能用，Console 通知默认开着）：

```toml
# %LOCALAPPDATA%\devzone\config\devzone.toml
[watchdog.notify.feishu]
enable = true
webhook_url = "https://open.feishu.cn/open-apis/bot/v2/hook/xxxxx"

[watchdog.notify.smtp]
enable = true
host = "smtp.qq.com"
port = 465
tls = true
username = "you@qq.com"
password = "<应用专用授权码，不要填登录密码>"
from = "you@qq.com"
to = ["you@qq.com"]
```

```powershell
devzone event remind           # 发一条测试事件，走通知链路
devzone doctor                 # 第 6 项会做 SMTP NOOP / 飞书 POST 探测
```

安全阀（建议第一次先这么跑）：

```powershell
$env:DEVZONE_HOME = "$PWD\.devzone-trial"   # 数据目录重定向，不动真实 %LOCALAPPDATA%
$env:DEVZONE_DRY_RUN = "1"                  # schtasks / powercfg / shutdown / 注册表改动只打印不执行
devzone crunch on --days 7
```

数据落在哪里：

```text
%LOCALAPPDATA%\devzone\
├── config\devzone.toml   # 唯一配置文件，首次运行自动生成（带注释的完整 schema）
├── data\devzone.db       # SQLite：zones / zone_snapshots / big_files / tasks / battery_events
├── logs\devzone.log      # >5MB 滚动，保留 3 份
├── logs\events.jsonl     # 事件审计（不含截图）
└── plugins\              # 插件目录（[plugin].dir）
```

## 命令速查表

命令树照 §5.1。「管理员」列 = 该子命令会写 HKLM / 改电源策略 / 注册系统级计划任务，非管理员时 devzone 会用 `ShellExecuteW("runas")` 带原参数 + `--elevated` 重启（§10）。

| 命令 | 作用 | 需要管理员 |
| --- | --- | --- |
| `devzone zone add <path\|drive> [--name <n>] [--alert-free <gb>]` | 登记开发区并立即扫描 | 否 |
| `devzone zone list` | 列出开发区 | 否 |
| `devzone zone scan <name\|all>` | 扫描并写快照 | 否 |
| `devzone zone report <name> [--json] [--top <n>]` | 当前指标 + 与上次快照的差值 | 否 |
| `devzone zone watch [--interval <sec>] [--daemon]` | 循环扫描预警；`--daemon` 注册计划任务 | `--daemon` 需要 |
| `devzone sched add <name> --cron <expr> --cmd <cmd> [--args <args>] [--system]` | 新建计划任务 | `--system` 需要 |
| `devzone sched list` | 列出 `devzone-` 前缀任务 | 否 |
| `devzone sched remove <name>` | 删除任务 | 否 |
| `devzone sched run <name>` | 立即运行一次 | 否 |
| `devzone guard status` | 守护状态总览（状态机 / 阈值 / tier 提示） | 否 |
| `devzone guard battery [--json]` | 读一次电池 | 否 |
| `devzone guard pause-on <app>` | 手动对某应用跑优雅暂停阶梯（测试用） | 否 |
| `devzone guard resume` | 手动触发恢复（拉起清单 + 通知） | 否 |
| `devzone guard schedule <HH:MM>` | 当天定时关机（`shutdown`） | 是 |
| `devzone guard wake <HH:MM>` | 唤醒定时器（S3 可靠；S4 待实测） | 是 |
| `devzone crunch on [--days <n>]` | 暂停 Windows 更新 N 天（上限 35） | 是 |
| `devzone crunch off` | 删除五个 `Pause*` 值，恢复更新 | 是 |
| `devzone crunch status` | 查看暂停状态；过期自动恢复 | 否 |
| `devzone watchdog start\|stop\|status` | 启停 AI 交付守护；`start --daemon` 注册计划任务 | `--daemon` 需要 |
| `devzone watchdog adapter list` | 列出感知适配器与可用级别 | 否 |
| `devzone watchdog adapter test <app>` | 实测某应用当前可用级别（只读，不真点击） | 否 |
| `devzone watchdog import-credits <platform> <value>` | 手动导入剩余额度 | 否 |
| `devzone profile apply <low\|mid\|pro>` | 套用预设（只写 `[profile].active`） | 否 |
| `devzone profile show [--json]` | 生效配置 + 逐项来源标记 | 否 |
| `devzone plugin list` | 列出已安装插件及订阅事件 | 否 |
| `devzone plugin install <dir>` | 校验 `plugin.toml` 并复制到插件目录 | 插件写 HKLM 时需要 |
| `devzone plugin run <event> [--json <context>]` | 把事件派发给订阅它的插件 | 否 |
| `devzone event <name> [--json <context>]` | 触发事件（写审计 + 通知 + 插件） | 否 |
| `devzone doctor` | 环境自检 9 项 | 否 |
| `devzone status [--json]` | 当前状态总览 | 否 |

约定：退出码 `0` 成功 / `1` 运行期与业务失败（错误写 stderr）/ `2` 用法错误；`--json` 时 stdout 只有一个包壳对象 `{ "ok": true, "command": "...", "data": {...}, "ts": "..." }`，日志一律走 stderr；修改类操作幂等，重复 `add` 同名项报错并提示 `--force`；耗时 >1s 会向 stderr 打 `[devzone] ...` 进度行。

补充命令（§5.1 树里没写、但按 §6.1 / §6.2 / §6.3.2 / §6.3.9 必须存在，见「实现差异说明」第 2 条）：

| 命令 | 作用 | 需要管理员 |
| --- | --- | --- |
| `devzone zone remove <name>` | 删除开发区登记（含快照历史） | 否 |
| `devzone sched template <name> [instance]` | 从内置模板（shutdown / reminder / cleanup）建任务 | 视模板而定（`shutdown` 模板以 SYSTEM 注册） |
| `devzone guard watch [--interval <sec>] [--once] [--daemon]` | 驱动三电平状态机；`--once` 只跑一轮冒烟 | `--daemon` 需要 |
| `devzone guard autologon enable\|disable\|status` | 无人值守自动登录（默认关闭，密码明文入注册表） | `enable` / `disable` 需要，`status` 否 |

## 电量守护说明

### 三电平阈值状态机（§6.3.2）

状态流：`NORMAL → WARN → PAUSED_WAIT →（SHUTDOWN | RESUMED）`。默认阈值可在 `[guard]` 里逐项改，`profile apply` 不会覆盖你写过的值。

| 迁移 | 条件 | 动作 |
| --- | --- | --- |
| NORMAL → WARN | `level < warn_level`（默认 20%） | 事件 `battery_low`；通知「快插电 / 保存」；任务继续跑 |
| WARN → PAUSED_WAIT | `level < pause_level`（默认 15%），**或**充电器歇息命中 | 优雅暂停 → 断点三层 → 省电等待 |
| PAUSED_WAIT → RESUMED | `level ≥ resume_level`（默认 20%） | 事件 `power_restored`；本地自动恢复 |
| PAUSED_WAIT → SHUTDOWN | `level < hard_level`（默认 5%），或 `max_wait_min`（默认 45min）到点仍 < 20% | 事件 `before_shutdown`；关机 |
| WARN → NORMAL | `level ≥ 20` 且 AC 在线充电 | 事件 `power_restored`（去抖） |
| 任意 → WARN | 充电器歇息检测命中 | 事件 `battery_low`，`reason = "charger_anomaly"`，提前进入准备 |

去抖：`battery_low` 每 60 分钟最多一次，`power_restored` 每 10 分钟最多一次；所有迁移写入 `battery_events` 表。

### 充电器歇息（§6.3.3）

轮询中若 AC 在线但电量持续下降（每次 ≥1%，累计 ≥ `charger_anomaly_min` = 10 分钟）→ 判定充电器歇息（超温 / 故障信号），提示「插着电但没在充，可能充电器过热，请检查」并提前进入 WARN 准备；恢复充电后发 `power_restored`。

### 优雅暂停五级阶梯（§6.3.6）

对 `[guard.whitelist.apps]` 里每个应用依序尝试，**每级成功即停止**，每级后等待退出确认（进程消失或断点文件出现，超时 `pause_timeout_sec` = 60s）：

| 级 | 机制 | 说明 |
| --- | --- | --- |
| ① | 快捷键 `SendInput` | 置前台 → Esc → 等 2s → Ctrl+S → 等 2s |
| ② | UIA Invoke | 按 Name 匹配「停止 / 暂停 / Stop / Save」并 Invoke |
| ③ | `WM_CLOSE` | `EnumWindows` 匹配目标进程主窗口后投递 |
| ④ | OCR + 点击 | **默认关闭**：需该应用 `enabled_levels` 含 `ocr` 且 `ocr_fallback = true`，并且编译时带 `--features ocr` |
| ⑤ | 强制结束 `TerminateProcess` | 前四级全失败时使用，`clean_exit = 0` 写入断点 |

对用户的承诺：**不干净退出就不关机、恢复时如实告知**。落到实现上就是：

- 每个应用的 `clean_exit`（`1` / `0` / `NULL`）与未干净退出清单写进 `tasks` 表的 `checkpoint_data`；
- 恢复通知（`power_restored` / `after_resume`）里明确告诉你哪些应用是被强制结束的、需要你去确认保存；
- `ShutdownBlockReasonCreate` 会在暂停期间注册阻断理由（如「devzone: 优雅暂停进行中」），暂停完成即 `ShutdownBlockReasonDestroy`。**诚实说明**：该 API 只能短暂阻止，Windows 最终仍会超时强关，devzone 不承诺它能挡住一切关机。

### 省电等待（§6.3.7）

进入 `PAUSED_WAIT` 后：

1. `powercfg /getactivescheme` 记下当前计划 GUID → `powercfg /setactive SCHEME_MIN` 切节能；
2. `powercfg /change standby-timeout-dc 0` 禁睡眠（DC），恢复时还原原值；
3. watchdog 切 Lite 感知模式，电池轮询间隔 `battery_poll_sec` = 60s；
4. 屏幕关闭：本版本只提示（`SetThreadExecutionState` 属 M3 托盘）；
5. 恢复时逆向还原电源设置（try/finally）；还原失败会记日志并把原值打出来让你手动还原。

### 断点三层（§6.3.5）与恢复（§6.3.8）

关机 / 暂停前依次做：① 让应用自己保存（由阶梯触发）；② 可选 git 自动提交（`guard.git_checkpoint = true`，无改动跳过、失败只记日志不阻塞）；③ 把白名单清单、每个应用的 `clean_exit`、时间与电量写进 `tasks` 表。恢复时 `guard resume` 读最新断点 → 按清单拉起应用 → 可选 `git pull`（`git_pull_on_resume`）→ 发 `power_restored` 通知，同时派发给插件。

自动登录（`guard autologon`）与「关机态定时开机」**默认关闭且不承诺**：前者密码明文入注册表，只建议专用无人值守账户；后者依赖主板 RTC / AC 恢复（BIOS 选项，机型相关），`doctor` 只输出提示不做检测。

## 免责声明

全文见 **[docs/disclaimers.md](docs/disclaimers.md)**。要点：

- **只读监测 + 本地系统管理**：不操控第三方应用的业务功能，不绕过登录、订阅或付费。
- **自动恢复 / 自动继续默认关闭**；自动继续**仅限官方通道**（平台自己的 CLI / API），GUI 注入式 auto-continue 不内置、示例里也没有，第三方自行实现者自担责任与免责。
- **优雅暂停的 OCR 兜底默认关闭**，仅用于「让应用保存并退出」这一单点动作，限定个人本地使用、非批量自动化。
- **autologon 与暂停更新有明确代价**：前者把密码明文写进注册表，后者让安全补丁停止下发；两者默认关闭，启用即视为已知悉。
- **插件执行的代码变更与积分消耗由用户 / 插件作者承担**；插件以自身权限跑任意脚本，devzone 只做超时与退出码记录。
- **不承诺纯软件定时开机**：依赖主板 RTC / AC 恢复；`ShutdownBlockReasonCreate` 只能短暂阻止关机。
- 软件按 MIT 原样提供，无担保；重要数据请自行备份。

## Non-goals（不做）

- OCR + 触控自动化（唯一例外：优雅暂停第 ④ 级兜底，默认关闭）
- GUI 注入式 auto-continue（不内置，留给插件）
- 跨平台（MVP 仅 Windows）
- 云端同步
- 第三方平台账号操作、绕过登录与付费
- 批量自动化任何第三方服务

## 待实测清单

devzone 里凡是依赖真机 / 主板 / 具体应用行为的项，一律实现为**可配置项 + `devzone doctor` 检测项**，不写死结论。跟踪在 GitHub Issues，模板见 [`.github/ISSUE_TEMPLATE/pending-verification.md`](.github/ISSUE_TEMPLATE/pending-verification.md)（每项附验证方法）。

1. `ShutdownBlockReasonCreate` 在目标 Windows 11 版本的行为与超时表现
2. schtasks 与注册表暂停更新的管理员权限路径
3. wake timer（S3 / S4）在当前硬件是否需要额外电源设置
4. 目标 AI 工具窗口（豆包桌面端 / Trae）输出区 / 积分区的稳定特征与区域坐标
5. BIOS「来电自启 / AC 恢复」选项（ThinkPad After Power Loss / Dell AC Recovery 等）—— 目标机型确认
6. autologon 注册表行为（路径已查证，行为需实测）
7. powercfg 电池策略别名与取值（`SetSuspendState` 与 hiberfil.sys 空间需实测）
8. 白名单应用数据目录清单（断点三层第 ③ 层落盘范围）
9. 逐应用优雅暂停适配器矩阵：豆包桌面端 / Trae 对快捷键 / UIA / WM_CLOSE 的保存与退出行为
10. 省电等待实现：节能计划切换 + 禁睡眠的无人值守行为；现代待机（S0）低电量耗电
11. Lite 感知模式：目标窗口区域坐标 / 标题 / UIA 文本在两级模式下的稳定特征
12. 暂停更新注册表：`HKLM\SOFTWARE\Microsoft\WindowsUpdate\UX\Settings` 的五个 `Pause*` 值（已查证，行为实测）

## Roadmap

| 里程碑 | 内容 | 当前状态 |
| --- | --- | --- |
| M1 | zone + sched + guard（tier1 三电平 / 优雅暂停 / 省电等待 / 恢复）+ crunch + profile + 事件总线 + plugin 骨架 + doctor / status | 代码在本仓库内；`cargo fmt` / `clippy -D warnings` / `cargo test` 为验收口径，见 CI 徽章 |
| M2 | watchdog（停顿检测 + 积分适配器 + 通知）+ plugin 命令完善 + Toast | 底层能力在仓库内：区域 dHash / 模板匹配 / UIA / 快捷键适配器（`src/adapters/`）、Full / Lite 两级制的配置面、`official_api > uia > ocr > manual` 优先级链、Console / SMTP / 飞书通道、`plugin install / list / run` 与事件派发；`watchdog start\|stop\|status` 的循环驱动以源码与 CI 为准。**未验证**：fixture 截图识别正确性与 `stall_detected` → 通知的端到端链路属【待实测】；Toast 通道需 `--features toast` 才编译 |
| M3 | 托盘 GUI + 防误关机弹窗 + 屏幕关闭 | **未实现**。`tray-icon` 依赖保留在 `tray` feature 之后但默认不启用；egui 界面、防误关机弹窗、`SetThreadExecutionState` 屏控都还没做。状态机目前由 `guard watch` 前台进程驱动，靠计划任务在登录时拉起 |

可选编译（默认全部关闭，因此 OCR / Toast / 托盘不会「悄悄被启用」）：

```powershell
cargo build --release --features ocr      # Windows OCR：优雅暂停第④级兜底 + Full 模式积分读数
cargo build --release --features toast    # Windows Toast 通知通道（失败降级 Console）
cargo build --release --features tray     # 托盘图标依赖（M3 GUI 尚未实现，只接入依赖）
```

## 实现差异说明

以下是本项目相对开发文档（build-guide）的偏离，逐条如实列出：

1. **为表达 §4.1 预设开关而新增的配置键**。§4 的 schema 只给了阈值类字段，无法表达 low 档「关」这件事，因此在 `[guard]` / `[crunch]` / `[watchdog]` / `[plugin]` 里补了：`guard.enable`、`guard.pause_enabled`、`guard.power_saving.enable`、`watchdog.enable`、`watchdog.notify.toast`、`watchdog.regions`（§6.5.1 要求输出区坐标可配）、`plugin.enable`、`crunch.enable`，以及 `crunch.dnd_enabled`（勿扰预留）。预设层通过 `config::preset_values()` 写这些键，用户文件一旦显式写过就仍然以用户值为准。另外为把 §6.3.5 / §6.3.7 / §6.5.3 / §6.7.4 落到配置上，schema 里还带了 `guard.git_checkpoint`、`guard.git_pull_on_resume`、`plugin.default_timeout_sec`、`AppPauseCfg.wait_exit_file` / `watch_region`、`CreditAdapterCfg.api_url` / `api_token_env`（端点与凭据只留配置位，devzone 绝不代填或编造平台接口）。
2. **§5.1 未列出但按其他小节必须实现的命令**：`guard watch`（§6.3.2 要求常驻驱动状态机）、`guard autologon enable|disable|status`（§6.3.9）、`zone remove`（§6.1 登记后需要能撤销）、`sched template`（§6.2 模板库需要入口）。速查表按实现现状列出，不假装 §5.1 里有它们。
3. **M3 托盘 GUI 本版本未实现**：`tray-icon` 依赖在 `tray` feature 之后保留但未启用，egui 界面 / 防误关机弹窗 / 屏幕关闭控制都未做。
4. **OCR 是可选编译**：作为 §6.3.6 第 ④ 级兜底与 Full 模式积分读数，需要 `--features ocr` 且默认关闭；未开该 feature 时 OCR 通道会如实报错，不假装可用。
5. **安全阀（文档未规定，为可测性而加）**：环境变量 `DEVZONE_DRY_RUN=1` 让所有外部系统改动（`schtasks` / `powercfg` / `shutdown` / 注册表写入）只打印不执行；`DEVZONE_HOME` 把 `%LOCALAPPDATA%\devzone` 整体重定向（集成测试与试跑用）。
6. **JSON 包壳的小扩展**：`profile show` 的 `data` 里除 `items`（`path` / `effective` / `source`）之外还带了 `active`、`active_pinned`、`config_path`、`counts`、`ignored_user_keys`；包壳本身（`ok` / `command` / `data` / `ts`）与 §5.3 一致。
7. **自动恢复的注册时机**：§6.3.8 优先用 `onlogon` 计划任务，意味着它只有在用户跑过 `guard watch --daemon` 或 `guard autologon enable` 之后才会存在；§4.1 也把「autologon / 自动恢复」在三档预设里都记为关，因此默认不会在你的登录会话里自动拉起应用。
8. **事件表多一项**：§6.7.1 列了 11 个事件，实现里 `events::name::KNOWN` 有 12 个 —— 新增的 `crunch_state` 用于报告暂停更新的状态变化（开启 / 关闭 / 到期自动恢复）。

## 隐私

- 默认**不落盘截图**：`watchdog.debug_save_screenshots = false`。
- `logs/events.jsonl` **不包含截图**，只写事件名、时间戳与 JSON payload（阈值、百分比之类）。
- 启用 OCR 时，识别结果**只保留数值 / 状态**（例如剩余额度百分比），不保存屏幕文本原文。
- 一切数据留在本机 `%LOCALAPPDATA%\devzone\`（或 `DEVZONE_HOME` 指定的目录），无云端同步、无遥测；唯一出站的是你自己配置的通知通道（SMTP / 飞书 Webhook）。
- 提醒：SMTP 密码与飞书 webhook URL 以明文存在 `devzone.toml`，请用应用专用授权码，并自行限制该文件的访问权限。

## License

MIT，见 [LICENSE](LICENSE)。

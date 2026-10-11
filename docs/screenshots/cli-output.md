# devzone CLI 输出实拍（v0.1.0，Windows 11）

M1/M2 阶段没有图形界面，截图位放真实控制台输出。下面两份来自本机
`target\release\devzone.exe`，用户名与临时目录已脱敏；`DEVZONE_DRY_RUN=1`
表示所有外部系统改动（计划任务 / 电源方案 / 关机 / 注册表）只打印不执行。

## `devzone doctor`（§5.4 环境自检 9 项）

```text
[devzone] dry-run: powercfg.exe /a
devzone 0.1.0 doctor
数据目录：%TEMP%/dz-shot
[△] 休眠可用性 (S4) — 休眠不可用（powercfg /a 未列出休眠）。tier1 不依赖休眠；tier2 若要睡眠需 S3=false
[△] hiberfil.sys 空间 — 休眠文件不存在：休眠未启用（powercfg /h on 可开启，会占用系统盘空间）。系统盘剩余 99.0 GiB，足以容纳休眠（约需 25.5 GiB）
[✓] 电池 API — AC=Some(true) 电量=Some(100)% 充电中=Some(false) flag=Some(1)
[△] 管理员权限 — 未提权：crunch on/off、guard schedule/wake/autologon、sched --system、--daemon 会在执行时请求 UAC
[✓] 适配器 doubao.exe — 配置=hotkey/uia/wm_close；本次实测可用=hotkey/wm_close；进程状态=running pid=2932 windows=0,pid=3992 windows=0,pid=4348 windows=0,pid=9860 windows=11,pid=11756 windows=0,pid=13668 windows=0,pid=14916 windows=0,pid=16288 windows=0,pid=16492 windows=0,pid=16792 windows=0,pid=17392 windows=0,pid=17908 windows=0,pid=18140 windows=5,pid=18324 windows=2,pid=19468 windows=0,pid=21972 windows=0,pid=24072 windows=0
[△] 适配器 trae.exe — 配置=hotkey/uia/wm_close；本次实测可用=-；进程状态=trae.exe 未运行
[✓] 通知通道 console — stderr 输出可用（不写盘、不外发）
[✓] UIA 可用性 — CoInitializeEx + CUIAutomation + GetRootElement 成功（根元素 Name="桌面 1"）；目标窗口的元素匹配仍需逐应用实测（§16 待实测项 9）
[✓] 配置 devzone.toml — 解析成功：profile=mid zones=0 whitelist_apps=2 regions=0 plugins_dir=%TEMP%/dz-shot\plugins
[△] 定时开机 (BIOS) — 定时开机需主板 RTC 唤醒 / AC 恢复，请到 BIOS 确认（机型相关：ThinkPad After Power Loss / Dell AC Recovery 等）。devzone 不承诺纯软件开机
[i] 数据库统计：{"battery_events":0,"big_files":0,"tasks":0,"zone_snapshots":0,"zones":0}
[i] 结论：5 项需要关注，其中 0 项失败
```

## `devzone status`（当前状态总览）

```text
[devzone] dry-run: schtasks.exe /query /fo CSV /v
devzone 0.1.0 — profile=mid admin=false dry_run=true
数据目录：%TEMP%/dz-shot

[battery]
  ac: true
  charging: false
  flag: 1
  level_pct: 100
  life_min: -

[crunch]
  auto_expire: true
  auto_recovered: false
  days_left: 0
  default_pause_days: 14
  dry_run: true
  expired: false
  expiry: -
  max_pause_days: 35
  message: 未暂停（HKLM 更新暂停键值不存在，Windows Update 正常）
  note: dry-run：读取的是进程内沙箱，未访问 HKLM
  pause_values: (空)
  pause_window: -
  paused: false
  registry: SOFTWARE\Microsoft\WindowsUpdate\UX\Settings

[events]
  (空)

[guard]
  autologon: {hint=自动登录密码在注册表里是明文（§6.3.9）：仅建议专用无人值守账户，README 已披露；查看状态不会回显密码，取消请用 devzone guard autologon disable registry={AutoAdminLogon= DefaultDomainName= DefaultPassword_present=- DefaultUserName= errors=(空)}}
  battery: {ac=true battery_flag=1 charging=false level_pct=100 life_min=- unknown=false}
  charger_anomaly: false
  checkpoint: -
  enable: true
  git: {checkpoint=false pull_on_resume=false}
  history: (空)
  inferred_from_single_reading: true
  note: CLI 单次调用用一次读数推断状态；真状态由常驻进程 `devzone guard watch` 维护（§6.3.2）
  pause_enabled: true
  power_saving: {disable_sleep=true enable=true power_plan_switch=true screen_off=false screen_off_note=enable_screen_off=false 时只打印提示：M1/M2 不控制屏幕（§6.3.7 第 4 项）}
  state: Normal
  strategy: tier1
  thresholds: {battery_poll_sec=60 hard_level=5 max_wait_min=45 pause_level=15 pause_timeout_sec=60 resume_level=20 warn_level=20}
  tier_hint: tier1：本机优雅暂停 + 省电等待 + 充电后本地恢复（strategy=tier1）
  whitelist: [app=doubao.exe configured_levels=[hotkey | uia | wm_close] ladder=[hotkey | uia | wm_close] ocr_fallback=false wait_exit_file=- | app=trae.exe configured_levels=[hotkey | uia | wm_close] ladder=[hotkey | uia | wm_close] ocr_fallback=false wait_exit_file=-]

[notify]
  [channel=console enabled=true | channel=smtp enabled=false host= port=465 recipients=0 | channel=feishu enabled=false webhook_set=false | built=false channel=toast enabled=false]

[plugins]
  dir: %TEMP%/dz-shot\plugins
  enabled: true
  installed: 0

[sched]
  count: 0
  dry_run: true
  error: 
  prefix: devzone-
  tasks: (空)
  templates: [cleanup | reminder | shutdown]

[watchdog]
  active_stalls: (空)
  configured_mode: auto
  credits: (空)
  daemon_task: devzone-watchdog-start
  debug_save_screenshots: false
  enabled: true
  features: {ocr=false toast=未编译（需 --features toast）}
  manual: (空)
  mode: full
  mode_reason: AC 在线、电量 100% ≥ pause_level 15% → Full
  poll_sec: 30
  regions_configured: 0
  regions_missing: [doubao.exe | trae.exe]
  stall_stable_polls: 5
  state_file: %TEMP%/dz-shot\data\watchdog_state.json
  targets: [app=doubao.exe from=whitelist region_configured=false signal= stable_polls=0 stalled=false | app=trae.exe from=whitelist region_configured=false signal= stable_polls=0 stalled=false]
  updated:

[zone]
  alerts: (空)
  default_alert_free_gb: 10.0
  excludes: [node_modules | target | .git | dist | .cache]
  orphan_db_rows: (空)
  registered: 0
  top_n: 20
  zones: (空)
```

说明：`[△]` 是需要注意但不阻塞（未提权、休眠未启用、无电池机型等），`[✗]` 才是失败；
`doctor` 的结论行统计两者。所有数值都是当次实测，文档标注【待实测】的项目不会写成“已验证”。

---
name: 待实测核查清单
about: 逐项核对开发文档 §16 的 12 个待实测项（只填真机观察到的结果，不编造结论）
title: "[待实测] "
labels: ["verification-needed", "help wanted"]
---

<!--
本模板对应开发文档 §16「待实测核查清单」。

规则：
- 只填你在自己机器上真实观察到的结果；没跑过的项请留空并写「未实测」。
- 文档里标【待实测】【待验证】的能力，代码一律实现为可配置项 + `devzone doctor`
  检测项，不允许在 README、注释或测试里伪造实测结论。
- 试跑会改系统状态的命令前，先设安全阀：
  `$env:DEVZONE_DRY_RUN = "1"`（schtasks / powercfg / shutdown / 注册表改动只打印不执行）
  `$env:DEVZONE_HOME = "D:\devzone-trial"`（数据目录重定向，不动真实 %LOCALAPPDATA%）
  真正验证效果时再取消这两个变量。
-->

## 环境

- 机型 / 主板：
- Windows 版本（`winver`）：
- 睡眠状态可用性（`powercfg /a` 摘要）：
- devzone 版本（`devzone --version`）：
- 是否以管理员运行（`devzone doctor` 第 4 项）：
- 电池 / AC 情况：

## 待实测项

- [ ] 1. `ShutdownBlockReasonCreate` 在目标 Windows 11 版本的行为与超时表现

  验证方法（§6.3.4）：`devzone guard pause-on <app>` 注册阻断理由，另一窗口执行
  `shutdown /s /t 120`，记录实际被阻止了多久、最终是否被系统强关。

- [ ] 2. schtasks 与注册表暂停更新的管理员权限路径

  验证方法（§5.4 第 4 项 / §10）：非管理员执行
  `devzone sched add probe --cron "* * * * *" --cmd whoami --system` 与
  `devzone crunch on --days 7`，确认 UAC 提权链带 `--elevated` 且不二次提权；
  在 UAC 里点"取消"时应报「需要管理员权限」并以退出码 1 结束。

- [ ] 3. wake timer（S3 / S4）在当前硬件是否需要额外电源设置

  验证方法（§6.3.9）：`devzone guard wake <HH:MM>` 后分别做 S3 睡眠与休眠，
  观察是否按时唤醒；S4 需先确认 `powercfg /a` 报告休眠可用。

- [ ] 4. 目标 AI 工具窗口（豆包桌面端 / Trae）输出区 / 积分区的稳定特征与区域坐标

  验证方法（§6.5.1 / §6.5.2）：`devzone watchdog adapter test <app>` 引导框选或手填，
  把得到的坐标写进 `[watchdog.regions]`，多次开窗 / 换分辨率后确认仍稳定。

- [ ] 5. BIOS「来电自启 / AC 恢复」选项（ThinkPad After Power Loss / Dell AC Recovery 等）

  验证方法（§6.3.9）：进 BIOS 确认该选项是否存在、当前取值；断电再上电，
  观察机器是否自行开机。devzone 不承诺纯软件定时开机，此项只记录机型事实。

- [ ] 6. autologon 注册表行为（路径已查证，行为需实测）

  验证方法（§6.3.9）：管理员 `devzone guard autologon enable` → 重启看是否自动登录；
  `devzone guard autologon status` 确认写入（不回显明文）；
  `devzone guard autologon disable` 确认 `DefaultPassword` 被删除。
  风险见 `docs/disclaimers.md` 第 4 节，建议在专用账户上测。

- [ ] 7. powercfg 电池策略别名与取值（BATACTIONCRIT / BATLEVELCRIT / BATLEVELLOW 已查证）

  验证方法（§6.3.7 / §5.4 第 1–2 项）：`devzone guard watch` 进入省电等待，
  看日志里 `powercfg /setactive SCHEME_MIN` 与 `standby-timeout-dc 0` 是否生效、
  恢复时是否逆向还原；顺带记录 `SetSuspendState` 行为与 hiberfil.sys 占用。

- [ ] 8. 白名单应用数据目录清单（断点三层第 ③ 层落盘范围）

  验证方法（§6.3.5）：对 `[guard.whitelist.apps]` 里每个应用执行
  `devzone guard pause-on <app>`，确认其会话是否自保存，
  并把需要额外落盘的目录补进 devzone checkpoint。

- [ ] 9. 逐应用优雅暂停适配器矩阵：豆包桌面端 / Trae 对快捷键 / UIA / WM_CLOSE 的保存与退出行为

  验证方法（§6.3.6 / §5.4 第 5 项）：先 `devzone doctor`（第 5 项实测可用级别，只提示不写死），
  再逐应用 `devzone guard pause-on <app>`，记录每级是否成功、是否真的保存。

- [ ] 10. 省电等待实现：节能计划切换 + 禁睡眠的无人值守行为；现代待机（S0）低电量耗电

  验证方法（§6.3.7 / §6.3.2）：`devzone profile apply mid` 后 `devzone guard watch`，
  拔电观察 `PAUSED_WAIT` 期间掉电速率、是否睡死、到 `max_wait_min` 是否按预期关机，
  插电后确认 `power_restored` 与电源计划还原。

- [ ] 11. Lite 感知模式：目标窗口区域坐标 / 标题 / UIA 文本在两级模式下的稳定特征

  验证方法（§6.5.2）：`devzone watchdog start --mode full` 与 `--mode lite` 各跑一段，
  对比 `stall_detected` 命中情况与轮询间隔（`poll_sec_full` / `poll_sec_lite`）。

- [ ] 12. 暂停更新注册表：`HKLM\SOFTWARE\Microsoft\WindowsUpdate\UX\Settings` 的五个 `Pause*` 值

  验证方法（§6.4）：`devzone crunch on --days 14` 后用
  `reg query "HKLM\SOFTWARE\Microsoft\WindowsUpdate\UX\Settings" /v PauseUpdatesExpiryTime`
  等方式核对 5 个值的格式（`yyyy-MM-ddTHH:mm:ssZ`），再到 Windows 设置 → Windows 更新 →
  高级选项确认界面一致；`devzone crunch off` 后确认 5 个值已删除。

## 结论

- 与文档描述不一致的点：
- 需要回写文档 / 代码的改动：
- 附：`devzone doctor` 输出与相关日志片段（`%LOCALAPPDATA%\devzone\logs\`）：

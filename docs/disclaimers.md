# 免责声明（全文）

本文是 devzone 免责声明的完整文本，README 的「免责声明」一节引用这里的要点。
适用范围：devzone v0.1.0（M1/M2 范围）。

## 1. 只读监测与本地系统管理

devzone 的全部能力限定在两件事：

1. **只读监测**：磁盘占用、进程与窗口是否存在、电池状态、窗口区域哈希、可选的 UIA 文本读取。
2. **本地系统管理**：写入自己的数据目录、注册计划任务、修改电源策略、读写 Windows 更新暂停相关的注册表项。

devzone **不操控第三方应用的业务功能，不绕过任何登录、订阅或付费流程**，不抓取账号凭据，
不做云端同步。所谓"接管"仅指：把事件与阈值通知到你自己的手机，由你本人在官方客户端里点继续。

## 2. 优雅暂停五级阶梯与 OCR 兜底

优雅暂停（§6.3.6）按快捷键 → UIA Invoke → WM_CLOSE → OCR + 点击 → 强制结束的次序逐降级执行。

- 第 ④ 级 **OCR + 点击默认关闭**：仅当某应用在 `[guard.whitelist.apps]` 中显式声明
  `enabled_levels` 含 `ocr` **且** `ocr_fallback = true` 时才启用，并且需要
  `cargo build --features ocr` 才会被编译进来。
- 该兜底**只用于"让应用保存并退出"这一个单点动作**，限定个人本地使用，
  不用于批量自动化任何第三方服务。
- 第 ⑤ 级 `TerminateProcess` 属于强制结束，可能造成应用侧未保存内容丢失；
  发生时会把 `clean_exit = 0` 写入断点，并在恢复通知中如实告知。
- 产品承诺：**不干净退出就不关机、恢复时如实告知**。devzone 承诺把未干净退出的应用
  写进断点清单、在恢复时明确报告；但 Windows 自身可能在 devzone 之外强制关机，
  这部分不受 devzone 控制。

## 3. 自动恢复 / 自动继续默认关闭

- 本地自动恢复（§6.3.8：登录时按断点清单拉起白名单应用）**默认不生效**：它需要一个
  `onlogon` 计划任务或 `shell:startup` 快捷方式，只有你自己执行过
  `devzone guard watch --daemon` 或 `devzone guard autologon enable` 之后才会注册上；
  §4.1 预设表里 low / mid / pro 三档都把"autologon / 自动恢复"记为关。
  未注册时 devzone 不会在你的登录会话里自动做任何事。
- **自动继续（auto-continue）不内置**。devzone 只在插件通道里提供事件派发；
  示例插件 `docs/plugins/auto-continue-example/` 演示的是"若平台官方 CLI/API 已配置才调用"，
  找不到官方通道就打印提示并正常退出。
- **GUI 注入式 auto-continue（模拟点击"继续"按钮）不在示例内、也不被 devzone 支持**，
  第三方若自行实现，责任与免责由实现方自行承担。

## 4. autologon 的安全代价

`guard autologon enable` 写入
`HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon` 的
`AutoAdminLogon` / `DefaultUserName` / `DefaultDomainName` / `DefaultPassword`。

- **`DefaultPassword` 在注册表中是明文存储的**，任何具备管理员权限的进程都能读到。
- 仅建议用于**专用无人值守账户**，不要用在日常办公或存有敏感数据、登录了银行或公司系统的账户上。
- 该路径已查证（Microsoft Learn「配置 Windows 以自动登录」），但在具体机器上的
  行为（组策略覆盖、BitLocker / Windows Hello 影响）**仍需实测**，devzone 不承诺生效。
- 用 `guard autologon disable` 会删除 `DefaultPassword` 并关闭 `AutoAdminLogon`。

## 5. 暂停 Windows 更新的系统代价

`crunch on` 写
`HKLM\SOFTWARE\Microsoft\WindowsUpdate\UX\Settings` 的五个 `Pause*` 值（已查证来源：
Microsoft Learn `waas-configure-wufb`），上限 35 天。

- 暂停期内**安全补丁也不会自动下发**，机器暴露在已知漏洞的时间被延长；请自行评估。
- 该键值属于系统更新策略，写坏可能影响 Windows Update 界面；devzone 只按文档格式写入并
  在 `crunch off` 时删除，仍建议在正式使用前实测一轮（见 GitHub Issue 待实测清单）。
- 到期自动恢复依赖 `auto_expire = true` 与 devzone 被再次调用（例如计划任务或
  `crunch status`）；若设备长期不运行 devzone，暂停只会随 Windows 自身上限过期。

## 6. 插件与动作原语

- 插件是**进程外脚本**：`devzone event <name>` 会把完整 payload 以 JSON 写入插件 stdin，
  插件能以自身权限执行任意命令。
- **插件造成的代码变更、文件改动、外部服务调用与积分消耗，由用户与插件作者承担**，
  不由 devzone 承担。
- 只安装你读过源码的插件。devzone 不对插件做签名校验或沙箱限制，只有超时（默认 30 秒）
  与非 0 退出码会被记录为失败，且不影响主流程。

## 7. 定时开机与唤醒

- **devzone 不承诺纯软件定时开机**。关机态开机依赖主板 RTC / "来电自启 / AC 恢复"
  等厂商选项（ThinkPad After Power Loss、Dell AC Recovery 等），`guard wake` 只管理
  Windows 唤醒定时器：S3（睡眠）可靠，S4 / 关机态是否生效**取决于机型，待实测**。
- `ShutdownBlockReasonCreate`（用于长任务防误关机）**只能短暂阻止**关机；
  Windows 最终仍会超时强制关机。防误关机弹窗属于 M3 托盘范围，本版本未实现。

## 8. 隐私

- 默认**不落盘截图**（`watchdog.debug_save_screenshots = false`）。
- `logs/events.jsonl` 审计日志**不包含截图**。
- 启用 OCR 时，识别结果只保留数值 / 状态（例如剩余额度百分比），不保存屏幕文本原文。

## 9. 无担保

devzone 以 MIT 许可证提供，**不含任何形式的担保**。它可能误判电量、误暂停或误结束你的进程，
可能因 Windows 更新、驱动、厂商电源策略而行为改变。重要数据请先自行备份；
在无人值守场景启用前，请至少在当前机器上手动跑通一轮
`devzone doctor` 与 `devzone guard battery`。

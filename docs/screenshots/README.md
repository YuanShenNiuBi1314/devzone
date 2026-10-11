# 截图占位

这里放 devzone 的 CLI 输出截图，README「截图」一节引用本目录。

约定：

- M1（v0.1.0）先放纯 CLI 文本截图，PNG，文件名小写中划线：
  - `doctor.png` —— `devzone doctor` 的 9 项自检输出
  - `profile-apply-mid.png` —— `devzone profile apply mid`（预设项 / 用户覆盖项 / 生效值变化三段）
  - `profile-show.png` —— `devzone profile show` 的「配置项 / 生效值 / 来源」表
  - `zone-report.png` —— `devzone zone report work --top 10`
  - `guard-battery.png` —— `devzone guard battery`
  - `crunch-status.png` —— `devzone crunch status`
- M2 补 `watchdog-status.png`（停顿检测 / 积分水位）与通知到达手机的截图。
- M3 托盘 GUI 截图（`tray.png`）——该里程碑本版本**未实现**，因此暂无截图。
- 临时草稿图用 `*.tmp` 命名，已在仓库根 `.gitignore` 中忽略；正式截图以 PNG 提交。
- 截图前请遮挡个人信息：用户名、邮箱、SMTP 密码、飞书 Webhook URL、AI 会话正文。
  devzone 本身默认不落盘任何截图（`watchdog.debug_save_screenshots = false`）。

本目录当前**没有真实截图**：README 里的截图位是占位说明，不是已完成的实测证据。

本目录已包含 [cli-output.md](cli-output.md)：v0.1.0 在本机实跑的 `devzone doctor` 与 `devzone status` 控制台输出（已脱敏）。

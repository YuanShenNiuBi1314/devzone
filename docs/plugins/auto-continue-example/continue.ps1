# ============================================================================
# devzone 示例插件：auto-continue-example / continue.ps1
#
# 订阅事件（见同目录 plugin.toml）：
#   power_restored —— 充电恢复到 resume_level 以上
#   stall_detected —— AI 交付会话疑似停住
#
# 调用协议（build guide §6.7.4）：
#   devzone 把完整上下文以 JSON 写入本脚本 stdin：
#       { "event": "...", "ts": "...", "payload": { ... } }
#   stdout 被 devzone 忽略；退出码 0 = 成功，非 0 = 失败（记日志）；
#   plugin.toml 的 timeout_sec = 30，超时会被 kill 并记为失败；
#   工作目录 = 插件目录。
#
# 免责与边界（必读，另见 docs/disclaimers.md 与 README「免责声明」一节）：
#   1. 本示例**只演示官方通道**：只有当平台自己的官方 CLI / API 在本机确实已配置时
#      才发起调用；找不到就打印提示并退出 0。devzone 不内置任何平台账号操作，
#      不绕过登录、订阅与付费。
#   2. **GUI 注入式 auto-continue（模拟点击 / 快捷键去按「继续」按钮）不在本示例内，
#      devzone 也不内置**；第三方若自行实现，相应风险、责任与免责由实现方自行承担。
#   3. 自动继续默认关闭：必须显式设置环境变量 DEVZONE_AUTO_CONTINUE=1 才会真的调用，
#      否则本脚本只打印提示并退出 0（兑现「默认关」的产品承诺）。
#   4. 插件造成的代码变更、会话状态变化与积分消耗，由用户与插件作者承担。
#
# 运行环境：Windows PowerShell 5.1（devzone 以
#   powershell.exe -NoProfile -ExecutionPolicy Bypass -File continue.ps1
#   的形态调用本脚本）。为兼容 5.1：语句一律单行书写，文件以带 BOM 的 UTF-8 保存。
# ============================================================================

$ErrorActionPreference = 'Stop'

# devzone 传入的是 UTF-8 JSON；PowerShell 5.1 的控制台编码可能不是 UTF-8，
# 这里尽力切换一次，失败（宿主无控制台 / 已重定向）就忽略，ASCII 字段不受影响。
try {
    [Console]::InputEncoding = [System.Text.Encoding]::UTF8
    [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
} catch {
    # 忽略：编码切换失败不影响本示例的判断逻辑
}

# --- 1. 读取 stdin 的 JSON 上下文 -----------------------------------------

$raw = [Console]::In.ReadToEnd()

if ([string]::IsNullOrWhiteSpace($raw)) {
    Write-Output '[auto-continue-example] stdin 为空，没有可处理的上下文。'
    exit 0
}

$context = $null

try {
    $context = $raw | ConvertFrom-Json
} catch {
    Write-Output '[auto-continue-example] stdin 不是合法 JSON，跳过本次事件。'
    exit 0
}

$eventName = [string]$context.event
$payload = $context.payload

# 推断目标平台：payload.platform 优先，其次把 payload.app（形如 "trae.exe"）去扩展名。
$platform = ''

if ($payload -and $payload.platform) {
    $platform = [string]$payload.platform
} elseif ($payload -and $payload.app) {
    $app = [string]$payload.app
    $platform = $app -replace '\.exe$', ''
}

if ([string]::IsNullOrWhiteSpace($platform)) {
    Write-Output ('[auto-continue-example] 事件 ' + $eventName + ' 未携带平台线索，只记录不动作。')
    exit 0
}

# --- 2. 官方通道：只有真的配置了才执行 ------------------------------------

# 各平台官方 CLI 的候选命令名：需要用户自行安装并登录；
# devzone 不内置、不猜测任何平台的接口，也不代管账号凭据。
$officialCliNames = @{}
$officialCliNames['doubao'] = @('doubao', 'doubao-cli')
$officialCliNames['trae'] = @('trae', 'trae-cli')

$resolved = $null
$preferred = $env:DEVZONE_OFFICIAL_CLI

if ($preferred) {
    $found = Get-Command $preferred -ErrorAction SilentlyContinue

    if ($found) {
        $resolved = [string]$found.Source
    }
}

if (-not $resolved) {
    $candidates = $officialCliNames[$platform.ToLowerInvariant()]

    if ($candidates) {
        foreach ($candidate in $candidates) {
            $found = Get-Command $candidate -ErrorAction SilentlyContinue

            if ($found) {
                $resolved = [string]$found.Source
                break
            }
        }
    }
}

if (-not $resolved) {
    Write-Output ('[auto-continue-example] 未检测到 ' + $platform + ' 的官方 CLI / API（可自行安装官方 CLI，或把 DEVZONE_OFFICIAL_CLI 指向它）。本示例不做任何 GUI 注入式自动继续；事件已在 devzone 的审计日志里。')
    exit 0
}

if ($env:DEVZONE_AUTO_CONTINUE -ne '1') {
    Write-Output ('[auto-continue-example] 已找到官方通道 ' + $resolved + '，但自动继续默认关闭：确认无误后设置 DEVZONE_AUTO_CONTINUE=1 才会真正调用。')
    exit 0
}

# --- 3. 调用官方通道（超时由 plugin.toml 的 timeout_sec 兜住）-------------

# 子命令与参数请按你所用平台的官方文档改写；下面只是示例形式。
$arguments = @('session', 'continue', '--source', 'devzone', '--event', $eventName)

try {
    & $resolved @arguments
    $code = $LASTEXITCODE

    if ($code -eq 0) {
        Write-Output ('[auto-continue-example] 官方通道已受理 ' + $platform + ' 的 continue 请求。')
        exit 0
    }

    Write-Output ('[auto-continue-example] 官方 CLI 返回退出码 ' + $code + '，devzone 会把本次派发记为失败。')
    exit 1
} catch {
    Write-Output '[auto-continue-example] 调用官方 CLI 失败，devzone 会把本次派发记为失败。'
    exit 1
}

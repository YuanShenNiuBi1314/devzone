//! Windows toast notifications (`--features toast`, section 6.5.4 ToastNotify).
//!
//! 本模块整体由 `adapters::mod` 的 `#[cfg(feature = "toast")]` 包住：默认构建完全不编译，
//! `notify::ToastNotify` 在没开特性时返回「需要 --features toast」并降级 Console（§9）。
//!
//! 实现走未打包（unpackaged）桌面应用的标准路径：
//! `Windows.Data.Xml.Dom.XmlDocument` 承载 `<toast><visual><binding template="ToastGeneric">…`
//! → `ToastNotification::CreateToastNotification` → `ToastNotificationManager::
//! CreateToastNotifierWithId("devzone")` → `Show`。
//!
//! **已知边界（如实记录）**：未打包进程要用 `CreateToastNotifierWithId`，Windows 需要
//! 该 AUMID 在 `HKCU\Software\Classes\AppId\devzone`（或 Start Menu 快捷方式）里注册过，
//! 否则 `Show` 返回 `0x800704CF`（“找不到应用程序”）。devzone **不自动写注册表**：
//! [`probe`] 会把当前状态如实报给 `watchdog status` / `adapter list`，失败时通知层降级
//! Console，绝不静默假装弹出了。

use anyhow::{anyhow, Context as _, Result};
use windows::core::HSTRING;
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{
    NotificationSetting, ToastNotification, ToastNotificationManager,
};

/// Fixed application user-model id used for every devzone toast.
pub const APP_ID: &str = "devzone";

/// Show a toast through `Windows.UI.Notifications`. Failures are reported so the
/// caller can degrade to `ConsoleNotify`.
pub fn show(title: &str, body: &str) -> Result<()> {
    let _com = ComApartment::initialize()?;
    let xml = build_toast_xml(title, body);
    let document = XmlDocument::new()
        .map_err(|err| anyhow!("XmlDocument 创建失败：{err}（WinRT/COM 初始化异常）"))?;
    document
        .LoadXml(&HSTRING::from(xml))
        .map_err(|err| anyhow!("toast XML 解析失败：{err}"))?;
    let notification = ToastNotification::CreateToastNotification(&document)
        .map_err(|err| anyhow!("ToastNotification 创建失败：{err}（需要 Data_Xml_Dom 运行时）"))?;
    let notifier = notifier()?;
    notifier
        .Show(&notification)
        .map_err(|err| anyhow!("ToastNotifier::Show 失败：{err}；{UNREGISTERED_HINT}"))?;
    Ok(())
}

/// Read-only capability probe used by `devzone watchdog status` / `adapter list`.
/// 不弹窗、不写注册表，只查询系统给本 AUMID 的通知许可状态。
pub fn probe() -> Result<String> {
    let _com = ComApartment::initialize()?;
    let notifier = notifier()?;
    let setting = notifier
        .Setting()
        .map_err(|err| anyhow!("ToastNotifier::Setting 查询失败：{err}；{UNREGISTERED_HINT}"))?;
    let readable = setting_name(setting);
    if setting == NotificationSetting::Enabled {
        Ok(format!("AUMID={APP_ID}：通知可用（{readable}）"))
    } else {
        Ok(format!(
            "AUMID={APP_ID}：系统当前不允许弹 toast（{readable}）；devzone 会降级到控制台通知"
        ))
    }
}

/// COM apartment guard: WinRT activation needs an initialized apartment on the
/// calling thread. devzone 的进程入口没有初始化 COM，所以每次用 toast 都兜一次。
struct ComApartment {
    owned: bool,
}

impl ComApartment {
    fn initialize() -> Result<ComApartment> {
        use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

        // toast 的回调（Dismissed/Activated）需要 STA；这里只是同步 Show，两种模型都行。
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr.is_ok() {
            return Ok(ComApartment { owned: true });
        }
        if hr == RPC_E_CHANGED_MODE {
            tracing::debug!("CoInitializeEx: 该线程已用其它模型初始化，沿用现状");
            return Ok(ComApartment { owned: false });
        }
        Err(anyhow!("CoInitializeEx 失败：{hr:?}"))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.owned {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

/// AUMID registration hint repeated in every toast failure.
const UNREGISTERED_HINT: &str =
    "未打包应用需在 HKCU\\Software\\Classes\\AppId\\devzone 注册 AUMID \
                                 （或开始菜单快捷方式带 AUMID）后才能弹 toast；\
                                 devzone 不自动写注册表，失败时按 §9 降级 Console";

fn notifier() -> Result<windows::UI::Notifications::ToastNotifier> {
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))
        .map_err(|err| anyhow!("CreateToastNotifierWithId 失败：{err}；{UNREGISTERED_HINT}"))
}

fn setting_name(setting: NotificationSetting) -> &'static str {
    if setting == NotificationSetting::Enabled {
        "Enabled"
    } else if setting == NotificationSetting::DisabledForApplication {
        "DisabledForApplication"
    } else if setting == NotificationSetting::DisabledForUser {
        "DisabledForUser"
    } else if setting == NotificationSetting::DisabledByGroupPolicy {
        "DisabledByGroupPolicy"
    } else if setting == NotificationSetting::DisabledByManifest {
        "DisabledByManifest"
    } else {
        "Unknown"
    }
}

/// Escape XML text (`&`, `<`, `>`, quotes) so a title containing `<ai>` cannot
/// break the toast document.
pub fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// The ToastGeneric payload described in section 6.5.4:
/// `<toast><visual><binding template="ToastGeneric"><text>title</text><text>body</text></binding></visual></toast>`.
pub fn build_toast_xml(title: &str, body: &str) -> String {
    let mut texts = String::new();
    let title = xml_escape(title.trim());
    let body = xml_escape(body.trim());
    // 换行在 XML 文本节点里合法，多行正文原样保留（§6.5.4 的消息模板就是多行）。
    if !title.is_empty() {
        texts.push_str(&format!("<text>{title}</text>"));
    }
    if !body.is_empty() {
        texts.push_str(&format!("<text>{body}</text>"));
    }
    format!("<toast><visual><binding template=\"ToastGeneric\">{texts}</binding></visual></toast>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_the_xml_metacharacters() {
        assert_eq!(xml_escape("a<b>&\"'"), "a&lt;b&gt;&amp;&quot;&apos;");
        assert_eq!(xml_escape("普通文本 100%"), "普通文本 100%");
        assert_eq!(xml_escape(""), "");
    }

    #[test]
    fn builds_the_toastgeneric_template() {
        let xml = build_toast_xml("AI 疑似停住", "剩余 18% <注意>");
        assert!(
            xml.starts_with("<toast><visual><binding template=\"ToastGeneric\">"),
            "{xml}"
        );
        assert!(xml.contains("<text>AI 疑似停住</text>"), "{xml}");
        assert!(xml.contains("<text>剩余 18% &lt;注意&gt;</text>"), "{xml}");
        assert!(xml.ends_with("</binding></visual></toast>"), "{xml}");
        // 两段文本 = 两个 <text> 节点
        assert_eq!(xml.matches("<text>").count(), 2);
    }

    #[test]
    fn omits_empty_slots_instead_of_emitting_blank_text() {
        assert_eq!(
            build_toast_xml("标题", "   "),
            "<toast><visual><binding template=\"ToastGeneric\"><text>标题</text></binding></visual></toast>"
        );
        assert_eq!(
            build_toast_xml("", "正文"),
            "<toast><visual><binding template=\"ToastGeneric\"><text>正文</text></binding></visual></toast>"
        );
        assert!(build_toast_xml("", "").contains("ToastGeneric"));
    }

    #[test]
    fn the_generated_document_is_parseable_by_the_dom() {
        // 真正的 XML 解析检查：只构造 XmlDocument，不创建 notifier、不弹窗。
        let xml = build_toast_xml("A & B <x>", "line1\nline2");
        let document = XmlDocument::new().expect("XmlDocument::new");
        document
            .LoadXml(&HSTRING::from(xml))
            .expect("toast XML 必须能被 DOM 解析");
        let root = document.DocumentElement().expect("有根节点");
        assert_eq!(root.NodeName().unwrap().to_string(), "toast");
    }

    #[test]
    fn probe_never_panics_and_never_shows_a_toast() {
        // probe 只查询许可状态；未注册 AUMID / 无 WinRT 环境时返回 Err 属正常。
        match probe() {
            Ok(text) => eprintln!("[toast] probe: {text}"),
            Err(err) => eprintln!("[toast] probe 不可用：{err}"),
        }
    }
}

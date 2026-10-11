//! UI Automation wrapper (graceful-pause level 2, watchdog Full/Lite text reads).
//!
//! COM initialisation is per-thread; every entry point initialises on demand and
//! leaves the apartment state to the caller.
//!
//! All UIA calls in `windows` 0.58 return `windows::core::Result`, so every layer
//! below propagates with `?` / `let-else` and never unwraps a COM failure.

use anyhow::{anyhow, Context as _, Result};
use windows::core::BSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationElementArray,
    IUIAutomationInvokePattern, IUIAutomationValuePattern, TreeScope_Subtree, UIA_InvokePatternId,
    UIA_ValuePatternId,
};

use crate::util;

/// Elements devzone will Invoke when pausing an app (section 6.3.6 level 2).
pub const PAUSE_BUTTON_NAMES: [&str; 6] = ["停止", "暂停", "保存", "Stop", "Pause", "Save"];

/// Hard cap on elements walked per call. UIA `FindAll` with a true condition
/// walks the whole subtree in-process; on Electron windows that can be tens of
/// thousands of nodes, so both entry points bound the result set.
pub const MAX_ELEMENTS: usize = 400;

/// `S_FALSE` (1) is success-but-already-initialised.
const S_FALSE: i32 = 1;

/// `RPC_E_CHANGED_MODE`: the thread already lives in the other apartment model.
/// UIA keeps working there, so it is tolerated rather than treated as failure.
const RPC_E_CHANGED_MODE: i32 = 0x8001_0106u32 as i32;

/// Idempotent `CoInitializeEx` for the current thread. The matching
/// `CoUninitialize` is deliberately left to the caller, which lives on the same
/// thread; repeated calls are legal and answer S_OK / S_FALSE.
fn init_com() -> Result<()> {
    // SAFETY: `pvReserved` must be NULL and the apartment flag is a plain value.
    let status = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if status.is_ok() || status.0 == S_FALSE {
        return Ok(());
    }
    if status.0 == RPC_E_CHANGED_MODE {
        tracing::debug!("CoInitializeEx: 线程已在 MTA，继续使用现有套间");
        return Ok(());
    }
    Err(anyhow!("CoInitializeEx 失败: {status:?}"))
}

/// Create the UIA client, or explain which COM step failed.
fn create_automation() -> Result<IUIAutomation> {
    init_com()?;
    // SAFETY: standard COM activation of a well-known in-proc server. The class
    // GUID comes from the windows metadata and the interface IID is taken from
    // the return type's `Interface::IID`, so there is no mismatch to guard.
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
            .context("CoCreateInstance(CUIAutomation)")?;
    Ok(automation)
}

/// `CoInitialize` + create `CUIAutomation`; used by `doctor` item 7.
pub fn probe() -> Result<String> {
    let automation = create_automation()?;
    // SAFETY: vtable call on a freshly created interface pointer.
    let root: IUIAutomationElement =
        unsafe { automation.GetRootElement() }.context("IUIAutomation::GetRootElement")?;
    // SAFETY: vtable call.
    let name = unsafe { root.CurrentName() }.context("IUIAutomationElement::CurrentName")?;
    let desktop = bstr_to_string(&name);
    Ok(format!(
        "CoInitializeEx + CUIAutomation + GetRootElement 成功（根元素 Name={desktop:?}）；\
         目标窗口的元素匹配仍需逐应用实测（§16 待实测项 9）"
    ))
}

/// BSTR -> String without panicking on lone surrogates; an empty BSTR becomes "".
fn bstr_to_string(value: &BSTR) -> String {
    if value.is_empty() {
        return String::new();
    }
    String::from_utf16_lossy(value.as_wide())
}

/// Trimmed, lower-cased form used for every Name comparison (Latin button labels
/// differ in case between builds, the Chinese ones do not differ at all).
fn normalise(value: &str) -> String {
    value.trim().to_lowercase()
}

/// Pure half of element matching: does `actual` (an element Name) equal one of
/// `wanted`? Exact after trimming/case folding — no substring matching, so a
/// "暂停更新" button is not mistaken for "暂停".
pub fn matches_name(actual: &str, wanted: &[&str]) -> bool {
    let actual = normalise(actual);
    !actual.is_empty() && wanted.iter().any(|want| normalise(want) == actual)
}

/// Bound a caller supplied element budget (always at least one).
pub fn clamp_limit(limit: usize) -> usize {
    limit.clamp(1, MAX_ELEMENTS)
}

/// Pure half of the walk loop bounds: never negative, never above the budget.
fn index_end(total: i32, limit: usize) -> i32 {
    let budget = i32::try_from(clamp_limit(limit)).unwrap_or(i32::MAX);
    total.max(0).min(budget)
}

/// `ElementFromHandle` with a message naming the handle, because a failure here
/// is the usual symptom of a window whose provider is not exposed to UIA.
fn root_element(automation: &IUIAutomation, hwnd: HWND) -> Result<IUIAutomationElement> {
    // SAFETY: vtable call; an invalid or foreign HWND simply returns a COM error.
    unsafe { automation.ElementFromHandle(hwnd) }
        .map_err(|err| anyhow!("ElementFromHandle({hwnd:?}) 失败: {err}"))
}

/// True-condition `FindAll` over the whole subtree.
fn subtree(
    automation: &IUIAutomation,
    root: &IUIAutomationElement,
) -> Result<IUIAutomationElementArray> {
    // SAFETY: vtable calls with owned interfaces.
    let condition = unsafe { automation.CreateTrueCondition() }.context("CreateTrueCondition")?;
    unsafe { root.FindAll(TreeScope_Subtree, &condition) }.context("FindAll")
}

/// Names of the elements under `hwnd` (subtree), capped at `limit`.
/// Read-only: never invokes a pattern, never sets focus, never posts a message —
/// which is exactly what `doctor` / `watchdog adapter test` may do.
pub fn element_names(hwnd: HWND, limit: usize) -> Result<Vec<String>> {
    let automation = create_automation()?;
    let root = root_element(&automation, hwnd)?;
    let found = subtree(&automation, &root)?;
    // SAFETY: vtable call.
    let total = unsafe { found.Length() }.context("IUIAutomationElementArray::Length")?;
    let mut names = Vec::new();
    for index in 0..index_end(total, limit) {
        // SAFETY: `index` stays below the length reported by `Length()`.
        let Ok(element) = (unsafe { found.GetElement(index) }) else {
            continue;
        };
        // SAFETY: vtable call.
        let Ok(name) = (unsafe { element.CurrentName() }) else {
            continue;
        };
        let text = bstr_to_string(&name);
        if !text.trim().is_empty() {
            names.push(text);
        }
    }
    Ok(names)
}

/// Find a clickable element whose Name matches one of `names` and Invoke it.
/// Returns the matched element name on success.
pub fn invoke_by_name(hwnd: HWND, names: &[&str]) -> Result<String> {
    if util::ExternalCmd::dry_run() {
        util::progress(&format!(
            "dry-run: UIA 将查找 Name∈[{}] 的元素并 InvokePattern.Invoke（未发送）",
            names.join("/")
        ));
        return Err(anyhow!("dry-run：UIA Invoke 未执行"));
    }
    let automation = create_automation()?;
    let root = root_element(&automation, hwnd)?;
    let found = subtree(&automation, &root)?;
    // SAFETY: vtable call.
    let total = unsafe { found.Length() }.context("IUIAutomationElementArray::Length")?;
    let mut last_error = String::new();
    for index in 0..index_end(total, MAX_ELEMENTS) {
        // SAFETY: `index` stays below the reported length.
        let Ok(element) = (unsafe { found.GetElement(index) }) else {
            continue;
        };
        // SAFETY: vtable call.
        let Ok(name) = (unsafe { element.CurrentName() }) else {
            continue;
        };
        let text = bstr_to_string(&name);
        if !matches_name(&text, names) {
            continue;
        }
        // SAFETY: vtable call; elements without the Invoke pattern answer an error.
        let pattern = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
        };
        match pattern {
            Ok(pattern) => {
                // SAFETY: `Invoke` is the documented pattern method; it posts no
                // keyboard input and closes nothing by itself.
                match unsafe { pattern.Invoke() } {
                    Ok(()) => return Ok(text),
                    Err(err) => last_error = format!("{text} Invoke 失败: {err}"),
                }
            }
            Err(err) => last_error = format!("{text} 无 InvokePattern: {err}"),
        }
    }
    Err(anyhow!(
        "UIA 未找到可 Invoke 的元素（Name∈[{}]）{}",
        names.join("/"),
        if last_error.is_empty() {
            String::new()
        } else {
            format!("；最后错误：{last_error}")
        }
    ))
}

/// Text snippets of a window (label + value pairs) for the watchdog's credit
/// reading (§6.5.3), without running OCR. Node count is capped by `clamp_limit`
/// and depth is handled inside UIA (`FindAll`), so a deep Electron tree cannot
/// overflow our stack. No in-binary caller until `mod_watchdog` lands.
#[allow(dead_code)]
pub fn text_snippets(hwnd: HWND, limit: usize) -> Vec<(String, String)> {
    let limit = clamp_limit(limit);
    let Ok(automation) = create_automation() else {
        tracing::debug!("uia::text_snippets 无法创建 CUIAutomation");
        return Vec::new();
    };
    let Ok(root) = root_element(&automation, hwnd) else {
        return Vec::new();
    };
    let Ok(found) = subtree(&automation, &root) else {
        return Vec::new();
    };
    // SAFETY: vtable call.
    let Ok(total) = (unsafe { found.Length() }) else {
        tracing::debug!("uia::text_snippets Length 失败");
        return Vec::new();
    };
    let mut out = Vec::new();
    for index in 0..index_end(total, limit) {
        // SAFETY: `index` stays below the reported length.
        let Ok(element) = (unsafe { found.GetElement(index) }) else {
            continue;
        };
        // SAFETY: vtable call.
        let label = unsafe { element.CurrentName() }
            .map(|name| bstr_to_string(&name))
            .unwrap_or_default();
        let value = value_of(&element);
        if label.trim().is_empty() && value.trim().is_empty() {
            continue;
        }
        out.push((label, value));
    }
    out
}

/// ValuePattern value of an element; empty when the element carries no value.
fn value_of(element: &IUIAutomationElement) -> String {
    // SAFETY: vtable call; a missing pattern is an ordinary COM error.
    let Ok(pattern) =
        (unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) })
    else {
        return String::new();
    };
    // SAFETY: vtable call.
    match unsafe { pattern.CurrentValue() } {
        Ok(value) => bstr_to_string(&value),
        Err(err) => {
            tracing::debug!("CurrentValue 读取失败: {err}");
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn pause_button_names_match_case_insensitively_and_exactly() {
        assert!(matches_name("Stop", &PAUSE_BUTTON_NAMES));
        assert!(matches_name("  stop  ", &PAUSE_BUTTON_NAMES));
        assert!(matches_name("暂停", &PAUSE_BUTTON_NAMES));
        assert!(matches_name("保存", &PAUSE_BUTTON_NAMES));
        // a longer label is a different button: no substring matching
        assert!(!matches_name("暂停更新", &PAUSE_BUTTON_NAMES));
        assert!(!matches_name("", &PAUSE_BUTTON_NAMES));
        assert!(!matches_name("取消", &PAUSE_BUTTON_NAMES));
        assert!(!matches_name("随便什么", &[]));
    }

    #[test]
    fn limit_is_bounded_and_never_zero() {
        assert_eq!(clamp_limit(0), 1);
        assert_eq!(clamp_limit(50), 50);
        assert_eq!(clamp_limit(usize::MAX), MAX_ELEMENTS);
    }

    #[test]
    fn walk_bounds_never_go_negative_or_exceed_the_budget() {
        assert_eq!(index_end(-7, 100), 0);
        assert_eq!(index_end(3, 100), 3);
        assert_eq!(index_end(10_000, 7), 7);
        assert_eq!(index_end(10_000, usize::MAX), MAX_ELEMENTS as i32);
    }

    #[test]
    fn bstr_conversion_round_trips_utf16() {
        let value = BSTR::from("豆包 Doubao");
        assert_eq!(bstr_to_string(&value), "豆包 Doubao");
        assert_eq!(bstr_to_string(&BSTR::default()), "");
    }

    #[test]
    fn probe_reports_ui_availability_or_a_readable_error() {
        // Read-only COM activation; on a machine without UIA this must answer an
        // Err naming the API rather than panicking.
        match probe() {
            Ok(text) => assert!(text.contains("CUIAutomation"), "{text}"),
            Err(err) => assert!(!err.to_string().is_empty(), "{err}"),
        }
    }

    #[test]
    fn element_names_survives_an_invalid_handle() {
        // NULL HWND: UIA rejects it and the message names the failing API.
        let error = element_names(HWND::default(), 8).unwrap_err().to_string();
        assert!(
            error.contains("ElementFromHandle") || error.contains("CoCreateInstance"),
            "{error}"
        );
    }

    #[test]
    fn text_snippets_survives_an_invalid_handle() {
        assert!(text_snippets(HWND::default(), 8).is_empty());
    }

    #[serial]
    #[test]
    fn invoke_by_name_refuses_in_dry_run() {
        std::env::set_var(util::DRY_RUN_ENV, "1");
        let result = invoke_by_name(HWND::default(), &PAUSE_BUTTON_NAMES);
        assert!(result.is_err(), "{result:?}");
        assert!(result.unwrap_err().to_string().contains("dry-run"));
        std::env::remove_var(util::DRY_RUN_ENV);
        assert!(!util::ExternalCmd::dry_run());
    }
}

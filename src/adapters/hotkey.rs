//! `SendInput` key sequences (graceful-pause level 1) and one synthetic click
//! used by the OCR fallback. Both build their input arrays through pure helpers
//! so the byte layout is unit-testable.
//!
//! Nothing here runs when `DEVZONE_DRY_RUN` is set: the planned inputs are logged
//! instead of delivered, so a smoke test can never touch a real window.

use std::time::Duration;

use anyhow::{bail, Result};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MOVE, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SetForegroundWindow, SM_CXSCREEN, SM_CYSCREEN,
};

use crate::util;

/// Virtual keys used by the documented level-1 sequence (section 6.3.6).
pub const VK_ESCAPE: u16 = 0x1B;
pub const VK_CONTROL: u16 = 0x11;
pub const VK_S: u16 = 0x53;

/// Pause between Esc and Ctrl+S, and after Ctrl+S, so the app has time to react
/// before the exit confirmation starts (section 6.3.6 level 1: "等 2s").
pub const STEP_WAIT: Duration = Duration::from_secs(2);

/// `INPUT` is a fixed-size POD; the union payload it embeds is what makes the
/// construction below `unsafe`. The safety precondition for every helper here is
/// simply: `r#type` must match which union field is initialised (`ki` for
/// `INPUT_KEYBOARD`, `mi` for `INPUT_MOUSE`), which the two builders guarantee
/// by construction.
fn keyboard(event: &KeyEvent) -> INPUT {
    // The Ctrl modifier is not a flag: the plan carries VK_CONTROL as its own
    // down/up events around the key, which is how SendInput models chords.
    let _ = event.ctrl;
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(event.vk),
                wScan: 0,
                // KEYEVENTF_KEYUP is the only flag we need: sendinput takes
                // separate down and up events, no unicode or extended-key forms.
                dwFlags: if event.down {
                    KEYBD_EVENT_FLAGS(0)
                } else {
                    KEYEVENTF_KEYUP
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// A single key event in the sequence: virtual key plus the Ctrl modifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub vk: u16,
    pub ctrl: bool,
    pub down: bool,
}

/// `Esc`, wait, `Ctrl+S`, wait  (section 6.3.6 level 1).
pub fn planned_sequence() -> Vec<KeyEvent> {
    vec![
        KeyEvent {
            vk: VK_ESCAPE,
            ctrl: false,
            down: true,
        },
        KeyEvent {
            vk: VK_ESCAPE,
            ctrl: false,
            down: false,
        },
        KeyEvent {
            vk: VK_CONTROL,
            ctrl: true,
            down: true,
        },
        KeyEvent {
            vk: VK_S,
            ctrl: true,
            down: true,
        },
        KeyEvent {
            vk: VK_S,
            ctrl: true,
            down: false,
        },
        KeyEvent {
            vk: VK_CONTROL,
            ctrl: true,
            down: false,
        },
    ]
}

/// Pure half of level 1: turn the planned key events into `INPUT` records, in
/// order. Esc needs no modifier; the Ctrl+S group relies on VK_CONTROL being
/// held down before VK_S and released after it.
pub fn key_inputs(events: &[KeyEvent]) -> Vec<INPUT> {
    events.iter().map(keyboard).collect()
}

/// Normalise an absolute screen point into the 0..=65535 range `SendInput`
/// expects for `MOUSEEVENTF_ABSOLUTE`, against the primary monitor size.
/// Out-of-range and degenerate monitor sizes are clamped, never panicking.
pub fn normalize_point(x: i32, y: i32, screen_w: i32, screen_h: i32) -> (i32, i32) {
    let map = |value: i32, extent: i32| {
        if extent <= 1 {
            return 0;
        }
        // SendInput's absolute space spans 0..=65535 over the whole virtual
        // screen, so the last pixel column/row has to map onto 65535.
        let scaled = (value.max(0).min(extent - 1) as i64 * 65535) / (extent as i64 - 1);
        scaled.clamp(0, 65535) as i32
    };
    (map(x, screen_w), map(y, screen_h))
}

/// Pure half of the OCR click: move to an absolute position, then press and
/// release the left button. Three inputs, all carrying the same coordinates so
/// the down/up events land where the move put the cursor.
pub fn click_inputs(x: i32, y: i32, screen_w: i32, screen_h: i32) -> Vec<INPUT> {
    let (nx, ny) = normalize_point(x, y, screen_w, screen_h);
    [
        MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE,
        MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_LEFTDOWN,
        MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_LEFTUP,
    ]
    .into_iter()
    .map(|flags| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: nx,
                dy: ny,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    })
    .collect()
}

/// Primary screen size; falls back to a sane 1x1 when `GetSystemMetrics`
/// reports nothing (headless service sessions) so callers never divide by zero.
fn primary_screen_size() -> (i32, i32) {
    let (width, height) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    if width <= 0 || height <= 0 {
        (1, 1)
    } else {
        (width, height)
    }
}

/// Deliver one batch of `INPUT` records; errors when the ring buffer accepted
/// fewer events than we asked for (usually a desktop switch or UIPI block).
///
/// # Safety
/// `inputs` is a contiguous `INPUT` array and `size_of::<INPUT>()` is passed as
/// the cbSize the API insists on; the caller (this module) built every element.
unsafe fn deliver(inputs: &[INPUT]) -> Result<()> {
    let size = i32::try_from(std::mem::size_of::<INPUT>())?;
    let sent = unsafe { SendInput(inputs, size) };
    if sent as usize != inputs.len() {
        bail!(
            "SendInput 只注入了 {sent}/{} 个事件（可能被安全桌面/UIPI 拦截）",
            inputs.len()
        );
    }
    Ok(())
}

/// Bring the window to the foreground and deliver the sequence.
pub fn pause_shortcuts(hwnd: HWND) -> Result<()> {
    if util::ExternalCmd::dry_run() {
        util::progress(&format!(
            "dry-run: SetForegroundWindow({:?}) + SendInput Esc, wait {}s, Ctrl+S, wait {}s",
            hwnd,
            STEP_WAIT.as_secs(),
            STEP_WAIT.as_secs()
        ));
        return Ok(());
    }
    // SAFETY: plain Win32 call; a NULL/invalid HWND simply returns FALSE.
    let focused = unsafe { SetForegroundWindow(hwnd) }.as_bool();
    if !focused {
        tracing::debug!(
            "SetForegroundWindow({hwnd:?}) 返回 FALSE（前台锁）：快捷键可能送进别的窗口"
        );
    }
    let inputs = key_inputs(&planned_sequence());
    // SAFETY: `inputs` was produced by `key_inputs`, which always initialises the
    // keyboard arm of the union for INPUT_KEYBOARD records.
    unsafe { deliver(&inputs)? };
    std::thread::sleep(STEP_WAIT);
    util::progress(&format!(
        "hotkey: 已发送 Esc + Ctrl+S 到 {:?}（focused={focused}），等待 {}s",
        hwnd,
        STEP_WAIT.as_secs()
    ));
    Ok(())
}

/// Single left click at absolute screen coordinates (OCR fallback only).
pub fn left_click_at(x: i32, y: i32) -> Result<()> {
    let (screen_w, screen_h) = primary_screen_size();
    let inputs = click_inputs(x, y, screen_w, screen_h);
    if util::ExternalCmd::dry_run() {
        util::progress(&format!(
            "dry-run: 鼠标左键点击 ({x},{y}) -> 归一化 {:?}（主屏 {screen_w}x{screen_h}）",
            normalize_point(x, y, screen_w, screen_h)
        ));
        return Ok(());
    }
    // SAFETY: records built by `click_inputs` (mouse arm of the union).
    unsafe { deliver(&inputs) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn planned_sequence_is_esc_then_ctrl_s() {
        let events = planned_sequence();
        assert_eq!(events.len(), 6);
        assert_eq!(
            events[0],
            KeyEvent {
                vk: VK_ESCAPE,
                ctrl: false,
                down: true
            }
        );
        assert_eq!(
            events[1],
            KeyEvent {
                vk: VK_ESCAPE,
                ctrl: false,
                down: false
            }
        );
        assert_eq!(
            events[2],
            KeyEvent {
                vk: VK_CONTROL,
                ctrl: true,
                down: true
            }
        );
        assert_eq!(
            events[3],
            KeyEvent {
                vk: VK_S,
                ctrl: true,
                down: true
            }
        );
        assert_eq!(
            events[4],
            KeyEvent {
                vk: VK_S,
                ctrl: true,
                down: false
            }
        );
        assert_eq!(
            events[5],
            KeyEvent {
                vk: VK_CONTROL,
                ctrl: true,
                down: false
            }
        );
        // Ctrl is held across S: down before it, up after it.
        assert!(events[2].down);
        assert!(!events[5].down);
        assert_eq!(STEP_WAIT, Duration::from_secs(2));
    }

    #[test]
    fn key_inputs_set_keyup_flag_only_on_release() {
        let inputs = key_inputs(&planned_sequence());
        assert_eq!(inputs.len(), 6);
        for (index, input) in inputs.iter().enumerate() {
            assert_eq!(input.r#type, INPUT_KEYBOARD);
            // SAFETY: every record was built by `key_inputs` with the keyboard arm.
            let key = unsafe { input.Anonymous.ki };
            let is_release = matches!(index, 1 | 4 | 5);
            if is_release {
                assert_eq!(key.dwFlags, KEYEVENTF_KEYUP, "event {index}");
            } else {
                assert_eq!(key.dwFlags.0, 0, "event {index}");
            }
        }
        // SAFETY: same guarantee as above.
        let vks: Vec<u16> = inputs
            .iter()
            .map(|input| unsafe { input.Anonymous.ki }.wVk.0)
            .collect();
        assert_eq!(vks, vec![0x1B, 0x1B, 0x11, 0x53, 0x53, 0x11]);
    }

    #[test]
    fn key_inputs_is_empty_for_empty_plan() {
        assert!(key_inputs(&[]).is_empty());
    }

    #[test]
    fn normalize_point_maps_to_0_65535() {
        assert_eq!(normalize_point(0, 0, 1920, 1080), (0, 0));
        assert_eq!(normalize_point(1919, 1079, 1920, 1080), (65535, 65535));
        let (cx, cy) = normalize_point(960, 540, 1920, 1080);
        assert!((32760..=32830).contains(&cx), "{cx}");
        assert!((32760..=32830).contains(&cy), "{cy}");
    }

    #[test]
    fn normalize_point_clamps_and_survives_bad_monitors() {
        // outside the desktop is clamped, not wrapped
        assert_eq!(normalize_point(-500, 4000, 1920, 1080), (0, 65535));
        // degenerate metrics must never divide by zero
        assert_eq!(normalize_point(10, 10, 0, 0), (0, 0));
        assert_eq!(normalize_point(10, 10, -1, 1), (0, 0));
    }

    #[test]
    fn click_inputs_moves_then_presses_and_releases_at_the_same_point() {
        let inputs = click_inputs(100, 200, 1000, 800);
        assert_eq!(inputs.len(), 3);
        let flags: Vec<_> = inputs
            .iter()
            // SAFETY: built by `click_inputs`, which always fills the mouse arm.
            .map(|input| unsafe { input.Anonymous.mi })
            .collect();
        assert_eq!(inputs[0].r#type, INPUT_MOUSE);
        assert_eq!(flags[0].dwFlags, MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE);
        assert_eq!(
            flags[1].dwFlags,
            MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_LEFTDOWN
        );
        assert_eq!(flags[2].dwFlags, MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_LEFTUP);
        for mouse in &flags {
            // same mapping as `normalize_point`: 0..=65535 over extent-1
            assert_eq!((mouse.dx, mouse.dy), (100 * 65535 / 999, 200 * 65535 / 799));
        }
    }

    #[serial]
    #[test]
    fn dry_run_never_delivers_input() {
        // The mutation entry points must be inert under DEVZONE_DRY_RUN, so a
        // smoke test on a real desktop cannot press a key or click.
        std::env::set_var(util::DRY_RUN_ENV, "1");
        let result = pause_shortcuts(HWND::default());
        assert!(result.is_ok(), "{result:?}");
        assert!(left_click_at(10, 10).is_ok());
        std::env::remove_var(util::DRY_RUN_ENV);
        assert!(!util::ExternalCmd::dry_run());
    }
}

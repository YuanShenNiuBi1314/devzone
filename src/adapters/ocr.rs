//! Screen capture (GDI `BitBlt`) plus the optional OCR pass.
//!
//! Capture serves both sensing tiers: Lite mode only hashes the captured region
//! (see `pattern::dhash`), Full mode additionally runs OCR on it. The OCR path is
//! off by default: it needs `--features ocr` **and** an app config with `ocr` in
//! `enabled_levels` and `ocr_fallback = true` (section 6.3.6 level 4). The
//! README discloses that boundary; nothing here batch-automates a third party.
//!
//! 捕获链路（GetDC/BitBlt/GetDIBits）是**纯只读**的：不移动窗口、不发消息、不点击，
//! 因此即使本二进制没编译 OCR，Lite 模式（§6.5.2 的区域哈希）依然可用。
//!
//! 隐私（§8）：像素只在内存里存在；落盘 PNG 的唯一入口是
//! [`maybe_save_screenshot`] / [`save_rgb_debug`]，且必须 `debug_save_screenshots = true`。

use std::path::Path;

use anyhow::{anyhow, Context as _, Result};
use image::RgbImage;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
    HGDIOBJ, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, GetWindowRect, IsIconic, SM_CXSCREEN, SM_CYSCREEN,
};

use crate::config::RegionCfg;

// WinRT 类型只在 `--features ocr` 构建里出现（Cargo.toml 的 ocr 特性已带齐
// Media_Ocr / Graphics_Imaging / Storage_Streams / Foundation_Collections / Foundation）。
#[cfg(feature = "ocr")]
use windows::{
    Foundation::{AsyncStatus, IAsyncOperation},
    Media::Ocr::{OcrEngine, OcrLine, OcrResult},
};

/// A recognised text run and the centre of its bounding box, in screen
/// coordinates relative to the captured region's origin.
///
/// 只在 `--features ocr` 的构建里被构造；未编译 OCR 时 `find_text` 直接返回 Err，
/// 所以这里显式允许 dead_code（字段仍被 mod_watchdog 静态读取，用于 JSON 输出）。
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct OcrHit {
    pub text: String,
    pub centre: (i32, i32),
}

/// True when this binary was built with the OCR feature.
pub const FEATURE_ENABLED: bool = cfg!(feature = "ocr");

/// Single capture side limit: a mistyped region must not allocate gigabytes.
const MAX_SIDE: i32 = 8192;

/// ReleaseDC(NULL) counterpart of [`GetDC`] — the screen DC must be released.
struct ScreenDc(HDC);

impl Drop for ScreenDc {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseDC(HWND::default(), self.0);
        }
    }
}

/// DeleteDC counterpart of [`CreateCompatibleDC`].
struct MemDc(HDC);

impl Drop for MemDc {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.0);
        }
    }
}

/// DeleteObject counterpart of [`CreateCompatibleBitmap`].
struct Dib(HBITMAP);

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(self.0 .0));
        }
    }
}

/// Restores the previously selected GDI object (a DC must never be deleted while
/// it still holds a bitmap we created).
struct Selected {
    dc: HDC,
    old: HGDIOBJ,
}

impl Drop for Selected {
    fn drop(&mut self) {
        unsafe {
            let _ = SelectObject(self.dc, self.old);
        }
    }
}

/// `(left, top, right, bottom)` → the config-shaped region.
pub fn region_from_rect(rect: (i32, i32, i32, i32)) -> RegionCfg {
    RegionCfg {
        x: rect.0,
        y: rect.1,
        width: rect.2 - rect.0,
        height: rect.3 - rect.1,
    }
}

/// Primary screen size in pixels, used to warn about off-screen regions.
pub fn screen_size() -> (i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_CXSCREEN).max(0),
            GetSystemMetrics(SM_CYSCREEN).max(0),
        )
    }
}

/// True when the region is (partly) outside the primary screen. Informational
/// only: multi-monitor setups legitimately capture beyond it.
pub fn region_offscreen(region: &RegionCfg) -> bool {
    let (sw, sh) = screen_size();
    region.x < 0 || region.y < 0 || region.x + region.width > sw || region.y + region.height > sh
}

/// Capture an absolute screen rectangle as an RGB image.
///
/// GDI 路径：`GetDC(NULL)` → `CreateCompatibleDC` → `CreateCompatibleBitmap` →
/// `BitBlt(SRCCOPY)` → `GetDIBits`（`biHeight` 取负 ⇒ 自上而下的 32bpp DIB）→ BGRA→RGB。
/// 每个句柄都由上面的 RAII 包装负责释放，中途失败也不会泄漏。
pub fn capture_region(region: &RegionCfg) -> Result<RgbImage> {
    if region.width <= 0 || region.height <= 0 {
        return Err(anyhow!(
            "截图区域尺寸无效：{}x{}（需要大于 0；窗口可能已最小化或已关闭）",
            region.width,
            region.height
        ));
    }
    if region.width > MAX_SIDE || region.height > MAX_SIDE {
        return Err(anyhow!(
            "截图区域过大：{}x{}（单边上限 {MAX_SIDE}px）",
            region.width,
            region.height
        ));
    }
    let (w, h) = (region.width, region.height);
    if region_offscreen(region) {
        tracing::debug!(
            "capture_region: 区域 ({},{}) {}x{} 超出主屏 {:?}，可能截到黑边（多显示器属正常）",
            region.x,
            region.y,
            w,
            h,
            screen_size()
        );
    }

    unsafe {
        let screen = ScreenDc(GetDC(HWND::default()));
        if screen.0.is_invalid() {
            return Err(anyhow!("GetDC(屏幕) 失败：无法取得屏幕 DC"));
        }
        let mem = MemDc(CreateCompatibleDC(screen.0));
        if mem.0.is_invalid() {
            return Err(anyhow!("CreateCompatibleDC 失败"));
        }
        let bitmap = Dib(CreateCompatibleBitmap(screen.0, w, h));
        if bitmap.0.is_invalid() {
            return Err(anyhow!("CreateCompatibleBitmap {w}x{h} 失败"));
        }
        let old = SelectObject(mem.0, HGDIOBJ(bitmap.0 .0));
        let _selected = Selected { dc: mem.0, old };

        BitBlt(mem.0, 0, 0, w, h, screen.0, region.x, region.y, SRCCOPY)
            .map_err(|err| anyhow!("BitBlt 拷贝屏幕区域失败：{err}"))?;

        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = w;
        // 负高度 ⇒ top-down DIB，第一行就是屏幕最上方那行，省掉一次翻转
        info.bmiHeader.biHeight = -h;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB.0;

        let mut bits = vec![0u8; (w as usize) * (h as usize) * 4];
        let scanned = GetDIBits(
            mem.0,
            bitmap.0,
            0,
            h as u32,
            Some(bits.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        if scanned == 0 {
            return Err(anyhow!(
                "GetDIBits 失败：区域 ({},{}) {}x{} 可能不在任何显示器范围内",
                region.x,
                region.y,
                w,
                h
            ));
        }

        // BGRA(BGR0) → RGB
        let mut rgb = Vec::with_capacity((w as usize) * (h as usize) * 3);
        for pixel in bits.chunks_exact(4) {
            rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
        }
        RgbImage::from_vec(w as u32, h as u32, rgb)
            .ok_or_else(|| anyhow!("像素缓冲组装 RgbImage 失败（尺寸不匹配）"))
    }
}

/// Capture the client area of a window (used when no region is configured).
pub fn capture_window(hwnd: HWND) -> Result<RgbImage> {
    if hwnd.is_invalid() {
        return Err(anyhow!("capture_window：HWND 无效（进程可能没有主窗口）"));
    }
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }
        .map_err(|err| anyhow!("GetWindowRect 失败：{err}"))?;
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if unsafe { IsIconic(hwnd) }.as_bool() {
        return Err(anyhow!(
            "capture_window：窗口已最小化（外框 {}x{}），请先还原窗口或改用 [watchdog.regions] 固定坐标",
            width,
            height
        ));
    }
    if width <= 0 || height <= 0 {
        return Err(anyhow!(
            "capture_window：窗口外框尺寸 {}x{} 为 0（窗口已关闭或被移到不可见区域）",
            width,
            height
        ));
    }
    capture_region(&region_from_rect((
        rect.left,
        rect.top,
        rect.right,
        rect.bottom,
    )))
}

/// Case-insensitive keyword filter shared by the OCR and UIA channels.
///
/// `pub`：UIA 通道（§6.5.3「找含 积分/额度/剩余 的文本」）与 OCR 通道共用这套关键词，
/// 未编译 OCR 时它也照样被使用。
pub fn matches_keywords(text: &str, keywords: &[&str]) -> bool {
    keywords.is_empty()
        || keywords
            .iter()
            .any(|keyword| crate::util::contains_ci(text, keyword))
}

/// Run OCR over a region and return the hits containing one of `keywords`.
///
/// 实现：Windows OCR（`Windows.Media.Ocr.OcrEngine`，仅需系统自带能力，不下载模型）。
/// `TryCreateFromUserProfileLanguages()` 在没有安装对应语言 OCR 可选组件时返回 null，
/// 这里如实报错而不是静默降级；`RecognizeAsync` 用轮询 `Status()` 阻塞等待（最多 10s），
/// 因为 watchdog 的调用点本身就运行在一次性进程里，不需要 async runtime。
#[cfg(feature = "ocr")]
pub fn find_text(region: &RegionCfg, keywords: &[&str]) -> Result<Vec<OcrHit>> {
    let image = capture_region(region)?;
    ocr_image(&image, keywords)
}

#[cfg(not(feature = "ocr"))]
pub fn find_text(_region: &RegionCfg, _keywords: &[&str]) -> Result<Vec<OcrHit>> {
    Err(anyhow::anyhow!(
        "OCR 未编译进本二进制（需 --features ocr）。Lite 模式只用区域哈希，不跑 OCR 模型"
    ))
}

/// COM apartment guard: WinRT activation needs an initialized apartment.
#[cfg(feature = "ocr")]
struct ComApartment {
    owned: bool,
}

#[cfg(feature = "ocr")]
impl ComApartment {
    fn initialize() -> Result<ComApartment> {
        use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_ok() {
            return Ok(ComApartment { owned: true });
        }
        if hr == RPC_E_CHANGED_MODE {
            // 该线程已用别的线程模型初始化过：COM 仍可用，只是不该由我们反初始化。
            tracing::debug!("CoInitializeEx: 线程已初始化（RPC_E_CHANGED_MODE），沿用现状");
            return Ok(ComApartment { owned: false });
        }
        Err(anyhow!("CoInitializeEx 失败：{hr:?}"))
    }
}

#[cfg(feature = "ocr")]
impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.owned {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

/// OCR a bitmap already in memory (no screen access).
#[cfg(feature = "ocr")]
fn ocr_image(image: &RgbImage, keywords: &[&str]) -> Result<Vec<OcrHit>> {
    use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
    use windows::Storage::Streams::DataWriter;

    let _com = ComApartment::initialize()?;
    let (w, h) = (image.width() as i32, image.height() as i32);
    if w <= 0 || h <= 0 {
        return Err(anyhow!("OCR：位图尺寸无效 {w}x{h}"));
    }

    // Windows OCR 只接受 Bgra8/Gray8；RGB → BGRA（alpha 用不透明）。
    let mut bgra = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for pixel in image.pixels() {
        bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 0xff]);
    }

    let writer = DataWriter::new().context("DataWriter 创建失败")?;
    writer
        .WriteBytes(&bgra)
        .context("DataWriter.WriteBytes 失败")?;
    let buffer = writer
        .DetachBuffer()
        .context("DataWriter.DetachBuffer 失败")?;
    drop(writer);

    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Bgra8, w, h)
        .with_context(|| format!("SoftwareBitmap::CreateCopyFromBuffer 失败（{w}x{h} Bgra8）"))?;
    let engine = OcrEngine::TryCreateFromUserProfileLanguages().map_err(|err| {
        anyhow!(
            "Windows OCR 引擎不可用：系统未安装当前语言的 OCR 可选组件（设置→可选功能→添加「光学字符识别」）：{err}"
        )
    })?;
    let operation = engine
        .RecognizeAsync(&bitmap)
        .context("OcrEngine::RecognizeAsync 调用失败")?;
    let result = await_operation(operation)?;

    let mut hits = Vec::new();
    let lines = result.Lines().context("OcrResult::Lines 读取失败")?;
    let count = lines.Size().context("OcrResult 行数读取失败")?;
    for index in 0..count {
        let line = lines.GetAt(index).context("OcrLine 读取失败")?;
        let text = line
            .Text()
            .map(|value| value.to_string())
            .unwrap_or_default();
        if text.trim().is_empty() || !matches_keywords(&text, keywords) {
            continue;
        }
        hits.push(OcrHit {
            centre: line_centre(&line)?,
            text,
        });
    }
    Ok(hits)
}

/// Poll `IAsyncOperation::Status()` until the OCR job finishes (max 10s).
///
/// `windows::core::Future` 需要外部 executor，devzone 的调用点是一次性进程/前台循环，
/// 因此这里用显式轮询 + `GetResults()`（WinRT 只在 Completed 之后允许取结果）。
#[cfg(feature = "ocr")]
fn await_operation(operation: IAsyncOperation<OcrResult>) -> Result<OcrResult> {
    use std::time::{Duration, Instant};

    const MAX_WAIT: Duration = Duration::from_secs(10);
    const TICK: Duration = Duration::from_millis(25);

    let deadline = Instant::now() + MAX_WAIT;
    loop {
        match operation.Status()? {
            AsyncStatus::Completed => return Ok(operation.GetResults()?),
            AsyncStatus::Error => {
                let code = operation.ErrorCode().map(|value| value.0).unwrap_or(0);
                return Err(anyhow!("OCR 异步识别失败（HRESULT=0x{code:08x}）"));
            }
            AsyncStatus::Canceled => return Err(anyhow!("OCR 异步识别被取消")),
            _ => {
                if Instant::now() >= deadline {
                    let _ = operation.Cancel();
                    return Err(anyhow!(
                        "OCR 识别超时（>{}s）：§6.5.2 的 Lite 模式不跑 OCR 正是为了避开这种开销",
                        MAX_WAIT.as_secs()
                    ));
                }
                std::thread::sleep(TICK);
            }
        }
    }
}

/// Centre of a line's word bounding boxes, relative to the region's origin.
#[cfg(feature = "ocr")]
fn line_centre(line: &OcrLine) -> Result<(i32, i32)> {
    let words = line.Words().context("OcrLine::Words 读取失败")?;
    let count = words.Size().context("OcrWord 数量读取失败")?;
    let (mut left, mut top) = (f32::MAX, f32::MAX);
    let (mut right, mut bottom) = (f32::MIN, f32::MIN);
    for index in 0..count {
        let rect = words
            .GetAt(index)
            .context("OcrWord 读取失败")?
            .BoundingRect()?;
        left = left.min(rect.X);
        top = top.min(rect.Y);
        right = right.max(rect.X + rect.Width);
        bottom = bottom.max(rect.Y + rect.Height);
    }
    if !(left <= right && top <= bottom) {
        return Ok((0, 0));
    }
    Ok((
        ((f64::from(left) + f64::from(right)) / 2.0).round() as i32,
        ((f64::from(top) + f64::from(bottom)) / 2.0).round() as i32,
    ))
}

/// JSON shape of one OCR hit (stable keys, used by `adapter test` / `status`).
pub fn hit_to_json(hit: &OcrHit) -> serde_json::Value {
    serde_json::json!({
        "text": hit.text,
        "centre": [hit.centre.0, hit.centre.1],
    })
}

/// `debug_save_screenshots` is the only path that writes a PNG to disk; the
/// default is off (section 8 privacy).
pub fn maybe_save_screenshot(image: &RgbImage, path: &Path, enabled: bool) -> Result<()> {
    save_rgb_debug(image, path, enabled)
}

/// Debug PNG sink. Writes **only** when `enabled` (i.e. the
/// `debug_save_screenshots` switch) is true — the flag is a parameter on purpose
/// so no caller can silently drop a screenshot.
pub fn save_rgb_debug(image: &RgbImage, path: &Path, enabled: bool) -> Result<()> {
    if !enabled {
        tracing::debug!("debug_save_screenshots=false，跳过落盘 {}", path.display());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    image
        .save(path)
        .with_context(|| format!("写入调试截图 {}", path.display()))?;
    tracing::info!(
        "已写入调试截图（debug_save_screenshots=true）：{}",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(x: i32, y: i32, width: i32, height: i32) -> RegionCfg {
        RegionCfg {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn rejects_degenerate_regions() {
        for bad in [
            region(0, 0, 0, 10),
            region(0, 0, 10, 0),
            region(0, 0, -5, 10),
        ] {
            let err = capture_region(&bad).unwrap_err();
            assert!(err.to_string().contains("尺寸无效"), "{err}");
        }
        let huge = region(0, 0, MAX_SIDE + 1, 10);
        assert!(capture_region(&huge)
            .unwrap_err()
            .to_string()
            .contains("过大"));
    }

    #[test]
    fn region_from_rect_uses_width_and_height() {
        let made = region_from_rect((10, 20, 110, 70));
        assert_eq!((made.x, made.y, made.width, made.height), (10, 20, 100, 50));
    }

    #[test]
    fn screen_size_is_plausible() {
        let (w, h) = screen_size();
        assert!(w >= 640, "{w}x{h}");
        assert!(h >= 480, "{w}x{h}");
        // 一个 1x1 的主屏左上角区域不该被判成屏外
        assert!(!region_offscreen(&region(0, 0, 1, 1)));
        assert!(region_offscreen(&region(-8, 0, 1, 1)));
        assert!(region_offscreen(&region(w, h, 4, 4)));
    }

    /// 只读截屏：真机与 CI 的 windows-latest 都有虚拟屏幕，允许在此执行一次。
    #[test]
    fn captures_one_pixel_region_with_real_data() {
        let image = capture_region(&region(0, 0, 1, 1)).expect("截 1x1 区域应成功");
        assert_eq!(image.width(), 1);
        assert_eq!(image.height(), 1);
        let pixel = *image.get_pixel(0, 0);
        // 任何屏幕（含纯黑桌面）都给出 3 个通道的具体值；关键是缓冲区非空且能组装
        assert_eq!(pixel.0.len(), 3);
        // 稍大一点的区域也必须成功，并给出可哈希的像素
        let bigger = capture_region(&region(0, 0, 24, 16)).expect("截 24x16 应成功");
        assert_eq!((bigger.width(), bigger.height()), (24, 16));
        assert!(crate::adapters::pattern::dhash(&bigger) != 0 || bigger.pixels().len() == 384);
    }

    #[test]
    fn capture_window_rejects_invalid_handle() {
        let err = capture_window(HWND::default()).unwrap_err();
        assert!(err.to_string().contains("HWND 无效"), "{err}");
    }

    #[test]
    fn keyword_filter_is_case_insensitive_and_empty_means_all() {
        assert!(matches_keywords("剩余积分 18%", &["积分", "额度"]));
        assert!(matches_keywords("REMAINING Credits", &["credits"]));
        assert!(!matches_keywords("nothing here", &["积分"]));
        assert!(matches_keywords("anything", &[]));
    }

    #[test]
    fn hit_json_has_stable_shape() {
        let hit = OcrHit {
            text: "剩余 18%".to_string(),
            centre: (12, -3),
        };
        let value = hit_to_json(&hit);
        assert_eq!(value["text"], "剩余 18%");
        assert_eq!(value["centre"][0], 12);
        assert_eq!(value["centre"][1], -3);
    }

    #[test]
    fn debug_sink_never_writes_when_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("shot.png");
        let image = RgbImage::from_pixel(2, 2, image::Rgb([1, 2, 3]));
        maybe_save_screenshot(&image, &path, false).unwrap();
        assert!(!path.exists(), "§8：默认不得落盘截图");
        save_rgb_debug(&image, &path, false).unwrap();
        assert!(!path.exists());
        save_rgb_debug(&image, &path, true).unwrap();
        assert!(path.exists());
        assert!(std::fs::metadata(&path).unwrap().len() > 0);
    }

    #[test]
    fn find_text_reports_missing_feature_or_works() {
        // 未编译 OCR：必须给出"未编译"提示；编译了 OCR：允许因无语言包/无屏幕权限而 Err，
        // 但绝不能 panic（该分支在有 OCR 的机器上真跑一次，结果如实打印）。
        let result = find_text(&region(0, 0, 32, 16), &["积分"]);
        if FEATURE_ENABLED {
            if let Err(err) = &result {
                let text = err.to_string();
                assert!(!text.contains("未编译"), "{text}");
                eprintln!("[ocr] 真机 OCR 通道返回 Err：{text}");
            } else {
                eprintln!("[ocr] 真机 OCR 通道可用，命中 {} 条", result.unwrap().len());
            }
        } else {
            assert!(result.unwrap_err().to_string().contains("未编译"));
        }
    }
}

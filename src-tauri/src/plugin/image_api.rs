//! 图像处理 + 屏幕辅助原语：缩放 / 裁剪 / 格式转换 / 质量压缩 / 信息查询，
//! 鼠标坐标、屏幕取色、DIP（设备无关像素）↔ 物理像素换算。
//!
//! **权限边界**：图像处理系列（resize/crop/convert/compress/info）只在内存里做纯计算，
//! 不读屏幕、不读文件系统之外的任何东西，因此**不设权限门禁**。取色（`pick_color_at`）
//! 读的是屏幕像素，等同于「截屏」这类读屏幕内容的能力，复用已有的 `screen-capture`
//! 权限标识（见 [`super::capture::require_capture`]），不新造权限名。
//!
//! **数据形态**：与项目里 `saveImage`/`readImage`/`plugin_write_image` 等既有图片类命令一致，
//! 输入输出都是**裸 base64 字符串**（无 `data:image/...;base64,` 前缀），桥接层负责解回
//! `ArrayBuffer`。
//!
//! **DIP ↔ 物理像素换算为什么必须做**：Win32 的鼠标 / 像素坐标（`GetCursorPos`、
//! `GetPixel`、`GetSystemMetrics(SM_CXVIRTUALSCREEN)` 等）全部是**物理像素**，而插件页面
//! 里的 CSS/DOM 坐标是**设备无关像素（DIP）**。两者只在 96 DPI（100% 缩放）下数值相等；
//! 混用会导致「鼠标点在这、取色/贴图却对到那」，多屏且各屏缩放不同时尤其明显。
//! 换算依据各显示器自身的缩放比（数据源同 [`super::capture::plugin_list_displays`]
//! 用的 xcap），从主屏（DIP 原点固定为物理原点 (0,0)）出发，按显示器物理位置的相邻关系
//! 逐个推算出各副屏在 DIP 坐标系下的原点——这是处理「虚拟桌面 DIP 坐标」的通用做法：
//! **沿共享边界换算，而不是各自独立除以自身缩放**，否则缩放不同的相邻屏在拼接处会有
//! 看得见的坐标错位。
//!
//! 已知局限见 [`resolve_dip_origins`] 文档。

use serde::Serialize;
use tauri::State;

use super::capture::require_capture;
use super::PluginRegistry;
use crate::settings::SettingsStore;

// ==================== base64 <-> 字节 ====================

fn decode_b64(data: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|e| format!("base64 解码失败: {e}"))
}

fn to_b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// [`image::ImageFormat`] → 人可读的短格式名（供错误信息使用）。非详尽枚举，未知一律给「其他格式」。
fn format_name(fmt: image::ImageFormat) -> &'static str {
    match fmt {
        image::ImageFormat::Png => "png",
        image::ImageFormat::Jpeg => "jpeg",
        image::ImageFormat::WebP => "webp",
        image::ImageFormat::Bmp => "bmp",
        image::ImageFormat::Gif => "gif",
        image::ImageFormat::Tiff => "tiff",
        image::ImageFormat::Ico => "ico",
        image::ImageFormat::Tga => "tga",
        image::ImageFormat::Pnm => "pnm",
        image::ImageFormat::Dds => "dds",
        _ => "其他格式",
    }
}

/// 解析 `plugin_image_convert`/内部编码目标用的格式字符串（大小写不敏感）。
///
/// **只放行本命令承诺支持的四种**（png/jpeg/webp/bmp）——即使 `image` crate 还能编码
/// gif/tiff/ico 等其它格式，也不在此处悄悄放行：调用方传了不认识的值就该看到明确报错，
/// 而不是猜一个「看起来差不多」的格式给它。
fn parse_target_format(format: &str) -> Result<image::ImageFormat, String> {
    match format.trim().to_ascii_lowercase().as_str() {
        "png" => Ok(image::ImageFormat::Png),
        "jpeg" | "jpg" => Ok(image::ImageFormat::Jpeg),
        "webp" => Ok(image::ImageFormat::WebP),
        "bmp" => Ok(image::ImageFormat::Bmp),
        other => Err(format!("不支持的目标格式：{other}（仅支持 png/jpeg/webp/bmp）")),
    }
}

// ==================== 图像处理 ====================

/// 缩放模式（与 CSS `object-fit` 同名同义，前端好理解）：
/// - `contain`：等比缩放，图片完整贴合目标框、不裁剪，某一边可能小于目标尺寸；
/// - `fill`：直接拉伸到目标宽高，不保持长宽比；
/// - `cover`：等比缩放并居中裁剪，铺满整个目标框（无留白）。
///
/// # Errors
/// - `width`/`height` 为 0；
/// - `data` 不是合法 base64 或不是 `image` crate 能解码的图片格式；
/// - `mode` 不是 `contain`/`fill`/`cover` 之一；
/// - 缩放后按原图格式重新编码失败（极少见，通常是内存不足）。
#[tauri::command]
pub async fn plugin_image_resize(
    data: String,
    width: u32,
    height: u32,
    mode: String,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || resize_blocking(&data, width, height, &mode))
        .await
        .map_err(|e| e.to_string())?
}

fn resize_blocking(data: &str, width: u32, height: u32, mode: &str) -> Result<String, String> {
    if width == 0 || height == 0 {
        return Err("width/height 必须大于 0".to_string());
    }
    let bytes = decode_b64(data)?;
    let img = image::load_from_memory(&bytes).map_err(|e| format!("图片解码失败: {e}"))?;
    // 输出沿用原图格式：resize 不该顺带把用户的 png 悄悄换成别的格式；要转格式请用 image_convert。
    let src_fmt = image::guess_format(&bytes).map_err(|e| format!("识别图片格式失败: {e}"))?;
    let filter = image::imageops::FilterType::Lanczos3;
    let out_img = match mode.trim().to_ascii_lowercase().as_str() {
        "contain" => img.resize(width, height, filter),
        "fill" => img.resize_exact(width, height, filter),
        "cover" => img.resize_to_fill(width, height, filter),
        other => {
            return Err(format!(
                "不支持的缩放模式：{other}（仅支持 contain=等比不裁剪 / fill=拉伸 / cover=裁剪填充）"
            ))
        }
    };
    let mut out = std::io::Cursor::new(Vec::new());
    out_img
        .write_to(&mut out, src_fmt)
        .map_err(|e| format!("{} 编码失败: {e}", format_name(src_fmt)))?;
    Ok(to_b64(&out.into_inner()))
}

/// 按矩形裁剪图片（`x`/`y` 为左上角、`w`/`h` 为宽高，单位均为原图像素）。
///
/// 严格校验裁剪区域必须完整落在原图范围内——不做「越界自动裁小」的静默处理，
/// 越界会明确报错并告知原图实际尺寸，避免调用方以为拿到的是完整请求尺寸的结果。
///
/// # Errors
/// - `w`/`h` 为 0，或 `x`/`y` 为负；
/// - 裁剪区域超出原图范围；
/// - `data` 解码失败或重新编码失败。
#[tauri::command]
pub async fn plugin_image_crop(data: String, x: i64, y: i64, w: u32, h: u32) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || crop_blocking(&data, x, y, w, h))
        .await
        .map_err(|e| e.to_string())?
}

fn crop_blocking(data: &str, x: i64, y: i64, w: u32, h: u32) -> Result<String, String> {
    if w == 0 || h == 0 {
        return Err("裁剪宽高必须大于 0".to_string());
    }
    if x < 0 || y < 0 {
        return Err("裁剪起点不能为负".to_string());
    }
    let bytes = decode_b64(data)?;
    let img = image::load_from_memory(&bytes).map_err(|e| format!("图片解码失败: {e}"))?;
    let src_fmt = image::guess_format(&bytes).map_err(|e| format!("识别图片格式失败: {e}"))?;
    let (iw, ih) = (i64::from(img.width()), i64::from(img.height()));
    if x + i64::from(w) > iw || y + i64::from(h) > ih {
        return Err(format!(
            "裁剪区域超出图片范围：图片为 {iw}x{ih}，请求区域为 ({x},{y})+{w}x{h}"
        ));
    }
    // 上面已校验 x/y 落在 [0, i64::from(u32::MAX)] 且不会越过图片边界，转 u32 是安全的窄化。
    let cropped = img.crop_imm(x as u32, y as u32, w, h);
    let mut out = std::io::Cursor::new(Vec::new());
    cropped
        .write_to(&mut out, src_fmt)
        .map_err(|e| format!("{} 编码失败: {e}", format_name(src_fmt)))?;
    Ok(to_b64(&out.into_inner()))
}

/// 图片格式互转：png / jpeg / webp / bmp 之间互转。
///
/// **WebP 说明（诚实标注，勿被「webp 通常更小」的印象误导）**：`image` crate 0.25 内置的
/// WebP 编码器**只有无损（VP8L）实现**，没有接入 `libwebp`，因此转出的 webp 是无损的，
/// 体积不一定比原图小（项目依赖清单里没有、也不允许新加 `libwebp`/`webp` 这类重依赖，
/// 见模块所在文件顶部对依赖最小化的约束）。若目标是「小体积、可接受有损」，请转 jpeg
/// 并配合 [`plugin_image_compress`]。
///
/// # Errors
/// - `format` 不是 png/jpeg/webp/bmp 之一；
/// - `data` 不是合法 base64 或不是 `image` crate 能解码的图片格式；
/// - 目标格式编码失败（例如源图带 `image` 该编码器不支持的颜色类型，理论存在、极少见）。
#[tauri::command]
pub async fn plugin_image_convert(data: String, format: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || convert_blocking(&data, &format))
        .await
        .map_err(|e| e.to_string())?
}

fn convert_blocking(data: &str, format: &str) -> Result<String, String> {
    let bytes = decode_b64(data)?;
    let target = parse_target_format(format)?;
    let img = image::load_from_memory(&bytes).map_err(|e| format!("图片解码失败: {e}"))?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, target)
        .map_err(|e| format!("{} 编码失败: {e}", format_name(target)))?;
    Ok(to_b64(&out.into_inner()))
}

/// 按质量压缩图片体积（真正的有损压缩，不是简单改名/换壳）。
///
/// **仅支持 JPEG 源图**——诚实标注，不做「传别的格式也照单全收、悄悄换成别的格式吐出去」
/// 那种表面能用、实际货不对板的实现：
/// - WebP：`image` crate 0.25 的内置 WebP 编码器只有无损实现，做不出「按 quality 调体积」
///   的有损压缩（原因同 [`plugin_image_convert`] 文档）；
/// - PNG/BMP 等：格式本身就是无损的，没有「质量」这个调节维度。
///
/// 需要压缩非 JPEG 图片时，请先用 [`plugin_image_convert`] 转成 jpeg 再调用本命令。
///
/// `quality` 取值 1-100，数值越小体积越小、失真越大；越界值会被夹到该区间而不是报错。
///
/// # Errors
/// - `data` 解码后识别出的源图格式不是 JPEG；
/// - `data` 不是合法 base64 或不是 `image` crate 能解码的图片格式；
/// - JPEG 重编码失败。
#[tauri::command]
pub async fn plugin_image_compress(data: String, quality: u8) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || compress_blocking(&data, quality))
        .await
        .map_err(|e| e.to_string())?
}

fn compress_blocking(data: &str, quality: u8) -> Result<String, String> {
    let bytes = decode_b64(data)?;
    let fmt = image::guess_format(&bytes).map_err(|e| format!("识别图片格式失败: {e}"))?;
    if fmt != image::ImageFormat::Jpeg {
        return Err(format!(
            "quality 压缩仅支持 JPEG 源图（当前识别为 {}）：WebP 在本项目里只有无损编码器、\
无法做有损质量压缩，PNG/BMP 等格式本身也没有「质量」这个参数——请先用 plugin_image_convert \
转成 jpeg 再压缩",
            format_name(fmt)
        ));
    }
    let img = image::load_from_memory(&bytes).map_err(|e| format!("图片解码失败: {e}"))?;
    // JPEG 不支持 alpha 通道，转 rgb8（与 image::DynamicImage::write_to 内部的自动颜色转换行为一致）。
    let rgb = img.to_rgb8();
    let q = quality.clamp(1, 100);
    let mut out = Vec::new();
    {
        use image::codecs::jpeg::JpegEncoder;
        use image::ImageEncoder;
        JpegEncoder::new_with_quality(&mut out, q)
            .write_image(rgb.as_raw(), rgb.width(), rgb.height(), image::ExtendedColorType::Rgb8)
            .map_err(|e| format!("JPEG 编码失败: {e}"))?;
    }
    Ok(to_b64(&out))
}

/// 图片信息：宽高、格式、体积（字节，即 base64 解码后的原始文件大小）。
///
/// # Errors
/// - `data` 不是合法 base64；
/// - 无法识别图片格式，或格式虽能识别但 `image` crate 无法解码（如未开启的格式变种）。
#[tauri::command]
pub async fn plugin_image_info(data: String) -> Result<ImageInfo, String> {
    tauri::async_runtime::spawn_blocking(move || info_blocking(&data))
        .await
        .map_err(|e| e.to_string())?
}

fn info_blocking(data: &str) -> Result<ImageInfo, String> {
    let bytes = decode_b64(data)?;
    let fmt = image::guess_format(&bytes).map_err(|e| format!("识别图片格式失败: {e}"))?;
    let img = image::load_from_memory(&bytes).map_err(|e| format!("图片解码失败: {e}"))?;
    Ok(ImageInfo {
        width: img.width(),
        height: img.height(),
        format: format_name(fmt).to_string(),
        size_bytes: bytes.len(),
    })
}

/// [`plugin_image_info`] 的返回值。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
    /// 短格式名，如 `"png"`/`"jpeg"`；无法归类的已知格式给 `"其他格式"`。
    pub format: String,
    /// 原始文件体积（base64 解码后的字节数），不是解码后的像素占用。
    pub size_bytes: usize,
}

// ==================== 屏幕辅助：坐标与取色 ====================

/// 物理像素坐标（屏幕/虚拟桌面坐标系，即 Win32 API 原生使用的坐标）。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhysicalPoint {
    pub x: i32,
    pub y: i32,
}

/// 设备无关像素坐标（DIP，96 DPI 为基准；与插件页面 CSS/DOM 坐标同一量纲）。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DipPoint {
    pub x: f64,
    pub y: f64,
}

/// 物理像素矩形。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhysicalRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// DIP 矩形。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DipRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// 取色结果：十六进制串（`#rrggbb`）+ 分量。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PixelColor {
    pub hex: String,
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// 鼠标当前所在的屏幕坐标（物理像素）。不读屏幕内容，未门控。
///
/// # Errors
/// 仅 Windows 上会真正调用 Win32 API；调用失败（极少见）时返回错误信息。非 Windows 平台
/// 恒返回错误（本项目当前仅支持 Windows，见 requirements）。
#[tauri::command]
pub fn plugin_screen_cursor_point() -> Result<PhysicalPoint, String> {
    cursor_physical_point()
}

#[cfg(windows)]
fn cursor_physical_point() -> Result<PhysicalPoint, String> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut pt = POINT::default();
    // SAFETY: `pt` 是本函数栈上刚构造、生命周期覆盖此次调用的有效 POINT；GetCursorPos 只会
    // 写入这一个地址，不越界、不悬垂。
    unsafe { GetCursorPos(&mut pt) }.map_err(|e| format!("获取光标位置失败: {e}"))?;
    Ok(PhysicalPoint { x: pt.x, y: pt.y })
}

#[cfg(not(windows))]
fn cursor_physical_point() -> Result<PhysicalPoint, String> {
    Err("取光标坐标仅在 Windows 上可用".to_string())
}

/// 取屏幕指定物理像素坐标处的颜色。**需要 `screen-capture` 授权**（读屏幕内容）。
///
/// 用 GDI `GetPixel` 直接读一个像素，不做整屏截图，开销极小。
///
/// # Errors
/// - 调用会话未获插件管理里的 `screen-capture` 授权；
/// - 坐标不在任何屏幕范围内，或取色失败（GDI 返回 `CLR_INVALID`）；
/// - 非 Windows 平台恒返回错误。
#[tauri::command]
pub fn plugin_screen_pick_color_at(
    x: i32,
    y: i32,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<PixelColor, String> {
    require_capture(&webview, &registry, &settings)?;
    pick_pixel_color(x, y)
}

#[cfg(windows)]
fn pick_pixel_color(x: i32, y: i32) -> Result<PixelColor, String> {
    use windows::Win32::Graphics::Gdi::{GetDC, GetPixel, ReleaseDC, CLR_INVALID};

    // SAFETY: hwnd 传 None 表示取「整个屏幕」的设备上下文（虚拟桌面坐标系，覆盖全部显示器，
    // 含左/上方向的负坐标）。返回的 HDC 只在本函数内使用，用完立即 ReleaseDC 释放，
    // 不跨函数边界持有、不重入，符合 Win32「谁 GetDC 谁 ReleaseDC」的配对规约。
    let hdc = unsafe { GetDC(None) };
    if hdc.is_invalid() {
        return Err("获取屏幕设备上下文失败".to_string());
    }
    // SAFETY: hdc 是上面刚取到的有效屏幕 DC；GetPixel 只读该 DC 在 (x,y) 处的像素，不写、不释放。
    let color = unsafe { GetPixel(hdc, x, y) };
    // SAFETY: 与上面的 GetDC(None) 一一配对释放；hdc 在此调用之后不再被使用。
    unsafe {
        ReleaseDC(None, hdc);
    }
    if color.0 == CLR_INVALID {
        return Err(format!("坐标 ({x}, {y}) 不在任何屏幕范围内，或取色失败"));
    }
    // COLORREF 是 0x00bbggrr（小端存成 u32 时低字节到高字节依次是 r,g,b,保留字节）。
    let r = (color.0 & 0xFF) as u8;
    let g = ((color.0 >> 8) & 0xFF) as u8;
    let b = ((color.0 >> 16) & 0xFF) as u8;
    Ok(PixelColor {
        hex: format!("#{r:02x}{g:02x}{b:02x}"),
        r,
        g,
        b,
    })
}

#[cfg(not(windows))]
fn pick_pixel_color(_x: i32, _y: i32) -> Result<PixelColor, String> {
    Err("屏幕取色仅在 Windows 上可用".to_string())
}

// ==================== 屏幕辅助：DIP ↔ 物理像素 ====================

/// 单个显示器的物理矩形 + 自身缩放比（内部换算用，字段含义同
/// [`super::capture::DisplayInfo`]，为避免跨文件依赖此处独立取一份 xcap 数据）。
struct MonitorRect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    scale: f64,
    is_primary: bool,
}

/// 枚举当前所有显示器的物理矩形与缩放比。未检测到任何显示器视为环境异常，报错而非返回空集
/// ——空集会让后续换算函数在「无显示器可归属」时得到误导性的默认值。
fn list_monitors() -> Result<Vec<MonitorRect>, String> {
    let monitors = xcap::Monitor::all().map_err(|e| format!("枚举显示器失败: {e}"))?;
    if monitors.is_empty() {
        return Err("未检测到任何显示器".to_string());
    }
    Ok(monitors
        .into_iter()
        .map(|m| MonitorRect {
            x: m.x().unwrap_or(0),
            y: m.y().unwrap_or(0),
            width: m.width().unwrap_or(0),
            height: m.height().unwrap_or(0),
            scale: f64::from(m.scale_factor().unwrap_or(1.0)),
            is_primary: m.is_primary().unwrap_or(false),
        })
        .collect())
}

/// 从主屏出发，按物理边界的相邻关系推算每个显示器在 DIP 坐标系下的原点。
///
/// 算法：主屏 DIP 原点固定为其物理原点对应的 (0,0)；随后反复扫描「还未解出、但与某个
/// 已解出的显示器共享一条物理边界（左右相邻或上下相邻）」的显示器，沿该共享边界换算出
/// 它的 DIP 原点——横向拼接用被依赖屏的横向缩放推它的右/左边界，纵向拼接同理。如此
/// BFS 直到再无法推进。
///
/// **已知局限**：显示器呈对角线摆放等既不共享左右边、也不共享上下边的非常规布局下，
/// 找不到已解出的相邻屏可依赖，退化为「用该屏自身缩放对其物理原点做独立换算」——
/// 常规的左右/上下拼接多屏布局换算是精确的；非常规布局可能有像素级偏差。
fn resolve_dip_origins(mons: &[MonitorRect]) -> Vec<(f64, f64)> {
    let n = mons.len();
    let mut origin: Vec<Option<(f64, f64)>> = vec![None; n];
    let primary = mons
        .iter()
        .position(|m| m.is_primary)
        .or_else(|| mons.iter().position(|m| m.x == 0 && m.y == 0))
        .unwrap_or(0);
    origin[primary] = Some((0.0, 0.0));

    let mut progressed = true;
    while progressed {
        progressed = false;
        for i in 0..n {
            if origin[i].is_some() {
                continue;
            }
            for j in 0..n {
                if i == j {
                    continue;
                }
                let Some((dj_x, dj_y)) = origin[j] else {
                    continue;
                };
                let (mi, mj) = (&mons[i], &mons[j]);
                let resolved = if mi.x == mj.x + mj.width as i32 {
                    // i 紧贴 j 右侧
                    Some((
                        dj_x + mj.width as f64 / mj.scale,
                        dj_y + f64::from(mi.y - mj.y) / mj.scale,
                    ))
                } else if mi.x + mi.width as i32 == mj.x {
                    // i 紧贴 j 左侧
                    Some((
                        dj_x - mi.width as f64 / mi.scale,
                        dj_y + f64::from(mi.y - mj.y) / mj.scale,
                    ))
                } else if mi.y == mj.y + mj.height as i32 {
                    // i 紧贴 j 下方
                    Some((
                        dj_x + f64::from(mi.x - mj.x) / mj.scale,
                        dj_y + mj.height as f64 / mj.scale,
                    ))
                } else if mi.y + mi.height as i32 == mj.y {
                    // i 紧贴 j 上方
                    Some((
                        dj_x + f64::from(mi.x - mj.x) / mj.scale,
                        dj_y - mi.height as f64 / mi.scale,
                    ))
                } else {
                    None
                };
                if let Some(o) = resolved {
                    origin[i] = Some(o);
                    progressed = true;
                    break;
                }
            }
        }
    }
    // 兜底：孤立（非常规对角布局）显示器，用自身缩放独立换算其物理原点。
    for (i, mon) in mons.iter().enumerate() {
        if origin[i].is_none() {
            origin[i] = Some((mon.x as f64 / mon.scale, mon.y as f64 / mon.scale));
        }
    }
    origin.into_iter().map(|o| o.unwrap_or((0.0, 0.0))).collect()
}

/// 精确命中某物理点所在的显示器下标；点不在任何显示器物理矩形内（理论上不该发生，鼠标坐标
/// 恒被系统钳制在虚拟桌面内）时返回 `None`，调用方应回落到 [`nearest_monitor_physical`]。
fn monitor_at_physical(mons: &[MonitorRect], x: i32, y: i32) -> Option<usize> {
    mons.iter()
        .position(|m| x >= m.x && x < m.x + m.width as i32 && y >= m.y && y < m.y + m.height as i32)
}

/// 精确命中某 DIP 点所在的显示器下标（各显示器的 DIP 矩形由 `origins` 给出）。
fn monitor_at_dip(mons: &[MonitorRect], origins: &[(f64, f64)], x: f64, y: f64) -> Option<usize> {
    mons.iter().enumerate().position(|(i, m)| {
        let (ox, oy) = origins[i];
        let (w, h) = (m.width as f64 / m.scale, m.height as f64 / m.scale);
        x >= ox && x < ox + w && y >= oy && y < oy + h
    })
}

/// 点到「矩形边界钳制后的最近点」的欧氏距离平方，供找不到精确命中时挑「最近的显示器」用。
fn clamp_dist2(mx: f64, my: f64, mw: f64, mh: f64, x: f64, y: f64) -> f64 {
    let cx = x.clamp(mx, mx + mw);
    let cy = y.clamp(my, my + mh);
    (x - cx).powi(2) + (y - cy).powi(2)
}

fn nearest_monitor_physical(mons: &[MonitorRect], x: i32, y: i32) -> usize {
    let (x, y) = (f64::from(x), f64::from(y));
    mons.iter()
        .enumerate()
        .map(|(i, m)| (i, clamp_dist2(f64::from(m.x), f64::from(m.y), f64::from(m.width), f64::from(m.height), x, y)))
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map_or(0, |(i, _)| i)
}

fn nearest_monitor_dip(mons: &[MonitorRect], origins: &[(f64, f64)], x: f64, y: f64) -> usize {
    mons.iter()
        .enumerate()
        .map(|(i, m)| {
            let (ox, oy) = origins[i];
            (i, clamp_dist2(ox, oy, m.width as f64 / m.scale, m.height as f64 / m.scale, x, y))
        })
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map_or(0, |(i, _)| i)
}

/// 物理像素坐标 → DIP 坐标（按该点所在显示器的缩放比换算）。未门控（不读屏幕内容，只做
/// 显示器拓扑查询与算术换算）。
///
/// # Errors
/// 枚举显示器失败或当前无任何显示器（详见 [`list_monitors`]）。
#[tauri::command]
pub fn plugin_screen_to_dip(x: i32, y: i32) -> Result<DipPoint, String> {
    let mons = list_monitors()?;
    let origins = resolve_dip_origins(&mons);
    let idx = monitor_at_physical(&mons, x, y).unwrap_or_else(|| nearest_monitor_physical(&mons, x, y));
    let m = &mons[idx];
    let (ox, oy) = origins[idx];
    Ok(DipPoint {
        x: ox + f64::from(x - m.x) / m.scale,
        y: oy + f64::from(y - m.y) / m.scale,
    })
}

/// DIP 坐标 → 物理像素坐标（按该点所在显示器的缩放比换算，结果四舍五入到整数像素）。未门控。
///
/// # Errors
/// 同 [`plugin_screen_to_dip`]。
#[tauri::command]
pub fn plugin_screen_to_physical(x: f64, y: f64) -> Result<PhysicalPoint, String> {
    let mons = list_monitors()?;
    let origins = resolve_dip_origins(&mons);
    let idx = monitor_at_dip(&mons, &origins, x, y).unwrap_or_else(|| nearest_monitor_dip(&mons, &origins, x, y));
    let m = &mons[idx];
    let (ox, oy) = origins[idx];
    Ok(PhysicalPoint {
        x: (f64::from(m.x) + (x - ox) * m.scale).round() as i32,
        y: (f64::from(m.y) + (y - oy) * m.scale).round() as i32,
    })
}

/// 物理像素矩形 → DIP 矩形。原点按其所在显示器换算；宽高按同一显示器的缩放比换算
/// （若矩形跨越缩放比不同的多个显示器，这是近似值——原点所在屏的缩放占主导）。未门控。
///
/// # Errors
/// 同 [`plugin_screen_to_dip`]。
#[tauri::command]
pub fn plugin_screen_rect_to_dip(x: i32, y: i32, width: u32, height: u32) -> Result<DipRect, String> {
    let mons = list_monitors()?;
    let origins = resolve_dip_origins(&mons);
    let idx = monitor_at_physical(&mons, x, y).unwrap_or_else(|| nearest_monitor_physical(&mons, x, y));
    let m = &mons[idx];
    let (ox, oy) = origins[idx];
    Ok(DipRect {
        x: ox + f64::from(x - m.x) / m.scale,
        y: oy + f64::from(y - m.y) / m.scale,
        width: f64::from(width) / m.scale,
        height: f64::from(height) / m.scale,
    })
}

/// DIP 矩形 → 物理像素矩形。同 [`plugin_screen_rect_to_dip`] 的「原点所在屏缩放占主导」近似。
/// 未门控。
///
/// # Errors
/// 同 [`plugin_screen_to_dip`]。
#[tauri::command]
pub fn plugin_screen_rect_to_physical(x: f64, y: f64, width: f64, height: f64) -> Result<PhysicalRect, String> {
    let mons = list_monitors()?;
    let origins = resolve_dip_origins(&mons);
    let idx = monitor_at_dip(&mons, &origins, x, y).unwrap_or_else(|| nearest_monitor_dip(&mons, &origins, x, y));
    let m = &mons[idx];
    let (ox, oy) = origins[idx];
    Ok(PhysicalRect {
        x: (f64::from(m.x) + (x - ox) * m.scale).round() as i32,
        y: (f64::from(m.y) + (y - oy) * m.scale).round() as i32,
        width: (width * m.scale).max(0.0).round() as u32,
        height: (height * m.scale).max(0.0).round() as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一张 4x3 纯色 PNG，返回其 base64（内部单测数据用，不落盘）。
    fn tiny_png_b64() -> String {
        let img = image::RgbaImage::from_pixel(4, 3, image::Rgba([10, 20, 30, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("测试用纯色 PNG 编码不应失败");
        to_b64(&out.into_inner())
    }

    #[test]
    fn info_reports_real_dimensions_and_format() {
        let b64 = tiny_png_b64();
        let info = info_blocking(&b64).expect("应能解出信息");
        assert_eq!(info.width, 4);
        assert_eq!(info.height, 3);
        assert_eq!(info.format, "png");
        assert!(info.size_bytes > 0);
    }

    #[test]
    fn resize_contain_keeps_aspect_within_box() {
        let b64 = tiny_png_b64();
        let out = resize_blocking(&b64, 8, 8, "contain").expect("等比缩放应成功");
        let info = info_blocking(&out).expect("应能解出信息");
        // 4x3 等比缩到 8x8 的框内：最长边 4 贴 8，得到 8x6（不裁剪、不超框）
        assert_eq!((info.width, info.height), (8, 6));
    }

    #[test]
    fn resize_fill_matches_exact_target() {
        let b64 = tiny_png_b64();
        let out = resize_blocking(&b64, 10, 5, "fill").expect("拉伸应成功");
        let info = info_blocking(&out).expect("应能解出信息");
        assert_eq!((info.width, info.height), (10, 5));
    }

    #[test]
    fn resize_rejects_unknown_mode() {
        let b64 = tiny_png_b64();
        let err = resize_blocking(&b64, 8, 8, "nope").unwrap_err();
        assert!(err.contains("不支持的缩放模式"));
    }

    #[test]
    fn crop_out_of_bounds_is_rejected_not_silently_clamped() {
        let b64 = tiny_png_b64();
        let err = crop_blocking(&b64, 0, 0, 100, 100).unwrap_err();
        assert!(err.contains("超出图片范围"));
    }

    #[test]
    fn crop_happy_path_matches_requested_size() {
        let b64 = tiny_png_b64();
        let out = crop_blocking(&b64, 1, 1, 2, 2).expect("裁剪应成功");
        let info = info_blocking(&out).expect("应能解出信息");
        assert_eq!((info.width, info.height), (2, 2));
    }

    #[test]
    fn convert_png_to_jpeg_changes_format() {
        let b64 = tiny_png_b64();
        let out = convert_blocking(&b64, "jpeg").expect("转 jpeg 应成功");
        let info = info_blocking(&out).expect("应能解出信息");
        assert_eq!(info.format, "jpeg");
    }

    #[test]
    fn convert_rejects_unsupported_format() {
        let b64 = tiny_png_b64();
        let err = convert_blocking(&b64, "avif").unwrap_err();
        assert!(err.contains("不支持的目标格式"));
    }

    #[test]
    fn compress_rejects_non_jpeg_instead_of_silently_switching_format() {
        let b64 = tiny_png_b64();
        let err = compress_blocking(&b64, 50).unwrap_err();
        assert!(err.contains("仅支持 JPEG 源图"));
    }

    #[test]
    fn compress_jpeg_roundtrip_stays_jpeg() {
        let png_b64 = tiny_png_b64();
        let jpeg_b64 = convert_blocking(&png_b64, "jpeg").expect("转 jpeg 应成功");
        let out = compress_blocking(&jpeg_b64, 40).expect("压缩应成功");
        let info = info_blocking(&out).expect("应能解出信息");
        assert_eq!(info.format, "jpeg");
    }

    fn mon(x: i32, y: i32, width: u32, height: u32, scale: f64, is_primary: bool) -> MonitorRect {
        MonitorRect { x, y, width, height, scale, is_primary }
    }

    #[test]
    fn dip_origin_single_primary_is_zero() {
        let mons = vec![mon(0, 0, 1920, 1080, 1.5, true)];
        let origins = resolve_dip_origins(&mons);
        assert_eq!(origins[0], (0.0, 0.0));
    }

    #[test]
    fn dip_origin_side_by_side_monitors_align_at_shared_edge() {
        // 主屏 1920x1080 @150%，右边紧贴一块 1080x1920（竖屏）@100%
        let mons = vec![
            mon(0, 0, 1920, 1080, 1.5, true),
            mon(1920, 0, 1080, 1920, 1.0, false),
        ];
        let origins = resolve_dip_origins(&mons);
        assert_eq!(origins[0], (0.0, 0.0));
        // 副屏 DIP 原点 x = 主屏宽度(物理) / 主屏缩放 = 1920/1.5 = 1280
        assert!((origins[1].0 - 1280.0).abs() < 1e-9);
        assert!((origins[1].1 - 0.0).abs() < 1e-9);
    }

    #[test]
    fn physical_to_dip_and_back_roundtrips_on_primary() {
        let mons = vec![mon(0, 0, 1920, 1080, 1.5, true)];
        let origins = resolve_dip_origins(&mons);
        let idx = monitor_at_physical(&mons, 300, 150).expect("应命中主屏");
        assert_eq!(idx, 0);
        let (ox, oy) = origins[idx];
        let dip = (
            ox + f64::from(300 - mons[0].x) / mons[0].scale,
            oy + f64::from(150 - mons[0].y) / mons[0].scale,
        );
        assert!((dip.0 - 200.0).abs() < 1e-9);
        assert!((dip.1 - 100.0).abs() < 1e-9);
    }

    #[test]
    fn parse_target_format_covers_supported_and_rejects_others() {
        assert!(matches!(parse_target_format("PNG"), Ok(image::ImageFormat::Png)));
        assert!(matches!(parse_target_format("jpg"), Ok(image::ImageFormat::Jpeg)));
        assert!(matches!(parse_target_format("WebP"), Ok(image::ImageFormat::WebP)));
        assert!(matches!(parse_target_format("bmp"), Ok(image::ImageFormat::Bmp)));
        assert!(parse_target_format("gif").is_err());
    }
}

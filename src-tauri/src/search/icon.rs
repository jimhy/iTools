//! 提取文件/文件夹/.lnk 的系统图标并转成 base64(PNG)。
//!
//! 路径 → `SHGetFileInfoW`(HICON) → `GetIconInfo`/`GetDIBits`(BGRA) → RGBA → PNG → base64。
//! 任一步失败返回 `None`（前端用占位字形兜底）；GDI 句柄在每条路径上都会释放。
#![cfg(windows)]

use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use base64::Engine as _;
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
};
use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

/// 提取线程启动时调用一次：`SHGetFileInfoW` 要求先初始化 COM。
/// 幂等：已初始化(S_FALSE)/模式冲突(RPC_E_CHANGED_MODE)都无害，忽略返回值。
pub fn init_com_for_thread() {
    // SAFETY: 仅初始化当前线程的 COM apartment，参数固定合法，返回值可安全忽略。
    unsafe {
        let _ = CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32);
    }
}

/// 路径(文件/文件夹/.lnk/shell:AppsFolder 项) → base64(PNG)。失败返回 `None`。
pub fn icon_base64_png(path: &Path) -> Option<String> {
    // AppsFolder 项（UWP 等）走 IShellItemImageFactory，普通路径走 SHGetFileInfoW
    let raw = path.to_string_lossy();
    if let Some(parse_name) = raw.strip_prefix(r"shell:AppsFolder\") {
        return appsfolder_icon_base64_png(parse_name);
    }
    // Shell 只认反斜杠：把 '/' 归一为 '\\'，再补结尾 0。
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .map(|c| if c == b'/' as u16 { b'\\' as u16 } else { c })
        .chain(std::iter::once(0))
        .collect();

    // 快捷方式先解析出目标 exe 再取图标：直接对 .lnk 取会被 shell 叠上快捷方式箭头。
    // 解析失败（指向 UWP / 虚拟项等）就退回原路径——宁可带个箭头，也不能没有图标。
    let target_wide = path
        .extension()
        .filter(|e| e.eq_ignore_ascii_case("lnk"))
        .and_then(|_| resolve_shortcut_target(path))
        .map(|t| {
            t.as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect::<Vec<u16>>()
        });

    // SAFETY: wide / target_wide 都是以 0 结尾的合法宽字符串。
    unsafe {
        let hicon = match target_wide.as_deref().and_then(|w| hicon_for(w)) {
            Some(h) => h,
            None => hicon_for(&wide)?,
        };
        let rgba = hicon_to_rgba(hicon);
        DestroyIcon(hicon); // 无论成败都释放

        let (w, h, pixels) = rgba?;
        let png = encode_png(w, h, &pixels)?;
        Some(base64::engine::general_purpose::STANDARD.encode(png))
    }
}

/// 解析 `.lnk` 指向的**真实目标**（可执行文件路径）。拿不到返回 `None`。
///
/// 为什么需要它：对 `.lnk` 直接调 `SHGFI_ICON`，shell 会在图标上叠加快捷方式角标，
/// 于是主面板里每个来自开始菜单的应用都顶着一个小箭头——那是文件管理器才需要的标记，
/// 在启动器里既没有信息量又难看。改成「解析出目标 exe，再对 exe 取图标」就不会有覆盖层。
///
/// 走过的弯路（留个记号免得再试）：`SHGFI_ICONLOCATION` 看似更省事，但实测对开始菜单里的
/// 快捷方式一律返回**空**的图标源（Chrome / 7-Zip / Lumen 全部 `szDisplayName=""`），
/// 只能回退，等于没做。所以这里老老实实用 `IShellLinkW` 解析。
fn resolve_shortcut_target(path: &Path) -> Option<std::path::PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: COM 已由 init_com_for_thread 在本线程初始化；wide 以 0 结尾；
    // buf 按 GetPath 契约提供可写缓冲，pfd 传 null 表示不需要 WIN32_FIND_DATAW。
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let persist: IPersistFile = link.cast().ok()?;
        persist.Load(PCWSTR(wide.as_ptr()), STGM_READ).ok()?;
        let mut buf = [0u16; 260];
        link.GetPath(&mut buf, ptr::null_mut(), 0).ok()?;
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        if len == 0 {
            return None; // 指向 UWP / 虚拟项等没有文件路径的目标
        }
        Some(std::path::PathBuf::from(OsString::from_wide(&buf[..len])))
    }
}

/// 对给定宽字符串路径取系统图标（32x32）。
///
/// # Safety
/// `wide` 必须以 0 结尾。返回的 `HICON` 所有权移交调用方，由其 `DestroyIcon`。
unsafe fn hicon_for(wide: &[u16]) -> Option<HICON> {
    let mut shfi: SHFILEINFOW = std::mem::zeroed();
    let ok = SHGetFileInfoW(
        wide.as_ptr(),
        0,
        &mut shfi,
        std::mem::size_of::<SHFILEINFOW>() as u32,
        SHGFI_ICON | SHGFI_LARGEICON,
    );
    if ok == 0 || shfi.hIcon.is_null() {
        return None;
    }
    Some(shfi.hIcon)
}

/// AppsFolder 项（按解析名）→ base64(PNG)。
/// GetImage 返回 32bpp 直通 alpha 的 DIB（实测非预乘），直接换通道写 PNG。
fn appsfolder_icon_base64_png(parse_name: &str) -> Option<String> {
    let hbm = super::apps_folder::shell_item_icon(parse_name, 32).ok()?;
    // windows 0.61 HBITMAP.0 与 windows-sys 的 HBITMAP 同为 *mut c_void
    let raw = hbm.0 as HBITMAP;
    // SAFETY: raw 为 GetImage 移交所有权的有效位图，用后立即释放。
    let out = unsafe {
        let converted = dib_to_rgba(raw);
        DeleteObject(raw as HGDIOBJ);
        converted
    };
    let (w, h, rgba) = out?;
    let png = encode_png(w, h, &rgba)?;
    Some(base64::engine::general_purpose::STANDARD.encode(png))
}

/// 32bpp DIB HBITMAP（直通 alpha）→ (w, h, RGBA)。内部负责释放 DC（不释放位图）。
///
/// # Safety
/// `hbm` 必须是有效的 32bpp 位图且未被 select 进任何 DC。
unsafe fn dib_to_rgba(hbm: HBITMAP) -> Option<(u32, u32, Vec<u8>)> {
    let mut bmp: BITMAP = std::mem::zeroed();
    let n = GetObjectW(
        hbm as HGDIOBJ,
        std::mem::size_of::<BITMAP>() as i32,
        &mut bmp as *mut BITMAP as *mut core::ffi::c_void,
    );
    if n == 0 || bmp.bmWidth <= 0 || bmp.bmHeight <= 0 {
        return None;
    }
    let (width, height) = (bmp.bmWidth as u32, bmp.bmHeight as u32);

    let hdc: HDC = CreateCompatibleDC(ptr::null_mut());
    if hdc.is_null() {
        return None;
    }
    let buf = read_dib32(hdc, hbm, width, height);
    DeleteDC(hdc);

    let mut rgba = buf?;
    // 个别老式图标 alpha 全零：整体置为不透明兜底
    let has_alpha = rgba.chunks_exact(4).any(|p| p[3] != 0);
    if !has_alpha {
        rgba.chunks_exact_mut(4).for_each(|px| px[3] = 255);
    }
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2); // BGRA -> RGBA
    }
    Some((width, height, rgba))
}

/// HICON → (w, h, RGBA)。内部负责释放位图与 DC。
///
/// # Safety
/// `hicon` 必须是有效图标句柄；调用方负责在之后 `DestroyIcon`。
unsafe fn hicon_to_rgba(hicon: HICON) -> Option<(u32, u32, Vec<u8>)> {
    let mut ii: ICONINFO = std::mem::zeroed();
    if GetIconInfo(hicon, &mut ii) == 0 {
        return None;
    }
    let hbm_color = ii.hbmColor;
    let hbm_mask = ii.hbmMask;

    // 闭包收敛 early-return，末尾统一释放两个 HBITMAP。
    let out = (|| {
        if hbm_color.is_null() {
            return None; // 极老单色图标(仅 mask)不处理，交占位兜底
        }
        let mut bmp: BITMAP = std::mem::zeroed();
        let n = GetObjectW(
            hbm_color as HGDIOBJ,
            std::mem::size_of::<BITMAP>() as i32,
            &mut bmp as *mut BITMAP as *mut core::ffi::c_void,
        );
        if n == 0 || bmp.bmWidth <= 0 || bmp.bmHeight <= 0 {
            return None;
        }
        let (width, height) = (bmp.bmWidth as u32, bmp.bmHeight as u32);

        let hdc: HDC = CreateCompatibleDC(ptr::null_mut());
        if hdc.is_null() {
            return None;
        }

        let mut color = read_dib32(hdc, hbm_color, width, height);
        // 彩色位图无 alpha(全 0) → 用 AND 掩码合成 alpha
        if let Some(ref mut buf) = color {
            let has_alpha = buf.chunks_exact(4).any(|p| p[3] != 0);
            if !has_alpha {
                match read_dib32(hdc, hbm_mask, width, height) {
                    Some(mask) => {
                        for (px, m) in buf.chunks_exact_mut(4).zip(mask.chunks_exact(4)) {
                            px[3] = if m[0] == 0 { 255 } else { 0 }; // 0=不透明
                        }
                    }
                    None => buf.chunks_exact_mut(4).for_each(|px| px[3] = 255),
                }
            }
        }
        DeleteDC(hdc);

        let mut rgba = color?;
        for px in rgba.chunks_exact_mut(4) {
            px.swap(0, 2); // BGRA -> RGBA
        }
        Some((width, height, rgba))
    })();

    if !hbm_color.is_null() {
        DeleteObject(hbm_color as HGDIOBJ);
    }
    if !hbm_mask.is_null() {
        DeleteObject(hbm_mask as HGDIOBJ);
    }
    out
}

/// GetDIBits：HBITMAP → 32bpp top-down BGRA
///
/// # Safety
/// `hdc` 为有效内存 DC，`hbm` 为有效位图且未被 select 进任何 DC。
unsafe fn read_dib32(hdc: HDC, hbm: HBITMAP, width: u32, height: u32) -> Option<Vec<u8>> {
    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = width as i32;
    bi.bmiHeader.biHeight = -(height as i32); // 负 = top-down
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB;

    let mut buf = vec![0u8; width as usize * height as usize * 4];
    let scanned = GetDIBits(
        hdc,
        hbm,
        0,
        height,
        buf.as_mut_ptr() as *mut core::ffi::c_void,
        &mut bi,
        DIB_RGB_COLORS,
    );
    if scanned == 0 {
        None
    } else {
        Some(buf)
    }
}

/// RGBA → PNG（png crate；Writer 在 finish()/Drop 时写 IEND）
fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut enc = png::Encoder::new(&mut out, width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().ok()?;
    w.write_image_data(rgba).ok()?;
    w.finish().ok()?;
    Some(out)
}

//! 枚举 `shell:AppsFolder`（FOLDERID_AppsFolder）——系统里所有应用的权威清单，
//! 含 UWP/MSIX、注册了 AUMID 的 Win32、路径型 Win32。启动方式统一为
//! `explorer.exe "shell:AppsFolder\<解析名>"`（与 `launch_detached` 天然兼容）。
#![cfg(windows)]

use std::ffi::c_void;

use windows::core::{Interface, HSTRING, PWSTR};
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::HBITMAP;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOLDERID_AppsFolder, IEnumShellItems, IShellItem, IShellItemImageFactory,
    SHCreateItemFromParsingName, SHGetKnownFolderItem, KF_FLAG_DEFAULT, SIGDN,
    SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING, SIIGBF_ICONONLY,
};

/// GetDisplayName 返回 CoTaskMem 分配的 PWSTR，必须 CoTaskMemFree
///
/// # Safety
/// `item` 为有效的 IShellItem，须在已初始化 COM 的线程上调用。
unsafe fn sigdn_string(item: &IShellItem, sigdn: SIGDN) -> Option<String> {
    let pw: PWSTR = item.GetDisplayName(sigdn).ok()?;
    let s = pw.to_string().ok();
    CoTaskMemFree(Some(pw.0 as *const c_void));
    s
}

/// 枚举 AppsFolder，返回 (本地化显示名, AUMID/解析名)。失败返回空。
/// 须在已 `CoInitializeEx` 的线程上调用（约 400ms/360 项，放后台线程）。
pub fn enum_apps_folder() -> Vec<(String, String)> {
    enum_inner().unwrap_or_default()
}

fn enum_inner() -> windows::core::Result<Vec<(String, String)>> {
    let mut out = Vec::with_capacity(512);
    // SAFETY: 调用方保证 COM 已初始化；批量 Next 的 fetched 数量由 shell 保证
    // 不超过切片长度；每个 IShellItem 在本函数内消费完毕。
    unsafe {
        let apps: IShellItem = SHGetKnownFolderItem(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None)?;
        let enumerator: IEnumShellItems = apps.BindToHandler(None, &BHID_EnumItems)?;
        let mut batch: [Option<IShellItem>; 16] = Default::default();
        loop {
            let mut fetched = 0u32;
            // windows 0.61：Next 把 S_FALSE 也映射为 Ok，用 fetched==0 判终止
            enumerator.Next(&mut batch, Some(&mut fetched))?;
            if fetched == 0 {
                break;
            }
            for slot in batch.iter_mut().take(fetched as usize) {
                if let Some(item) = slot.take() {
                    let name = sigdn_string(&item, SIGDN_NORMALDISPLAY).unwrap_or_default();
                    let parse_name =
                        sigdn_string(&item, SIGDN_PARENTRELATIVEPARSING).unwrap_or_default();
                    if !name.is_empty() && !parse_name.is_empty() {
                        out.push((name, parse_name));
                    }
                }
            }
        }
    }
    Ok(out)
}

/// 按解析名取纯图标 HBITMAP（32bpp DIB，直通 alpha）。
/// 调用方负责 `DeleteObject` 释放；须在已初始化 COM 的线程上调用。
pub fn shell_item_icon(parse_name: &str, size: i32) -> windows::core::Result<HBITMAP> {
    // SAFETY: 解析名来自枚举结果；GetImage 由 shell 分配位图，所有权移交调用方。
    unsafe {
        let parse = format!("shell:AppsFolder\\{parse_name}");
        let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(parse.as_str()), None)?;
        let factory: IShellItemImageFactory = item.cast()?;
        factory.GetImage(SIZE { cx: size, cy: size }, SIIGBF_ICONONLY)
    }
}

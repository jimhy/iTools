use tauri::{AppHandle, Manager, WebviewWindow};

/// 全局快捷键触发：可见则隐藏，不可见则居中显示并聚焦
pub fn toggle(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        if matches!(win.is_visible(), Ok(true)) {
            let _ = win.hide();
        } else {
            show(&win);
        }
    }
}

/// 显示 + 抢占焦点（窗口位置由前端锚到屏幕上部，便于向下伸缩；前端 onFocusChanged 会自动聚焦输入框）
pub fn show(win: &WebviewWindow) {
    let _ = win.show();
    let _ = win.set_focus();
}

/// 仅应用 DWM 圆角（管理中心大窗用：不透明、无边框，靠 DWM 裁出 Win11 圆角避免四角露底）。
pub fn apply_rounded(win: &WebviewWindow) {
    #[cfg(target_os = "windows")]
    {
        apply_rounded_corners(win);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = win;
    }
}

/// 应用平台原生视觉效果：Windows 上叠加 Acrylic 毛玻璃 + DWM 圆角
pub fn apply_effects(win: &WebviewWindow, alpha: u8) {
    #[cfg(target_os = "windows")]
    {
        apply_opacity(win, alpha);
        apply_rounded_corners(win);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (win, alpha);
    }
}

/// Acrylic 底色（浅灰白）
#[cfg(target_os = "windows")]
const TINT: (u8, u8, u8) = (246, 246, 248);

/// 调整 Acrylic 毛玻璃底色透明度（alpha 越小越透，0 被钳为 1；255=纯色不透明）。
///
/// 不走 window-vibrancy::apply_acrylic——它在 Win11 22H2+（build≥22523）改走
/// DWMSBT_TRANSIENTWINDOW 且【静默忽略】颜色/alpha 参数，动态调透明度永远无效。
/// 这里直接调 SetWindowCompositionAttribute(ACCENT_ENABLE_ACRYLICBLURBEHIND)，
/// 实测（Win11 26200）幂等、可反复调用直接覆盖旧值。
pub fn apply_opacity(win: &WebviewWindow, alpha: u8) {
    #[cfg(target_os = "windows")]
    {
        if swca_apply_acrylic(win, alpha.max(1)).is_err() {
            // SWCA 属未文档化 API，将来被移除时退回 crate（有毛玻璃但 alpha 固定）
            let _ = window_vibrancy::apply_acrylic(win, Some((TINT.0, TINT.1, TINT.2, alpha)));
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (win, alpha);
    }
}

#[cfg(target_os = "windows")]
mod swca {
    use std::ffi::c_void;
    use std::sync::OnceLock;

    #[repr(C)]
    pub struct AccentPolicy {
        pub accent_state: u32,
        pub accent_flags: u32,
        /// 0xAABBGGRR
        pub gradient_color: u32,
        pub animation_id: u32,
    }

    #[repr(C)]
    pub struct WindowCompositionAttribData {
        /// 0x13 = WCA_ACCENT_POLICY
        pub attrib: u32,
        pub pv_data: *mut c_void,
        pub cb_data: usize,
    }

    pub type SwcaFn =
        unsafe extern "system" fn(*mut c_void, *mut WindowCompositionAttribData) -> i32;

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryA(name: *const u8) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }

    /// 动态解析 user32!SetWindowCompositionAttribute（未文档化，不在导入表里硬链接）
    pub fn get() -> Option<SwcaFn> {
        static CELL: OnceLock<Option<usize>> = OnceLock::new();
        let addr = *CELL.get_or_init(|| {
            // SAFETY: C 字符串字面量以 0 结尾；user32 在 GUI 进程中常驻不会被卸载。
            unsafe {
                let user32 = LoadLibraryA(c"user32.dll".as_ptr() as *const u8);
                if user32.is_null() {
                    return None;
                }
                let p = GetProcAddress(
                    user32,
                    c"SetWindowCompositionAttribute".as_ptr() as *const u8,
                );
                (!p.is_null()).then_some(p as usize)
            }
        });
        // SAFETY: 地址来自 GetProcAddress，签名与系统 ABI 一致。
        addr.map(|a| unsafe { std::mem::transmute::<usize, SwcaFn>(a) })
    }
}

/// SWCA 路径：先关掉 DWM 系统级 backdrop（避免与启动时 crate 设置的叠加双重模糊），
/// 再设 ACCENT_ENABLE_ACRYLICBLURBEHIND 带指定 tint/alpha。
#[cfg(target_os = "windows")]
fn swca_apply_acrylic(win: &WebviewWindow, alpha: u8) -> Result<(), ()> {
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMSBT_NONE, DWMWA_SYSTEMBACKDROP_TYPE,
    };

    let hwnd = win.hwnd().map_err(|_| ())?.0;
    let f = swca::get().ok_or(())?;

    let gradient = (TINT.0 as u32)
        | ((TINT.1 as u32) << 8)
        | ((TINT.2 as u32) << 16)
        | ((alpha as u32) << 24);

    // SAFETY: hwnd 有效；DWMSBT_NONE 为 i32 值，按 Win32 约定传入；
    // AccentPolicy/AttribData 均为栈上合法结构，尺寸如实填写。
    unsafe {
        let disable = DWMSBT_NONE;
        DwmSetWindowAttribute(
            hwnd as _,
            DWMWA_SYSTEMBACKDROP_TYPE as u32,
            &disable as *const _ as *const core::ffi::c_void,
            core::mem::size_of_val(&disable) as u32,
        );

        let mut policy = swca::AccentPolicy {
            accent_state: 4, // ACCENT_ENABLE_ACRYLICBLURBEHIND
            accent_flags: 0,
            gradient_color: gradient,
            animation_id: 0,
        };
        let mut data = swca::WindowCompositionAttribData {
            attrib: 0x13,
            pv_data: &mut policy as *mut _ as *mut core::ffi::c_void,
            cb_data: core::mem::size_of::<swca::AccentPolicy>(),
        };
        f(hwnd as _, &mut data);
    }
    Ok(())
}

/// 通过 DWM 让无边框窗口拥有 Win11 圆角，贴近 macOS 面板观感
#[cfg(target_os = "windows")]
fn apply_rounded_corners(win: &WebviewWindow) {
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE,
    };

    // DWMWCP_ROUND = 2：强制圆角
    const DWMWCP_ROUND: i32 = 2;

    if let Ok(hwnd) = win.hwnd() {
        let preference: i32 = DWMWCP_ROUND;
        // SAFETY: hwnd 由 Tauri 提供且窗口存活；attribute/size 与 Win32 约定一致，
        // pvAttribute 指向栈上 i32，cbAttribute 精确为其字节数。
        unsafe {
            DwmSetWindowAttribute(
                hwnd.0 as _,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                &preference as *const i32 as *const core::ffi::c_void,
                core::mem::size_of::<i32>() as u32,
            );
        }
    }
}

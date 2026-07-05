//! 原生 GDI 截图覆盖层（Windows）。取代 WebView 覆盖层以逼近 PixPin 的即时速度：
//! 冻结画面用 GDI 直接 BitBlt 显示（无 14MB 传给 webview 的开销），跨**整个虚拟桌面（所有屏）**。
//!
//! 阶段：选区（拖框）→ 编辑（悬浮工具栏 + 就地标注）→ 动作（复制/保存/贴图/OCR）。
//! 标注用 GDI 画（矩形/椭圆/箭头/直线/画笔/荧光笔/序号/马赛克；文字见 [`text`] 子逻辑，用原生 EDIT 控件）。
//! 运行在**独立线程**的 Win32 消息循环里（capture.rs 用 spawn_blocking 调用），结果经返回值回传。

#![cfg(windows)]

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    AlphaBlend, BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW,
    CreatePen, CreateSolidBrush, DeleteDC, DeleteObject, Ellipse, EndPaint, FillRect, GetDC,
    GetDIBits, GetStockObject, InvalidateRect, LineTo, MoveToEx, Polygon, Polyline, Rectangle,
    GetTextExtentPoint32W, ReleaseDC, RoundRect, ScreenToClient, SelectObject, SetBkMode, SetDIBits,
    SetStretchBltMode, SetTextColor, StretchBlt, TextOutW, AC_SRC_OVER, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
    COLORONCOLOR, DIB_RGB_COLORS, FW_BOLD, HBITMAP, HDC, HOLLOW_BRUSH, PS_SOLID, SRCCOPY,
    TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus, VK_ESCAPE};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
    GetWindowLongPtrW, LoadCursorW, PostQuitMessage, RegisterClassExW, SetCursor,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage,
    GWLP_USERDATA, HTCLIENT, HWND_TOPMOST, IDC_ARROW, IDC_CROSS, IDC_SIZEALL, IDC_SIZENESW,
    IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, MSG, SWP_SHOWWINDOW, SW_SHOW, WM_CHAR, WM_DESTROY,
    WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT,
    WM_RBUTTONDOWN, WM_SETCURSOR, WNDCLASSEXW, WS_CLIPCHILDREN, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    None,
    Rect,
    Ellipse,
    Arrow,
    Line,
    Pen,
    Marker,
    Text,
    Step,
    Mosaic,
}

#[derive(Clone)]
enum Op {
    Rect { r: RECT, c: u32, w: i32 },
    Ellipse { r: RECT, c: u32, w: i32 },
    Line { a: POINT, b: POINT, c: u32, w: i32 },
    Arrow { a: POINT, b: POINT, c: u32, w: i32 },
    Pen { pts: Vec<POINT>, c: u32, w: i32 },
    Marker { pts: Vec<POINT>, c: u32, w: i32 },
    Text { x: i32, y: i32, s: String, c: u32, size: i32 },
    Step { x: i32, y: i32, n: i32, c: u32 },
    Mosaic { r: RECT },
}

/// 选区把手/移动命中。
#[derive(Clone, Copy, PartialEq)]
enum Edge {
    N,
    S,
    E,
    W,
    Nw,
    Ne,
    Sw,
    Se,
}
#[derive(Clone, Copy)]
enum SelMode {
    Move,
    Resize(Edge),
}

/// 命中选区：返回 缩放(哪个把手) / 移动 / 无。把手取选区四边四角 ±HANDLE 像素。
fn hit_selection(sel: &RECT, p: POINT) -> Option<SelMode> {
    const H: i32 = 10;
    if p.x < sel.left - H || p.x > sel.right + H || p.y < sel.top - H || p.y > sel.bottom + H {
        return None;
    }
    let nl = (p.x - sel.left).abs() <= H;
    let nr = (p.x - sel.right).abs() <= H;
    let nt = (p.y - sel.top).abs() <= H;
    let nb = (p.y - sel.bottom).abs() <= H;
    let e = if nl && nt {
        Some(Edge::Nw)
    } else if nr && nt {
        Some(Edge::Ne)
    } else if nl && nb {
        Some(Edge::Sw)
    } else if nr && nb {
        Some(Edge::Se)
    } else if nl {
        Some(Edge::W)
    } else if nr {
        Some(Edge::E)
    } else if nt {
        Some(Edge::N)
    } else if nb {
        Some(Edge::S)
    } else {
        None
    };
    if let Some(e) = e {
        return Some(SelMode::Resize(e));
    }
    if p.x > sel.left && p.x < sel.right && p.y > sel.top && p.y < sel.bottom {
        return Some(SelMode::Move);
    }
    None
}

/// 按拖动模式算出新选区（含边界钳制、防翻转）。
fn apply_sel_drag(mode: SelMode, sm: POINT, ss: RECT, p: POINT, vw: i32, vh: i32) -> RECT {
    let dx = p.x - sm.x;
    let dy = p.y - sm.y;
    let mut r = ss;
    match mode {
        SelMode::Move => {
            let w = ss.right - ss.left;
            let h = ss.bottom - ss.top;
            r.left = (ss.left + dx).clamp(0, (vw - w).max(0));
            r.top = (ss.top + dy).clamp(0, (vh - h).max(0));
            r.right = r.left + w;
            r.bottom = r.top + h;
        }
        SelMode::Resize(e) => {
            match e {
                Edge::W | Edge::Nw | Edge::Sw => r.left = (ss.left + dx).clamp(0, r.right - 10),
                Edge::E | Edge::Ne | Edge::Se => r.right = (ss.right + dx).clamp(r.left + 10, vw),
                _ => {}
            }
            match e {
                Edge::N | Edge::Nw | Edge::Ne => r.top = (ss.top + dy).clamp(0, r.bottom - 10),
                Edge::S | Edge::Sw | Edge::Se => r.bottom = (ss.bottom + dy).clamp(r.top + 10, vh),
                _ => {}
            }
        }
    }
    r
}

/// 把手对应的鼠标光标资源。
fn cursor_for_edge(e: Edge) -> windows_sys::core::PCWSTR {
    match e {
        Edge::Nw | Edge::Se => IDC_SIZENWSE,
        Edge::Ne | Edge::Sw => IDC_SIZENESW,
        Edge::E | Edge::W => IDC_SIZEWE,
        Edge::N | Edge::S => IDC_SIZENS,
    }
}

const COLORS: [u32; 8] = [
    rgb(255, 59, 48),   // 红
    rgb(255, 149, 0),   // 橙
    rgb(255, 204, 0),   // 黄
    rgb(52, 199, 89),   // 绿
    rgb(76, 157, 255),  // 蓝
    rgb(175, 82, 222),  // 紫
    rgb(255, 255, 255), // 白
    rgb(26, 26, 26),    // 黑
];
const TOOLS: [Tool; 9] = [
    Tool::Rect,
    Tool::Ellipse,
    Tool::Arrow,
    Tool::Line,
    Tool::Pen,
    Tool::Marker,
    Tool::Text,
    Tool::Step,
    Tool::Mosaic,
];

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

/// 工具栏可点项。
#[derive(Clone)]
enum ItemKind {
    Tool(Tool),
    Color(u32),
    WidthDec,
    WidthInc,
    Undo,
    Action(&'static str),
}
struct Item {
    r: RECT,
    kind: ItemKind,
}

struct State {
    hwnd: HWND,
    vx: i32, // 虚拟桌面左上（抓屏源坐标，可为负）
    vy: i32,
    vw: i32,
    vh: i32,
    // 阶段
    editing: bool,
    dragging: bool, // 选区拖动中
    started: bool,
    sx: i32,
    sy: i32,
    sel: Option<RECT>,
    sel_drag: Option<(SelMode, POINT, RECT)>, // 移动/缩放选区中：(模式, 起始鼠标, 起始选区)
    // 编辑
    tool: Tool,
    color: u32,
    width: i32,
    ops: Vec<Op>,
    drawing: bool,       // 标注绘制中
    temp: Option<Op>,    // 进行中的标注
    dpts: Vec<POINT>,    // 画笔/荧光笔累积点
    dstart: POINT,       // 形状起点
    step_n: i32,
    // 文字输入
    edit_hwnd: HWND,
    text_pos: POINT,
    // 结果
    action: Option<&'static str>,
    cancelled: bool,
    // 工具栏命中区
    items: Vec<Item>,
    tb: RECT,
    hover: i32, // 悬停的工具栏项索引（-1 无）
    // 缓存 GDI
    back_dc: HDC,
    back_bmp: HBITMAP,
    black_dc: HDC,
    black_bmp: HBITMAP,
    frozen_dc: HDC,
    frozen_bmp: HBITMAP,
}

fn make_bmi(w: i32, h: i32) -> BITMAPINFO {
    let mut bmi: BITMAPINFO = unsafe { std::mem::zeroed() };
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = w;
    bmi.bmiHeader.biHeight = -h; // top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB as u32;
    bmi
}

fn norm_rect(x1: i32, y1: i32, x2: i32, y2: i32) -> RECT {
    RECT {
        left: x1.min(x2),
        top: y1.min(y2),
        right: x1.max(x2),
        bottom: y1.max(y2),
    }
}

/// 建后台缓冲/暗化源/冻结位图，并**直接从屏幕 BitBlt 抓当前桌面到冻结位图**（一次拷贝，省去
/// 之前 GetDIBits→Vec→SetDIBits 的三次搬运）。务必在窗口**显示前**调用，否则会把覆盖层自身抓进去。
unsafe fn init_buffers(st: &mut State) {
    if !st.back_dc.is_null() {
        return;
    }
    let sdc = GetDC(std::ptr::null_mut()); // 整个虚拟桌面的屏幕 DC
    st.back_dc = CreateCompatibleDC(sdc);
    st.back_bmp = CreateCompatibleBitmap(sdc, st.vw, st.vh);
    SelectObject(st.back_dc, st.back_bmp as _);
    // 黑色 1x1（AlphaBlend 暗化源）
    st.black_dc = CreateCompatibleDC(sdc);
    let bb = CreateCompatibleBitmap(sdc, 1, 1);
    SelectObject(st.black_dc, bb as _);
    let mut bmi1 = make_bmi(1, 1);
    let black = [0u8, 0, 0, 255];
    SetDIBits(st.black_dc, bb, 0, 1, black.as_ptr() as *const _, &mut bmi1, DIB_RGB_COLORS);
    st.black_bmp = bb;
    // 冻结画面位图：直接 BitBlt 屏幕（此刻覆盖层还没显示 → 抓的是干净桌面）
    st.frozen_dc = CreateCompatibleDC(sdc);
    let fb = CreateCompatibleBitmap(sdc, st.vw, st.vh);
    SelectObject(st.frozen_dc, fb as _);
    BitBlt(st.frozen_dc, 0, 0, st.vw, st.vh, sdc, st.vx, st.vy, SRCCOPY);
    st.frozen_bmp = fb;
    ReleaseDC(std::ptr::null_mut(), sdc);
}

/// 在 dc 上画一个标注（dx,dy 为坐标偏移：显示为 0，合成为 -sel 左上）。
unsafe fn draw_op(dc: HDC, op: &Op, dx: i32, dy: i32, frozen_dc: HDC) {
    match op {
        Op::Rect { r, c, w } | Op::Ellipse { r, c, w } => {
            let pen = CreatePen(PS_SOLID as i32, *w, *c);
            let op_pen = SelectObject(dc, pen as _);
            let op_br = SelectObject(dc, GetStockObject(HOLLOW_BRUSH));
            let (l, t, ri, b) = (r.left + dx, r.top + dy, r.right + dx, r.bottom + dy);
            if matches!(op, Op::Rect { .. }) {
                Rectangle(dc, l, t, ri, b);
            } else {
                Ellipse(dc, l, t, ri, b);
            }
            SelectObject(dc, op_pen);
            SelectObject(dc, op_br);
            DeleteObject(pen as _);
        }
        Op::Line { a, b, c, w } => {
            let pen = CreatePen(PS_SOLID as i32, *w, *c);
            let op_pen = SelectObject(dc, pen as _);
            MoveToEx(dc, a.x + dx, a.y + dy, std::ptr::null_mut());
            LineTo(dc, b.x + dx, b.y + dy);
            SelectObject(dc, op_pen);
            DeleteObject(pen as _);
        }
        Op::Arrow { a, b, c, w } => {
            let pen = CreatePen(PS_SOLID as i32, *w, *c);
            let op_pen = SelectObject(dc, pen as _);
            let (ax, ay, bx, by) = (a.x + dx, a.y + dy, b.x + dx, b.y + dy);
            MoveToEx(dc, ax, ay, std::ptr::null_mut());
            LineTo(dc, bx, by);
            // 箭头（实心三角）
            let ang = (by as f64 - ay as f64).atan2(bx as f64 - ax as f64);
            let head = 10.0 + *w as f64 * 2.5;
            let spread = 0.42;
            let p1 = POINT { x: bx, y: by };
            let p2 = POINT {
                x: (bx as f64 - head * (ang - spread).cos()) as i32,
                y: (by as f64 - head * (ang - spread).sin()) as i32,
            };
            let p3 = POINT {
                x: (bx as f64 - head * (ang + spread).cos()) as i32,
                y: (by as f64 - head * (ang + spread).sin()) as i32,
            };
            let br = CreateSolidBrush(*c);
            let op_br = SelectObject(dc, br as _);
            let pts = [p1, p2, p3];
            Polygon(dc, pts.as_ptr(), 3);
            SelectObject(dc, op_br);
            DeleteObject(br as _);
            SelectObject(dc, op_pen);
            DeleteObject(pen as _);
        }
        Op::Pen { pts, c, w } | Op::Marker { pts, c, w } => {
            if pts.len() < 2 {
                return;
            }
            let lw = if matches!(op, Op::Marker { .. }) { *w * 4 } else { *w };
            let pen = CreatePen(PS_SOLID as i32, lw, *c);
            let op_pen = SelectObject(dc, pen as _);
            let shifted: Vec<POINT> = pts.iter().map(|p| POINT { x: p.x + dx, y: p.y + dy }).collect();
            Polyline(dc, shifted.as_ptr(), shifted.len() as i32);
            SelectObject(dc, op_pen);
            DeleteObject(pen as _);
        }
        Op::Text { x, y, s, c, size } => {
            let font = CreateFontW(
                *size, 0, 0, 0, FW_BOLD as i32, 0, 0, 0, 0, 0, 0, 0, 0,
                wide("Microsoft YaHei").as_ptr(),
            );
            let of = SelectObject(dc, font as _);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, *c);
            for (i, line) in s.split('\n').enumerate() {
                let ws = wide(line);
                TextOutW(
                    dc, *x + dx, *y + dy + i as i32 * (*size as f64 * 1.25) as i32,
                    ws.as_ptr(), (ws.len() - 1) as i32,
                );
            }
            SelectObject(dc, of);
            DeleteObject(font as _);
        }
        Op::Step { x, y, n, c } => {
            let rr = 14;
            let br = CreateSolidBrush(*c);
            let ob = SelectObject(dc, br as _);
            let pen = CreatePen(PS_SOLID as i32, 1, *c);
            let op = SelectObject(dc, pen as _);
            Ellipse(dc, x + dx - rr, y + dy - rr, x + dx + rr, y + dy + rr);
            let font = CreateFontW(
                22, 0, 0, 0, FW_BOLD as i32, 0, 0, 0, 0, 0, 0, 0, 0, wide("Arial").as_ptr(),
            );
            let of = SelectObject(dc, font as _);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, if *c == rgb(255, 255, 255) { rgb(0, 0, 0) } else { rgb(255, 255, 255) });
            let ws = wide(&n.to_string());
            // 粗略居中
            TextOutW(dc, x + dx - 6 * ws.len() as i32 / 2, y + dy - 11, ws.as_ptr(), (ws.len() - 1) as i32);
            SelectObject(dc, of);
            DeleteObject(font as _);
            SelectObject(dc, op);
            DeleteObject(pen as _);
            SelectObject(dc, ob);
            DeleteObject(br as _);
        }
        Op::Mosaic { r } => {
            // 从冻结画面把该区域缩小再放大 → 像素化
            let (l, t, w, h) = (r.left, r.top, r.right - r.left, r.bottom - r.top);
            if w < 4 || h < 4 {
                return;
            }
            let block = 10;
            let sw = (w / block).max(1);
            let sh = (h / block).max(1);
            SetStretchBltMode(dc, COLORONCOLOR as i32);
            // 源坐标是 frozen_dc 的绝对坐标（client），目标含 dx,dy 偏移
            StretchBlt(dc, l + dx, t + dy, w, h, frozen_dc, l, t, sw, sh, SRCCOPY);
            let _ = (sw, sh);
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 布局工具栏（editing 时）：紧贴选区下方（放不下则上方），生成命中区。
unsafe fn layout_toolbar(st: &mut State) {
    st.items.clear();
    let sel = match st.sel {
        Some(s) => s,
        None => return,
    };
    let btn = 34i32;
    let gap = 3i32;
    let pad = 8i32;
    // 工具 | 颜色 | 宽度± | 撤销 | 动作
    let mut items: Vec<ItemKind> = Vec::new();
    for t in TOOLS {
        items.push(ItemKind::Tool(t));
    }
    for c in COLORS {
        items.push(ItemKind::Color(c));
    }
    items.push(ItemKind::WidthDec);
    items.push(ItemKind::WidthInc);
    items.push(ItemKind::Undo);
    let actions = ["ocr", "pin", "save", "copy"];
    for a in actions {
        items.push(ItemKind::Action(a));
    }
    // 每项宽度：动作按钮宽些
    let width_of = |k: &ItemKind| -> i32 {
        match k {
            ItemKind::Action(_) => 54,
            ItemKind::Color(_) => 26,
            _ => btn,
        }
    };
    let total: i32 = pad * 2 + items.iter().map(|k| width_of(k) + gap).sum::<i32>() - gap;
    let h = btn + pad * 2;
    let mut x = sel.left;
    if x + total > st.vw {
        x = (st.vw - total).max(0);
    }
    let mut y = sel.bottom + 8;
    if y + h > st.vh {
        y = (sel.top - h - 8).max(0);
    }
    st.tb = RECT { left: x, top: y, right: x + total, bottom: y + h };
    let mut cx = x + pad;
    let cy = y + pad;
    for k in items {
        let w = width_of(&k);
        let r = RECT { left: cx, top: cy, right: cx + w, bottom: cy + btn };
        st.items.push(Item { r, kind: k });
        cx += w + gap;
    }
}

unsafe fn fill_rect(dc: HDC, r: &RECT, color: u32) {
    let br = CreateSolidBrush(color);
    FillRect(dc, r, br as _);
    DeleteObject(br as _);
}

/// 圆角实心填充（无描边）。
unsafe fn round_fill(dc: HDC, r: &RECT, color: u32, rad: i32) {
    let br = CreateSolidBrush(color);
    let ob = SelectObject(dc, br as _);
    let pen = CreatePen(PS_SOLID as i32, 1, color);
    let op = SelectObject(dc, pen as _);
    RoundRect(dc, r.left, r.top, r.right, r.bottom, rad, rad);
    SelectObject(dc, op);
    SelectObject(dc, ob);
    DeleteObject(pen as _);
    DeleteObject(br as _);
}

/// 在矩形内居中画文字。
unsafe fn draw_label(dc: HDC, r: &RECT, s: &str, color: u32, size: i32, font_name: &str) {
    let font = CreateFontW(size, 0, 0, 0, FW_BOLD as i32, 0, 0, 0, 0, 0, 0, 0, 0, wide(font_name).as_ptr());
    let of = SelectObject(dc, font as _);
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, color);
    let ws = wide(s);
    let n = (ws.len() - 1) as i32;
    let mut sz = windows_sys::Win32::Foundation::SIZE { cx: 0, cy: 0 };
    GetTextExtentPoint32W(dc, ws.as_ptr(), n, &mut sz);
    let tx = r.left + ((r.right - r.left) - sz.cx) / 2;
    let ty = r.top + ((r.bottom - r.top) - sz.cy) / 2;
    TextOutW(dc, tx, ty, ws.as_ptr(), n);
    SelectObject(dc, of);
    DeleteObject(font as _);
}

/// 画工具栏图标（矢量线条，col 为线色）。
unsafe fn draw_tool_icon(dc: HDC, r: &RECT, t: Tool, col: u32) {
    let pen = CreatePen(PS_SOLID as i32, 2, col);
    let op = SelectObject(dc, pen as _);
    let ob = SelectObject(dc, GetStockObject(HOLLOW_BRUSH));
    let (l, t2, ri, b) = (r.left + 8, r.top + 8, r.right - 8, r.bottom - 8);
    match t {
        Tool::Rect => {
            Rectangle(dc, l, t2, ri, b);
        }
        Tool::Ellipse => {
            Ellipse(dc, l, t2, ri, b);
        }
        Tool::Arrow => {
            MoveToEx(dc, l, b, std::ptr::null_mut());
            LineTo(dc, ri, t2);
            MoveToEx(dc, ri, t2, std::ptr::null_mut());
            LineTo(dc, ri - 5, t2 + 1);
            MoveToEx(dc, ri, t2, std::ptr::null_mut());
            LineTo(dc, ri - 1, t2 + 5);
        }
        Tool::Line => {
            MoveToEx(dc, l, b, std::ptr::null_mut());
            LineTo(dc, ri, t2);
        }
        Tool::Pen => {
            MoveToEx(dc, l, b, std::ptr::null_mut());
            LineTo(dc, (l + ri) / 2, t2);
            LineTo(dc, ri, b);
        }
        Tool::Marker => {
            let pen2 = CreatePen(PS_SOLID as i32, 5, col);
            let o2 = SelectObject(dc, pen2 as _);
            MoveToEx(dc, l, b, std::ptr::null_mut());
            LineTo(dc, ri, t2);
            SelectObject(dc, o2);
            DeleteObject(pen2 as _);
        }
        Tool::Text => {
            let font = CreateFontW(16, 0, 0, 0, FW_BOLD as i32, 0, 0, 0, 0, 0, 0, 0, 0, wide("Arial").as_ptr());
            let of = SelectObject(dc, font as _);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, col);
            let ws = wide("T");
            TextOutW(dc, l + 3, t2 - 2, ws.as_ptr(), 1);
            SelectObject(dc, of);
            DeleteObject(font as _);
        }
        Tool::Step => {
            Ellipse(dc, l, t2, ri, b);
        }
        Tool::Mosaic => {
            for i in 0..3 {
                for j in 0..3 {
                    if (i + j) % 2 == 0 {
                        let rr = RECT {
                            left: l + i * (ri - l) / 3,
                            top: t2 + j * (b - t2) / 3,
                            right: l + (i + 1) * (ri - l) / 3,
                            bottom: t2 + (j + 1) * (b - t2) / 3,
                        };
                        fill_rect(dc, &rr, col);
                    }
                }
            }
        }
        Tool::None => {}
    }
    SelectObject(dc, op);
    SelectObject(dc, ob);
    DeleteObject(pen as _);
}

unsafe fn draw_toolbar(st: &State, dc: HDC) {
    let accent = rgb(76, 157, 255);
    let hov_bg = rgb(54, 57, 63);
    // 背板：深色圆角 + 细边框
    round_fill(dc, &st.tb, rgb(37, 38, 43), 16);
    let pen = CreatePen(PS_SOLID as i32, 1, rgb(58, 60, 66));
    let opn = SelectObject(dc, pen as _);
    let ob0 = SelectObject(dc, GetStockObject(HOLLOW_BRUSH));
    RoundRect(dc, st.tb.left, st.tb.top, st.tb.right, st.tb.bottom, 16, 16);
    SelectObject(dc, opn);
    SelectObject(dc, ob0);
    DeleteObject(pen as _);

    let mut prev_group = 0i32;
    let group_of = |k: &ItemKind| match k {
        ItemKind::Tool(_) => 0,
        ItemKind::Color(_) => 1,
        ItemKind::WidthDec | ItemKind::WidthInc => 2,
        ItemKind::Undo => 3,
        ItemKind::Action(_) => 4,
    };
    for (i, it) in st.items.iter().enumerate() {
        let hovered = st.hover == i as i32;
        // 组间竖分隔线
        let g = group_of(&it.kind);
        if i > 0 && g != prev_group {
            let sx = it.r.left - 3;
            let vp = CreatePen(PS_SOLID as i32, 1, rgb(60, 62, 68));
            let ovp = SelectObject(dc, vp as _);
            MoveToEx(dc, sx, st.tb.top + 8, std::ptr::null_mut());
            LineTo(dc, sx, st.tb.bottom - 8);
            SelectObject(dc, ovp);
            DeleteObject(vp as _);
        }
        prev_group = g;
        match &it.kind {
            ItemKind::Tool(t) => {
                let sel = *t == st.tool;
                if sel {
                    round_fill(dc, &it.r, accent, 9);
                } else if hovered {
                    round_fill(dc, &it.r, hov_bg, 9);
                }
                let col = if sel { rgb(255, 255, 255) } else { rgb(203, 206, 212) };
                draw_tool_icon(dc, &it.r, *t, col);
            }
            ItemKind::Color(c) => {
                if hovered {
                    round_fill(dc, &it.r, hov_bg, 9);
                }
                let cx = (it.r.left + it.r.right) / 2;
                let cy = (it.r.top + it.r.bottom) / 2;
                let rr = 9;
                let border = if *c == rgb(255, 255, 255) { rgb(150, 150, 155) } else { *c };
                let br = CreateSolidBrush(*c);
                let ob = SelectObject(dc, br as _);
                let bp = CreatePen(PS_SOLID as i32, 1, border);
                let obp = SelectObject(dc, bp as _);
                Ellipse(dc, cx - rr, cy - rr, cx + rr, cy + rr);
                SelectObject(dc, ob);
                SelectObject(dc, obp);
                DeleteObject(br as _);
                DeleteObject(bp as _);
                if *c == st.color {
                    let ring = CreatePen(PS_SOLID as i32, 2, rgb(255, 255, 255));
                    let orn = SelectObject(dc, ring as _);
                    let oh = SelectObject(dc, GetStockObject(HOLLOW_BRUSH));
                    Ellipse(dc, cx - rr - 3, cy - rr - 3, cx + rr + 3, cy + rr + 3);
                    SelectObject(dc, orn);
                    SelectObject(dc, oh);
                    DeleteObject(ring as _);
                }
            }
            ItemKind::WidthDec | ItemKind::WidthInc | ItemKind::Undo => {
                if hovered {
                    round_fill(dc, &it.r, hov_bg, 9);
                }
                let label = match &it.kind {
                    ItemKind::WidthDec => "–",
                    ItemKind::WidthInc => "+",
                    _ => "↶",
                };
                draw_label(dc, &it.r, label, rgb(210, 213, 219), 20, "Segoe UI Symbol");
            }
            ItemKind::Action(a) => {
                let is_copy = *a == "copy";
                if is_copy {
                    round_fill(dc, &it.r, accent, 9);
                } else if hovered {
                    round_fill(dc, &it.r, hov_bg, 9);
                }
                let label = match *a {
                    "ocr" => "OCR",
                    "pin" => "贴图",
                    "save" => "保存",
                    _ => "复制",
                };
                let col = if is_copy { rgb(255, 255, 255) } else { rgb(214, 217, 223) };
                draw_label(dc, &it.r, label, col, 15, "Microsoft YaHei");
            }
        }
    }
}

unsafe fn paint(st: &mut State) {
    let wdc = GetDC(st.hwnd);
    init_buffers(st); // 兜底（正常已在显示前初始化过 → 直接返回）
    let back = st.back_dc;
    // 1) 冻结画面铺满
    BitBlt(back, 0, 0, st.vw, st.vh, st.frozen_dc, 0, 0, SRCCOPY);
    // 2) 暗化
    let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 110, AlphaFormat: 0 };
    AlphaBlend(back, 0, 0, st.vw, st.vh, st.black_dc, 0, 0, 1, 1, blend);
    // 3) 选区亮 + 标注 + 蓝框
    if let Some(r) = st.sel {
        let (x, y, w, h) = (r.left, r.top, r.right - r.left, r.bottom - r.top);
        if w > 0 && h > 0 {
            BitBlt(back, x, y, w, h, st.frozen_dc, x, y, SRCCOPY);
            for op in &st.ops {
                draw_op(back, op, 0, 0, st.frozen_dc);
            }
            if let Some(t) = &st.temp {
                draw_op(back, t, 0, 0, st.frozen_dc);
            }
            let pen = CreatePen(PS_SOLID as i32, 2, rgb(76, 157, 255));
            let op = SelectObject(back, pen as _);
            let ob = SelectObject(back, GetStockObject(HOLLOW_BRUSH));
            Rectangle(back, x, y, x + w, y + h);
            SelectObject(back, op);
            SelectObject(back, ob);
            DeleteObject(pen as _);
            if st.editing {
                draw_handles(back, x, y, w, h); // 8 个缩放把手
            }
        }
    }
    // 4) 工具栏
    if st.editing && !st.items.is_empty() {
        draw_toolbar(st, back);
    }
    // 5) 呈现
    BitBlt(wdc, 0, 0, st.vw, st.vh, back, 0, 0, SRCCOPY);
    ReleaseDC(st.hwnd, wdc);
}

/// 合成最终图（裁剪选区 + 标注，无暗化/无边框/无工具栏）→ RGBA + 宽高。
unsafe fn compose(st: &State) -> Option<(Vec<u8>, u32, u32)> {
    let sel = st.sel?;
    let (x, y, w, h) = (sel.left, sel.top, sel.right - sel.left, sel.bottom - sel.top);
    if w < 1 || h < 1 {
        return None;
    }
    // 用**屏幕 DC**（窗口此时可能已销毁，GetDC(hwnd) 会失效 → 全黑）
    let sdc = GetDC(std::ptr::null_mut());
    let cdc = CreateCompatibleDC(sdc);
    let cbmp = CreateCompatibleBitmap(sdc, w, h);
    let ob = SelectObject(cdc, cbmp as _);
    // 冻结裁剪
    BitBlt(cdc, 0, 0, w, h, st.frozen_dc, x, y, SRCCOPY);
    // 标注（偏移 -sel 左上）
    for op in &st.ops {
        draw_op_compose(cdc, op, -x, -y, st.frozen_dc);
    }
    // 读回像素
    let mut buf = vec![0u8; (w * h * 4) as usize];
    let mut bmi = make_bmi(w, h);
    GetDIBits(cdc, cbmp, 0, h as u32, buf.as_mut_ptr() as *mut _, &mut bmi, DIB_RGB_COLORS);
    SelectObject(cdc, ob);
    DeleteObject(cbmp as _);
    DeleteDC(cdc);
    ReleaseDC(std::ptr::null_mut(), sdc);
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2); // BGRA → RGBA
        px[3] = 0xFF;
    }
    Some((buf, w as u32, h as u32))
}

/// 合成用的 draw_op：与显示一致，但马赛克源坐标要用「绝对」（frozen_dc），目标用偏移。
unsafe fn draw_op_compose(dc: HDC, op: &Op, dx: i32, dy: i32, frozen_dc: HDC) {
    if let Op::Mosaic { r } = op {
        let (l, t, w, h) = (r.left, r.top, r.right - r.left, r.bottom - r.top);
        if w >= 4 && h >= 4 {
            let block = 10;
            let sw = (w / block).max(1);
            let sh = (h / block).max(1);
            SetStretchBltMode(dc, COLORONCOLOR as i32);
            StretchBlt(dc, l + dx, t + dy, w, h, frozen_dc, l, t, sw, sh, SRCCOPY);
        }
        return;
    }
    draw_op(dc, op, dx, dy, frozen_dc);
}

/// 画 8 个缩放把手（四角 + 四边中点）。
unsafe fn draw_handles(dc: HDC, x: i32, y: i32, w: i32, h: i32) {
    let pts = [
        (x, y),
        (x + w / 2, y),
        (x + w, y),
        (x, y + h / 2),
        (x + w, y + h / 2),
        (x, y + h),
        (x + w / 2, y + h),
        (x + w, y + h),
    ];
    let s = 4;
    let br = CreateSolidBrush(rgb(255, 255, 255));
    let ob = SelectObject(dc, br as _);
    let pen = CreatePen(PS_SOLID as i32, 1, rgb(76, 157, 255));
    let op = SelectObject(dc, pen as _);
    for (px, py) in pts {
        Rectangle(dc, px - s, py - s, px + s, py + s);
    }
    SelectObject(dc, ob);
    SelectObject(dc, op);
    DeleteObject(br as _);
    DeleteObject(pen as _);
}

fn pt(lp: LPARAM) -> POINT {
    POINT { x: (lp & 0xFFFF) as i16 as i32, y: ((lp >> 16) & 0xFFFF) as i16 as i32 }
}

fn in_rect(p: POINT, r: &RECT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

unsafe fn commit_text(st: &mut State) {
    if st.edit_hwnd.is_null() {
        return;
    }
    let txt = get_edit_text(st.edit_hwnd);
    DestroyWindow(st.edit_hwnd);
    st.edit_hwnd = std::ptr::null_mut();
    let t = txt.trim_end().to_string();
    if !t.is_empty() {
        st.ops.push(Op::Text {
            x: st.text_pos.x,
            y: st.text_pos.y,
            s: t,
            c: st.color,
            size: 14 + st.width * 3,
        });
    }
    InvalidateRect(st.hwnd, std::ptr::null(), 0);
}

unsafe fn get_edit_text(edit: HWND) -> String {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowTextLengthW, GetWindowTextW};
    let len = GetWindowTextLengthW(edit);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; (len + 1) as usize];
    let n = GetWindowTextW(edit, buf.as_mut_ptr(), len + 1);
    String::from_utf16_lossy(&buf[..n as usize])
}

unsafe fn open_text_edit(st: &mut State, p: POINT) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        ES_AUTOHSCROLL, ES_MULTILINE, WS_BORDER, WS_CHILD,
    };
    commit_text(st);
    st.text_pos = p;
    let size = 14 + st.width * 3;
    let hinst = GetModuleHandleW(std::ptr::null());
    let edit = CreateWindowExW(
        0,
        wide("EDIT").as_ptr(),
        std::ptr::null(),
        WS_CHILD | WS_VISIBLE | WS_BORDER | ES_MULTILINE as u32 | ES_AUTOHSCROLL as u32,
        p.x,
        p.y,
        200,
        (size as f64 * 1.6) as i32,
        st.hwnd,
        std::ptr::null_mut(),
        hinst,
        std::ptr::null(),
    );
    if !edit.is_null() {
        st.edit_hwnd = edit;
        SetFocus(edit);
    }
}

unsafe fn make_shape(st: &State, a: POINT, b: POINT) -> Option<Op> {
    let c = st.color;
    let w = st.width;
    Some(match st.tool {
        Tool::Rect => Op::Rect { r: norm_rect(a.x, a.y, b.x, b.y), c, w },
        Tool::Ellipse => Op::Ellipse { r: norm_rect(a.x, a.y, b.x, b.y), c, w },
        Tool::Line => Op::Line { a, b, c, w },
        Tool::Arrow => Op::Arrow { a, b, c, w },
        Tool::Mosaic => Op::Mosaic { r: norm_rect(a.x, a.y, b.x, b.y) },
        _ => return None,
    })
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let st = &mut *ptr;
    match msg {
        WM_PAINT => {
            let mut ps = std::mem::zeroed();
            BeginPaint(hwnd, &mut ps);
            paint(st);
            EndPaint(hwnd, &ps);
            0
        }
        WM_LBUTTONDOWN => {
            let p = pt(lp);
            if !st.editing {
                // 选区阶段：开始拖框
                st.dragging = true;
                st.started = false;
                st.sx = p.x;
                st.sy = p.y;
                SetCapture(hwnd);
                return 0;
            }
            // 编辑阶段：先看工具栏
            for it in &st.items {
                if in_rect(p, &it.r) {
                    let kind = it.kind.clone();
                    handle_item(st, kind);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
                }
            }
            if let Some(sel) = st.sel {
                // 选区把手/内部：缩放优先；无工具时选区内=移动；有工具时选区内=画标注
                let hit = hit_selection(&sel, p);
                let on_handle = matches!(hit, Some(SelMode::Resize(_)));
                let can_move = matches!(hit, Some(SelMode::Move)) && st.tool == Tool::None;
                if on_handle || can_move {
                    commit_text(st);
                    st.sel_drag = Some((hit.unwrap(), p, sel));
                    SetCapture(hwnd);
                    return 0;
                }
                if in_rect(p, &sel) && st.tool != Tool::None {
                    if st.tool == Tool::Text {
                        open_text_edit(st, p);
                        return 0;
                    }
                    if st.tool == Tool::Step {
                        st.ops.push(Op::Step { x: p.x, y: p.y, n: st.step_n, c: st.color });
                        st.step_n += 1;
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                        return 0;
                    }
                    commit_text(st);
                    st.drawing = true;
                    st.dstart = p;
                    st.dpts = vec![p];
                    SetCapture(hwnd);
                }
            }
            0
        }
        WM_MOUSEMOVE => {
            let p = pt(lp);
            if st.dragging {
                if !st.started && (p.x - st.sx).abs() < 3 && (p.y - st.sy).abs() < 3 {
                    return 0;
                }
                st.started = true;
                st.sel = Some(norm_rect(st.sx, st.sy, p.x, p.y));
                InvalidateRect(hwnd, std::ptr::null(), 0);
            } else if let Some((mode, sm, ss)) = st.sel_drag {
                st.sel = Some(apply_sel_drag(mode, sm, ss, p, st.vw, st.vh));
                layout_toolbar(st); // 工具栏跟随选区
                InvalidateRect(hwnd, std::ptr::null(), 0);
            } else if st.drawing {
                match st.tool {
                    Tool::Pen | Tool::Marker => {
                        st.dpts.push(p);
                        st.temp = Some(if st.tool == Tool::Pen {
                            Op::Pen { pts: st.dpts.clone(), c: st.color, w: st.width }
                        } else {
                            Op::Marker { pts: st.dpts.clone(), c: st.color, w: st.width }
                        });
                    }
                    _ => {
                        st.temp = make_shape(st, st.dstart, p);
                    }
                }
                InvalidateRect(hwnd, std::ptr::null(), 0);
            } else if st.editing {
                // 工具栏悬停高亮（变化才重绘）
                let mut hv = -1i32;
                if in_rect(p, &st.tb) {
                    for (i, it) in st.items.iter().enumerate() {
                        if in_rect(p, &it.r) {
                            hv = i as i32;
                            break;
                        }
                    }
                }
                if hv != st.hover {
                    st.hover = hv;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
            }
            0
        }
        WM_LBUTTONUP => {
            if st.dragging {
                st.dragging = false;
                ReleaseCapture();
                if let Some(r) = st.sel {
                    if (r.right - r.left) >= 6 && (r.bottom - r.top) >= 6 {
                        // 进入编辑阶段
                        st.editing = true;
                        layout_toolbar(st);
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    } else {
                        st.sel = None;
                    }
                }
            } else if st.sel_drag.is_some() {
                st.sel_drag = None;
                ReleaseCapture();
                layout_toolbar(st);
                InvalidateRect(hwnd, std::ptr::null(), 0);
            } else if st.drawing {
                st.drawing = false;
                ReleaseCapture();
                if let Some(t) = st.temp.take() {
                    st.ops.push(t);
                }
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
            0
        }
        WM_LBUTTONDBLCLK => {
            // 编辑阶段选区内双击 = 复制；Shift+双击 = 贴图
            if st.editing {
                let p = pt(lp);
                if let Some(sel) = st.sel {
                    if in_rect(p, &sel) {
                        commit_text(st);
                        let shift = (wp & 0x0004) != 0; // MK_SHIFT
                        st.action = Some(if shift { "pin" } else { "copy" });
                        DestroyWindow(hwnd);
                    }
                }
            }
            0
        }
        WM_SETCURSOR => {
            // 客户区自定义光标：把手→缩放箭头、可移动→四向、否则十字/箭头
            if (lp & 0xFFFF) as u32 == HTCLIENT {
                let mut cp = POINT { x: 0, y: 0 };
                GetCursorPos(&mut cp);
                ScreenToClient(hwnd, &mut cp);
                let idc = if st.editing {
                    match st.sel.and_then(|s| hit_selection(&s, cp)) {
                        Some(SelMode::Resize(e)) => cursor_for_edge(e),
                        Some(SelMode::Move) if st.tool == Tool::None => IDC_SIZEALL,
                        _ if st.tool == Tool::None => IDC_ARROW,
                        _ => IDC_CROSS,
                    }
                } else {
                    IDC_CROSS
                };
                SetCursor(LoadCursorW(std::ptr::null_mut(), idc));
                return 1;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_CHAR => {
            // 文字输入交给 EDIT 控件（有焦点时）；这里兜底无
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_RBUTTONDOWN => {
            if !st.edit_hwnd.is_null() {
                commit_text(st);
            } else {
                st.cancelled = true;
                DestroyWindow(hwnd);
            }
            0
        }
        WM_KEYDOWN => {
            if wp as i32 == VK_ESCAPE as i32 {
                if !st.edit_hwnd.is_null() {
                    DestroyWindow(st.edit_hwnd);
                    st.edit_hwnd = std::ptr::null_mut();
                } else {
                    st.cancelled = true;
                    DestroyWindow(hwnd);
                }
            }
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn handle_item(st: &mut State, kind: ItemKind) {
    match kind {
        ItemKind::Tool(t) => {
            commit_text(st);
            st.tool = if st.tool == t { Tool::None } else { t };
        }
        ItemKind::Color(c) => st.color = c,
        ItemKind::WidthDec => st.width = (st.width - 1).max(1),
        ItemKind::WidthInc => st.width = (st.width + 1).min(12),
        ItemKind::Undo => {
            st.ops.pop();
        }
        ItemKind::Action(a) => {
            commit_text(st);
            st.action = Some(a);
            DestroyWindow(st.hwnd);
        }
    }
}

/// 覆盖层结果：动作名 + 合成后的 RGBA + 宽高。
pub struct NativeResult {
    pub action: String,
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
}

/// 在**当前线程**跑覆盖层消息循环。vx/vy/vw/vh：虚拟桌面矩形（覆盖层自己 BitBlt 抓屏，无需外部传图）。
/// full=true 时开局即选中整屏进入编辑。取消返回 None。
pub fn run(vx: i32, vy: i32, vw: i32, vh: i32, full: bool) -> Option<NativeResult> {
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let cls = wide("ITiToolsCaptureOverlay");
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.style = 0x0008; // CS_DBLCLKS
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_CROSS);
        wc.lpszClassName = cls.as_ptr();
        RegisterClassExW(&wc);

        // 先建**隐藏**窗（无 WS_VISIBLE）：这样 init_buffers 抓屏时不会把覆盖层自己抓进去
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST,
            cls.as_ptr(),
            std::ptr::null(),
            WS_POPUP | WS_CLIPCHILDREN,
            vx,
            vy,
            vw,
            vh,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return None;
        }
        let mut st = Box::new(State {
            hwnd,
            vx,
            vy,
            vw,
            vh,
            editing: false,
            dragging: false,
            started: false,
            sx: 0,
            sy: 0,
            sel: None,
            sel_drag: None,
            tool: Tool::None,
            color: COLORS[0],
            width: 3,
            ops: Vec::new(),
            drawing: false,
            temp: None,
            dpts: Vec::new(),
            dstart: POINT { x: 0, y: 0 },
            step_n: 1,
            edit_hwnd: std::ptr::null_mut(),
            text_pos: POINT { x: 0, y: 0 },
            action: None,
            cancelled: false,
            items: Vec::new(),
            tb: RECT { left: 0, top: 0, right: 0, bottom: 0 },
            hover: -1,
            back_dc: std::ptr::null_mut(),
            back_bmp: std::ptr::null_mut(),
            black_dc: std::ptr::null_mut(),
            black_bmp: std::ptr::null_mut(),
            frozen_dc: std::ptr::null_mut(),
            frozen_bmp: std::ptr::null_mut(),
        });
        if full {
            st.sel = Some(RECT { left: 0, top: 0, right: vw, bottom: vh });
            st.editing = true;
            layout_toolbar(&mut st);
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, &mut *st as *mut State as isize);
        init_buffers(&mut st); // 窗口仍隐藏时抓屏 → 干净桌面
        SetWindowPos(hwnd, HWND_TOPMOST, vx, vy, vw, vh, SWP_SHOWWINDOW);
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        InvalidateRect(hwnd, std::ptr::null(), 0);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let out = if st.cancelled || st.action.is_none() {
            None
        } else {
            let action = st.action.unwrap().to_string();
            compose(&st).map(|(rgba, w, h)| NativeResult { action, rgba, w, h })
        };
        // 清理 GDI
        for (dc, bmp) in [
            (st.back_dc, st.back_bmp),
            (st.black_dc, st.black_bmp),
            (st.frozen_dc, st.frozen_bmp),
        ] {
            if !dc.is_null() {
                DeleteDC(dc);
                DeleteObject(bmp as _);
            }
        }
        let _ = IDC_ARROW;
        out
    }
}

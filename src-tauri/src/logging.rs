//! 轻量文件日志：把运行期日志写到 **exe 同目录**下的 `itools.log`，便于打 debug 版给海风哥
//! 测试时就地查看日志反馈问题；同时保留 stderr 输出（dev 控制台仍可见）。
//!
//! 用法：`ilog!("[iTools] ... {err}")`，格式化语法同 `eprintln!`。启动最早期调用一次 `init()`。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

static LOG_FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();

/// 初始化日志文件：在 exe 同目录下以追加方式打开 `itools.log`。
/// 目录不可写（如装进 Program Files）时静默回退——仅输出 stderr，不影响启动。
pub fn init() {
    // release 版不落文件日志：文件日志本意只给 debug/测试版（见模块头注释）。
    // 直接返回后 LOG_FILE 保持未初始化，write() 里 LOG_FILE.get() 恒为 None，
    // 所有 ilog! 只走 stderr（GUI release 下无控制台，等于静默，不产 itools.log）。
    if !cfg!(debug_assertions) {
        return;
    }
    let file = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("itools.log")))
        .and_then(|path| OpenOptions::new().create(true).append(true).open(path).ok());
    let _ = LOG_FILE.set(Mutex::new(file));
    write(format_args!("[iTools] ==== 日志启动 ===="));
}

/// 写一行日志：加本地时间戳，落 `itools.log` 并回显 stderr。
pub fn write(args: std::fmt::Arguments) {
    let line = format!(
        "[{}] {}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
        args
    );
    eprintln!("{line}");
    if let Some(cell) = LOG_FILE.get() {
        if let Ok(mut guard) = cell.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = writeln!(f, "{line}");
                let _ = f.flush();
            }
        }
    }
}

/// 运行期日志宏，语法同 `eprintln!`：`ilog!("[iTools] xxx {err}")`。
macro_rules! ilog {
    ($($arg:tt)*) => {
        $crate::logging::write(std::format_args!($($arg)*))
    };
}
pub(crate) use ilog;

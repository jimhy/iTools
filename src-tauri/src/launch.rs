//! 本地启动：数据模型见 `settings::LaunchItem`；这里是「打开目标」的单一实现，
//! 既服务搜索结果执行（`commands::execute`），也服务 iTools 启动时的本地启动清单。
//!
//! 统一走 explorer.exe 中转（等同资源管理器双击）：目标进程父级是 explorer，
//! 与 iTools 的控制台/stdio 完全脱钩（否则 dev 模式子应用日志会串进宿主终端），
//! 且 spawn 立即返回，不阻塞。

use crate::settings::LaunchItem;

/// 经 explorer.exe 中转打开一个目标（文件/文件夹/程序均可）。
/// 特例：`cmd:<命令行>` 前缀表示这是 explorer 打不开、需要命令行执行的系统功能
/// （如环境变量 `rundll32 sysdm.cpl,EditEnvironmentVariables`），用 cmd /C 跑并隐藏控制台窗。
#[cfg(windows)]
pub fn open_detached(target: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    if let Some(cmdline) = target.strip_prefix("cmd:") {
        return Command::new("cmd.exe")
            .args(["/C", cmdline])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string());
    }

    match Command::new("explorer.exe")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(_) => Ok(()),
        // 极端情况下 explorer 不可用，退回 opener（同步，但至少能打开）
        Err(_) => opener::open(target).map_err(|e| e.to_string()),
    }
}

#[cfg(not(windows))]
pub fn open_detached(target: &str) -> Result<(), String> {
    opener::open(target).map_err(|e| e.to_string())
}

/// 直接执行程序（显式 program + args，**不经 cmd.exe**，元字符不被解释，无 shell 注入面）。
/// 供插件 `runCommand` 用（受 ALLOW_RUN_COMMAND 开关约束）。
#[cfg(windows)]
pub fn spawn_program(program: &str, args: &[String]) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(not(windows))]
pub fn spawn_program(program: &str, args: &[String]) -> Result<(), String> {
    std::process::Command::new(program)
        .args(args)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 由绝对路径构造一条 LaunchItem（自动判定文件夹、取末段为显示名）。
pub fn item_from_path(path: &str) -> LaunchItem {
    let p = std::path::Path::new(path);
    let is_dir = p.is_dir();
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| path.to_string());
    LaunchItem {
        id: path.to_string(),
        path: path.to_string(),
        name,
        is_dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_from_path_basics() {
        let it = item_from_path(r"C:\Tools\demo.exe");
        assert_eq!(it.name, "demo.exe");
        assert_eq!(it.id, it.path);
        // 临时目录一定是文件夹
        let dir = std::env::temp_dir();
        let dir_str = dir.to_string_lossy().into_owned();
        let d = item_from_path(&dir_str);
        assert!(d.is_dir, "临时目录应判定为文件夹");
    }
}

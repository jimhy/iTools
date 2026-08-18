//! 全盘文件名秒搜（NTFS MFT 直读）。
//!
//! # 为什么要有这一套（2026-08-17 起因与取证）
//!
//! `/f` 原先只走 Windows Search 索引，实测本机索引范围（注册表 `WorkingSetRules`）只有
//! `C:\Users\` 与开始菜单——按盘符查 `D:`/`E:`/`F:` **一律 0 条**，用户看到的「其它盘搜不出来」
//! 不是查询写错，是索引里根本没有那些数据。同时 `CONTAINS` 只支持**词前缀**，
//! 搜「报告」搜不到「季度报告.docx」。
//!
//! 直读 MFT 两个问题一起解决，实测全盘 1350 万条记录 42 秒建完索引
//! （Windows Search 索引器爬同样的量要数小时）。
//!
//! # 进程结构
//!
//! ```text
//! iTools 主进程（普通权限）                提权守护进程（itools.exe --mft-daemon）
//!   MftSearch::query ──命名管道──────────▶ daemon::handle
//!                                            ├─ volume.rs  读 MFT / USN（需管理员）
//!                                            └─ index.rs   紧凑内存索引 + 子串搜索
//! ```
//!
//! 主进程**不提权**，理由见 `ipc.rs` 的模块文档（提权会让启动器拉起的所有程序都变管理员、
//! 让开机自启失效、让拖放失效）。

pub mod daemon;
pub mod index;
pub mod ipc;
pub mod volume;

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use super::SearchItem;

/// 上次尝试拉起守护的时间戳（毫秒）。用来做冷却，避免用户被反复弹 UAC。
static LAST_SPAWN_MS: AtomicI64 = AtomicI64::new(0);
/// 用户拒绝过提权 → 本次运行不再自动弹，改由设置里手动开启
static SPAWN_DECLINED: AtomicBool = AtomicBool::new(false);

/// 拉起守护的最小间隔。守护若因故起不来，不能变成每次按键弹一次 UAC。
const SPAWN_COOLDOWN_MS: i64 = 60_000;

/// 主进程侧的搜索客户端。无状态——每次查询走一条短连接（见 `ipc::request`）。
pub struct MftSearch;

impl MftSearch {
    /// 查询。守护未就绪 / 未启动 / 超时都返回 `None`，调用方据此降级到别的后端。
    ///
    /// 返回 `Some(vec![])` 与 `None` 语义**不同**：前者是「索引可用但确实没有匹配」，
    /// 后者是「这个后端用不了」。降级逻辑依赖这个区分，别合并。
    pub fn query(needle: &str, limit: usize) -> Option<Vec<SearchItem>> {
        match ipc::request(&ipc::Request::Query {
            needle: needle.to_string(),
            limit,
        }) {
            Some(ipc::Response::Hits(hits)) => Some(
                hits.into_iter()
                    .map(|h| SearchItem {
                        id: h.path.clone(),
                        title: h.name,
                        subtitle: h.path.clone(),
                        kind: if h.is_dir { "folder" } else { "file" }.to_string(),
                        target: h.path,
                        icon: None,
                        action: "open".to_string(),
                    })
                    .collect(),
            ),
            _ => None,
        }
    }

    /// 取索引状态。守护未启动时返回 `None`——前端据此显示「未启用全盘索引」。
    pub fn status() -> Option<ipc::StatusDto> {
        match ipc::request(&ipc::Request::Status) {
            Some(ipc::Response::Status(s)) => Some(s),
            _ => None,
        }
    }

    /// 请求守护重建索引（用户在设置里点「重建索引」时调）。
    pub fn rebuild() -> bool {
        matches!(ipc::request(&ipc::Request::Rebuild), Some(ipc::Response::Ok))
    }

    /// 关掉守护、释放它占的内存（用户在设置里关闭「全盘索引」时调）。
    ///
    /// 守护已经不在跑时返回 `true`——目标状态已达成，对用户而言就是成功。
    ///
    /// 全链路已接通：`commands::file_index_disable` → `build.rs` 的 COMMANDS 清单
    /// → `capabilities/default.json` 的 `allow-file-index-disable` → `/f` 状态条上
    /// 就绪态右侧的「关闭」按钮（`main.ts::disableFileIndex`）。四处齐全才算真能点。
    pub fn shutdown() -> bool {
        if !Self::is_running() {
            return true;
        }
        // 关掉后本次运行内不再自动拉起：用户刚明确表示不要它，
        // 下一次按键又给他弹一个 UAC 是最糟糕的体验。
        SPAWN_DECLINED.store(true, Ordering::Relaxed);
        matches!(
            ipc::request(&ipc::Request::Shutdown),
            Some(ipc::Response::Ok)
        )
    }

    /// 守护是否在跑
    pub fn is_running() -> bool {
        ipc::daemon_alive()
    }

    /// 确保守护在跑；不在则提权拉起。
    ///
    /// `user_initiated=true`（用户在设置里主动点开启）会忽略冷却与「上次被拒绝」的记忆
    /// ——用户明确要开，就该弹给他，而不是被我们自己的防抖挡住。
    ///
    /// 返回是否**已经在跑**（刚拉起的情况下返回 false，因为索引还要建几十秒）。
    pub fn ensure_running(user_initiated: bool) -> bool {
        if Self::is_running() {
            return true;
        }
        if !user_initiated {
            if SPAWN_DECLINED.load(Ordering::Relaxed) {
                return false;
            }
            let now = now_ms();
            let last = LAST_SPAWN_MS.load(Ordering::Relaxed);
            if now - last < SPAWN_COOLDOWN_MS {
                return false;
            }
        }
        LAST_SPAWN_MS.store(now_ms(), Ordering::Relaxed);
        // ⚠ 必须放后台线程：ShellExecuteW 的 "runas" 会**阻塞到用户在 UAC 对话框上做出选择**。
        // 这个函数是从 `search` 命令（进而是逐键搜索）里调的，同步等下去会把 invoke 线程挂住
        // ——用户看到的就是「输入框打字卡死几秒」，而他还没意识到那个 UAC 窗口在等他。
        std::thread::spawn(|| {
            if let Err(code) = spawn_elevated_daemon() {
                // 用户在 UAC 对话框上点了「否」→ ERROR_CANCELLED(1223)。
                // 记下来，这次运行内不再自动弹——要开只能由用户去设置里主动点。
                if code == 1223 {
                    SPAWN_DECLINED.store(true, Ordering::Relaxed);
                }
            }
        });
        false // 刚拉起（或正在等用户授权），索引还没建完
    }

    /// 用户是否拒绝过提权。前端据此把「开启全盘索引」的入口从状态条挪到设置里，
    /// 而不是反复在他面前弹同一个提示。
    pub fn spawn_declined() -> bool {
        SPAWN_DECLINED.load(Ordering::Relaxed)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 命令行参数：带上它启动就进守护模式而不是开窗口。
pub const DAEMON_ARG: &str = "--mft-daemon";

/// 以管理员身份启动自身的守护模式。返回 `Err(win32 错误码)`。
///
/// 用 `ShellExecuteW` + `runas` 而不是 `CreateProcess`——后者无法提权，
/// 只有 shell 动词 `runas` 会触发 UAC 同意流程。
fn spawn_elevated_daemon() -> Result<(), u32> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    use windows::core::HSTRING;

    let exe = std::env::current_exe().map_err(|_| 0u32)?;
    let file = HSTRING::from(exe.as_os_str());
    let params = HSTRING::from(DAEMON_ARG);
    let verb = HSTRING::from("runas");
    // SAFETY: 三个宽串在调用期间存活；SW_HIDE 让守护无窗口。
    // ShellExecuteW 的返回值是「伪 HINSTANCE」，<= 32 表示失败，错误码即该值。
    let rc = unsafe {
        ShellExecuteW(
            None,
            &verb,
            &file,
            &params,
            None,
            SW_HIDE,
        )
    };
    let code = rc.0 as usize;
    if code > 32 {
        Ok(())
    } else {
        Err(code as u32)
    }
}

/// 当前进程是否已提权。守护进程启动时自检用——没提权就直接失败退出，
/// 而不是让它建出一个空索引、装作在工作。
pub fn is_elevated() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = HANDLE::default();
    // SAFETY: 伪句柄无需释放；token 在下面关闭。
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut size = 0u32;
    // SAFETY: 输出缓冲是栈上 POD，长度如实传入。
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut core::ffi::c_void),
            core::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
    };
    // SAFETY: token 来自成功的 OpenProcessToken，只关一次
    unsafe {
        let _ = CloseHandle(token);
    }
    ok.is_ok() && elevation.TokenIsElevated != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 守护没在跑时，query 必须返回 None（而不是 Some(vec![])）——
    /// 降级逻辑靠这个区分「后端不可用」与「确实没匹配」
    #[test]
    fn query_returns_none_when_daemon_absent() {
        if MftSearch::is_running() {
            println!("本机守护在跑，跳过该用例");
            return;
        }
        assert!(
            MftSearch::query("test", 10).is_none(),
            "守护缺席时必须返回 None，不能返回空 Vec 冒充「没搜到」"
        );
        assert!(MftSearch::status().is_none());
    }

    /// 提权自检在非提权测试环境下应为 false，且不能 panic
    #[test]
    fn elevation_check_does_not_panic() {
        println!("当前进程已提权: {}", is_elevated());
    }
}

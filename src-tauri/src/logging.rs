//! 轻量文件日志：给运行期 `[iTools]` 日志加本地时间戳，落 `itools.log`，同时回显 stderr。
//!
//! 用法：`ilog!("[iTools] ... {err}")`，格式化语法同 `eprintln!`。启动最早期调用一次 [`init`]。
//!
//! # 两种构建写两个文件（互不覆盖）
//!
//! | 构建 | 日志文件 | 为什么是这里 |
//! |---|---|---|
//! | debug | **exe 同目录**（即 `target/debug/itools.log`） | 开发时就地看，不用去翻 `%LOCALAPPDATA%` |
//! | release | [`crate::paths::data_root`] 下（`%LOCALAPPDATA%\itools\itools.log`） | 安装目录（Program Files 之类）普通用户不可写 |
//!
//! 分成两个文件也是「debug 与 release 彻底隔离」的一环：开发常态是一边跑 release 干活、
//! 一边编 debug 调，两个进程共写一个文件只会把时间线搅成一锅粥。
//!
//! # release 从「压根不写」改成「写」（2026-08-18）
//!
//! 改之前 [`init`] 的第一句是 `if !cfg!(debug_assertions) { return; }`，release 只走
//! `eprintln!`；而 release 是 GUI 子系统（`main.rs` 的 `windows_subsystem = "windows"`），
//! 进程根本没有控制台，stderr 句柄是空的——所有日志等于扔进黑洞。后果很具体：
//! 「已有同类实例在运行，本进程退出」「MCP 默认端口被占已改用 7346」「单实例互斥体创建失败」
//! 这些**唯一能解释用户所见怪象**的日志，在用户机器上并不存在。用户报「双击图标没反应」，
//! 我们和他都拿不出任何证据。所以现在两种构建都落文件。
//!
//! ⚠ 与之相关的一条旧约定：`doc/开发准则.md` 的安全红线写着「Release 版不产调试日志」。
//! 本模块现在写的是**运行诊断日志**（谁启动了、端口换没换、哪一步失败了），不含凭据；
//! 但它会长期躺在用户磁盘上，新增 `ilog!` 时务必自觉——见下面「隐私」。
//!
//! # 大小上限与轮转
//!
//! 常驻托盘进程可以连着跑几周，日志不能无限长。上限 [`MAX_BYTES`]，超了就把
//! `itools.log` 改名成 `itools.log.1`（**只留一代**，上一代的 `.1` 被覆盖），再开一个空的继续写。
//! 判定分两处：
//!
//! - [`init`] 时按文件**真实大小**判一次（新一次运行从干净的文件头开始）；
//! - 之后按**累计写入字节数**判（而不是每行 `metadata()`——那是一次系统调用，
//!   而 `ilog!` 就在启动关键路径上）。
//!
//! 已知取舍（是坑就写下来，别让下一个人以为是 bug）：
//!
//! - 字节计数是**每进程**的。`--mft-daemon` 提权索引守护跑的是同一个 exe、写同一个文件，
//!   两个进程各记各的账，所以文件最多可能涨到 2×[`MAX_BYTES`] 才有人去轮转；且一方轮转后，
//!   另一方的句柄会跟着改名后的文件走（Windows 改名不影响已开句柄），它后续的行会落进 `.1`。
//!   日志会错行，但不会丢，也不影响功能。（极端情况：两个进程**同时**在启动、且文件正好超限，
//!   两次轮转叠在一起会把上一代盖掉——需要守护与主进程几乎同秒冷启动，代价也只是少一代旧日志。）
//! - 轮转失败（例如文件被别的东西独占）后**本次运行不再重试**：这种失败通常不是偶然，
//!   每行重试一次只会白白多出几万次系统调用。下次启动 [`init`] 会重新试。
//!   代价是这一轮日志不再受上限约束——总比为了守住上限而丢日志强。
//!
//! # 失败时怎么办：静默回退，绝不牵连启动
//!
//! 取不到路径、目录建不出来、文件打不开、写盘失败——一律忽略，只保留 stderr。
//! 日志设施本身绝不能成为「app 起不来」的原因，所以本模块对外不返回任何错误。
//!
//! # 隐私（新增 `ilog!` 之前先读这段）
//!
//! 日志现在会**长期留在用户机器上**，且用户报障时多半整份发过来。所以：
//! **不要**把凭据（token / 密码 / Cookie）、账号、剪贴板内容、搜索词、文件正文写进日志。
//! （出站代理那条日志已经用 `ProxySpec::display()` 把凭据抹成 `***`，是正确示范。）
//!
//! 判定口径就一条：**这一行如果原样贴进工单，用户会不会介意？**
//!
//! - **用户内容**（插件经 `window.itools` 传进来的任意字符串、剪贴板、查询词、账号 / 昵称）
//!   一律不入日志，只记「谁 + 多长 + 什么错」。`plugin_notify` 是范例：记插件 id 与正文字数，
//!   不记正文本身。
//! - **用户自选的路径**（文件 / 目录对话框选出来的）只记扩展名或末级目录名——完整路径含
//!   Windows 用户名和私人目录结构，文件名本身也可能就是隐私。`read_image`、`dev_add_dir` 是范例。
//! - **应用自有路径**（`%LOCALAPPDATA%\itools\...`、插件根、暂存区）可以照打：它们结构固定、
//!   只多暴露一个 Windows 用户名，而排查安装 / 播种 / 回滚必须知道具体是哪个目录，去掉就等于
//!   把可观测性清零。
//! - 插件安装确认 token 只打前 8 位（那是本机会话号、不是任何服务的凭据）；插件清单里的字段
//!   （name / version / icon / regex）是插件作者写的、随包分发，不算用户隐私。
//!
//! ⚠ 别走另一个极端把日志删空——本模块给 release 加文件日志就是为了让「用户说没反应」时
//! 有据可查；脱敏是把**用户内容**换成**可诊断的元信息**，不是把整行删掉。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 日志文件名。轮转后的上一代是它再加 `.1`（`itools.log.1`）。
const LOG_NAME: &str = "itools.log";

/// 单个日志文件的大小上限（2 MiB）。超过就轮转，只留一代。
///
/// 2 MiB 差不多是一两万行：足够装下一次完整的「启动 → 出问题」时间线，
/// 又不至于让用户发日志给我们时卡在附件大小上。
const MAX_BYTES: u64 = 2 * 1024 * 1024;

/// 已打开的日志文件及其当前大小。
struct Sink {
    /// 日志文件路径：轮转之后要靠它把新文件重新开出来。
    path: PathBuf,
    file: File,
    /// 这个文件现在有多少字节：[`open_sink`] 时取真实大小，之后每写一行自己累加。
    written: u64,
    /// 还要不要再尝试轮转。轮转失败过一次就置 `false`（理由见模块头「已知取舍」）。
    rotatable: bool,
}

/// 日志文件句柄。`None` = 这次运行没能打开文件，所有日志只走 stderr。
static LOG: OnceLock<Mutex<Option<Sink>>> = OnceLock::new();

/// 初始化文件日志：定路径 → 该轮转就先轮转 → 以追加方式打开 → 记一行启动横幅。
///
/// 任何一步失败都静默回退成「只有 stderr」：不返回错误、不 panic、不影响启动。
/// 只应在进程最早期调用一次；重复调用时后面几次是空操作（`OnceLock::set` 失败即忽略）。
pub fn init() {
    let sink = log_path().and_then(|path| open_sink(&path));
    let _ = LOG.set(Mutex::new(sink));
    // 横幅带版本号与构建类型：用户发来的日志十有八九不会附带「我装的是哪个版本」，
    // 而 debug / release 现在各写各的文件，这一行也用来确认手里这份是哪个构建产生的。
    write(format_args!(
        "[iTools] ==== 日志启动 v{}（{}）====",
        env!("CARGO_PKG_VERSION"),
        build_kind()
    ));
}

/// 写一行日志：加本地时间戳 → 回显 stderr → 落文件（一行一次写，交给内核后进程被强杀也不丢）。
pub fn write(args: std::fmt::Arguments) {
    let line = format!(
        "[{}] {}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
        args
    );
    // release 是 GUI 子系统、没有控制台，这一行等于空转（std 对空 stderr 句柄按「写成功」
    // 处理，不会 panic）；dev 控制台与 `cargo test` 下它才是主要出口，所以保留。
    eprintln!("{line}");
    if let Some(cell) = LOG.get() {
        if let Ok(mut guard) = cell.lock() {
            if let Some(sink) = guard.as_mut() {
                append(sink, &line);
            }
        }
    }
}

/// 构建类型标签，只用于日志横幅。
fn build_kind() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

/// 本次构建该往哪写日志。返回 `None` 表示这次定不出可用路径，调用方静默回退到只有 stderr。
///
/// 用运行期的 `cfg!` 而不是 `#[cfg]`：两个分支都要参与编译，改坏哪一边都会当场报错，
/// 不会等到切了构建类型才发现。
fn log_path() -> Option<PathBuf> {
    if cfg!(debug_assertions) {
        debug_log_path()
    } else {
        release_log_path()
    }
}

/// debug：**exe 同目录**（`target/debug/itools.log`）。
///
/// 保持历史行为不变——开发时 `cargo run` 完日志就在手边，这份便利别弄没了。
fn debug_log_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join(LOG_NAME))
}

/// release：用户数据目录（`%LOCALAPPDATA%\itools`）下。
///
/// ⚠ 不能沿用 exe 同目录：正式版装在 `Program Files` 之类的地方，普通用户没有写权限，
/// 打开必然失败，于是又回到「一条日志都没有」的老问题上。
fn release_log_path() -> Option<PathBuf> {
    let root = crate::paths::data_root();
    // 本模块跑在所有存储初始化之前，首启时这个目录可能还不存在，得自己建。
    std::fs::create_dir_all(&root).ok()?;
    Some(root.join(LOG_NAME))
}

/// 打开（必要时先轮转）日志文件。任何一步失败都返回 `None`。
fn open_sink(path: &Path) -> Option<Sink> {
    if oversized(path) {
        // 启动时先轮转：新一次运行从干净的文件头开始，上一次的完整时间线也还留在 `.1` 里
        //（排查「上次退出前到底发生了什么」全靠它）。
        rotate(path);
    }
    let file = OpenOptions::new().create(true).append(true).open(path).ok()?;
    let written = file.metadata().map(|m| m.len()).unwrap_or(0);
    Some(Sink {
        path: path.to_path_buf(),
        file,
        written,
        rotatable: true,
    })
}

/// 往已打开的日志里追加一行，超过上限就地轮转。
///
/// 单独拆出来是为了能脱开全局状态直接测（见 `rotates_mid_run_by_byte_count`）。
fn append(sink: &mut Sink, line: &str) {
    // 「正文 + 换行」拼好一次 `write_all` 写下去，而不是 `writeln!` 的两次写：
    // 提权索引守护跑的是同一个 exe、写的是同一个文件，append 模式下**单次**写入是原子的
    //（内核把偏移挪到文件尾再写），分两次写则可能被另一个进程的行插在正文与换行之间。
    let _ = sink.file.write_all(format!("{line}\n").as_bytes());
    // 裸 `File` 没有用户态缓冲，这句 flush 其实是空操作；留着是为了哪天有人给它套上
    // `BufWriter` 时不至于忘了刷——日志的价值全在「崩之前那几行也在文件里」。
    let _ = sink.file.flush();
    // `line.len()` 是 UTF-8 字节数，与真正落盘的字节数一致（中文日志尤其别用 `chars().count()`）；
    // `+1` 是上面那个 `\n`（Rust 不做 CRLF 转换，Windows 上也只占一个字节）。
    sink.written += line.len() as u64 + 1;
    if sink.rotatable && sink.written >= MAX_BYTES {
        roll(sink);
    }
}

/// 运行途中就地轮转：改名 → 重开一个空文件继续写。失败则本次运行不再尝试轮转。
fn roll(sink: &mut Sink) {
    if !rotate(&sink.path) {
        sink.rotatable = false;
        return;
    }
    // 改名之后旧句柄仍然指着已改名的 `.1`，必须按原路径重新开一个，
    // 否则后面的日志会全部写进上一代文件里。
    match OpenOptions::new()
        .create(true)
        .append(true)
        .open(&sink.path)
    {
        Ok(file) => {
            sink.file = file;
            sink.written = 0;
        }
        Err(_) => sink.rotatable = false,
    }
}

/// 文件是否已到上限。文件不存在 / 属性读不到都算「没超」——没什么可轮转的。
fn oversized(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() >= MAX_BYTES)
        .unwrap_or(false)
}

/// 轮转：`itools.log` → `itools.log.1`（覆盖上一代，只留一代）。返回是否成功。
///
/// Windows 上 `rename` 走的是 `MoveFileExW(..., MOVEFILE_REPLACE_EXISTING)`：目标已存在会被
/// 替换掉；源文件即使正被自己或别的进程开着也能改名（Rust 开文件时默认带 `FILE_SHARE_DELETE`）。
fn rotate(path: &Path) -> bool {
    std::fs::rename(path, backup_path(path)).is_ok()
}

/// 上一代日志的路径：整个文件名后面接 `.1`（`itools.log` → `itools.log.1`）。
///
/// 用 `OsString` 拼而不是 `set_extension`：后者会把 `.log` 换掉、变成 `itools.1`，
/// 既不像日志文件，也和「收集 `itools.log*`」这种通配对不上。
fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".1");
    PathBuf::from(name)
}

/// 运行期日志宏，语法同 `eprintln!`：`ilog!("[iTools] xxx {err}")`。
macro_rules! ilog {
    ($($arg:tt)*) => {
        $crate::logging::write(std::format_args!($($arg)*))
    };
}
pub(crate) use ilog;

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个用例一个独立临时目录（进程号 + 纳秒，避免并行用例互相踩）。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "itools-log-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    /// 造一个指定大小的日志文件：开头写一句可辨认的标记，再用 `set_len` 撑到指定长度。
    ///
    /// 用 `set_len` 而不是真写 2 MiB：被测的是「大小判定 + 改名」，没必要为此搬 2 MiB 数据。
    fn make_log(path: &Path, marker: &str, len: u64) {
        let mut f = File::create(path).expect("建日志文件");
        f.write_all(marker.as_bytes()).expect("写标记");
        f.set_len(len).expect("撑到指定长度");
    }

    /// 到上限就轮转：旧内容整份变成 `.1`，新 `itools.log` 从 0 字节开始。
    #[test]
    fn rotates_when_at_limit() {
        let dir = temp_dir("rotate");
        let log = dir.join(LOG_NAME);
        make_log(&log, "第一代", MAX_BYTES);

        let sink = open_sink(&log).expect("应能打开日志");
        assert_eq!(sink.written, 0, "轮转后应是一个空文件");
        drop(sink); // Windows 上句柄不放手就删不掉目录

        let backup = backup_path(&log);
        assert_eq!(
            std::fs::metadata(&backup).expect("应有上一代 .1").len(),
            MAX_BYTES,
            "上一代要原样保留，不能被截断"
        );
        assert!(
            std::fs::read(&backup)
                .expect("读 .1")
                .starts_with("第一代".as_bytes()),
            "上一代的内容不能被清掉"
        );
        assert_eq!(std::fs::metadata(&log).expect("应有新日志").len(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没到上限就别动它：继续追加，也不该凭空造出 `.1`。
    #[test]
    fn keeps_appending_when_under_limit() {
        let dir = temp_dir("append");
        let log = dir.join(LOG_NAME);
        make_log(&log, "旧日志", 1024);

        let mut sink = open_sink(&log).expect("应能打开日志");
        assert_eq!(
            sink.written, 1024,
            "written 要等于文件真实大小，否则轮转会提前或滞后"
        );
        assert!(!backup_path(&log).exists(), "没超限不该产生 .1");
        append(&mut sink, "新的一行");
        drop(sink);

        assert!(
            std::fs::metadata(&log).expect("读新日志").len() > 1024,
            "应是追加，不是把旧内容截掉重写"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 跑着跑着才超限也照样轮转——不是只在 [`init`] 那一下判一次。
    #[test]
    fn rotates_mid_run_by_byte_count() {
        let dir = temp_dir("midrun");
        let log = dir.join(LOG_NAME);

        let mut sink = open_sink(&log).expect("应能打开日志");
        append(&mut sink, "轮转前");
        assert!(!backup_path(&log).exists(), "才写一行不该轮转");

        // 把计数顶到压线位置，模拟「常驻跑了很久」，省下真写 2 MiB 的开销
        sink.written = MAX_BYTES - 1;
        append(&mut sink, "这一行压过线，触发轮转");
        assert_eq!(sink.written, 0, "轮转后计数要归零");
        append(&mut sink, "轮转后");
        drop(sink);

        let old = std::fs::read_to_string(backup_path(&log)).expect("读 .1");
        assert!(
            old.contains("轮转前") && old.contains("这一行压过线，触发轮转"),
            "轮转前写的行必须完整留在 .1 里，实得：{old}"
        );
        let new = std::fs::read_to_string(&log).expect("读新日志");
        assert!(
            new.contains("轮转后") && !new.contains("轮转前"),
            "轮转后要写进一个干净的新文件，实得：{new}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 只留一代：第二次轮转会把上一次的 `.1` 覆盖掉，不会越攒越多。
    #[test]
    fn keeps_only_one_generation() {
        let dir = temp_dir("onegen");
        let log = dir.join(LOG_NAME);

        make_log(&log, "第一代", MAX_BYTES);
        drop(open_sink(&log).expect("第一次轮转"));
        make_log(&log, "第二代", MAX_BYTES);
        drop(open_sink(&log).expect("第二次轮转"));

        let backup = backup_path(&log);
        assert!(
            std::fs::read(&backup)
                .expect("读 .1")
                .starts_with("第二代".as_bytes()),
            ".1 里应该是最近一代，旧的那份被覆盖"
        );
        let names: Vec<String> = std::fs::read_dir(&dir)
            .expect("列目录")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names.len(),
            2,
            "只该有 itools.log 与 itools.log.1 两个文件，实得 {names:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 打不开的路径：返回 `None`，不 panic、不牵连调用方。
    ///
    /// 诚实说明：真正的「目录存在但没有写权限」（Program Files 那种）在测试里不好稳定构造
    ///（要改 ACL 且可能需要提权），这里用两种同样会让 `open` 失败的路径形态代替。
    /// 覆盖到的是同一段代码路径：`open(...).ok()?` 失败 → `None` → 调用方只走 stderr。
    #[test]
    fn unopenable_path_returns_none_without_panic() {
        let dir = temp_dir("nowrite");

        // ① 父目录根本不存在
        assert!(
            open_sink(&dir.join("并不存在的子目录").join(LOG_NAME)).is_none(),
            "父目录不存在时应静默返回 None"
        );

        // ② 父目录的位置上是个文件
        let blocker = dir.join("blocker");
        File::create(&blocker).expect("建占位文件");
        assert!(
            open_sink(&blocker.join(LOG_NAME)).is_none(),
            "父路径是文件时应静默返回 None"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 轮转文件名必须还是 `itools.log.1`（别哪天被 `set_extension` 改成 `itools.1`）。
    #[test]
    fn backup_keeps_full_name() {
        assert_eq!(
            backup_path(&PathBuf::from("some/dir").join(LOG_NAME)),
            PathBuf::from("some/dir").join("itools.log.1")
        );
    }

    /// debug 构建仍旧写 exe 同目录——开发时「就地看日志」的便利不能丢。
    #[test]
    fn debug_build_logs_next_to_exe() {
        let exe = std::env::current_exe().expect("取 exe 路径");
        assert_eq!(
            debug_log_path().expect("debug 下应能定出路径"),
            exe.parent().expect("exe 应有父目录").join(LOG_NAME)
        );
        // `cargo test` 恒为 debug 构建，所以 log_path() 这时必须选 debug 这一支
        assert_eq!(log_path(), debug_log_path());
    }

    /// release 构建的日志落在用户数据目录下，且目录会被建出来。
    ///
    /// 注意：`cfg!(debug_assertions)` 在测试里恒为 true，走不到 [`log_path`] 的 release 分支，
    /// 所以这里直接测它要调的 [`release_log_path`]；此时 `paths::data_root()` 给的是 **dev**
    /// 那份根目录（它自己按构建分目录），断言只能落在「就在 data_root() 下、目录已建好、
    /// 不是 exe 同目录」这三点上。
    #[test]
    fn release_build_logs_under_data_root() {
        let root = crate::paths::data_root();
        assert_eq!(
            release_log_path().expect("应能定出 release 日志路径"),
            root.join(LOG_NAME)
        );
        assert!(root.is_dir(), "日志目录应已被创建：{}", root.display());
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(PathBuf::from));
        assert_ne!(
            Some(root),
            exe_dir,
            "release 不能写回 exe 同目录（Program Files 不可写）"
        );
    }
}

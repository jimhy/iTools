//! 插件「执行外部程序并拿到结果」能力：`plugin_run_command`（见 `commands.rs`）只 spawn 不回收
//! 结果，插件拿不到 stdout / 退出码，做不了 adb 这类需要读命令输出的场景。这里补上两条腿：
//!
//! - [`plugin_exec`]：一次性执行，等它跑完，把 `{ code, stdout, stderr }` 整体吐回去。
//! - [`plugin_exec_stream`] + [`plugin_exec_kill`] / [`plugin_exec_quit`]：长跑命令（如
//!   `adb logcat`）用这条，stdout/stderr 一到就经 Tauri 事件增量推给插件窗口，进程结束再推一条
//!   exit 事件；调用方可以强杀（kill）或礼貌地请它自己退（quit）。
//!
//! 三个设计取舍：
//!
//! 1. **不经 shell**：跟 `launch::spawn_program` 一样，直接 `Command::new(program).args(args)`，
//!    元字符 `&`/`|`/`>` 不会被解释，没有 shell 注入面。
//! 2. **编码**：Windows 上不少中文向 CLI 工具（如 adb、部分老工具链）用系统 ANSI 代码页
//!    （简体中文环境下是 GBK / 936）写控制台输出，而不是 UTF-8——不处理直接拿字节转 UTF-8
//!    会整段乱码。这里裸 FFI 声明 `kernel32!MultiByteToWideChar` 自己转（见 [`win_codepage`]），
//!    没有引 encoding_rs / gbk 之类的新依赖，也没有去 Cargo.toml 加 `Win32_Globalization`
//!    feature——`extern "system"` 直接连 kernel32 是 Windows 平台自带的系统库，不用额外声明。
//!    `encoding` 参数：`"utf8"` / `"gbk"` / `"auto"`（缺省）。`plugin_exec` 一次性能拿到完整字节，
//!    `auto` 直接对整段做严格 UTF-8 校验，能过就是 UTF-8，不能过就当 GBK 解——不存在边界问题。
//!    `plugin_exec_stream` 是边到边解，用 [`StreamDecoder`] 按「已确定完整、可安全转文本的前缀」
//!    增量吐，未完整的多字节序列尾巴留到下一块，避免在字符中间切断导致的乱码。
//! 3. **超时是真杀**：`timeout_ms` 到点后对子进程 `kill()`（Windows 上是 `TerminateProcess`），
//!    不是掐断读取假装它退出了；`plugin_exec` 里 kill 后还会阻塞等它真正退出、再回收
//!    stdout/stderr 读线程，`plugin_exec_stream` 由后台等待线程做同样的事。
//!
//! 会话归属：流式会话按**完整会话身份**（插件 id + `dev` 标志）校验，不是裸 id——参考
//! `audio.rs` 模块头注释，调试插件与同名正式插件共用一个 id，只比 id 会让调试窗口的插件
//! kill/quit 掉正式窗口里同名插件正在跑的进程（反向亦然）。

use std::collections::HashMap;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};

use tauri::{Manager, State};

use super::commands::{caller_session, plugin_granted};
use super::{ActiveSession, PluginRegistry};
use crate::logging::ilog;
use crate::settings::SettingsStore;

// ==================== 对外事件名 ====================

const EVT_STDOUT: &str = "plugin-exec-stdout";
const EVT_STDERR: &str = "plugin-exec-stderr";
const EVT_EXIT: &str = "plugin-exec-exit";

// ==================== 参数 / 返回结构 ====================

/// `plugin_exec` / `plugin_exec_stream` 的可选参数（字段名与 JS 桥接层保持 camelCase）。
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ExecOptions {
    /// 子进程工作目录；缺省继承 iTools 主进程当前目录。
    pub cwd: Option<String>,
    /// 超时（毫秒）；到点仍未结束就强制终止。缺省不设超时。
    pub timeout_ms: Option<u64>,
    /// 输出解码方式："utf8" / "gbk" / "auto"（缺省，自动探测）。
    pub encoding: Option<String>,
}

/// [`plugin_exec`] 的返回结果。
#[derive(serde::Serialize)]
pub struct ExecResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    /// 是否因超时被强制终止——为 true 时 `code` 只是「被杀那一刻」系统给的退出码，不代表程序
    /// 自身的真实退出状态，调用方不应把它当业务上的成功/失败解读。
    #[serde(rename = "timedOut")]
    pub timed_out: bool,
    /// 输出是否因超过单路 16 MB 上限被截断。为 true 时 `stdout`/`stderr` **不是完整输出**，
    /// 调用方不能据此下「结果为空/没有匹配项」这类结论——要完整输出请改用 execStream。
    #[serde(rename = "truncated")]
    pub truncated: bool,
}

/// 一次性执行的单路输出上限（16 MB）。见 [`plugin_exec`] 里的说明。
const EXEC_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;

/// 读一路管道到上限为止。返回 `(内容, 是否被截断)`。
fn read_capped(pipe: &mut impl std::io::Read) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => return (buf, false),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() >= EXEC_OUTPUT_LIMIT {
                    buf.truncate(EXEC_OUTPUT_LIMIT);
                    return (buf, true);
                }
            }
            Err(_) => return (buf, false),
        }
    }
}

/// 把一条事件推给**插件页**。
///
/// 必须走 `webview.eval` → `window.__itoolsEmit(channel, payload)` 这条总线，**不能用
/// `app.emit_to` 的 Tauri 原生事件**：插件页是受限自定义协议页，拿不到 `window.__TAURI__`
/// （见 `bridge.js` 顶部说明），原生事件推过去根本没有接收方，表现就是「命令能跑、
/// 但插件永远收不到输出」——一个典型的「看着实现了、实际不生效」。
///
/// payload 用 `serde_json` 序列化后直接插进 JS 字面量：它保证了引号/反斜杠/控制字符的转义，
/// 子进程输出里的任意字节都不会破坏这段脚本。
fn emit_to_plugin<T: serde::Serialize>(app: &tauri::AppHandle, label: &str, channel: &str, payload: &T) {
    let Some(win) = app.get_webview_window(label) else {
        return;
    };
    let json = match serde_json::to_string(payload) {
        Ok(j) => j,
        Err(_) => return,
    };
    let js = format!("window.__itoolsEmit && window.__itoolsEmit('{channel}', {json})");
    let _ = win.eval(&js);
}

#[derive(serde::Serialize, Clone)]
struct ExecChunk {
    #[serde(rename = "streamId")]
    stream_id: String,
    data: String,
}

#[derive(serde::Serialize, Clone)]
struct ExecExit {
    #[serde(rename = "streamId")]
    stream_id: String,
    code: i32,
    #[serde(rename = "timedOut")]
    timed_out: bool,
}

// ==================== 运行期状态（流式会话表） ====================

/// 一条流式执行会话：归属者 + 对子进程的共享句柄（读线程 / 等待线程 / kill·quit 命令都要用它）。
struct StreamSession {
    owner: ActiveSession,
    child: Arc<Mutex<Child>>,
}

/// `plugin_exec_stream` 的运行期状态（managed）。会话在进程自然退出 / 被杀 / 被优雅结束后，
/// 由后台等待线程自动从表里摘除（见 [`spawn_waiter_thread`]），不会常驻泄漏。
#[derive(Default)]
pub struct ExecState {
    sessions: Mutex<HashMap<String, StreamSession>>,
}

/// 容忍锁中毒（持锁线程 panic）：拿到内部值继续用，而不是让调用方也跟着 panic。
/// 本模块里没有会在持锁期间 panic 的逻辑（都是简单的 HashMap / Child 操作），这里只是防守。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 日志里只记「谁 + 干了什么」，不记程序参数 / 输出内容本身——道理同 `commands.rs::plugin_notify`
/// 的注释：这些参数/输出可能是插件在处理的用户数据（账号、路径、剪贴板内容……）。
fn who(session: &ActiveSession) -> String {
    if session.dev {
        format!("dev:{}", session.id)
    } else {
        session.id.clone()
    }
}

// ==================== 编码处理 ====================

#[derive(Clone, Copy)]
enum EncodingMode {
    Utf8,
    Gbk,
    Auto,
}

fn resolve_encoding(raw: Option<&str>) -> EncodingMode {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("utf8") | Some("utf-8") => EncodingMode::Utf8,
        Some("gbk") | Some("gb2312") | Some("cp936") | Some("ansi") => EncodingMode::Gbk,
        // 缺省 / "auto" / 无法识别的值一律按自动探测处理，不因为参数写错就直接报错拒绝执行。
        _ => EncodingMode::Auto,
    }
}

/// 一次性解码整段字节（`plugin_exec` 用）：进程已经跑完，拿到的是完整字节，不存在「切断多字节
/// 字符」的边界问题，`auto` 直接严格校验一遍 UTF-8 即可。
fn decode_full(bytes: &[u8], mode: EncodingMode) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    match mode {
        EncodingMode::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
        EncodingMode::Gbk => win_codepage::decode(bytes, win_codepage::CP_GBK)
            .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned()),
        EncodingMode::Auto => match std::str::from_utf8(bytes) {
            Ok(s) => s.to_string(),
            Err(_) => win_codepage::decode(bytes, win_codepage::CP_GBK)
                .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned()),
        },
    }
}

#[derive(Clone, Copy)]
enum ResolvedEncoding {
    Utf8,
    Gbk,
}

/// 增量解码器（`plugin_exec_stream` 用）：数据是分块到达的，可能在多字节字符正中间被切断。
/// 策略：`carry` 攒着「已到货但还不能确定是否完整」的尾巴字节；每次喂入新数据后，只把
/// 「确定完整」的前缀解码吐出去，剩下的留在 `carry` 里等下一块数据补全。
struct StreamDecoder {
    mode: EncodingMode,
    /// `auto` 模式下嗅探出的实际编码；显式指定 utf8/gbk 时首次调用就会立即定型。
    resolved: Option<ResolvedEncoding>,
    carry: Vec<u8>,
}

impl StreamDecoder {
    fn new(mode: EncodingMode) -> Self {
        Self {
            mode,
            resolved: None,
            carry: Vec::new(),
        }
    }

    /// 喂入新到达的一段字节，返回「当前可以安全交给前端」的文本（可能是空串——数据不完整时
    /// 先攒着，不产出半个字符）。
    fn push(&mut self, chunk: &[u8]) -> String {
        self.carry.extend_from_slice(chunk);
        if self.resolved.is_none() {
            match self.sniff() {
                Some(enc) => self.resolved = Some(enc),
                // 嗅探样本还不够多、判不出来，先不产出，等下一块数据再试。
                None => return String::new(),
            }
        }
        match self.resolved {
            Some(ResolvedEncoding::Utf8) => drain_utf8(&mut self.carry),
            Some(ResolvedEncoding::Gbk) => drain_gbk(&mut self.carry),
            // 上面已确保走到这里时 resolved 必为 Some，这条分支理论不可达，防守式兜底不产出。
            None => String::new(),
        }
    }

    /// 流结束时收尾：把 `carry` 里剩下的（哪怕不完整的）字节尽量转成文本，不悄悄丢弃用户数据。
    fn finish(&mut self) -> String {
        if self.carry.is_empty() {
            return String::new();
        }
        let enc = self.resolved.unwrap_or(ResolvedEncoding::Utf8);
        let bytes = std::mem::take(&mut self.carry);
        match enc {
            ResolvedEncoding::Utf8 => String::from_utf8_lossy(&bytes).into_owned(),
            ResolvedEncoding::Gbk => win_codepage::decode(&bytes, win_codepage::CP_GBK)
                .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned()),
        }
    }

    /// 决定这条流该按 UTF-8 还是 GBK 解：显式指定就直接用；`auto` 对累计到的字节整体嗅探一次——
    /// 完整合法 UTF-8 就判 UTF-8；出现「确凿非法」（不是数据不完整，是真不合法）的字节就判 GBK；
    /// 样本不到 256 字节还判不出来就先不下结论，继续攒。
    fn sniff(&self) -> Option<ResolvedEncoding> {
        match self.mode {
            EncodingMode::Utf8 => Some(ResolvedEncoding::Utf8),
            EncodingMode::Gbk => Some(ResolvedEncoding::Gbk),
            EncodingMode::Auto => match std::str::from_utf8(&self.carry) {
                Ok(_) => Some(ResolvedEncoding::Utf8),
                Err(e) if e.error_len().is_some() => Some(ResolvedEncoding::Gbk),
                Err(_) if self.carry.len() >= 256 => Some(ResolvedEncoding::Utf8),
                Err(_) => None,
            },
        }
    }
}

/// 从 `carry` 中取出「当前已确定完整」的 UTF-8 前缀转成文本，剩余（不完整的）字节留在 `carry`。
fn drain_utf8(carry: &mut Vec<u8>) -> String {
    let valid_len = match std::str::from_utf8(carry) {
        Ok(_) => carry.len(),
        Err(e) => e.valid_up_to(),
    };
    if valid_len == 0 {
        return String::new();
    }
    let rest = carry.split_off(valid_len);
    let complete = std::mem::replace(carry, rest);
    // complete 已经过 valid_up_to()/from_utf8 校验为合法 UTF-8 前缀，这里理论不会失败；
    // 万一出现意外（不可达），退化为丢弃这一小段而不是 panic 拖垮整条读取线程。
    String::from_utf8(complete).unwrap_or_default()
}

/// 从 `carry` 中取出「当前已确定完整」的 GBK 前缀转成文本，剩余字节留在 `carry`。
fn drain_gbk(carry: &mut Vec<u8>) -> String {
    let complete_len = gbk_complete_prefix_len(carry);
    if complete_len == 0 {
        return String::new();
    }
    let rest = carry.split_off(complete_len);
    let complete = std::mem::replace(carry, rest);
    win_codepage::decode(&complete, win_codepage::CP_GBK)
        .unwrap_or_else(|| String::from_utf8_lossy(&complete).into_owned())
}

/// GBK 是「单字节 ASCII / 双字节（0x81-0xFE 引导 + 任意尾字节）」结构。这里只负责找出
/// 「不会把一个双字节字符从中间切断」的最长前缀，不校验尾字节是否真的合法——真正的解码
/// 校验交给 [`win_codepage::decode`]，这里的职责仅仅是不切错边界。
fn gbk_complete_prefix_len(buf: &[u8]) -> usize {
    let mut i = 0;
    while i < buf.len() {
        if (0x81..=0xFE).contains(&buf[i]) {
            if i + 1 < buf.len() {
                i += 2;
            } else {
                break; // 引导字节落在末尾，尾字节还没到货，留给下一块数据
            }
        } else {
            i += 1;
        }
    }
    i
}

/// 用 Win32 `MultiByteToWideChar` 按指定代码页解码字节串——专治 Windows 中文控制台程序
/// （如 adb、部分老 CLI 工具）默认用 GBK（代码页 936）输出而不是 UTF-8 的问题。
///
/// 为什么裸 FFI 而不是引 `encoding_rs` / `gbk` 之类的 crate：任务要求不引新依赖；这层转换
/// Windows 系统自带（`kernel32.dll`），直接 `extern "system"` 声明就能用，不用因为这一个
/// 函数去 Cargo.toml 给 `windows`/`windows-sys` 加 `Win32_Globalization` feature。
#[cfg(windows)]
mod win_codepage {
    use std::os::raw::{c_int, c_uint};

    #[link(name = "kernel32")]
    extern "system" {
        fn MultiByteToWideChar(
            code_page: c_uint,
            dw_flags: c_uint,
            lp_multi_byte_str: *const u8,
            cb_multi_byte: c_int,
            lp_wide_char_str: *mut u16,
            cch_wide_char: c_int,
        ) -> c_int;
    }

    /// GBK 代码页（简体中文，兼容 GB2312）。
    pub const CP_GBK: u32 = 936;

    /// 用给定 Windows 代码页把字节串解码为 `String`；系统不支持该代码页或转换失败时返回
    /// `None`（调用方应退化为 UTF-8 宽松解码，不能让「解码失败」变成整段用户数据被吞掉）。
    pub fn decode(bytes: &[u8], code_page: u32) -> Option<String> {
        if bytes.is_empty() {
            return Some(String::new());
        }
        if bytes.len() > i32::MAX as usize {
            return None; // 不会真的发生（单次进程输出不可能这么大），防守式拒绝而不是截断吞数据
        }
        // SAFETY:
        // - 第一次调用只为拿「需要多少个 u16」的长度：lp_wide_char_str 传空指针 + cch_wide_char=0
        //   是 MultiByteToWideChar 官方文档明确支持的用法，不写任何内存。
        // - 第二次调用前按第一次返回的 len 精确分配了 len 个 u16 的缓冲区，并把该长度原样传给
        //   cch_wide_char，系统调用因此不会越界写（这是该 Win32 API 的既定契约）。
        // - bytes / buf 在两次调用期间都作为局部变量存活，函数返回前不会被释放，指针全程有效。
        let len = unsafe {
            MultiByteToWideChar(
                code_page,
                0,
                bytes.as_ptr(),
                bytes.len() as c_int,
                std::ptr::null_mut(),
                0,
            )
        };
        if len <= 0 {
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        // SAFETY：同上一次调用；buf 长度精确等于 len，写入不会越界。
        let written = unsafe {
            MultiByteToWideChar(
                code_page,
                0,
                bytes.as_ptr(),
                bytes.len() as c_int,
                buf.as_mut_ptr(),
                len,
            )
        };
        if written <= 0 {
            return None;
        }
        String::from_utf16(&buf[..written as usize]).ok()
    }
}

#[cfg(not(windows))]
mod win_codepage {
    pub const CP_GBK: u32 = 936;
    /// 非 Windows 平台没有这套系统代码页转换，统一交给调用方退化为 UTF-8 宽松解码。
    pub fn decode(_bytes: &[u8], _code_page: u32) -> Option<String> {
        None
    }
}

// ==================== 子进程构造（不经 shell） ====================

/// 构造一条「不经 cmd.exe」的命令：显式 program + args，元字符不被解释，无 shell 注入面。
/// `with_new_process_group`：流式会话需要它才能用 [`plugin_exec_quit`] 精确点杀这一个进程组，
/// 一次性 `plugin_exec` 没有「优雅退出」这回事，不需要。
#[cfg(windows)]
fn new_command(program: &str, args: &[String], with_new_process_group: bool) -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let mut cmd = Command::new(program);
    cmd.args(args);
    let mut flags = CREATE_NO_WINDOW;
    if with_new_process_group {
        flags |= CREATE_NEW_PROCESS_GROUP;
    }
    cmd.creation_flags(flags);
    cmd
}

#[cfg(not(windows))]
fn new_command(program: &str, args: &[String], _with_new_process_group: bool) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd
}

fn build_piped_command(program: &str, args: &[String], opts: &ExecOptions, new_group: bool) -> Command {
    let mut cmd = new_command(program, args, new_group);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = opts.cwd.as_deref() {
        cmd.current_dir(dir);
    }
    cmd
}

// ==================== plugin_exec：一次性执行 ====================

/// 执行程序并等它跑完，拿到 `{ code, stdout, stderr }`。需已获 `runCommand` 授权。
///
/// 长耗时（子进程本身可能跑很久），放到 blocking 线程池执行，不占用/阻塞 UI 线程。
#[tauri::command]
pub async fn plugin_exec(
    program: String,
    args: Vec<String>,
    opts: Option<ExecOptions>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<ExecResult, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "runCommand") {
        return Err("插件未获授权执行程序（请在「插件管理」里授权 runCommand）".to_string());
    }
    if program.trim().is_empty() {
        return Err("program 为空".to_string());
    }
    let opts = opts.unwrap_or_default();
    let mode = resolve_encoding(opts.encoding.as_deref());
    let timeout_ms = opts.timeout_ms;
    let program_owned = program.clone();
    tauri::async_runtime::spawn_blocking(move || {
        run_and_collect(&program_owned, &args, &opts, timeout_ms, mode)
    })
    .await
    .map_err(|e| format!("执行线程异常: {e}"))?
}

/// `plugin_exec` 的实际执行体：spawn → 并行读 stdout/stderr（避免管道打满死锁）→ 轮询等待，
/// 超时就 kill 后继续等它真正退出 → 收线程 → 按 `mode` 解码整段输出。
fn run_and_collect(
    program: &str,
    args: &[String],
    opts: &ExecOptions,
    timeout_ms: Option<u64>,
    mode: EncodingMode,
) -> Result<ExecResult, String> {
    let mut cmd = build_piped_command(program, args, opts, false);
    let mut child = cmd.spawn().map_err(|e| format!("启动进程失败: {e}"))?;

    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| "拿不到子进程 stdout".to_string())?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| "拿不到子进程 stderr".to_string())?;
    // 一次性执行会把全部输出读进内存，必须有上限：某些命令（find / 日志导出 / 递归列目录）
    // 能吐出几百 MB 甚至无穷无尽，`read_to_end` 会一路吃到内存耗尽把整个 iTools 拖死。
    // 达到上限就停止读取并在结果里标出——**不静默截断**，静默截断会让插件拿到半截输出
    // 却以为拿全了，据此做判断（比如「设备列表为空」）就是错的。
    // 要处理大输出请用 execStream 边收边处理。
    let out_handle = std::thread::spawn(move || read_capped(&mut stdout_pipe));
    let err_handle = std::thread::spawn(move || read_capped(&mut stderr_pipe));

    let start = std::time::Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => {
                // try_wait 本身查询失败极罕见（如句柄失效）；按「状态未知」处理，防守式先杀掉
                // 避免残留一个我们已经跟丢、拿不到句柄结果的僵尸子进程。
                let _ = child.kill();
                break child.wait().ok();
            }
        }
        if let Some(t) = timeout_ms {
            if start.elapsed().as_millis() as u64 >= t {
                timed_out = true;
                let _ = child.kill();
                // kill 之后阻塞等它真正退出——TerminateProcess 生效很快，这里不会久等。
                break child.wait().ok();
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };

    let (stdout_bytes, out_cut) = out_handle.join().unwrap_or_default();
    let (stderr_bytes, err_cut) = err_handle.join().unwrap_or_default();
    let code = status.and_then(|s| s.code()).unwrap_or(-1);

    Ok(ExecResult {
        code,
        stdout: decode_full(&stdout_bytes, mode),
        stderr: decode_full(&stderr_bytes, mode),
        timed_out,
        truncated: out_cut || err_cut,
    })
}

// ==================== plugin_exec_stream：流式执行 ====================

fn new_stream_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{now_ns:x}-{n:x}")
}

/// 启动一条流式执行会话，返回 `stream_id`。stdout/stderr 增量经 [`EVT_STDOUT`]/[`EVT_STDERR`]
/// 事件推给**发起调用的窗口**（只推给它，不广播给别的插件窗口），进程结束推 [`EVT_EXIT`]。
/// 需已获 `runCommand` 授权。
#[tauri::command]
pub fn plugin_exec_stream(
    program: String,
    args: Vec<String>,
    opts: Option<ExecOptions>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    state: State<'_, ExecState>,
) -> Result<String, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "runCommand") {
        return Err("插件未获授权执行程序（请在「插件管理」里授权 runCommand）".to_string());
    }
    if program.trim().is_empty() {
        return Err("program 为空".to_string());
    }
    let opts = opts.unwrap_or_default();
    let mode = resolve_encoding(opts.encoding.as_deref());
    let timeout_ms = opts.timeout_ms;

    let mut cmd = build_piped_command(&program, &args, &opts, true);
    let mut child = cmd.spawn().map_err(|e| format!("启动进程失败: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "拿不到子进程 stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "拿不到子进程 stderr".to_string())?;

    let stream_id = new_stream_id();
    let app = webview.app_handle().clone();
    let label = webview.label().to_string();
    let child_arc = Arc::new(Mutex::new(child));

    {
        let mut sessions = lock(&state.sessions);
        sessions.insert(
            stream_id.clone(),
            StreamSession {
                owner: session.clone(),
                child: child_arc.clone(),
            },
        );
    }
    // program 只记**末段**：插件完全可以把 fs.pickFile 选出来的完整路径传进来，
    // 那条路径里通常带着用户名、项目名这类不该长期躺在 release 日志里的东西
    //（准则第二节：用户自选路径只留末段）。留文件名足够回答排查时唯一要问的
    //「到底是哪个程序被拉起来了」，而目录段对这个问题没有任何价值。
    ilog!(
        "[iTools][plugin] exec_stream 启动: 来自 {}，program={}，argc={}，stream={stream_id}",
        who(&session),
        std::path::Path::new(&program)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| program.clone()),
        args.len()
    );

    spawn_reader_thread(app.clone(), label.clone(), stream_id.clone(), EVT_STDOUT, stdout, mode);
    spawn_reader_thread(app.clone(), label.clone(), stream_id.clone(), EVT_STDERR, stderr, mode);
    spawn_waiter_thread(app, label, stream_id.clone(), child_arc, timeout_ms);

    Ok(stream_id)
}

/// 持续读一路管道（stdout 或 stderr），增量解码、非空就发事件；EOF（进程结束/句柄关闭）退出
/// 循环后，把解码器里剩余的尾巴字节也 flush 一次，不悄悄丢最后一小段输出。
fn spawn_reader_thread<R: Read + Send + 'static>(
    app: tauri::AppHandle,
    label: String,
    stream_id: String,
    event_name: &'static str,
    mut pipe: R,
    mode: EncodingMode,
) {
    std::thread::spawn(move || {
        let mut decoder = StreamDecoder::new(mode);
        let mut buf = [0u8; 8192];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let text = decoder.push(&buf[..n]);
                    if !text.is_empty() {
                        emit_to_plugin(
                            &app,
                            label.as_str(),
                            event_name,
                            &ExecChunk {
                                stream_id: stream_id.clone(),
                                data: text,
                            },
                        );
                    }
                }
                // 管道异常（如进程被强杀导致句柄失效）：当作流结束处理，不重试、不 panic。
                Err(_) => break,
            }
        }
        let tail = decoder.finish();
        if !tail.is_empty() {
            emit_to_plugin(
                &app,
                label.as_str(),
                event_name,
                &ExecChunk { stream_id, data: tail },
            );
        }
    });
}

/// 后台等待子进程结束：自然退出 / 超时强杀 / 被 [`plugin_exec_kill`] 或 [`plugin_exec_quit`]
/// 从外部结束，这里都统一感知得到（`try_wait` 轮询）。结束后推 exit 事件，并把这条会话从
/// [`ExecState`] 里摘除——这是「进程表不泄漏」的唯一收口点，不管会话是怎么结束的都会走到这。
fn spawn_waiter_thread(
    app: tauri::AppHandle,
    label: String,
    stream_id: String,
    child: Arc<Mutex<Child>>,
    timeout_ms: Option<u64>,
) {
    std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let mut timed_out = false;
        let status = loop {
            {
                let mut guard = lock(&child);
                match guard.try_wait() {
                    Ok(Some(status)) => break Some(status),
                    Ok(None) => {}
                    Err(_) => {
                        let _ = guard.kill();
                        break None;
                    }
                }
            }
            if let Some(t) = timeout_ms {
                if !timed_out && start.elapsed().as_millis() as u64 >= t {
                    timed_out = true;
                    let mut guard = lock(&child);
                    let _ = guard.kill();
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let code = status.and_then(|s| s.code()).unwrap_or(-1);
        emit_to_plugin(
            &app,
            label.as_str(),
            EVT_EXIT,
            &ExecExit {
                stream_id: stream_id.clone(),
                code,
                timed_out,
            },
        );
        if let Some(state) = app.try_state::<ExecState>() {
            let mut sessions = lock(&state.sessions);
            sessions.remove(&stream_id);
        }
    });
}

/// 取出某 `stream_id` 对应的子进程句柄，带归属校验：**完整会话身份**（id + dev 标志）必须与
/// 当前调用窗口一致，否则拒绝——理由同模块头注释，调试/正式同名插件不能互相 kill 对方的进程。
fn take_owned_child(
    state: &ExecState,
    stream_id: &str,
    session: &ActiveSession,
) -> Result<Arc<Mutex<Child>>, String> {
    let sessions = lock(&state.sessions);
    match sessions.get(stream_id) {
        None => Err("执行会话不存在或已结束".to_string()),
        Some(s) if !s.owner.same_as(session) => Err("该执行会话不属于本插件".to_string()),
        Some(s) => Ok(s.child.clone()),
    }
}

/// 强制结束一条流式执行会话（`TerminateProcess`）。需已获 `runCommand` 授权 + 会话归属本插件。
#[tauri::command]
pub fn plugin_exec_kill(
    stream_id: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    state: State<'_, ExecState>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "runCommand") {
        return Err("插件未获授权执行程序（请在「插件管理」里授权 runCommand）".to_string());
    }
    let child = take_owned_child(&state, &stream_id, &session)?;
    ilog!("[iTools][plugin] exec_kill: 来自 {}，stream={stream_id}", who(&session));
    let mut guard = lock(&child);
    guard.kill().map_err(|e| format!("强制结束进程失败: {e}"))
}

/// 礼貌地请一条流式执行会话自己退出（Windows 上是给它的控制台进程组发 `CTRL_BREAK`，不是
/// 硬杀）。程序若没有自定义处理这个信号，系统默认动作通常也是终止，但至少给了它走正常
/// 退出路径（刷新缓冲区、跑 atexit 之类清理逻辑）的机会，这一点区别于 [`plugin_exec_kill`]
/// 的 `TerminateProcess`（零清理、立即死亡）。需已获 `runCommand` 授权 + 会话归属本插件。
#[tauri::command]
pub fn plugin_exec_quit(
    stream_id: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    state: State<'_, ExecState>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "runCommand") {
        return Err("插件未获授权执行程序（请在「插件管理」里授权 runCommand）".to_string());
    }
    let child = take_owned_child(&state, &stream_id, &session)?;
    ilog!("[iTools][plugin] exec_quit: 来自 {}，stream={stream_id}", who(&session));
    quit_process(&child)
}

#[cfg(windows)]
fn quit_process(child: &Arc<Mutex<Child>>) -> Result<(), String> {
    let pid = lock(child).id();
    win_ctrl::send_ctrl_break(pid)
}

#[cfg(not(windows))]
fn quit_process(child: &Arc<Mutex<Child>>) -> Result<(), String> {
    // 非 Windows 没有 CTRL_BREAK 这套控制台信号机制；诚实起见，这条路径目前等同硬杀，
    // 不假装自己做了「优雅」这件事。
    lock(child).kill().map_err(|e| format!("结束进程失败: {e}"))
}

#[cfg(windows)]
mod win_ctrl {
    //! 给子进程发 `CTRL_BREAK` 请求它自行退出，而不是 `TerminateProcess` 硬杀。
    //!
    //! `GenerateConsoleCtrlEvent` 只能对「当前线程已 attach 的那个控制台」上的进程组生效，
    //! 而 iTools 主进程是 GUI 应用、默认没有控制台。所以每次都要：`AttachConsole(目标 pid)`
    //! 临时接到目标进程的控制台上 → 发信号 → `FreeConsole` 脱离，恢复到「没有控制台」的原状。
    //!
    //! 子进程必须在创建时带了 `CREATE_NEW_PROCESS_GROUP`（见 `super::new_command`），这样它
    //! 的进程组 id 就是它自己的 pid；`GenerateConsoleCtrlEvent` 指定这个非零的进程组 id 时，
    //! 事件**只会**送到这个组里的进程，不会波及同一控制台上别的进程（包括我们自己短暂 attach
    //! 上去的宿主进程）——这正是用「进程组」而不是广播（组 id 传 0）的原因，不需要额外去改
    //! 宿主自己的 Ctrl 处理器状态，天然安全。

    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(dw_process_id: u32) -> i32;
        fn FreeConsole() -> i32;
        fn GenerateConsoleCtrlEvent(dw_ctrl_event: u32, dw_process_group_id: u32) -> i32;
    }

    const CTRL_BREAK_EVENT: u32 = 1;

    /// 给 `pid` 所在的进程组发 `CTRL_BREAK`。要求该进程创建时带了 `CREATE_NEW_PROCESS_GROUP`
    /// （否则这个信号会波及同组里的其它进程——本模块所有子进程都满足这个前提）。
    pub fn send_ctrl_break(pid: u32) -> Result<(), String> {
        // SAFETY：这三个 Win32 调用都不接受/返回需要手动管理生命周期的资源句柄或函数指针，
        // 只操作「调用线程当前 attach 的控制台」这个进程级状态；pid 是调用方从
        // `std::process::Child::id()` 拿到的、此刻仍在会话表里的进程 id——即便该进程已经
        // 提前退出，`AttachConsole` 对不存在的进程也只会返回失败（0），不会有 UB。
        unsafe {
            // 宿主是 GUI 进程，正常情况下本来就没有控制台；FreeConsole 在这里失败是预期情况
            // （没有可释放的），忽略返回值。
            let _ = FreeConsole();
            if AttachConsole(pid) == 0 {
                return Err(format!("附加目标进程控制台失败（pid={pid}，可能已退出）"));
            }
            let ok = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
            // 不管发送是否成功都要脱离，不能让宿主进程状态卡在「附着在别人控制台上」。
            let _ = FreeConsole();
            if ok == 0 {
                return Err(format!("发送 CTRL_BREAK 失败（pid={pid}）"));
            }
        }
        Ok(())
    }
}

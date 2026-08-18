//! 主进程（**非提权**）↔ 索引守护进程（**提权**）之间的命名管道通道。
//!
//! # 为什么要跨进程，而不是让 iTools 整体提权
//!
//! 读 MFT 必须管理员（`volume.rs` 有实测取证），但给主程序挂
//! `requestedExecutionLevel=requireAdministrator` 会连带三个副作用，对一个**启动器**是致命的：
//!
//! 1. 提权进程 `ShellExecute` 出来的子进程**继承管理员令牌**——用 iTools 启动的每个程序
//!    都变成管理员运行（Chrome 会直接拒绝启动，编辑器会弹警告，之后它们创建的文件权限也变了）；
//! 2. `tauri-plugin-autostart` 走注册表 `Run` 键，而 UAC 会**静默跳过**要求提权的 Run 项
//!    ——「开机自启」会变成一个点了没反应的开关；
//! 3. UIPI 让高完整性级别的窗口收不到普通进程的拖放。
//!
//! 所以主进程保持普通权限，只把「读 MFT」这一件事隔离到提权子进程里。
//!
//! # 管道的安全描述符不能用默认值（这是本文件最容易踩的坑）
//!
//! 提权进程创建的内核对象默认带 **High** 完整性标签，而 Windows 的强制完整性控制策略是
//! `NO_WRITE_UP`：Medium 完整性的主进程要往管道**写**请求会被直接拒（`ERROR_ACCESS_DENIED`），
//! 哪怕 DACL 里明明写着同一个用户拥有全部权限。必须显式把标签降到 Medium（`S:(ML;;NW;;;ME)`）。
//! 同时 DACL 只授给**当前用户 SID** 与 SYSTEM——否则同机别的账号可以连上来枚举你的全盘文件名。

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// 单帧上限。查询请求只有几十字节；响应最多 `limit` 条命中，几十 KB 足够。
/// 设上限是为了让「对端发疯或不是我们的协议」时立刻失败，而不是无限分配内存。
const MAX_FRAME: usize = 4 << 20;

/// 客户端整轮往返的等待上限。超时即放弃本次查询让调用方降级——
/// 逐键搜索的场合，宁可这一次没结果，也不能把输入框卡住。
const CLIENT_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Serialize, Deserialize)]
pub enum Request {
    /// 子串查询（`needle` 由客户端负责转小写）
    Query { needle: String, limit: usize },
    /// 取索引状态（用于把「正在建索引 / 已就绪 / 哪些盘失败」诚实显示给用户）
    Status,
    /// 丢弃现有索引重建
    Rebuild,
    /// 让守护退出并释放索引占用的内存。
    ///
    /// 必须有这条：守护是**常驻**进程（主程序退出后它继续活着，好让下次开机即搜），
    /// 而它要吃掉几百 MB 内存。不给用户一个关掉的办法，就是背着他常驻——违反诚信红线。
    Shutdown,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct HitDto {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

/// 索引整体状态。字段直接用于前端展示，不要在前端再算一遍。
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct StatusDto {
    /// building=首次/重建中；ready=全部盘就绪；partial=部分盘失败
    pub state: String,
    /// 已就绪的盘符，如 ["C","D"]
    pub ready_drives: Vec<String>,
    /// 建索引失败的盘及原因（诚实暴露，不假装全盘可搜）
    pub failed_drives: Vec<(String, String)>,
    /// 可搜索条目总数
    pub entries: usize,
    /// 索引真实占用（MB），非估算
    pub memory_mb: usize,
    /// 因排除规则跳过的条目数
    pub excluded: usize,
    /// 最近一次查询在**守护侧**花的毫秒数（不含管道往返与客户端开销）。
    ///
    /// 为什么要单独报这个：客户端量到的往返时间里混着 IPC、JSON、以及调用方自己的开销，
    /// 用它来判断「搜索本身快不快」会得出错误结论（2026-08-17 就靠这个字段才认清
    /// PowerShell 测出的 176 ms 里真正属于搜索的只有一小部分）。0 = 还没查过。
    pub last_query_ms: u64,
}

#[derive(Serialize, Deserialize)]
pub enum Response {
    Hits(Vec<HitDto>),
    Status(StatusDto),
    Ok,
    Error(String),
}

/// 管道名按**用户 SID** 区分：同机多个账号各自一份索引，互不可见也互不冲突。
pub fn pipe_name() -> String {
    let sid = current_user_sid().unwrap_or_else(|| "unknown".to_string());
    // SID 字符串形如 S-1-5-21-...，字符集只有字母数字和连字符，可直接用作管道名
    format!(r"\\.\pipe\itools-mft-{sid}")
}

/// 从当前进程令牌取用户 SID 的字符串形式。
fn current_user_sid() -> Option<String> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = HANDLE::default();
    // SAFETY: GetCurrentProcess 是伪句柄无需释放；token 由下面的 CloseHandle 收回。
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.ok()?;
    let mut size = 0u32;
    // 第一次调用只为问出所需长度，返回 ERROR_INSUFFICIENT_BUFFER 是预期行为
    // SAFETY: 传 None 缓冲 + 接收长度，是该 API 的标准两段式用法。
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut size) };
    if size == 0 {
        // SAFETY: token 来自上面成功的 OpenProcessToken
        unsafe { let _ = CloseHandle(token); }
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    // SAFETY: buf 长度恰为上一步问出的 size；写入的是 TOKEN_USER + 尾随 SID。
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            size,
            &mut size,
        )
    };
    // SAFETY: 同上，token 只关一次
    unsafe { let _ = CloseHandle(token); }
    ok.ok()?;
    // SAFETY: buf 已被内核填成合法 TOKEN_USER；其 User.Sid 指向 buf 内的尾随 SID，
    // 在本函数返回前一直有效。
    let sid = unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid };
    let mut pwstr = windows::core::PWSTR::null();
    // SAFETY: sid 有效；成功后 pwstr 指向由 LocalAlloc 分配的串，下面用 LocalFree 释放。
    unsafe { ConvertSidToStringSidW(sid, &mut pwstr) }.ok()?;
    // SAFETY: pwstr 是 API 写好的 NUL 结尾宽串
    let s = unsafe { pwstr.to_string() }.ok();
    // SAFETY: pwstr 由 ConvertSidToStringSidW 用 LocalAlloc 分配，须用 LocalFree 释放
    unsafe { let _ = LocalFree(Some(HLOCAL(pwstr.as_ptr() as *mut _))); }
    s
}

// ---------- 帧编解码 ----------

fn write_frame(w: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
    if bytes.len() > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "帧超长",
        ));
    }
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(bytes)?;
    w.flush()
}

fn read_frame(r: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("帧声明长度 {len} 超出上限"),
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

// ---------- 客户端（主进程侧） ----------

/// 发一个请求并等响应。整轮限时 [`CLIENT_TIMEOUT`]，超时/管道不存在都返回 `None`，
/// 由调用方降级到别的搜索后端。
///
/// 每次调用建一条**短连接**：命名管道本地连接是亚毫秒级的，相对一次搜索可以忽略，
/// 换来的是不必在主进程里维护长连接的重连状态机。
///
/// 想知道搜索本身花了多久，看 [`StatusDto::last_query_ms`]（守护自计时）——
/// 别拿这里的往返时间去推断，那里面混着 IPC 与调用方自己的开销。
pub fn request(req: &Request) -> Option<Response> {
    request_on(pipe_name(), req)
}

/// 往**指定**管道名发一个请求。生产路径只用 [`pipe_name`]（见 [`request`]）。
///
/// 之所以把名字提成参数：单测里 [`serve`] 是**死循环、起了就不退**，如果它占的是生产
/// 管道名，同进程里那些「守护缺席时必须降级」的用例（`mft/mod.rs`、`search/mod.rs`）
/// 前提就被破坏了——测试之间不能靠执行顺序才成立。
fn request_on(name: String, req: &Request) -> Option<Response> {
    let payload = serde_json::to_vec(req).ok()?;
    let (tx, rx) = mpsc::channel();
    // 同步命名管道 IO 没法直接设超时，故整轮放到临时线程，主线程限时等结果。
    // 超时后该线程仍会自行结束（对端要么回包要么断开），不会长期泄漏。
    std::thread::spawn(move || {
        let _ = tx.send(round_trip(&name, &payload));
    });
    match rx.recv_timeout(CLIENT_TIMEOUT) {
        Ok(Ok(bytes)) => serde_json::from_slice(&bytes).ok(),
        _ => None,
    }
}

fn round_trip(name: &str, payload: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut pipe = connect(name)?;
    write_frame(&mut pipe, payload)?;
    read_frame(&mut pipe)
}

/// 连管道，撞上「所有实例都忙」时等一下重试。
///
/// # 为什么必须重试（不是防御性编程，是必然会发生的竞态）
///
/// 命名管道的服务端实例是**一对一**的：一个实例被客户端占用后，服务端得再造一个新实例
/// 才能接下一位。哪怕服务端开了多个 acceptor，在「全部实例都刚被占用、新实例还没造好」
/// 的那一瞬间，新来的客户端会收到 `ERROR_PIPE_BUSY(231)` 而不是排队。
///
/// `/f` 是**逐键搜索**——用户打「报告」会连着发好几个查询，撞上这个窗口期是常态而非异常。
/// 直接失败会让某几个按键「没有结果」，看起来像搜索时好时坏。
fn connect(name: &str) -> std::io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    use windows::Win32::System::Pipes::WaitNamedPipeW;
    use windows::core::HSTRING;

    const ERROR_PIPE_BUSY: i32 = 231;
    /// 重试次数 × 每次最多等 100 ms，总计仍远小于 CLIENT_TIMEOUT
    const RETRIES: usize = 8;

    let mut last = std::io::Error::other("未尝试连接");
    for _ in 0..RETRIES {
        match OpenOptions::new().read(true).write(true).open(name) {
            Ok(f) => return Ok(f),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                let wide = HSTRING::from(name);
                // SAFETY: wide 是 NUL 结尾宽串，调用期间存活。有实例空出来即返回，
                // 最多等 100 ms；返回 Err 只表示这次没等到，由外层循环再试。
                unsafe {
                    let _ = WaitNamedPipeW(&wide, 100);
                }
                last = e;
            }
            // 管道不存在（守护没起）等情况直接失败，不值得重试
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

/// 守护进程是否已在监听（管道存在即视为在）。
pub fn daemon_alive() -> bool {
    matches!(request(&Request::Status), Some(Response::Status(_)))
}

// ---------- 服务端（守护进程侧） ----------

/// 并发 acceptor 数量。
///
/// 必须 > 1：命名管道实例是一对一的，单 acceptor 在「处理请求」和「造下一个实例」这两段
/// 时间里**没有任何实例在监听**，逐键搜索的连续查询会成片撞上 `ERROR_PIPE_BUSY`。
/// 开 4 个让总有实例在等；配合客户端的重试（见 [`connect`]），实际不会漏查询。
const ACCEPTORS: usize = 4;

/// 阻塞式监听。开 [`ACCEPTORS`] 条线程各自「造实例 → 等连接 → 处理一轮 → 关闭」。
///
/// `handler` 会被多条线程并发调用，故要求 `Sync`。
pub fn serve<H>(handler: H) -> std::io::Result<()>
where
    H: Fn(Request) -> Response + Send + Sync + 'static,
{
    serve_on(pipe_name(), handler)
}

/// 在**指定**管道名上监听。名字提成参数的理由同 [`request_on`]：单测要用独立的名字，
/// 否则这个永不返回的循环会一直占着生产管道名。
fn serve_on<H>(name: String, handler: H) -> std::io::Result<()>
where
    H: Fn(Request) -> Response + Send + Sync + 'static,
{
    use std::sync::Arc;
    let handler = Arc::new(handler);
    for _ in 1..ACCEPTORS {
        let handler = Arc::clone(&handler);
        let name = name.clone();
        std::thread::spawn(move || {
            // 解引用成 &H 再传：Arc<H> 自身**不实现** Fn（只有 Box 有那层 blanket impl），
            // 直接传 &Arc<H> 会让 accept_loop 的 H 被推断成 Arc<H> 而不满足约束。
            if let Err(e) = accept_loop(&name, &*handler) {
                eprintln!("[mft-daemon] acceptor 线程退出: {e}");
            }
        });
    }
    // 主线程也当一个 acceptor：它返回即整个守护结束
    accept_loop(&name, &*handler)
}

fn accept_loop<H>(name: &str, handler: &H) -> std::io::Result<()>
where
    H: Fn(Request) -> Response,
{
    loop {
        // 创建/等待失败通常是致命的（名字被占、安全描述符非法），返回让进程退出
        let mut pipe = accept_one(name)?;
        // 一条连接只处理一个请求-响应然后关闭：与客户端的短连接策略对齐。
        // 同步处理（不再 spawn）——一次查询实测约 12 ms，4 个 acceptor 足够。
        let result = read_frame(&mut pipe).and_then(|bytes| {
            let req: Request = serde_json::from_slice(&bytes)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
            let resp = handler(req);
            let out = serde_json::to_vec(&resp)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
            write_frame(&mut pipe, &out)
        });
        if let Err(e) = result {
            // 客户端超时后主动断开是常态（BrokenPipe），不值得刷日志
            if e.kind() != std::io::ErrorKind::BrokenPipe {
                eprintln!("[mft-daemon] 处理连接失败: {e}");
            }
        }
    }
}

/// 在 `name` 上创建一个管道实例并等一个客户端接进来，返回可读写的句柄。
fn accept_one(name: &str) -> std::io::Result<std::fs::File> {
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::Foundation::{HLOCAL, INVALID_HANDLE_VALUE, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::{FILE_FLAGS_AND_ATTRIBUTES, PIPE_ACCESS_DUPLEX};
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows::core::HSTRING;

    let sid = current_user_sid().unwrap_or_else(|| "BA".to_string());
    // D: 只给当前用户(GA=全部)与 SYSTEM；S: 把完整性标签降到 Medium，
    // 否则 Medium 完整性的主进程写不进这个由 High 完整性进程创建的管道。
    let sddl = HSTRING::from(format!("D:(A;;GA;;;{sid})(A;;GA;;;SY)S:(ML;;NW;;;ME)"));
    let mut psd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: sddl 是 NUL 结尾宽串；成功后 psd 指向 LocalAlloc 的内存，函数末尾 LocalFree。
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &sddl,
            SDDL_REVISION_1,
            &mut psd,
            None,
        )
    }
    .map_err(std::io::Error::other)?;

    let sa = SECURITY_ATTRIBUTES {
        nLength: core::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: psd.0,
        bInheritHandle: false.into(),
    };
    let name = HSTRING::from(name);
    // SAFETY: name/sa 在调用期间存活；PIPE_UNLIMITED_INSTANCES 允许并发连接。
    let handle = unsafe {
        CreateNamedPipeW(
            &name,
            FILE_FLAGS_AND_ATTRIBUTES(PIPE_ACCESS_DUPLEX.0),
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            255, // PIPE_UNLIMITED_INSTANCES 在部分绑定里是 255
            64 * 1024,
            64 * 1024,
            0,
            Some(&sa),
        )
    };
    // SAFETY: psd 由 ConvertStringSecurityDescriptor... 分配，CreateNamedPipeW 已复制其内容
    unsafe {
        let _ = LocalFree(Some(HLOCAL(psd.0)));
    }
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: handle 有效；ConnectNamedPipe 同步等待一个客户端。
    // ERROR_PIPE_CONNECTED 表示客户端在本调用前已接入，属成功。
    let connected = unsafe { ConnectNamedPipe(handle, None) };
    if let Err(e) = connected {
        const ERROR_PIPE_CONNECTED: i32 = 535;
        if e.code().0 & 0xFFFF != ERROR_PIPE_CONNECTED {
            // SAFETY: 失败路径要收回句柄，否则每次失败泄漏一个
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(handle);
            }
            return Err(std::io::Error::other(e));
        }
    }
    // SAFETY: 句柄所有权转移给 File，由它负责 CloseHandle
    Ok(unsafe { std::fs::File::from_raw_handle(handle.0 as *mut _) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 管道名必须带上真实 SID——否则同机多账号会抢同一个管道
    #[test]
    fn pipe_name_is_per_user() {
        let name = pipe_name();
        println!("管道名：{name}");
        assert!(name.starts_with(r"\\.\pipe\itools-mft-"));
        assert!(
            name.contains("S-1-"),
            "应含真实用户 SID，实得 {name}（取 SID 失败会退化成 unknown）"
        );
    }

    /// 帧编解码往返
    #[test]
    fn frame_round_trip() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, b"hello").expect("写帧失败");
        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut cursor).expect("读帧失败"), b"hello");
    }

    /// 声明长度超上限必须报错，而不是尝试分配那么大的内存
    #[test]
    fn oversized_frame_is_rejected() {
        let mut data = ((MAX_FRAME + 1) as u32).to_le_bytes().to_vec();
        data.extend_from_slice(b"x");
        let mut cursor = std::io::Cursor::new(data);
        assert!(read_frame(&mut cursor).is_err(), "超长帧应被拒绝");
    }

    /// 请求/响应的 JSON 往返（协议两端各自 serde，字段错位会在这里暴露）
    #[test]
    fn protocol_serde_round_trip() {
        let req = Request::Query {
            needle: "报告".to_string(),
            limit: 30,
        };
        let bytes = serde_json::to_vec(&req).expect("序列化请求失败");
        match serde_json::from_slice::<Request>(&bytes).expect("反序列化请求失败") {
            Request::Query { needle, limit } => {
                assert_eq!(needle, "报告");
                assert_eq!(limit, 30);
            }
            _ => panic!("请求类型错"),
        }

        // 数字用本机 2026-08-17 的实测量级，别换成好看的圆整数——
        // 这里顺带充当「一份真实状态长什么样」的样本
        let resp = Response::Status(StatusDto {
            state: "ready".into(),
            ready_drives: vec!["C".into(), "F".into()],
            failed_drives: vec![("E".into(), "拒绝访问".into())],
            entries: 7_330_567,
            memory_mb: 358,
            excluded: 6_183_653,
            last_query_ms: 42,
        });
        let bytes = serde_json::to_vec(&resp).expect("序列化响应失败");
        match serde_json::from_slice::<Response>(&bytes).expect("反序列化响应失败") {
            Response::Status(s) => {
                assert_eq!(s.ready_drives, vec!["C", "F"]);
                assert_eq!(s.failed_drives[0].1, "拒绝访问");
                assert_eq!(s.last_query_ms, 42, "服务端自计时字段必须跨序列化保住");
                assert_eq!(s.entries, 7_330_567);
            }
            _ => panic!("响应类型错"),
        }
    }

    /// 守护未启动时，客户端必须**快速**返回 None 而不是卡住 UI
    #[test]
    fn client_fails_fast_without_daemon() {
        let start = std::time::Instant::now();
        let r = request(&Request::Status);
        let elapsed = start.elapsed();
        println!("无守护时耗时 {elapsed:?}，结果 is_some={}", r.is_some());
        assert!(
            elapsed < CLIENT_TIMEOUT + Duration::from_millis(500),
            "必须在超时上限内返回，实测 {elapsed:?}"
        );
    }

    /// 端到端：起一个服务端线程，客户端连上去问 Status。
    /// 这条同时验证了安全描述符可用（SDDL 写错会在 accept_one 直接失败）。
    ///
    /// ⚠ 必须用**独立的管道名**，不能用生产的 [`pipe_name`]：`serve_on` 是死循环、
    /// 起了就不退，占着生产名会让同进程里那些「守护缺席 → 必须降级」的用例
    /// （`mft::tests::query_returns_none_when_daemon_absent`、
    /// `search::tests::walkdir_fallback_index_is_built`）随执行顺序时红时绿。
    /// 这条曾真实发生过（全量 `cargo test` 报 `MftSearch::status().is_none()` 失败）。
    #[test]
    fn server_client_end_to_end() {
        let name = format!(r"\\.\pipe\itools-mft-test-e2e-{}", std::process::id());
        let server_name = name.clone();
        std::thread::spawn(move || {
            let _ = serve_on(server_name, |req| match req {
                Request::Status => Response::Status(StatusDto {
                    state: "ready".into(),
                    entries: 42,
                    ..Default::default()
                }),
                _ => Response::Error("未实现".into()),
            });
        });
        // 给服务端一点时间把管道创建出来
        for _ in 0..50 {
            if let Some(Response::Status(s)) = request_on(name.clone(), &Request::Status) {
                assert_eq!(s.entries, 42, "应拿到服务端给的状态");
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("5s 内没能与服务端完成一次往返");
    }
}

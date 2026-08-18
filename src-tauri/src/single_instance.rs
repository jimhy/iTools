//! 单实例守卫：同一台机器上同一种构建的 iTools 只允许跑一个。
//!
//! # 为什么需要
//!
//! iTools 是常驻托盘的启动器：全局热键、托盘图标、插件热键、MCP 端口（127.0.0.1:7345）、
//! MFT 索引守护管道、`%LOCALAPPDATA%\itools\itools.db` 全是**进程外的独占资源**。
//! 开第二个进程不会「多一个窗口」这么温和，而是热键被先注册的那个抢走、托盘多出一个点不响的
//! 图标、MCP 端口占用报错（os error 10048）、两个进程并发写同一个 SQLite。
//! 用户的期望也很朴素：再点一次图标应该是**把已有的窗口叫出来**，不是再开一份。
//!
//! # dev 与 release 必须能共存
//!
//! 开发时常态是「一边跑 release 正常用，一边跑 debug 调」。所以锁名按
//! `cfg!(debug_assertions)` 分成两套（[`mutex_name`] / [`activate_pipe_name`] 都分），
//! 两种构建各自单实例、互不打扰。**改动这两个函数时务必保住这条**，否则一编译 debug
//! 就会把用户正在用的 release 顶掉（或者反过来，debug 死活起不来）。
//!
//! # 机制
//!
//! - **判定**：`CreateMutexW` 建一个命名互斥体，`GetLastError() == ERROR_ALREADY_EXISTS`
//!   即说明同类实例已在跑。用互斥体而不是「扫进程列表」：内核对象的创建是原子的，
//!   两个进程同时启动也只会有一个拿到「首次创建」，扫进程则必然有竞态窗口。
//! - **唤起**：第二个进程通过命名管道给第一个发一个字节的信号，第一个收到就把主窗口显示出来。
//!   通道是单向的（服务端只读），信号不带内容——这里不需要协议，只需要「有人按了门铃」。

use std::sync::OnceLock;

use crate::logging::ilog;

/// 互斥体名。`Local\` 前缀 = **仅当前登录会话**可见。
///
/// ⚠ 不要改成 `Global\`：那是跨会话的全机名字空间，在多用户 / 远程桌面同时登录的机器上，
/// 另一个账号（或另一个 RDP 会话）里跑着的 iTools 会让本会话误判成「已有实例」，
/// 表现为「双击图标什么都没发生」——而那个「已有实例」的窗口在别的会话里，用户根本看不到。
/// 部分环境下建 `Global\` 对象还需要 `SeCreateGlobalPrivilege`。
fn mutex_name() -> &'static str {
    if cfg!(debug_assertions) {
        r"Local\iTools-SingleInstance-dev"
    } else {
        r"Local\iTools-SingleInstance-release"
    }
}

/// 构建类型标签，用于把 dev / release 的名字彻底分开。
fn build_tag() -> &'static str {
    if cfg!(debug_assertions) {
        "dev"
    } else {
        "release"
    }
}

/// 「唤起窗口」信号管道名。
///
/// 带**用户 SID**：同机多账号各自一个实例，谁也别把窗口叫到别人桌面上去。
///
/// ⚠ 已知取舍：名字里没有**会话号**，而互斥体用的是按会话隔离的 `Local\`。
/// 于是「同一个用户同时登录控制台 + 远程桌面」这种少见情形下，两个会话各自会有一个实例
///（互斥体不冲突，符合预期），但它们监听的是**同一个**管道名，第三次启动发出的唤起信号
/// 可能被另一个会话的实例接走——表现为「点了图标窗口没出来」。
/// 要根治得把 `ProcessIdToSessionId` 的结果也拼进名字里；当前 `windows` crate 没开
/// `Win32_System_RemoteDesktop` feature，且该场景对本产品极边缘，先如实记在这里。
fn activate_pipe_name() -> String {
    let sid = current_user_sid().unwrap_or_else(|| {
        // 取不到 SID 时退化成固定串：同机不同账号会共用同一个管道名，唤起信号可能被
        // 别人的实例接走。概率极低，但**必须留痕**——否则真出问题时日志里一片空白。
        ilog!("[iTools] 取当前用户 SID 失败，唤起管道名退化为 unknown（多账号环境下可能串扰）");
        "unknown".to_string()
    });
    // SID 字符串形如 S-1-5-21-...，字符集只有字母数字和连字符，可直接用作管道名
    format!(r"\\.\pipe\itools-activate-{}-{}", build_tag(), sid)
}

/// 门铃信号。内容无意义，收到即「请把窗口显示出来」。
const ACTIVATE_SIGNAL: &[u8] = b"show\n";

/// 并发 acceptor 数量。
///
/// 必须 > 1：命名管道实例是**一对一**的，单 acceptor 在「处理这一次唤起」和「造下一个实例」
/// 之间有一段没有任何实例在监听的空档，用户连点两下托盘/快捷方式就会撞上。
/// 2 个够用——唤起是极低频操作，不是 mft/ipc.rs 那种逐键搜索。
const ACCEPTORS: usize = 2;

/// acceptor 连续失败多少次才真的放弃监听（见 `serve_activation` 里的理由）。
const MAX_CONSECUTIVE_ERRORS: usize = 5;

/// acceptor 出错后的退避间隔，避免持续性故障把日志刷爆。
const ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_millis(200);

/// 通知已有实例时的重试上限。覆盖两种真实会撞上的时序：
/// 1. 已有实例的 acceptor 正好都在忙（ERROR_PIPE_BUSY）；
/// 2. 已有实例还卡在 `setup()` 里，管道**还没建**（ERROR_FILE_NOT_FOUND）
///    ——用户开机后连点两下快捷方式就是这个场景。
///
/// ⚠ 这个数字是**实测**定的，别随手调小：`acquire()` 在 Builder 之前就返回，
/// 而 `serve_activation()` 要等 setup 收尾，两者实测差 **874ms**（热机 + SSD + debug；
/// 冷启动 WebView2 首次初始化还要更久）。原先 6 次 × 250ms = 1.25s 只剩 375ms 余量，
/// 冷启动连点两下就会「第二次完全没反应」。24 次 = 6s，留足冗余；代价仅仅是极端情况下
/// 第二个进程在后台多活几秒（无窗口无托盘，用户完全无感）。
///
/// 放到模块级是为了让测试能引用它算期望值——写死在测试里的话，改这个常量就会失配
///（2026-08-18 从 6 调到 24 时就撞了一次）。
const RETRIES: usize = 24;

/// 每次重试之间的间隔。最坏总耗时 ≈ `RETRIES × RETRY_GAP`（约 6s），
/// 全程发生在**没有窗口的第二个进程**里，用户无感：他看到的是第一个实例就绪后窗口弹出。
const RETRY_GAP: std::time::Duration = std::time::Duration::from_millis(250);

/// 互斥体句柄的**长期持有者**。
///
/// ⚠ 这是本文件最容易写错的地方：句柄一旦被关闭，内核对象的最后一个引用就没了，
/// 互斥体随之销毁，下一个进程再 `CreateMutexW` 会拿到「首次创建」——单实例形同虚设。
/// 而这种 bug 手测时**看不出来**（第一个进程还活着的那一刻锁确实在），只有把 acquire()
/// 里的句柄写成局部变量并且将来某次重构给它套上 `Owned<HANDLE>` 之类的 RAII 包装才会爆。
/// 所以显式存进 static，进程活多久它活多久，**永不 CloseHandle**（进程退出时内核统一回收）。
///
/// 存 `usize` 而不是 `HANDLE`：Windows 句柄在 `windows` crate 里是裸指针 newtype，
/// 既不是 `Send` 也不是 `Sync`，塞不进 static。句柄本身只是内核对象表的索引，
/// 跨线程使用完全合法，用整数存值是标准做法。
static MUTEX_HANDLE: OnceLock<usize> = OnceLock::new();

/// 一份「Medium 完整性 + 只授当前用户与 SYSTEM」的安全描述符，连同指向它的
/// `SECURITY_ATTRIBUTES` 一起打包，保证两者生命周期一致。
///
/// 单独提出来是因为 `SECURITY_ATTRIBUTES.lpSecurityDescriptor` 是**裸指针**：
/// 若安全描述符先于结构体释放，`CreateMutexW` 读到的就是悬垂指针。把两者绑在同一个
/// 值里，只要这个值活着就都活着。
struct SecAttrs {
    sa: windows::Win32::Security::SECURITY_ATTRIBUTES,
    /// 由 `ConvertStringSecurityDescriptorToSecurityDescriptorW` 用 LocalAlloc 分配，
    /// Drop 时 LocalFree。
    psd: windows::Win32::Security::PSECURITY_DESCRIPTOR,
}

impl SecAttrs {
    fn attrs(&self) -> *const windows::Win32::Security::SECURITY_ATTRIBUTES {
        &self.sa
    }
}

impl Drop for SecAttrs {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        if !self.psd.is_invalid() {
            // SAFETY: psd 由 ConvertStringSecurityDescriptorToSecurityDescriptorW 分配，
            // 只在这里释放一次；CreateMutexW 已经把内容复制进内核对象，不再引用它。
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.psd.0)));
            }
        }
    }
}

/// 构造上述安全描述符。失败返回 `None`（调用方退回默认安全属性，行为与加固前一致）。
fn medium_integrity_sa() -> Option<SecAttrs> {
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::core::HSTRING;

    let sid = current_user_sid()?;
    // D: 只给当前用户与 SYSTEM 全权；S: 把完整性标签降到 Medium，
    // 这样提权实例建的互斥体，普通权限的实例也能打开（NO_WRITE_UP 不再挡）。
    let sddl = HSTRING::from(format!("D:(A;;GA;;;{sid})(A;;GA;;;SY)S:(ML;;NW;;;ME)"));
    let mut psd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: sddl 是 NUL 结尾宽串；成功后 psd 指向 LocalAlloc 的内存，由 SecAttrs::drop 释放。
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, SDDL_REVISION_1, &mut psd, None)
    }
    .ok()?;
    Some(SecAttrs {
        sa: SECURITY_ATTRIBUTES {
            nLength: core::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd.0,
            bInheritHandle: false.into(),
        },
        psd,
    })
}

/// 互斥体是否已存在——只申请 `SYNCHRONIZE` 这一最低权限去打开。
///
/// 用途见 [`acquire`]：`CreateMutexW` 因权限被拒时，不能就此认定「没有实例」。
/// `SYNCHRONIZE` 的门槛远低于 `MUTEX_ALL_ACCESS`，提权实例建的对象通常也能打开。
fn mutex_exists(name: &windows::core::HSTRING) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_ACCESS_RIGHTS};

    // 标准 Win32 的 SYNCHRONIZE（0x0010_0000）。windows crate 0.61 只在
    // FILE_ACCESS_RIGHTS 下导出了同名常量，类型对不上 OpenMutexW，故直接按值构造。
    const SYNCHRONIZE: SYNCHRONIZATION_ACCESS_RIGHTS =
        SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000);

    // SAFETY: name 是 NUL 结尾宽串；成功时拿到的句柄立刻关闭，不外传。
    match unsafe { OpenMutexW(SYNCHRONIZE, false, name) } {
        Ok(h) => {
            // SAFETY: h 来自上面成功的 OpenMutexW，只关一次
            unsafe {
                let _ = CloseHandle(h);
            }
            true
        }
        Err(_) => false,
    }
}

/// 尝试成为唯一实例。
///
/// 返回 `false` = 已有同类实例在跑（此时**已经通知它唤起窗口了**），调用方应立即退出本进程。
///
/// ⚠ 调用点必须在 `--mft-daemon` 拦截**之后**，理由见 `lib.rs::run` 里那段注释。
pub fn acquire() -> bool {
    use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::HSTRING;

    let name = HSTRING::from(mutex_name());
    // 不取所有权（binitialowner = false）：我们只把互斥体当「有没有人在」的存在性标记用，
    // 不做互斥临界区。取所有权反而要处理进程被杀后的 WAIT_ABANDONED 语义，纯属自找麻烦。
    //
    // 安全属性**不能传 None**（这一点 2026-08-18 的对抗审查抓到过）：用户「以管理员身份运行」
    // 过 iTools 时，那个 High 完整性进程建出来的互斥体默认带 High 标签，而 Windows 的强制
    // 完整性策略是 NO_WRITE_UP——之后普通双击起来的 Medium 进程请求 MUTEX_ALL_ACCESS 会被拒成
    // ERROR_ACCESS_DENIED，走到下面的 fail-open 分支，于是**真的开出第二个实例**：托盘两个图标、
    // 热键被先注册的抢走。所以显式把标签降到 Medium（与 mft/ipc.rs 的管道同一个道理），
    // 让提权实例建的互斥体也能被普通实例看见。
    let sd = medium_integrity_sa();
    let sa = sd.as_ref().map(|s| s.attrs());
    // SAFETY: name 是 NUL 结尾宽串且在调用期间存活；sa 指向的 SECURITY_ATTRIBUTES 与其
    // 安全描述符在本次调用期间都存活（sd 到函数末尾才 drop）。返回的句柄不在此处释放，
    // 成功路径交给 MUTEX_HANDLE 长期持有，冲突路径下面显式 CloseHandle。
    let created = unsafe { CreateMutexW(sa.map(|p| p as *const _), false, &name) };
    let handle = match created {
        Ok(h) => h,
        Err(e) => {
            // 建不出来时先别急着放行：旧版本（或别的什么东西）建的同名互斥体可能没有我们
            // 这套安全描述符，此时 CreateMutexW 会因权限被拒，但**对象确实存在**，
            // 说明已有实例在跑。用只要 SYNCHRONIZE 权限的 OpenMutexW 再探一次——
            // 这个权限低得多，通常能过。探到了就按「已有实例」处理，而不是开出第二个。
            if mutex_exists(&name) {
                ilog!(
                    "[iTools] 互斥体 {} 已存在但无权创建（多半是提权实例建的），                     按「已有实例」处理：{e}",
                    mutex_name()
                );
                notify_existing();
                return false;
            }
            // 确实建不出也探不到（同名对象类型冲突等），属于极罕见情况。
            // 这里**放行**而不是拦住启动：对一个启动器来说，「偶尔多开一个」远好过
            // 「双击了打不开，且没有任何界面提示」。如实记一笔日志便于事后定位。
            ilog!("[iTools] 单实例互斥体创建失败，本次不做单实例约束：{e}");
            return true;
        }
    };

    // ⚠ ERROR_ALREADY_EXISTS 时 CreateMutexW 依然**返回有效句柄**（不是失败），
    // 判据只能是紧随其后的 GetLastError。上面的 `windows` crate 包装在成功路径上
    // 不再调用任何 Win32 API，末次错误码不会被覆盖。
    // SAFETY: GetLastError 只读取本线程的末次错误码，无参数、无副作用。
    let already = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;

    if already {
        // 冲突路径也拿到了一个**有效句柄**，必须收回：互斥体是引用计数的，只要还有句柄
        // 没关，对象就不销毁。本进程接下来还要花最多 1.5s 重试通知对端（见 notify_on），
        // 这期间若第一个实例正好退出，多出来的这个引用会让互斥体继续「存在」，
        // 于是紧接着的第三次启动仍会被判成「已有实例」——而那时其实没有实例在跑了。
        // SAFETY: handle 来自上面成功的 CreateMutexW，只在这里关一次，之后不再使用。
        unsafe {
            let _ = CloseHandle(handle);
        }
        ilog!(
            "[iTools] 检测到已有 {} 实例在运行（互斥体 {}），本进程改为通知它唤起窗口。",
            build_tag(),
            mutex_name()
        );
        // 通知不成功时不能直接退出——可能对方正在退出，那样会「两头落空」。
        // 回头再抢一次锁：抢到就正常启动（见 reacquire_after_failed_notify）。
        if !notify_existing() {
            return reacquire_after_failed_notify();
        }
        return false;
    }

    // 首次创建成功：把句柄钉在 static 上（理由见 MUTEX_HANDLE 的注释）
    let _ = MUTEX_HANDLE.set(handle.0 as usize);
    true
}

/// 通知已有实例把窗口显示出来。失败不改变调用方的决定（照样退出本进程），只记日志。
fn notify_existing() -> bool {
    allow_foreground_handoff();
    notify_on(&activate_pipe_name())
}

/// 把「抢前台窗口」的权利让渡给**任意**进程，紧接着再发唤起信号。
///
/// # 不做这一步，窗口会弹出来但没有焦点
///
/// Windows 的前台锁规则：只有当前拥有前台权的进程才能 `SetForegroundWindow`，别人调用
/// 会被降级成任务栏闪烁。用户双击快捷方式时，拿到前台权的是**刚启动的第二个进程**，
/// 而真正去显示窗口的却是**第一个进程**（它此刻在后台）——于是搜索框要么只是闪一下任务栏，
/// 要么弹出来却不聚焦，用户敲的字进了上一个应用。
///
/// `AllowSetForegroundWindow(ASFW_ANY)` 就是把这份权利交出去。放 `ASFW_ANY` 而不是第一个
/// 实例的 PID：我们手上没有对方 PID（互斥体不携带），而这个让渡窗口极短（紧接着本进程就退出），
/// 风险可忽略。
///
/// 现有的全局热键 / 托盘点击路径没有这个问题：那两条是用户输入直接落在本进程，
/// Windows 会显式授予前台权。**只有新增的「第二个进程通知第一个」这条路径缺这一环。**
fn allow_foreground_handoff() {
    use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;
    const ASFW_ANY: u32 = u32::MAX;
    // SAFETY: 无指针参数；失败（本进程本来就没有前台权时会失败）无副作用，忽略即可。
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
    }
}

/// 往**指定**管道名发一次唤起信号。生产路径只用 [`notify_existing`]。
///
/// 名字提成参数是为了单测：直接测 [`notify_existing`] 会往**生产管道**发信号，
/// 本机正好开着 debug 版 iTools 时，跑一次 `cargo test` 就会把用户的搜索框弹出来。
fn notify_on(name: &str) -> bool {
    use std::io::Write;

    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_PIPE_BUSY: i32 = 231;

    // 重试次数 / 间隔见模块级的 RETRIES、RETRY_GAP（那里写了为什么是 24 次）。
    let mut last = String::from("未尝试");
    for i in 0..RETRIES {
        // 只写不读：服务端是 PIPE_ACCESS_INBOUND 单向管道（见 accept_one）
        match std::fs::OpenOptions::new().write(true).open(name) {
            Ok(mut pipe) => {
                match pipe.write_all(ACTIVATE_SIGNAL).and_then(|_| pipe.flush()) {
                    Ok(()) => {
                        ilog!("[iTools] 已通知已有实例唤起窗口（{name}）");
                        return true;
                    }
                    Err(e) => last = format!("写信号失败：{e}"),
                }
            }
            Err(e) => {
                let code = e.raw_os_error().unwrap_or(0);
                last = format!("连接管道失败：{e}");
                // 只有这两种错值得再等一下；别的（如拒绝访问）重试也不会变好
                if code != ERROR_PIPE_BUSY && code != ERROR_FILE_NOT_FOUND {
                    break;
                }
            }
        }
        if i + 1 < RETRIES {
            std::thread::sleep(RETRY_GAP);
        }
    }
    // 对端可能正在退出（互斥体还没释放但管道已经关了），这不是错误，但必须留痕：
    // 否则用户遇到「双击没反应」时日志里一片空白，只能靠猜。
    ilog!("[iTools] 通知已有实例失败（{name}）：{last}");
    false
}

/// 通知失败后回头再抢一次互斥体；抢到就说明「那个实例其实已经走完了」，本进程可以正常启动。
///
/// # 为什么需要这条兜底
///
/// 用户点了托盘「退出」后**立刻**双击图标（更新安装后的重启也是这个时序）时，会落进一个窄窗口：
/// 第一个实例的管道已经关了（acceptor 线程随进程收尾），互斥体却还没释放（进程尚未走完）。
/// 于是新进程判定「已有实例」→ 通知失败 → 自己退出，而老进程随后也退干净了——
/// 结果是**一个 iTools 都没有**，用户双击了却什么都没发生。
///
/// 这里在通知失败后重试一次：此时老进程通常已经彻底退出、互斥体已释放，本进程顺理成章接班。
fn reacquire_after_failed_notify() -> bool {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::HSTRING;

    let name = HSTRING::from(mutex_name());
    let sd = medium_integrity_sa();
    let sa = sd.as_ref().map(|s| s.attrs());
    // SAFETY: 与 acquire() 中同一调用，参数生命周期同样由 sd 保住。
    let Ok(handle) = (unsafe { CreateMutexW(sa.map(|p| p as *const _), false, &name) }) else {
        return false;
    };
    // SAFETY: 只读末次错误码
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        // 对方确实还在（只是没应门铃），本进程照旧退出；这个多余句柄留给进程退出时回收
        return false;
    }
    let _ = MUTEX_HANDLE.set(handle.0 as usize);
    ilog!("[iTools] 通知失败后重试拿锁成功——上一个实例已退出，本进程接管为唯一实例。");
    true
}

/// 第一个实例在 `setup` 完成后调用：起后台线程监听「唤起」请求，收到就把主窗口显示出来。
///
/// 只在拿到互斥体的那个进程里调用。管道监听起不来只影响「第二次启动能不能唤起窗口」，
/// 不影响本进程自身，所以全部失败都只记日志、不上抛。
pub fn serve_activation(app: tauri::AppHandle) {
    use std::io::Read;

    let name = activate_pipe_name();
    for _ in 0..ACCEPTORS {
        let name = name.clone();
        let app = app.clone();
        std::thread::spawn(move || {
            let mut consecutive = 0usize;
            loop {
                match accept_one(&name) {
                    Ok(mut pipe) => {
                        // 内容无意义，读一次只是为了等对端写完；读失败（对端写完就关）也照样唤起
                        let mut buf = [0u8; 16];
                        let _ = pipe.read(&mut buf);
                        drop(pipe);
                        consecutive = 0; // 成功一次就把连续失败计数清零
                        show_main(&app);
                    }
                    Err(e) => {
                        // ⚠ 不能一出错就结束线程（2026-08-18 对抗审查抓到）：
                        // `ConnectNamedPipe` 会因 ERROR_NO_DATA(232) 失败——客户端在
                        // CreateNamedPipeW 返回后、ConnectNamedPipe 调用前就连上并关闭，
                        // 而我们的客户端恰恰是「写 5 字节立刻关闭并退出进程」，这个窗口不是零。
                        // 两次这样的偶发事件就能把两条 acceptor 全耗光，此后该实例**再也无法被唤起**，
                        // 双击图标永久没反应，且 release 下没有任何日志线索。
                        //
                        // 所以只对连续失败计数：偶发错误退避后继续，连续失败够多次
                        //（说明是名字被占、权限不足这类持续性故障）才罢手。
                        consecutive += 1;
                        if consecutive >= MAX_CONSECUTIVE_ERRORS {
                            ilog!(
                                "[iTools] 单实例唤起通道连续失败 {consecutive} 次，停止监听（{name}）：{e}"
                            );
                            return;
                        }
                        ilog!("[iTools] 单实例唤起通道出错（第 {consecutive} 次，将重试）：{e}");
                        std::thread::sleep(ERROR_BACKOFF);
                    }
                }
            }
        });
    }
    // 不说「就绪」：建管道是在各 acceptor 线程里做的，这一刻还不知道成没成功。
    // 真失败了上面那条 `监听结束` 日志会说明原因。
    ilog!("[iTools] 单实例唤起通道监听已启动（{ACCEPTORS} 条）：{name}");
}

/// 把主窗口显示出来。
///
/// 走 `run_on_main_thread`：本函数被 acceptor 后台线程调用，而窗口操作属于 UI 线程的活。
/// （Tauri 的窗口 API 自己会把消息投递到事件循环，直接调也不会崩；这里显式切主线程是为了
/// 语义清楚，且避免后台线程阻塞等待事件循环。）
///
/// ⚠ 回调里**只 show 已有窗口，绝不创建窗口**：在 `run_on_main_thread` 回调里
/// `WebviewWindowBuilder::build()` 会死锁（tauri#13963 / wry#583，见 `lib.rs::open_admin`）。
fn show_main(app: &tauri::AppHandle) {
    let inner = app.clone();
    let r = app.run_on_main_thread(move || {
        use tauri::Manager;
        match inner.get_webview_window("main") {
            Some(win) => crate::window::show(&win),
            // 主窗口由 tauri.conf.json 静态创建，正常不会缺席；缺了如实说，别静默吞掉
            None => ilog!("[iTools] 收到唤起请求，但主窗口不存在"),
        }
    });
    if let Err(e) = r {
        ilog!("[iTools] 收到唤起请求，但派发到主线程失败：{e}");
    }
}

/// 在 `name` 上创建一个管道实例并等一个客户端接进来，返回可读的句柄。
///
/// 与 `search/mft/ipc.rs::accept_one` 的关键区别：**这里不写 SDDL、不降完整性标签**。
/// 那边的管道由**提权（High 完整性）**的索引守护创建，而 Windows 的强制完整性控制是
/// `NO_WRITE_UP`——Medium 完整性的主进程写不进去，必须显式 `S:(ML;;NW;;;ME)` 把标签降下来。
/// 本管道两端都是**同一个用户、同一个完整性级别**的普通进程（都是双击启动的 iTools），
/// 不存在跨级别写入，用默认安全描述符（继承创建者令牌的默认 DACL：本用户 + SYSTEM）
/// 既够用也更不容易写错。
fn accept_one(name: &str) -> std::io::Result<std::fs::File> {
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND;
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows::core::HSTRING;

    let wide = HSTRING::from(name);
    // PIPE_ACCESS_INBOUND：服务端只读。信号是单向的，不给自己开一条用不上的回写通道。
    // SAFETY: wide 是 NUL 结尾宽串且在调用期间存活；安全属性传 None（理由见函数文档）；
    // 出方向缓冲给 0（本端从不写），入方向 256 字节足够装下几字节的信号。
    let handle = unsafe {
        CreateNamedPipeW(
            &wide,
            PIPE_ACCESS_INBOUND,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            255, // PIPE_UNLIMITED_INSTANCES 在部分绑定里是 255
            0,
            256,
            0,
            None,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: handle 有效；ConnectNamedPipe 同步等待一个客户端接入。
    // ERROR_PIPE_CONNECTED 表示客户端在本调用之前就已经接上了，属于成功。
    let connected = unsafe { ConnectNamedPipe(handle, None) };
    if let Err(e) = connected {
        const ERROR_PIPE_CONNECTED: i32 = 535;
        if e.code().0 & 0xFFFF != ERROR_PIPE_CONNECTED {
            // SAFETY: 失败路径必须收回句柄，否则每次失败泄漏一个内核对象
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(handle);
            }
            return Err(std::io::Error::other(e));
        }
    }
    // SAFETY: 句柄所有权转移给 File，由它负责 CloseHandle
    Ok(unsafe { std::fs::File::from_raw_handle(handle.0 as *mut _) })
}

/// 从当前进程令牌取用户 SID 的字符串形式。
///
/// 与 `search/mft/ipc.rs::current_user_sid` 是同一份实现（那边是私有函数）。
/// 这里刻意重复而不是把那个改成 `pub`：单实例判定跑在**启动最早期**，
/// 不该为了一个取 SID 的小工具去依赖搜索子系统（那条链路将来若被条件编译掉，
/// 单实例会跟着一起没）。两处都只用 Win32 API，不存在会漂移的业务逻辑。
fn current_user_sid() -> Option<String> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = HANDLE::default();
    // SAFETY: GetCurrentProcess 返回伪句柄无需释放；token 由下面的 CloseHandle 收回。
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.ok()?;
    let mut size = 0u32;
    // 第一次调用只为问出所需长度，返回 ERROR_INSUFFICIENT_BUFFER 是预期行为
    // SAFETY: 传 None 缓冲 + 接收长度，是该 API 的标准两段式用法。
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut size) };
    if size == 0 {
        // SAFETY: token 来自上面成功的 OpenProcessToken
        unsafe {
            let _ = CloseHandle(token);
        }
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
    unsafe {
        let _ = CloseHandle(token);
    }
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
    unsafe {
        let _ = LocalFree(Some(HLOCAL(pwstr.as_ptr() as *mut _)));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 锁名必须按构建类型分开——这条一旦破了，编译一次 debug 就会顶掉用户正在用的 release。
    ///
    /// 注意：测试跑在 debug 下，所以只能断言「当前是 dev 那一套」+「两套名字不相等」。
    /// 后者靠的是 [`mutex_name`] / [`build_tag`] 里字面量本身不同，改名时这条会跟着提醒你。
    #[test]
    fn lock_names_separate_dev_and_release() {
        assert_eq!(build_tag(), "dev", "cargo test 恒为 debug 构建");
        assert_eq!(mutex_name(), r"Local\iTools-SingleInstance-dev");
        assert!(
            mutex_name().starts_with(r"Local\"),
            "必须是 Local\\ 而非 Global\\：跨会话会误判（见 mutex_name 注释）"
        );
        assert!(
            !mutex_name().contains("release"),
            "dev 与 release 的互斥体名不能撞车"
        );
    }

    /// 唤起管道名必须同时带构建类型与真实 SID
    #[test]
    fn activate_pipe_name_is_per_build_and_user() {
        let name = activate_pipe_name();
        println!("唤起管道名：{name}");
        assert!(name.starts_with(r"\\.\pipe\itools-activate-dev-"));
        assert!(
            name.contains("S-1-"),
            "应含真实用户 SID，实得 {name}（取 SID 失败会退化成 unknown）"
        );
    }

    /// 没有实例在监听时，通知必须**快速**失败并返回，不能把第二个进程卡住不退。
    ///
    /// 上限**按常量算**而不是写死数字：`RETRIES × RETRY_GAP` 再留一倍余量。
    /// 这样调整重试预算时这条用例会自动跟随，不会像 2026-08-18 那次一样失配。
    /// ⚠ 用独立管道名，不能用生产名：本机开着 debug 版 iTools 时，
    /// 往生产管道发信号会真的把用户的搜索框弹出来（跑个测试而已，不该有这种副作用）。
    #[test]
    fn notify_fails_fast_without_peer() {
        let name = format!(
            r"\\.\pipe\itools-activate-test-nopeer-{}",
            std::process::id()
        );
        let start = std::time::Instant::now();
        notify_on(&name);
        let elapsed = start.elapsed();
        println!("无对端时通知耗时 {elapsed:?}");
        let budget = RETRY_GAP * RETRIES as u32;
        assert!(
            elapsed < budget * 2,
            "必须在重试预算（{budget:?}）的两倍内返回，实测 {elapsed:?}"
        );
        // 同时钉住「确实重试过」——若哪天 RETRIES 被误设成 0/1，这条会立刻发现：
        // 那样第二个实例在冷启动时根本等不到第一个的管道就绪。
        assert!(
            elapsed > RETRY_GAP,
            "应当经历多次重试，实测只用了 {elapsed:?}"
        );
    }

    /// 端到端：起一条 acceptor 等信号，客户端连上去发一次，服务端必须收到。
    /// 这条同时验证了默认安全描述符可用（写错的话 CreateNamedPipeW / 连接会直接失败）。
    ///
    /// ⚠ 同样用独立管道名（理由见上一条）。这里不经 [`serve_activation`]——它要一个
    /// 真的 `AppHandle`，单测里造不出来；被测的是管道那一半（accept_one + notify_on）。
    #[test]
    fn signal_round_trip() {
        use std::io::Read;
        use std::sync::mpsc;

        let name = format!(r"\\.\pipe\itools-activate-test-e2e-{}", std::process::id());
        let server_name = name.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            if let Ok(mut pipe) = accept_one(&server_name) {
                let mut buf = Vec::new();
                let _ = pipe.read_to_end(&mut buf);
                let _ = tx.send(buf);
            }
        });
        // 给服务端一点时间把管道创建出来；notify_on 自带重试，撞上也会自己等
        std::thread::sleep(std::time::Duration::from_millis(200));
        notify_on(&name);
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("5s 内服务端没收到唤起信号");
        assert_eq!(got, ACTIVATE_SIGNAL, "服务端应原样收到门铃信号");
    }
}

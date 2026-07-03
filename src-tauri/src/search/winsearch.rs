//! 通过 OLE DB 直连 Windows Search Index（SystemIndex）实现全盘按文件名秒搜。
//!
//! 路径：IDataInitialize → IDBInitialize → IDBCreateSession → IDBCreateCommand
//! → ICommandText → IRowset → IAccessor（DBTYPE_WSTR 列绑定读宽串）。
//!
//! COM 对象不是 `Send`，且 Tauri 命令线程未初始化 COM/可能是 STA，故查询全部
//! 放到一条常驻 MTA 工作线程上（`WinSearchWorker`，mpsc 请求-应答）。

use core::ffi::c_void;
use core::ptr::null_mut;

use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Search::{
    IAccessor, ICommandText, IDBCreateCommand, IDBCreateSession, IDBInitialize, IDataInitialize,
    IRowset, DBACCESSOR_ROWDATA, DBBINDING, DBMEMOWNER_CLIENTOWNED, DBPARAMIO_NOTPARAM,
    DBPART_LENGTH, DBPART_STATUS, DBPART_VALUE, DBTYPE_WSTR, HACCESSOR, MSDAINITIALIZE,
};

// DBGUID_DEFAULT {C8B521FB-5CF3-11CE-ADE5-00AA0044773D}（crate 未导出常量，手写）
const DBGUID_DEFAULT: GUID = GUID::from_u128(0xc8b521fb_5cf3_11ce_ade5_00aa0044773d);

const NAME_CAP: usize = 260; // wchar
const PATH_CAP: usize = 1024; // wchar（ItemUrl 可能较长）
const TYPE_CAP: usize = 64; // wchar（".txt" / "Directory"）

#[repr(C)]
struct RowBuf {
    name_status: u32,
    name_length: usize,
    name_value: [u16; NAME_CAP],
    url_status: u32,
    url_length: usize,
    url_value: [u16; PATH_CAP],
    type_status: u32,
    type_length: usize,
    type_value: [u16; TYPE_CAP],
}

fn to_wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// CONTAINS 词前缀查询的安全清洗：去掉引号/星号/AQS 运算符，防注入、防短语被破坏
fn sanitize_term(q: &str) -> String {
    q.chars()
        .filter(|c| !matches!(c, '"' | '\'' | '*' | '(' | ')' | '{' | '}' | '\\'))
        .collect::<String>()
        .trim()
        .to_string()
}

fn build_sql(term: &str, limit: usize) -> String {
    // 主路径：全文倒排「词前缀」，实测亚百毫秒，逐键可用。
    // CONTAINS 只支持词前缀、不支持真子串（前导 '*' 无效）。
    format!(
        "SELECT TOP {limit} System.ItemNameDisplay, System.ItemUrl, System.ItemType \
         FROM SystemIndex \
         WHERE CONTAINS(System.FileName, '\"{term}*\"') \
         ORDER BY System.DateModified DESC"
    )
}

fn read_col(status: u32, length: usize, value: &[u16]) -> String {
    // DBSTATUS_S_OK=0, DBSTATUS_S_TRUNCATED=4 都视为有值
    if status != 0 && status != 4 {
        return String::new();
    }
    let n = (length / 2).min(value.len());
    let s = &value[..n];
    let end = s.iter().position(|&c| c == 0).unwrap_or(n);
    String::from_utf16_lossy(&s[..end])
}

/// System.ItemUrl: "file:C:/Users/.../a.txt"（真实名、未编码）→ 可打开的 "C:\\Users\\...\\a.txt"
fn url_to_path(url: &str) -> Option<String> {
    let p = url.strip_prefix("file:")?;
    if p.len() < 3 || p.as_bytes()[1] != b':' {
        return None; // 只接受本地盘符
    }
    Some(p.replace('/', "\\"))
}

fn wstr_binding(ord: usize, ob_s: usize, ob_l: usize, ob_v: usize, cap: usize) -> DBBINDING {
    // 其余字段（pTypeInfo=ManuallyDrop(None)、pObject/pBindExt=null）取 Default
    DBBINDING {
        iOrdinal: ord,
        obStatus: ob_s,
        obLength: ob_l,
        obValue: ob_v,
        dwPart: (DBPART_VALUE.0 | DBPART_STATUS.0 | DBPART_LENGTH.0) as u32,
        dwMemOwner: DBMEMOWNER_CLIENTOWNED.0 as u32, // 0：串复制进本地缓冲，无需释放
        eParamIO: DBPARAMIO_NOTPARAM.0 as u32,
        cbMaxLen: cap * 2,
        wType: DBTYPE_WSTR.0 as u16,
        ..Default::default()
    }
}

/// 同步查询 Windows Search Index，返回 (文件名, 真实可打开路径, is_dir)。
///
/// # Safety
/// 必须在「已 `CoInitializeEx` 的线程」上调用（见 [`WinSearchWorker`]）。内部创建的
/// OLE DB COM 对象生命周期均在本次调用内闭合，行句柄数组用后经 `CoTaskMemFree` 释放。
pub unsafe fn query_search_index(
    query: &str,
    limit: usize,
) -> windows::core::Result<Vec<(String, String, bool)>> {
    let term = sanitize_term(query);
    if term.is_empty() {
        return Ok(Vec::new());
    }

    let conn = to_wide_nul("Provider=Search.CollatorDSO;Extended Properties='Application=Windows'");
    let sql = to_wide_nul(&build_sql(&term, limit));

    // 打开数据源：IDataInitialize::GetDataSource 直接解析整条连接串
    let data_init: IDataInitialize =
        CoCreateInstance(&MSDAINITIALIZE, None, CLSCTX_INPROC_SERVER)?;
    let mut ds: Option<windows::core::IUnknown> = None;
    data_init.GetDataSource(
        None,
        CLSCTX_INPROC_SERVER.0,
        PCWSTR(conn.as_ptr()),
        &IDBInitialize::IID,
        &mut ds,
    )?;
    let Some(ds) = ds else {
        return Ok(Vec::new());
    };
    let dbinit: IDBInitialize = ds.cast()?;
    dbinit.Initialize()?;

    // session -> command
    let sess: IDBCreateSession = dbinit.cast()?;
    let cmd: IDBCreateCommand = sess.CreateSession(None, &IDBCreateCommand::IID)?.cast()?;
    let text: ICommandText = cmd.CreateCommand(None, &ICommandText::IID)?.cast()?;
    text.SetCommandText(&DBGUID_DEFAULT, PCWSTR(sql.as_ptr()))?;

    // 执行 -> IRowset -> IAccessor
    let mut rs_unk: Option<windows::core::IUnknown> = None;
    text.Execute(None, &IRowset::IID, None, None, Some(&mut rs_unk))?;
    let Some(rs_unk) = rs_unk else {
        return Ok(Vec::new());
    };
    let rowset: IRowset = rs_unk.cast()?;
    let accessor: IAccessor = rowset.cast()?;

    let bindings = [
        wstr_binding(
            1,
            core::mem::offset_of!(RowBuf, name_status),
            core::mem::offset_of!(RowBuf, name_length),
            core::mem::offset_of!(RowBuf, name_value),
            NAME_CAP,
        ),
        wstr_binding(
            2,
            core::mem::offset_of!(RowBuf, url_status),
            core::mem::offset_of!(RowBuf, url_length),
            core::mem::offset_of!(RowBuf, url_value),
            PATH_CAP,
        ),
        wstr_binding(
            3,
            core::mem::offset_of!(RowBuf, type_status),
            core::mem::offset_of!(RowBuf, type_length),
            core::mem::offset_of!(RowBuf, type_value),
            TYPE_CAP,
        ),
    ];
    let mut hacc = HACCESSOR::default();
    accessor.CreateAccessor(
        DBACCESSOR_ROWDATA.0 as u32,
        bindings.len(),
        bindings.as_ptr(),
        core::mem::size_of::<RowBuf>(),
        &mut hacc,
        None,
    )?;

    let mut out = Vec::with_capacity(limit);
    let batch = limit.max(1);
    'outer: loop {
        // 切片长度 = 请求行数；provider 分配 HROW 数组，指针写入 row_handles[0]
        let mut row_handles: Vec<*mut usize> = vec![null_mut(); batch];
        let mut obtained: usize = 0;
        rowset.GetNextRows(0, 0, &mut obtained, &mut row_handles)?;
        if obtained == 0 {
            break;
        }
        let hrow_array = row_handles[0] as *const usize;

        for i in 0..obtained {
            let hrow = *hrow_array.add(i);
            let mut buf: RowBuf = core::mem::zeroed();
            if rowset
                .GetData(hrow, hacc, &mut buf as *mut _ as *mut c_void)
                .is_ok()
            {
                let itype = read_col(buf.type_status, buf.type_length, &buf.type_value);
                let is_dir = itype.eq_ignore_ascii_case("Directory");
                let name = read_col(buf.name_status, buf.name_length, &buf.name_value);
                let url = read_col(buf.url_status, buf.url_length, &buf.url_value);
                if let Some(path) = url_to_path(&url) {
                    out.push((name, path, is_dir));
                }
            }
            if out.len() >= limit {
                let _ = rowset.ReleaseRows(obtained, hrow_array, null_mut(), null_mut(), null_mut());
                CoTaskMemFree(Some(row_handles[0] as *const c_void));
                break 'outer;
            }
        }
        let _ = rowset.ReleaseRows(obtained, hrow_array, null_mut(), null_mut(), null_mut());
        CoTaskMemFree(Some(row_handles[0] as *const c_void)); // 释放 provider 分配的 HROW 数组
    }

    let _ = accessor.ReleaseAccessor(hacc, None);
    let _ = dbinit.Uninitialize();
    Ok(out)
}

use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;

struct Req {
    query: String,
    limit: usize,
    reply: Sender<Vec<(String, String, bool)>>,
}

/// 常驻 MTA 工作线程：启动时 `CoInitializeEx(MTA)` 一次并探测 Search 服务是否可用；
/// 之后逐条处理查询请求。COM 对象不跨线程，全部生命周期留在该线程。
pub struct WinSearchWorker {
    tx: Mutex<Sender<Req>>,
    /// Search 服务是否可用；false 时调用方应降级到 walkdir
    pub available: bool,
}

impl WinSearchWorker {
    pub fn new() -> Self {
        let (tx, rx) = channel::<Req>();
        let (ptx, prx) = channel::<bool>();
        std::thread::spawn(move || {
            // SAFETY: 本线程独占该 COM apartment，初始化后所有查询都在此线程执行
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let ok = query_search_index("a", 1).is_ok();
                let _ = ptx.send(ok);
                while let Ok(req) = rx.recv() {
                    let res = query_search_index(&req.query, req.limit).unwrap_or_default();
                    let _ = req.reply.send(res);
                }
                CoUninitialize();
            }
        });
        let available = prx.recv().unwrap_or(false);
        Self {
            tx: Mutex::new(tx),
            available,
        }
    }

    /// 同步查询（阻塞等 worker 回包）。worker 不可用/已死则返回空，调用方降级。
    pub fn query(&self, query: &str, limit: usize) -> Vec<(String, String, bool)> {
        let (reply, back) = channel();
        let req = Req {
            query: query.to_string(),
            limit,
            reply,
        };
        if self
            .tx
            .lock()
            .ok()
            .and_then(|g| g.send(req).ok())
            .is_none()
        {
            return Vec::new();
        }
        back.recv().unwrap_or_default()
    }
}

impl Default for WinSearchWorker {
    fn default() -> Self {
        Self::new()
    }
}

//! NTFS 卷的原始访问：MFT 全量枚举（`FSCTL_ENUM_USN_DATA`）与 USN 变更日志增量读取
//! （`FSCTL_READ_USN_JOURNAL`）。这是「全盘秒搜」的数据来源。
//!
//! # 为什么必须提权（2026-08-17 实测取证）
//!
//! 打开 `\\.\C:` 这类卷设备句柄需要 `GENERIC_READ`，非提权进程**四个盘全部**返回
//! `ERROR_ACCESS_DENIED(5)`；只申请 `FILE_READ_ATTRIBUTES(0x80)` 能开到句柄，但那种
//! 句柄发 `FSCTL_ENUM_USN_DATA` 会失败——读 MFT 没有「非提权变通路径」。
//! 故本模块只在**提权的索引守护进程**（见 `daemon.rs`）里调用；主进程走 `ipc.rs` 取结果。
//!
//! # 为什么不用 Windows Search
//!
//! 同日实测本机 `SystemIndex` 的索引范围（注册表 `WorkingSetRules`）只有 `C:\Users\`
//! 与开始菜单，按盘符查 `D:`/`E:`/`F:` 一律 0 条——不是查询写错，是索引里根本没有数据。
//! 且 `CONTAINS` 只支持**词前缀**，搜「报告」搜不到「季度报告.docx」。MFT 两个问题一并解决。

use core::ffi::c_void;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_HANDLE_EOF, ERROR_JOURNAL_NOT_ACTIVE, GENERIC_READ, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{
    MFT_ENUM_DATA_V0, READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0, USN_RECORD_V2,
    FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::HSTRING;

/// 每批 IOCTL 的输出缓冲。1 MB 实测吞吐足够（全盘 1350 万条 42 秒），再大收益递减。
const BATCH_BYTES: usize = 1 << 20;

/// MFT 里一条条目的原始信息。路径不在这里拼——只存父引用，由索引层按需回溯。
#[derive(Clone, Copy)]
pub struct RawEntry {
    /// 文件引用号（低 48 位是 MFT 记录号，高 16 位是序列号）
    pub frn: u64,
    /// 父目录的文件引用号
    pub parent: u64,
    pub is_dir: bool,
    /// 重解析点（符号链接 / 联接点）。
    ///
    /// **目前没有任何消费者**（故加 allow）：路径回溯的成环保护实际由 `index.rs` 的
    /// `MAX_DEPTH` 兜住，MFT 的父引用是真实包含关系、不会因联接点成环。这一位仍如实
    /// 从 `FILE_ATTRIBUTE_REPARSE_POINT` 解析出来，留给将来「结果里标注符号链接」用；
    /// 不要因为它存在就以为已经有基于它的过滤逻辑。
    #[allow(dead_code)]
    pub is_reparse: bool,
}

/// USN 日志里的一条变更。`name` 为 `None` 表示这条记录不该进索引。
pub struct Change {
    pub entry: RawEntry,
    pub reason: u32,
}

/// 一个已打开的 NTFS 卷设备句柄。`Drop` 时关闭。
pub struct Volume {
    handle: HANDLE,
    letter: char,
}

impl Volume {
    /// 打开 `\\.\<letter>:`。失败多半是没提权（`ERROR_ACCESS_DENIED`）或该盘不是本地卷。
    pub fn open(letter: char) -> windows::core::Result<Self> {
        let path = HSTRING::from(format!(r"\\.\{letter}:"));
        // SAFETY: path 是以 NUL 结尾的宽串且在调用期间存活；其余参数为常量。
        // 失败返回 Err（windows crate 已把 INVALID_HANDLE_VALUE 映射成 Err）。
        let handle = unsafe {
            CreateFileW(
                &path,
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )?
        };
        Ok(Self { handle, letter })
    }

    pub fn letter(&self) -> char {
        self.letter
    }

    /// 查询 USN 日志的当前状态（拿 `UsnJournalID` 与 `NextUsn` 作为增量起点）。
    ///
    /// 卷上日志未激活时返回 `Ok(None)`——该盘只能做全量、拿不到增量，调用方据此降级。
    pub fn query_journal(&self) -> windows::core::Result<Option<USN_JOURNAL_DATA_V0>> {
        let mut data = USN_JOURNAL_DATA_V0::default();
        let mut returned = 0u32;
        // SAFETY: 输出缓冲是栈上的 POD 结构，尺寸如实传入；无输入缓冲。
        let res = unsafe {
            DeviceIoControl(
                self.handle,
                FSCTL_QUERY_USN_JOURNAL,
                None,
                0,
                Some(&mut data as *mut _ as *mut c_void),
                core::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
                Some(&mut returned),
                None,
            )
        };
        match res {
            Ok(()) => Ok(Some(data)),
            Err(e) if e.code() == ERROR_JOURNAL_NOT_ACTIVE.to_hresult() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// 全量枚举整个 MFT，每解析出一条就交给 `sink`（名字以 UTF-16 码元切片给出，
    /// 避免中间再分配一次 `String`——1350 万条上这笔开销很实在）。
    ///
    /// 正常收尾是 `ERROR_HANDLE_EOF`，不是错误。
    pub fn enumerate<F>(&self, mut sink: F) -> windows::core::Result<()>
    where
        F: FnMut(RawEntry, &[u16]),
    {
        let mut buf = vec![0u8; BATCH_BYTES];
        let mut input = MFT_ENUM_DATA_V0 {
            StartFileReferenceNumber: 0,
            LowUsn: 0,
            // 必须 >= 当前 NextUsn，否则一条都不返回。i64::MAX 恒成立。
            HighUsn: i64::MAX,
        };
        loop {
            let mut returned = 0u32;
            // SAFETY: 输入是栈上 POD；输出缓冲长度如实传入，返回字节数由 returned 回填。
            let res = unsafe {
                DeviceIoControl(
                    self.handle,
                    FSCTL_ENUM_USN_DATA,
                    Some(&input as *const _ as *const c_void),
                    core::mem::size_of::<MFT_ENUM_DATA_V0>() as u32,
                    Some(buf.as_mut_ptr() as *mut c_void),
                    buf.len() as u32,
                    Some(&mut returned),
                    None,
                )
            };
            match res {
                Ok(()) => {}
                // 枚举完毕的正常出口
                Err(e) if e.code() == ERROR_HANDLE_EOF.to_hresult() => return Ok(()),
                Err(e) => return Err(e),
            }
            // 头 8 字节是下一批的起始 FRN，不足则说明没有记录了
            if (returned as usize) <= 8 {
                return Ok(());
            }
            input.StartFileReferenceNumber =
                u64::from_le_bytes(buf[..8].try_into().expect("已判定长度 > 8"));
            parse_records(&buf[8..returned as usize], &mut |e, name, _| sink(e, name));
        }
    }

    /// 读取自 `start_usn` 起的变更。返回下一次的起始 USN。
    ///
    /// `Timeout`/`BytesToWaitFor` 都传 0 → **非阻塞**：日志里没有新记录就立刻返回空。
    /// 守护线程靠外层定时轮询，不在这里睡（阻塞式等待会让停机时无法及时退出）。
    pub fn read_changes<F>(
        &self,
        journal_id: u64,
        start_usn: i64,
        mut sink: F,
    ) -> windows::core::Result<i64>
    where
        F: FnMut(Change, &[u16]),
    {
        let input = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: start_usn,
            ReasonMask: u32::MAX, // 全部原因；筛选交给索引层
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: journal_id,
        };
        let mut buf = vec![0u8; BATCH_BYTES];
        let mut returned = 0u32;
        // SAFETY: 同 enumerate——输入为栈上 POD，输出缓冲长度如实传入。
        unsafe {
            DeviceIoControl(
                self.handle,
                FSCTL_READ_USN_JOURNAL,
                Some(&input as *const _ as *const c_void),
                core::mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
                Some(buf.as_mut_ptr() as *mut c_void),
                buf.len() as u32,
                Some(&mut returned),
                None,
            )?
        };
        if (returned as usize) < 8 {
            return Ok(start_usn);
        }
        let next = i64::from_le_bytes(buf[..8].try_into().expect("已判定长度 >= 8"));
        parse_records(&buf[8..returned as usize], &mut |entry, name, reason| {
            sink(Change { entry, reason }, name)
        });
        Ok(next)
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        // SAFETY: handle 由本类型独占，构造成功才会走到这里，且只关一次。
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// 解析一批紧密排列的 `USN_RECORD_V2`。
///
/// 只认 `MajorVersion == 2`：`MFT_ENUM_DATA_V0` 与 `READ_USN_JOURNAL_DATA_V0` 都只产 V2，
/// 见到别的版本（V3 的 FRN 是 128 位，字段偏移完全不同）宁可整批停下，也不能按错误的
/// 偏移读——那会拼出乱码文件名和错误的父引用。
fn parse_records(mut data: &[u8], sink: &mut impl FnMut(RawEntry, &[u16], u32)) {
    const HEADER: usize = core::mem::size_of::<USN_RECORD_V2>();
    while data.len() >= 4 {
        let len = u32::from_le_bytes(data[..4].try_into().expect("已判定长度 >= 4")) as usize;
        // len 必须能容下定长头，且不越出本批剩余字节；否则数据不可信，停止解析
        if len < HEADER || len > data.len() {
            return;
        }
        let rec = data[..len].as_ptr() as *const USN_RECORD_V2;
        // SAFETY: 上面已校验 len >= size_of::<USN_RECORD_V2>()，故读定长字段不越界。
        // 指针来自 &[u8]，对齐要求由 repr(C) 结构的 8 字节对齐决定——DeviceIoControl 的
        // 输出缓冲是 Vec<u8> 起始（16 对齐），且每条 RecordLength 都是 8 的倍数，故成立。
        let r = unsafe { &*rec };
        if r.MajorVersion != 2 {
            return;
        }
        let name_off = r.FileNameOffset as usize;
        let name_len = r.FileNameLength as usize;
        // 文件名必须整段落在本条记录内，且长度是偶数（UTF-16 码元）
        if name_off >= len || name_len % 2 != 0 || name_off + name_len > len {
            data = &data[len..];
            continue;
        }
        // SAFETY: 起点与长度都已校验在 data[..len] 内；u16 的对齐由 name_off 为偶数保证
        // （FileNameOffset 恒为 60，偶数）。
        let name = unsafe {
            core::slice::from_raw_parts(data.as_ptr().add(name_off) as *const u16, name_len / 2)
        };
        let attrs = r.FileAttributes;
        sink(
            RawEntry {
                frn: r.FileReferenceNumber,
                parent: r.ParentFileReferenceNumber,
                is_dir: attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
                is_reparse: attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0,
            },
            name,
            r.Reason,
        );
        data = &data[len..];
    }
}

/// 枚举所有本地固定 NTFS 盘符。网络盘 / 光驱 / U 盘不进索引（可拔出、且没有 MFT 访问意义）。
pub fn fixed_ntfs_drives() -> Vec<char> {
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumeInformationW};
    // DRIVE_FIXED 在 windows-0.61 里不在 Storage::FileSystem，而在 System::WindowsProgramming，
    // 且是**裸 u32 常量**（值 3）不是 newtype——所以下面直接与 GetDriveTypeW 的 u32 返回值比，
    // 不能写 DRIVE_FIXED.0。
    use windows::Win32::System::WindowsProgramming::DRIVE_FIXED;
    let mut out = Vec::new();
    for letter in 'A'..='Z' {
        let root = HSTRING::from(format!(r"{letter}:\"));
        // SAFETY: root 是 NUL 结尾宽串，调用期间存活。
        if unsafe { GetDriveTypeW(&root) } != DRIVE_FIXED {
            continue;
        }
        let mut fs_name = [0u16; 16];
        // SAFETY: 各输出缓冲长度如实传入；不需要卷名/序列号，故传 None。
        let ok = unsafe {
            GetVolumeInformationW(
                &root,
                None,
                None,
                None,
                None,
                Some(&mut fs_name),
            )
        };
        if ok.is_err() {
            continue;
        }
        let end = fs_name.iter().position(|&c| c == 0).unwrap_or(fs_name.len());
        if String::from_utf16_lossy(&fs_name[..end]).eq_ignore_ascii_case("NTFS") {
            out.push(letter);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 盘符枚举必须至少认出系统盘，且结果里没有重复。
    #[test]
    fn drives_include_system_volume() {
        let drives = fixed_ntfs_drives();
        println!("固定 NTFS 盘：{drives:?}");
        assert!(!drives.is_empty(), "至少应认出系统盘");
        let mut sorted = drives.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), drives.len(), "不应有重复盘符");
    }

    /// 非提权下打开卷必须是 ACCESS_DENIED 而不是 panic；提权下必须能枚举出记录。
    /// 两种环境都要能跑过——CI 与开发机权限不同。
    #[test]
    fn open_and_enumerate_or_access_denied() {
        let Some(&letter) = fixed_ntfs_drives().first() else {
            println!("无固定 NTFS 盘，跳过");
            return;
        };
        match Volume::open(letter) {
            Err(e) => {
                println!("打开 {letter}: 失败（预期于非提权环境）：{e}");
                assert_eq!(
                    e.code(),
                    windows::Win32::Foundation::ERROR_ACCESS_DENIED.to_hresult(),
                    "非提权下应恰好是 ACCESS_DENIED，其它错误说明代码有问题"
                );
            }
            Ok(vol) => {
                let mut count = 0usize;
                let mut dirs = 0usize;
                let mut sample = Vec::new();
                vol.enumerate(|e, name| {
                    count += 1;
                    if e.is_dir {
                        dirs += 1;
                    }
                    if sample.len() < 3 && !e.is_dir {
                        sample.push(String::from_utf16_lossy(name));
                    }
                })
                .expect("枚举 MFT 失败");
                println!("{letter}: 枚举 {count} 条（目录 {dirs}），样例 {sample:?}");
                assert!(count > 1000, "系统盘至少应有上千条记录，实得 {count}");
                assert!(dirs > 0, "应枚举到目录");
            }
        }
    }
}

//! 紧凑内存索引：MFT 条目 → 可子串搜索的结构。
//!
//! # 内存预算（2026-08-17 于本机 C/D/E/F 四盘**实测**，不是估算）
//!
//! 全盘 MFT 共 **1350 万**条记录、文件名合计 688 MB（UTF-16）。照原样进内存要 700-800 MB，
//! 对一个启动器不可接受。两处压缩把它降到实测 **358 MB / 733 万条可搜索条目**
//! （守护进程真实占用 371 MB WorkingSet）：
//!
//! 1. **排除噪音子树**（`node_modules`/`target`/`.git`/`WinSxS`/各类 cache，见
//!    [`DEFAULT_EXCLUDES`]）：实测排掉 **618 万**条。这同时提升结果质量——没人想在
//!    搜索结果里看到几百万个 WinSxS 硬链接副本。
//! 2. **名字进紧凑池**：文件名转 UTF-8 连续存放，条目只留 `(u32 偏移, u16 长度)`，
//!    不存 `String`（每个 `String` 光头部就 24 B，再加堆碎片，733 万条要多花 200 MB+）。
//!
//! 路径**不存**——只留父目录引用，命中后才回溯拼接。全路径平均 60+ 字节，
//! 存下来比文件名本身还贵，而每次搜索只需要回溯前几十条。
//!
//! ⚠ 排除清单是**故意偏保守**的：没有排 `bin`、`Library`、`Documents` 这类既常见于
//!    构建产物、也常见于用户真实文件的目录名。代价就是上面那个 358 MB——宁可多占内存，
//!    也不能让用户「明明有这个文件却搜不到」。要再往下压内存，得先想清楚会漏掉什么。

use std::collections::HashSet;

use super::volume::RawEntry;

/// 路径回溯的深度上限。防重解析点 / 损坏 MFT 造出的父链成环。
/// Windows 路径实际最深约 30 层，64 足够宽松。
const MAX_DEPTH: usize = 64;

/// NTFS 根目录固定占 MFT 记录号 5。
const ROOT_RECORD: u64 = 5;

/// 单个卷内并行扫描的最大段数（见 [`VolumeIndex::shard_count`]）。
const MAX_SCAN_SHARDS: usize = 8;

/// 少于这么多条目就别分段了——`thread::scope` 每条线程有几十微秒的固定开销，
/// 小索引上开线程是纯亏。
const MIN_PARALLEL_ENTRIES: usize = 50_000;

/// FRN 的低 48 位是 MFT 记录号，高 16 位是序列号。判定「是不是同一个文件」用记录号即可
/// ——序列号只在记录被复用时递增，而我们的父引用查找恰恰要跨复用保持可查。
#[inline]
fn record_of(frn: u64) -> u64 {
    frn & 0x0000_FFFF_FFFF_FFFF
}

/// 默认排除的目录名（大小写无关）。全是「机器生成、用户不会去搜」的目录。
///
/// 判定是**整棵子树**：一旦某目录名命中，其下所有层级全部排除。
pub const DEFAULT_EXCLUDES: &[&str] = &[
    // 包管理器 / 构建产物
    "node_modules",
    "target",
    "build",
    "obj",
    "dist",
    "out",
    ".next",
    ".nuxt",
    ".turbo",
    ".pnpm-store",
    "bower_components",
    "vendor",
    "packages",
    "site-packages",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    // 版本控制内部对象
    ".git",
    ".svn",
    ".hg",
    // 工具链本地仓库
    ".cargo",
    ".rustup",
    ".gradle",
    ".m2",
    ".nuget",
    ".pub-cache",
    ".stack",
    ".cocoapods",
    // 各类缓存
    "cache",
    "caches",
    "cachestorage",
    "code cache",
    "gpucache",
    "serviceworker",
    "crashpad",
    "temp",
    "tmp",
    "logs",
    // Windows 系统组件仓库（WinSxS 里绝大多数是硬链接副本）
    "winsxs",
    "driverstore",
    "servicing",
    "softwaredistribution",
    "assembly",
    "installer",
    "$recycle.bin",
    "system volume information",
];

/// 目录所处的「区域」，决定它下面的条目在搜索结果里的基础权重。
///
/// 判定靠自顶向下传播（见 [`IndexBuilder::finish_dirs`]），不靠字符串匹配完整路径
/// ——后者要为 170 万个目录各拼一次路径，纯浪费。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Zone {
    /// 卷根下的普通目录，以及非系统盘的一切
    Normal = 0,
    /// `C:\Users` 本身
    UsersRoot = 1,
    /// `C:\Users\<某人>` —— 用户 profile 根
    UserProfile = 2,
    /// 用户的桌面 / 文档 / 下载 / 图片 / 视频 / 音乐 / OneDrive 及其子树
    UserCommon = 3,
    /// Windows / Program Files / ProgramData / AppData —— 系统与程序自有的东西
    System = 4,
}

impl Zone {
    fn from_bits(bits: u8) -> Self {
        match bits {
            1 => Zone::UsersRoot,
            2 => Zone::UserProfile,
            3 => Zone::UserCommon,
            4 => Zone::System,
            _ => Zone::Normal,
        }
    }

    /// 区域基准权重。差距刻意拉得比「名字长度」大、比「匹配质量」小：
    /// 位置只该在匹配质量相当时起作用，不该让一个位置好的模糊匹配盖过精确匹配。
    fn base_bias(self) -> i16 {
        match self {
            Zone::UserCommon => 80,
            Zone::UserProfile => 40,
            Zone::Normal => 28,
            Zone::UsersRoot => 20,
            // AppData / Windows / Program Files：不是负数——那里确实有用户要找的东西
            //（装好的程序、游戏存档），只是默认让位于用户自己的文件
            Zone::System => 0,
        }
    }
}

/// 由父目录的区域 + 本目录名，推出本目录的区域。
///
/// `depth` 是本目录的层数（卷根的直接子目录 = 1）。只在特定层级上认名字，
/// 免得把 `D:\备份\Users\Windows` 这种普通目录误判成系统区。
fn child_zone(parent: Zone, depth: u8, name_lower: &str) -> Zone {
    match parent {
        // 卷根的直接子目录才认这些名字
        Zone::Normal if depth == 1 => match name_lower {
            "users" => Zone::UsersRoot,
            "windows" | "winnt" | "program files" | "program files (x86)" | "programdata"
            | "perflogs" | "recovery" | "system volume information" | "$recycle.bin" => {
                Zone::System
            }
            _ => Zone::Normal,
        },
        // C:\Users 的子目录就是各个用户的 profile 根
        Zone::UsersRoot => Zone::UserProfile,
        Zone::UserProfile => match name_lower {
            // 用户真正放东西的地方（中文名对应「此电脑」里的本地化目录名）
            "desktop" | "documents" | "downloads" | "pictures" | "videos" | "music"
            | "onedrive" | "桌面" | "文档" | "下载" | "图片" | "视频" | "音乐" => Zone::UserCommon,
            // AppData 虽在 profile 内，但里面全是程序自己的东西，不该和用户文件抢排序
            "appdata" | "application data" | "local settings" => Zone::System,
            _ => Zone::UserProfile,
        },
        // 其余一律继承父区域（UserCommon / System 的整棵子树都保持原样）
        other => other,
    }
}

/// 目录的最终位置权重 = 区域基准 − 深度惩罚。
///
/// 深度惩罚的用意：`D:\项目\方案.docx` 该排在
/// `D:\项目\a\b\c\d\e\f\旧版\方案.docx` 前面——埋得越深越可能是历史堆积。
/// 惩罚封顶，免得深层目录被压成负分而与 `System` 区不可区分。
fn dir_bias(zone: Zone, depth: u8) -> i8 {
    let penalty = (depth as i16 * 2).min(28);
    (zone.base_bias() - penalty).clamp(-128, 127) as i8
}

/// 一个目录节点。目录**全部保留**（含被排除的），因为它们是路径回溯的骨架：
/// 排除只影响「这个目录及其内容是否作为搜索结果出现」，不影响它作为别人的父节点被查到。
///
/// 布局刻意凑满 24 字节：`8+8+4+2` 之后本就有 2 字节 padding，
/// `flags` 与 `depth` 正好填进去——加这两个排序信号**没有多花一个字节**。
#[derive(Clone, Copy)]
struct DirNode {
    record: u64,
    parent_record: u64,
    name_off: u32,
    name_len: u16,
    /// bit0 = 落在排除子树内；bit1..4 = [`Zone`]
    flags: u8,
    /// 从卷根算起的层数（根的直接子目录为 1），用于深度惩罚
    depth: u8,
}

impl DirNode {
    const EXCLUDED_BIT: u8 = 1;

    fn excluded(&self) -> bool {
        self.flags & Self::EXCLUDED_BIT != 0
    }

    fn set_excluded(&mut self, v: bool) {
        if v {
            self.flags |= Self::EXCLUDED_BIT;
        } else {
            self.flags &= !Self::EXCLUDED_BIT;
        }
    }

    fn zone(&self) -> Zone {
        Zone::from_bits((self.flags >> 1) & 0b111)
    }

    fn set_zone(&mut self, z: Zone) {
        self.flags = (self.flags & !(0b111 << 1)) | ((z as u8) << 1);
    }

    fn bias(&self) -> i8 {
        dir_bias(self.zone(), self.depth)
    }
}

/// 一个文件节点。被排除的文件根本不进这个数组。
///
/// 同样是零成本地多带一个字段：`8+4+2` 之后有 2 字节 padding，`bias` 填进其中之一。
#[derive(Clone, Copy)]
struct FileNode {
    parent_record: u64,
    name_off: u32,
    name_len: u16,
    /// 父目录位置权重的**快照**。存下来而不是打分时去二分查父目录：
    /// 命中条目可能上万，每条都查一次父目录是白白多出来的随机访问。
    bias: i8,
}

/// 一条搜索命中。`path` 已回溯拼好。
pub struct Hit {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    /// 打分（来自 [`Candidate::score`]）。
    ///
    /// 消费者是 `daemon::query`：它在存在性校验时顺带 stat 出修改时间，把
    /// `score + freshness_bonus` 作为最终排序键。所以物化之后**还会再排一次序**
    /// ——这是有意的，新鲜度信号只有拿到 Metadata 才算得出来。
    pub score: i64,
}

/// 单个卷的索引。
pub struct VolumeIndex {
    letter: char,
    /// 按 `record` 升序，二分查父目录用
    dirs: Vec<DirNode>,
    files: Vec<FileNode>,
    /// UTF-8 名字池，无分隔符，靠 (off,len) 切片
    names: Vec<u8>,
    /// 建索引时遇到的、名字过长存不下的条目数（诚实计数，不静默丢弃）
    dropped: usize,
    /// 被排除子树覆盖的条目数（目录 + 丢弃的文件），用于向用户交代「少了多少」
    excluded: usize,
    /// 增量追加的目录。**不并进 `dirs`**——那是有序数组，插入要 memmove 41 MB。
    /// 这里数量少（一个重建周期内新建的目录），线性查找足够，重建时归并回 `dirs`。
    extra_dirs: Vec<DirNode>,
    /// 排除名清单，增量追加新目录时要重新判定
    excludes: HashSet<String>,
}

/// 建索引的两阶段收集器。
///
/// # 为什么必须两阶段（别改成一遍过）
///
/// 判定「某文件是否在 `node_modules` 子树里」需要知道它父目录的**完整祖先链**，而 MFT 的
/// 枚举顺序是按记录号、**父目录可能后于子目录出现**。所以第一遍只收目录、建完树并标好
/// `excluded`，第二遍才按父目录的排除状态决定文件收不收。
///
/// 代价是全盘读两遍。换来的是**峰值内存等于稳态内存**——一遍过的写法得先把 1350 万条
/// 全收进来再筛，峰值 800 MB，那才是真问题。
///
/// 两遍并不慢：四个盘并行读，实测**全盘 17 秒**建完（单盘串行枚举一遍约 8-21 秒，
/// 见 `volume.rs`；四盘分布在三块物理磁盘上，并行几乎线性摊薄）。
pub struct IndexBuilder {
    letter: char,
    dirs: Vec<DirNode>,
    files: Vec<FileNode>,
    names: Vec<u8>,
    excludes: HashSet<String>,
    dropped: usize,
    excluded: usize,
}

impl IndexBuilder {
    pub fn new(letter: char, excludes: &[String]) -> Self {
        Self {
            letter,
            dirs: Vec::new(),
            files: Vec::new(),
            names: Vec::new(),
            excludes: excludes.iter().map(|s| s.to_lowercase()).collect(),
            dropped: 0,
            excluded: 0,
        }
    }

    /// 把 UTF-16 名字转 UTF-8 塞进池子。返回 `None` 表示放不下（长度超 u16），调用方计入 dropped。
    fn intern(&mut self, name: &[u16]) -> Option<(u32, u16)> {
        let off = u32::try_from(self.names.len()).ok()?;
        let before = self.names.len();
        // 直接把码元解码进池子，避免中间 String 分配
        for ch in char::decode_utf16(name.iter().copied()) {
            let ch = ch.unwrap_or(char::REPLACEMENT_CHARACTER);
            let mut buf = [0u8; 4];
            self.names.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
        match u16::try_from(self.names.len() - before) {
            Ok(len) => Some((off, len)),
            Err(_) => {
                // 超长（>64 KB 的 UTF-8 文件名，理论上不可能）——回滚池子，别留垃圾
                self.names.truncate(before);
                None
            }
        }
    }

    /// 第一阶段：只收目录条目。
    pub fn add_dir(&mut self, e: &RawEntry, name: &[u16]) {
        let Some((name_off, name_len)) = self.intern(name) else {
            self.dropped += 1;
            return;
        };
        self.dirs.push(DirNode {
            record: record_of(e.frn),
            parent_record: record_of(e.parent),
            name_off,
            name_len,
            // excluded / zone / depth 全部由 finish_dirs 一趟算出（那时父目录才齐）
            flags: 0,
            depth: 0,
        });
    }

    /// 第一阶段收尾：排序 + 自上而下标记排除子树。返回被排除的目录数（供日志核对）。
    ///
    /// 判定必须在这里一次做完：`add_dir` 时父目录可能还没出现，当场判不了。
    pub fn finish_dirs(&mut self) -> usize {
        self.dirs.sort_unstable_by_key(|d| d.record);
        // 名字命中排除清单的，自身先标上
        let mut seeds: Vec<usize> = Vec::new();
        for i in 0..self.dirs.len() {
            let d = self.dirs[i];
            let name = self.name_at(d.name_off, d.name_len).to_lowercase();
            if self.excludes.contains(&name) {
                seeds.push(i);
            }
        }
        for i in seeds {
            self.dirs[i].set_excluded(true);
        }
        // 再把三样东西一起向下传播：排除标记、区域（Zone）、深度。
        // 对每个目录回溯祖先链，然后**从最浅到最深**回填——回填顺序天然就是自顶向下，
        // 正好是 Zone/depth 的传播方向，不必为它们多走一趟。
        //
        // 用「回溯 + 就地写回」而不是建子节点表——后者要额外 40 MB 邻接表。
        let mut resolved = vec![false; self.dirs.len()];
        for i in 0..self.dirs.len() {
            if resolved[i] {
                continue;
            }
            // 收集从 i 向上的整条链，直到遇到已解析的节点 / 根 / 深度上限
            let mut chain = Vec::new();
            let mut cur = i;
            let mut inherited = false;
            // 链顶之上那个节点的状态；走到卷根时保持默认（Normal / 深度 0）
            let mut base_zone = Zone::Normal;
            let mut base_depth: u8 = 0;
            for _ in 0..MAX_DEPTH {
                if resolved[cur] {
                    inherited = self.dirs[cur].excluded();
                    base_zone = self.dirs[cur].zone();
                    base_depth = self.dirs[cur].depth;
                    break;
                }
                chain.push(cur);
                let parent = self.dirs[cur].parent_record;
                if parent == self.dirs[cur].record || self.dirs[cur].record == ROOT_RECORD {
                    break;
                }
                match self.dirs.binary_search_by_key(&parent, |d| d.record) {
                    Ok(p) => cur = p,
                    // 父目录不在本卷（跨卷挂载点等）→ 视为未排除的普通目录
                    Err(_) => break,
                }
            }
            // 回填整条链（最浅 → 最深）
            let mut zone = base_zone;
            let mut depth = base_depth;
            for &idx in chain.iter().rev() {
                depth = depth.saturating_add(1);
                let name = self.name_at(self.dirs[idx].name_off, self.dirs[idx].name_len);
                zone = child_zone(zone, depth, &name.to_lowercase());
                // 自身命中排除清单，或任一祖先被排除 → 整棵子树排除
                if self.dirs[idx].excluded() {
                    inherited = true;
                }
                self.dirs[idx].set_excluded(inherited);
                self.dirs[idx].set_zone(zone);
                self.dirs[idx].depth = depth;
                resolved[idx] = true;
            }
        }
        let n = self.dirs.iter().filter(|d| d.excluded()).count();
        self.excluded += n;
        n
    }

    /// 第二阶段：收文件，父目录被排除的直接丢弃。
    pub fn add_file(&mut self, e: &RawEntry, name: &[u16]) {
        if self.is_excluded_dir(record_of(e.parent)) {
            self.excluded += 1;
            return;
        }
        let Some((name_off, name_len)) = self.intern(name) else {
            self.dropped += 1;
            return;
        };
        self.files.push(FileNode {
            parent_record: record_of(e.parent),
            name_off,
            name_len,
            // 建索引时就把父目录的位置权重快照下来，省得每次搜索再查一遍
            bias: self.dir_bias_of(record_of(e.parent)),
        });
    }

    /// 取某个目录的位置权重；父目录不在本卷时按普通目录算
    fn dir_bias_of(&self, record: u64) -> i8 {
        self.dirs
            .binary_search_by_key(&record, |d| d.record)
            .map(|i| self.dirs[i].bias())
            .unwrap_or_else(|_| dir_bias(Zone::Normal, 1))
    }

    fn is_excluded_dir(&self, record: u64) -> bool {
        self.dirs
            .binary_search_by_key(&record, |d| d.record)
            .map(|i| self.dirs[i].excluded())
            .unwrap_or(false)
    }

    fn name_at(&self, off: u32, len: u16) -> &str {
        let s = off as usize;
        // 池子里存的是我们自己编码的 UTF-8，故 from_utf8_lossy 不会走到 lossy 分支
        std::str::from_utf8(&self.names[s..s + len as usize]).unwrap_or("")
    }

    pub fn build(self) -> VolumeIndex {
        let mut names = self.names;
        names.shrink_to_fit();
        let mut files = self.files;
        files.shrink_to_fit();
        let mut dirs = self.dirs;
        dirs.shrink_to_fit();
        VolumeIndex {
            letter: self.letter,
            dirs,
            files,
            names,
            dropped: self.dropped,
            excluded: self.excluded,
            extra_dirs: Vec::new(),
            excludes: self.excludes,
        }
    }
}

impl VolumeIndex {
    pub fn letter(&self) -> char {
        self.letter
    }

    /// 参与搜索的条目数（不含被排除的目录）
    pub fn searchable_count(&self) -> usize {
        self.files.len()
            + self
                .dirs
                .iter()
                .chain(self.extra_dirs.iter())
                .filter(|d| !d.excluded())
                .count()
    }

    /// 名字池 + 条目数组占用的字节数（用于把真实内存开销显示给用户，不靠估算）
    pub fn memory_bytes(&self) -> usize {
        self.names.capacity()
            + (self.dirs.capacity() + self.extra_dirs.capacity())
                * core::mem::size_of::<DirNode>()
            + self.files.capacity() * core::mem::size_of::<FileNode>()
    }

    pub fn dropped(&self) -> usize {
        self.dropped
    }

    pub fn excluded_count(&self) -> usize {
        self.excluded
    }

    fn name_at(&self, off: u32, len: u16) -> &str {
        let s = off as usize;
        std::str::from_utf8(&self.names[s..s + len as usize]).unwrap_or("")
    }

    /// 按记录号找目录：先二分主表，再线性扫增量表。
    fn find_dir(&self, record: u64) -> Option<DirNode> {
        if let Ok(i) = self.dirs.binary_search_by_key(&record, |d| d.record) {
            return Some(self.dirs[i]);
        }
        self.extra_dirs.iter().find(|d| d.record == record).copied()
    }

    /// 增量追加一条 USN 变更带来的新条目（新建 / 改名后的新名）。
    ///
    /// 父目录落在排除子树里的一律不收——否则 `cargo build` 一跑，
    /// `target/` 下几万个新文件会顺着增量爬回索引，把排除规则彻底架空。
    pub fn append(&mut self, e: &RawEntry, name: &[u16]) {
        let parent = record_of(e.parent);
        let parent_excluded = self.find_dir(parent).map(|d| d.excluded()).unwrap_or(false);
        if parent_excluded {
            self.excluded += 1;
            return;
        }
        let Some((name_off, name_len)) = self.intern(name) else {
            self.dropped += 1;
            return;
        };
        if e.is_dir {
            // 新目录自身的名字也要过一遍排除清单：新建的 node_modules 得当场排除，
            // 不然它下面后续新建的文件都会被当成正常条目收进来
            let excluded = self
                .excludes
                .contains(&self.name_at(name_off, name_len).to_lowercase());
            // 增量新建的目录：区域与深度继承父目录（新目录名本身只可能把它降为
            // 排除项，不会把它升进 UserCommon——那些目录名只在固定层级上认，见 child_zone）
            let (pz, pd) = self
                .find_dir(parent)
                .map(|d| (d.zone(), d.depth))
                .unwrap_or((Zone::Normal, 0));
            let mut node = DirNode {
                record: record_of(e.frn),
                parent_record: parent,
                name_off,
                name_len,
                flags: 0,
                depth: pd.saturating_add(1),
            };
            node.set_excluded(excluded);
            node.set_zone(pz);
            self.extra_dirs.push(node);
        } else {
            let bias = self
                .find_dir(parent)
                .map(|d| d.bias())
                .unwrap_or_else(|| dir_bias(Zone::Normal, 1));
            self.files.push(FileNode {
                parent_record: parent,
                name_off,
                name_len,
                bias,
            });
        }
    }

    /// 与 [`IndexBuilder::intern`] 同一套池子语义，供增量追加复用。
    fn intern(&mut self, name: &[u16]) -> Option<(u32, u16)> {
        let off = u32::try_from(self.names.len()).ok()?;
        let before = self.names.len();
        for ch in char::decode_utf16(name.iter().copied()) {
            let ch = ch.unwrap_or(char::REPLACEMENT_CHARACTER);
            let mut buf = [0u8; 4];
            self.names.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
        match u16::try_from(self.names.len() - before) {
            Ok(len) => Some((off, len)),
            Err(_) => {
                self.names.truncate(before);
                None
            }
        }
    }

    /// 从父目录记录号回溯出完整路径（含盘符），末尾**不带**分隔符。
    ///
    /// 父链断裂 / 成环时返回已拼出的部分并冠以盘符——宁可给出 `F:\…\某目录` 这样不完整
    /// 但真实的前缀，也不要凭空编一个路径出来。
    fn dir_path(&self, record: u64) -> String {
        let mut parts: Vec<&str> = Vec::new();
        let mut cur = record;
        for _ in 0..MAX_DEPTH {
            if cur == ROOT_RECORD {
                break;
            }
            // 走 find_dir 而非直接二分：增量新建的目录在 extra_dirs 里，
            // 漏了它就会把「新目录下的新文件」的路径回溯到错误的祖先上
            let Some(d) = self.find_dir(cur) else {
                break;
            };
            parts.push(self.name_at(d.name_off, d.name_len));
            if d.parent_record == d.record {
                break;
            }
            cur = d.parent_record;
        }
        let mut path = format!("{}:", self.letter);
        for part in parts.iter().rev() {
            path.push('\\');
            path.push_str(part);
        }
        path
    }

    /// 名字的**原始字节**。搜索热路径专用——不做 UTF-8 校验。
    ///
    /// 池子里的内容是 [`Self::intern`] 自己按 UTF-8 写进去的，一定合法；但 `from_utf8`
    /// 的校验要逐字节扫一遍，放在 733 万条的循环里就是纯浪费（实测占了搜索耗时的一大块）。
    /// 匹配与打分全部在字节层做，只有真要返回给用户的那几十条才转成 `str`（见 [`Self::materialize`]）。
    #[inline]
    fn name_bytes(&self, off: u32, len: u16) -> &[u8] {
        let s = off as usize;
        &self.names[s..s + len as usize]
    }

    /// 大小写无关的子串搜索，返回本卷分数最高的至多 `cap` 条**候选**。
    ///
    /// # 为什么产出候选而不是直接产出 `Hit`（2026-08-17 实测教训，别改回去）
    ///
    /// 最初的写法是命中即构造 `Hit`：当场回溯完整路径 + 两次 `String` 分配。
    /// 真机实测单次搜索 **208–273 ms**——因为搜「报告」这类高频词会命中上万条，
    /// 而最终只返回 20 条，等于为 99% 用不上的候选付了全部路径回溯与分配成本。
    ///
    /// 现在分两段：这里只算分数、记位置（16 字节 POD，零分配），跨卷合并排序后
    /// 才对最终要返回的那几十条调 [`Self::materialize`]。
    ///
    /// 不建倒排索引：倒排表在这个量级下比名字池本身还大，且只能加速**词前缀**
    /// ——那正是 Windows Search 那个「搜『报告』搜不到『季度报告.docx』」缺陷的来源。
    pub fn search_candidates(&self, needle_lower: &str, cap: usize) -> Vec<Candidate> {
        let needle = needle_lower.as_bytes();
        if needle.is_empty() || cap == 0 {
            return Vec::new();
        }
        // 分段并行扫。段内各自维护 top-cap，最后合并再取 top-cap——与串行结果等价，
        // 因为全局前 cap 名必然分布在各段的前 cap 名之内。
        //
        // 为什么要分段（2026-08-17 实测，四盘 733 万条，服务端自计时）：
        // 这是纯顺序内存扫描，四个卷并行后单次查询从 184-216 ms 降到 68-96 ms，
        // 但**只用掉 4 条线程**——本机 32 核，其余全闲着，而最大的 C 盘一个人就占掉
        // 那 68 ms 里的大半。段内再切开把核用满后降到 **28-40 ms**。
        let shards = self.shard_count();
        let total = self.files.len();
        let mut merged: Vec<Candidate> = if shards <= 1 || total < MIN_PARALLEL_ENTRIES {
            // 小索引不值得开线程：spawn 本身要几十微秒，条目少时纯亏
            self.scan_files(needle, cap, 0..total)
        } else {
            let per = total.div_ceil(shards);
            std::thread::scope(|scope| {
                let handles: Vec<_> = (0..shards)
                    .map(|k| {
                        let start = k * per;
                        let end = ((k + 1) * per).min(total);
                        scope.spawn(move || self.scan_files(needle, cap, start..end))
                    })
                    .collect();
                handles
                    .into_iter()
                    // 单段 panic 不该让整次查询失败：丢掉那一段继续
                    .filter_map(|h| h.join().ok())
                    .flatten()
                    .collect()
            })
        };
        // 目录数量比文件少一个数量级（本机 170 万 vs 560 万），串行足够
        merged.extend(self.scan_dirs(needle, cap));

        let mut ignored = i64::MIN;
        trim(&mut merged, cap, &mut ignored);
        merged
    }

    /// 本卷该切几段扫。既受可用核数约束，也别为小卷开一堆线程。
    ///
    /// 除以 4 是因为调用方（`daemon::query`）已经在**卷级**并行了：本机四个盘同时在搜，
    /// 每个卷再切满核数会开出 4×32 条线程，光调度开销就吃掉收益。
    fn shard_count(&self) -> usize {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        (cores / 4).clamp(1, MAX_SCAN_SHARDS)
    }

    /// 扫 `files` 的一段，返回该段的 top-`cap`。
    fn scan_files(&self, needle: &[u8], cap: usize, range: std::ops::Range<usize>) -> Vec<Candidate> {
        let mut out: Vec<Candidate> = Vec::new();
        // 已攒够 cap 条后，低于当前最低分的候选直接丢——省掉后续 push 与排序开销。
        // 攒到 4×cap 才整理一次，摊薄排序成本。
        let flush_at = cap.saturating_mul(4);
        let mut floor = i64::MIN;
        for i in range {
            let f = self.files[i];
            let name = self.name_bytes(f.name_off, f.name_len);
            if let Some(pos) = find_ci(name, needle) {
                let s = score(name, needle, pos, false, f.bias);
                if s <= floor {
                    continue;
                }
                out.push(Candidate {
                    score: s,
                    slot: Slot::File(i as u32),
                });
                if out.len() >= flush_at {
                    trim(&mut out, cap, &mut floor);
                }
            }
        }
        out
    }

    /// 扫全部目录（主表 + 增量表），返回 top-`cap`。
    fn scan_dirs(&self, needle: &[u8], cap: usize) -> Vec<Candidate> {
        let mut out: Vec<Candidate> = Vec::new();
        let flush_at = cap.saturating_mul(4);
        let mut floor = i64::MIN;
        for (i, d) in self.dirs.iter().enumerate() {
            if d.excluded() {
                continue;
            }
            let name = self.name_bytes(d.name_off, d.name_len);
            if let Some(pos) = find_ci(name, needle) {
                let s = score(name, needle, pos, true, d.bias());
                if s <= floor {
                    continue;
                }
                out.push(Candidate {
                    score: s,
                    slot: Slot::Dir(i as u32),
                });
                if out.len() >= flush_at {
                    trim(&mut out, cap, &mut floor);
                }
            }
        }
        for (i, d) in self.extra_dirs.iter().enumerate() {
            if d.excluded() {
                continue;
            }
            let name = self.name_bytes(d.name_off, d.name_len);
            if let Some(pos) = find_ci(name, needle) {
                let s = score(name, needle, pos, true, d.bias());
                if s <= floor {
                    continue;
                }
                out.push(Candidate {
                    score: s,
                    slot: Slot::ExtraDir(i as u32),
                });
            }
        }
        out
    }

    /// 把候选变成完整命中：**这一步才**回溯路径、分配 `String`、做 UTF-8 转换。
    ///
    /// 只对最终要返回给用户的条目调用（几十条），所以这里的开销无所谓。
    pub fn materialize(&self, c: &Candidate) -> Hit {
        let (name_off, name_len, parent, is_dir) = match c.slot {
            Slot::File(i) => {
                let f = self.files[i as usize];
                (f.name_off, f.name_len, f.parent_record, false)
            }
            Slot::Dir(i) => {
                let d = self.dirs[i as usize];
                (d.name_off, d.name_len, d.record, true)
            }
            Slot::ExtraDir(i) => {
                let d = self.extra_dirs[i as usize];
                (d.name_off, d.name_len, d.record, true)
            }
        };
        let name = self.name_at(name_off, name_len);
        // 目录的路径回溯从它**自己**的记录号开始（含自身名）；文件从父目录开始再拼上文件名
        let path = if is_dir {
            self.dir_path(parent)
        } else {
            format!("{}\\{}", self.dir_path(parent), name)
        };
        Hit {
            name: name.to_string(),
            path,
            is_dir,
            score: c.score,
        }
    }
}

/// 候选在卷内的定位。用下标而不是引用，才能让 [`Candidate`] 是 `Copy` 的 POD。
#[derive(Clone, Copy)]
enum Slot {
    File(u32),
    Dir(u32),
    ExtraDir(u32),
}

/// 一条搜索候选：只有分数与位置，**不含任何堆分配**。
///
/// 16 字节，可放心在跨卷合并时成千上万地搬来搬去。
#[derive(Clone, Copy)]
pub struct Candidate {
    pub score: i64,
    slot: Slot,
}

/// 按分数降序排、截到 `cap` 条，并把新的「最低分」写回 `floor` 作为后续快速拒绝的阈值。
fn trim(out: &mut Vec<Candidate>, cap: usize, floor: &mut i64) {
    // Reverse = 分数高的在前，与 `|a, b| b.score.cmp(&a.score)` 等价
    out.sort_unstable_by_key(|c| std::cmp::Reverse(c.score));
    out.truncate(cap);
    if out.len() >= cap {
        if let Some(last) = out.last() {
            *floor = last.score;
        }
    }
}

/// ASCII 大小写无关的子串查找，返回命中起始字节位置。
///
/// 只折叠 ASCII 字母：非 ASCII（中文等）在 UTF-8 下本无大小写，逐字节比对即正确。
/// 不做 Unicode 完整 case folding——那需要查表且会让这个热路径慢一个数量级，
/// 而文件名搜索场景下 ASCII 折叠已覆盖实际需求。
#[inline]
fn find_ci(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let first = lower(needle[0]);
    let last_start = haystack.len() - needle.len();
    'outer: for i in 0..=last_start {
        if lower(haystack[i]) != first {
            continue;
        }
        for j in 1..needle.len() {
            if lower(haystack[i + j]) != lower(needle[j]) {
                continue 'outer;
            }
        }
        return Some(i);
    }
    None
}

#[inline]
fn lower(b: u8) -> u8 {
    if b.is_ascii_uppercase() {
        b | 0x20
    } else {
        b
    }
}

/// 命中打分。越大越靠前。
///
/// 排序意图：完全同名 > 从头匹配 > 词首匹配 > 中间匹配；同档内短名字优先（更可能是
/// 用户心里想的那个），目录略优于文件（用户搜目录名时通常就是想打开那个目录）。
/// 命中打分。越大越靠前。
///
/// # 各档权重的用意（按影响力从大到小）
///
/// 1. **匹配质量**（0–1000）：完全同名 > 从头匹配 > 词首 / 词尾 > 中间。这一档最大，
///    位置再好也不该让模糊匹配盖过精确匹配。
/// 2. **扩展名**（−200–70）：重点不是给文档加分，而是把 `.tmp/.log/.bak/.pdb` 这类
///    **机器产物压下去**——它们在盘上数量极大，却几乎从不是搜索目标。
/// 3. **位置**（约 −60–320）：桌面/文档/下载 里的东西优先于系统深处，越浅越优先。
/// 4. **名字长度**（0–200）：同等条件下短名字更可能是用户心里想的那个。
///
/// 「最近修改」不在这里——MFT 枚举拿不到有意义的时间戳，它由 `daemon::query` 在
/// 存在性校验时顺带 stat 出来，作用于最终返回的那几十条（见 `freshness_bonus`）。
fn score(name: &[u8], needle: &[u8], pos: usize, is_dir: bool, bias: i8) -> i64 {
    let mut s = 0i64;
    if name.len() == needle.len() {
        s += 1000; // 完全等长且命中 → 就是它
    }
    let at_word_start = pos == 0 || is_boundary(name[pos - 1]);
    // 词尾：匹配段之后是名字末尾、扩展名的点、或别的分隔符。
    // 这一档是「搜『报告』要能命中『季度报告.docx』」的关键——那种名字里
    // 「报告」不在词首，只有词尾能把它从一堆无关的中间匹配里拉起来。
    let end = pos + needle.len();
    let at_word_end = end >= name.len() || is_boundary(name[end]);
    if pos == 0 {
        s += 500;
    } else if at_word_start {
        s += 250;
    }
    if at_word_end && pos != 0 {
        s += 150;
    }
    // 整词命中（两端都是边界）额外再给一点：`报告.docx` 比 `报告书初稿.docx` 更贴近意图
    if at_word_start && at_word_end {
        s += 100;
    }
    if is_dir {
        s += 40;
    } else {
        s += ext_bias(name);
    }
    s += bias as i64 * 4;
    // 名字越短越靠前（上限 200 分，避免长名字被压到无序）
    s += 200i64.saturating_sub(name.len() as i64);
    s
}

/// 文件名里的「词」边界字符
#[inline]
fn is_boundary(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'_' | b'-' | b'.' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b',' | b'~' | b'@' | b'#' | b'+'
    )
}

/// 按扩展名给的类型权重。
///
/// 负分那一档才是重点：`.tmp`/`.log`/`.pdb`/`.rmeta` 这些在盘上动辄几十万个，
/// 名字还常常和真实文件同名（`main.pdb` vs `main.rs`），不压下去就会淹没结果。
fn ext_bias(name: &[u8]) -> i64 {
    let Some(dot) = name.iter().rposition(|&b| b == b'.') else {
        return 0;
    };
    let ext = &name[dot + 1..];
    if ext.is_empty() || ext.len() > 8 {
        return 0;
    }
    let mut buf = [0u8; 8];
    for (i, &b) in ext.iter().enumerate() {
        buf[i] = lower(b);
    }
    match &buf[..ext.len()] {
        // 用户文档：按文件名找东西时，绝大多数是在找这些
        b"doc" | b"docx" | b"xls" | b"xlsx" | b"ppt" | b"pptx" | b"pdf" | b"txt" | b"md"
        | b"rtf" | b"csv" | b"odt" | b"ods" | b"one" | b"vsd" | b"vsdx" => 70,
        // 媒体
        b"jpg" | b"jpeg" | b"png" | b"gif" | b"webp" | b"bmp" | b"svg" | b"psd" | b"ai"
        | b"mp3" | b"mp4" | b"mkv" | b"avi" | b"mov" | b"wav" | b"flac" => 45,
        // 源码与配置
        b"rs" | b"ts" | b"tsx" | b"js" | b"jsx" | b"vue" | b"py" | b"go" | b"java" | b"kt"
        | b"cs" | b"cpp" | b"cc" | b"c" | b"h" | b"hpp" | b"php" | b"rb" | b"swift"
        | b"json" | b"toml" | b"yaml" | b"yml" | b"xml" | b"ini" | b"conf" | b"html"
        | b"css" | b"scss" | b"sql" | b"sh" | b"ps1" | b"bat" => 40,
        // 程序与归档
        b"exe" | b"msi" | b"lnk" | b"zip" | b"7z" | b"rar" | b"tar" | b"gz" | b"iso" => 25,
        // 机器产物：数量极大且几乎不会是搜索目标
        b"tmp" | b"temp" | b"log" | b"bak" | b"old" | b"cache" | b"lock" | b"swp"
        | b"pyc" | b"pyo" | b"obj" | b"o" | b"a" | b"lib" | b"pdb" | b"ilk" | b"exp"
        | b"idb" | b"rmeta" | b"rlib" | b"d" | b"map" | b"dmp" | b"etl" | b"crc"
        | b"crdownload" | b"part" | b"~tmp" => -200,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn raw(frn: u64, parent: u64, is_dir: bool) -> RawEntry {
        RawEntry {
            frn,
            parent,
            is_dir,
            is_reparse: false,
        }
    }

    /// 造一棵小树：F:\proj\src\main.rs + F:\proj\node_modules\lodash\index.js
    /// 断言 node_modules 整棵子树被排除，正常文件能搜到、路径回溯正确。
    fn build_sample() -> VolumeIndex {
        let excludes: Vec<String> = DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect();
        let mut b = IndexBuilder::new('F', &excludes);
        // 目录：proj(100)←根5, src(101)←100, node_modules(102)←100, lodash(103)←102
        b.add_dir(&raw(100, ROOT_RECORD, true), &w("proj"));
        b.add_dir(&raw(101, 100, true), &w("src"));
        b.add_dir(&raw(102, 100, true), &w("node_modules"));
        b.add_dir(&raw(103, 102, true), &w("lodash"));
        let excluded = b.finish_dirs();
        assert_eq!(excluded, 2, "node_modules 与其下的 lodash 都应被排除");
        // 文件
        b.add_file(&raw(200, 101, false), &w("main.rs"));
        b.add_file(&raw(201, 103, false), &w("index.js")); // 在 node_modules 里 → 应丢弃
        b.add_file(&raw(202, 100, false), &w("报告 v2.docx"));
        b.build()
    }

    /// 测试辅助：走一遍生产路径的两段式（候选 → 排序 → 物化）。
    /// 与 `daemon::query` 的做法一致，只是省掉跨卷合并与存在性校验。
    fn search_hits(idx: &VolumeIndex, q: &str, limit: usize) -> Vec<Hit> {
        let cands = idx.search_candidates(&q.to_lowercase(), limit.max(1));
        cands.iter().take(limit).map(|c| idx.materialize(c)).collect()
    }

    #[test]
    fn excludes_whole_subtree() {
        let idx = build_sample();
        let hits = search_hits(&idx, "index.js", 10);
        assert!(
            hits.is_empty(),
            "node_modules 里的文件不应进索引，实得 {}",
            hits.len()
        );
        let hits = search_hits(&idx, "lodash", 10);
        assert!(hits.is_empty(), "被排除的目录本身也不该作为结果出现");
    }

    #[test]
    fn path_is_reconstructed_from_parent_chain() {
        let idx = build_sample();
        let hits = search_hits(&idx, "main.rs", 10);
        assert_eq!(hits.len(), 1, "应恰好命中 main.rs");
        assert_eq!(hits[0].path, r"F:\proj\src\main.rs");
        assert!(!hits[0].is_dir);
    }

    /// 目录命中时，路径必须是它**自己**（含自身名），不能少一层也不能多拼一次。
    /// 两段式重构把「目录从自身记录号回溯、文件从父目录回溯」挪到了 materialize，
    /// 这里钉住那个分支。
    #[test]
    fn directory_hit_path_includes_itself() {
        let idx = build_sample();
        let hits = search_hits(&idx, "src", 10);
        assert_eq!(hits.len(), 1, "应命中目录 src");
        assert!(hits[0].is_dir);
        assert_eq!(hits[0].path, r"F:\proj\src", "目录路径应含自身名且不重复拼接");
    }

    /// 这条正是 Windows Search 做不到的：搜「报告」要能命中「报告 v2.docx」，
    /// 也要能命中中间位置——CONTAINS 只支持词前缀，这里必须是真子串。
    #[test]
    fn matches_substring_not_just_word_prefix() {
        let idx = build_sample();
        for q in ["报告", "v2", "docx", "告 v"] {
            let hits = search_hits(&idx, q, 10);
            assert_eq!(hits.len(), 1, "搜 {q:?} 应命中「报告 v2.docx」");
            assert_eq!(hits[0].name, "报告 v2.docx");
        }
    }

    #[test]
    fn search_is_case_insensitive() {
        let idx = build_sample();
        for q in ["main.rs", "MAIN.RS", "Main", "AIN.R"] {
            let hits = search_hits(&idx, q, 10);
            assert_eq!(hits.len(), 1, "搜 {q:?} 应命中 main.rs");
        }
    }

    /// 打分意图：完全同名 > 前缀 > 中间命中
    #[test]
    fn scoring_prefers_exact_then_prefix() {
        let mut b = IndexBuilder::new('C', &[]);
        b.add_dir(&raw(100, ROOT_RECORD, true), &w("d"));
        b.finish_dirs();
        b.add_file(&raw(200, 100, false), &w("log"));
        b.add_file(&raw(201, 100, false), &w("log-2026.txt"));
        b.add_file(&raw(202, 100, false), &w("app.log"));
        let idx = b.build();

        let names: Vec<String> = search_hits(&idx, "log", 10)
            .into_iter()
            .map(|h| h.name)
            .collect();
        assert_eq!(
            names,
            vec!["log", "log-2026.txt", "app.log"],
            "排序不符预期（search_candidates 应已按分数降序返回）"
        );
    }

    /// top-K 剪枝不能把本该入选的高分条目丢掉。
    ///
    /// `search_candidates` 攒到 4×cap 就整理一次并抬高快速拒绝阈值——若阈值用错了
    /// 比较方向，会把后出现的高分条目误杀。这里让高分条目**排在最后**被遍历到。
    #[test]
    fn top_k_pruning_keeps_the_best_even_if_seen_last() {
        let mut b = IndexBuilder::new('C', &[]);
        b.add_dir(&raw(100, ROOT_RECORD, true), &w("d"));
        b.finish_dirs();
        // 先塞 200 个低分命中（长名字、中间匹配），再塞一个完全同名的最高分条目
        for i in 0..200u32 {
            b.add_file(&raw(1000 + i as u64, 100, false), &w(&format!("xx-target-{i}.dat")));
        }
        b.add_file(&raw(9999, 100, false), &w("target"));
        let idx = b.build();

        let hits = search_hits(&idx, "target", 5);
        assert_eq!(
            hits[0].name, "target",
            "完全同名的条目即使最后才被遍历到，也必须排第一（剪枝阈值方向错了会丢掉它）"
        );
    }

    /// 父链成环不能让回溯死循环
    #[test]
    fn cyclic_parent_chain_terminates() {
        let mut b = IndexBuilder::new('C', &[]);
        b.add_dir(&raw(100, 101, true), &w("a"));
        b.add_dir(&raw(101, 100, true), &w("b")); // 100 ↔ 101 互为父
        b.finish_dirs();
        b.add_file(&raw(200, 100, false), &w("x.txt"));
        let idx = b.build();
        let hits = search_hits(&idx, "x.txt", 10); // 不 hang 即通过
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.starts_with("C:"), "成环时也要给出真实前缀");
    }

    /// FRN 高 16 位是序列号，必须被忽略，否则父子关联不上
    #[test]
    fn sequence_number_in_frn_is_ignored() {
        let mut b = IndexBuilder::new('C', &[]);
        // 目录 FRN 带序列号 0x0002，文件的 parent 带 0x0007——记录号同为 100
        b.add_dir(&raw(0x0002_0000_0000_0064, ROOT_RECORD, true), &w("dir"));
        b.finish_dirs();
        b.add_file(&raw(200, 0x0007_0000_0000_0064, false), &w("f.txt"));
        let idx = b.build();
        let hits = search_hits(&idx, "f.txt", 10);
        assert_eq!(hits[0].path, r"C:\dir\f.txt", "序列号未被忽略，父子关联失败");
    }

    /// 造一棵带真实区域结构的树：
    /// `C:\Users\me\Desktop`（UserCommon）、`C:\Users\me\AppData`（System）、
    /// `C:\Windows\System32`（System）、`C:\proj`（Normal）
    fn build_zoned() -> IndexBuilder {
        let mut b = IndexBuilder::new('C', &[]);
        b.add_dir(&raw(10, ROOT_RECORD, true), &w("Users"));
        b.add_dir(&raw(11, 10, true), &w("me"));
        b.add_dir(&raw(12, 11, true), &w("Desktop"));
        b.add_dir(&raw(13, 11, true), &w("AppData"));
        b.add_dir(&raw(14, 13, true), &w("Roaming"));
        b.add_dir(&raw(20, ROOT_RECORD, true), &w("Windows"));
        b.add_dir(&raw(21, 20, true), &w("System32"));
        b.add_dir(&raw(30, ROOT_RECORD, true), &w("proj"));
        b.finish_dirs();
        b
    }

    /// 区域判定必须按**层级**来，不能只看名字——否则 `D:\备份\Users\...` 会被误判
    #[test]
    fn zones_are_assigned_by_position_not_just_name() {
        let idx = build_zoned().build();
        let zone_of = |record: u64| idx.find_dir(record).map(|d| d.zone());
        assert_eq!(zone_of(10), Some(Zone::UsersRoot), "C:\\Users");
        assert_eq!(zone_of(11), Some(Zone::UserProfile), "C:\\Users\\me");
        assert_eq!(zone_of(12), Some(Zone::UserCommon), "桌面");
        assert_eq!(
            zone_of(13),
            Some(Zone::System),
            "AppData 虽在 profile 内，但里面是程序自己的东西"
        );
        assert_eq!(zone_of(14), Some(Zone::System), "AppData 子树应继承 System");
        assert_eq!(zone_of(20), Some(Zone::System), "C:\\Windows");
        assert_eq!(zone_of(21), Some(Zone::System));
        assert_eq!(zone_of(30), Some(Zone::Normal), "普通目录");

        // 深度也要正确（卷根的直接子目录 = 1）
        assert_eq!(idx.find_dir(10).map(|d| d.depth), Some(1));
        assert_eq!(idx.find_dir(12).map(|d| d.depth), Some(3));
    }

    /// 同名文件，桌面上的那个要排在 Windows 深处的前面。
    /// 这是「位置权重」存在的全部理由。
    #[test]
    fn desktop_beats_system_for_identical_names() {
        let mut b = build_zoned();
        b.add_file(&raw(200, 12, false), &w("方案.docx")); // 桌面
        b.add_file(&raw(201, 21, false), &w("方案.docx")); // System32
        b.add_file(&raw(202, 14, false), &w("方案.docx")); // AppData\Roaming
        let idx = b.build();

        let hits = search_hits(&idx, "方案", 10);
        assert_eq!(hits.len(), 3, "三个同名文件都该命中");
        assert_eq!(
            hits[0].path, r"C:\Users\me\Desktop\方案.docx",
            "桌面上的应排第一，实际顺序：{:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
    }

    /// 机器产物（.tmp/.log/.pdb）必须沉到真实文件后面——它们数量巨大且几乎从不是搜索目标
    #[test]
    fn machine_generated_extensions_sink() {
        let mut b = IndexBuilder::new('C', &[]);
        b.add_dir(&raw(100, ROOT_RECORD, true), &w("d"));
        b.finish_dirs();
        b.add_file(&raw(200, 100, false), &w("report.tmp"));
        b.add_file(&raw(201, 100, false), &w("report.log"));
        b.add_file(&raw(202, 100, false), &w("report.docx"));
        b.add_file(&raw(203, 100, false), &w("report.pdb"));
        let idx = b.build();

        let names: Vec<String> = search_hits(&idx, "report", 10)
            .into_iter()
            .map(|h| h.name)
            .collect();
        assert_eq!(
            names[0], "report.docx",
            "文档应排第一，实际顺序：{names:?}"
        );
        assert!(
            names.iter().position(|n| n == "report.docx").unwrap()
                < names.iter().position(|n| n == "report.tmp").unwrap(),
            "「.tmp」不该压过真实文档：{names:?}"
        );
    }

    /// 词尾匹配：搜「报告」要能把「季度报告.docx」从一堆中间匹配里拉起来。
    ///
    /// 这正是 2026-08-17 真机验证里唯一没通过的那条——当时「报告」在名字中间、
    /// 既不在词首也没有词尾加分，结果被挤出前 30 名。
    #[test]
    fn word_end_match_lifts_suffix_hits() {
        let mut b = IndexBuilder::new('D', &[]);
        b.add_dir(&raw(100, ROOT_RECORD, true), &w("d"));
        b.finish_dirs();
        // 「报告」处于词尾（后面是扩展名的点）
        b.add_file(&raw(200, 100, false), &w("2026年第三季度报告.docx"));
        // 「报告」在中间，两侧都不是边界——应排在后面
        b.add_file(&raw(201, 100, false), &w("关于报告制度的说明文件.docx"));
        let idx = b.build();

        let names: Vec<String> = search_hits(&idx, "报告", 10)
            .into_iter()
            .map(|h| h.name)
            .collect();
        assert_eq!(names.len(), 2);
        assert_eq!(
            names[0], "2026年第三季度报告.docx",
            "词尾命中应优先于纯中间命中：{names:?}"
        );
    }

    /// 位置权重不能盖过匹配质量：一个埋在系统目录里的**精确**匹配，
    /// 仍应排在桌面上一个模糊的中间匹配之前。
    #[test]
    fn location_never_outweighs_match_quality() {
        let mut b = build_zoned();
        b.add_file(&raw(200, 21, false), &w("方案")); // System32，完全同名
        b.add_file(&raw(201, 12, false), &w("很长的方案相关材料汇编.docx")); // 桌面，中间匹配
        let idx = b.build();

        let hits = search_hits(&idx, "方案", 10);
        assert_eq!(
            hits[0].name, "方案",
            "精确匹配必须压过位置优势，实际：{:?}",
            hits.iter().map(|h| &h.name).collect::<Vec<_>>()
        );
    }

    /// 越浅越优先（同区域、同匹配质量时）
    #[test]
    fn shallower_paths_rank_higher() {
        let mut b = IndexBuilder::new('D', &[]);
        b.add_dir(&raw(100, ROOT_RECORD, true), &w("proj"));
        let mut parent = 100u64;
        for (i, seg) in ["a", "b", "c", "d", "e"].iter().enumerate() {
            b.add_dir(&raw(110 + i as u64, parent, true), &w(seg));
            parent = 110 + i as u64;
        }
        b.finish_dirs();
        b.add_file(&raw(200, 100, false), &w("方案.docx")); // D:\proj\
        b.add_file(&raw(201, parent, false), &w("方案.docx")); // D:\proj\a\b\c\d\e\
        let idx = b.build();

        let hits = search_hits(&idx, "方案", 10);
        assert_eq!(
            hits[0].path, r"D:\proj\方案.docx",
            "浅的应排前面，实际：{:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
    }

    /// 分段并行扫的结果必须与串行**逐条等价**。
    ///
    /// 上面所有用例的索引都远小于 [`MIN_PARALLEL_ENTRIES`]，走的全是串行分支
    /// ——并行分支等于没被覆盖。这里造一个够大的索引把它逼出来，并与串行结果对账。
    /// 分段边界算错（漏一段 / 段间重叠）在这条用例里会当场暴露。
    #[test]
    fn parallel_shards_agree_with_serial() {
        let mut b = IndexBuilder::new('C', &[]);
        b.add_dir(&raw(100, ROOT_RECORD, true), &w("d"));
        b.finish_dirs();
        // 超过并行阈值，且分数各不相同（名字长度递增 → score 递减），便于对账排序
        let total = MIN_PARALLEL_ENTRIES + 1234;
        for i in 0..total {
            b.add_file(&raw(1000 + i as u64, 100, false), &w(&format!("t{}-needle.dat", "x".repeat(i % 40))));
        }
        let idx = b.build();
        assert!(
            idx.files.len() >= MIN_PARALLEL_ENTRIES,
            "索引必须够大才能走到并行分支，实得 {}",
            idx.files.len()
        );
        assert!(idx.shard_count() >= 1);

        let cap = 200;
        // 并行路径（search_candidates 内部按 shard_count 分段）
        let mut par = idx.search_candidates("needle", cap);
        // 串行基准：一整段扫完
        let mut ser = idx.scan_files(b"needle", cap, 0..idx.files.len());
        let mut ignored = i64::MIN;
        trim(&mut ser, cap, &mut ignored);

        assert_eq!(par.len(), ser.len(), "并行与串行的候选条数应相同");
        // 同分条目的相对顺序不保证（sort_unstable + 分段合并），故按 (分数, 路径) 对账
        let key = |c: &Candidate| (c.score, idx.materialize(c).path);
        let mut par_keys: Vec<_> = par.iter_mut().map(|c| key(c)).collect();
        let mut ser_keys: Vec<_> = ser.iter_mut().map(|c| key(c)).collect();
        par_keys.sort();
        ser_keys.sort();
        assert_eq!(par_keys, ser_keys, "并行分段的结果集与串行不一致");
    }

    /// 增量追加的条目必须能被搜到，且路径回溯要走 extra_dirs
    #[test]
    fn appended_entries_are_searchable_with_correct_path() {
        let mut b = IndexBuilder::new('D', &DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        b.add_dir(&raw(100, ROOT_RECORD, true), &w("work"));
        b.finish_dirs();
        let mut idx = b.build();

        // 新建目录 work\新项目(101)，再在其下新建文件
        idx.append(&raw(101, 100, true), &w("新项目"));
        idx.append(&raw(200, 101, false), &w("方案.md"));
        let hits = search_hits(&idx, "方案", 10);
        assert_eq!(hits.len(), 1, "增量追加的文件应能搜到");
        assert_eq!(hits[0].path, r"D:\work\新项目\方案.md");

        // 新建的 node_modules 要当场排除，其下后续新建的文件也不该进索引
        idx.append(&raw(300, 100, true), &w("node_modules"));
        idx.append(&raw(301, 300, false), &w("方案-noise.md"));
        let hits = search_hits(&idx, "方案", 10);
        assert_eq!(
            hits.len(),
            1,
            "新建的 node_modules 子树必须当场排除，否则 cargo build/npm i 会把索引灌满"
        );
    }
}

//! 提权索引守护进程：建 MFT 索引、跟 USN 日志增量、应答主进程的查询。
//!
//! 进程入口是 `itools.exe --mft-daemon`（见 `main.rs`），由主进程按需提权拉起。
//! 它**不创建任何窗口**，纯后台。
//!
//! # 增量维护为什么只处理「新增」，不处理「删除」
//!
//! 直觉写法是给每个条目存 FRN、删除时打墓碑，代价是每条多 8 字节（464 万条 = +35 MB）、
//! 外加墓碑回收与名字池碎片整理两套逻辑。这里改用一个更简单且**正确性更强**的办法：
//!
//! - USN 日志里的 `FILE_CREATE`/`RENAME_NEW_NAME` → 直接追加进索引；
//! - 删除与改名的旧名**不从索引里摘**，而是在返回结果前对候选做一次
//!   `fs::metadata` 存在性校验（只校验最终要返回的几十条，实测每条约 10 µs）。
//!
//! 这样已删除/已改名的条目一律不会出现在结果里，省掉了墓碑机制；索引里的陈旧条目
//! 由定期全量重建回收。副作用是「删了很多文件后索引会虚胖」——用重建周期兜住，
//! 而不是让内存里长出一套 GC。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use super::index::{IndexBuilder, VolumeIndex, DEFAULT_EXCLUDES};
use super::ipc::{self, HitDto, Request, Response, StatusDto};
use super::volume::{fixed_ntfs_drives, Volume};

/// USN 增量轮询间隔。1 秒足够让「刚存的文件立刻能搜到」，也不会有明显 CPU 占用
/// （无变更时一次非阻塞 IOCTL 即返回）。
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 全量重建周期。回收陈旧条目（删除的文件在索引里的残留）与名字池碎片。
const REBUILD_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// 返回给主进程前，最多校验多少条候选的真实存在性。
const VERIFY_CAP: usize = 200;

struct Shared {
    volumes: RwLock<Vec<VolumeIndex>>,
    status: RwLock<StatusDto>,
    /// 置位后各卷的增量线程会在下一轮退出，让全量重建独占
    rebuilding: AtomicBool,
    /// 最近一次查询在守护侧的耗时（微秒）。用原子量而不是塞进 `status` 的锁里：
    /// 每次查询都要写它，不该为此去抢那把被 Status 请求读着的写锁。
    last_query_us: AtomicU64,
}

/// 守护进程主入口。建完索引即开始应答；建索引期间查询会拿到 `state="building"`。
pub fn run() -> std::io::Result<()> {
    let shared = Arc::new(Shared {
        volumes: RwLock::new(Vec::new()),
        status: RwLock::new(StatusDto {
            state: "building".to_string(),
            ..Default::default()
        }),
        rebuilding: AtomicBool::new(false),
        last_query_us: AtomicU64::new(0),
    });

    // 先把索引建起来（各盘并行），再进增量循环
    {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            build_all(&shared);
            incremental_loop(&shared);
        });
    }

    let handler_shared = Arc::clone(&shared);
    ipc::serve(move |req| handle(&handler_shared, req))
}

fn handle(shared: &Arc<Shared>, req: Request) -> Response {
    match req {
        Request::Status => {
            let mut dto = shared
                .status
                .read()
                .map(|s| s.clone())
                .unwrap_or_else(|_| StatusDto {
                    state: "error".to_string(),
                    ..Default::default()
                });
            // 微秒转毫秒向上取整：亚毫秒的查询报 1 ms 而不是 0，避免看起来「没测到」
            let us = shared.last_query_us.load(Ordering::Relaxed);
            dto.last_query_ms = if us == 0 { 0 } else { us.div_ceil(1000) };
            Response::Status(dto)
        }
        Request::Query { needle, limit } => {
            // 首次建索引期间必须报「后端不可用」而不是回一个空命中列表。
            //
            // 主进程按 `Some(vec![]) = 索引可用但确实没匹配` 处理，收到空列表就**不再降级**
            // （见 search/mod.rs 的 pick_file_backend）。若这里回空列表，用户在首次建索引的
            // 那几十秒里搜什么都是「未找到」——而 Windows Search 本来还能给他一些结果。
            //
            // 判据是 `volumes` 空而非 `state=="building"`：**重建**时 volumes 里仍是上一版
            // 可用的索引，那种情况要继续拿旧索引服务，比降级到覆盖更窄的后端好。
            let empty = shared.volumes.read().map(|v| v.is_empty()).unwrap_or(true);
            if empty {
                return Response::Error("索引尚未就绪".to_string());
            }
            let started = Instant::now();
            let hits = query(shared, &needle, limit);
            shared
                .last_query_us
                .store(started.elapsed().as_micros() as u64, Ordering::Relaxed);
            Response::Hits(hits)
        }
        Request::Rebuild => {
            let shared = Arc::clone(shared);
            std::thread::spawn(move || {
                build_all(&shared);
            });
            Response::Ok
        }
        Request::Shutdown => {
            // 先把响应发出去再退，否则客户端等不到回包只能超时——用户点「关闭全盘索引」
            // 会看到一次莫名的失败。延时给 IPC 层足够时间把帧写完。
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(300));
                eprintln!("[mft-daemon] 收到关闭请求，退出");
                std::process::exit(0);
            });
            Response::Ok
        }
    }
}

fn query(shared: &Arc<Shared>, needle: &str, limit: usize) -> Vec<HitDto> {
    let needle = needle.to_lowercase();
    let Ok(volumes) = shared.volumes.read() else {
        return Vec::new();
    };

    // 各卷先各自取 top-VERIFY_CAP 的**轻量候选**（无路径回溯、无 String 分配）。
    //
    // 取各卷 top-N 再合并，与「全局 top-N」等价：全局前 N 名必然分布在各卷的前 N 名之内。
    // 而候选是 16 字节 POD，跨卷搬运几千条的成本可以忽略。
    //
    // **各卷并行扫**：这是纯 CPU 的顺序内存扫描，各卷之间毫无共享状态，串行就是白等
    // ——2026-08-17 实测串行版单次查询 184-216 ms（服务端自计时，四盘 733 万条），
    // 其中最大的 C 盘占掉一半多。用 scope 而不是 rayon：只 fork 四条短命线程，
    // 不值得为它引入一个线程池依赖。
    let needle_ref: &str = &needle;
    let mut merged: Vec<(usize, super::index::Candidate)> = std::thread::scope(|scope| {
        let handles: Vec<_> = volumes
            .iter()
            .enumerate()
            .map(|(vi, vol)| {
                scope.spawn(move || {
                    vol.search_candidates(needle_ref, VERIFY_CAP)
                        .into_iter()
                        .map(|c| (vi, c))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            // 某个卷的搜索线程 panic 不该让整次查询失败：丢掉那个卷的结果继续
            .filter_map(|h| h.join().ok())
            .flatten()
            .collect()
    });
    // Reverse 即「分数高的在前」；与 `|a, b| b.score.cmp(&a.score)` 等价，但 clippy 更认这种写法
    merged.sort_unstable_by_key(|(_, c)| std::cmp::Reverse(c.score));

    // 只有排在前面、真要返回的条目才付路径回溯 + String 分配的代价（见 index.rs::materialize）。
    // 同时做存在性校验：索引里可能有已删除/已改名的陈旧条目（见本模块文档），
    // 校验上限 VERIFY_CAP 条，避免为一次搜索 stat 上万个路径。
    //
    // 逐个物化 + stat，`scored` 始终保持按最终分降序、长度不超过 limit。
    //
    // # 这里的剪枝为什么不改变结果
    //
    // `merged` 已按初始分降序。新鲜度加分有确定的上界 [`MAX_FRESHNESS_BONUS`]，
    // 所以一旦「当前候选的初始分 + 上界」都够不到已选出的第 limit 名，
    // 后面所有候选（初始分只会更低）就更够不到——可以立即收工。
    //
    // 不做这个剪枝就得把 200 条全部 stat：实测那样搜 `itools` 要 66 ms，
    // 剪枝后回到 30 ms 上下，而结果集一模一样。
    let mut scored: Vec<(i64, HitDto)> = Vec::with_capacity(limit.min(VERIFY_CAP));
    for (vi, c) in merged.into_iter().take(VERIFY_CAP) {
        if scored.len() >= limit {
            let cutoff = scored[limit - 1].0;
            if c.score.saturating_add(MAX_FRESHNESS_BONUS) <= cutoff {
                break;
            }
        }
        let hit = volumes[vi].materialize(&c);
        // 顺带拿 Metadata：存在性校验本来就要 stat 一次，修改时间是白送的
        let Ok(meta) = std::fs::symlink_metadata(&hit.path) else {
            continue; // 已删除 / 已改名的陈旧条目
        };
        let final_score = hit.score + freshness_bonus(&meta);
        let dto = HitDto {
            name: hit.name,
            path: hit.path,
            is_dir: hit.is_dir,
        };
        // 插入排序保持有序：limit 是几十，memmove 成本远低于每轮重排
        let at = scored.partition_point(|(s, _)| *s >= final_score);
        scored.insert(at, (final_score, dto));
        scored.truncate(limit);
    }
    scored.into_iter().map(|(_, h)| h).collect()
}

/// [`freshness_bonus`] 可能给出的最大加分。剪枝的正确性依赖它是**真正的上界**
/// ——调那边的档位就必须同步调这里（有测试钉住，见 `freshness_bonus_never_exceeds_bound`）。
const MAX_FRESHNESS_BONUS: i64 = 200;

/// 「最近改过」的加分。
///
/// 为什么放在这里而不是打分函数里：MFT 枚举返回的 `USN_RECORD::TimeStamp` 在
/// `FSCTL_ENUM_USN_DATA` 模式下**没有意义**（那个字段是给日志记录用的），要拿真实的
/// 修改时间只能对具体文件 stat。全盘 733 万条不可能都 stat，所以这个信号只作用于
/// 已经进了 top-`VERIFY_CAP` 的候选——足够把「就是我刚存的那个」从同分堆里拉上来。
///
/// 权重刻意压在匹配质量之下（最高 200，而完全同名是 1000）：用户搜的是名字，
/// 不是「最近文件列表」，新鲜度只该在名字匹配得差不多时起作用。
fn freshness_bonus(meta: &std::fs::Metadata) -> i64 {
    let Ok(modified) = meta.modified() else {
        return 0; // 文件系统不提供修改时间
    };
    let Ok(age) = std::time::SystemTime::now().duration_since(modified) else {
        // 修改时间在未来：时钟漂移或从别处拷来的文件，不给它凭空加分
        return 0;
    };
    freshness_by_age(age.as_secs())
}

/// 按「距今多少秒」给新鲜度加分。单拆出来是为了能直接测边界——
/// 剪枝的正确性依赖它**恒不超过** [`MAX_FRESHNESS_BONUS`]。
fn freshness_by_age(age_secs: u64) -> i64 {
    match age_secs / 86_400 {
        0 => 200,      // 今天
        1..=7 => 140,  // 一周内
        8..=30 => 80,  // 一个月内
        31..=180 => 30,
        _ => 0,
    }
}

/// 全量重建所有盘。各盘并行——它们分布在不同物理磁盘上，串行会白等。
fn build_all(shared: &Arc<Shared>) {
    if shared.rebuilding.swap(true, Ordering::SeqCst) {
        return; // 已有重建在跑
    }
    let started = Instant::now();
    if let Ok(mut s) = shared.status.write() {
        s.state = "building".to_string();
    }

    let drives = fixed_ntfs_drives();
    let excludes: Vec<String> = DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect();
    let mut handles = Vec::new();
    for letter in drives {
        let excludes = excludes.clone();
        handles.push(std::thread::spawn(move || {
            (letter, build_volume(letter, &excludes))
        }));
    }

    let mut volumes = Vec::new();
    let mut ready = Vec::new();
    let mut failed = Vec::new();
    for h in handles {
        match h.join() {
            Ok((letter, Ok(idx))) => {
                ready.push(letter.to_string());
                volumes.push(idx);
            }
            Ok((letter, Err(e))) => failed.push((letter.to_string(), e)),
            // 建索引线程 panic：如实记为该盘失败，不能让整个守护跟着倒
            Err(_) => failed.push(("?".to_string(), "建索引线程异常终止".to_string())),
        }
    }

    let entries: usize = volumes.iter().map(|v| v.searchable_count()).sum();
    let bytes: usize = volumes.iter().map(|v| v.memory_bytes()).sum();
    let dropped: usize = volumes.iter().map(|v| v.dropped()).sum();
    let excluded: usize = volumes.iter().map(|v| v.excluded_count()).sum();

    if let Ok(mut guard) = shared.volumes.write() {
        *guard = volumes;
    }
    if let Ok(mut s) = shared.status.write() {
        s.state = if failed.is_empty() { "ready" } else { "partial" }.to_string();
        s.ready_drives = ready;
        s.failed_drives = failed;
        s.entries = entries;
        s.memory_mb = bytes / (1024 * 1024);
        s.excluded = excluded;
    }
    eprintln!(
        "[mft-daemon] 索引就绪：{entries} 条，{} MB，耗时 {:?}，排除 {excluded} 条，超长丢弃 {dropped} 条",
        bytes / (1024 * 1024),
        started.elapsed()
    );
    shared.rebuilding.store(false, Ordering::SeqCst);
}

/// 建单个卷的索引。两遍枚举，理由见 [`IndexBuilder`] 的文档。
fn build_volume(letter: char, excludes: &[String]) -> Result<VolumeIndex, String> {
    let vol = Volume::open(letter).map_err(|e| describe(&e))?;
    let mut builder = IndexBuilder::new(letter, excludes);

    // 第一遍：只收目录，建出路径骨架并标记排除子树
    vol.enumerate(|entry, name| {
        if entry.is_dir {
            builder.add_dir(&entry, name);
        }
    })
    .map_err(|e| describe(&e))?;
    builder.finish_dirs();

    // 第二遍：收文件，父目录被排除的直接丢
    vol.enumerate(|entry, name| {
        if !entry.is_dir {
            builder.add_file(&entry, name);
        }
    })
    .map_err(|e| describe(&e))?;

    Ok(builder.build())
}

/// 把 Win32 错误翻成用户看得懂的话——这些字符串会直接显示在设置界面上。
fn describe(e: &windows::core::Error) -> String {
    use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_FUNCTION};
    if e.code() == ERROR_ACCESS_DENIED.to_hresult() {
        "拒绝访问（读取磁盘索引需要管理员权限）".to_string()
    } else if e.code() == ERROR_INVALID_FUNCTION.to_hresult() {
        "该卷不支持 MFT 枚举（可能不是 NTFS）".to_string()
    } else {
        format!("{}（0x{:08X}）", e.message(), e.code().0)
    }
}

/// 增量循环：跟 USN 日志把新建/改名的条目追加进索引，并按周期触发全量重建。
fn incremental_loop(shared: &Arc<Shared>) {
    let mut trackers: Vec<Tracker> = fixed_ntfs_drives()
        .into_iter()
        .filter_map(Tracker::new)
        .collect();
    if trackers.is_empty() {
        eprintln!("[mft-daemon] 无可跟踪的 USN 日志，增量更新不可用（仍会按周期全量重建）");
    }
    let mut last_rebuild = Instant::now();
    loop {
        std::thread::sleep(POLL_INTERVAL);
        if last_rebuild.elapsed() >= REBUILD_INTERVAL {
            build_all(shared);
            // 重建后各卷索引对象已换新，USN 起点也要重新对齐，否则会把重建期间
            // 已经进了索引的变更再追加一遍
            for t in trackers.iter_mut() {
                t.resync();
            }
            last_rebuild = Instant::now();
            continue;
        }
        if shared.rebuilding.load(Ordering::SeqCst) {
            continue; // 重建中，别往即将被替换的索引里写
        }
        for t in trackers.iter_mut() {
            t.poll(shared);
        }
    }
}

/// 单个卷的 USN 日志跟踪状态。
///
/// 盘符不在这里再存一份——`Volume` 自己就记着它（`Volume::letter()`），
/// 两处各存一个 char 早晚会出现「一处改了另一处没改」的不一致。
struct Tracker {
    volume: Volume,
    journal_id: u64,
    next_usn: i64,
}

impl Tracker {
    fn new(letter: char) -> Option<Self> {
        let volume = Volume::open(letter).ok()?;
        let journal = volume.query_journal().ok()??;
        Some(Self {
            volume,
            journal_id: journal.UsnJournalID,
            next_usn: journal.NextUsn,
        })
    }

    /// 把 USN 起点对齐到「现在」，丢弃期间累积的变更
    fn resync(&mut self) {
        if let Ok(Some(j)) = self.volume.query_journal() {
            self.journal_id = j.UsnJournalID;
            self.next_usn = j.NextUsn;
        }
    }

    fn poll(&mut self, shared: &Arc<Shared>) {
        use windows::Win32::System::Ioctl::{USN_REASON_FILE_CREATE, USN_REASON_RENAME_NEW_NAME};

        let mut additions: Vec<(super::volume::RawEntry, Vec<u16>)> = Vec::new();
        let result = self.volume.read_changes(self.journal_id, self.next_usn, |change, name| {
            // 只收「出现了一个新名字」的两类变更；删除与旧名不处理，
            // 由返回结果前的存在性校验兜住（见模块文档）
            if change.reason & (USN_REASON_FILE_CREATE | USN_REASON_RENAME_NEW_NAME) != 0 {
                additions.push((change.entry, name.to_vec()));
            }
        });
        match result {
            Ok(next) => self.next_usn = next,
            Err(e) => {
                // 日志被重建（ID 变了）等情况：重新对齐，下一轮继续
                eprintln!(
                    "[mft-daemon] {}: 读 USN 日志失败 {e}，重新对齐",
                    self.volume.letter()
                );
                self.resync();
                return;
            }
        }
        if additions.is_empty() {
            return;
        }
        let count = additions.len();
        let letter = self.volume.letter();
        if let Ok(mut volumes) = shared.volumes.write() {
            if let Some(vol) = volumes.iter_mut().find(|v| v.letter() == letter) {
                for (entry, name) in additions {
                    vol.append(&entry, &name);
                }
            }
        }
        eprintln!("[mft-daemon] {letter}: 增量追加 {count} 条");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个「守护刚起来、索引还没建好」的状态
    fn fresh_shared() -> Arc<Shared> {
        Arc::new(Shared {
            volumes: RwLock::new(Vec::new()),
            status: RwLock::new(StatusDto {
                state: "building".to_string(),
                ..Default::default()
            }),
            rebuilding: AtomicBool::new(false),
            last_query_us: AtomicU64::new(0),
        })
    }

    /// 首次建索引期间 Query 必须报「后端不可用」，让主进程降级到 Windows Search。
    ///
    /// 回空命中列表会被主进程当成「索引可用但确实没匹配」而**停止降级**
    /// （见 `search/mod.rs::pick_file_backend`），于是用户在建索引的那几十秒里
    /// 搜什么都是「未找到」——而降级后端本来还能给他一些结果。
    #[test]
    fn query_reports_unavailable_while_index_empty() {
        let shared = fresh_shared();
        match handle(
            &shared,
            Request::Query {
                needle: "x".to_string(),
                limit: 10,
            },
        ) {
            Response::Error(msg) => {
                assert!(msg.contains("尚未就绪"), "错误信息应说明原因，实得 {msg:?}");
            }
            Response::Hits(h) => panic!(
                "索引为空却回了 {} 条 Hits——主进程会据此停止降级，用户将什么都搜不到",
                h.len()
            ),
            _ => panic!("意外的响应类型"),
        }
    }

    /// Status 必须如实回内部状态，不能因为「还在建」就假装 ready
    #[test]
    fn status_is_reported_honestly() {
        let shared = fresh_shared();
        match handle(&shared, Request::Status) {
            Response::Status(s) => {
                assert_eq!(s.state, "building");
                assert_eq!(s.entries, 0, "没建好就不该报条目数");
                assert!(s.ready_drives.is_empty());
            }
            _ => panic!("Status 请求应回 Status 响应"),
        }
    }

    /// `MAX_FRESHNESS_BONUS` 必须是 `freshness_by_age` 的**真上界**。
    ///
    /// 这不是形式主义：`query` 用它做剪枝提前收工，一旦某个档位的加分超过这个常量，
    /// 就会有本该入选的条目被跳过——而且症状是「偶尔少一条结果」，极难察觉。
    /// 调档位就必须调这里。
    #[test]
    fn freshness_bonus_never_exceeds_bound() {
        let day = 86_400u64;
        // 覆盖每个档位的两端与跨界点，外加若干远期值
        let probes = [
            0, 1, day - 1, day, day + 1, 7 * day, 7 * day + 1, 8 * day, 30 * day,
            30 * day + 1, 31 * day, 180 * day, 180 * day + 1, 365 * day, 10_000 * day,
            u64::MAX,
        ];
        for secs in probes {
            let b = freshness_by_age(secs);
            assert!(
                (0..=MAX_FRESHNESS_BONUS).contains(&b),
                "age={secs}s 的加分 {b} 越出 [0, {MAX_FRESHNESS_BONUS}]"
            );
        }
        // 单调不增：越久远不该比越新的加分还高
        let mut prev = i64::MAX;
        for d in [0u64, 1, 3, 7, 8, 20, 30, 31, 90, 180, 181, 400] {
            let b = freshness_by_age(d * day);
            assert!(b <= prev, "第 {d} 天的加分 {b} 高于更新的 {prev}");
            prev = b;
        }
        assert_eq!(freshness_by_age(0), MAX_FRESHNESS_BONUS, "上界应当可达");
    }

    /// Win32 错误要翻成用户看得懂的话——这些字符串会直接显示在界面上
    #[test]
    fn access_denied_is_explained_in_plain_words() {
        use windows::Win32::Foundation::ERROR_ACCESS_DENIED;
        let msg = describe(&windows::core::Error::from_hresult(
            ERROR_ACCESS_DENIED.to_hresult(),
        ));
        println!("ACCESS_DENIED → {msg}");
        assert!(
            msg.contains("管理员"),
            "应告诉用户这是权限问题、需要管理员，实得 {msg:?}"
        );
    }
}

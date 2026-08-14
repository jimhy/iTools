//! 提审插件包：解压、安全校验、清单解析、内容哈希。
//!
//! # 为什么服务端要把客户端那套校验再做一遍
//!
//! 提审包是**用户上传的任意 zip**，服务端在把它落到磁盘、喂给大模型、最终发布到市场之前，
//! 是这条链路上第一个碰到它的地方。客户端安装时的校验（`src-tauri/src/plugin/install.rs`）
//! 保护的是终端用户的机器；这里的校验保护的是**服务器自己**，以及所有后来从市场下载这个包的人。
//!
//! 两侧规则必须一致，否则会出现最糟的一种分叉：**服务端收下并发布了一个客户端装不上的包**——
//! 作者看到「已上线」，用户点安装却永远失败，而两边都说自己没错。所以这里的
//! [`DENIED_EXTS`]、路径规则、体积上限逐条对齐客户端，改一侧必须改另一侧。
//!
//! # 内容哈希
//!
//! 算法与 `src-tauri/src/plugin/market.rs::content_hash` 和 `scripts/registry/hash.mjs`
//! **逐字一致**（三处实现同一套口径）：
//!
//! ```text
//! 对插件目录下每个【文件】（目录不算，符号链接在解 zip 时已拒收）：
//!     rel  = 相对插件目录的路径，分隔符固定 '/'
//!     line = rel + "\t" + hex(sha256(文件字节))
//! 所有 line 按 UTF-8 字节序升序排序
//! contentHash = "sha256:" + hex(sha256(utf8(lines.join("\n"))))
//! ```
//!
//! 排序用字节序而非语言默认排序：Rust 的 `String` 排序天然是 UTF-8 字节序，JS 默认按 UTF-16
//! 比较——纯 ASCII 路径下两者一致，插件里只要有一个中文文件名就会分道扬镳，
//! 导致该插件在客户端**永远校验失败**。

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 上传包（zip）体积上限。与客户端 `MAX_ARCHIVE_BYTES` 一致。
pub const MAX_ARCHIVE_BYTES: u64 = 32 * 1024 * 1024;
/// 解压后总体积上限（防 zip 炸弹）。与客户端 `MAX_TOTAL_BYTES` 一致。
pub const MAX_TOTAL_BYTES: u64 = 32 * 1024 * 1024;
/// 单文件解压体积上限。与客户端 `MAX_FILE_BYTES` 一致。
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// 归档条目数上限。与客户端 `MAX_ENTRIES` 一致。
pub const MAX_ENTRIES: usize = 4000;

/// 拒收的可执行/脚本扩展名（23 个）。**与客户端 `install.rs::DENIED_EXTS` 逐项一致**。
///
/// 收录标准是「资源管理器里双击即执行 / 即改系统状态」。插件是纯前端页面，
/// 没有任何理由携带 PE 或脚本载荷。
pub const DENIED_EXTS: &[&str] = &[
    "exe", "dll", "bat", "cmd", "com", "scr", "ps1", "psm1", "msi", "vbs", "vbe", "wsf", "jar",
    "sys", "drv", "cpl", "lnk", "url", "hta", "reg", "scf", "pif", "msc",
];

/// 已知的高危能力名（与客户端 `plugin.json` 的 `permissions` 取值域一致）。
pub const KNOWN_PERMISSIONS: &[&str] =
    &["runCommand", "network", "screen-capture", "audio-capture", "hotkey"];

// ==================== 清单 ====================

/// 提审包里的 `plugin.json`（只取服务端与市场条目需要的字段，未知字段忽略）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default, alias = "displayName")]
    pub display_name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub features: Vec<Feature>,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub license: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Feature {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub explain: String,
    #[serde(default)]
    pub cmds: Vec<serde_json::Value>,
}

impl Manifest {
    /// 从 `cmds` 里摘出**裸字符串关键字**（对象形态的 regex / text 不是关键字）。
    /// 市场条目用它给用户展示「敲什么能唤起」。
    pub fn keywords(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for f in &self.features {
            for c in &f.cmds {
                if let Some(s) = c.as_str() {
                    let s = s.trim();
                    if !s.is_empty() && !out.iter().any(|x| x == s) {
                        out.push(s.to_string());
                    }
                }
            }
        }
        out
    }
}

/// `name` 的硬约束（与客户端 `install.rs::is_valid_plugin_name` 一致）：
/// 小写字母数字开头，可含 `.` `_` `-`，最长 64。
pub fn is_valid_plugin_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 || name == "." || name == ".." {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap_or(' ');
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-')
}

/// 版本号比较：按 `.` 分段数值比较，缺失段补 0，解析不出数字的段按 0。
/// 与客户端 `install.rs::version_gt` 同一口径（预发布后缀不被识别，见分发规范第五节）。
pub fn version_gt(a: &str, b: &str) -> bool {
    let seg = |s: &str| -> Vec<u64> {
        s.trim()
            .trim_start_matches(['v', 'V'])
            .split('.')
            .map(|p| p.trim().parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (x, y) = (seg(a), seg(b));
    for i in 0..x.len().max(y.len()) {
        let (l, r) = (x.get(i).copied().unwrap_or(0), y.get(i).copied().unwrap_or(0));
        if l != r {
            return l > r;
        }
    }
    false
}

// ==================== 解压（安全） ====================

/// 校验并规范化一个 zip 条目名，返回可安全 join 的相对路径。
///
/// Zip Slip 第一道闸：拒绝绝对路径、盘符、`..` 段、NUL，并把反斜杠一并当分隔符处理。
/// 结尾的点与空格同样拒绝——Windows 落盘会静默剥掉它们，使「校验时看到的名字」与
/// 「磁盘上真实的名字」不一致，可被用来绕过 [`DENIED_EXTS`]。服务端跑在 Linux 上也照拒，
/// 因为这个包最终要发给 Windows 客户端解压。
pub fn safe_entry_path(name: &str) -> Result<PathBuf, String> {
    if name.is_empty() {
        return Err("归档中存在空路径条目".to_string());
    }
    if name.contains('\0') {
        return Err(format!("归档条目路径非法：{name}"));
    }
    let unified = name.replace('\\', "/");
    if unified.starts_with('/') {
        return Err(format!("归档含绝对路径条目（不安全）：{name}"));
    }
    let mut out = PathBuf::new();
    let mut count = 0usize;
    for seg in unified.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg.chars().all(|c| c == '.') {
            return Err(format!("归档条目含上跳路径（..）：{name}"));
        }
        let b = seg.as_bytes();
        if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
            return Err(format!("归档条目含盘符（不安全）：{name}"));
        }
        if seg.ends_with(' ') || seg.ends_with('.') {
            return Err(format!("归档条目名以空格或点结尾（不安全）：{name}"));
        }
        out.push(seg);
        count += 1;
    }
    if count == 0 {
        return Err(format!("归档条目路径为空：{name}"));
    }
    Ok(out)
}

/// 命中拒收扩展名则返回该扩展名。
///
/// 不用 `Path::extension()`：对 `"payload.exe."` 它返回 `Some("")`，永远不命中清单，
/// 而 Windows 落盘时会把结尾点剥掉，磁盘上就躺着一个真正的 `payload.exe`。
/// 这里按 Windows 规则先剥掉结尾的点与空格，再取最后一个 `.` 之后的部分比对。
pub fn denied_ext(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?.trim_end_matches(['.', ' ']);
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    DENIED_EXTS.iter().copied().find(|d| *d == ext)
}

/// 把 zip 字节流安全解压到 `dest`（`dest` 必须已存在且为规范化路径）。
pub fn extract_zip(bytes: &[u8], dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("创建解压目录失败: {e}"))?;
    let canon_dest = dest.canonicalize().map_err(|e| format!("解析解压目录失败: {e}"))?;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("上传的不是有效的 zip 包: {e}"))?;
    if zip.len() > MAX_ENTRIES {
        return Err(format!("归档条目数 {} 超过上限 {MAX_ENTRIES}，已中止", zip.len()));
    }
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).map_err(|e| format!("读取归档条目失败: {e}"))?;
        let raw_name = f.name().to_string();
        // 符号链接条目：解出来在 Windows 上可能变成指向任意位置的重解析点，一律拒绝
        if let Some(mode) = f.unix_mode() {
            if mode & 0xF000 == 0xA000 {
                return Err(format!("归档含符号链接条目（不安全）：{raw_name}"));
            }
        }
        let rel = safe_entry_path(&raw_name)?;
        let out = canon_dest.join(&rel);
        if f.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| format!("创建目录失败 {}: {e}", out.display()))?;
            continue;
        }
        if let Some(ext) = denied_ext(&rel) {
            return Err(format!(
                "包内含可执行/脚本文件（.{ext}）：{raw_name}。插件应为纯前端资源，已整包拒绝"
            ));
        }
        if f.size() > MAX_FILE_BYTES {
            return Err(format!(
                "文件 {raw_name} 声明大小超过单文件上限 {} MB，已中止",
                MAX_FILE_BYTES / 1024 / 1024
            ));
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
            // 落点复核（Zip Slip 第二道闸）：父目录 canonicalize 后必须仍在解压根内
            let cp = parent.canonicalize().map_err(|e| format!("解析落点失败: {e}"))?;
            if !cp.starts_with(&canon_dest) {
                return Err(format!("归档条目落点越界（不安全）：{raw_name}"));
            }
        }
        let mut w = std::fs::File::create(&out)
            .map_err(|e| format!("写入文件失败 {}: {e}", out.display()))?;
        let written = std::io::copy(&mut f.by_ref().take(MAX_FILE_BYTES + 1), &mut w)
            .map_err(|e| format!("写入文件失败 {}: {e}", out.display()))?;
        if written > MAX_FILE_BYTES {
            return Err(format!(
                "文件 {raw_name} 实际大小超过单文件上限 {} MB，已中止",
                MAX_FILE_BYTES / 1024 / 1024
            ));
        }
        total += written;
        if total > MAX_TOTAL_BYTES {
            return Err(format!(
                "解压总体积超过上限 {} MB，已中止（疑似 zip 炸弹）",
                MAX_TOTAL_BYTES / 1024 / 1024
            ));
        }
    }
    Ok(())
}

/// 若目录内容恰好只有**一个顶层目录**，返回插件根为它；否则返回目录本身。
///
/// 客户端打包时直接压插件目录的**内容**（`plugin.json` 在 zip 根），但作者手工用资源管理器
/// 「压缩到 zip」会多套一层目录名。两种都收，避免一个纯操作差异变成「提审总是失败」。
fn strip_single_root(dir: &Path) -> Result<PathBuf, String> {
    let mut entries = Vec::new();
    for e in std::fs::read_dir(dir).map_err(|e| format!("读取解压结果失败: {e}"))? {
        let e = e.map_err(|e| format!("读取解压结果失败: {e}"))?;
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        entries.push((e.file_name().to_string_lossy().into_owned(), is_dir));
    }
    Ok(match entries.as_slice() {
        [(name, true)] => dir.join(name),
        _ => dir.to_path_buf(),
    })
}

// ==================== 内容哈希 ====================

/// 计算目录的内容哈希，返回形如 `sha256:9f86d0…`。跳过 `.git`（与客户端一致）。
pub fn content_hash(dir: &Path) -> Result<String, String> {
    let mut lines: Vec<String> = Vec::new();
    for entry in walkdir::WalkDir::new(dir).into_iter().filter_entry(|e| e.file_name() != ".git") {
        let entry = entry.map_err(|e| format!("遍历插件目录失败: {e}"))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(dir)
            .map_err(|e| format!("计算相对路径失败: {e}"))?
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = std::fs::read(entry.path()).map_err(|e| format!("读 {rel} 失败: {e}"))?;
        lines.push(format!("{rel}\t{}", hex::encode(Sha256::digest(&bytes))));
    }
    lines.sort();
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(lines.join("\n").as_bytes()))))
}

// ==================== 校验结果 ====================

/// 一个通过机械校验的提审包。
#[derive(Debug, Clone)]
pub struct StagedPackage {
    pub manifest: Manifest,
    pub content_hash: String,
    pub file_count: usize,
    pub total_bytes: u64,
    /// 供大模型审阅的源码（相对路径 → 文本内容）。二进制文件不在其中。
    pub sources: BTreeMap<String, String>,
    /// README 原文（截断后），用于市场展示。
    pub readme: String,
}

/// 可作为「源码」交给大模型审阅的扩展名。二进制（图片/字体）没有审阅价值，
/// 塞进 prompt 只会挤掉真正该看的代码。
const TEXT_EXTS: &[&str] = &[
    "html", "htm", "js", "mjs", "cjs", "jsx", "ts", "tsx", "css", "json", "txt", "md", "svg", "xml",
    "yml", "yaml",
];

/// 单个源码文件交给模型的字符上限（超出截断并标注）。
const MAX_SOURCE_CHARS: usize = 60_000;
/// 全部源码合计的字符上限：超出后不再收录更多文件，并在 prompt 里如实说明「有文件未被审阅」。
const MAX_SOURCES_TOTAL_CHARS: usize = 400_000;
/// README 展示上限（与客户端 `MAX_README_CHARS` 一致）。
const MAX_README_CHARS: usize = 20_000;

/// 解压 + 逐条机械校验 + 收集源码。**通过它才有资格进入大模型审核**。
///
/// `dest` 是本次提审的解压目录（调用方保证是全新的空目录）。
pub fn stage(bytes: &[u8], dest: &Path) -> Result<StagedPackage, String> {
    if bytes.len() as u64 > MAX_ARCHIVE_BYTES {
        return Err(format!(
            "上传包体积超过上限 {} MB",
            MAX_ARCHIVE_BYTES / 1024 / 1024
        ));
    }
    extract_zip(bytes, dest)?;
    let root = strip_single_root(dest)?;

    let manifest_path = root.join("plugin.json");
    if !manifest_path.is_file() {
        return Err("包里没有 plugin.json（应位于压缩包根目录，或其唯一顶层目录下）".to_string());
    }
    if !root.join("index.html").is_file() {
        return Err("包里没有 index.html —— 插件没有入口页面，装上也打不开".to_string());
    }
    let manifest_raw = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("读取 plugin.json 失败: {e}"))?;
    let manifest: Manifest = serde_json::from_str(&manifest_raw)
        .map_err(|e| format!("plugin.json 解析失败: {e}（JSON 不支持注释与尾逗号）"))?;

    // 清单硬约束：这几条不满足，客户端根本装不上，收下来发布就是发一个装不上的包
    if !is_valid_plugin_name(&manifest.name) {
        return Err(format!(
            "plugin.json 的 name「{}」不合法：须以小写字母或数字开头，只含小写字母、数字与 . _ -，最长 64",
            manifest.name
        ));
    }
    if manifest.version.trim().is_empty() {
        return Err("plugin.json 缺少 version —— 没有版本号就无法做更新检查".to_string());
    }
    if manifest.features.is_empty() {
        return Err("plugin.json 的 features 为空 —— 插件不会被任何输入命中，装上也用不了".to_string());
    }
    let mut codes: Vec<&str> = Vec::new();
    for (i, f) in manifest.features.iter().enumerate() {
        let code = f.code.trim();
        if code.is_empty() {
            return Err(format!("features[{i}].code 为空"));
        }
        if codes.contains(&code) {
            return Err(format!("features[{i}].code「{code}」与前面的 feature 重复"));
        }
        codes.push(code);
    }
    for (i, p) in manifest.permissions.iter().enumerate() {
        if !KNOWN_PERMISSIONS.contains(&p.as_str()) {
            return Err(format!(
                "permissions[{i}]「{p}」不是已知能力，可用的只有：{}",
                KNOWN_PERMISSIONS.join(" / ")
            ));
        }
    }

    let content_hash = content_hash(&root)?;
    let (file_count, total_bytes, sources) = collect(&root)?;
    let readme = read_readme(&root);

    Ok(StagedPackage {
        manifest,
        content_hash,
        file_count,
        total_bytes,
        sources,
        readme,
    })
}

/// 统计文件数/总字节，并收集文本源码（供大模型审阅）。
fn collect(root: &Path) -> Result<(usize, u64, BTreeMap<String, String>), String> {
    let mut count = 0usize;
    let mut bytes = 0u64;
    let mut sources: BTreeMap<String, String> = BTreeMap::new();
    let mut budget = MAX_SOURCES_TOTAL_CHARS;

    // 先收集路径并排序，保证「预算用完时被丢下的是哪些文件」是确定的，
    // 而不是随文件系统遍历顺序变化——否则同一个包两次提审可能审到不同的内容。
    let mut files: Vec<PathBuf> = Vec::new();
    for e in walkdir::WalkDir::new(root).into_iter().filter_entry(|e| e.file_name() != ".git") {
        let e = e.map_err(|e| format!("遍历插件目录失败: {e}"))?;
        if e.file_type().is_file() {
            files.push(e.path().to_path_buf());
        }
    }
    files.sort();

    for path in files {
        let meta = std::fs::metadata(&path).map_err(|e| format!("读取文件信息失败: {e}"))?;
        count += 1;
        bytes += meta.len();
        let rel = path
            .strip_prefix(root)
            .map_err(|e| format!("计算相对路径失败: {e}"))?
            .to_string_lossy()
            .replace('\\', "/");
        let is_text = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| TEXT_EXTS.contains(&e.to_ascii_lowercase().as_str()))
            .unwrap_or(false);
        if !is_text || budget == 0 {
            continue;
        }
        // 非 UTF-8 的「文本」文件读不出来就跳过：给模型看一堆替换字符没有意义
        let Ok(mut text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if text.chars().count() > MAX_SOURCE_CHARS {
            text = truncate_chars(&text, MAX_SOURCE_CHARS);
            text.push_str("\n…（本文件过长，此处截断，未展示的部分未被审阅）");
        }
        let take = text.chars().count().min(budget);
        if take < text.chars().count() {
            text = truncate_chars(&text, take);
            text.push_str("\n…（审阅预算用尽，此处截断）");
        }
        budget -= take;
        sources.insert(rel, text);
    }
    Ok((count, bytes, sources))
}

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn read_readme(root: &Path) -> String {
    for name in ["README.md", "readme.md", "README", "README.txt"] {
        let p = root.join(name);
        if p.is_file() {
            if let Ok(s) = std::fs::read_to_string(&p) {
                return truncate_chars(&s, MAX_README_CHARS);
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 造一个内存 zip：`entries` 是 (路径, 内容)。
    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<'_, ()> =
                zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            for (name, content) in entries {
                w.start_file(*name, opts).unwrap();
                w.write_all(content).unwrap();
            }
            w.finish().unwrap();
        }
        buf
    }

    fn good_manifest() -> &'static [u8] {
        br#"{"name":"demo","version":"1.0.0","description":"d","features":[{"code":"main","cmds":["demo"]}]}"#
    }

    #[test]
    fn content_hash_matches_client_golden() {
        // 与客户端 market.rs / scripts/registry/hash.mjs 的基准用例同一组输入输出。
        // 单文件 a.txt 内容 "hello"：
        //   sha256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        //   line = "a.txt\t2cf24d…"
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let line = format!("a.txt\t{}", hex::encode(Sha256::digest(b"hello")));
        let want = format!("sha256:{}", hex::encode(Sha256::digest(line.as_bytes())));
        assert_eq!(content_hash(dir.path()).unwrap(), want);
    }

    #[test]
    fn content_hash_sorts_by_utf8_bytes() {
        // 中文文件名下 UTF-8 与 UTF-16 排序会分叉，这里钉死用的是字节序。
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("中文.txt"), b"x").unwrap();
        std::fs::write(dir.path().join("a.txt"), b"y").unwrap();
        let mut lines = [
            format!("a.txt\t{}", hex::encode(Sha256::digest(b"y"))),
            format!("中文.txt\t{}", hex::encode(Sha256::digest(b"x"))),
        ];
        lines.sort();
        let want = format!("sha256:{}", hex::encode(Sha256::digest(lines.join("\n").as_bytes())));
        assert_eq!(content_hash(dir.path()).unwrap(), want);
    }

    #[test]
    fn rejects_executables() {
        let zip = make_zip(&[
            ("plugin.json", good_manifest()),
            ("index.html", b"<html></html>"),
            ("tool.exe", b"MZ"),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let err = stage(&zip, dir.path()).unwrap_err();
        assert!(err.contains(".exe"), "{err}");
    }

    #[test]
    fn rejects_trailing_dot_bypass() {
        // "payload.exe." 在 Windows 落地会变成 payload.exe —— 必须在校验期就拒
        let zip = make_zip(&[("payload.exe.", b"MZ")]);
        let dir = tempfile::tempdir().unwrap();
        assert!(stage(&zip, dir.path()).is_err());
    }

    #[test]
    fn rejects_zip_slip() {
        let zip = make_zip(&[("../evil.js", b"x")]);
        let dir = tempfile::tempdir().unwrap();
        let err = stage(&zip, dir.path()).unwrap_err();
        assert!(err.contains("上跳路径"), "{err}");
    }

    #[test]
    fn rejects_missing_index_html() {
        let zip = make_zip(&[("plugin.json", good_manifest())]);
        let dir = tempfile::tempdir().unwrap();
        let err = stage(&zip, dir.path()).unwrap_err();
        assert!(err.contains("index.html"), "{err}");
    }

    #[test]
    fn rejects_unknown_permission() {
        let zip = make_zip(&[
            (
                "plugin.json",
                br#"{"name":"demo","version":"1.0.0","permissions":["netwrok"],"features":[{"code":"m","cmds":["d"]}]}"#,
            ),
            ("index.html", b"<html></html>"),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let err = stage(&zip, dir.path()).unwrap_err();
        assert!(err.contains("netwrok"), "{err}");
    }

    #[test]
    fn rejects_duplicate_feature_code() {
        let zip = make_zip(&[
            (
                "plugin.json",
                br#"{"name":"demo","version":"1.0.0","features":[{"code":"m","cmds":["a"]},{"code":"m","cmds":["b"]}]}"#,
            ),
            ("index.html", b"<html></html>"),
        ]);
        let dir = tempfile::tempdir().unwrap();
        assert!(stage(&zip, dir.path()).unwrap_err().contains("重复"));
    }

    #[test]
    fn accepts_wrapped_single_root() {
        // 作者用资源管理器压缩会多套一层目录，这种也要收
        let zip = make_zip(&[
            ("demo/plugin.json", good_manifest()),
            ("demo/index.html", b"<html></html>"),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let pkg = stage(&zip, dir.path()).unwrap();
        assert_eq!(pkg.manifest.name, "demo");
        assert_eq!(pkg.file_count, 2);
    }

    #[test]
    fn collects_sources_for_review() {
        let zip = make_zip(&[
            ("plugin.json", good_manifest()),
            ("index.html", b"<html><script src=x.js></script></html>"),
            ("x.js", b"console.log(1)"),
            ("logo.png", b"\x89PNG\r\n"),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let pkg = stage(&zip, dir.path()).unwrap();
        assert!(pkg.sources.contains_key("x.js"));
        assert!(pkg.sources.contains_key("index.html"));
        // 二进制不进 prompt
        assert!(!pkg.sources.contains_key("logo.png"));
        assert_eq!(pkg.file_count, 4);
    }

    #[test]
    fn version_compare_matches_client() {
        assert!(version_gt("1.2.1", "1.2.0"));
        assert!(version_gt("1.2", "1.1.9"));
        assert!(!version_gt("1.2.0", "1.2"));
        assert!(!version_gt("1.1.9", "1.2.0"));
        // 预发布后缀不被识别（与分发规范第五节写明的行为一致）
        assert!(!version_gt("1.2.0-beta", "1.2.0"));
    }

    #[test]
    fn plugin_name_rules() {
        assert!(is_valid_plugin_name("base64"));
        assert!(is_valid_plugin_name("my.plugin_x-1"));
        assert!(!is_valid_plugin_name("Base64"));
        assert!(!is_valid_plugin_name("-lead"));
        assert!(!is_valid_plugin_name(".."));
        assert!(!is_valid_plugin_name(""));
    }
}

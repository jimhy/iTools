use std::path::PathBuf;

use walkdir::WalkDir;

use super::SearchItem;

/// 索引条目数量上限，防止大目录把内存撑爆
const MAX_ENTRIES: usize = 20_000;
/// 从每个根目录向下递归的最大深度
const MAX_DEPTH: usize = 4;

/// 一个可打开的文件/文件夹条目
#[derive(Clone)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

impl FileEntry {
    pub fn to_item(&self) -> SearchItem {
        let target = self.path.to_string_lossy().into_owned();
        SearchItem {
            id: target.clone(),
            title: self.name.clone(),
            subtitle: target.clone(),
            kind: if self.is_dir {
                "folder".to_string()
            } else {
                "file".to_string()
            },
            target,
            icon: None,
            action: "open".to_string(),
        }
    }
}

/// 扫描用户常用目录（桌面/文档/下载/图片）建立文件索引
pub fn scan_files() -> Vec<FileEntry> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        for sub in ["Desktop", "Documents", "Downloads", "Pictures"] {
            let dir = home.join(sub);
            if dir.is_dir() {
                roots.push(dir);
            }
        }
    }

    let mut out: Vec<FileEntry> = Vec::new();
    'outer: for root in roots {
        for entry in WalkDir::new(&root)
            .max_depth(MAX_DEPTH)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if path == root {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            // 跳过隐藏项（. 开头）
            if name.starts_with('.') {
                continue;
            }
            out.push(FileEntry {
                name: name.to_string(),
                path: path.to_path_buf(),
                is_dir: entry.file_type().is_dir(),
            });
            if out.len() >= MAX_ENTRIES {
                break 'outer;
            }
        }
    }

    out
}

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use pinyin::ToPinyinMulti;
use walkdir::WalkDir;

use super::SearchItem;

/// 多音字展开的变体数量上限（应用名短，2~4 组已覆盖绝大多数；防极端名爆炸）
const MAX_PY_VARIANTS: usize = 8;
/// 噪音条目（卸载/帮助类）的降权分：命中仍可搜到，但排序沉底
const DEMOTED_PENALTY: i64 = 100;

/// 一个可启动的应用条目
#[derive(Clone)]
pub struct AppEntry {
    /// 展示名（本地化显示名优先，如「远程桌面连接」）——主搜索键
    pub name: String,
    /// 文件名/注册键名（多为英文，如 "Remote Desktop Connection"）——辅搜索键
    pub file_stem: String,
    /// 启动目标：.lnk / .exe / .cpl / .msc / ms-settings: URI 等，交给 shell 打开
    pub path: PathBuf,
    /// 拼音搜索键变体：(全拼, 首字母)，由展示名+文件名合并生成
    pub py_keys: Vec<(String, String)>,
    /// 噪音条目（卸载/帮助/说明类）：不过滤，仅降权沉底
    pub demoted: bool,
}

impl AppEntry {
    pub fn new(name: String, file_stem: String, path: PathBuf) -> Self {
        let mut py_keys = pinyin_keys(&name);
        if file_stem != name {
            py_keys.extend(pinyin_keys(&file_stem));
        }
        let demoted = is_demoted(&name) || is_demoted(&file_stem);
        Self {
            name,
            file_stem,
            path,
            py_keys,
            demoted,
        }
    }

    pub fn to_item(&self) -> SearchItem {
        let target = self.path.to_string_lossy().into_owned();
        SearchItem {
            id: target.clone(),
            title: self.name.clone(),
            subtitle: target.clone(),
            kind: "app".to_string(),
            target,
            icon: None,
            action: "open".to_string(),
        }
    }

    /// 展示名/文件名/拼音键取最高模糊匹配分（噪音条目降权）；均不匹配返回 None
    pub fn match_score(&self, matcher: &SkimMatcherV2, query: &str) -> Option<i64> {
        let mut best = matcher.fuzzy_match(&self.name, query);
        if self.file_stem != self.name {
            best = best.max(matcher.fuzzy_match(&self.file_stem, query));
        }
        for (full, initials) in &self.py_keys {
            best = best.max(matcher.fuzzy_match(full, query));
            best = best.max(matcher.fuzzy_match(initials, query));
        }
        best.map(|s| if self.demoted { s - DEMOTED_PENALTY } else { s })
    }
}

/// 应用名 → 拼音搜索键变体（多音字按笛卡尔积展开，封顶 MAX_PY_VARIANTS）。
/// 中文字符取所有去调读音；ASCII 字母/数字小写后同时进两键；其余字符跳过。
/// 全串无汉字返回空（直接用原名匹配即可）。
pub fn pinyin_keys(name: &str) -> Vec<(String, String)> {
    let mut variants: Vec<(String, String)> = vec![(String::new(), String::new())];
    let mut has_chinese = false;

    for c in name.chars() {
        if let Some(multi) = c.to_pinyin_multi() {
            has_chinese = true;
            // 去调后去重的读音集合（如 乐 → ["le", "yue"]）
            let mut readings: Vec<&str> = Vec::new();
            for py in multi {
                let plain = py.plain();
                if !readings.contains(&plain) {
                    readings.push(plain);
                }
            }
            let mut next: Vec<(String, String)> = Vec::with_capacity(variants.len());
            'expand: for (full, initials) in &variants {
                for reading in &readings {
                    let mut f = full.clone();
                    f.push_str(reading);
                    let mut i = initials.clone();
                    if let Some(first) = reading.chars().next() {
                        i.push(first);
                    }
                    next.push((f, i));
                    if next.len() >= MAX_PY_VARIANTS {
                        break 'expand;
                    }
                }
            }
            variants = next;
        } else if c.is_ascii_alphanumeric() {
            let lc = c.to_ascii_lowercase();
            for (full, initials) in variants.iter_mut() {
                full.push(lc);
                initials.push(lc);
            }
        }
        // 其余字符（空格/标点/全角符号）跳过
    }

    if has_chinese {
        variants
    } else {
        Vec::new()
    }
}

/// 汇总四个来源：用户手动添加 + 开始菜单 .lnk + 注册表 App Paths + 内置系统命令。
/// 注意：内部会调用 SHGetFileInfoW 取本地化名，须在已初始化 COM 的线程上调用。
pub fn scan_apps(custom_paths: &[String]) -> Vec<AppEntry> {
    let mut apps: Vec<AppEntry> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    add_custom(custom_paths, &mut apps, &mut seen);
    scan_start_menu(&mut apps, &mut seen);
    scan_app_paths(&mut apps, &mut seen);
    add_system_commands(&mut apps, &mut seen);
    scan_apps_folder(&mut apps, &mut seen);

    apps
}

/// shell:AppsFolder 枚举：补齐 UWP/MSIX 等开始菜单 .lnk 覆盖不到的应用。
/// 放最后：与前面来源按显示名去重，重复项以 .lnk 版本优先（保留其图标/降权逻辑）。
fn scan_apps_folder(apps: &mut Vec<AppEntry>, seen: &mut HashSet<String>) {
    for (name, parse_name) in super::apps_folder::enum_apps_folder() {
        // 跳过指向 .msi 的安装器条目
        if parse_name.to_ascii_lowercase().ends_with(".msi") {
            continue;
        }
        let key = name.to_lowercase();
        if seen.insert(key) {
            let target = format!("shell:AppsFolder\\{parse_name}");
            apps.push(AppEntry::new(name.clone(), name, PathBuf::from(target)));
        }
    }
}

/// 用户在设置里手动添加的程序（exe/lnk），优先级最高（先注册占名）
fn add_custom(paths: &[String], apps: &mut Vec<AppEntry>, seen: &mut HashSet<String>) {
    for p in paths {
        let path = Path::new(p);
        if !path.exists() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let display = localized_display_name(path).unwrap_or_else(|| stem.to_string());
        if seen.insert(display.to_lowercase()) {
            apps.push(AppEntry::new(display, stem.to_string(), path.to_path_buf()));
        }
    }
}

/// 扫描系统级 + 用户级开始菜单下的所有 .lnk（本地化显示名做标题）
fn scan_start_menu(apps: &mut Vec<AppEntry>, seen: &mut HashSet<String>) {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(program_data) = std::env::var("ProgramData") {
        roots.push(PathBuf::from(program_data).join(r"Microsoft\Windows\Start Menu\Programs"));
    }
    if let Ok(app_data) = std::env::var("AppData") {
        roots.push(PathBuf::from(app_data).join(r"Microsoft\Windows\Start Menu\Programs"));
    }

    for root in roots {
        for entry in WalkDir::new(&root).into_iter().filter_map(Result::ok) {
            let path = entry.path();
            if !is_lnk(path) {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // 资源管理器显示的本地化名（如「远程桌面连接」）；取不到退回文件名
            let display = localized_display_name(path).unwrap_or_else(|| stem.to_string());
            let key = display.to_lowercase();
            if seen.insert(key) {
                apps.push(AppEntry::new(display, stem.to_string(), path.to_path_buf()));
            }
        }
    }
}

/// 注册表 App Paths：只在注册表注册、开始菜单没有快捷方式的程序
fn scan_app_paths(apps: &mut Vec<AppEntry>, seen: &mut HashSet<String>) {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    const SUBKEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths";

    for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        let Ok(app_paths) = RegKey::predef(root).open_subkey(SUBKEY) else {
            continue;
        };
        for key_name in app_paths.enum_keys().filter_map(Result::ok) {
            let Ok(sub) = app_paths.open_subkey(&key_name) else {
                continue;
            };
            // 默认值 = 可执行文件完整路径
            let Ok(exe_path) = sub.get_value::<String, _>("") else {
                continue;
            };
            let exe_path = exe_path.trim_matches('"').to_string();
            if exe_path.is_empty() || !Path::new(&exe_path).exists() {
                continue;
            }
            let stem = key_name.trim_end_matches(".exe").trim_end_matches(".EXE");
            let key = stem.to_lowercase();
            if seen.insert(key) {
                apps.push(AppEntry::new(
                    stem.to_string(),
                    stem.to_string(),
                    PathBuf::from(exe_path),
                ));
            }
        }
    }
}

/// 内置系统命令：常用系统功能入口（卸载程序/控制面板/设备管理器等）
fn add_system_commands(apps: &mut Vec<AppEntry>, seen: &mut HashSet<String>) {
    let windir = std::env::var("windir").unwrap_or_else(|_| r"C:\Windows".to_string());
    let sys32 = format!(r"{windir}\System32");

    let commands: Vec<(&str, String)> = vec![
        // ---- 有专属彩色图标的 12 个（前端 SYSTEM_ICONS 按中文名匹配，改名需同步）----
        ("卸载或更新程序", format!(r"{sys32}\appwiz.cpl")),
        ("控制面板", format!(r"{sys32}\control.exe")),
        ("设备管理器", format!(r"{sys32}\devmgmt.msc")),
        ("服务", format!(r"{sys32}\services.msc")),
        ("任务管理器", format!(r"{sys32}\Taskmgr.exe")),
        ("注册表编辑器", format!(r"{windir}\regedit.exe")),
        ("磁盘管理", format!(r"{sys32}\diskmgmt.msc")),
        ("网络连接", format!(r"{sys32}\ncpa.cpl")),
        ("电源选项", format!(r"{sys32}\powercfg.cpl")),
        ("系统属性", format!(r"{sys32}\SystemPropertiesAdvanced.exe")),
        ("系统设置", "ms-settings:".to_string()),
        ("Windows 更新", "ms-settings:windowsupdate".to_string()),
        // ---- 系统管理工具（图标走系统提取/兜底）----
        // 环境变量无单文件入口，走 rundll32（cmd: 前缀由 launch::open_detached 特判执行）
        (
            "环境变量",
            "cmd:rundll32.exe sysdm.cpl,EditEnvironmentVariables".to_string(),
        ),
        ("计算机管理", format!(r"{sys32}\compmgmt.msc")),
        ("事件查看器", format!(r"{sys32}\eventvwr.msc")),
        ("任务计划程序", format!(r"{sys32}\taskschd.msc")),
        ("系统配置", format!(r"{sys32}\msconfig.exe")),
        ("系统信息", format!(r"{sys32}\msinfo32.exe")),
        ("资源监视器", format!(r"{sys32}\resmon.exe")),
        ("性能监视器", format!(r"{sys32}\perfmon.exe")),
        ("磁盘清理", format!(r"{sys32}\cleanmgr.exe")),
        ("磁盘碎片整理", format!(r"{sys32}\dfrgui.exe")),
        ("组策略编辑器", format!(r"{sys32}\gpedit.msc")),
        ("本地安全策略", format!(r"{sys32}\secpol.msc")),
        ("本地用户和组", format!(r"{sys32}\lusrmgr.msc")),
        ("用户账户", format!(r"{sys32}\netplwiz.exe")),
        ("Windows 功能", format!(r"{sys32}\OptionalFeatures.exe")),
        ("防火墙", format!(r"{sys32}\firewall.cpl")),
        ("高级防火墙", format!(r"{sys32}\WF.msc")),
        ("DirectX 诊断工具", format!(r"{sys32}\dxdiag.exe")),
        ("字符映射表", format!(r"{sys32}\charmap.exe")),
        ("远程桌面连接", format!(r"{sys32}\mstsc.exe")),
        // ---- 常用设置 / 控制面板项 ----
        ("显示设置", "ms-settings:display".to_string()),
        ("蓝牙设置", "ms-settings:bluetooth".to_string()),
        ("网络和 Internet", "ms-settings:network".to_string()),
        ("默认应用", "ms-settings:defaultapps".to_string()),
        ("存储设置", "ms-settings:storagesense".to_string()),
        ("声音", format!(r"{sys32}\mmsys.cpl")),
        ("鼠标设置", format!(r"{sys32}\main.cpl")),
        ("Internet 选项", format!(r"{sys32}\inetcpl.cpl")),
        ("日期和时间", format!(r"{sys32}\timedate.cpl")),
        ("区域", format!(r"{sys32}\intl.cpl")),
    ];

    for (name, target) in commands {
        // 本地文件路径必须存在；ms-settings: URI 与 cmd: 命令行直接放行
        let skip_exist_check =
            target.starts_with("ms-settings:") || target.starts_with("cmd:");
        if !skip_exist_check && !Path::new(&target).exists() {
            continue;
        }
        if seen.insert(name.to_lowercase()) {
            apps.push(AppEntry::new(
                name.to_string(),
                name.to_string(),
                PathBuf::from(target),
            ));
        }
    }
}

fn is_lnk(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("lnk"))
}

/// 噪音条目判定（卸载/帮助/说明类）——不过滤，仅降权
fn is_demoted(name: &str) -> bool {
    let lower = name.to_lowercase();
    const NOISE: [&str; 6] = ["uninstall", "卸载", "readme", "help", "帮助", "说明"];
    NOISE.iter().any(|kw| lower.contains(kw))
}

/// 取资源管理器显示的本地化名称（SHGFI_DISPLAYNAME）。
/// 系统自带快捷方式（如 Remote Desktop Connection.lnk）会返回「远程桌面连接」。
#[cfg(windows)]
fn localized_display_name(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_DISPLAYNAME};

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: wide 为 0 结尾宽串；shfi 零初始化输出结构，尺寸如实传入。
    unsafe {
        let mut shfi: SHFILEINFOW = std::mem::zeroed();
        let ok = SHGetFileInfoW(
            wide.as_ptr(),
            0,
            &mut shfi,
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_DISPLAYNAME,
        );
        if ok == 0 {
            return None;
        }
        let name = &shfi.szDisplayName;
        let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        if end == 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&name[..end]))
    }
}

#[cfg(not(windows))]
fn localized_display_name(_path: &Path) -> Option<String> {
    None
}

//! 账号资料：纯本地模拟的「个人中心」数据层（落盘 `%LOCALAPPDATA%\itools\profile.json`）。
//!
//! 不接真实服务器：昵称/头像本地持久化，手机号/微信绑定/数据同步/注销/退出只是还原
//! 拟真的账号交互样式。陪伴天数从首次使用（`first_use_ts`）累计。

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 当前 Unix 时间（秒）。系统时钟异常时回退 0（视为「刚开始使用」）。
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 账号资料。加 `serde(default)`：老配置缺字段时用默认值补齐。
/// 序列化结构与前端 `Profile`（src/types.ts）保持一致。
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// 昵称（空则前端回退系统用户名）
    pub nickname: String,
    /// 头像绝对路径（None = 用默认字母头像）
    pub avatar_path: Option<String>,
    /// 手机号（脱敏展示，模拟）
    pub phone: String,
    /// 首次使用时间戳（秒）；0 表示尚未记录，加载时补当前时间
    pub first_use_ts: u64,
    /// 数据同步开关（模拟）
    pub data_sync_enabled: bool,
    /// 是否已绑定微信（模拟）
    pub wechat_bound: bool,
    /// 是否已登录（退出/注销后置 false）
    pub logged_in: bool,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            nickname: String::new(),
            avatar_path: None,
            phone: "139****1030".to_string(),
            first_use_ts: 0,
            data_sync_enabled: true,
            wechat_bound: false,
            logged_in: true,
        }
    }
}

impl Profile {
    /// 陪伴天数 = (现在 - 首次使用) / 86400，至少 1（首日也显示「陪伴 1 天」）。
    pub fn companion_days(&self) -> u64 {
        let start = if self.first_use_ts == 0 {
            now_secs()
        } else {
            self.first_use_ts
        };
        let now = now_secs();
        let days = now.saturating_sub(start) / 86_400;
        days.max(1)
    }

    /// 重置为游客态：清昵称/头像、解绑微信、置未登录；保留 `first_use_ts`（陪伴天数不清零）。
    fn reset_to_guest(&mut self) {
        self.nickname = String::new();
        self.avatar_path = None;
        self.wechat_bound = false;
        self.logged_in = false;
    }
}

/// 账号资料快照：把 `Profile` 摊平 + 附派生的陪伴天数，前端一次拿全。
/// 序列化结构与前端 `ProfileView`（Profile 全字段 + companion_days）保持一致。
#[derive(Serialize)]
pub struct ProfileView {
    #[serde(flatten)]
    pub profile: Profile,
    pub companion_days: u64,
}

/// 线程安全的账号存储；每次变更立即落盘。
pub struct ProfileStore {
    path: PathBuf,
    data: Mutex<Profile>,
}

impl ProfileStore {
    pub fn load() -> Self {
        let dir = dirs::data_local_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("itools");
        Self::load_from(dir.join("profile.json"))
    }

    fn load_from(path: PathBuf) -> Self {
        let mut data = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Profile>(&s).ok())
            .unwrap_or_default();
        // 首次使用时间戳缺失时补齐（用于陪伴天数计算），并立即落盘固化。
        let mut needs_persist = false;
        if data.first_use_ts == 0 {
            data.first_use_ts = now_secs();
            needs_persist = true;
        }
        let store = Self {
            path,
            data: Mutex::new(data),
        };
        if needs_persist {
            store.persist();
        }
        store
    }

    /// 把当前内存态写盘（容错：目录创建/写入失败均忽略）。
    fn persist(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(guard) = self.data.lock() {
            if let Ok(json) = serde_json::to_string_pretty(&*guard) {
                let _ = std::fs::write(&self.path, json);
            }
        }
    }

    /// 当前资料快照（克隆）。
    pub fn get(&self) -> Profile {
        self.data.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// 当前资料 + 陪伴天数，供前端一次拿全。
    pub fn view(&self) -> ProfileView {
        let profile = self.get();
        let companion_days = profile.companion_days();
        ProfileView {
            profile,
            companion_days,
        }
    }

    /// 就地修改资料并落盘。
    pub fn update<F: FnOnce(&mut Profile)>(&self, f: F) {
        if let Ok(mut guard) = self.data.lock() {
            f(&mut guard);
        }
        self.persist();
    }

    /// 重置为游客态并落盘（退出账号 / 注销共用）。
    pub fn reset_to_guest(&self) {
        self.update(|p| p.reset_to_guest());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_roundtrip_and_reset() {
        let path = std::env::temp_dir().join("itools-test-profile.json");
        let _ = std::fs::remove_file(&path);

        // 首次加载：补 first_use_ts、默认值
        let store = ProfileStore::load_from(path.clone());
        let p = store.get();
        assert_eq!(p.phone, "139****1030");
        assert!(p.data_sync_enabled);
        assert!(p.logged_in);
        assert!(p.first_use_ts > 0, "首次加载应补 first_use_ts");
        assert!(store.view().companion_days >= 1, "陪伴天数至少 1");

        // 改昵称 + 头像并落盘
        store.update(|p| {
            p.nickname = "海风哥".to_string();
            p.avatar_path = Some(r"C:\a.png".to_string());
            p.wechat_bound = true;
        });
        let reloaded = ProfileStore::load_from(path.clone());
        assert_eq!(reloaded.get().nickname, "海风哥");
        let first_use = reloaded.get().first_use_ts;

        // 重置游客：清昵称/头像/微信、未登录，保留 first_use_ts
        reloaded.reset_to_guest();
        let g = reloaded.get();
        assert_eq!(g.nickname, "");
        assert!(g.avatar_path.is_none());
        assert!(!g.wechat_bound);
        assert!(!g.logged_in);
        assert_eq!(g.first_use_ts, first_use, "陪伴天数基准不应清零");

        let _ = std::fs::remove_file(&path);
    }
}

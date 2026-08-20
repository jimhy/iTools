//! 本地数据根目录：全应用**唯一**的 `%LOCALAPPDATA%\itools` 出口。
//!
//! # 为什么要单独抽一个模块
//!
//! 这个根目录原先被 8 处代码各自 `dirs::data_local_dir().join("itools")` 拼一遍
//! （itools.db、头像、插件数据、插件设置值、可写插件根、市场索引缓存、镜像缓存、调试家目录）。
//! 只要有一处改了、另一处漏了，用户的数据就会变成「一半在新目录、一半在旧目录」——
//! 这比不改还糟：两边都不完整，还没人看得出来。所以路径只在这里定义一次，
//! 其它地方一律走 [`data_root`]，**不要**再出现第二处 `join("itools")`。
//!
//! # release 的路径是**冻结**的
//!
//! release 恒为 `%LOCALAPPDATA%\itools`，一个字节都不能动。已装用户的设置、固定项、
//! 使用记录、插件数据（`itools.db` + `plugin-data/` + `plugins/`）全在这个目录下。
//! 改目录名对用户来说不是「换了个位置」，而是**升级完打开发现数据全没了**——
//! 旧目录还躺在磁盘上，但没有任何代码再去读它。任何「顺手把目录名规范化一下」的念头，
//! 到这行为止。
//!
//! # debug 为什么必须分开
//!
//! 单实例守卫按构建类型分锁（见 [`crate::single_instance`]），也就是说 debug 与 release
//! **可以同时在跑**——开发时的常态就是「一边跑 release 正常用，一边编 debug 调」。
//! 两个进程指着同一个 `itools.db` 就是两个进程并发写同一个 SQLite：轻则报
//! `database is locked`（写入丢失、UI 上表现为「保存没反应」），重则「这边改了那边看不到」
//! （各自连接持有的 WAL 快照不一致）。所以 debug 挪到 `%LOCALAPPDATA%\itools-dev`，
//! 两套数据物理隔离，谁也踩不着谁。
//!
//! # 刻意**不**做迁移
//!
//! debug 首次启动看到的是一份干净的空数据，这是**预期行为**，不是 bug（启动日志里会明说，
//! 见 [`log_startup_data_root`]）。这里绝不从 release 目录复制 / 迁移任何东西：
//! 那等于背着用户把他的真实库（含账号、同步数据、插件里存的东西）复制一份出来，
//! 而且复制出来的那份改了也不会回到 release，只会造成「我明明改了怎么没生效」的错觉。
//! 需要拿真实数据调试时，由开发者自己手动复制过去——一个明确的动作，胜过一次自作主张。

use std::path::PathBuf;

use crate::logging::ilog;

/// release 数据根的目录名。**冻结值**：改它 = 已装用户升级后数据凭空消失（见模块头注释）。
const RELEASE_DIR_NAME: &str = "itools";

/// debug 数据根的目录名：与 release 并存时的物理隔离（见模块头注释）。
const DEBUG_DIR_NAME: &str = "itools-dev";

/// 本次构建用的数据根目录名。
///
/// 用 `cfg!`（运行期布尔）而不是 `#[cfg]`：两条分支都参与编译，写错了任一边都会立刻报错，
/// 不会出现「release 编译时才发现 debug 分支根本编不过」这种事。与
/// [`crate::single_instance::mutex_name`] 的写法保持一致。
fn root_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        DEBUG_DIR_NAME
    } else {
        RELEASE_DIR_NAME
    }
}

/// 本地数据根目录。
///
/// - release：`%LOCALAPPDATA%\itools`（**永不改变**，理由见模块头注释）
/// - debug：`%LOCALAPPDATA%\itools-dev`
///
/// 取不到系统本地数据目录时回退到临时目录——这是各调用点改动前就有的行为，原样保留：
/// 数据会随临时目录被系统清理，但至少 app 能起来，不至于因为拿不到路径就崩在启动期。
///
/// 只返回路径，**不建目录**：要不要 `create_dir_all` 由调用方按自己的语义决定
/// （读缓存的那几处根本不该顺手把目录建出来）。
pub fn data_root() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(root_dir_name())
}

/// **两个构建的数据根都算上**（release 的 `itools` 与 debug 的 `itools-dev`）。
///
/// 用途是给插件的文件类能力做拒绝名单：插件数据都存在数据根下的 `plugin-data/`，
/// 放行任何一个数据根都等于打破插件间数据隔离——而 [`data_root`] 只返回**当前构建**的那个，
/// 拿它当黑名单会漏掉另一个（release 版插件仍能读到 debug 版的插件数据，反之亦然）。
/// 同机同时装了两种构建是开发者的常态，这个漏不是理论问题。
///
/// 同样只返回路径，不建目录；取不到本地数据目录时与 [`data_root`] 一样回退到临时目录。
pub fn all_data_roots() -> Vec<PathBuf> {
    let base = dirs::data_local_dir().unwrap_or_else(std::env::temp_dir);
    [RELEASE_DIR_NAME, DEBUG_DIR_NAME]
        .iter()
        .map(|n| base.join(n))
        .collect()
}

/// debug 构建启动时记一行「这次用的是哪个数据目录」。
///
/// 为什么值得占一行日志：debug 换到独立目录后，首次启动看到的是空设置、空固定项、
/// 空插件数据，开发者第一反应必然是「我的数据怎么没了 / 是不是库损坏了」。
/// 把完整路径直接摆在启动日志里，一眼就能确认这是隔离目录而不是数据出事。
///
/// release 不打这条：那边路径从来没变过，打出来只是噪音。
pub fn log_startup_data_root() {
    if cfg!(debug_assertions) {
        ilog!(
            "[iTools] debug 构建使用独立数据目录：{}（与 release 的 %LOCALAPPDATA%\\{} 物理隔离；\
             首次启动数据为空属正常，不会也不该自动迁移 release 的数据）",
            data_root().display(),
            RELEASE_DIR_NAME
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// release 目录名冻结：这条断言就是那道闸门。
    /// 谁把 `RELEASE_DIR_NAME` 改了，测试立刻红——因为那等于让所有已装用户的数据「消失」。
    #[test]
    fn release_root_name_is_frozen() {
        assert_eq!(
            RELEASE_DIR_NAME, "itools",
            "release 数据根目录名不可更改：改了 = 已装用户升级后看不到自己的任何数据"
        );
    }

    /// debug 构建必须落在独立目录，绝不能回落到 release 那个根。
    ///
    /// 加 `#[cfg(debug_assertions)]`：`cargo test --release` 下 debug_assertions 关闭、
    /// `root_dir_name()` 本就该返回 `itools`，那时这条断言不成立也不该跑。
    #[cfg(debug_assertions)]
    #[test]
    fn debug_root_is_isolated_from_release() {
        assert_eq!(root_dir_name(), DEBUG_DIR_NAME);
        let root = data_root();
        assert!(
            root.ends_with(DEBUG_DIR_NAME),
            "debug 数据根应以 {DEBUG_DIR_NAME} 结尾，实际：{}",
            root.display()
        );
        let release_root = dirs::data_local_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(RELEASE_DIR_NAME);
        assert_ne!(
            root,
            release_root,
            "debug 与 release 共用数据根 = 两个进程并发写同一个 itools.db"
        );
    }
}

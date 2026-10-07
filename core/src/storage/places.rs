//! 常用目录（桌面 / 下载 / 文档…）—— 穿梭栏两个栏的"快捷位置"来源。
//!
//! ## 为什么需要它
//!
//! 把它当文件管理器看的话，"常用位置"是绕不开的：真实用户要传的东西
//! 绝大多数就在桌面、下载、文档里。让人每次从 `C:\` 一层层点进去，
//! 等于逼用户做机器本来就知道的事。
//!
//! ## 路径怎么来的：`dirs` crate，**不是** `%USERPROFILE%` 拼字符串
//!
//! 拼字符串会对两类用户**直接失效**，而这两类在真实用户里占比很高：
//!
//! 1. **OneDrive 重定向**。桌面/文档/图片默认可能被搬到
//!    `C:\Users\<u>\OneDrive\桌面`，此时 `C:\Users\<u>\Desktop`
//!    根本不存在 —— 表现为"点了没反应"或"路径不存在"，而用户无从判断。
//! 2. **本地化目录名**。中文 Windows 里叫 `文档` 不是 `Documents`，
//!    繁体系统又是另一套。写死英文名对非英文系统一律失效。
//!
//! `dirs` 6.0 在 Windows 上内部走的是 `SHGetKnownFolderPath` +
//! `FOLDERID_*`（已实测 `dirs-sys-0.3.7` 源码确认），这两件事它天然对。
//!
//! ### 为什么不是自己调 `SHGetKnownFolderPath`
//!
//! 试过，手写并不比 `dirs` 好：
//!
//! - 那个 API 返回的 `PWSTR` 由 `CoTaskMemAlloc` 分配，**必须**配
//!   `CoTaskMemFree`，漏了就每次调用泄漏一段内存（反复切目录会累积）；
//! - 还要自己处理 COM 初始化（`CoInitializeEx`）与线程模型假设；
//! - 而 `dirs` 已经把上面这些做对了。
//!
//! 等价的正确实现没有理由重写一遍。手写唯一的"好处"是把已验证的依赖
//! 换成未验证的自研代码。
//!
//! ## 为什么 `key` 与 `label` 分开
//!
//! `key` 是稳定语义标识（`desktop`），`label` 是界面文案。
//! 分开是因为**目录名会本地化，而界面文案不该跟着系统语言变** ——
//! 界面上永远显示"桌面"，实际路径可能是 `桌面` 也可能是 `Desktop`。
//! 前端也靠 `key` 选图标，不必解析路径。

use serde::{Deserialize, Serialize};

/// 一个常用位置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KnownPlace {
    /// 稳定语义标识（`desktop` / `downloads` / …）。
    ///
    /// 前端靠它选图标与做本地化，**不要**拿 `label` 或路径去匹配 ——
    /// 目录名会本地化，路径会随用户变动。
    pub key: String,
    /// 界面显示名。固定中文，不随系统语言变化。
    pub label: String,
    /// 浏览根 = 该卷（`C:` / `D:`；非 Windows 为 `""`）。
    ///
    /// 之所以**预先拆好**而不是把绝对路径整个丢给前端：拆分的规则是
    /// 平台相关的（盘符 / SAF 卷 / 根目录），放前端就等于把同一套规则
    /// 用 TypeScript 再写一遍，早晚和 Rust 侧漂移。
    pub volume: String,
    /// 根之下的相对路径，分隔符固定为 `/`（与 `normalize_sub_path` 一致）。
    pub rel_path: String,
    /// 绝对路径，仅用于 title 悬浮提示与调试；导航不依赖它。
    pub path: String,
}

/// 内置表：`(dirs 访问器名, key, 界面文案)`。
///
/// 顺序即界面顺序，按"传输场景最常用"排：桌面是拖拽重灾区，
/// 下载是收件后翻找的重灾区，文档/图片/音乐/视频是次常用。
/// `home`（用户目录）是上面几项的容器，放最后。
const BUILTIN: &[(&str, &str, &str)] = &[
    ("desktop", "desktop", "桌面"),
    ("download", "downloads", "下载"),
    ("document", "documents", "文档"),
    ("picture", "pictures", "图片"),
    ("audio", "music", "音乐"),
    ("video", "videos", "视频"),
    ("home", "home", "用户目录"),
];

/// 列出本机可用的常用位置。
///
/// ## 不存在的目录会被**剔除**，而不是列出来再报错
///
/// `SHGetKnownFolderPath` 在目录不存在时照样返回路径（我们**不给**
/// `KF_FLAG_CREATE`，因为"看看有哪些常用目录"是只读操作，
/// 不该在用户磁盘上留下痕迹 —— 那需要建目录才有意义）。
///
/// 剔掉的理由很实际：一个点了必然报错的死条目，比少一个条目更糟 ——
/// 用户会反复点它，并且开始怀疑程序坏了。
///
/// ## 返回顺序稳定
///
/// 固定顺序 + 剔除缺失项，顺序不会因为某个目录缺失而整体错位，
/// 前端不需要为"第 3 项可能是空的"写任何逻辑。
pub fn known_places() -> Vec<KnownPlace> {
    let mut out = Vec::new();
    for (accessor, key, label) in BUILTIN {
        let Some(path) = resolve(accessor) else {
            continue;
        };
        if !path.is_dir() {
            tracing::debug!("常用位置 {} 不存在，跳过: {}", key, path.display());
            continue;
        }
        // 拆不开（盘符不合法 / 卷无法解析）就跳过：给前端一个
        // 点了会失败的选项，比不给更糟。
        let Some((volume, rel_path)) = super::volumes::split_browse_path(&path) else {
            tracing::debug!("常用位置 {} 无法拆成卷+相对路径，跳过: {}", key, path.display());
            continue;
        };
        out.push(KnownPlace {
            key: (*key).to_string(),
            label: (*label).to_string(),
            volume,
            rel_path,
            path: path.to_string_lossy().to_string(),
        });
    }
    out
}

/// `dirs` 访问器名 → 目录。集中在一处，方便测试替换。
fn resolve(accessor: &str) -> Option<std::path::PathBuf> {
    match accessor {
        "desktop" => dirs::desktop_dir(),
        "download" => dirs::download_dir(),
        "document" => dirs::document_dir(),
        "picture" => dirs::picture_dir(),
        "audio" => dirs::audio_dir(),
        "video" => dirs::video_dir(),
        "home" => dirs::home_dir(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机至少要能给出**桌面**。
    ///
    /// 为什么只钉这一个：Windows 上一定有用户桌面，而"图片/音乐/视频"
    /// 可能压根没建过（`is_dir` 为 false，被设计剔掉是对的）。
    /// 断言"必须恰好 7 条"会在任何一台干净机器上失败 ——
    /// 那样的测试测的是测试机器，不是代码。
    #[test]
    fn known_places_contains_desktop_on_windows() {
        let places = known_places();
        if cfg!(windows) {
            assert!(
                places.iter().any(|p| p.key == "desktop"),
                "Windows 上应有桌面，实际: {:?}",
                places.iter().map(|p| &p.key).collect::<Vec<_>>()
            );
        }
    }

    /// 输出顺序 = `BUILTIN` 顺序**扣掉缺失项**，不能错位。
    ///
    /// ## 期望值怎么算
    ///
    /// 取 `BUILTIN` 的 key 序列，再剔除本机解析不出来的那些 —— 也就是
    /// 独立复算一遍"应该是什么"。前端与协议都假设列表是稳定的，
    /// 顺序一漂，界面上"下载"就会跑到"文档"后面。
    ///
    /// ⚠️ 第一版这里拿**字典序**去比，于是把 `documents` 排到了
    /// `downloads` 前面而失败。可字典序根本不是我们要的顺序 ——
    /// 期望值必须来自 `BUILTIN` 自己。
    #[test]
    fn known_places_follow_builtin_order() {
        let present: std::collections::HashSet<String> =
            known_places().into_iter().map(|p| p.key).collect();
        let expected: Vec<&str> = BUILTIN
            .iter()
            .map(|(_, key, _)| *key)
            .filter(|k| present.contains(*k))
            .collect();
        let actual: Vec<String> = known_places().into_iter().map(|p| p.key).collect();
        assert_eq!(actual, expected, "常用位置顺序与 BUILTIN 不一致");
    }

    /// 返回的每一项都必须能**真的导航过去**。
    ///
    /// 这条比"非空"强得多：`volume` + `rel_path` 拼回来必须等于原路径，
    /// 否则前端一点就跳到别的地方去 —— 而那种 bug 在界面上表现为
    /// "点桌面进了 C:\ 根目录"，很难联想到是拆分逻辑错了。
    #[test]
    fn known_places_round_trip_through_volume_and_rel() {
        for p in known_places() {
            let abs = super::super::volumes::resolve_browse_path(&p.volume, &p.rel_path)
                .unwrap_or_else(|| panic!("{} 的 volume+rel 拼不出路径: {:?} + {:?}", p.key, p.volume, p.rel_path));
            assert_eq!(
                abs.to_string_lossy().to_string(),
                p.path,
                "{} 往返不一致：拆开再拼回去变成了别的目录",
                p.key
            );
        }
    }

    /// key 不得重复 —— 前端靠 key 索引，重复会静默覆盖掉一项。
    #[test]
    fn known_place_keys_are_unique() {
        let mut keys: Vec<String> = known_places().into_iter().map(|p| p.key).collect();
        let n = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), n, "key 出现重复");
    }

    /// rel_path 不得带 `..`，也不得是绝对路径。
    ///
    /// `normalize_sub_path` 会挡，但**在这里**就该是干净的：
    /// 这些值会被前端原样回传成浏览请求，带着 `..` 等于自造一次
    /// "路径逃逸"告警，而真因只是我们拼错了。
    #[test]
    fn known_places_rel_paths_are_clean() {
        for p in known_places() {
            assert!(
                !p.rel_path.starts_with('/') && !p.rel_path.contains(':'),
                "{} 的 rel_path 不该是绝对路径: {:?}",
                p.key,
                p.rel_path
            );
            assert!(
                !p.rel_path.split('/').any(|s| s == ".."),
                "{} 的 rel_path 含 '..': {:?}",
                p.key,
                p.rel_path
            );
        }
    }
}

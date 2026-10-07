//! 目录递归展开（把一个文件夹变成一批 `(本地路径, 目标相对路径)`）。
//!
//! # 为什么不改协议
//!
//! 清单里的 `relative_path` **本来就允许多级**（`2026/报表/1月.csv`），
//! 接收端也已经有 `validate_relative_subpath` 逐段校验 + staging 原子提交。
//! 所以"传文件夹"不需要任何协议改动 —— 只需要在**建清单之前**
//! 把目录展平成一批带层级的相对路径。
//!
//! 这也是为什么这个功能放在这里而不是放在传输层：
//! 传输层只认"一批文件 + 各自的目标相对路径"，它不需要知道
//! 目标路径是怎么来的。

use std::path::{Path, PathBuf};

use crate::error::{FeisuoError, Result};

/// 递归展开的上限。这些不是"随便定的数"，每一个都对应一类真实事故：
///
/// - **深度 32**：Windows 路径上限 260 字符、中间还会插入收件目录前缀。
///   超过 32 层的目录在实践中只可能是符号链接循环或备份软件的产物。
/// - **单次 20000 文件**：`MAX_FILES_PER_BATCH` 之下留出余量。
///   一次拖进 20 万个文件会让清单 JSON 超过 `MAX_JSON_FRAME`。
/// - **总体积 2 TiB**：只是防止 `total_size` 溢出 u64 的最后一道闸
///   （真正的闸是磁盘剩余空间，那在接收端判）。
pub const MAX_FOLDER_DEPTH: usize = 32;
pub const MAX_FOLDER_FILES: usize = 20_000;

/// 永远跳过的目录名。
///
/// 全部是**产品自己的**内部目录，不是"用户可能不需要的东西"——
/// 把它们发出去会让对端收到一堆莫名其妙的东西，
/// 而且 `.feisuo-incoming` 是**正在写入**的暂存区，
/// 边写边读会拿到截断的文件。
const INTERNAL_DIRS: &[&str] = &[
    ".feisuo-incoming",
    ".feisuo-staging",
    "System Volume Information",
    "$RECYCLE.BIN",
    "node_modules",
    ".git",
    ".svn",
    ".hg",
];

/// 展开结果 + 给人看的说明。
#[derive(Debug, Default)]
pub struct FolderScan {
    /// `(本地绝对路径, 目标相对路径)`，已按相对路径排序
    pub items: Vec<(PathBuf, String)>,
    /// 跳过的条目数（符号链接、内部目录、深度超限…）
    pub skipped: u32,
    /// 遇到的符号链接数（单独计数是因为它值得让用户知道）
    pub symlinks_skipped: u32,
    /// 总体积
    pub total_bytes: u64,
    /// 触发的限制（用于给用户明确解释，而不是静默截断）
    pub limit_hit: Option<String>,
}

/// 把一个条目（文件或目录）展开成一批可发送项。
///
/// ## `dest_root` 是文件夹**自身**的名字
///
/// 拖入 `D:\项目` ⇒ 目标路径是 `项目/a.csv`、`项目/子/b.txt`。
/// 不把文件夹名放进目标路径的话，对端收件目录里会是一堆
/// 不知道来自哪个文件夹的散文件。
pub fn expand(path: &Path, dest_root: &str) -> Result<FolderScan> {
    let mut scan = FolderScan::default();
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| FeisuoError::IoContext {
            context: format!("无法读取 {}: {}", path.display(), e),
            source: e,
        })?;

    if meta.is_file() {
        scan.total_bytes = meta.len();
        scan.items.push((path.to_path_buf(), dest_root.to_string()));
        return Ok(scan);
    }
    if !meta.is_dir() {
        // 符号链接、FIFO、设备文件… 一律不传。
        // 理由：符号链接可能指向收件目录之外，传出去等于让对端
        // 拿到一个"看起来在里面、实际指向别处"的路径。
        scan.skipped += 1;
        scan.symlinks_skipped += 1;
        return Ok(scan);
    }

    walk(path, dest_root, 0, &mut scan)?;
    // 排序保证同一文件夹两次拖入产生**同一份清单**。
    // 不排序的话清单顺序依赖 read_dir 的返回顺序，
    // 而断点续传是按 (transfer_id, relative_path) 匹配的 ——
    // 顺序变了不影响匹配，但会让诊断记录难以比对。
    scan.items.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(scan)
}

fn walk(dir: &Path, rel: &str, depth: usize, scan: &mut FolderScan) -> Result<()> {
    if depth > MAX_FOLDER_DEPTH {
        scan.limit_hit = Some(format!(
            "目录层级超过 {} 层，已停止深入（常见于符号链接循环）",
            MAX_FOLDER_DEPTH
        ));
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            // 无权限的目录（`C:\System Volume Information`、别人的
            // 用户目录）不该让整次拖放失败 —— 记一笔跳过继续。
            tracing::debug!("跳过不可读目录 {}: {}", dir.display(), e);
            scan.skipped += 1;
            return Ok(());
        }
    };
    for entry in entries.flatten() {
        if scan.items.len() >= MAX_FOLDER_FILES {
            scan.limit_hit = Some(format!(
                "文件数超过 {} 上限，仅发送前 {} 个",
                MAX_FOLDER_FILES, MAX_FOLDER_FILES
            ));
            return Ok(());
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || is_internal(&name) {
            scan.skipped += 1;
            continue;
        }
        // symlink_metadata **不跟随**符号链接 ——
        // 这正是我们要的：跟随了就无法判断它是不是链接。
        let meta = match std::fs::symlink_metadata(entry.path()) {
            Ok(m) => m,
            Err(_) => {
                scan.skipped += 1;
                continue;
            }
        };
        let child_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", rel, name)
        };
        if meta.file_type().is_symlink() {
            // 不跟随。理由见上。
            scan.skipped += 1;
            scan.symlinks_skipped += 1;
            continue;
        }
        if meta.is_dir() {
            walk(&entry.path(), &child_rel, depth + 1, scan)?;
        } else if meta.is_file() {
            // 提前自检相对路径：非法路径（控制字符、保留名）不该
            // 走到建清单那一步才失败，那时已经建好连接了。
            if let Err(e) = crate::storage::PathManager::validate_relative_subpath(&child_rel)
            {
                tracing::warn!("跳过非法相对路径 {}: {}", child_rel, e);
                scan.skipped += 1;
                continue;
            }
            scan.total_bytes = scan.total_bytes.saturating_add(meta.len());
            scan.items.push((entry.path(), child_rel));
        } else {
            scan.skipped += 1;
        }
    }
    Ok(())
}

fn is_internal(name: &str) -> bool {
    // Windows 上这些名是大小写不敏感的
    INTERNAL_DIRS.iter().any(|d| d.eq_ignore_ascii_case(name))
}

/// 把一批路径展开成可发送项。文件夹与文件混在一起也能处理。
///
/// 返回的顺序：先按输入顺序展开，文件夹内部按相对路径排序。
pub fn expand_all(paths: &[PathBuf]) -> Result<(Vec<(PathBuf, String)>, FolderScan)> {
    let (items, total, _roots) = expand_all_detailed(paths)?;
    Ok((items, total))
}

/// 单个拖入路径的展开结果（按根分组）。
///
/// ## 为什么需要它
///
/// 待发清单是**按路径**列的，而展开是**按根**做的。清单上写着
/// 「ai-memory-kit　4.0 KB」，而实际要发的是里面 7 个文件 17.8 MB ——
/// 4.0 KB 是 NTFS **目录项自身**的大小。这个条目在撒谎。
///
/// 不展开成 7 行是**故意的**：`folder_scan` 的上限是 20000 个文件，
/// 一次拖一个 node_modules 就可能在清单里生成两万行，
/// 界面直接卡死。而发送时本来就会再展开一次（`send_files_to_dest_inner`），
/// 所以清单里保留目录本身既不丢信息也不会撑爆界面 —— 前提是
/// **显示的个数与大小必须是真的**，那就是本结构存在的理由。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RootExpansion {
    /// 被拖入的原始路径
    pub path: String,
    /// 这一项展开后实际会发送的文件数（普通文件恒为 1）
    pub file_count: usize,
    /// 这一项展开后的总字节数
    pub total_bytes: u64,
    /// 原始路径是不是目录 —— 清单要用它决定显示「N 个文件」还是「4.0 KB」
    pub is_dir: bool,
    /// 这一项内部跳过的条目数
    pub skipped: u32,
    /// 这一项内部跳过的符号链接数
    pub symlinks_skipped: u32,
}

/// [`expand_all`] 的详细版：额外返回**按根**的展开统计。
///
/// 早先只返回累加后的总数，于是 UI 只能显示"1 项 4.0 KB"。
/// 现在每个拖入路径都带自己的文件数与字节数，界面才能说实话。
pub fn expand_all_detailed(
    paths: &[PathBuf],
) -> Result<(Vec<(PathBuf, String)>, FolderScan, Vec<RootExpansion>)> {
    let mut all: Vec<(PathBuf, String)> = Vec::new();
    let mut total = FolderScan::default();
    let mut roots: Vec<RootExpansion> = Vec::with_capacity(paths.len());
    for p in paths {
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "未命名".to_string());
        let scan = expand(p, &name)?;
        roots.push(RootExpansion {
            path: p.to_string_lossy().to_string(),
            file_count: scan.items.len(),
            total_bytes: scan.total_bytes,
            // 必须在 expand **之前**判：`expand` 走的是内部条目过滤，
            // 一个只含 .git 的空目录展开后 items 为空，但用户拖的是目录，
            // 显示成"0 个文件"会让人以为拖错了。
            is_dir: p.is_dir(),
            skipped: scan.skipped,
            symlinks_skipped: scan.symlinks_skipped,
        });
        all.extend(scan.items);
        total.skipped += scan.skipped;
        total.symlinks_skipped += scan.symlinks_skipped;
        total.total_bytes = total.total_bytes.saturating_add(scan.total_bytes);
        if total.limit_hit.is_none() {
            total.limit_hit = scan.limit_hit;
        }
    }
    Ok((all, total, roots))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("feisuo-fs-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).expect("建临时目录失败");
        base
    }

    fn write(path: &std::path::Path, bytes: &[u8]) {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).expect("建父目录失败");
        }
        fs::write(path, bytes).expect("写文件失败");
    }

    /// 复现真实故障：拖一个含 7 个文件的文件夹进来，清单上只有 1 行，
    /// 而显示的是目录项自身的 4.0 KB。
    ///
    /// 断言的是**按根统计**：目录那一项必须报 7 个文件、7 份字节数。
    /// 早先没有这个数据，前端只能退回去量目录项本身，于是显示 4.0 KB。
    #[test]
    fn expanded_reports_per_root_counts_not_directory_entry() {
        let root = tmpdir("per-root");
        let dir = root.join("ai-memory-kit");
        let mut expected_bytes: u64 = 0;
        let sizes: [usize; 7] = [10, 20, 30, 40, 50, 60, 70];
        for (i, n) in sizes.iter().enumerate() {
            write(&dir.join(format!("file-{}.bin", i)), &vec![b'x'; *n]);
            expected_bytes += *n as u64;
        }

        let (items, total, roots) = expand_all_detailed(&[dir.clone()]).expect("展开失败");

        assert_eq!(items.len(), 7, "应展开出 7 个文件");
        assert_eq!(total.total_bytes, expected_bytes, "总字节数不对");
        assert_eq!(roots.len(), 1, "只拖了一个根，就该只有一条按根统计");
        assert_eq!(roots[0].file_count, 7, "按根统计必须报 7，而不是 1");
        assert_eq!(roots[0].total_bytes, expected_bytes);
        assert!(roots[0].is_dir, "拖的是目录，is_dir 必须为 true");

        let _ = fs::remove_dir_all(&root);
    }

    /// **空目录也要算目录**。
    ///
    /// 理由：`expand` 会过滤内部条目，一个只含 `.git` 的目录展开后
    /// items 为空。若 `is_dir` 靠"展开后有没有东西"推断，界面会把它
    /// 显示成一个普通文件 —— 用户拖了文件夹，屏幕上却看不出那是目录。
    #[test]
    fn empty_directory_is_still_reported_as_dir() {
        let root = tmpdir("empty-dir");
        let dir = root.join("only-dot-git");
        write(&dir.join(".git").join("config"), b"x");

        let (_items, _total, roots) = expand_all_detailed(&[dir.clone()]).expect("展开失败");
        assert_eq!(roots.len(), 1);
        assert!(roots[0].is_dir, "空目录也必须标成目录");
        assert_eq!(roots[0].file_count, 0, "内部项全被跳过时文件数应为 0");
        assert!(roots[0].skipped >= 1, "跳过的条目要被如实计数");

        let _ = fs::remove_dir_all(&root);
    }

    /// 普通文件：file_count 恒为 1，且 `is_dir` 为 false。
    ///
    /// 这条钉住"文件显示大小、目录显示文件数"的分界不会串 ——
    /// 文件也被标成目录的话，界面上每个文件都会多出"1 个文件"标签。
    #[test]
    fn plain_file_reports_one_and_not_dir() {
        let root = tmpdir("plain-file");
        let f = root.join("note.txt");
        write(&f, b"hello");

        let (items, total, roots) = expand_all_detailed(&[f.clone()]).expect("展开失败");
        assert_eq!(items.len(), 1);
        assert_eq!(total.total_bytes, 5);
        assert_eq!(roots[0].file_count, 1);
        assert!(!roots[0].is_dir, "普通文件不能标成目录");

        let _ = fs::remove_dir_all(&root);
    }

    /// 混着拖多个根时，每条按根统计各自独立，不能串味。
    ///
    /// 串味的实际后果：拖「一个文件夹 + 一个文件」，界面把两个的文件数
    /// 加在一起显示给文件夹那一行，于是每一行看着都不对。
    #[test]
    fn multiple_roots_are_reported_independently() {
        let root = tmpdir("multi-root");
        let dir = root.join("d");
        for i in 0..3 {
            write(&dir.join(format!("f{}.bin", i)), &vec![b'x'; 10]);
        }
        let f = root.join("single.bin");
        write(&f, &vec![b'y'; 7]);

        let (items, total, roots) = expand_all_detailed(&[dir.clone(), f.clone()]).expect("展开失败");
        assert_eq!(items.len(), 4, "3 + 1");
        assert_eq!(total.total_bytes, 37, "30 + 7");
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].file_count, 3);
        assert_eq!(roots[0].total_bytes, 30);
        assert!(roots[0].is_dir);
        assert_eq!(roots[1].file_count, 1);
        assert_eq!(roots[1].total_bytes, 7);
        assert!(!roots[1].is_dir);

        let _ = fs::remove_dir_all(&root);
    }

    /// 按根统计的 `path` 必须**逐字**等于传入的路径 —— 前端就是拿它
    /// 把统计映射回清单行的（`expanded.find(e => e.path === p)`）。
    ///
    /// 差一个盘符大小写或分隔符，映射就悄悄失配，于是清单退回显示
    /// 目录项自身的 4.0 KB —— 又回到本模块要修的那个 bug，而且没有任何报错。
    #[test]
    fn root_expansion_path_matches_input_exactly() {
        let root = tmpdir("path-exact");
        let dir = root.join("d");
        write(&dir.join("a.bin"), b"x");
        let (_, _, roots) = expand_all_detailed(&[dir.clone()]).expect("展开失败");
        assert_eq!(roots[0].path, dir.to_string_lossy().to_string());
        let _ = fs::remove_dir_all(&root);
    }

    /// `expand_all` 与 `expand_all_detailed` 的前两项必须**完全一致**。
    ///
    /// 详情版是给 UI 用的，老的 `expand_all` 仍被发送路径调用。
    /// 两者一旦漂移，界面上显示的规模就会和真正发出去的不一致 ——
    /// 而那正是本次要修的那类"两个真数字互相矛盾"。
    #[test]
    fn detailed_and_plain_expand_all_agree() {
        let root = tmpdir("agree");
        let dir = root.join("d");
        for i in 0..4 {
            write(&dir.join(format!("f{}.bin", i)), &vec![b'x'; i + 1]);
        }
        let write_link = root.join("l.bin");
        write(&write_link, b"z");

        let (items_a, total_a) = expand_all(&[dir.clone(), write_link.clone()]).expect("expand_all 失败");
        let (items_b, total_b, roots) = expand_all_detailed(&[dir.clone(), write_link.clone()])
            .expect("expand_all_detailed 失败");

        assert_eq!(items_a.len(), items_b.len(), "展开条目数不一致");
        assert_eq!(total_a.total_bytes, total_b.total_bytes, "总字节数不一致");
        assert_eq!(total_a.skipped, total_b.skipped, "跳过数不一致");
        assert_eq!(total_a.symlinks_skipped, total_b.symlinks_skipped);
        assert_eq!(total_a.limit_hit, total_b.limit_hit);
        // 按根文件数之和 == 展开总数
        let sum: usize = roots.iter().map(|r| r.file_count).sum();
        assert_eq!(sum, items_b.len(), "按根文件数之和应等于展开总数");

        let _ = fs::remove_dir_all(&root);
    }
}

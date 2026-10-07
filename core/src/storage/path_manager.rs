use std::path::{Component, Path, PathBuf};
use crate::error::{FeisuoError, Result};

/// Windows 保留设备名: 即便带扩展名也会被系统吞掉, 必须直接拒绝。
const RESERVED_STEMS: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

pub struct PathManager;

impl PathManager {
    /// 校验对端给出的相对文件名是否安全。
    ///
    /// 关键点: 只接受**单个** `Normal` 组件。旧实现仅过滤 `ParentDir`,
    /// 而 `Path::join` 在传入绝对路径时会**直接替换** base,
    /// 于是 `\Windows\System32\...`、`C:\...`、`\\server\share\...` 全部可以逃逸接收目录。
    /// 发送端本来就只会发裸文件名, 因此"扁平化"是最简单也最彻底的做法。
    pub fn validate_relative_path(relative_path: &str) -> Result<()> {
        if relative_path.is_empty() {
            return Err(FeisuoError::Security("文件名为空".into()));
        }
        // 先按字节长度限流, 避免超长输入进入后续逐字符检查
        if relative_path.len() > 200 {
            return Err(FeisuoError::Security("文件名过长".into()));
        }
        if relative_path.contains('\0') {
            return Err(FeisuoError::Security("文件名包含非法字符".into()));
        }
        if relative_path.contains('/') || relative_path.contains('\\') {
            return Err(FeisuoError::Security("文件名不得包含路径分隔符".into()));
        }
        if relative_path == "." || relative_path == ".." {
            return Err(FeisuoError::Security("非法文件名".into()));
        }
        // Windows 会静默吞掉结尾的点和空格: "a.txt " 落盘后变成 "a.txt",
        // 造成"界面显示一个文件, 磁盘上却是另一个"的错位。必须基于**原始输入**判断,
        // 绝不能先 trim 再判断 —— 那样这条检查永远命中不了。
        if relative_path.ends_with('.') || relative_path.ends_with(' ') {
            return Err(FeisuoError::Security("文件名不得以空格或点结尾".into()));
        }
        // 只允许单个 Normal 组件
        let mut components = Path::new(relative_path).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(part)), None) => {
                if part.is_empty() {
                    return Err(FeisuoError::Security("非法文件名".into()));
                }
            }
            (Some(Component::Prefix(_)), _) | (Some(Component::RootDir), _) => {
                return Err(FeisuoError::Security("不允许写入接收目录之外的位置".into()));
            }
            (Some(Component::ParentDir), _) => {
                return Err(FeisuoError::Security("检测到路径穿越攻击".into()));
            }
            _ => return Err(FeisuoError::Security("非法文件名".into())),
        }

        let stem_upper = Path::new(relative_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_uppercase();
        if RESERVED_STEMS.contains(&stem_upper.as_str()) {
            return Err(FeisuoError::Security(format!(
                "文件名 \"{}\" 是系统保留设备名",
                relative_path
            )));
        }

        Ok(())
    }

    /// 校验"相对子路径"(可含 `/` 分隔的多级目录)。
    ///
    /// 与 [`validate_relative_path`](Self::validate_relative_path) 的区别:
    /// 后者只接受**单个** `Normal` 组件(扁平落盘), 本方法允许
    /// `2026/报表/1月.csv` 这样的多级相对路径 —— 穿梭"取回"需要保留
    /// 对端的目录结构, 否则在子文件夹里选文件就毫无意义。
    ///
    /// 安全约束与单段版本完全一致, 只是逐段施加:
    /// - 任一段都不得是 `.` / `..` / 空
    /// - 不得以 `/` 或 `\` 开头 (绝对路径), 不得含盘符或 `\\`
    /// - 不得含 NUL
    /// - 总长度有上限, 防止超长路径耗尽栈/磁盘
    pub fn validate_relative_subpath(subpath: &str) -> Result<()> {
        if subpath.is_empty() {
            return Err(FeisuoError::Security("文件名为空".into()));
        }
        if subpath.len() > 512 {
            return Err(FeisuoError::Security("相对路径过长".into()));
        }
        if subpath.contains('\0') {
            return Err(FeisuoError::Security("路径包含非法字符".into()));
        }
        // 统一分隔符后再判绝对路径
        let unified = subpath.replace('\\', "/");
        if unified.starts_with('/') {
            return Err(FeisuoError::Security("不允许写入接收目录之外的位置".into()));
        }
        if unified.contains(':') {
            return Err(FeisuoError::Security("路径不得包含盘符".into()));
        }
        let segments: Vec<&str> = unified
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        if segments.is_empty() {
            return Err(FeisuoError::Security("文件名为空".into()));
        }
        // 逐段套用单段校验(它会拒绝 `..`、保留设备名、结尾空格/点等)
        for seg in &segments {
            Self::validate_relative_path(seg)?;
        }
        Ok(())
    }

    /// 在 base_dir 下为 relative_path 解析出一个安全的落盘路径,
    /// 若同名文件已存在则自动追加序号 (如 "file (1).ext"), 绝不覆盖原有数据。
    pub fn resolve_destination(base_dir: &Path, relative_path: &str) -> Result<PathBuf> {
        Self::validate_relative_path(relative_path)?;
        Self::resolve_unique_path(base_dir, relative_path.trim())
    }

    /// 已完成安全校验后的路径解析 (同目录, 不再重复做分隔符检查)
    pub fn resolve_unique_path(base_dir: &Path, file_name: &str) -> Result<PathBuf> {
        Self::resolve_unique_subpath(base_dir, file_name)
    }

    /// 为**可含多级目录**的相对路径解析落盘位置, 并保证父目录存在。
    ///
    /// "绝不覆盖"的红线在这里同样成立: 逐级用 `create_new` 独占创建占位,
    /// 同名时只在**最后一级**追加序号 (`报表 (1).csv`), 父目录结构保持不变。
    ///
    /// 安全: 本方法**自己就先校验**一遍, 而不是依赖调用方记得先调
    /// [`validate_relative_subpath`](Self::validate_relative_subpath)。
    /// 它是 `pub` 的, 而下面直接 `parent.push(seg)` + `create_dir_all` ——
    /// 一旦有调用方忘了校验, `../../` 就会在接收目录之外建出目录树。
    /// 重复校验的代价可以忽略(纯字符串检查), 换来的是"默认安全"。
    pub fn resolve_unique_subpath(base_dir: &Path, subpath: &str) -> Result<PathBuf> {
        Self::validate_relative_subpath(subpath)?;
        let unified = subpath.replace('\\', "/");
        let mut parent = base_dir.to_path_buf();
        let mut segments: Vec<&str> = Vec::new();
        for seg in unified.split('/').filter(|s| !s.is_empty()) {
            segments.push(seg);
        }
        if segments.is_empty() {
            return Err(FeisuoError::Security("文件名为空".into()));
        }
        // 除最后一级外都当作目录创建
        for dir_seg in &segments[..segments.len() - 1] {
            parent.push(dir_seg);
            std::fs::create_dir_all(&parent)?;
        }
        let leaf = segments[segments.len() - 1];

        // 直接用 create_new 独占创建来"占位":
        // 只做 exists() 预检查的话, 两个并发同名传输会拿到同一个路径互相覆盖。
        // 这里的文件随后会被 ChunkStore::write_chunk_at 继续写入。
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(parent.join(leaf))
        {
            Ok(_) => return Ok(parent.join(leaf)),
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(FeisuoError::Io(e));
            }
            Err(_) => {}
        }

        let stem = Path::new(leaf)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("file");
        let ext = Path::new(leaf)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        // 递增循环必须有上限, 否则攻击者预先造出上万个同名文件就能让服务端忙等。
        for counter in 1..=9999u32 {
            let new_filename = if ext.is_empty() {
                format!("{} ({})", stem, counter)
            } else {
                format!("{} ({}).{}", stem, counter, ext)
            };
            match std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(parent.join(&new_filename))
            {
                Ok(_) => return Ok(parent.join(&new_filename)),
                Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
                    return Err(FeisuoError::Io(e))
                }
                Err(_) => continue,
            }
        }

        Err(FeisuoError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "同名文件过多, 无法为接收文件分配新名称",
        )))
    }
}

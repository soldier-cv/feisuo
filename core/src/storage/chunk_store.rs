use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use crate::error::Result;

pub struct ChunkStore;

impl ChunkStore {
    /// Calculate BLAKE3 hash for a local file
    pub fn hash_file(path: &Path) -> Result<String> {
        let mut hasher = blake3::Hasher::new();
        let mut file = File::open(path)?;
        let mut buffer = vec![0u8; 256 * 1024];

        loop {
            let bytes_read = file.read(&mut buffer)?;
            if bytes_read == 0 {
                break;
            }
            hasher.update(&buffer[..bytes_read]);
        }

        Ok(hasher.finalize().to_hex().to_string())
    }

    /// Calculate BLAKE3 hash for in-memory chunk
    pub fn hash_chunk(data: &[u8]) -> String {
        blake3::hash(data).to_hex().to_string()
    }

    /// 预分配接收文件: 独占创建 + 固定最终长度。
    ///
    /// 旧实现每 4MB 重新 `create(true).write(true)` 打开一次,
    /// 既无法独占创建 (两个并发同名传输会互相覆盖), 也无法截断,
    /// 于是旧文件残留的尾部会被拼进"新文件"里, 却依然上报为成功。
    ///
    /// 注意: 目标文件已由 `PathManager::resolve_unique_path` 以 `create_new` 独占占位,
    /// 这里只负责固定长度。
    ///
    /// ## 为什么这里**不再** `sync_all`（P4 性能修正）
    ///
    /// 旧实现每文件做 **两次**磁盘屏障：本方法一次、`finalize_file` 一次。
    /// 对"多小文件同步"这个产品核心用法是主要开销 ——
    /// 1000 个文件 = 2000 次屏障，SSD 上约 2 秒，机械盘/网络盘更差。
    /// 内容落盘由 `finalize_file` 的**一次** `sync_all` 统一兜底。
    pub fn prepare_file(file_path: &Path, total_len: u64) -> Result<()> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .open(file_path)?;
        // set_len 会同时截断, 保证不会残留上一份文件的尾部
        file.set_len(total_len)?;
        Ok(())
    }

    /// Read chunk bytes from a local file
    pub fn read_chunk(path: &Path, chunk_index: u64, chunk_size: usize) -> Result<Vec<u8>> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        let offset = chunk_index * (chunk_size as u64);
        if offset >= file_len {
            return Ok(Vec::new());
        }
        file.seek(SeekFrom::Start(offset))?;

        // 末块只读实际剩余长度, 避免为一个 1 字节文件分配并清零 4 MiB
        let want = std::cmp::min(chunk_size as u64, file_len - offset) as usize;
        let mut buffer = vec![0u8; want];
        let mut total_read = 0;
        while total_read < want {
            let n = file.read(&mut buffer[total_read..])?;
            if n == 0 {
                break;
            }
            total_read += n;
        }
        buffer.truncate(total_read);
        Ok(buffer)
    }

    /// Write chunk data into a (already created) file at specific offset
    pub fn write_chunk_at(file_path: &Path, chunk_index: u64, chunk_size: usize, data: &[u8]) -> Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).open(file_path)?;

        let offset = chunk_index * (chunk_size as u64);
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(data)?;
        Ok(())
    }

    /// 收完一个文件后 flush + 落盘
    pub fn finalize_file(file_path: &Path) -> Result<()> {
        let file = std::fs::OpenOptions::new().write(true).open(file_path)?;
        file.sync_all()?;
        Ok(())
    }
}

/// 发送侧：整个文件只开一次句柄，分块循环内用**定位读**。
///
/// ## 为什么要这个类型（设计文档 §9.3 第 1 行）
///
/// 旧实现 `ChunkStore::read_chunk(path, idx, size)` **每块都 `File::open`**。
/// 一个 4 GiB 文件按 4 MiB 分块 = **1024 次 open + 1024 次 seek**。
/// 在 3 MB/s 时这一点都不显著（每 1.4 秒才 open 一次），
/// 但一旦链路是千兆 / 有线，1024 次 open 就会成为可测量的损耗。
///
/// ## 为什么用 `&self` 而不是 `&mut self`
///
/// 定位读 `seek_read` 只需要共享引用，于是本类型可以直接
/// `Arc<ChunkReader>` 包起来，在每次 `spawn_blocking` 里克隆一份 Arc。
/// 若用 `&mut self` 就必须把句柄 move 进闭包再 move 回来 —— 在
/// async 循环里做不到，会编译失败。
///
/// 保留 [`ChunkStore::read_chunk`] 供单块读取场景与既有测试使用。
pub struct ChunkReader {
    file: File,
    chunk_size: u64,
    file_len: u64,
}

impl ChunkReader {
    pub fn open(path: &Path, chunk_size: usize) -> Result<Self> {
        let file = File::open(path)?;
        let file_len = file.metadata()?.len();
        Ok(Self { file, chunk_size: chunk_size as u64, file_len })
    }

    /// 读取第 `chunk_index` 块。越界返回空 `Vec`（与旧实现语义一致）。
    pub fn read_chunk(&self, chunk_index: u64) -> Result<Vec<u8>> {
        let offset = chunk_index * self.chunk_size;
        if offset >= self.file_len {
            return Ok(Vec::new());
        }
        // 末块只读实际剩余长度, 避免为一个 1 字节文件分配并清零 4 MiB
        let want = std::cmp::min(self.chunk_size, self.file_len - offset) as usize;
        let mut buffer = vec![0u8; want];
        let mut total_read = 0;
        while total_read < want {
            let n = seek_read_at(&self.file, &mut buffer[total_read..], offset + total_read as u64)?;
            if n == 0 {
                break;
            }
            total_read += n;
        }
        buffer.truncate(total_read);
        Ok(buffer)
    }
}

/// 接收侧：整个文件只开一次句柄，分块循环内用**定位写**（§9.3 第 2 行）。
pub struct ChunkWriter {
    file: File,
    chunk_size: u64,
}

impl ChunkWriter {
    /// 打开**已由 `PathManager` 以 `create_new` 独占占位**的目标文件。
    pub fn open(file_path: &Path, chunk_size: usize) -> Result<Self> {
        let file = std::fs::OpenOptions::new().write(true).open(file_path)?;
        Ok(Self { file, chunk_size: chunk_size as u64 })
    }

    pub fn write_chunk(&self, chunk_index: u64, data: &[u8]) -> Result<()> {
        seek_write_at(&self.file, data, chunk_index * self.chunk_size)
    }

    /// 收完该文件后落盘（每文件**仅此一次** `sync_all`）。
    pub fn finalize(&self) -> Result<()> {
        // `flush` 需要 &mut, 因此这里对内部句柄用一次性可变借用;
        // 因为是 &self, 调用方仍可继续用这个 writer（虽然此时已不应再写）。
        let mut f = &self.file;
        f.flush()?;
        f.sync_all()?;
        Ok(())
    }
}

// 定位读/写的跨平台薄封装。
// Windows 与 Unix 的 `FileExt` 都提供 `seek_read` / `seek_write`（只需要 `&self`）。
#[cfg(windows)]
fn seek_read_at(file: &File, buf: &mut [u8], offset: u64) -> Result<usize> {
    use std::os::windows::fs::FileExt;
    let n = file.seek_read(buf, offset)?;
    Ok(n)
}

#[cfg(windows)]
fn seek_write_at(file: &File, buf: &[u8], offset: u64) -> Result<()> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buf, offset)?;
    Ok(())
}

// Android 走 `unix` 分支：`std::os::unix::fs::FileExt` 在 target_os="android"
// 上可用，而 `#[cfg(unix)]` 也覆盖它。
//
// `read_at` 返回的是 **`io::Result<usize>`**，不是 `usize`。
// 早先这里写 `Ok(file.read_at(buf, offset))` —— 那是在**编一个
// `Result<Result<usize, _>>`**，类型不匹配。
//
// 为什么 host 上 `cargo check` 一直是零错误：这段代码在
// `#[cfg(unix)]` 里，而 Windows 上根本不参与编译，**编译期完全看不见**。
// 它第一次被编译是在真的去编 Android 目标时 —— 也就是说，
// 在装上 NDK 之前，这个错误一直躺在树里。
#[cfg(unix)]
fn seek_read_at(file: &File, buf: &mut [u8], offset: u64) -> Result<usize> {
    use std::os::unix::fs::FileExt;
    Ok(file.read_at(buf, offset)?)
}

#[cfg(unix)]
fn seek_write_at(file: &File, buf: &[u8], offset: u64) -> Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)?;
    Ok(())
}

#[cfg(not(any(windows, unix)))]
fn seek_read_at(_file: &File, _buf: &mut [u8], _offset: u64) -> Result<usize> {
    Err(crate::error::FeisuoError::Internal("当前平台不支持定位读".into()))
}

#[cfg(not(any(windows, unix)))]
fn seek_write_at(_file: &File, _buf: &[u8], _offset: u64) -> Result<()> {
    Err(crate::error::FeisuoError::Internal("当前平台不支持定位写".into()))
}

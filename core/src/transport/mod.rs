pub mod client;
pub mod diagnostics;
pub mod server;
pub mod session;
pub mod sockopt;

use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::error::{FeisuoError, Result};
use crate::protocol::IO_TIMEOUT_SECS;

pub use client::{BrowseTarget, RemoteBrowseListing, TransferClient, UnpairReport};
pub use diagnostics::{
    classify_ip, BdpSnapshot, LinkProfile, OverlayKind, PhaseTimings, ThroughputStats,
    TransferDiagnostics, MIN_SPEED_SAMPLE_MS,
};
pub use server::TransferServer;
pub use session::{TransferDirection, TransferProgress, TransferStatus};
pub use sockopt::{BdpEstimate, MAX_SOCK_BUF, MIN_SOCK_BUF};

/// 把一次**带语义标签**的 socket I/O 失败翻译成 [`FeisuoError`]。
///
/// ## 为什么不直接 `FeisuoError::Io`
///
/// 裸 errno 排障时等于没信息。`error.rs` 里 `IoContext` 的注释已经写过一遍，
/// 但 socket 这一层的十几个调用点全都走的 `FeisuoError::Io`，把标签丢在了
/// 半路 —— 于是用户在界面上看到的是 `"I/O error: early eof"`：既不知道是哪个
/// 阶段（发消息类型 / 读握手应答 / 读分块），也不知道是谁关的连接。
///
/// 标签必须由**调用点**给出，因为只有它知道当时在干什么。
/// 传 `what` 而不是让这里猜 —— 猜出来的上下文在这种场景下毫无价值。
pub(crate) fn io_error_with(what: &str, e: std::io::Error) -> FeisuoError {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        // **刻意不把 errno 原文带进消息**。tokio 给它的文案是字面量
        // `"early eof"`, 而它携带的信息量恰好等于 `UnexpectedEof` 这个
        // kind —— 也就是说它除了"用户看不懂"之外不提供任何东西。
        // 守卫因此可以收成一条很干净的断言:
        // **这个字面量永远不许出现在用户看到的文案里。**
        // （日志侧也不损失什么: 同一个分支里可查的信息已经在
        // `accept` 循环那条 `closed by peer` 里。）
        return FeisuoError::PeerClosed(format!(
            "{} 时对方就关闭了连接（一个应答字节都没发出来）。\
             对方可能拒绝了这次请求、版本不兼容，或该端口根本不是飞梭在监听",
            what
        ));
    }
    FeisuoError::io_context(what, e)
}

/// 给单次 socket 写操作套上 IO 超时。
///
/// tokio 的 `TcpStream` 没有 std 那套 `set_write_timeout`, 所以每一个
/// `write_all` 都必须自己包一层: 只要对端停止读取(挂机 / 拔网线 / 防火墙
/// 半开连接), 裸 `write_all` 就会**永远**挂起, 表现为 UI 上进度条卡死
/// 且任务无法取消, 只能重启应用。
pub(crate) async fn with_io_timeout<F, T>(
    what: &str,
    fut: F,
) -> Result<T>
where
    F: std::future::Future<Output = std::io::Result<T>>,
{
    match tokio::time::timeout(Duration::from_secs(IO_TIMEOUT_SECS), fut).await {
        // 标签(`what`)必须传进错误里: 半路丢掉它, 用户就只看到
        // "I/O error: broken pipe", 完全不知道是哪一步写出去的。
        Ok(res) => res.map_err(|e| io_error_with(what, e)),
        Err(_) => Err(FeisuoError::Network(format!(
            "{} 超时 ({} 秒无进展), 已中断; 对方可能已离线或网络中断",
            what, IO_TIMEOUT_SECS
        ))),
    }
}

/// 供 server/client 复用的写超时包装(与 [`with_io_timeout`] 同义, 便于就近调用)。
pub(crate) async fn write_with_timeout(stream: &mut TcpStream, buf: &[u8], what: &str) -> Result<()> {
    with_io_timeout(what, stream.write_all(buf)).await
}

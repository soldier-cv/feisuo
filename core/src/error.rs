use thiserror::Error;

#[derive(Error, Debug)]
pub enum FeisuoError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// 附带上下文的 I/O 失败 (例如"创建数据目录失败: <path>")。
    /// 缺少它时调用方只能看到一个没有线索的 errno, 排障时无从判断
    /// 到底是哪个路径、哪个阶段失败。
    #[error("I/O error: {context} ({source})")]
    IoContext {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("Security error: {0}")]
    Security(String),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Device not trusted: {0}")]
    Untrusted(String),

    #[error("Device not found: {0}")]
    NotFound(String),

    #[error("Network error: {0}")]
    Network(String),

    /// 对端在应答发完之前就关闭了连接（tokio `read_exact` 的 `UnexpectedEof`）。
    ///
    /// ## 为什么必须是**独立的错误类型**而不是塞进 `Io`
    ///
    /// tokio 给这个错误的文案是字面量 `early eof`（见
    /// `tokio/src/io/util/read_exact.rs`），经 `Io` 的 Display 拼出来就是
    /// `"I/O error: early eof"` —— 对用户零信息量。而这一个现象背后至少有
    /// 三件本质不同的事：
    /// - 对方**拒绝了**这次请求（版本不兼容 / 签名不过 / 被拉黑）；
    /// - 对方**根本不是飞梭**（端口被别的服务占着）；
    /// - 中间的路由/覆盖网设备把连接掐了。
    ///
    /// 三种情况用户该做的事完全不同（升级对端 / 换端口 / 查网络），
    /// 而裸 errno 让它们长得一模一样。宿主只能靠字符串匹配
    /// （`"early eof" in err.to_string()`）分支 —— 文案一改就静默失效。
    #[error("对方已关闭连接: {0}")]
    PeerClosed(String),

    /// 对端处于「每次匹配码」等级，本次需要出示传输码（§2.3）。
    ///
    /// ## 必须是**独立的错误类型**而不是塞进 `Security`/`Protocol`
    ///
    /// 宿主要靠它区分两件本质不同的事：
    /// - `GrantCodeRequired` = **正常的协商回合**，等用户抄码后重试即可；
    /// - `Security` = **真的失败了**。
    ///
    /// 混在一个变体里，宿主只能靠字符串匹配（`"匹配码" in err.to_string()`）
    /// 来分支 —— 而文案一改就静默失效，且日志里也没法统计。
    /// 承载的字符串是接收方给的人话提示，直接展示给用户。
    #[error("需要本次传输码: {0}")]
    GrantCodeRequired(String),

    #[error("Checksum mismatch for chunk {chunk_index}: expected {expected}, actual {actual}")]
    ChecksumMismatch {
        chunk_index: u64,
        expected: String,
        actual: String,
    },

    #[error("Transfer cancelled")]
    Cancelled,

    /// 本机在对端真正开传之前就撤销了（§5.2 的 5 秒窗口）。
    ///
    /// 必须是独立变体，不能塞进 `Protocol` / `Security`：
    /// 宿主要靠它区分「我自己点了撤销」和「对方拒绝 / 网络断了」。
    /// 混在一个变体里，界面只能靠字符串匹配分支，文案一改就静默失效，
    /// 用户会看到红色「发送失败」，于是再发一遍刚撤销的文件。
    /// `bytes_sent` 是停下来时已经发出的字节。0 表示一个字节都还没出站。
    /// 历史记录和界面文案靠它区分「没发出」和「传了一半再撤销」，
    /// 不能再从文案里猜。
    #[error("{message}")]
    LocallyAborted { message: String, bytes_sent: u64 },

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, FeisuoError>;

impl FeisuoError {
    /// 给 I/O 错误补上上下文, 例如"创建数据目录失败: C:\...\feisuo"
    pub fn io_context(context: impl Into<String>, source: std::io::Error) -> Self {
        FeisuoError::IoContext {
            context: context.into(),
            source,
        }
    }
}

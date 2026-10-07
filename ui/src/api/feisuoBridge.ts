export interface LocalDeviceInfo {
  device_id: string;
  device_name: string;
  local_ip: string;
  transfer_port: number;
  discovery_port: number;
  receive_dir: string;
  autostart: boolean;
  auto_receive: boolean;
  log_level: string;
  max_log_size_mb: number;
  max_history_records: number;
  record_retention_days: number;
  close_action: "ask" | "tray" | "exit";
  theme: "dark" | "light";
  device_count: number;
  trusted_count: number;
  /** 桌面端当前版本 (Rust 侧 CARGO_PKG_VERSION) */
  app_version: string;
  /** 是否后台静默检查更新 */
  auto_check_update: boolean;
  /**
   * 来源网段白名单（CIDR）。**空数组 = 接受任何来源**。
   *
   * 只接受来自这些网段的连接，**配对也必须过这一关**。
   * 详见 `core/src/security/auth_policy.rs` 的 `subnet_verdict`。
   */
  allowed_peer_subnets: string[];
}

/** 更新流程阶段 (与 Rust 侧 UpdatePhase 一一对应) */
export type UpdatePhase = "idle" | "checking" | "downloading" | "ready" | "uptodate" | "failed";

/** 更新状态快照; 由命令返回, 也通过 feisuo://update-status 事件推送 */
export interface UpdateStatus {
  phase: UpdatePhase;
  message: string;
  currentVersion: string;
  latestVersion: string;
  tagName: string;
  pendingPath: string;
  bytesReceived: number;
  bytesTotal: number;
  /** 手动下载页 (自动下载失败时的兜底) */
  releasePage: string;
  /** 检查或下载进行中 —— 前端据此禁用按钮 */
  busy: boolean;
}

export interface TransferRecord {
  id: number;
  file_name: string;
  file_size: number;
  file_size_formatted: string;
  direction: "send" | "recv";
  peer_name: string;
  peer_ip: string;
  status: "completed" | "failed";
  created_at: number;
  time_formatted: string;
  // ---- v2：速度度量的原始量（速度是派生值，由后端算好）----
  /** 清单声明的总字节（失败时可能大于实际传输量） */
  declared_size: number;
  /** 纯数据流耗时（毫秒）—— 速度的唯一分母 */
  duration_active_ms: number;
  /** 墙钟耗时（含审批等待） */
  duration_wall_ms: number;
  /** 整文件 BLAKE3 复核耗时（不计入速度） */
  duration_verify_ms: number;
  /** 是否走覆盖网（ZeroTier / Tailscale 的 100.64/10） */
  over_overlay: boolean;
  /** 两端 TCP 建连耗时（毫秒） */
  connect_ms: number;
  /** 后端算好的速度文本；样本不足时为 "—" */
  speed_display: string;
}

/**
 * 一条诊断记录（`get_transfer_diagnostics` 的返回形状）。
 *
 * 由后端裁剪过：core 的 `TransferDiagnostics` 里还有 nonce、采样点、
 * 分块数等内部量，全塞过来只会让人不知道该看哪个。
 */
export interface TransferDiagnostic {
  direction: string;
  peer_name: string;
  peer_ip: string;
  /** `completed` / `failed` / `cancelled` */
  outcome: string;
  error: string | null;
  file_count: number;
  bytes_transferred: number;
  /**
   * 归因：core 算好的人话，界面**原样显示**。
   *
   * 刻意不在前端重新推导：同样的输入在「导出诊断报告」里已经有
   * 同一套判定，两处各算一次必然出现说法不一致，而用户看到两个地方
   * 讲得不一样，就两个都不信。
   */
  attribution: string;
  speed_display: string;
  /** 纯数据流耗时（毫秒） */
  data_ms: number;
  connect_ms: number;
  /** 人工审批等待 —— 唯一"不是程序慢"的原因，单独显示 */
  approval_wait_ms: number;
  /** 整文件 BLAKE3 复核（不计入速度） */
  verify_ms: number;
  over_overlay: boolean;
  created_at: number;
}

export interface DiscoveredDevice {
  device_id: string;
  device_name: string;
  os_type: string;
  ip: string;
  transfer_port: number;
  is_trusted: boolean;
  last_seen_secs: number;
  /** 对端能力位（§7.3）。0 = 对端未声明能力（老客户端或字段缺失） */
  caps: number;
  /** 对端应用版本（仅展示与诊断） */
  app_version: string;
}

/** 设备信任等级（后端 `TrustLevel`，snake_case 序列化） */
export type TrustLevel = "pending" | "permanent" | "session";

/** 解除配对的结果（§14.5）。界面必须按 `reason_code` 分支，不要匹配文案。 */
export interface UnpairReport {
  local_applied: boolean;
  peer_notified: boolean;
  reason_code: string;
  user_message: string;
}

/** 设备在线状态（后端 `Presence`） */
export type Presence = "online" | "reconnecting" | "offline" | "hidden";

/**
 * 设备名册条目 —— **侧栏应消费这个而不是 `getOnlineDevices`**。
 *
 * 后者的在线表只有 20 秒 TTL，配对过的设备离线就会从列表彻底消失，
 * 用户无法区分"它关机了"与"发现服务被防火墙挡了"（后者才需要报警）。
 */
export interface DeviceRosterEntry {
  device_id: string;
  device_name: string;
  os_type: string;
  trust_level: TrustLevel;
  visible: boolean;
  presence: Presence;
  last_seen_at: number;
  /** "3 分钟前" / "未知"（旧库没有该字段时不得显示成"刚刚"） */
  last_seen_human: string;
  ip: string | null;
  transfer_port: number | null;
  last_ip: string;
  is_paired: boolean;
  is_self: boolean;
  /**
   * 对端能力位（§7.3）。离线设备为 0 —— 上次在线时的能力不代表它现在
   * 还支持，所以不能从信任库里猜。0 = 对端未声明能力。
   *
   * 穿梭右栏据此决定初始模式：`BROWSE_VOLUMES`（1 << 3）置位时直接进
   * 真实卷模式，不做"先试后降"（试错会让列表闪一下再弹报错）。
   */
  peer_caps: number;
  /** 对端应用版本（仅展示与诊断） */
  peer_version: string;
}

/** 能力位常量（与 `core/src/protocol.rs` 的 `caps` 模块一一对应） */
export const CAPS = {
  TRANSFER_BASIC: 1 << 0,
  BROWSE: 1 << 1,
  PULL: 1 << 2,
  /** 真实卷浏览（`volume` + 绝对路径 + 分页）—— 2.x 才有 */
  BROWSE_VOLUMES: 1 << 3,
  FOLDER_TRANSFER: 1 << 4,
  PULL_DEST: 1 << 5,
  ACCESS_SCOPE: 1 << 6,
  SESSION_GRANT: 1 << 7,
  CANCEL: 1 << 8,
} as const;

/** 可访问范围（后端 `AccessScope`） */
export interface AccessScope {
  /** all | allowlist | receive_only —— 默认 all（D1） */
  mode: "all" | "allowlist" | "receive_only";
  allow_volumes: string[];
  allow_paths: string[];
  deny_paths: string[];
  can_pull: boolean;
  /** 默认 false（D2：写入与读取权限解耦） */
  can_push: boolean;
  updated_at: number;
}

/** 剪贴板内容分类（后端 `ClipboardContent`，serde tag="kind"） */
export type ClipboardContent =
  | { kind: "files"; paths: string[] }
  | { kind: "image"; file_name: string; size: number; preview_base64: string }
  | { kind: "text"; file_name: string; size: number; preview: string }
  | { kind: "rejected"; reason: string }
  | { kind: "empty" };

export interface SecurityEvent {
  id: number;
  kind: string;
  peer_device_id: string;
  peer_name: string;
  detail: string;
  created_at: number;
  time_formatted: string;
}

export interface TrustedDevice {
  device_id: string;
  device_name: string;
  public_key_hex: string;
  last_ip: string;
  bound_at: string;
  is_trusted: boolean;
}

export interface DiskFileInfo {
  name: string;
  is_dir: boolean;
  size: number;
  size_formatted: string;
  modified: string;
}

/** 一次本机目录列举的完整结果 (左栏面包屑导航用) */
export interface LocalDirListing {
  files: DiskFileInfo[];
  /** 当前相对路径, 空串 = 卷根 / 收件根 */
  current_path: string;
  /** 上一级; 已在根目录时为 null */
  parent_path: string | null;
  /** 还有更多条目（分页）—— 必须提示, 否则用户以为文件丢了 */
  truncated: boolean;
  /** 本目录总条目数 */
  total: number;
  /** 本次返回的偏移 */
  offset: number;
  /** 是否走真实卷模式（false = 收件目录镜像） */
  volume_mode: boolean;
  /** 本次使用的卷 id */
  volume: string;
  /** 本机可浏览的卷（地址栏下拉的数据源） */
  volumes: VolumeInfo[];
}

export interface RemoteFileEntry {
  name: string;
  is_dir: boolean;
  size: number;
  size_formatted: string;
  modified: string;
}

/** 本机浏览目标（与对端同构，§7.1） */
export interface LocalBrowseTarget {
  /** 卷 id（`C:`）。空串 = 收件目录镜像（兼容模式） */
  volume: string;
  /** 卷内相对路径，`/` 分隔。空串 = 卷根。 */
  rel_path: string;
  offset: number;
  /** 0 = 用服务端默认单页条数 */
  limit: number;
}

/** 对端开放的一个可浏览卷（地址栏下拉的数据源，§7.2） */
export interface VolumeInfo {
  /** Windows `"C:"` / Android `"internal"` */
  id: string;
  label: string;
  total_bytes: number;
  free_bytes: number;
  readable: boolean;
}

/**
 * 一个常用位置（桌面 / 下载 / 文档…）。
 *
 * `key` 是**稳定语义标识**（`desktop`），界面文案固定中文。
 * 两者分开是因为**实际目录名会本地化** —— 中文 Windows 里真路径叫
 * `文档`，不是 `Documents`；按 `label` 或路径去匹配一定会漏。
 *
 * `volume` + `rel_path` 是**后端预先拆好**的：拆分规则是平台相关的
 * （盘符 / SAF 卷 / 根目录），放到前端就等于用 TypeScript 把同一套规则
 * 再写一遍，早晚和 Rust 侧漂移成"点桌面跳进了 C:\ 根目录"。
 */
export interface KnownPlace {
  key: string;
  label: string;
  volume: string;
  rel_path: string;
  /** 绝对路径，仅用于 title 悬浮提示；导航不依赖它 */
  path: string;
}

/** 穿梭浏览目标：卷 + 卷内相对路径 + 分页游标（§7.2 / §7.3） */
export interface BrowseTarget {
  /**
   * 浏览的**根**：
   * - 空串 = 根是本机的收件目录（`receive_dir` 配的那个路径）；
   * - `"C:"` / `"D:"` = 根是那个真实卷；
   * - `"*"` = 让服务端按访问范围挑一个可读卷（客户端不该猜盘符）。
   *
   * 三者都是**正常用法**，没有哪个是"兼容模式"：区别只是根在哪。
   * 后端 `resolve_browse_dir` / `resolve_browse_path` 两条分支共用同一套
   * 路径归一化与逃逸校验，所以行为一致。
   */
  volume: string;
  /** 卷内相对路径，`/` 分隔。空串 = 卷根。 */
  rel_path: string;
  offset: number;
  /** 0 = 用服务端默认单页条数 */
  limit: number;
  /**
   * 本次传输码（对端处于「每次匹配码」等级时必填，§2.3）。
   * 浏览也是对端发起的操作，所以与传输同规则：发起方出码、对方核对。
   */
  grant_code: string;
}

/** 卷通配符（服务端会挑一个 scope 内的可读卷并回显真实卷 id） */
export const VOLUME_ANY = "*";

/** 一次目录浏览的完整结果 (右栏面包屑导航用) */
export interface RemoteBrowseListing {
  files: RemoteFileEntry[];
  /** 当前相对路径, 空串 = 卷根 / 落盘根 */
  current_path: string;
  /** 上一级; 已在根目录时为 null */
  parent_path: string | null;
  /** 还有更多条目（分页）—— 必须提示, 否则用户以为文件丢了 */
  truncated: boolean;
  message: string;
  // ---- v2：真实文件系统 ----
  /** 对端开放的可浏览卷。为空数组 = 对端没开放任何卷
   *  （访问范围是「仅收件目录」或白名单为空）——
   *  前端据此隐藏盘符下拉，而不是画一个点了就报错的空下拉框。 */
  volumes: VolumeInfo[];
  /**
   * 对端的常用位置（桌面 / 下载 / 文档…），**已按该对端的访问范围过滤过**。
   *
   * 空数组 = 对端没有可用的（受限，或非 Windows 平台）。
   * 界面据此隐藏这一组，而不是画一个必然点不动的入口 ——
   * 受限时若照发，对方还能看到这台机器的用户目录名，
   * 那等于泄露了对方的系统用户名。
   *
   * 本机那一份**刻意不走这里**：本机的常用位置是启动时读一次的静态值
   * （`getKnownPlaces`），没必要每次列目录都重复回一遍。
   */
  places: KnownPlace[];
  /** 本目录总条目数（分页用） */
  total: number;
  /** 本次返回的偏移 */
  offset: number;
  /** 对端是否走真实卷模式 */
  volume_mode: boolean;
  /** 实际使用的卷 id（请求发的是通配 `*` 时由服务端回显真实卷） */
  volume: string;
  /** 对端能力位（诊断用） */
  peer_caps: number;
}

export interface ApprovalRequest {
  approval_id: string;
  sender_id: string;
  sender_name: string;
  sender_ip: string;
  file_count: number;
  total_size: number;
  total_size_formatted: string;
  first_file_name: string;
  created_at: number;
  /**
   * 对端处于「每次匹配码」等级：本次审批**必须**由发起方出示匹配码。
   *
   * 界面在这个值为 true 时**必须**显示 `grant_challenge`（本机生成的码），
   * 提示用户念给对方 —— 审批人自己什么都不用输入。
   */
  requires_grant_code: boolean;
  /**
   * **本机生成、显示给本机用户**的 6 位匹配码；空串 = 本次不需要码。
   *
   * 它只随这条 `ApprovalRequest` 走到本机界面，**绝不能**出现在任何
   * 回给对端的帧里 —— 一旦发出去，发起方就能自动填上，
   * "对方屏幕前得有人"这件事就不存在了。
   */
  grant_challenge: string;
}

/** 目录展开预览的结果（§7.6） */
export interface SendPreview {
  /** 展开后实际会发送的文件数 */
  fileCount: number;
  totalBytes: number;
  /** 跳过的条目总数（隐藏项 + 内部目录 + 符号链接 + 超限） */
  skipped: number;
  /** 其中符号链接的数量（单独给出，因为"不跟随"是个需要解释的决定） */
  symlinksSkipped: number;
  /** 触发的限制说明；`null` = 一切正常 */
  limitHit: string | null;
  /**
   * 拖入的路径里**有没有目录**。
   *
   * UI 用它决定"直接发出"还是"先展开预览再确认"。
   * 必须精确 —— 靠"拖入数 vs 展开后文件数"推断会把**只含 1 个文件的文件夹**
   * 误判成普通文件，而那正是要保护用户、不该静默直发的场景。
   */
  hasDirectory: boolean;
  /** 前若干条目标相对路径（样例，不是全部） */
  sample: string[];
  /**
   * **按拖入路径**分组的展开统计。
   *
   * 没有它，待发清单就只能显示目录项自身的大小（NTFS 上是 4.0 KB），
   * 于是同一件事出现两个数字：提示说"7 个文件 17.8 MB"，清单写"1 项 4.0 KB"。
   * 用户只看得见清单那个 —— 那就是在撒谎。
   */
  expanded: RootExpansion[];
}

/** 单个拖入路径的展开结果（Rust 侧 `RootExpansion`） */
export interface RootExpansion {
  /** 被拖入的原始路径 */
  path: string;
  /** 这一项展开后实际会发送的文件数（普通文件恒为 1） */
  fileCount: number;
  /** 这一项展开后的总字节数 */
  totalBytes: number;
  /** 原始路径是不是目录 —— 清单据此显示「N 个文件」而不是「4.0 KB」 */
  isDir: boolean;
  skipped: number;
  symlinksSkipped: number;
}

/** 设备端点（多地址发现 / 路径选择，§3.9 / P4 ⑧） */
export interface DeviceEndpoint {
  device_id: string;
  ip: string;
  /** 传输端口；`0` = 未知（老库只有裸 IP） */
  port: number;
  /** `"overlay"`（ZeroTier 等虚拟网卡）/ `"lan"` / `"public"` / `"unknown"` */
  kind: string;
  first_seen: number;
  last_seen: number;
  /** 是否真的在这个地址上收到过签名信标 */
  verified: boolean;
}

/** 发送结果（断点续传 + 「每次匹配码」协商 + 目录展开，§2.3 / §7.6 / P1 ⑪） */
export interface SendOutcome {
  /** 因断点续传而跳过的文件数 */
  skipped: number;
  /**
   * 对端处于「每次匹配码」等级，需要用户抄码后重试。
   * 这是**一次正常的协商回合**，不是失败 —— 待发队列必须保留。
   */
  grant_code_required: boolean;
  message: string;
  /**
   * **实际发出的文件数**（目录已递归展开）。
   *
   * 必须与"选中了几个"分开：选 1 个含 3000 个文件的文件夹时，
   * "已发送 1 个文件"是彻头彻尾的谎报。
   */
  expanded_files: number;
  /** 展开时跳过的条目数（符号链接 / 隐藏 / 内部目录 / 超限） */
  expand_skipped: number;
  expand_symlinks_skipped: number;
  expand_limit_hit: string | null;
}

/** 一张生效中的短期授权（§2.3.1）。 */
export interface ActiveGrant {
  /** 操作类别：`receive` / `browse` / `pull`。**按类别隔离**，不互通。 */
  scope: string;
  /** 展示用的类别名（服务端给，避免前端各处硬编码中文映射表）。 */
  scope_label: string;
  /** 过期时间（epoch 秒）。到点自动失效，不需要任何后台任务。 */
  expires_at: number;
}

export interface TransferProgress {
  transfer_id: string;
  direction: "Send" | "Receive";
  peer_device_id: string;
  peer_device_name: string;
  current_file: string;
  file_index: number;
  total_files: number;
  bytes_transferred: number;
  total_bytes: number;
  progress_percent: number;
  speed_bytes_per_sec: number;
  /** Rust 侧 enum: 单元变体为字符串, Failed(String) 会被序列化成 {"Failed":"原因"} */
  status: "Queued" | "Transferring" | "Completed" | "Failed" | "Cancelled" | { Failed: string };
}

/** 解析传输状态, 统一返回 { kind, reason } */
export function parseTransferStatus(
  status: TransferProgress["status"]
): { kind: "Queued" | "Transferring" | "Completed" | "Failed" | "Cancelled"; reason: string } {
  if (typeof status === "string") return { kind: status, reason: "" };
  if (status && typeof status === "object" && "Failed" in status) {
    return { kind: "Failed", reason: String((status as { Failed: string }).Failed ?? "") };
  }
  return { kind: "Transferring", reason: "" };
}

const isTauri = typeof window !== "undefined" && ("__TAURI_INTERNALS__" in window || "__TAURI__" in window);

/** 把后端返回的错误转成可读文案 */
function toMessage(e: unknown, fallback: string): string {
  if (typeof e === "string") return e;
  if (e && typeof e === "object" && "message" in e) return String((e as { message: unknown }).message);
  return fallback;
}

/** 只有在"浏览器预览模式"下才回落到演示数据; Tauri 环境下失败必须抛错,
 *  否则会把假数据当真实结果显示出来 (旧实现正是这么做的)。 */
function notInTauri(): boolean {
  return !isTauri;
}

async function invokeSafe<T>(cmd: string, args?: Record<string, unknown>, fallback?: () => T): Promise<T> {
  if (!isTauri) return fallback!();
  const { invoke } = await import("@tauri-apps/api/core");
  return await invoke<T>(cmd, args);
}

export class FeisuoBridge {
  static get runningInTauri(): boolean {
    return isTauri;
  }

  // ------------------------------------------------------------------ 本机信息
  static async getLocalInfo(): Promise<LocalDeviceInfo> {
    if (notInTauri()) {
      return {
        device_id: "feisuo-local-win",
        device_name: "书房台式 (本机)",
        local_ip: "192.168.31.120",
        transfer_port: 42100,
        discovery_port: 42101,
        receive_dir: "C:\\Users\\Chen\\feisuo",
        autostart: true,
        auto_receive: true,
        log_level: "INFO",
        max_log_size_mb: 5,
        max_history_records: 500,
        record_retention_days: 30,
        close_action: "ask",
        theme: "dark",
        device_count: 1,
        trusted_count: 1,
        app_version: "0.1.0",
        auto_check_update: true,
        // 浏览器预览态：给一个合理的示例值，让设置页那段 UI 能被看到
        allowed_peer_subnets: ["192.168.31.0/24"],
      };
    }
    return invokeSafe<LocalDeviceInfo>("get_local_info");
  }

  /**
   * 本机常用位置（桌面 / 下载 / 文档…）。
   *
   * **不要在前端拼 `USERPROFILE + "\\Desktop"`** —— 桌面/文档/图片常被
   * 重定向到 OneDrive（真路径是 `C:\Users\<u>\OneDrive\桌面`），
   * 且目录名本地化（中文系统叫 `文档`）。拼错的表现是"点了没反应"，
   * 界面毫无线索。
   */
  static async getKnownPlaces(): Promise<KnownPlace[]> {
    if (notInTauri()) {
      // 浏览器预览态：给几条示例，让这段 UI 能被看到
      return [
        { key: "desktop", label: "桌面", volume: "C:", rel_path: "Users/Chen/Desktop", path: "C:\\Users\\Chen\\Desktop" },
        { key: "downloads", label: "下载", volume: "C:", rel_path: "Users/Chen/Downloads", path: "C:\\Users\\Chen\\Downloads" },
        { key: "documents", label: "文档", volume: "C:", rel_path: "Users/Chen/Documents", path: "C:\\Users\\Chen\\Documents" },
        { key: "home", label: "用户目录", volume: "C:", rel_path: "Users/Chen", path: "C:\\Users\\Chen" },
      ];
    }
    return invokeSafe<KnownPlace[]>("get_known_places");
  }

  static async getOnlineDevices(): Promise<DiscoveredDevice[]> {
    if (notInTauri()) {
      return [
        {
          device_id: "feisuo-phone-01",
          device_name: "随身 Pixel (安卓手机)",
          os_type: "Android",
          ip: "192.168.31.88",
          transfer_port: 42100,
          is_trusted: true,
          last_seen_secs: Math.floor(Date.now() / 1000),
          caps: 0xff,
          app_version: "2.0.0",
        },
      ];
    }
    return invokeSafe<DiscoveredDevice[]>("get_online_devices", undefined, () => []);
  }

  /**
   * 设备名册（在线表 ∪ 信任库）。
   *
   * **侧栏必须用这个而不是 `getOnlineDevices`**：后者只有 20 秒 TTL，
   * 配对过的设备离线就会从列表消失（§3.4）。
   */
  static async getDeviceRoster(): Promise<DeviceRosterEntry[]> {
    if (notInTauri()) return [];
    return invokeSafe<DeviceRosterEntry[]>("get_device_roster", undefined, () => []);
  }

  static async setDeviceVisible(deviceId: string, visible: boolean): Promise<boolean> {
    return invokeSafe<boolean>("set_device_visible", { deviceId, visible }, () => true);
  }

  static async setDeviceTrustLevel(deviceId: string, level: TrustLevel): Promise<boolean> {
    return invokeSafe<boolean>("set_device_trust_level", { deviceId, level }, () => true);
  }

  static async getHiddenDevices(): Promise<TrustedDevice[]> {
    return invokeSafe<TrustedDevice[]>("get_hidden_devices", undefined, () => []);
  }

  static async getSecurityEvents(limit?: number): Promise<SecurityEvent[]> {
    return invokeSafe<SecurityEvent[]>("get_security_events", { limit: limit ?? null }, () => []);
  }

  static async getAccessScope(deviceId: string): Promise<AccessScope> {
    return invokeSafe<AccessScope>("get_device_access_scope", { deviceId }, () => ({
      mode: "all",
      allow_volumes: [],
      allow_paths: [],
      deny_paths: [],
      can_pull: true,
      // 与 core 的 `AccessScope::default()` 保持一致：允许写入，
      // 落点由接收端强制落在收件目录。之前这里是 false，与后端默认值
      // 相反 —— 非 Tauri 环境（浏览器预览）下会表现为"能看不能收"。
      can_push: true,
      updated_at: 0,
    }));
  }

  static async setAccessScope(deviceId: string, scope: AccessScope): Promise<boolean> {
    return invokeSafe<boolean>("set_device_access_scope", { deviceId, scope }, () => true);
  }

  /** 导出传输诊断报告，返回生成的文件路径（§9.7）。 */
  static async exportTransferDiagnostics(limit?: number): Promise<string> {
    return invokeSafe<string>("export_transfer_diagnostics", { limit: limit ?? null }, () => "");
  }

  // ------------------------------------------------------------------ 剪贴板（§6）
  /**
   * 请求对端丢弃本机发起的某次传输（直发撤销，§5.2）。
   *
   * 语义是"**尽力而为**"：如果对端已经开始把数据落盘，就撤不掉了 ——
   * 这一点必须在界面上说清楚，不能假装撤销一定成功。
   */
  static async cancelIncomingTransfer(
    peerDeviceId: string,
    transferId?: string
  ): Promise<boolean> {
    return invokeSafe<boolean>(
      "cancel_incoming_transfer",
      { peerDeviceId, transferId: transferId ?? null },
      () => false
    );
  }

  /**
   * 读剪贴板并返回预览（**不落盘**）。
   *
   * D5：预览默认开启。所以拆成两步 —— 先预览、用户确认后再
   * [`stageClipboardPayload`] 落盘。"看了没发"不该在磁盘上留明文。
   */
  static async readClipboardPreview(): Promise<ClipboardContent> {
    return invokeSafe<ClipboardContent>("read_clipboard_preview", undefined, () => ({
      kind: "empty",
    }));
  }

  /** 把剪贴板内容落盘，返回可直接发送的绝对路径列表。 */
  static async stageClipboardPayload(): Promise<string[]> {
    return invokeSafe<string[]>("stage_clipboard_payload", undefined, () => []);
  }

  /** 清理超期剪贴板暂存文件（默认 24h）。 */
  static async purgeClipboardStaging(ttlHours?: number): Promise<number> {
    return invokeSafe<number>("purge_clipboard_staging", { ttlHours: ttlHours ?? null }, () => 0);
  }

  /** 发送成功后精确删除暂存文件（不能全量清空，会误删排队中的内容）。 */
  static async cleanupClipboardStaging(paths: string[]): Promise<number> {
    return invokeSafe<number>("cleanup_clipboard_staging", { paths }, () => 0);
  }

  static async getTrustedDevices(): Promise<TrustedDevice[]> {
    return invokeSafe<TrustedDevice[]>("get_trusted_devices", undefined, () => []);
  }

  /**
   * 解除配对（§14.5）：**双向**。这是界面唯一的解除配对入口。
   *
   * ❌ 早先这里还有一个 `removeTrustedDevice` → `remove_trusted_device`
   * 命令（纯本地 `DELETE`，不通知对端、且连「隐藏」偏好一起删）。
   * 它**已整体删除**，三个理由见
   * `desktop/src-tauri/src/commands.rs` 里那段说明。简要说：
   * 界面不用它、Android 侧没接，于是它是一个**已注册但无调用方**的
   * 命令 —— 而 WebView 里任何 JS 都能调到它，调到的正是
   * §14.5 要消灭的「单方面断开」。更糟的是它当时还是
   * 「找不到对方地址」那条兜底分支的实现，于是**有没有 IP
   * 决定了行是被 UPDATE 还是被 DELETE**。
   *
   * `reason_code` 是机器可读原因，界面据此决定提示措辞 ——
   * **不要**去 `user_message` 里 `includes()` 判断，那正是本轮在
   * core 里刚拆掉的反模式（`DenyCode` / `requires_grant_code`）。
   */
  static async unpairDevice(deviceId: string): Promise<UnpairReport> {
    return invokeSafe<UnpairReport>("unpair_device", { deviceId }, () => ({
      local_applied: true,
      peer_notified: false,
      reason_code: "dev_stub",
      user_message: "开发模式：未通知对端",
    }));
  }

  /** 主动探测: 真正等待对端应答, 失败会抛出可读原因 */
  static async probeDevice(targetIp: string): Promise<DiscoveredDevice> {
    if (notInTauri()) {
      return {
        device_id: "feisuo-dev-" + Math.floor(Math.random() * 1000),
        device_name: "直连设备 (" + targetIp + ")",
        os_type: "Windows",
        ip: targetIp,
        transfer_port: 42100,
        is_trusted: false,
        last_seen_secs: Math.floor(Date.now() / 1000),
        caps: 0xff,
        app_version: "2.0.0",
      };
    }
    return invokeSafe<DiscoveredDevice>("probe_device", { targetIp });
  }

  static async pairWithDevice(targetIp: string, targetPort: number, pin: string): Promise<TrustedDevice> {
    if (notInTauri()) {
      return {
        device_id: "feisuo-dev-" + Math.floor(Math.random() * 1000),
        device_name: "新配对设备 (" + targetIp + ")",
        public_key_hex: "mock_pubkey_hex",
        last_ip: targetIp,
        bound_at: new Date().toISOString(),
        is_trusted: true,
      };
    }
    return invokeSafe<TrustedDevice>("pair_with_device", { targetIp, targetPort, pin });
  }

  static async generatePairPin(): Promise<string> {
    if (notInTauri()) return "482917";
    return invokeSafe<string>("generate_pair_pin");
  }

  // ------------------------------------------------------------------ 文件发送
  /**
   * 发送文件。
   *
   * `grantCode` 是本次传输码（§2.3）。第一次省略；
   * 收到 `grant_code_required: true` 后，展示自己生成的 6 位码让用户
   * 抄到对端屏幕，再带上码重试本方法。
   *
   * 返回里的 `skipped` 必须展示给用户：拖了 10 个只传 2 个而界面只说
   * "成功"，用户会以为另外 8 个丢了并反复重传。
   */
  static async sendFiles(
    targetDeviceId: string,
    targetIp: string,
    targetPort: number,
    targetName: string,
    files: string[],
    grantCode = "",
    destSubPath = ""
  ): Promise<SendOutcome> {
    if (notInTauri()) {
      console.log(`[Bridge] 模拟发送到 ${targetName} (${targetIp}):`, files);
      return { skipped: 0, grant_code_required: false, message: "", expanded_files: 0, expand_skipped: 0, expand_symlinks_skipped: 0, expand_limit_hit: null };
    }
    return invokeSafe<SendOutcome>(
      "send_files",
      {
        targetDeviceId,
        targetIp,
        targetPort,
        targetName,
        files,
        grantCode: grantCode || null,
        destSubPath: destSubPath || null,
      },
      () => ({ skipped: 0, grant_code_required: false, message: "", expanded_files: 0, expand_skipped: 0, expand_symlinks_skipped: 0, expand_limit_hit: null })
    );
  }

  /**
   * ~~生成一个 6 位传输码~~ —— **已删除**。
   *
   * 码现在由**接收方**（Rust 侧 `ApprovalManager::challenge_for`）生成，
   * 前端不再有任何生成路径。
   *
   * 留着这个函数是个陷阱：任何"顺手在前端生成一个码"的调用都会把
   * 方向悄悄改回"发起方出码"，而那种方向下这道门等于没锁 ——
   * 而且**功能上看起来完全正常**，只有安全模型是错的。
   * 所以这里是删掉而不是标注废弃。
   */

  /** 使用系统原生文件选择框, 返回真实磁盘路径 */
  static async pickFilesForSend(): Promise<string[]> {
    if (notInTauri()) return [];
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({
      multiple: true,
      directory: false,
      title: "选择要发送的文件",
    });
    if (!picked) return [];
    return Array.isArray(picked) ? picked : [picked];
  }

  /** 把内存中的字节暂存成真实文件, 返回其绝对路径 */
  static async stageTempPayload(fileName: string, bytes: number[] | Uint8Array): Promise<string> {
    return invokeSafe<string>(
      "stage_temp_payload",
      { fileName, bytes: Array.from(bytes) },
      () => ""
    );
  }

  /**
   * 清理暂存文件。
   * `paths` 传入本次实际提交的暂存文件路径时只删这些; 省略时才清空整个目录。
   * 必须按路径精确删除: 暂存目录里可能还有用户排队等待发送的剪贴板文件。
   */
  static async cleanupTempPayloads(paths?: string[]): Promise<boolean> {
    return invokeSafe<boolean>(
      "cleanup_temp_payloads",
      { paths: paths ?? null },
      () => true
    );
  }

  // ------------------------------------------------------------------ 目录
  /**
   * 列举本机目录 (穿梭左栏)。
   *
   * `volume` 非空 = 走真实文件系统（`D:` / `项目/2026`），与右栏同构；
   * 留空 = 收件目录镜像。
   *
   * 与对端浏览**刻意不同**：本机浏览不套用 `access_scope`。
   * scope 是"我允许别人看什么"，与"我看自己的盘"无关 ——
   * 套上去会让用户设了白名单 `D:` 之后连 C 盘都看不到，
   * 而那本机文件他本来就有完整权限。
   */
  static async listDirectoryFiles(
    target: LocalBrowseTarget
  ): Promise<LocalDirListing> {
    const relPath = target.rel_path;
    if (notInTauri()) {
      const volumes: VolumeInfo[] = target.volume
        ? [
            { id: "C:", label: "本地磁盘 (C:)", total_bytes: 512e9, free_bytes: 208e9, readable: true },
            { id: "D:", label: "资料盘 (D:)", total_bytes: 1e12, free_bytes: 640e9, readable: true },
          ]
        : [];
      const base = {
        total: 3,
        offset: target.offset,
        volume_mode: !!target.volume,
        volume: target.volume,
        volumes,
        // 原型模式没有对端常用位置（真实运行由服务端按 scope 过滤后下发）
        places: [],
      };
      if (relPath === "") {
        return {
          ...base,
          files: [
            { name: "项目架构规范与部署指南.pdf", is_dir: false, size: 19503513, size_formatted: "18.6 MB", modified: "2026-09-28 11:20" },
            { name: "飞梭跨端核心引擎设计草案.docx", is_dir: false, size: 1258291, size_formatted: "1.2 MB", modified: "2026-09-28 10:14" },
            { name: "素材与设计稿", is_dir: true, size: 0, size_formatted: "文件夹", modified: "2026-09-27 16:30" },
          ],
          current_path: "",
          parent_path: null,
          truncated: false,
        };
      }
      return {
        ...base,
        files: [
          { name: "封面_横版.png", is_dir: false, size: 2190443, size_formatted: "2.1 MB", modified: "2026-09-26 10:03" },
        ],
        current_path: relPath,
        parent_path: relPath.includes("/") ? relPath.slice(0, relPath.lastIndexOf("/")) : "",
        truncated: false,
      };
    }
    return invokeSafe<LocalDirListing>(
      "list_directory_files",
      {
        volume: target.volume || null,
        relPath: target.rel_path,
        offset: target.offset,
        limit: target.limit,
      },
      () => ({
        files: [],
        current_path: relPath,
        parent_path: null,
        truncated: false,
        total: 0,
        offset: target.offset,
        volume_mode: false,
        volume: "",
        volumes: [],
      })
    );
  }

  /**
   * 浏览对端目录 (双栏穿梭右栏), 仅已配对设备可用。
   *
   * `target.volume` 非空 = 根是那个真实卷（`C:` / `项目/2026`）；
   * 留空 = 根是对端的收件目录（`receive_dir`）。
   * `offset` / `limit` = 分页游标。
   */
  static async listRemoteFiles(
    targetIp: string,
    targetPort: number,
    target: BrowseTarget,
  ): Promise<RemoteBrowseListing> {
    const relPath = target.rel_path;
    if (notInTauri()) {
      // 原型模式: 根目录给示例文件, 子目录给一个更小的示例集。
      // 卷模式下给两个假卷, 让地址栏下拉在原型里也能看到。
      const volumes: VolumeInfo[] = target.volume
        ? [
            { id: "C:", label: "本地磁盘 (C:)", total_bytes: 512e9, free_bytes: 208e9, readable: true },
            { id: "D:", label: "资料盘 (D:)", total_bytes: 1e12, free_bytes: 640e9, readable: true },
          ]
        : [];
      const base = {
        volumes,
        // 原型模式没有对端常用位置（真实运行由服务端按 scope 过滤后下发）
        places: [],
        total: 3,
        offset: target.offset,
        volume_mode: !!target.volume,
        // 原型模式：`*` 假装挑了第一个卷
        volume: target.volume === VOLUME_ANY ? "C:" : target.volume,
        peer_caps: target.volume ? 0xff : 0x07,
      };
      if (relPath === "") {
        return {
          ...base,
          files: [
            { name: "IMG_2026_0928_Live.jpg", is_dir: false, size: 4404019, size_formatted: "4.2 MB", modified: "09-28 11:42" },
            { name: "VID_20260927_4K.mp4", is_dir: false, size: 673185792, size_formatted: "642 MB", modified: "09-27 21:10" },
            { name: "素材与设计稿", is_dir: true, size: 0, size_formatted: "文件夹", modified: "09-27 16:30" },
          ],
          current_path: "",
          parent_path: null,
          truncated: false,
          message: "OK",
        };
      }
      return {
        ...base,
        files: [
          { name: "封面_横版.png", is_dir: false, size: 2190443, size_formatted: "2.1 MB", modified: "09-26 10:03" },
        ],
        current_path: relPath,
        parent_path: relPath.includes("/") ? relPath.slice(0, relPath.lastIndexOf("/")) : "",
        truncated: false,
        message: "OK",
      };
    }
    return invokeSafe<RemoteBrowseListing>(
      "list_remote_files",
      {
        targetIp,
        targetPort,
        volume: target.volume,
        relPath: target.rel_path,
        offset: target.offset,
        limit: target.limit,
        grantCode: target.grant_code || null,
      },
      () => ({
        files: [],
        current_path: relPath,
        parent_path: null,
        truncated: false,
        message: "",
        volumes: [],
        places: [],
        total: 0,
        offset: target.offset,
        volume_mode: false,
        volume: "",
        peer_caps: 0,
      }),
    );
  }

  /**
   * 请求对端把指定文件推送回本机 (双栏穿梭"取回")。
   * `subPaths` 每项是相对落盘根目录的路径, **可以含子目录层级**。
   * `destSubPath` 指定本机落点子目录（§7.7），空串 = 落收件根。
   * `grantCode` 是本次传输码（对端处于「每次匹配码」等级时必填，§2.3）。
   *
   * 返回 `grant_code_required: true` 时**选中项必须保留** ——
   * 那不是失败，是一次协商回合。
   */
  static async requestPull(
    targetIp: string,
    targetPort: number,
    subPaths: string[],
    destSubPath = "",
    grantCode = "",
    volume = ""
  ): Promise<SendOutcome> {
    if (notInTauri()) {
      return { skipped: 0, grant_code_required: false, message: "", expanded_files: 0, expand_skipped: 0, expand_symlinks_skipped: 0, expand_limit_hit: null };
    }
    return invokeSafe<SendOutcome>(
      "request_pull",
      {
        targetIp,
        targetPort,
        subPaths,
        destSubPath,
        grantCode: grantCode || null,
        volume: volume || null,
      },
      () => ({ skipped: 0, grant_code_required: false, message: "", expanded_files: 0, expand_skipped: 0, expand_symlinks_skipped: 0, expand_limit_hit: null })
    );
  }

  /**
   * 预览一批路径（文件或文件夹）会被展开成什么（§7.6）。
   *
   * **必须在入待发清单之前调用**：目录递归会跳过符号链接、隐藏文件、
   * `.git` / `node_modules` 等内部目录。用户拖一个含 `.git` 的项目进去
   * expecting 完整同步，结果只收到 20 个文件 —— 而界面上一个字的
   * 解释都没有。预览让用户在按发送之前就知道会发什么、跳过什么。
   */
  static async previewSendPaths(paths: string[]): Promise<SendPreview> {
    if (notInTauri()) {
      return {
        fileCount: paths.length,
        totalBytes: 0,
        skipped: 0,
        symlinksSkipped: 0,
        limitHit: null,
        // 原型模式（浏览器直开）没有真实文件系统可问。
        // 猜"不含目录"会让原型里的拖拽一律直发 —— 而原型本来也没有
        // 真实的传输链路，直发只是把文件列出来给人看，行为上可接受。
        hasDirectory: false,
        sample: paths,
        expanded: paths.map((p) => ({
          path: p,
          fileCount: 1,
          totalBytes: 0,
          isDir: false,
          skipped: 0,
          symlinksSkipped: 0,
        })),
      };
    }
    return invokeSafe<SendPreview>(
      "preview_send_paths",
      { paths },
      () => ({
        fileCount: 0,
        totalBytes: 0,
        skipped: 0,
        symlinksSkipped: 0,
        limitHit: null,
        hasDirectory: false,
        sample: [],
        expanded: [],
      })
    );
  }

  // ------------------------------------------------------------------ 端点（§3.9）
  /**
   * 某台设备的全部已知端点（按"覆盖网优先"排序）。
   *
   * 一台设备同时有 ZeroTier 与物理网卡时会返回多条 —— 这正是旧设计
   * 只记一个 `last_ip` 时会丢掉的：DHCP 一换就彻底失联，
   * 而用户既没重启也没改配置，表现为"设备明明开着却搜不到"。
   */
  static async listDeviceEndpoints(deviceId: string): Promise<DeviceEndpoint[]> {
    if (notInTauri()) return [];
    return invokeSafe<DeviceEndpoint[]>("list_device_endpoints", { deviceId }, () => []);
  }

  /** 某台设备的首选端点（覆盖网优先，D6 决策） */
  static async getPreferredEndpoint(
    deviceId: string
  ): Promise<DeviceEndpoint | null> {
    if (notInTauri()) return null;
    return invokeSafe<DeviceEndpoint | null>(
      "get_preferred_endpoint",
      { deviceId },
      () => null
    );
  }

  /** 批量取本地文件真实大小, 供待发清单展示 */
  static async probeFileSizes(paths: string[]): Promise<Record<string, number>> {
    if (notInTauri()) return {};
    return invokeSafe<Record<string, number>>("probe_file_sizes", { paths }, () => ({}));
  }

  static async openReceiveFolder(): Promise<void> {
    if (notInTauri()) return;
    await invokeSafe<void>("open_receive_folder");
  }

  static async openLogFolder(): Promise<void> {
    if (notInTauri()) return;
    await invokeSafe<void>("open_log_folder");
  }

  static async setAutostart(enabled: boolean): Promise<void> {
    if (notInTauri()) return;
    await invokeSafe<void>("set_autostart", { enabled });
  }

  // ------------------------------------------------------------------ 窗口
  static async minimizeWindow(): Promise<void> {
    if (notInTauri()) return;
    await invokeSafe<void>("minimize_window");
  }

  static async toggleMaximizeWindow(): Promise<void> {
    if (notInTauri()) return;
    await invokeSafe<void>("toggle_maximize_window");
  }

  static async hideWindow(): Promise<void> {
    if (notInTauri()) return;
    await invokeSafe<void>("hide_window");
  }

  static async quitApp(): Promise<void> {
    if (notInTauri()) return;
    await invokeSafe<void>("quit_app");
  }

  static async startWindowDragging(): Promise<void> {
    if (notInTauri()) return;
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    await getCurrentWindow().startDragging();
  }

  // ------------------------------------------------------------------ 审批 / 历史
  /**
   * 回应审批请求。
   *
   * **没有 `grantCode`。** 审批人只决定"允许一次 / 允许并永久信任 / 拒绝"：
   * 「每次匹配码」等级下要出示的码是**本机生成、显示在审批窗口上**的
   * （见 `ApprovalRequest.grant_challenge`），由**发起方**敲回它自己的
   * 界面。不信任发起方的是接收方，所以该由接收方出题 ——
   * 论证见 `core/src/transport/server.rs` 的 `ApprovalManager::challenge_for`。
   *
   * 动作集合里**没有 `block`**：那个「阻止该设备」维度是实现期自己加的
   * （原始提交 `735dd66` 里零命中），已按 `DESIGN_TRUST_SHUTTLE.md`
   * §14.11 决策 1 整体删除。它想覆盖的两个诉求分别由
   * **解除配对**（双向）与**隐藏**承担。
   */
  static async respondApproval(
    approvalId: string,
    action: "allow_once" | "allow_with_grant" | "allow_and_trust" | "reject"
  ): Promise<boolean> {
    return invokeSafe<boolean>("respond_approval", { approvalId, action }, () => true);
  }

  /**
   * 撤销某台设备的全部短期授权（§2.3.1 的「可撤销」）。
   *
   * 返回被撤销的项数（0 = 本来就没有）。
   */
  static async revokeDeviceGrants(deviceId: string): Promise<number> {
    return invokeSafe<number>("revoke_device_grants", { deviceId }, () => 0);
  }

  /**
   * 列出**全部设备**当前生效中的短期授权，key = device_id（§2.3.1）。
   *
   * 界面上必须能看到"这台设备现在免确认到什么时候"——
   * **看不见的状态不该存在**：那正是"用户以为关掉了、其实还开着"。
   */
  static async listActiveGrants(): Promise<Record<string, ActiveGrant[]>> {
    return invokeSafe<Record<string, ActiveGrant[]>>("list_active_grants", undefined, () => ({}));
  }

  /**
   * 实际生效的授权窗口秒数（0 = 功能关闭，弹窗里不出现第三个按钮）。
   *
   * 界面**不许**自己硬编码这个数字：core 那边有 10 分钟硬上界，
   * 前端写死就会在用户调大配置时说谎。
   */
  static async getSessionGrantWindow(): Promise<number> {
    return invokeSafe<number>("get_session_grant_window", undefined, () => 0);
  }

  static async getTransferHistory(limit?: number): Promise<TransferRecord[]> {
    return invokeSafe<TransferRecord[]>("get_transfer_history", { limit: limit ?? null }, () => []);
  }

  /**
   * 最近 N 条**诊断**记录（比传输记录更细：阶段耗时、归因）。
   *
   * 与 `getTransferHistory` 分开而不是合并：历史记录回答"发生了什么"，
   * 诊断回答"**时间花在哪 / 瓶颈在谁**"。用户报"飞梭传得慢"时，
   * 真正能改配置的信息（socket buffer 太小、缓冲区不足、链路是覆盖网）
   * **只存在于诊断里** —— 合并成一个列表只会把两者混成一团。
   */
  static async getTransferDiagnostics(limit?: number): Promise<TransferDiagnostic[]> {
    return invokeSafe<TransferDiagnostic[]>(
      "get_transfer_diagnostics",
      { limit: limit ?? null },
      () => []
    );
  }

  static async clearTransferHistory(): Promise<boolean> {
    return invokeSafe<boolean>("clear_transfer_history", undefined, () => true);
  }

  // ------------------------------------------------------------------ 配置
  static async updateAppConfig(params: {
    device_name?: string;
    auto_receive?: boolean;
    log_level?: string;
    max_log_size_mb?: number;
    max_history_records?: number;
    record_retention_days?: number;
    close_action?: "ask" | "tray" | "exit";
    theme?: "dark" | "light";
    receive_dir?: string;
    auto_check_update?: boolean;
    /**
     * 来源网段白名单。**空数组 = 不限制**。
     *
     * 存盘前 Rust 侧会**逐条校验**格式 —— 前端那份 `isValidCidr` 只是
     * 即时提示，不是不放行的依据。校验不过会整条 `update` 失败并回传原因，
     * 而不是"存下一条用不了的配置"。
     */
    allowed_peer_subnets?: string[];
  }): Promise<boolean> {
    if (notInTauri()) return true;
    const { invoke } = await import("@tauri-apps/api/core");
    // 整体作为一个 patch 传参, 后端逐项校验后原子落盘
    return await invoke<boolean>("update_app_config", { payload: params });
  }

  // ------------------------------------------------------------------ 应用内更新
  /**
   * 读取更新状态快照。
   *
   * 必须在进入设置页时主动拉一次, 不能只靠事件:
   * 托盘常驻下窗口可能挂机几小时, 期间后台调度完成下载,
   * 事件早就发完了 —— 只听事件的话用户看到的还是"idle"。
   */
  static async getUpdateStatus(): Promise<UpdateStatus> {
    if (notInTauri()) {
      return {
        phase: "uptodate",
        message: "当前已是最新版本 v0.1.0",
        currentVersion: "0.1.0",
        latestVersion: "",
        tagName: "",
        pendingPath: "",
        bytesReceived: 0,
        bytesTotal: 0,
        releasePage: "https://gitee.com/huaxudong/feisuo/releases",
        busy: false,
      };
    }
    return invokeSafe<UpdateStatus>("get_update_status");
  }

  /**
   * 检查更新并自动下载 (Gitee 优先, GitHub 降级)。
   *
   * 过程状态由 `feisuo://update-status` 事件推送, 所以这里返回的
   * 只是**结束态**快照, 界面上不要靠它显示进度。
   */
  static async checkForUpdate(interactive = true): Promise<UpdateStatus> {
    if (notInTauri()) return await this.getUpdateStatus();
    return invokeSafe<UpdateStatus>("check_for_update", { interactive });
  }

  /**
   * 应用已下载的更新: 换名接力 → 拉起新版本 → 退出当前实例。
   *
   * 成功时进程已退出, 调用方**不会**拿到 resolve ——
   * 所以调用处只处理 reject 即可, 不要在之后刷新界面。
   */
  static async applyUpdate(): Promise<boolean> {
    if (notInTauri()) return true;
    return invokeSafe<boolean>("apply_update");
  }

  /** 自动下载失败时的兜底: 在系统浏览器打开下载页 */
  static async openUpdatePage(url?: string): Promise<boolean> {
    if (notInTauri()) return true;
    return invokeSafe<boolean>("open_update_page", { url: url ?? null });
  }

  /** 更新状态变化 (检查中 / 下载进度 / 就绪 / 失败) */
  static async onUpdateStatus(cb: (s: UpdateStatus) => void): Promise<() => void> {
    if (notInTauri()) return () => {};
    const { listen } = await import("@tauri-apps/api/event");
    return await listen<UpdateStatus>("feisuo://update-status", (e) => cb(e.payload));
  }

  /**
   * 全局热键 / 托盘菜单请求打开剪贴板直发（§6.4）。
   *
   * 后端在触发前已唤出窗口 —— 前端只管切到发送页并读剪贴板。
   * 载荷为空：无参数事件。
   */
  static async onOpenClipboardRequest(cb: () => void): Promise<() => void> {
    if (notInTauri()) return () => {};
    const { listen } = await import("@tauri-apps/api/event");
    return await listen("feisuo://open-clipboard", () => cb());
  }

  // ------------------------------------------------------------------ 事件订阅
  static async onTransferProgress(cb: (p: TransferProgress) => void): Promise<() => void> {
    if (notInTauri()) return () => {};
    const { listen } = await import("@tauri-apps/api/event");
    return await listen<TransferProgress>("feisuo://transfer-progress", (e) => cb(e.payload));
  }

  static async onDeviceDiscovered(cb: (d: DiscoveredDevice) => void): Promise<() => void> {
    if (notInTauri()) return () => {};
    const { listen } = await import("@tauri-apps/api/event");
    return await listen<DiscoveredDevice>("feisuo://device-discovered", (e) => cb(e.payload));
  }

  static async onApprovalRequest(cb: (req: ApprovalRequest) => void): Promise<() => void> {
    if (notInTauri()) return () => {};
    const { listen } = await import("@tauri-apps/api/event");
    return await listen<ApprovalRequest>("feisuo://transfer-approval", (e) => cb(e.payload));
  }

  /** 原生关闭 (Alt+F4 / 标题栏 ×) 被拦截后由后端派发, 前端据此弹出选择框 */
  static async onWindowCloseRequested(cb: () => void): Promise<() => void> {
    if (notInTauri()) return () => {};
    const { listen } = await import("@tauri-apps/api/event");
    return await listen("feisuo://window-close-requested", () => cb());
  }

  static async onConfigUpdated(cb: () => void): Promise<() => void> {
    if (notInTauri()) return () => {};
    const { listen } = await import("@tauri-apps/api/event");
    return await listen("feisuo://config-updated", () => cb());
  }

  /** 引擎启动失败 (端口被占用 / 权限不足等), 必须在界面上明确告知 */
  static async onEngineError(cb: (message: string) => void): Promise<() => void> {
    if (notInTauri()) return () => {};
    const { listen } = await import("@tauri-apps/api/event");
    return await listen<string>("feisuo://engine-error", (e) => cb(String(e.payload)));
  }

  // ------------------------------------------------------------------ 原生拖放
  /**
   * 监听 Tauri 原生拖放事件。
   * 浏览器 File 对象在 WebView2 下拿不到真实磁盘路径,
   * 必须靠这个事件才能把"用户拖进来的文件"真正发出去。
   */
  static async onNativeDragDrop(cb: (paths: string[]) => void): Promise<() => void> {
    if (notInTauri()) return () => {};
    const { getCurrentWebview } = await import("@tauri-apps/api/webview");
    return await getCurrentWebview().onDragDropEvent((event) => {
      if (event.payload.type === "drop") {
        cb(event.payload.paths);
      }
    });
  }

  /**
   * 确保主窗口可见。
   *
   * 飞梭是托盘常驻应用, 关窗后窗口隐藏; 而 Tauri 的原生拖放在窗口隐藏时
   * 照样触发。不唤出窗口的话, 用户把文件拖到看不见的地方, 文件进了清单
   * 却毫无反馈 —— 表现为"拖了没反应"。
   */
  /**
   * 引擎的真实健康状态。
   *
   * 必须查这个而**不能**靠"设备列表调用没抛异常"来推断在线:
   * 引擎启动失败时设备表本来就是空的, 调用会成功返回 `[]`,
   * 于是界面显示绿灯"在线", 还会把错误横幅清掉 ——
   * 托盘常驻下用户完全无从察觉引擎根本没起来。
   */
  static async getEngineStatus(): Promise<{ running: boolean; error: string | null }> {
    if (notInTauri()) return { running: false, error: null };
    const { invoke } = await import("@tauri-apps/api/core");
    return await invoke<{ running: boolean; error: string | null }>("get_engine_status");
  }

  static async ensureWindowVisible(): Promise<void> {
    if (notInTauri()) return;
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("ensure_window_visible");
  }

  static describeError(e: unknown, fallback: string): string {
    return toMessage(e, fallback);
  }
}

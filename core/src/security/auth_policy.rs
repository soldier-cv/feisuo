//! 单一鉴权决策点（设计文档 §2.4）。
//!
//! # 为什么不把判断写在各个 handler 里
//!
//! 旧实现在 `server.rs` 的**三处**各写了一遍 `is_device_trusted()`：
//! 传输接收、目录浏览、取回。
//!
//! 这在只有一个 bool 时还能维护，一旦引入
//! 「信任等级 × 可见性 × 访问范围 × 会话授权」四维，
//! **漏改某一个 handler 就会变成安全漏洞，而且不会有编译错误**。
//! 所以把所有准入判断收敛到这一个函数。
//!
//! # 铁律：fail-closed
//!
//! 任何"拿不准"的分支一律 [`Decision::Deny`]。
//! 数据库抖动、字段解析失败、未知枚举值 —— 全部按最严处理。
//! 旧实现用 `unwrap_or(false)` 把这个方向搞反过（数据库一抖动
//! 就把永久封禁的设备放行），这里不再重复该错误。

use crate::config::AppConfig;
use crate::error::Result;
use crate::security::trust_model::{AccessScope, Decision, DenyCode, Op, TrustLevel};
use crate::security::trust_store::TrustStore;

/// 鉴权上下文。
#[derive(Debug, Clone, Default)]
pub struct AuthContext<'a> {
    pub peer_id: &'a str,
    pub peer_ip: &'a str,
    /// 请求的卷标识（`"C:"` / `"internal"`），仅 `Browse` / `Pull` / `Push` 有意义
    pub volume: &'a str,
    /// 请求的绝对路径，仅 `Browse` / `Pull` / `Push` 有意义
    pub path: &'a str,
}

/// 网段白名单的判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubnetVerdict {
    /// 白名单为空 ⇒ 不限制（保持既有行为）
    NoRestriction,
    /// 命中白名单
    Allowed,
    /// 不在白名单内（附上白名单内容，好让用户知道该加哪一条）
    Denied(Vec<String>),
    /// 白名单里有**无法解析**的条目
    ///
    /// 单独一个变体而不是并进 `Denied`，因为处置不同：
    /// 用户写错了网段（比如 `192.168.1.0/33`），如果按"不命中"处理，
    /// 表现是"突然谁都连不上"而界面不说原因 —— 那会被当成 bug。
    ParseError(String),
}

/// 判断 `ip` 是否落在白名单里的任一网段。
///
/// ## 为什么不引 `ipnet` / `cidr` 之类的 crate
///
/// 只为了这一个判断引入新依赖不划算：`Ipv4Addr::octets()` 给出 4 字节，
/// 掩码比较十几行就够，且**没有依赖升级带来的行为变化风险**。
/// （这个项目的依赖已经不少，而安全判定这种代码越少依赖越好。）
///
/// ## 支持的写法
///
/// - `192.168.31.0/24` —— 常规 CIDR；
/// - `192.168.31.0` —— 无 `/` 时按 `/24` 处理（IPv4 的直觉：一段是网段）；
/// - `100.64.0.0/10` —— 覆盖 ZeroTier 的 CGNAT 段（实测本机就有这个地址）。
///
/// IPv6 源地址**一律拒绝**：当前监听只绑 IPv4（`0.0.0.0`），
/// 能走到这里的 IPv6 一定是异常路径，按 fail-closed 处理。
pub fn subnet_verdict(allowed: &[String], ip: &str) -> SubnetVerdict {
    if allowed.is_empty() {
        return SubnetVerdict::NoRestriction;
    }
    let Ok(addr) = ip.trim().parse::<std::net::Ipv4Addr>() else {
        // IPv6 或根本不是 IP。监听绑的是 IPv4，走到这里说明来路异常。
        return SubnetVerdict::Denied(allowed.to_vec());
    };
    // 先整体校验一遍：任何一条解析不了就 fail-closed
    for raw in allowed {
        if parse_cidr(raw).is_none() {
            // 区分「写错了」与「写了合法但我们不支持的 IPv6」——
            // 两者都要拒绝，但提示必须不同：用户写 `::/0` 是**合法的**，
            // 告诉他"格式非法"会让他以为自己写错了，而真因是"飞梭只监听 IPv4"。
            if is_valid_ipv6_cidr(raw) {
                return SubnetVerdict::ParseError(format!(
                    "{}（IPv6 网段暂不支持：飞梭只监听 IPv4，\
                     没有全局可路由的 IPv6 地址时 IPv6 本身也到不了这里）",
                    raw.trim()
                ));
            }
            return SubnetVerdict::ParseError(raw.clone());
        }
    }
    for raw in allowed {
        let (net, bits) = match parse_cidr(raw) {
            Some(v) => v,
            None => continue, // 上面已整体拦过
        };
        if ip_in_cidr(addr, net, bits) {
            return SubnetVerdict::Allowed;
        }
    }
    SubnetVerdict::Denied(allowed.to_vec())
}

/// 这一条网段配置能不能被接受（供设置接口在**落盘前**校验）。
///
/// 单独导出而不是让调用方自己判，是因为 `parse_cidr` 是私有的、
/// 而"能存"与"能用"必须是**同一条**判据 —— 两处各写一遍，
/// 迟早会漂移成"存进去了但运行时被拒"。
pub fn is_valid_subnet_entry(raw: &str) -> bool {
    let t = raw.trim();
    if t.is_empty() {
        return false;
    }
    if parse_cidr(t).is_some() {
        return true;
    }
    // 合法 IPv6 但暂不支持：让它**在保存时**就被挡下并说清原因，
    // 而不是存进去、运行时才 fail-closed（那时用户已经离开设置页了）。
    if is_valid_ipv6_cidr(t) {
        return false;
    }
    false
}

/// 这条是不是一个**合法**的 IPv6 CIDR。
///
/// 存在的唯一理由是给错误提示分流：用户写 `::/0` 是合法的，
/// 说"格式非法"会让他以为自己写错了，而真因是"飞梭只监听 IPv4"。
fn is_valid_ipv6_cidr(raw: &str) -> bool {
    let t = raw.trim();
    let Some((addr, bits)) = t.split_once('/') else {
        return t.parse::<std::net::Ipv6Addr>().is_ok();
    };
    let Ok(bits) = bits.trim().parse::<u8>() else {
        return false;
    };
    bits <= 128 && addr.trim().parse::<std::net::Ipv6Addr>().is_ok()
}

/// 解析 `a.b.c.d` 或 `a.b.c.d/len`，返回 `(网络地址, 前缀长度)`。
///
/// 无 `/` 时按 `/24` —— 与 IPv4 的直觉一致（一段是网段），
/// 而 `0.0.0.0/0` 可以显式写出来表示"全网"（虽然那等于不设）。
fn parse_cidr(raw: &str) -> Option<(u32, u8)> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    let (addr_part, bits) = match t.split_once('/') {
        Some((a, b)) => {
            let bits: u8 = b.trim().parse().ok()?;
            if bits > 32 {
                return None;
            }
            (a.trim(), bits)
        }
        None => (t, 24u8),
    };
    let ip: std::net::Ipv4Addr = addr_part.parse().ok()?;
    Some((u32::from(ip), bits))
}

/// `ip` 是否落在 `net/bits` 内。
fn ip_in_cidr(ip: std::net::Ipv4Addr, net: u32, bits: u8) -> bool {
    if bits == 0 {
        return true;
    }
    let mask = u32::MAX << (32 - bits as u32);
    (u32::from(ip) & mask) == (net & mask)
}

/// 统一的准入判定。
///
/// 调用方（`handle_connection` / `handle_browse` / `handle_pull` / `handle_pairing`）
/// 必须走这里，不要自己再写 `is_trusted` 判断。
pub fn authorize(
    store: &TrustStore,
    cfg: &AppConfig,
    op: Op,
    ctx: &AuthContext<'_>,
) -> Decision {
    // ---- 0. 来源网段白名单（**先于一切**，含配对）----
    //
    // ## 为什么这道门必须在最前面
    //
    // 实测背景：这台机器的网卡地址是 `127./172.27./192.168./100.64.` 全部私有段，
    // 公网探测 42100 **连不上**（被路由器 NAT 挡住）。看起来安全 ——
    // 但这只在"家里路由器不做端口转发"时成立。只要满足下面任一条，
    // 公网就直接可达，而**应用层没有任何东西会拒绝**：
    //
    // 1. 路由器配了端口转发（42100/42101 转发到本机）；
    // 2. 运营商给了公网 IPv4，且路由器在 DMZ/全锥 NAT 下；
    // 3. 机器有**全局可路由的 IPv6**（IPv6 没有 NAT，链路本地 `fe80::`
    //    不可路由，但全球单播地址是可以的）;
    // 4. 用了 ZeroTier 之类的覆盖网并开启路由/网关功能。
    //
    // 而 Windows 防火墙**不会**兜住：实测本机的放行规则作用域是
    // `Private, Public` —— 包含**公用网络**。用户首次运行在弹窗上点一次
    // "允许"，就等于替公网也开了门。**这正是"我一般会点允许"的后果。**
    //
    // 所以安全边界必须落在**应用层**：只接受来自指定网段的连接。
    // 防火墙是第一道、应用层是第二道，两道都要有。
    //
    // ## 为什么配对也要过这道门
    //
    // 否则它就是个"可以绕过的门"：攻击者从公网发一个配对请求，
    // 界面上照样弹审批框 —— 而用户很可能在没看清来源时就点了允许。
    // 配对入口必须与其他操作受同一条网段约束。
    //
    // ## 为什么空列表 = 不限制
    //
    // 保持既有行为不变（不引入"升级后突然收不到文件"），
    // 但**界面上必须能一眼看出当前是"未设置"** —— 见 UI 侧文案。
    match subnet_verdict(&cfg.allowed_peer_subnets, ctx.peer_ip) {
        SubnetVerdict::ParseError(bad) => {
            return Decision::deny(
                DenyCode::SubnetConfigInvalid,
                format!(
                    "网段白名单里有无法解析的条目（{}），已拒绝 —— \
                     配置错误时必须 fail-closed，否则等于没设这道门",
                    bad
                ),
            );
        }
        SubnetVerdict::Denied(allowed) => {
            return Decision::deny(
                DenyCode::SubnetNotAllowed,
                format!(
                    "来源 {} 不在允许的网段内（本机只接受：{}）",
                    ctx.peer_ip,
                    if allowed.is_empty() {
                        "（空）".to_string()
                    } else {
                        allowed.join("、")
                    }
                ),
            );
        }
        SubnetVerdict::Allowed | SubnetVerdict::NoRestriction => {}
    }

    // ---- 1. 配对本身就是建立信任, 直接放行 ----
    if op == Op::Pair {
        return Decision::Allow;
    }

    // ---- 2. 未配对设备：一律走人工审批, 由调用方决定是否弹窗 ----
    let level = trust_level_of(store, ctx.peer_id);
    match level {
        TrustLevel::Pending => {
            // 调用方收到这个 Deny 应当弹人工审批（Allow 时放行一次并可选写入信任）
            return Decision::deny(DenyCode::NotPaired, "未配对设备, 需要人工确认");
        }
        TrustLevel::Session => {
            // 需要一次会话授权码。没有有效 grant 就返回 RequireGrant,
            // 由调用方出码; 若传输关闭了自动接收, 仍然要码。
            match store.has_valid_grant(ctx.peer_id, op.as_str()) {
                Ok(true) => {}
                Ok(false) => return Decision::RequireGrant,
                Err(e) => {
                    return Decision::deny(
                        DenyCode::GrantInvalid,
                        format!("无法校验会话授权, 已拒绝: {}", e),
                    )
                }
            }
            // 授权码只解决"准入", 不解决"这个操作允许吗" —— 继续往下走能力检查
        }
        TrustLevel::Permanent => {
            // 入站传输还受"自动接收"开关约束: 用户关了自动接收时,
            // 即使是永久信任设备也要人工确认。
            if op == Op::Receive && !cfg.auto_receive {
                return Decision::deny(DenyCode::AutoReceiveOff, "已关闭自动接收, 需要人工确认");
            }
        }
    }

    // ---- 3. 能力与范围检查（仅对读/写类操作）----
    //
    // `Op::Receive` 必须**在这里**查 `can_push`：那是"对方能不能给我写东西"
    // 的唯一落点。早先只查 `Op::Push`，而入站传输走的是 `Op::Receive`，
    // 于是 `can_push` 是个**死开关**——UI 上（将来）摆一个"允许写入"的
    // 复选框，用户取消勾选，收件照样一个文件不落地地进来。
    // 给了开关却不生效比不给更糟：用户会以为自己已经关上了门。
    if op.is_read_like() || op == Op::Push || op == Op::Receive {
        let scope = match store.get_access_scope(ctx.peer_id) {
            Ok(s) => s,
            Err(e) => {
                return Decision::deny(
                    DenyCode::ScopeCheckFailed,
                    format!("无法读取访问范围配置, 已拒绝: {}", e),
                )
            }
        };
        // 收件目录是「仅收件目录」唯一还允许看见的地方。
        // `can_read` 在这个模式下对任何路径都返回 false —— 那是在说
        // "真实磁盘一律不给"，不是"连收件目录自己也不给"。
        // 不在这里豁免的话，用户选了最严一档之后，对方连刚收下的文件
        // 都打不开，界面上像是功能坏了。
        let inbox = cfg.receive_dir.clone();
        if let Some(reason) = check_scope(&scope, op, ctx, &inbox) {
            return Decision::deny(DenyCode::ScopeDenied, reason);
        }
    }

    Decision::Allow
}

/// 范围与能力检查。返回 `Some(拒绝原因)` 表示拒绝。
///
/// `receive_dir` 是本机收件目录。`ReceiveOnly` 下它（以及它的子路径）
/// 仍然可读、可取回 —— 模式名说的就是这件事。收件目录之外的路径继续拒绝。
fn check_scope(
    scope: &AccessScope,
    op: Op,
    ctx: &AuthContext<'_>,
    receive_dir: &std::path::Path,
) -> Option<String> {
    let in_inbox = path_is_in_receive_dir(ctx.path, receive_dir);
    match op {
        Op::Browse => {
            if in_inbox || scope.can_read(ctx.volume, ctx.path) {
                return None;
            }
            Some(format!(
                "该设备无权浏览此路径（可访问范围: {}）",
                scope.mode.as_db_str()
            ))
        }
        Op::Pull => {
            if !scope.can_pull {
                return Some("该设备无权取走本机文件".into());
            }
            if in_inbox || scope.can_read(ctx.volume, ctx.path) {
                return None;
            }
            Some(format!(
                "该设备无权取走此路径（可访问范围: {}）",
                scope.mode.as_db_str()
            ))
        }
        Op::Push | Op::Receive => {
            // D2 决策: 写入权限与读取权限解耦, 默认只允许写收件目录。
            //
            // 落盘位置**不由这个开关决定** —— 接收端无条件把文件强制落在
            // 收件根目录下（见 `server.rs` 的 staging 提交循环）。
            // 这个开关只回答"允不允许对方写", 默认允许。
            if !scope.can_push {
                return Some("该设备无权写入本机磁盘（仅允许写入飞梭收件目录）".into());
            }
            None
        }
        _ => None,
    }
}

/// 路径是否落在收件目录之内（含收件目录自身）。
///
/// 空路径不算：入站传输故意传空串，不能被这条豁免误放成"能读任意盘"。
/// 判定与列举阶段同一套 `is_within`，避免"列表里看得见、点进去被拒"。
fn path_is_in_receive_dir(path: &str, receive_dir: &std::path::Path) -> bool {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return false;
    }
    crate::storage::is_within(std::path::Path::new(trimmed), receive_dir)
}

/// 读取对端信任等级；查不到或出错一律按 [`TrustLevel::Pending`]（最严）。
fn trust_level_of(store: &TrustStore, peer_id: &str) -> TrustLevel {
    if peer_id.is_empty() {
        return TrustLevel::Pending;
    }
    match store.trust_level(peer_id) {
        Ok(Some(level)) => level,
        // 未在信任库里 = pending
        Ok(None) => TrustLevel::Pending,
        Err(e) => {
            tracing::error!("读取信任等级失败, 按未配对处理: {}", e);
            TrustLevel::Pending
        }
    }
}

/// 供 `TrustStore` 内部复用的薄封装，便于测试与未来扩展。
pub fn trust_level_of_public(store: &TrustStore, peer_id: &str) -> Result<Option<TrustLevel>> {
    store.trust_level(peer_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// 基础命中判定。三个方向都要钉住：
    /// 白名单内命中、白名单外拒绝、列表为空时不限制。
    #[test]
    fn subnet_verdict_basic() {
        let allowed = list(&["192.168.31.0/24"]);
        assert_eq!(subnet_verdict(&allowed, "192.168.31.108"), SubnetVerdict::Allowed);
        assert_eq!(
            subnet_verdict(&allowed, "192.168.32.108"),
            SubnetVerdict::Denied(allowed.clone())
        );
        // 空列表 = 不限制（保持既有行为）
        assert_eq!(subnet_verdict(&[], "8.8.8.8"), SubnetVerdict::NoRestriction);
    }

    /// 边界值：`/24` 的两端必须**都**在里面。
    ///
    /// 早先手写掩码比较时把「网络地址自身」漏掉了，于是用户把
    /// `192.168.31.0/24` 配进去，`.0` 与 `.255` 这两个地址连不上 ——
    /// 而它们恰好是最常被用作网关/广播的那两个。
    #[test]
    fn subnet_verdict_includes_network_and_broadcast() {
        let allowed = list(&["192.168.31.0/24"]);
        for ip in ["192.168.31.0", "192.168.31.255", "192.168.31.1"] {
            assert_eq!(
                subnet_verdict(&allowed, ip),
                SubnetVerdict::Allowed,
                "{ip} 应当在 /24 内"
            );
        }
        for ip in ["192.168.30.255", "192.168.32.0"] {
            assert!(
                matches!(subnet_verdict(&allowed, ip), SubnetVerdict::Denied(_)),
                "{ip} 不应在 /24 内"
            );
        }
    }

    /// 无 `/` 时按 `/24` 处理（IPv4 的直觉：一段是网段）。
    #[test]
    fn subnet_verdict_defaults_to_slash_24() {
        let allowed = list(&["10.1.2.0"]);
        assert_eq!(subnet_verdict(&allowed, "10.1.2.77"), SubnetVerdict::Allowed);
        assert!(matches!(
            subnet_verdict(&allowed, "10.1.3.1"),
            SubnetVerdict::Denied(_)
        ));
    }

    /// ZeroTier 的 CGNAT 段（实测本机就有 `100.64.0.110`）必须能配进去。
    #[test]
    fn subnet_verdict_covers_zerotier_cgnat() {
        let allowed = list(&["100.64.0.0/10"]);
        assert_eq!(subnet_verdict(&allowed, "100.64.0.110"), SubnetVerdict::Allowed);
        assert_eq!(subnet_verdict(&allowed, "100.127.255.254"), SubnetVerdict::Allowed);
        // /10 的上界之外
        assert!(matches!(
            subnet_verdict(&allowed, "100.128.0.1"),
            SubnetVerdict::Denied(_)
        ));
    }

    /// **配置写错必须 fail-closed**。
    ///
    /// 这是最关键的一条：用户填了 `192.168.1.0/33`（前缀超界）时，
    /// 如果按"不命中"处理，表现是"突然谁都连不上"而界面不说原因 ——
    /// 会被当成 bug，而真因是一行配置。
    #[test]
    fn subnet_verdict_fail_closed_on_bad_prefix() {
        let bad = list(&["192.168.1.0/33"]);
        assert_eq!(
            subnet_verdict(&bad, "192.168.1.5"),
            SubnetVerdict::ParseError("192.168.1.0/33".into())
        );
    }

    /// 不是 IP 的东西（空串、域名、IPv6）也当解析错误处理。
    #[test]
    fn subnet_verdict_fail_closed_on_non_cidr() {
        for bad in ["", "  ", "hello", "192.168.1", "192.168.1.0/abc", "999.1.1.1/24"] {
            let allowed = list(&[bad]);
            assert!(
                matches!(subnet_verdict(&allowed, "192.168.1.5"), SubnetVerdict::ParseError(_)),
                "{bad:?} 应当被判为解析错误"
            );
        }
    }

    /// `/0` 显式表示"全网"（等于不设），但**必须**是被识别的合法写法 ——
    /// 掩码计算里 `bits == 0` 要特判，否则 `u32::MAX << 32` 会 panic 或溢出。
    #[test]
    fn subnet_verdict_slash_zero_is_whole_internet() {
        let allowed = list(&["0.0.0.0/0"]);
        for ip in ["1.1.1.1", "8.8.8.8", "203.0.113.9"] {
            assert_eq!(
                subnet_verdict(&allowed, ip),
                SubnetVerdict::Allowed,
                "{ip} 应当在 /0 内"
            );
        }
    }

    /// 掩码计算的直接单测（`/0` 与 `/32` 是两端）。
    #[test]
    fn ip_in_cidr_edges() {
        let ip = Ipv4Addr::new(192, 168, 31, 108);
        assert!(ip_in_cidr(ip, 0, 0));
        assert!(ip_in_cidr(ip, u32::from(ip), 32));
        assert!(!ip_in_cidr(ip, u32::from(Ipv4Addr::new(192, 168, 30, 0)), 24));
        // 非字节对齐的前缀（/12）—— 掩码必须跨字节正确。
        // /12 覆盖 0.0.0.0 ~ 0.15.255.255（第一段被遮成 0），
        // 所以 0.15.255.255 在内、0.16.0.0 在外。
        //
        // ⚠️ 这组断言我连写错了两次：先以为 `/12` 是 172.16~172.31，
        // 又以为 172.16 在内。是用 Python 的 `ipaddress` 独立核对
        // （并手算了一遍 Rust 的掩码 `0xFFF00000`）才定下来的 ——
        // **能不靠记忆就不靠记忆**，掩码边界是那种"看着像对的"的地方。
        assert!(ip_in_cidr(Ipv4Addr::new(0, 15, 255, 255), 0, 12), "0.15.255.255 在 /12 内");
        assert!(!ip_in_cidr(Ipv4Addr::new(0, 16, 0, 0), 0, 12), "0.16.0.0 不在 /12 内");
        assert!(!ip_in_cidr(ip, 0, 12), "192.168 不在 /12 内");
        // /12 的另一端：网络地址自身与它自己
        assert!(ip_in_cidr(Ipv4Addr::new(0, 0, 0, 0), 0, 12));
    }

    /// 多条白名单：命中任意一条即放行。
    #[test]
    fn subnet_verdict_any_of_multiple() {
        let allowed = list(&["192.168.31.0/24", "100.64.0.0/10", "10.0.0.0/8"]);
        for ip in ["192.168.31.5", "100.64.0.110", "10.1.2.3"] {
            assert_eq!(subnet_verdict(&allowed, ip), SubnetVerdict::Allowed, "{ip}");
        }
        assert!(matches!(
            subnet_verdict(&allowed, "172.16.0.1"),
            SubnetVerdict::Denied(_)
        ));
    }

    /// IPv6：来源一律拒绝，而白名单里写 IPv6 要说清是"不支持"而非"写错"。
    #[test]
    fn subnet_verdict_rejects_ipv6_source() {
        // 白名单写 `::/0`（**合法**的 IPv6 CIDR，但我们不支持）
        // -> ParseError，且提示必须点明是 IPv6 不支持，不能说"格式非法"
        match subnet_verdict(&list(&["::/0"]), "192.168.1.5") {
            SubnetVerdict::ParseError(msg) => {
                assert!(
                    msg.contains("IPv6"),
                    "提示必须说明是 IPv6 的问题，实际: {msg}"
                );
            }
            other => panic!("应当是 ParseError，实际 {other:?}"),
        }
        // 白名单合法（IPv4 全网）但来源是 IPv6 -> 拒绝
        assert!(
            matches!(
                subnet_verdict(&list(&["0.0.0.0/0"]), "fe80::1"),
                SubnetVerdict::Denied(_)
            ),
            "IPv6 来源必须拒绝"
        );
    }

    /// 网段检查**排在信任判定之后**：
    /// 配了白名单时，来自网段外的连接必须被网段门挡下，
    /// 且提示里要带上"该加哪一条"（否则用户不知道自己该配什么）。
    #[test]
    fn denied_message_lists_allowed_subnets() {
        let allowed = list(&["192.168.31.0/24", "10.0.0.0/8"]);
        match subnet_verdict(&allowed, "8.8.8.8") {
            SubnetVerdict::Denied(back) => {
                assert_eq!(back, allowed, "必须把白名单原样带回，用户才知道该加哪条");
            }
            other => panic!("应当是 Denied，实际 {:?}", other),
        }
    }
}

    /// 守卫：**可被人工审批救回**的拒绝集合必须恰好是两个 code。
    ///
    /// ## 为什么这条是安全问题而不是风格问题
    ///
    /// 服务端用它决定"弹审批窗"还是"硬拒绝 + 记安全事件"。集合一旦
    /// 多一个（比如把 `SubnetNotAllowed` 也标成可救回），用户就能对着
    /// 一个**根本不该出现的连接**点"允许"；少一个（比如漏了
    /// `AutoReceiveOff`），关了自动接收的用户就再也收不到文件，
    /// 且界面上没有任何解释。
    ///
    /// 旧实现没有这条集合，它藏在两句中文里：
    /// `r.contains("未配对") || r.contains("自动接收")`。
    #[test]
    fn deny_approvable_set_is_exactly_the_two_expected_codes() {
        let all = [
            DenyCode::SubnetConfigInvalid,
            DenyCode::SubnetNotAllowed,
            DenyCode::NotPaired,
            DenyCode::AutoReceiveOff,
            DenyCode::GrantInvalid,
            DenyCode::ScopeCheckFailed,
            DenyCode::ScopeDenied,
        ];
        let approvable: Vec<DenyCode> = all.iter().copied().filter(|c| c.is_approvable()).collect();
        assert_eq!(
            approvable,
            vec![DenyCode::NotPaired, DenyCode::AutoReceiveOff],
            "可审批救回的集合变了 —— 先问'是不是真的还能救回', 再改这里"
        );
        // 反向: 网段 / 范围这两类**永远**不能靠点"允许"绕过。
        // 它们是"用户根本没授权过这件事"而不是"这次忘了点"。
        for c in [
            DenyCode::SubnetNotAllowed,
            DenyCode::ScopeDenied,
            DenyCode::SubnetConfigInvalid,
        ] {
            assert!(!c.is_approvable(), "{:?} 不该能被审批救回", c);
        }
    }

    /// 守卫：`Decision::deny` 造出来的 `approvable` 必须与 code 一致。
    ///
    /// 这条是上一条的兜底: 上一条钉住 `DenyCode::is_approvable`,
    /// 这条钉住 `Decision` 真的**用了**它 —— 否则有人给 `Deny` 加回
    /// 一个独立的 `approvable` 字段并传错值，测试全绿而线上出错。
    #[test]
    fn decision_deny_reports_approvable_from_its_code() {
        for c in [
            DenyCode::NotPaired,
            DenyCode::AutoReceiveOff,
            DenyCode::ScopeDenied,
        ] {
            let d = Decision::deny(c, "随便一句文案");
            assert_eq!(d.is_approvable_deny(), c.is_approvable(), "{:?}", c);
            // 非拒绝的两态必须一律 false, 否则 `is_approvable_deny`
            // 会在 match 漏分支时静默变成 true
            assert!(!Decision::Allow.is_approvable_deny());
            assert!(!Decision::RequireGrant.is_approvable_deny());
        }
    }

    /// 守卫：控制流**不得**依赖中文文案。
    ///
    /// 本仓库已经因为这个模式出过两次事故：
    /// 1. `server.rs` 的 `Deny(r) if r.contains("未配对") || …`
    ///    —— 把"弹审批还是硬拒绝"挂在措辞上；
    /// 2. `client.rs` 的 `resp.message.contains("传输码")`
    ///    —— 把"这是协商回合还是最终拒绝"挂在措辞上。
    ///
    /// 两处的根因都是**类型缺字段**（`Deny` 只有文案、
    /// `PullResponse` 没有 `requires_grant_code`），所以现在字段补齐了。
    /// 这条守卫防止第三次：扫 `core/src` 下所有 `.rs`，
    /// 找出"用中文字面量做 `contains`"的行（注释行除外 ——
    /// 注释里引用旧代码是刻意留的证据）。
    #[test]
    fn no_control_flow_on_chinese_wording() {
        fn is_comment(line: &str) -> bool {
            let t = line.trim_start();
            t.starts_with("//") || t.starts_with('*') || t.starts_with("/*")
        }
        let src_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders: Vec<String> = Vec::new();
        let mut stack = vec![src_root];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if p.extension().and_then(|s| s.to_str()) != Some("rs") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&p) else {
                    continue;
                };
                for (i, line) in text.lines().enumerate() {
                    if is_comment(line) {
                        continue;
                    }
                    if line.contains("contains(\"") && line.chars().any(|c| c as u32 >= 0x4E00) {
                        offenders.push(format!(
                            "{}:{}: {}",
                            p.file_name().unwrap_or_default().to_string_lossy(),
                            i + 1,
                            line.trim()
                        ));
                    }
                }
            }
        }
        offenders.sort();
        assert!(
            offenders.is_empty(),
            "控制流不得依赖中文措辞（这些行用中文字面量做 contains）:\n{}",
            offenders.join("\n")
        );
}

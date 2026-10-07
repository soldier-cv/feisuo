//! 设备名册：**在线表 ∪ 信任库**。
//!
//! # 解决什么问题（设计文档 §3.4~§3.7）
//!
//! 旧实现里侧栏只消费 `get_online_devices()`, 而那张表是
//! "最近 20 秒内收到过信标"的纯在线表（`discovery/mod.rs` 的
//! `DEVICE_TTL_SECS = 20` + `map.retain(...)`）。
//! 于是配对过的设备一旦离线就**从列表彻底消失**，用户无法区分
//! 下面四种完全不同的情况：
//!
//! 1. 它只是关机了
//! 2. 信任被解除了 / 配对丢了
//! 3. 我把它隐藏了（其实在抽屉里）
//! 4. **发现服务被防火墙挡住了** ← 唯一真正需要报警的故障
//!
//! 第 4 种和前三种长得一模一样，这就是"静默消失 = 假阴性"的危害。
//!
//! # 本模块的做法
//!
//! 侧栏数据源改成**两个表的并集**：
//!
//! - 在**在线表**里 → `Presence::Online`，带实时 IP / 端口
//! - **只在信任库**里 → `Reconnecting` / `Offline`，带 `last_seen_at`
//!
//! 于是"信任设备离线"变成**灰显 + 上次在线时间**，而不是消失；
//! 而"未配对设备离线"仍然不显示（它们没有持久关系，留着只是噪音）。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use crate::discovery::DiscoveredDevice;
use crate::security::trust_model::{Presence, TrustLevel};
use crate::security::{TrustStore, TrustedDevice};

/// 名册条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRosterEntry {
    pub device_id: String,
    pub device_name: String,
    pub os_type: String,

    /// 信任等级
    pub trust_level: TrustLevel,
    /// 是否在主列表可见
    pub visible: bool,
    /// 在线状态
    pub presence: Presence,

    /// 最后一次收到其信标的 Unix 秒。0 = 未知（**不得**显示成"刚刚在线"）
    pub last_seen_at: i64,
    /// 相对当前时间的"已离线多久"，人话格式
    pub last_seen_human: String,

    /// 仅 `Online` 时有值
    pub ip: Option<String>,
    pub transfer_port: Option<u16>,
    /// 信任库里的历史 IP（离线设备的唯一可用地址线索）
    pub last_ip: String,

    /// 是否已配对（便于前端分组）
    pub is_paired: bool,
    /// 本机自身
    pub is_self: bool,
    /// 对端能力位（§7.3）。离线设备为 0 —— 上次在线时的能力不代表
    /// 它现在还支持，所以**不能**从信任库里猜，必须以在线信标为准。
    #[serde(default)]
    pub peer_caps: u32,
    /// 对端应用版本（仅展示与诊断）
    #[serde(default)]
    pub peer_version: String,
}

impl DeviceRosterEntry {
    /// 是否可作为拖放发送目标。
    ///
    /// **UI 的在线态只是提示，不是事实** —— 真正的判定发生在松手瞬间的
    /// 实际连通性探测（§3.8）。所以 `Reconnecting` 也允许尝试投放。
    pub fn accepts_drop(&self) -> bool {
        self.presence.accepts_drop()
    }
}

/// 把 Unix 秒转成"多久之前"的人话。
fn humanize_since(ts: i64, now: i64) -> String {
    if ts <= 0 {
        return "未知".to_string();
    }
    let d = now - ts;
    if d < 0 {
        return "刚刚".to_string();
    }
    if d < 60 {
        format!("{} 秒前", d)
    } else if d < 3600 {
        format!("{} 分钟前", d / 60)
    } else if d < 86400 {
        format!("{} 小时前", d / 3600)
    } else {
        format!("{} 天前", d / 86400)
    }
}

/// 合并在线表与信任库，产出名册。
///
/// 排序规则（让"我的设备"自然浮到上面）：
/// 1. 可见性（隐藏的排到最后，由 UI 放进抽屉）
/// 2. 本机在前
/// 3. 已配对在前
/// 4. 在线在前
/// 5. 名称
pub fn build_roster(
    online: &[DiscoveredDevice],
    trust_devices: &[TrustedDevice],
    self_device_id: &str,
    now: i64,
) -> Vec<DeviceRosterEntry> {
    let mut by_id: HashMap<String, DeviceRosterEntry> = HashMap::new();

    // ---- 先放信任库里的全部设备（**含离线**）----
    for d in trust_devices {
        let presence = if !d.visible {
            Presence::Hidden
        } else {
            Presence::from_last_seen(d.last_seen_at, now)
        };
        by_id.insert(
            d.device_id.clone(),
            DeviceRosterEntry {
                device_id: d.device_id.clone(),
                device_name: d.device_name.clone(),
                os_type: String::new(),
                trust_level: d.trust_level,
                visible: d.visible,
                presence,
                last_seen_at: d.last_seen_at,
                last_seen_human: humanize_since(d.last_seen_at, now),
                ip: None,
                transfer_port: None,
                last_ip: d.last_ip.clone(),
                is_paired: d.trust_level.is_paired(),
                is_self: d.device_id == self_device_id,
                peer_caps: 0,
                peer_version: String::new(),
            },
        );
    }

    // ---- 再用在线表覆盖（在线信息更准）----
    for o in online {
        let trust_level = trust_devices
            .iter()
            .find(|d| d.device_id == o.device_id)
            .map(|d| d.trust_level)
            .unwrap_or(TrustLevel::Pending);
        let visible = trust_devices
            .iter()
            .find(|d| d.device_id == o.device_id)
            .map(|d| d.visible)
            .unwrap_or(true);
        let entry = DeviceRosterEntry {
            device_id: o.device_id.clone(),
            device_name: o.device_name.clone(),
            os_type: o.os_type.clone(),
            trust_level,
            visible,
            // 在线表里的设备必然刚刚收发过信标
            presence: if !visible {
                Presence::Hidden
            } else {
                Presence::Online
            },
            last_seen_at: now,
            last_seen_human: "刚刚".to_string(),
            ip: Some(o.ip.clone()),
            transfer_port: Some(o.transfer_port),
            last_ip: o.ip.clone(),
            is_paired: trust_level.is_paired(),
            is_self: o.device_id == self_device_id,
            peer_caps: o.caps,
            peer_version: o.app_version.clone(),
        };
        by_id.insert(o.device_id.clone(), entry);
    }

    let mut list: Vec<DeviceRosterEntry> = by_id.into_values().collect();
    list.sort_by(|a, b| {
        a.visible
            .cmp(&b.visible)
            .then_with(|| b.is_self.cmp(&a.is_self))
            .then_with(|| b.is_paired.cmp(&a.is_paired))
            .then_with(|| online_rank(a.presence).cmp(&online_rank(b.presence)))
            .then_with(|| a.device_name.cmp(&b.device_name))
    });
    list
}

fn online_rank(p: Presence) -> u8 {
    match p {
        Presence::Online => 0,
        Presence::Reconnecting => 1,
        Presence::Offline => 2,
        Presence::Hidden => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::TrustLevel;

    fn trust_row(id: &str, level: TrustLevel, last_seen: i64, visible: bool) -> TrustedDevice {
        TrustedDevice {
            device_id: id.to_string(),
            device_name: format!("node-{}", id),
            public_key_hex: "00".repeat(32),
            last_ip: "192.168.1.9".into(),
            bound_at: "2026-01-01T00:00:00Z".into(),
            is_trusted: level.is_paired(),
            trust_level: level,
            visible,
            last_seen_at: last_seen,
            pairing_epoch: "e1".into(),
        }
    }

    fn online_row(id: &str) -> DiscoveredDevice {
        DiscoveredDevice {
            device_id: id.to_string(),
            device_name: format!("node-{}", id),
            is_trusted: false,
            os_type: "windows".into(),
            ip: "192.168.1.9".into(),
            transfer_port: 42100,
            last_seen_secs: 0,
            caps: 7,
            app_version: "0.1.0".into(),
        }
    }

    /// 解除配对之后，名册里那台设备**仍然**要显示为在线。
    ///
    /// 这条守的是一个容易被"顺手修掉"的假 bug：
    /// 解除配对会把 `trust_level` 打成 `pending`，而发现侧只对
    /// **已配对**设备调用 `mark_seen`（`discovery/mod.rs`）——
    /// 于是落库的 `last_seen_at` **冻结在解除的那一刻**。
    /// 只看 `trusted_devices` 表的话，这台明明在网上的设备会显示成
    /// "上次在线 3 天前"。
    ///
    /// 之所以没出问题，是因为名册是 **在线表 ∪ 信任库**，而在线表那一路
    /// 会用 `last_seen_at: now` 覆盖掉冻结值（`build_roster` 的第二步）。
    /// 设备一旦真的离线，才会回落到那个冻结值 —— 那时它显示的是
    /// "上次在线 <解除时刻>"，**偏旧但不为错**。
    ///
    /// ❌ **不要**为了让那个时间准确就把 `mark_seen` 挪出验签门：
    /// 那样任何知道 `device_id` 的主机都能往库里写 `last_ip` / `last_seen_at`，
    /// 把受害者记录污染到自己机器上 —— 正是 `discovery/mod.rs` 里
    /// "只有验签通过的受信设备才允许更新 last_ip" 那条注释防的事。
    /// **用一点显示精度换签名校验，这笔交易是划算的。**
    #[test]
    fn unpaired_but_online_device_still_shows_as_online() {
        let now = 1_800_000_000;
        // 落库的 last_seen_at 冻结在 3 天前（解除配对的那一刻）
        let trust = vec![trust_row("peer", TrustLevel::Pending, now - 3 * 86_400, true)];
        let online = vec![online_row("peer")];

        let roster = build_roster(&online, &trust, "self", now);
        assert_eq!(roster.len(), 1);
        let e = &roster[0];
        assert_eq!(
            e.presence,
            Presence::Online,
            "解除配对后设备仍在发信标，名册必须显示在线（落库的 last_seen_at 已冻结）"
        );
        assert_eq!(e.last_seen_at, now, "在线表那一路应当用 now 覆盖冻结值");
        assert!(!e.is_paired, "信任档位确实是未信任");
        assert_eq!(e.trust_level, TrustLevel::Pending);
    }

    /// 对照：设备真的离线之后，名册才回落到落库的时间。
    ///
    /// 这条是为了让上一条的"覆盖"逻辑有个反面锚点 ——
    /// 免得有人把"在线表覆盖"改成"永远用落库值"。
    #[test]
    fn offline_unpaired_device_falls_back_to_the_persisted_timestamp() {
        let now = 1_800_000_000;
        let trust = vec![trust_row("peer", TrustLevel::Pending, now - 3 * 86_400, true)];

        let roster = build_roster(&[], &trust, "self", now);
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].presence, Presence::Offline);
        assert_eq!(roster[0].last_seen_at, now - 3 * 86_400);
    }

    /// 隐藏优先于在线：被隐藏的设备即使正在发信标，名册也必须显示「已隐藏」。
    ///
    /// 这是 I6（`hidden` 不改变准入、但改变**呈现**）在名册层的落点。
    /// 早先的 `TrustLevel::Blocked` 分支抢在 `!visible` 之前，
    /// 于是"被阻止"和"被隐藏"在名册里长成同一个态 —— 那是该维度
    /// 必须删掉的另一个理由。
    #[test]
    fn hidden_beats_online_in_the_roster() {
        let now = 1_800_000_000;
        let trust = vec![trust_row("peer", TrustLevel::Permanent, now, false)];
        let online = vec![online_row("peer")];

        let roster = build_roster(&online, &trust, "self", now);
        assert_eq!(roster[0].presence, Presence::Hidden);
        // 信任强度不受隐藏影响 —— 隐藏是显示偏好，不是信任变更
        assert_eq!(roster[0].trust_level, TrustLevel::Permanent);
        assert!(roster[0].is_paired);
    }

    /// 隐藏也优先于**离线**判定。
    ///
    /// 上一条守的是「隐藏 + 在线」，这一条守「隐藏 + 离线」——
    /// 也就是**「已隐藏」抽屉**真正要处理的场景：你把一台设备隐藏了，
    /// 然后它下线了。此时名册必须仍然说「已隐藏」，
    /// 否则那台设备会**从抽屉里掉回主列表**，用户刚藏起来的东西
    /// 又出现在眼前 —— 而它当时隐藏的理由很可能正是"不想看见它"。
    ///
    /// 上一条漏掉的就是这一半：在线表那一路覆盖掉了信任库那一路的
    /// 隐藏判定，于是只有「隐藏 + 在线」被守住，「隐藏 + 离线」没人管。
    /// 这是变异测试逼出来的，不是推测出来的。
    #[test]
    fn hidden_also_beats_offline_in_the_roster() {
        let now = 1_800_000_000;
        let trust = vec![trust_row("peer", TrustLevel::Permanent, now - 86_400, false)];

        let roster = build_roster(&[], &trust, "self", now);
        assert_eq!(roster[0].presence, Presence::Hidden);
        // 但"上次在线"仍然如实显示 —— 隐藏的是呈现，不是事实
        assert_eq!(roster[0].last_seen_at, now - 86_400);
        assert_eq!(roster[0].last_seen_human, humanize_since(now - 86_400, now));
    }
}

/// 从引擎侧构建名册的便捷入口。
pub async fn roster_from(
    discovery: &Arc<crate::discovery::DiscoveryService>,
    trust_store: &Arc<TrustStore>,
    self_device_id: &str,
) -> crate::error::Result<Vec<DeviceRosterEntry>> {
    let online = discovery.get_online_devices().await;
    let trusted = trust_store.list_devices()?;
    let now = chrono::Utc::now().timestamp();
    Ok(build_roster(&online, &trusted, self_device_id, now))
}

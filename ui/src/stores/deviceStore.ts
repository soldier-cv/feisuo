import { defineStore } from "pinia";
import { FeisuoBridge, DeviceRosterEntry, Presence } from "../api/feisuoBridge";

function readStoredTheme(): "dark" | "light" {
  return localStorage.getItem("feisuo-theme") === "light" ? "light" : "dark";
}

export const useDeviceStore = defineStore("devices", {
  state: () => ({
    /**
     * 设备名册（在线表 ∪ 信任库）。
     *
     * 为什么不是 `getOnlineDevices()`：那张表只有 20 秒 TTL，
     * 配对过的设备离线就会从列表彻底消失，用户无法区分
     * 「它关机了」和「发现服务被防火墙挡了」——后者才需要报警。
     */
    roster: [] as DeviceRosterEntry[],
    /** 已隐藏设备（「已隐藏」抽屉） */
    hidden: [] as DeviceRosterEntry[],
    selectedDeviceId: "",
    theme: readStoredTheme() as "dark" | "light",
    /** 「其他」分组是否折叠（默认折叠，解决办公室 20 台设备的噪音） */
    othersCollapsed: true,
  }),

  getters: {
    /** 我的设备：已配对且可见的设备（**含离线**），永不为空到消失 */
    myDevices: (state): DeviceRosterEntry[] =>
      state.roster.filter((d) => d.is_paired && d.visible),

    /** 附近的陌生设备：仅在线的未配对设备（离线不显示，它们没有持久关系） */
    nearbyDevices: (state): DeviceRosterEntry[] =>
      state.roster.filter((d) => !d.is_paired && d.visible && d.presence === "online"),

    hiddenDevices: (state): DeviceRosterEntry[] => state.roster.filter((d) => !d.visible),

    selectedDevice: (state): DeviceRosterEntry | undefined =>
      state.roster.find((d) => d.device_id === state.selectedDeviceId)
      ?? state.roster.find((d) => d.is_paired && d.visible)
      ?? state.roster[0],

    /** 兼容旧代码：在线设备数 */
    onlineCount: (state): number =>
      state.roster.filter((d) => d.presence === "online" && d.visible).length,
  },

  actions: {
    async refreshDevices() {
      try {
        this.roster = await FeisuoBridge.getDeviceRoster();
      } catch {
        // 名册拿不到时退回在线表, 至少别让侧栏空掉。
        // 在线表只有 `is_trusted`，分不出「永久信任」和「每次匹配码」。
        // 把已信任一律写成 permanent，名册一失败，每次匹配码就会静默收文件。
        // 信任库还在时用它的真实等级；两边都失败才退回 pending，宁可多确认一次。
        let trustedLevel = new Map<string, DeviceRosterEntry["trust_level"]>();
        try {
          const trusted = await FeisuoBridge.getTrustedDevices();
          trustedLevel = new Map(trusted.map((d) => [d.device_id, d.trust_level]));
        } catch {
          trustedLevel = new Map();
        }
        const online = await FeisuoBridge.getOnlineDevices();
        this.roster = online.map((d) => {
          const level = trustedLevel.get(d.device_id) ?? "pending";
          return {
            device_id: d.device_id,
            device_name: d.device_name,
            os_type: d.os_type,
            trust_level: level,
            visible: true,
            presence: "online" as Presence,
            last_seen_at: Math.floor(Date.now() / 1000),
            last_seen_human: "刚刚",
            ip: d.ip,
            transfer_port: d.transfer_port,
            last_ip: d.ip,
            is_paired: level === "permanent" || level === "session",
            is_self: false,
            // 退回在线表时也要带上 caps —— 否则名册接口一失败，
            // 穿梭右栏就会以为所有设备都是 1.x，退回收件目录视图。
            peer_caps: d.caps,
            peer_version: d.app_version,
          };
        });
      }
      // 选中的设备离线时**不**自动切走 —— 灰显保留比突然换目标更安全（§3.7）
      if (
        this.selectedDeviceId &&
        !this.roster.some((d) => d.device_id === this.selectedDeviceId)
      ) {
        this.selectedDeviceId = "";
      }
    },

    async toggleHidden(deviceId: string) {
      const dev = this.roster.find((d) => d.device_id === deviceId);
      if (!dev) return;
      await FeisuoBridge.setDeviceVisible(deviceId, !dev.visible);
      await this.refreshDevices();
    },

    /** 幂等地应用主题: 同时写 <html> 与 localStorage, 供刷新后复用 */
    applyTheme(theme?: "dark" | "light") {
      const next = theme ?? (this.theme === "dark" ? "light" : "dark");
      this.theme = next;
      document.documentElement.dataset.theme = next;
      // color-scheme 必须跟着主题一起改, 否则切换后原生滚动条/表控件
      // 仍是上一套皮肤 —— 表现为"界面已经变浅色, 滚动条还是黑的"。
      // 见 main.ts 里同名逻辑的说明。
      document.documentElement.style.colorScheme = next;
      localStorage.setItem("feisuo-theme", next);
    },
  },
});

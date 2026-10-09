<template>
  <div class="desktop-shell" :data-theme="store.theme">
    <!-- 1. Windows 原生标题栏 -->
    <header class="app-titlebar">
      <div class="titlebar-drag-area" data-tauri-drag-region>
        <div class="brand-badge">
          <img src="/favicon.svg" alt="飞梭" class="brand-logo-img" />
        </div>
        <!--
          h1 = 应用本身。此前全篇没有 h1，标题从 h3 起跳：
          屏幕阅读器按标题跳转的用户拿不到任何顶层锚点，
          也无从判断「我现在在哪个应用里」。
          改成 h1 在视觉上零影响 —— .titlebar-drag-area 是 flex 容器，
          span 与 h1 都是 flex 项，块级化行为一致；.brand-text 自己
          声明了 font-size/font-weight/letter-spacing，而 margin 已由
          `* { margin: 0 }` 全局归零。
        -->
        <h1 class="brand-text">飞梭 Feisuo</h1>
        <div class="lan-network-chip">
          <span class="live-dot"></span>
          <span>局域网在线 · {{ localInfo.local_ip }}</span>
        </div>
      </div>

      <!-- 原生 caption 按钮: 关闭会先弹"最小化 / 退出"选择框 -->
      <div class="caption-controls" @mousedown.stop>
        <button class="caption-btn" @click.stop="toggleTheme" title="切换深色/浅色外观">
          <i :class="store.theme === 'dark' ? 'ph ph-sun' : 'ph ph-moon'"></i>
        </button>
        <button class="caption-btn" @click.stop="minimizeWindow" title="最小化">
          <i class="ph ph-minus"></i>
        </button>
        <button class="caption-btn" @click.stop="toggleMaximize" title="最大化 / 还原">
          <i class="ph ph-square"></i>
        </button>
        <button class="caption-btn close-caption" @click.stop="requestClose" title="关闭">
          <i class="ph ph-x"></i>
        </button>
      </div>
    </header>

    <!-- 2. 主区域 -->
    <div class="app-container">
      <!-- 左侧: 本机 + 设备列表 -->
      <aside class="app-sidebar">
        <div
          class="local-machine-card"
          :class="{ selected: currentTab === 'settings' }"
          v-key-activate
          role="button"
          tabindex="0"
          :aria-pressed="currentTab === 'settings'"
          @click="currentTab = 'settings'"
          title="系统设置"
        >
          <div class="local-icon">{{ (localInfo.device_name || 'H').slice(0, 1).toUpperCase() }}</div>
          <div class="local-meta">
            <strong class="machine-name">{{ localInfo.device_name || '本机电脑' }}</strong>
            <span class="daemon-status">
              <span class="mini-status-dot" :class="{ active: engineOnline }"></span>
              {{ engineOnline ? '在线 · ' + localInfo.local_ip : '守护进程未就绪' }}
            </span>
          </div>
          <!--
            这里原来有个齿轮按钮，与整块卡片的点击行为**完全重复**
            （两者都是 `currentTab = 'settings'`），而页签里已经有「系统设置」。
            同一目的地放两个入口只增加决策成本：用户要判断"这两个有什么区别"。
            已删除。
          -->
        </div>

        <div class="sidebar-title-bar">
          <span>我的设备 ({{ store.myDevices.length }})</span>
          <div class="sidebar-header-actions">
            <button class="icon-action-btn" @click="openDirectConnectModal" title="直连 IP">
              <i class="ph ph-plus"></i>
            </button>
            <button class="icon-action-btn" @click="refreshDevices()" title="刷新设备">
              <i class="ph ph-arrows-clockwise" :class="{ spinning: isRefreshing }"></i>
            </button>
          </div>
        </div>

        <div class="devices-scroll-area">
          <!--
            分组而不是一锅端（§3.7）：「我的设备」= 信任库全量, **含离线**
            （灰显 + 上次在线时间）, 永不消失；「其他」= 仅在线的未配对设备, 可折叠。
          -->
          <div class="roster-group-head">
            <span class="roster-group-title"><i class="ph-bold ph-shield-check"></i> 我的设备</span>
            <span class="roster-group-count">{{ store.myDevices.length }}</span>
          </div>
          <div
            v-for="dev in store.myDevices"
            :key="dev.device_id"
            class="device-card-row"
            :class="{
              active: dev.device_id === store.selectedDeviceId && currentTab !== 'settings',
              offline: dev.presence !== 'online',
              // §5.1 目标即动作：拖到卡片上高亮, 松手直接发
              'drop-target': dragOverDeviceId === dev.device_id && canDropTo(dev),
              'drop-blocked': dragOverDeviceId === dev.device_id && !canDropTo(dev),
            }"
            v-key-activate
            role="button"
            tabindex="0"
            :aria-pressed="dev.device_id === store.selectedDeviceId"
            :aria-disabled="dev.presence !== 'online'"
            @click="selectDevice(dev.device_id)"
            @dragover.prevent="onDeviceDragOver($event, dev)"
            @dragleave="onDeviceDragLeave(dev)"
            @drop.prevent="onDeviceDrop($event, dev)"
          >
            <div class="os-icon-box">
              <i :class="getOsIcon(dev.os_type)"></i>
            </div>
            <div class="device-text-col">
              <div class="device-line1">
                <span class="device-title">{{ dev.device_name }}</span>
                <span v-if="dev.is_self" class="self-tag">本机</span>
              </div>
              <div class="device-line2">
                <template v-if="dev.presence === 'online'">{{ dev.ip || dev.last_ip }} · 在线</template>
                <template v-else-if="dev.presence === 'reconnecting'">{{ dev.last_ip }} · 重连中…</template>
                <template v-else>{{ dev.last_ip || '地址未知' }} · 离线 · 上次在线 {{ dev.last_seen_human }}</template>
              </div>
            </div>

            <div class="device-action-badge">
              <!-- 信任等级（§2.4）：点击在「永久信任 / 每次匹配码」之间切换 -->
              <button
                v-if="dev.trust_level === 'permanent'"
                class="trust-badge clickable"
                title="永久信任：点击改为「每次匹配码」"
                @click.stop="toggleTrustLevel(dev.device_id)"
              >
                <i class="ph-bold ph-shield-check"></i> 永久信任
              </button>
              <button
                v-else-if="dev.trust_level === 'session'"
                class="trust-badge session clickable"
                :title="
                  activeGrants[dev.device_id]?.length
                    ? `每次匹配码。${activeGrants[dev.device_id].map((g) => g.scope_label).join('、')}已免重复确认至 ${humanizeUntil(activeGrants[dev.device_id][0].expires_at)}。点击改为「永久信任」`
                    : '每次匹配码：每次操作都需出示授权码。点击改为「永久信任」'
                "
                @click.stop="toggleTrustLevel(dev.device_id)"
              >
                <i class="ph-bold ph-key"></i> 每次验证
              </button>
              <!--
                生效中的短期授权（§2.3.1）：必须**看得见、关得掉**。

                刻意做成**兄弟**按钮，而不是在徽标里嵌一个可点的 `<span>`：
                ① 按钮里套按钮是**无效 HTML**；
                ② `<span @click>` **键盘不可达** —— Tab 走不到、Enter 没反应，
                   而「关掉授权」恰恰是**只能用手点**的路径。

                这不是我想到了才改的，是守卫
                `clickable_non_button_elements_must_be_keyboard_reachable`
                实测报红才改的 —— 可见性做了一半（看得见）、
                可撤销做成了鼠标专属。
              -->
              <button
                v-if="dev.trust_level === 'session' && activeGrants[dev.device_id]?.length"
                class="grant-active-dot"
                :aria-label="`取消 ${dev.device_name} 的免重复确认`"
                :title="
                  activeGrants[dev.device_id]
                    .map((g) => `${g.scope_label}免确认至 ${humanizeUntil(g.expires_at)}`)
                    .join('；') + '。点击立即取消'
                "
                @click.stop="revokeDeviceGrants(dev.device_id, dev.device_name)"
              >
                <i class="ph-bold ph-timer"></i>
              </button>
              <!-- 隐藏（§3.1）：不想再看到它，但保留已建立的信任与推送 -->
              <button
                class="hide-device-btn"
                title="隐藏该设备（仍可接收其推送）"
                @click.stop="hideDevice(dev.device_id)"
              >
                <i class="ph ph-eye-slash"></i>
              </button>
            </div>
          </div>

          <div v-if="store.myDevices.length === 0" class="roster-empty-hint">
            还没有已配对的设备。在下面的「其他」里找到目标设备后点「配对」。
          </div>

          <div
            v-if="store.nearbyDevices.length > 0"
            class="roster-group-head collapsible"
            v-key-activate
            role="button"
            tabindex="0"
            :aria-expanded="!store.othersCollapsed"
            @click="store.othersCollapsed = !store.othersCollapsed"
          >
            <span class="roster-group-title">
              <i class="ph ph-caret-right" :class="{ rotated: !store.othersCollapsed }"></i>
              其他（附近的陌生设备）
            </span>
            <span class="roster-group-count">{{ store.nearbyDevices.length }}</span>
          </div>

          <div
            v-for="dev in store.nearbyDevices"
            v-show="!store.othersCollapsed"
            :key="dev.device_id"
            class="device-card-row nearby"
            v-key-activate
            role="button"
            tabindex="0"
            :aria-pressed="dev.device_id === store.selectedDeviceId"
            @click="selectDevice(dev.device_id)"
          >
            <div class="os-icon-box">
              <i :class="getOsIcon(dev.os_type)"></i>
            </div>
            <div class="device-text-col">
              <div class="device-line1"><span class="device-title">{{ dev.device_name }}</span></div>
              <div class="device-line2">{{ dev.ip }} · 在线</div>
            </div>
            <div class="device-action-badge">
              <button class="btn-quick-pair" @click.stop="pairDevice(dev)">配对</button>
            </div>
          </div>

          <div v-if="store.roster.length === 0" class="discovery-empty-state">
            <div class="radar-pulse-ring"><i class="ph ph-broadcast"></i></div>
            <!--
              这里原本是 <h4>，但它不是章节标题 —— 是一句**会变的状态提示**。
              设备一搜到，这个元素整个消失，它作为标题就会在大纲里
              凭空出现又凭空消失：按标题跳转的用户会撞到一个不存在的锚点。
              改成 role="status" 的段落，状态变化仍会被朗读（aria-live=polite），
              但不再污染文档大纲。
            -->
            <p role="status" class="discovery-status-text">正在搜索附近设备…</p>
            <div class="empty-actions-row">
              <button class="btn-clean-primary" @click="openDirectConnectModal">
                <i class="ph ph-plus"></i> 直连对端 IP
              </button>
              <button class="btn-clean-subtle" @click="openPairModal('input')">
                <i class="ph ph-key"></i> 配对码
              </button>
            </div>
            <!--
              防火墙引导。

              为什么需要它（实测得来的，不是设想的）：首次运行绿色版时，
              Windows 会弹出「是否要允许 feisuo-desktop.exe 访问网络」。
              用户不点"允许"，这台机器就**收不到任何文件**，而界面上
              只会一直转着"正在搜索附近设备…" —— 没有任何地方说明原因。

              而**升级之后还会再弹一次**：Windows 防火墙规则按**程序路径**
              匹配，绿色版每次更新 exe 都会换掉路径，于是规则失效、
              需要重新授权。所以这不是"装一次点一次"就忘了的事。

              为什么**不自动改防火墙**：改防火墙要管理员权限，而且用户
              有权知道自己的机器上被放行了什么 —— 静默加规则等于
              绕过他们的判断。所以这里只说清"要做什么"和"为什么"。

              只在"搜了很久还一台都没有"时出现，不打扰正常用户。
            -->
            <div v-if="showFirewallHint" class="firewall-hint">
              <i class="ph ph-shield-warning"></i>
              <div class="firewall-hint-body">
                <strong>一直搜不到设备？看看 Windows 防火墙</strong>
                <p>
                  首次运行与每次升级后，Windows 都会询问是否允许本程序访问网络。
                  没点「允许」的话，本机<strong>收不到</strong>任何文件。
                </p>
                <p class="firewall-hint-how">
                  若已弹过窗且点了拒绝：开始菜单搜「Windows 安全中心」→
                  防火墙和网络保护 → 允许应用通过防火墙 → 勾选
                  <code>feisuo-desktop</code>。
                </p>
              </div>
            </div>
          </div>
        </div>

        <div class="sidebar-pinned-footer">
          <!-- 已隐藏抽屉（§3.1）：隐藏是**纯显示**偏好，与信任强度正交 -->
          <button
            v-if="store.hiddenDevices.length > 0 || showHiddenDrawer"
            class="btn-open-pairing hidden-drawer-toggle"
            @click="showHiddenDrawer = !showHiddenDrawer"
          >
            <i class="ph ph-eye-slash"></i>
            <span>已隐藏 ({{ store.hiddenDevices.length }})</span>
            <i class="ph ph-caret-down" :class="{ rotated: showHiddenDrawer }"></i>
          </button>

          <div v-if="showHiddenDrawer && store.hiddenDevices.length > 0" class="hidden-drawer">
            <p class="hidden-drawer-hint">
              隐藏只是不再显示，<strong>已建立的信任与推送接收都保留</strong>。
            </p>
            <div
              v-for="dev in store.hiddenDevices"
              :key="dev.device_id"
              class="hidden-device-row"
            >
              <div class="device-text-col">
                <div class="device-line1"><span class="device-title">{{ dev.device_name }}</span></div>
                <div class="device-line2">
                  {{ dev.presence === 'online' ? '在线' : '离线 · 上次在线 ' + dev.last_seen_human }}
                </div>
              </div>
              <button class="btn-quick-pair" @click="store.toggleHidden(dev.device_id)">取消隐藏</button>
            </div>
          </div>

          <button class="btn-open-pairing" @click="openPairModal('show')">
            <i class="ph-bold ph-qr-code"></i>
            <span>查看本机配对码</span>
          </button>
          <div class="save-dir-card">
            <div class="dir-header">
              <span>文件保存目录</span>
              <button type="button" class="dir-open-btn" @click="openReceiveFolder">打开</button>
            </div>
            <div
              class="dir-path-string"
              :title="localInfo.receive_dir + '（点击复制）'"
              v-key-activate
              role="button"
              tabindex="0"
              @click="copyText(localInfo.receive_dir)"
            >
              {{ localInfo.receive_dir }}
            </div>
          </div>
        </div>
      </aside>

      <!-- 右侧工作区 -->
      <main class="app-workspace">
        <div class="workspace-tabs-bar">
          <div class="segmented-tabs" role="tablist" aria-label="工作区视图">
            <button
              role="tab"
              :aria-selected="currentTab === 'send'"
              :class="{ active: currentTab === 'send' }"
              @click="currentTab = 'send'"
            >
              <i class="ph-bold ph-paper-plane-right"></i> 发送文件
            </button>
            <button
              role="tab"
              :aria-selected="currentTab === 'shuttle'"
              :class="{ active: currentTab === 'shuttle' }"
              @click="currentTab = 'shuttle'"
            >
              <i class="ph-bold ph-arrows-left-right"></i> 双栏穿梭
            </button>
            <button
              role="tab"
              :aria-selected="currentTab === 'log'"
              :class="{ active: currentTab === 'log' }"
              @click="currentTab = 'log'"
            >
              <i class="ph-bold ph-clock-counter-clockwise"></i> 传输记录
            </button>
            <button
              role="tab"
              :aria-selected="currentTab === 'settings'"
              :class="{ active: currentTab === 'settings' }"
              @click="currentTab = 'settings'"
            >
              <i class="ph-bold ph-sliders-horizontal"></i> 系统设置
            </button>
          </div>

          <div class="current-target-pill" v-if="currentTab !== 'settings'">
            <span class="target-prefix">目标</span>
            <strong class="target-name">{{ store.selectedDevice?.device_name || '未选择设备' }}</strong>
            <span
              v-if="store.selectedDevice?.trust_level === 'permanent'"
              class="status-chip trusted"
              title="永久信任：对方投递文件不会弹窗确认"
              >已信任</span
            >
            <span
              v-else-if="store.selectedDevice?.trust_level === 'session'"
              class="status-chip session"
              title="每次匹配码：对方每次操作都需出示授权码"
              >每次验证</span
            >
            <button
              v-else-if="store.selectedDevice"
              class="status-chip untrusted"
              @click="pairDevice(store.selectedDevice)"
            >
              未配对 · 点击配对
            </button>
            <span
              v-if="store.selectedDevice && store.selectedDevice.presence !== 'online'"
              class="status-chip offline"
              :title="'上次在线：' + store.selectedDevice.last_seen_human"
            >
              <i class="ph ph-plugs"></i>
              {{ store.selectedDevice.presence === 'reconnecting' ? '重连中' : '离线' }}
            </span>
            <!--
              访问范围配置（需求 ④「可访问范围配置」）。

              ## 为什么必须有这个入口

              `AccessScope` 的模型、判定、落库、Tauri 命令**早就都有了**，
              但整个前端**没有任何地方调用** `getAccessScope` / `setAccessScope`
              —— 也就是说这个功能对用户**根本不存在**：他打开飞梭，看不到任何
              "这台设备能看什么、能拿什么"的入口，只能被动接受默认的"全部"。

              而"给了模型却没给入口"比"没有这个功能"更糟：文档里写着
              "可多选盘符"，用户去找，找不到 —— 于是他要么以为功能不存在，
              要么以为**默认已经收窄了**（实际是全开的）。后者是要命的：
              他会以为同事看不到他的 D 盘。

              面板刻意放在**头部信任等级旁边**而不是设置页深处：信任等级与
              访问范围是同一件事的两个维度（谁能连进来 / 连进来能干什么），
              拆到两处会让用户不知道该在哪里改。
            -->
            <button
              v-if="store.selectedDevice"
              class="scope-open-btn"
              :class="{ 'is-warn': scopeIsRestricted }"
              title="设置这台设备能浏览哪些盘符、能否取回、能否写入"
              @click="openScopeEditor"
            >
              <i class="ph ph-sliders-horizontal"></i> 访问范围
            </button>
          </div>

          <!-- 访问范围编辑弹层 -->
          <div
            v-if="scopeEditorOpen"
            class="modal-mask"
            @click.self="closeScopeEditor"
          >
            <div class="scope-modal">
              <div class="scope-head">
                <h3>
                  <i class="ph ph-sliders-horizontal"></i>
                  {{ store.selectedDevice?.device_name }} 的访问范围
                </h3>
                <button class="scope-close" @click="closeScopeEditor" title="关闭">
                  <i class="ph ph-x"></i>
                </button>
              </div>

              <div class="scope-body" v-if="scopeDraft">
                <!-- 访问模式 -->
                <div class="scope-field">
                  <p class="scope-label">可访问范围</p>
                  <div class="scope-modes">
                    <label
                      v-for="m in scopeModes"
                      :key="m.value"
                      class="scope-mode"
                      :class="{ 'is-on': scopeDraft.mode === m.value }"
                      :title="m.tip"
                    >
                      <input type="radio" :value="m.value" v-model="scopeDraft.mode" />
                      <span class="scope-mode-title">{{ m.label }}</span>
                    </label>
                  </div>
                </div>

                <!-- 全部模式视图 -->
                <div class="scope-field" v-if="scopeDraft.mode === 'all'">
                  <div class="scope-status-chip">
                    <i class="ph-fill ph-check-circle"></i>
                    <span>所有盘符与目录均允许访问（不设系统目录限制）</span>
                  </div>
                </div>

                <!-- 仅收件目录视图 -->
                <div class="scope-field" v-if="scopeDraft.mode === 'receive_only'">
                  <div class="scope-status-chip">
                    <i class="ph-fill ph-tray"></i>
                    <span>仅允许访问收件目录：<code class="path-mono">{{ localInfo.receive_dir }}</code></span>
                  </div>
                </div>

                <!-- 白名单模式视图 -->
                <div class="scope-field" v-if="scopeDraft.mode === 'allowlist'">
                  <p class="scope-sub-label">允许的盘符</p>
                  <div class="scope-volumes">
                    <label
                      v-for="v in localVolumes"
                      :key="v.id"
                      class="scope-vol"
                      :class="{ 'is-on': scopeDraft.allow_volumes?.includes(v.id) }"
                    >
                      <input
                        type="checkbox"
                        :checked="scopeDraft.allow_volumes?.includes(v.id)"
                        @change="toggleScopeVolume(v.id)"
                      />
                      <span>{{ v.label }}</span>
                      <span class="scope-vol-free" v-if="v.free_bytes > 0">
                        剩 {{ formatRemoteVolumeSize(v.free_bytes) }}
                      </span>
                    </label>
                  </div>

                  <p class="scope-sub-label" style="margin-top: 12px;">允许的指定目录</p>
                  <div class="path-input-group">
                    <input
                      type="text"
                      class="standard-text-input"
                      v-model="newScopeAllowPath"
                      placeholder="输入目录路径，例如 D:\Projects"
                      aria-label="输入允许访问的目录路径"
                      @keyup.enter="addScopeAllowPath()"
                    />
                    <button type="button" class="btn-clean-subtle" @click="pickFolderForAllow" title="从文件系统中选择目录">
                      <i class="ph ph-folder-open"></i> 选择目录
                    </button>
                    <button type="button" class="btn-clean-primary" @click="addScopeAllowPath()">
                      <i class="ph ph-plus"></i> 添加
                    </button>
                  </div>
                  <div class="path-tags-list" v-if="scopeDraft.allow_paths && scopeDraft.allow_paths.length > 0">
                    <div v-for="(p, idx) in scopeDraft.allow_paths" :key="p + idx" class="path-tag-item">
                      <i class="ph ph-folder"></i>
                      <span class="path-tag-text" :title="p">{{ p }}</span>
                      <button type="button" class="path-tag-remove" @click="removeScopeAllowPath(idx)" title="移除">
                        <i class="ph ph-x"></i>
                      </button>
                    </div>
                  </div>
                </div>

                <!-- 排除模式视图 -->
                <div class="scope-field" v-if="scopeDraft.mode === 'denylist'">
                  <p class="scope-sub-label">排除的目录</p>
                  <div class="path-input-group">
                    <input
                      type="text"
                      class="standard-text-input"
                      v-model="newScopeDenyPath"
                      placeholder="输入要排除的目录，例如 D:\Private"
                      aria-label="输入要排除的目录路径"
                      @keyup.enter="addScopeDenyPath()"
                    />
                    <button type="button" class="btn-clean-subtle" @click="pickFolderForDeny" title="从文件系统中选择目录">
                      <i class="ph ph-folder-open"></i> 选择目录
                    </button>
                    <button type="button" class="btn-clean-primary" @click="addScopeDenyPath()">
                      <i class="ph ph-plus"></i> 添加
                    </button>
                  </div>

                  <!-- 常用排除快捷添加 -->
                  <div class="quick-preset-row">
                    <span class="quick-preset-label">快捷排除：</span>
                    <button type="button" class="quick-preset-btn" @click="addScopeDenyPath('C:\\Windows')">+ C:\Windows</button>
                    <button type="button" class="quick-preset-btn" @click="addScopeDenyPath('C:\\Program Files')">+ Program Files</button>
                    <button type="button" class="quick-preset-btn" @click="addScopeDenyPath('C:\\Users\\*\\AppData')">+ AppData</button>
                  </div>

                  <div class="path-tags-list" v-if="scopeDraft.deny_paths && scopeDraft.deny_paths.length > 0">
                    <div v-for="(p, idx) in scopeDraft.deny_paths" :key="p + idx" class="path-tag-item is-deny">
                      <i class="ph ph-shield-slash"></i>
                      <span class="path-tag-text" :title="p">{{ p }}</span>
                      <button type="button" class="path-tag-remove" @click="removeScopeDenyPath(idx)" title="移除">
                        <i class="ph ph-x"></i>
                      </button>
                    </div>
                  </div>
                </div>

                <!-- 权限开关 -->
                <div class="scope-field">
                  <p class="scope-label">操作权限</p>
                  <div class="scope-toggles-row">
                    <label class="scope-toggle-clean" title="关闭后对端无法从本机取回文件">
                      <input type="checkbox" v-model="scopeDraft.can_pull" />
                      <span>允许取回本机文件</span>
                    </label>
                    <label class="scope-toggle-clean" title="对端发送的文件将安全保存于本机收件目录">
                      <input type="checkbox" v-model="scopeDraft.can_push" />
                      <span>允许往本机写入文件</span>
                    </label>
                  </div>
                </div>
              </div>

              <div class="scope-body" v-else>
                <p class="scope-note">正在读取…</p>
              </div>

              <div class="scope-foot">
                <button class="scope-btn ghost" @click="resetScopeToDefault">
                  恢复默认
                </button>
                <span style="flex: 1"></span>
                <button class="scope-btn ghost" @click="closeScopeEditor">取消</button>
                <button
                  class="scope-btn primary"
                  :disabled="!scopeDraft || scopeSaving"
                  @click="saveScope"
                >
                  {{ scopeSaving ? "保存中…" : "保存" }}
                </button>
              </div>
            </div>
          </div>

          <!--
            「输入对方窗口上的匹配码」（§2.3）

            码由**接收方**生成、只显示在接收方的审批窗口上；发起方在这里
            把它敲回来。方向的理由见
            `core/src/transport/server.rs` 的 `ApprovalManager::challenge_for`。
          -->
          <div v-if="codePrompt" class="modal-mask" @click.self="cancelCodePrompt">
            <div
              v-modal-focus
              class="code-prompt-modal"
              role="dialog"
              aria-modal="true"
              aria-labelledby="code-prompt-title"
              tabindex="-1"
            >
              <div class="scope-head">
                <h3 id="code-prompt-title"><i class="ph ph-key"></i> 输入匹配码</h3>
                <button class="scope-close" @click="cancelCodePrompt" title="关闭">
                  <i class="ph ph-x"></i>
                </button>
              </div>
              <div class="code-prompt-body">
                <p class="code-prompt-line">
                  「{{ codePrompt.deviceName }}」被设置为「每次匹配码」，
                  {{ codePrompt.opLabel }}需要核对。
                </p>
                <p class="code-prompt-hint">
                  请看向<strong>对方飞梭的审批窗口</strong>，把它显示的 6 位码敲在下面。
                </p>
                <p v-if="codePrompt.reason" class="code-prompt-reason">
                  对方提示：{{ codePrompt.reason }}
                </p>
                <label class="code-prompt-field">
                  <span class="code-prompt-field-label">对方审批窗口上显示的 6 位码</span>
                  <input
                    v-model="codePrompt.value"
                    class="code-prompt-input"
                    type="text"
                    inputmode="numeric"
                    autocomplete="off"
                    maxlength="12"
                    placeholder="例如 123456"
                    autofocus
                    @keyup.enter="submitCodePrompt"
                    @keyup.esc="cancelCodePrompt"
                  />
                </label>
                <p class="code-prompt-note">
                  码只显示在对方屏幕上，所以对方屏幕前得有人。
                </p>
              </div>
              <div class="scope-foot">
                <button class="scope-btn ghost" @click="cancelCodePrompt">取消</button>
                <button class="scope-btn primary" @click="submitCodePrompt">
                  确定并重试
                </button>
              </div>
            </div>
          </div>

          <!--
            连接路径（§3.9 / P4 ⑧）。
            一台设备同时有 ZeroTier 与物理网卡时，用户需要能看见"现在走的是哪条"
            并能显式切换 —— 尤其是"覆盖网通但很慢、想试试局域网"这种场景。
            默认按覆盖网优先自动选（D6），这里只把选择权交还给用户。
          -->
          <div
            v-if="store.selectedDevice && deviceEndpoints.length > 0"
            class="endpoint-bar"
          >
            <span class="endpoint-label">
              <i class="ph ph-signpost"></i> 连接路径
            </span>
            <button
              class="endpoint-chip"
              :class="{ 'is-active': !pinnedEndpoints[store.selectedDevice.device_id] }"
              title="按“覆盖网优先”自动选择（ZeroTier 优先，绕开家庭 NAT）"
              @click="pinDeviceEndpoint(store.selectedDevice.device_id, null)"
            >
              自动
            </button>
            <button
              v-for="ep in deviceEndpoints"
              :key="`${ep.ip}:${ep.port}`"
              class="endpoint-chip"
              :class="{
                'is-active':
                  pinnedEndpoints[store.selectedDevice.device_id]?.ip === ep.ip &&
                  pinnedEndpoints[store.selectedDevice.device_id]?.port === ep.port,
              }"
              :disabled="ep.port === 0"
              :title="
                (ep.port === 0
                  ? '该地址还没有已知的传输端口（设备从未在这个地址上回应过），暂不可选'
                  : endpointKindLabel(ep.kind) +
                    (ep.verified ? '' : ' · 尚未验证过')) +
                ' · 上次活动 ' +
                humanizeEndpoint(ep.last_seen)
              "
              @click="pinDeviceEndpoint(store.selectedDevice.device_id, ep)"
            >
              {{ endpointKindLabel(ep.kind) }}
              <span class="endpoint-ip">{{ ep.ip }}</span>
              <i v-if="!ep.verified" class="ph ph-question" title="尚未验证过"></i>
            </button>
          </div>
        </div>

        <!-- ===== Tab 1: 发送 ===== -->
        <div v-if="currentTab === 'send'" class="tab-page view-send">
      <!-- 视觉隐藏但保留在无障碍树里：这个视图没有可见页标题，
           而"双栏穿梭"那个视图有 —— 按标题跳转的用户会以为发送页不存在。 -->
      <h2 class="view-heading-sr">发送文件</h2>
          <div
            class="drag-drop-card"
            :class="{ 'is-dragging': isDragOver, 'is-busy': isConcurrencyFull }"
            @dragover.prevent="isDragOver = true"
            @dragleave.prevent="isDragOver = false"
            @drop.prevent="isDragOver = false"
          >
            <div class="drop-illustration-icon">
              <i class="ph-bold ph-arrow-fat-lines-up"></i>
            </div>
            <h3>{{ isConcurrencyFull ? '并发传输已满，请稍候' : (runningCount > 0 ? '正在传输中 · 可继续添加或拖放' : '拖放文件或文件夹到此处') }}</h3>
            <p class="drop-sub-tip" title="文件松手即发，5秒内可撤销；文件夹递归展开；剪贴板支持预览确认">
              <i class="ph ph-info"></i> 支持拖放单个或多个文件，也可点击下方按钮选取
            </p>

            <div class="drop-cta-buttons">
              <button class="btn-fluent-primary" @click="pickFilesToSend">
                <i class="ph-bold ph-folder-open"></i> 选取本地文件
              </button>
              <button class="btn-fluent-secondary" @click="handleSendClipboard">
                <i class="ph-bold ph-clipboard-text"></i> 发送剪贴板
              </button>
            </div>
          </div>

          <div v-if="stagedFiles.length > 0" class="staged-file-tray">
            <div class="tray-title-bar">
              <!--
                计数与大小都要算上**目录展开后的文件数**。
                拖一个文件夹进来，清单上是 1 项；实际会发 7 个文件 17.8 MB。
                头部写「1 项，共 4.0 KB」时，那 4.0 KB 是 NTFS **目录项自身**的
                大小 —— 数字小得让人以为程序弄丢了文件。
                所以：项数用 stagedFileCount（展开后），大小用 totalStagedSize
                （已是展开后的字节）。
              -->
              <span>待发清单 ({{ stagedFileCount }} 个文件，共 {{ totalStagedSize }})</span>
              <button class="clear-all-link" @click="clearStaged">清空全部</button>
            </div>
            <!--
              落点常驻显示。用户点「发送」之前**必须**知道文件会去哪儿：
              两种来源（拖进窗口 / 穿梭里拖文件夹）落到完全不同的目录，
              发出去之后再去收件目录找不到的人，会怀疑文件丢了而不是怀疑落点。
            -->
            <div class="tray-dest-line" :title="stagedDestTitle">
              <i class="ph ph-map-pin"></i>
              <span>落点：{{ stagedDestLabel }}</span>
            </div>
            <div class="staged-scroll-rows">
              <div v-for="(file, idx) in stagedFiles" :key="file.path + idx" class="staged-row">
                <i :class="getFileIcon(file.name)"></i>
                <span class="row-fname" :title="file.path">
                  {{ file.name }}
                  <!--
                    目录必须显式写出「N 个文件」，不能只显示大小。
                    用户拖进来的是**一个文件夹**，屏幕上只出现一行、
                    大小又只有 4.0 KB，两边都在暗示"里面没别的东西"。
                  -->
                  <em v-if="file.isDir" class="row-dir-count">{{ stagedRowSize(file) }}</em>
                </span>
                <span class="row-fsize" :title="stagedRowTitle(file)">{{ file.sizeFormatted }}</span>
                <button class="row-remove-btn" @click="stagedFiles.splice(idx, 1)" title="移除此项">
                  <i class="ph ph-x"></i>
                </button>
              </div>
            </div>
            <div class="tray-action-bottom">
              <button
                class="btn-fluent-primary execute-send-btn"
                :disabled="isConcurrencyFull || !store.selectedDevice"
                :title="stagedDestTitle"
                @click="startSendTransfer"
              >
                <i class="ph-bold" :class="isConcurrencyFull ? 'ph-arrows-clockwise spinning' : 'ph-paper-plane-tilt'"></i>
                <span>{{ sendButtonText }}</span>
              </button>
            </div>
          </div>
        </div>

        <!-- ===== Tab 2: 双栏穿梭 ===== -->
        <div v-if="currentTab === 'shuttle'" class="tab-page view-shuttle">
      <h2 class="view-heading-sr">双栏穿梭</h2>
          <div class="shuttle-side-pane">
            <div class="shuttle-pane-header">
              <div class="pane-name">
                <i class="ph-bold ph-hard-drive"></i>
                <span class="pane-name-text">{{ localVolumeMode ? "本机磁盘" : "本机落盘目录" }}</span>
                <span class="pane-count">{{ localDiskFiles.length }}</span>
              </div>
              <button class="pane-refresh-btn" @click="loadLocalFiles()" title="刷新">
                <i class="ph ph-arrows-clockwise" :class="{ spinning: isLoadingLocal }"></i>
              </button>
            </div>
            <!-- 地址栏（§7.1）：与右栏同构 —— 卷下拉 + 完整路径 -->
            <div class="pane-address-bar">
              <!--
                一个下拉装三类根：收件目录 / 常用位置 / 真实卷。
                分组（optgroup）而不是三个并列控件 —— 地址栏的横向空间本来
                就紧，而"选根"本来就是**一个**动作。资源管理器的地址栏下拉
                也是这个结构。
              -->
              <div
                v-if="localVolumes.length > 0 || localPlaces.length > 0"
                class="custom-dropdown-wrap"
              >
                <button
                  type="button"
                  class="custom-dropdown-trigger"
                  :class="{ 'is-active': localDropdownOpen }"
                  @click="localDropdownOpen = !localDropdownOpen"
                  title="选择要浏览的位置"
                >
                  <i class="ph ph-hard-drives"></i>
                  <span class="dropdown-trigger-text">{{ localCurrentLabel }}</span>
                  <i class="ph ph-caret-down dropdown-trigger-caret" :class="{ 'is-open': localDropdownOpen }"></i>
                </button>
                <div v-if="localDropdownOpen" class="custom-dropdown-panel">
                  <button
                    type="button"
                    class="dropdown-menu-item"
                    :class="{ 'is-selected': localRootToken === '' }"
                    @click="handlePickLocalToken('')"
                  >
                    <i class="ph ph-tray"></i>
                    <span class="item-main-text">{{ receiveRootLabel }}</span>
                    <span class="item-tag-text">收件目录</span>
                  </button>
                  <div v-if="localPlaces.length > 0" class="dropdown-group-header">常用位置</div>
                  <button
                    v-for="p in localPlaces"
                    :key="p.key"
                    type="button"
                    class="dropdown-menu-item"
                    :class="{ 'is-selected': localRootToken === 'p:' + p.key }"
                    @click="handlePickLocalToken('p:' + p.key)"
                    :title="p.path"
                  >
                    <i class="ph ph-folder"></i>
                    <span class="item-main-text">{{ p.label }}</span>
                  </button>
                  <div v-if="localVolumes.length > 0" class="dropdown-group-header">本地磁盘</div>
                  <button
                    v-for="v in localVolumes"
                    :key="v.id"
                    type="button"
                    class="dropdown-menu-item"
                    :class="{ 'is-selected': localRootToken === 'v:' + v.id }"
                    @click="handlePickLocalToken('v:' + v.id)"
                  >
                    <i class="ph ph-hard-drive"></i>
                    <span class="item-main-text">{{ v.label }}</span>
                    <span v-if="v.free_bytes > 0" class="item-sub-text">剩 {{ formatRemoteVolumeSize(v.free_bytes) }}</span>
                  </button>
                </div>
              </div>
              <span class="address-text" :title="localAddressDisplay">{{ localAddressDisplay }}</span>
            </div>
            <div class="pane-breadcrumb">
              <button
                class="crumb-up"
                :disabled="localParentPath === null"
                @click="goLocalParent"
                title="返回上一级"
              >
                <i class="ph ph-arrow-u-up-left"></i>
              </button>
              <template v-for="(c, ci) in localCrumbs" :key="c.path + ci">
                <i v-if="ci > 0" class="ph ph-caret-right crumb-sep"></i>
                <button
                  class="crumb-item"
                  :class="{ 'is-current': ci === localCrumbs.length - 1 }"
                  @click="goLocalCrumb(c.path)"
                >
                  {{ c.label }}
                </button>
              </template>
            </div>
            <div v-if="localTruncated" class="pane-truncated-hint">
              <i class="ph ph-warning"></i>
              共 {{ localTotal }} 项，已显示 {{ localDiskFiles.length }} 项
            </div>
            <div
              class="shuttle-items-scroll"
              :class="{ 'is-drop-armed': shuttleDropArmed('local') }"
              :aria-busy="isLoadingLocal"
              @dragover="onShuttlePaneDragOver($event, 'local')"
              @drop="onShuttlePaneDrop($event, 'local')"
            >
              <!--
                首屏加载用**骨架行**而不是转圈图标。

                为什么: 之前这里什么都没有 —— `v-for` 在数组为空时不渲染,
                于是列目录的那几百毫秒里整个面板是**一片空白**。用户看到
                空白会以为"这个目录没文件"或"程序卡住了", 于是去点别的
                地方。转圈图标好一点, 但它不告诉用户"接下来会出现什么形状
                的东西"。

                骨架行形状必须与真实行一致(勾选框 + 图标 + 名称 + 大小),
                文件加载完成时列表才不会"跳一下"。行数刻意少于真实列表,
                免得骨架比内容还长, 反倒给人更慢的错觉。

                `aria-hidden` 是必须的: 骨架不是内容, 屏幕阅读器念出来
                只会变成一串无意义的空文本。加载状态另有 `aria-busy`
                在滚动容器上表达。
              -->
              <div
                v-if="isLoadingLocal && localDiskFiles.length === 0"
                class="shuttle-skeleton-stack"
                aria-hidden="true"
              >
                <div v-for="n in 6" :key="'local-skel-' + n" class="shuttle-file-line is-skeleton">
                  <div class="check-box-square skeleton-block"></div>
                  <i class="ph ph-file skeleton-glyph"></i>
                  <span class="file-label-col">
                    <span class="skeleton-block" :style="{ width: 40 + ((n * 17) % 45) + '%' }"></span>
                  </span>
                  <span class="file-size-col"><span class="skeleton-block" style="width: 34px"></span></span>
                </div>
              </div>
              <div
                v-for="item in localDiskFiles"
                :key="localEntryKey(item)"
                class="shuttle-file-line"
                :class="{
                  'is-selected': selectedLocalNames.has(localEntryKey(item)),
                  'is-drop-armed': shuttleDropArmed('remote')
                }"
                draggable="true"
                v-key-activate
                role="button"
                tabindex="0"
                :aria-pressed="item.is_dir ? undefined : selectedLocalNames.has(localEntryKey(item))"
                @dragstart="onShuttleRowDragStart($event, 'local', item)"
                @dragend="onShuttleRowDragEnd"
                @click="item.is_dir ? enterLocalDir(item.name) : toggleLocalSelection(item)"
              >
                <div class="check-box-square">
                  <i v-if="!item.is_dir && selectedLocalNames.has(localEntryKey(item))" class="ph-bold ph-check"></i>
                  <i v-else-if="item.is_dir" class="ph-bold ph-caret-right"></i>
                </div>
                <i :class="item.is_dir ? 'ph-fill ph-folder file-glyph dir' : getFileIcon(item.name)"></i>
                <span class="file-label-col" :title="item.name + ' · ' + item.modified">{{ item.name }}</span>
                <span class="file-size-col">{{ item.size_formatted }}</span>
                <!--
                  文件夹行额外给一个"整夹发送"按钮（§7.6）。
                  点行本身是"进入", 所以必须有一个**独立**入口才能把整个
                  文件夹发出去 —— 让用户逐个进子目录、逐个勾选 3000 个文件
                  是不现实的。按钮只在悬停/选中时出现, 避免列表右侧全是图标。
                -->
                <button
                  v-if="item.is_dir"
                  class="dir-send-btn"
                  :class="{ 'is-on': selectedLocalNames.has(localEntryKey(item)) }"
                  :disabled="isConcurrencyFull"
                  :title="
                    selectedLocalNames.has(localEntryKey(item))
                      ? '取消选择整个文件夹'
                      : '把整个文件夹（含子目录）加入待发'
                  "
                  @click.stop="toggleLocalSelection(item)"
                >
                  <i class="ph ph-folder-plus"></i>
                </button>
              </div>
              <button
                v-if="localTruncated"
                class="load-more-btn"
                :disabled="isLoadingLocal"
                @click="loadMoreLocal"
              >
                <i v-if="isLoadingLocal" class="ph ph-circle-notch spinning"></i>
                <template v-else>加载更多</template>
              </button>
              <div v-if="localMessage" class="shuttle-empty-tip">{{ localMessage }}</div>
            </div>
          </div>

          <div class="shuttle-control-divider">
            <button
              class="shuttle-op-button send-op"
              :disabled="selectedLocalNames.size === 0 || !store.selectedDevice || isConcurrencyFull"
              @click="shuttleSend"
              :title="'把选中的 ' + selectedLocalNames.size + ' 项送到对方 ' + sendDestLabel"
            >
              <i class="ph-bold ph-arrow-right"></i>
              <span>发送到对方</span>
            </button>
            <button
              class="shuttle-op-button fetch-op"
              :disabled="selectedRemoteNames.size === 0 || !store.selectedDevice || isConcurrencyFull"
              @click="shuttleFetch"
              :title="'把选中的 ' + selectedRemoteNames.size + ' 项取回到本机 ' + fetchDestLabel"
            >
              <i class="ph-bold ph-arrow-left"></i>
              <span>取回到本机</span>
            </button>
          </div>

          <div class="shuttle-side-pane">
            <div class="shuttle-pane-header">
              <div class="pane-name">
                <i class="ph-bold ph-device-mobile"></i>
                <span class="pane-name-text" :title="store.selectedDevice?.device_name">{{ store.selectedDevice?.device_name || '对端存储' }}</span>
                <span class="pane-count">{{ remoteDiskFiles.length }}</span>
              </div>
              <button class="pane-refresh-btn" @click="loadRemoteFiles()" title="刷新对端目录">
                <i class="ph ph-arrows-clockwise" :class="{ spinning: isLoadingRemote }"></i>
              </button>
            </div>
            <!-- 地址栏（§7.2）：卷下拉 + 面包屑 -->
            <div class="pane-address-bar">
              <!-- 与左栏同构：常用位置 + 磁盘。对端的是**对端机器**的
                   目录，已按该设备的访问范围过滤过（服务端做的）。 -->
              <div
                v-if="remoteVolumes.length > 0 || remotePlaces.length > 0"
                class="custom-dropdown-wrap"
              >
                <button
                  type="button"
                  class="custom-dropdown-trigger"
                  :class="{ 'is-active': remoteDropdownOpen }"
                  @click="remoteDropdownOpen = !remoteDropdownOpen"
                  title="选择要浏览的位置"
                >
                  <i class="ph ph-hard-drives"></i>
                  <span class="dropdown-trigger-text">{{ remoteCurrentLabel }}</span>
                  <i class="ph ph-caret-down dropdown-trigger-caret" :class="{ 'is-open': remoteDropdownOpen }"></i>
                </button>
                <div v-if="remoteDropdownOpen" class="custom-dropdown-panel">
                  <div v-if="remotePlaces.length > 0" class="dropdown-group-header">常用位置</div>
                  <button
                    v-for="p in remotePlaces"
                    :key="p.key"
                    type="button"
                    class="dropdown-menu-item"
                    :class="{ 'is-selected': remoteRootToken === 'p:' + p.key }"
                    @click="handlePickRemoteToken('p:' + p.key)"
                    :title="p.path"
                  >
                    <i class="ph ph-folder"></i>
                    <span class="item-main-text">{{ p.label }}</span>
                  </button>
                  <div v-if="remoteVolumes.length > 0" class="dropdown-group-header">磁盘</div>
                  <button
                    v-for="v in remoteVolumes"
                    :key="v.id"
                    type="button"
                    class="dropdown-menu-item"
                    :class="{ 'is-selected': remoteRootToken === 'v:' + v.id }"
                    @click="handlePickRemoteToken('v:' + v.id)"
                  >
                    <i class="ph ph-hard-drive"></i>
                    <span class="item-main-text">{{ v.label }}</span>
                    <span v-if="v.free_bytes > 0" class="item-sub-text">剩 {{ formatRemoteVolumeSize(v.free_bytes) }}</span>
                  </button>
                </div>
              </div>
              <span class="address-text" :title="remoteAddressDisplay">{{ remoteAddressDisplay }}</span>
            </div>
            <div class="pane-breadcrumb">
              <button
                class="crumb-up"
                :disabled="remoteParentPath === null"
                @click="goRemoteParent"
                title="返回上一级"
              >
                <i class="ph ph-arrow-u-up-left"></i>
              </button>
              <template v-for="(c, ci) in remoteCrumbs" :key="c.path + ci">
                <i v-if="ci > 0" class="ph ph-caret-right crumb-sep"></i>
                <button
                  class="crumb-item"
                  :class="{ 'is-current': ci === remoteCrumbs.length - 1 }"
                  @click="goRemoteCrumb(c.path)"
                >
                  {{ c.label }}
                </button>
              </template>
            </div>
            <div v-if="remoteTruncated" class="pane-truncated-hint">
              <i class="ph ph-warning"></i>
              共 {{ remoteTotal }} 项，已显示 {{ remoteDiskFiles.length }} 项
            </div>
            <div
              class="shuttle-items-scroll"
              :class="{ 'is-drop-armed': shuttleDropArmed('remote') }"
              :aria-busy="isLoadingRemote"
              @dragover="onShuttlePaneDragOver($event, 'remote')"
              @drop="onShuttlePaneDrop($event, 'remote')"
            >
              <!-- 对端目录要**跨网络**去问, 首屏空白比本机更久, 骨架更该有 -->
              <div
                v-if="isLoadingRemote && remoteDiskFiles.length === 0"
                class="shuttle-skeleton-stack"
                aria-hidden="true"
              >
                <div v-for="n in 6" :key="'remote-skel-' + n" class="shuttle-file-line is-skeleton">
                  <div class="check-box-square skeleton-block"></div>
                  <i class="ph ph-file skeleton-glyph"></i>
                  <span class="file-label-col">
                    <span class="skeleton-block" :style="{ width: 40 + ((n * 23) % 45) + '%' }"></span>
                  </span>
                  <span class="file-size-col"><span class="skeleton-block" style="width: 34px"></span></span>
                </div>
              </div>
              <div
                v-for="item in remoteDiskFiles"
                :key="remoteEntryKey(item)"
                class="shuttle-file-line"
                :class="{
                  'is-selected': selectedRemoteNames.has(remoteEntryKey(item)),
                  'is-drop-armed': shuttleDropArmed('local')
                }"
                draggable="true"
                v-key-activate
                role="button"
                tabindex="0"
                :aria-pressed="item.is_dir ? undefined : selectedRemoteNames.has(remoteEntryKey(item))"
                @dragstart="onShuttleRowDragStart($event, 'remote', item)"
                @dragend="onShuttleRowDragEnd"
                @click="item.is_dir ? enterRemoteDir(item.name) : toggleRemoteSelection(item)"
              >
                <div class="check-box-square">
                  <i v-if="!item.is_dir && selectedRemoteNames.has(remoteEntryKey(item))" class="ph-bold ph-check"></i>
                  <i v-else-if="item.is_dir" class="ph-bold ph-caret-right"></i>
                </div>
                <i :class="item.is_dir ? 'ph-fill ph-folder file-glyph dir' : getFileIcon(item.name)"></i>
                <span class="file-label-col" :title="item.name + ' · ' + item.modified">{{ item.name }}</span>
                <span class="file-size-col">{{ item.size_formatted }}</span>
                <!--
                  目录给一个"取回"按钮（§7.6 同款语义）。

                  为什么需要它：点目录默认是**进入**目录，所以目录**永远无法被选中**
                  —— 于是"取回整个文件夹"这条路径在界面上根本不存在。
                  用户只能一级级进去逐个勾文件：目录深一点就是几十次点击，
                  而漏掉一个文件的代价是他根本不知道自己漏了。

                  和左栏的"整夹发送"对称：两个方向都能整夹操作。
                -->
                <button
                  v-if="item.is_dir"
                  class="dir-send-btn dir-pull-btn"
                  :disabled="isConcurrencyFull"
                  title="取回整个文件夹（递归展开，保留目录层级）"
                  @click.stop="pullRemoteDir(item.name)"
                >
                  <i class="ph ph-folder-minus"></i>
                </button>
              </div>
              <!-- 分页（§7.3）：不自动滚到底加载，用户可能只是停下来看文件名 -->
              <button
                v-if="remoteTruncated"
                class="load-more-btn"
                :disabled="isLoadingRemote"
                @click="loadMoreRemote"
              >
                <i v-if="isLoadingRemote" class="ph ph-circle-notch spinning"></i>
                <template v-else>加载更多</template>
              </button>
              <div v-if="remoteMessage" class="shuttle-empty-tip">{{ remoteMessage }}</div>
            </div>
          </div>
        </div>

        <!-- ===== Tab 3: 传输记录 ===== -->
        <div v-if="currentTab === 'log'" class="tab-page view-log">
          <div class="fluent-card-panel">
            <div class="panel-header-row">
              <div class="header-titles">
                <h2>传输记录 <i class="ph ph-question" title="传输速度基于纯数据传输耗时计算，已排除审批与落盘等待"></i></h2>
              </div>
              <div class="header-actions">
                <!-- 诊断报告导出：复现问题后把这份文件交给开发者即可定位瓶颈（§9.7） -->
                <button class="btn-clear-history" @click="exportDiagnostics">
                  <i class="ph ph-file-arrow-down"></i> 导出诊断报告
                </button>
                <button class="btn-clear-history" @click="clearHistory" v-if="transferLogs.length > 0">
                  <i class="ph ph-trash"></i> 清空记录
                </button>
              </div>
            </div>
            <div class="log-entries-list">
              <div v-for="log in transferLogs" :key="log.id" class="log-entry-row">
                <div class="direction-badge" :class="log.direction === 'recv' ? 'recv' : 'send'">
                  <i :class="log.direction === 'recv' ? 'ph-bold ph-arrow-down-left' : 'ph-bold ph-arrow-up-right'"></i>
                </div>
                <div class="log-body-info">
                  <div class="entry-title-wrap">
                    <span class="entry-title">{{ log.file_name }}</span>
                    <button
                      v-if="log.file_paths && log.file_paths.length > 0"
                      class="btn-view-paths-badge"
                      @click.stop="openPathsModal(log)"
                      :title="log.file_paths.length === 1 ? '查看完整路径并在文件夹中定位' : `查看全部 ${log.file_paths.length} 个文件的完整路径`"
                    >
                      <i class="ph ph-folder-open"></i>
                      <span>{{ log.file_paths.length === 1 ? '查看路径' : `完整路径 (${log.file_paths.length})` }}</span>
                    </button>
                  </div>
                  <div class="entry-sub">
                    {{ log.direction === 'recv' ? '来自 ' + log.peer_name : '发送至 ' + log.peer_name }}
                    · {{ log.time_formatted }}
                    <span
                      v-if="log.over_overlay"
                      class="link-tag"
                      title="经覆盖网（ZeroTier / Tailscale 的 100.64/10）"
                      >经虚拟网卡</span
                    >
                    <span
                      v-if="log.connect_ms > 0"
                      class="link-tag"
                      :title="'两端 TCP 建连耗时 ' + log.connect_ms + 'ms'"
                      >建连 {{ log.connect_ms }}ms</span
                    >
                  </div>
                  <!-- 慢的时候说清"时间花在哪"：审批等待 vs 数据流 -->
                  <div
                    v-if="log.duration_active_ms > 0 && log.duration_wall_ms > log.duration_active_ms + 500"
                    class="entry-sub dimmed"
                    :title="'纯数据流 ' + log.duration_active_ms + 'ms / 墙钟 ' + log.duration_wall_ms + 'ms'"
                  >
                    含等待 {{ ((log.duration_wall_ms - log.duration_active_ms) / 1000).toFixed(1) }}s（不计入速度）
                  </div>
                </div>
                <div class="entry-meta-col">
                  <span class="entry-size-text">{{ log.file_size_formatted }}</span>
                  <!-- 样本不足时后端给"—"，而不是一个荒谬的数字（§9.6.5） -->
                  <span class="entry-speed-text" :class="{ unknown: log.speed_display === '—' }">
                    {{ log.speed_display }}
                  </span>
                  <span class="entry-status-badge" :class="{ failed: log.status === 'failed' || log.status === 'cancelled' }">
                    {{ historyStatusText(log.status) }}
                  </span>
                </div>
              </div>
              <div v-if="transferLogs.length === 0" class="empty-log-box">
                <i class="ph ph-tray"></i>
                <span>暂无传输记录</span>
              </div>
            </div>

            <!--
              最近诊断：**默认折叠**。

              为什么不默认展开：诊断条目比传输记录多 3~4 倍（每次传输一条，
              且每条带归因与阶段耗时），默认展开会把上面的历史记录挤到看不见。
              而它的使用场景是"用户已经报过一次慢/失败"——那时候他会主动找。

              折叠标题上**写明里面是什么**（"含瓶颈归因"），否则用户看到
              一个不认识的折叠条，猜不到里面是自己要的东西。
            -->
            <div class="diag-section">
              <button
                class="diag-section-toggle"
                :aria-expanded="showDiagnostics"
                @click="toggleDiagnostics"
              >
                <i class="ph" :class="showDiagnostics ? 'ph-caret-down' : 'ph-caret-right'"></i>
                <span>
                  最近诊断（{{ diagnostics.length }} 条，含瓶颈归因）<template v-if="diagnosticsFailed">
                    · 读取失败</template
                  >
                </span>
              </button>
              <div v-if="showDiagnostics" class="diag-list">
                <div v-if="diagnostics.length === 0" class="empty-log-box">
                  <i class="ph ph-waveform"></i>
                  <span>{{ diagnosticsFailed ? "诊断记录读取失败（不影响传输记录）" : "暂无诊断记录" }}</span>
                </div>
                <div v-for="(d, di) in diagnostics" :key="di" class="diag-row">
                  <div class="diag-row-head">
                    <span class="direction-badge" :class="d.direction === 'recv' ? 'recv' : 'send'">
                      <i
                        :class="
                          d.direction === 'recv'
                            ? 'ph-bold ph-arrow-down-left'
                            : 'ph-bold ph-arrow-up-right'
                        "
                      ></i>
                    </span>
                    <span class="diag-peer">
                      {{ d.direction === 'recv' ? '来自 ' + d.peer_name : '发送至 ' + d.peer_name }}
                    </span>
                    <span class="entry-status-badge" :class="{ failed: d.outcome !== 'completed' }">
                      {{ diagOutcomeText(d.outcome) }}
                    </span>
                    <span class="diag-speed" :title="'纯数据流 ' + d.data_ms + 'ms'">
                      {{ d.speed_display }}
                    </span>
                    <span v-if="d.over_overlay" class="link-tag" title="经覆盖网（100.64/10）">经虚拟网卡</span>
                  </div>
                  <!--
                    归因是这一节存在的**唯一理由**。
                    「缓冲区不足，瓶颈在飞梭」是用户能直接拿去改配置的话；
                    在此之前它只存在于导出的 txt 里，用户看不到。
                  -->
                  <div class="diag-attribution">{{ d.attribution }}</div>
                  <div v-if="d.error" class="diag-error">{{ d.error }}</div>
                  <div class="diag-phases">
                    <span :title="'TCP 建连 ' + d.connect_ms + 'ms'">建连 {{ d.connect_ms }}ms</span>
                    <span :title="'纯数据流 ' + d.data_ms + 'ms'">数据流 {{ d.data_ms }}ms</span>
                    <span :title="'整文件 BLAKE3 复核 ' + d.verify_ms + 'ms（不计入速度）'"
                      >校验 {{ d.verify_ms }}ms</span
                    >
                    <!--
                      审批等待单独显示：它是唯一"**不是程序慢**"的原因。
                      混进总耗时里，用户会以为飞梭有问题，而实际只是
                      对方看了一眼弹窗。
                    -->
                    <span v-if="d.approval_wait_ms > 0" class="link-tag">
                      含人工等待 {{ (d.approval_wait_ms / 1000).toFixed(1) }}s（不计入速度）
                    </span>
                    <span>{{ d.file_count }} 个文件 · {{ formatBytes(d.bytes_transferred) }}</span>
                  </div>
                </div>
              </div>
            </div>
          </div>
        </div>

        <!-- ===== Tab 4: 系统设置 ===== -->
        <div v-if="currentTab === 'settings'" class="tab-page view-settings">
          <div class="fluent-card-panel">
            <div class="panel-header-row">
              <div class="header-titles">
                <h2>系统设置</h2>
              </div>
            </div>

            <div class="panel-section-divider first">
              <h3>基础</h3>
            </div>

            <div class="setting-item-block">
              <div class="setting-desc-text">
                <strong title="局域网内其他设备看到的名字">本机设备名称</strong>
              </div>
              <div class="setting-action-group">
                <input
                  class="standard-text-input name-input"
                  v-model="deviceNameInput"
                  maxlength="32"
                  placeholder="输入设备名称"
                  @blur="commitDeviceName"
                  @keyup.enter="commitDeviceName" aria-label="本机设备名称" />
                <span v-if="nameSaved" class="saved-hint"><i class="ph-fill ph-check"></i> 已保存</span>
              </div>
            </div>

            <div class="setting-item-block">
              <div class="setting-desc-text">
                <strong title="登录系统时自动在后台启动并驻留托盘">开机自启动</strong>
                <span v-if="!updateStatus.isInstalled && localInfo.autostart" class="setting-sub-hint warn-hint">
                  <i class="ph ph-warning"></i> 便携版自启依赖当前文件路径，请勿移动或删除程序
                </span>
              </div>
              <label class="fluent-switch">
                <input aria-label="开机自启动" type="checkbox" v-model="localInfo.autostart" @change="toggleAutostart" />
                <span class="fluent-slider"></span>
              </label>
            </div>

            <div class="setting-item-block">
              <div class="setting-desc-text">
                <strong title="关闭后，即使来自已信任设备的传输也会先请求确认">自动接收文件</strong>
              </div>
              <label class="fluent-switch">
                <input aria-label="自动接收文件" type="checkbox" v-model="localInfo.auto_receive" @change="saveSettings" />
                <span class="fluent-slider"></span>
              </label>
            </div>

            <div class="setting-item-block">
              <div class="setting-desc-text">
                <strong title="同时进行的文件传输任务上限，超出时需等待当前任务完成">最大并发传输数</strong>
              </div>
              <div class="select-compact">
                <CustomSelect
                  v-model="concurrentTransfersSelect"
                  :options="concurrentOptions"
                  @change="saveSettings"
                  aria-label="最大并发传输数"
                />
              </div>
            </div>

            <div class="setting-item-block">
              <div class="setting-desc-text">
                <strong title="点击右上角关闭按钮或按下快捷键时的行为">关闭窗口时</strong>
              </div>
              <div class="select-compact">
                <CustomSelect
                  v-model="localInfo.close_action"
                  :options="closeActionOptions"
                  @change="saveSettings"
                  aria-label="关闭窗口时"
                />
              </div>
            </div>

            <div class="setting-item-block">
              <div class="setting-desc-text">
                <strong>界面外观</strong>
              </div>
              <div class="select-compact">
                <CustomSelect
                  v-model="themeSelect"
                  :options="themeOptions"
                  @change="onThemeSelect"
                  aria-label="界面外观"
                />
              </div>
            </div>

            <div class="setting-item-block">
              <div class="setting-desc-text">
                <strong>文件保存目录</strong>
                <span class="path-mono">{{ localInfo.receive_dir }}</span>
              </div>
              <div class="setting-action-group">
                <button class="btn-fluent-secondary" @click="openReceiveFolder">
                  <i class="ph ph-folder-open"></i> 打开目录
                </button>
                <button class="btn-fluent-secondary" @click="chooseReceiveDir">
                  <i class="ph ph-folder-plus"></i> 更改…
                </button>
              </div>
            </div>

            <div class="setting-item-block">
              <div class="setting-desc-text">
                <strong>设备指纹</strong>
                <span class="path-mono">{{ localInfo.device_id || '—' }}</span>
              </div>
              <button class="btn-fluent-secondary" @click="copyText(localInfo.device_id)">
                <i class="ph ph-fingerprint"></i> 复制指纹
              </button>
            </div>

            <div class="panel-section-divider">
              <h3>受信设备</h3>
            </div>
            <div class="trust-table-wrap">
              <div v-if="trustedDevices.length === 0" class="empty-trust-hint">
                <i class="ph ph-shield-check"></i>
                <span>暂无已信任设备</span>
              </div>
              <div v-else class="trust-item-row" v-for="dev in trustedDevices" :key="dev.device_id">
                <div class="trust-meta">
                  <strong class="dev-name">{{ dev.device_name }}</strong>
                  <span class="dev-ip font-mono">{{ dev.last_ip }} · {{ dev.device_id }}</span>
                </div>
                <button class="btn-unblock" @click="removeTrusted(dev.device_id)">解除信任</button>
              </div>
            </div>

            <div class="panel-section-divider">
              <h3>日志与排障</h3>
            </div>

            <div class="setting-item-block">
              <div class="setting-desc-text">
                <strong title="常规运行使用 INFO，排障时可切换为 DEBUG">日志级别</strong>
              </div>
              <div class="select-compact">
                <CustomSelect
                  v-model="localInfo.log_level"
                  :options="logLevelOptions"
                  @change="saveSettings"
                  aria-label="日志级别"
                />
              </div>
            </div>

            <div class="setting-item-block">
              <div class="setting-desc-text">
                <strong title="单文件达到上限后自动轮转归档，最多保留 3 个归档">本地日志文件</strong>
              </div>
              <button class="btn-fluent-secondary" @click="openLogFolder">
                <i class="ph ph-folder-open"></i> 打开日志目录
              </button>
            </div>

            <div class="panel-section-divider">
              <h3>传输记录保留策略</h3>
            </div>

            <div class="setting-item-block">
              <div class="setting-desc-text">
                <strong title="超出数量上限后自动清理最老记录">记录存储上限</strong>
              </div>
              <div class="select-compact">
                <CustomSelect
                  v-model="localInfo.max_history_records"
                  :options="maxHistoryOptions"
                  @change="saveSettings"
                  aria-label="记录存储上限"
                />
              </div>
            </div>

            <div class="setting-item-block">
              <div class="setting-desc-text">
                <strong title="超过保留天数的历史记录自动清理">记录保存周期</strong>
              </div>
              <div class="select-compact">
                <CustomSelect
                  v-model="localInfo.record_retention_days"
                  :options="retentionDaysOptions"
                  @change="saveSettings"
                  aria-label="记录保存周期"
                />
              </div>
            </div>

            <div class="setting-item-block">
              <div class="setting-desc-text">
                <strong title="清空数据库中的所有历史传输记录">一键清理历史</strong>
              </div>
              <button class="btn-fluent-secondary danger-action" @click="clearHistory">
                <i class="ph ph-trash"></i> 清空传输记录
              </button>
            </div>

            <div class="panel-section-divider">
              <h3>来源网段</h3>
            </div>
            <!--
              来源网段白名单：只接受来自这些网段的连接（**含配对**）。

              ## 为什么需要它（都是实测来的，不是设想的）

              1. 传输端口绑在 `0.0.0.0`（所有网卡，含公网网卡）；
              2. 实测本机的 Windows 防火墙放行规则作用域是 `Private, Public`
                 —— **包含公用网络**。首次运行在弹窗上点一次「允许」，
                 就等于替公网也开了门；
              3. 实测本机公网出口是 `64.120.95.50`，从公网反连 42100
                 **连不上** —— 但那只在"家里路由器不做端口转发"时成立。

              满足下面任一条，公网就直接可达，而应用层**不会有任何东西拒绝**：
              路由器配了端口转发 / 运营商给了公网 IPv4 且全锥 NAT /
              有全局可路由的 IPv6（IPv6 没有 NAT）。

              防火墙是第一道，它挡不住"用户点了允许"这件事；这道是第二道。

              ## 空 = 不限制

              保持既有行为（不引入"升级后突然收不到文件"），
              但**必须一眼看出当前是未设置** —— 所以下面有醒目提示。
            -->
            <div class="setting-item-block setting-item-block-col">
              <div class="setting-desc-text">
                <strong>允许的来源网段</strong>
                <span>
                  留空 = 接受任何来源。填了之后，配对与传输都必须来自这些网段。
                  格式 <code class="path-mono">192.168.31.0/24</code>，
                  省略 <code class="path-mono">/24</code> 时按 <code class="path-mono">/24</code> 处理。
                </span>
              </div>
              <div class="subnet-editor">
                <div
                  v-for="(cidr, i) in allowedPeerSubnets"
                  :key="i"
                  class="subnet-chip"
                  :class="{ 'is-invalid': !isValidCidr(cidr) }"
                >
                  <i class="ph ph-globe"></i>
                  <input
                    class="subnet-chip-input font-mono"
                    v-model="allowedPeerSubnets[i]"
                    :title="isValidCidr(cidr) ? cidr : '格式无法识别'"
                    spellcheck="false"
                  />
                  <button
                    class="subnet-chip-del"
                    title="移除这条"
                    @click="allowedPeerSubnets.splice(i, 1)"
                  >
                    <i class="ph ph-x"></i>
                  </button>
                </div>
                <button class="btn-fluent-secondary subnet-add" @click="allowedPeerSubnets.push('')">
                  <i class="ph ph-plus"></i> 添加网段
                </button>
              </div>
              <p v-if="allowedPeerSubnets.length === 0" class="subnet-warn-hint">
                <i class="ph ph-shield-warning"></i>
                <span>
                  <strong>当前未设置</strong> —— 同一网络内的任何设备都可以发起配对。
                  配上 <code class="path-mono">192.168.31.0/24</code> 之类，
                  公网即使能连到本机端口也进不来。
                </span>
              </p>
              <p v-for="(cidr, i) in badSubnets" :key="'bad' + i" class="subnet-warn-hint is-error">
                <i class="ph-fill ph-warning-circle"></i>
                <span><code class="path-mono">{{ cidr }}</code> 格式无法识别，会被拒绝（fail-closed）。</span>
              </p>
              <div class="setting-action-group subnet-actions">
                <button class="btn-fluent-primary" @click="saveAllowedSubnets" :disabled="!subnetsDirty">
                  <i class="ph ph-floppy-disk"></i> 保存
                </button>
                <span v-if="subnetsDirty" class="subnet-dirty-hint">有未保存的改动</span>
              </div>
            </div>

            <div class="panel-section-divider">
              <h3>关于与更新</h3>
            </div>

            <div class="setting-item-block">
              <div class="setting-desc-text">
                <strong>版本</strong>
                <span class="path-mono">
                  v{{ localInfo.app_version || '—' }}
                  <span class="channel-badge" :class="updateStatus.isInstalled ? 'badge-installed' : 'badge-portable'">
                    {{ updateStatus.isInstalled ? '安装版' : '绿色便携版' }}
                  </span>
                </span>
                <span v-if="!updateStatus.isInstalled" class="setting-sub-hint">
                  提示：当前为便携版。若需要桌面快捷方式和更稳固的自启，推荐下载官方安装包。
                </span>
              </div>
              <div class="setting-action-group">
                <button class="btn-fluent-secondary" @click="openUpdatePage" :disabled="!updateStatus.releasePage">
                  <i class="ph ph-download-simple"></i> 前往下载页
                </button>
              </div>
            </div>

            <div class="setting-item-block">
              <div class="setting-desc-text">
                <strong title="启动及周期性自动检查最新版本并提醒更新">自动检查更新</strong>
              </div>
              <label class="fluent-switch">
                <input aria-label="自动检查更新" type="checkbox" v-model="localInfo.auto_check_update" @change="toggleAutoUpdate" />
                <span class="fluent-slider"></span>
              </label>
            </div>

            <div class="setting-item-block update-block">
              <div class="setting-desc-text">
                <strong>检查更新</strong>
                <span class="path-mono" :class="{ 'update-failed': updateStatus.phase === 'failed' }">
                  {{ updateStatusText }}
                </span>
              </div>
              <div class="setting-action-group">
                <div
                  v-if="updateStatus.phase === 'downloading' && updateStatus.bytesTotal > 0"
                  class="update-progress-track"
                >
                  <div
                    class="update-progress-fill"
                    :style="{ transform: `scaleX(${updatePercent / 100})` }"
                  ></div>
                </div>
                <button
                  class="btn-fluent-secondary"
                  @click="checkForUpdate"
                  :disabled="updateStatus.busy"
                >
                  <i class="ph" :class="updateStatus.busy ? 'ph-spinner ph-spin' : 'ph-arrows-clockwise'"></i>
                  {{ updateStatus.busy ? '进行中…' : '检查更新' }}
                </button>
                <button
                  v-if="updateStatus.phase === 'ready'"
                  class="btn-fluent-primary"
                  @click="applyUpdate"
                >
                  <i class="ph ph-download"></i> 立即更新
                </button>
              </div>
            </div>

            <div class="settings-footer-note">
              <i class="ph ph-info"></i>
              飞梭 v{{ localInfo.app_version || '0.1.0' }} · 设备指纹 {{ localInfo.device_id || '—' }} · 传输端口
              {{ localInfo.transfer_port }} · 发现端口 {{ localInfo.discovery_port }}
            </div>
          </div>
        </div>
      </main>
    </div>

    <!-- 3. 引擎异常横幅 -->
    <div v-if="engineError" class="engine-error-banner">
      <i class="ph-fill ph-warning-octagon"></i>
      <div class="engine-error-text">
        <strong>局域网守护引擎启动失败</strong>
        <span>{{ engineError }}</span>
      </div>
      <button class="banner-close" @click="engineError = ''" title="知道了"><i class="ph ph-x"></i></button>
    </div>

    <!-- 4. 底部实时传输进度条 -->
    <footer v-if="activeProgress" class="desktop-speed-dock" :class="{ failed: activeProgress.settled === 'failed' || activeProgress.settled === 'cancelled' }">
      <div class="speed-dock-meta">
        <button type="button" class="speed-file-title" @click="showActiveTransfersModal = true" style="cursor: pointer" title="点击查看所有进行中的任务列表">
          <i class="ph-bold" :class="activeProgress.settled ? (activeProgress.settled === 'ok' ? 'ph-check' : 'ph-warning') : 'ph-arrows-clockwise spinning'"></i>
          {{ progressDockLabel }}：
          <strong :title="activeProgress.current_file">{{ activeProgress.current_file }}</strong>
        </button>
        <div class="speed-dock-actions">
          <span class="speed-bandwidth-rate">{{ activeProgress.speedFormatted }}</span>
          <button
            class="speed-dock-list-btn"
            title="查看当前所有进行中的传输任务清单"
            @click.stop="showActiveTransfersModal = true"
          >
            <i class="ph ph-list-dashes"></i>
            任务列表 ({{ runningCount }})
          </button>
          <button
            v-if="!activeProgress.settled"
            class="speed-dock-cancel-btn"
            title="取消当前传输"
            @click.stop="cancelActiveTransfer()"
          >
            取消
          </button>
        </div>
      </div>
      <div class="speed-dock-track">
        <div class="speed-dock-fill" :style="{ transform: `scaleX(${activeProgress.progress_percent / 100})` }"></div>
      </div>
    </footer>

    <!-- 4. 关闭程序确认框 -->
    <div v-if="showCloseDialog" class="fluent-modal-overlay" @click.self="showCloseDialog = false">
      <div
        v-modal-focus
        class="fluent-modal-dialog close-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="close-dialog-title"
        tabindex="-1"
      >
        <div class="modal-dialog-titlebar">
          <h3 id="close-dialog-title">要关闭飞梭吗？</h3>
          <button class="modal-close-icon" @click="showCloseDialog = false" title="关闭"><i class="ph ph-x"></i></button>
        </div>
        <p class="close-dialog-hint">
          最小化到托盘后飞梭会继续在后台守护局域网连接，随时可从托盘图标重新唤起。
        </p>
        <div class="close-dialog-actions">
          <button class="btn-fluent-secondary" @click="showCloseDialog = false">取消</button>
          <button class="btn-fluent-secondary" @click="closeToTray">
            <i class="ph ph-minus-circle"></i> 最小化到托盘
          </button>
          <button class="btn-danger-solid" @click="quitApp">
            <i class="ph ph-power"></i> 退出程序
          </button>
        </div>
      </div>
    </div>

    <!-- 5. 直连设备 IP -->
    <div v-if="showDirectConnectDialog" class="fluent-modal-overlay" @click.self="showDirectConnectDialog = false">
      <div
        v-modal-focus
        class="fluent-modal-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="direct-connect-title"
        tabindex="-1"
      >
        <div class="modal-dialog-titlebar">
          <h3 id="direct-connect-title">直连设备 IP</h3>
          <button class="modal-close-icon" @click="showDirectConnectDialog = false" title="关闭"><i class="ph ph-x"></i></button>
        </div>
        <div class="pair-view-content">
          <p class="pair-guide-hint">输入对方设备的局域网 IP，飞梭会主动发起一次发现探测：</p>
          <div class="form-input-group">
            <label for="direct-connect-ip">对方 IP 地址</label>
            <input
              type="text"
              class="standard-text-input"
              id="direct-connect-ip"
              v-model="directConnectIp"
              :class="{ 'is-invalid': !!directConnectIpHint }"
              :aria-invalid="!!directConnectIpHint"
              :aria-describedby="directConnectIpHint ? 'direct-connect-ip-hint' : undefined"
              placeholder="例如 192.168.1.120"
              @keyup.enter="submitDirectConnect"
              autofocus
            />
            <!--
              行内格式提示。放在输入框正下方而不是等提交后才弹 toast ——
              用户的手指还在键盘上, 这时告诉他"这里填错了"比让他等一次
              网络往返再看到"连接失败"有用得多。
            -->
            <p v-if="directConnectIpHint" id="direct-connect-ip-hint" class="field-hint is-error">
              <i class="ph ph-warning-circle"></i>
              {{ directConnectIpHint }}
            </p>
          </div>
          <div v-if="directConnectMsg" class="error-notice-card" :class="{ success: isDirectConnectSuccess }">
            <i :class="isDirectConnectSuccess ? 'ph-fill ph-check-circle' : 'ph-fill ph-warning-circle'"></i>
            {{ directConnectMsg }}
          </div>
          <div class="modal-actions-tray">
            <button class="btn-fluent-secondary" @click="showDirectConnectDialog = false">取消</button>
            <!-- 格式不对时也禁用：明知会失败还让人点一次网络往返，
                 只是把"你没填对"换成"连接失败"，后者没法照着改。 -->
            <button
              class="btn-fluent-primary"
              :disabled="isConnectingIp || !directConnectIp.trim() || !!directConnectIpHint"
              @click="submitDirectConnect"
            >
              <i class="ph-bold ph-arrows-clockwise spinning" v-if="isConnectingIp"></i>
              <i class="ph-bold ph-lightning" v-else></i>
              <span>{{ isConnectingIp ? '正在探测...' : '开始连接' }}</span>
            </button>
          </div>
        </div>
      </div>
    </div>

    <!-- 6. 设备配对 -->
    <div v-if="showPairingDialog" class="fluent-modal-overlay" @click.self="showPairingDialog = false">
      <div
        v-modal-focus
        class="fluent-modal-dialog"
        :class="{ 'dialog-success-mode': pairMode === 'success' }"
        role="dialog"
        aria-modal="true"
        aria-labelledby="pair-dialog-title"
        tabindex="-1"
      >
        <div class="modal-dialog-titlebar">
          <h3 id="pair-dialog-title">{{ pairMode === 'success' ? '配对完成' : '设备配对' }}</h3>
          <button class="modal-close-icon" @click="showPairingDialog = false" title="关闭"><i class="ph ph-x"></i></button>
        </div>

        <!--
          流程说明。**这是首用最大的摩擦点**，而两个并列的标签页无法表达
          "谁给谁"这种方向性：新人看到「输入配对码」和「查看本机配对码」
          并排，只会问"我该点哪个"。而正确顺序是有方向的：

            对方点「查看本机配对码」→ 把屏幕上的 6 位码念给你
            你点「输入配对码」→ 敲进去 → 配对完成

          两边**不用同时操作**，但被念的一方要在 30 秒内敲进去（码一次性）。
          这些话写在界面上，比让用户点错一次再回头看文档便宜得多。
        -->


        <div class="pair-mode-selector" v-if="pairMode !== 'success'">
          <button :class="{ active: pairMode === 'input' }" @click="pairMode = 'input'">
            <i class="ph-bold ph-keyboard"></i> 输入配对码
          </button>
          <button :class="{ active: pairMode === 'show' }" @click="pairMode = 'show'">
            <i class="ph-bold ph-qr-code"></i> 查看本机配对码
          </button>
        </div>

        <!-- 输入对端配对码 -->
        <div v-if="pairMode === 'input'" class="pair-view-content">
          <div class="pin-input-container">
            <input
              ref="pinInputRef"
              type="text"
              class="code-pin-input"
              aria-label="输入配对码"
              v-model="inputPin"
              placeholder="请输入 6 位配对码"
              maxlength="7"
              autofocus
              @input="formatPinInput"
              @keyup.enter="submitPairing"
            />
          </div>

          <div class="form-input-group">
            <div class="ip-select-header">
              <label id="pair-ip-label">目标设备 IP</label>
              <button
                v-if="discoveredPeerDevices.length > 1"
                type="button"
                class="btn-text-link"
                @click="isManualIpMode = !isManualIpMode"
              >
                {{ isManualIpMode ? '选择附近设备' : '手动输入其它 IP' }}
              </button>
            </div>

            <div v-if="discoveredPeerDevices.length > 1 && !isManualIpMode" class="pair-select-wrapper">
              <CustomSelect
                v-model="inputTargetIp"
                :options="discoveredPeerDeviceOptions"
                aria-label="选择附近设备"
                placeholder="选择附近设备"
              />
            </div>

            <div v-else-if="discoveredPeerDevices.length === 1 && !isManualIpMode" class="auto-ip-box">
              <input
                type="text"
                class="standard-text-input auto-filled-input" aria-labelledby="pair-ip-label" 
                v-model="inputTargetIp"
                placeholder="例如 192.168.1.120"
              />
              <span class="auto-recognized-badge" :title="discoveredPeerDevices[0].device_name">
                <i class="ph-fill ph-check"></i> {{ discoveredPeerDevices[0].device_name }}
              </span>
            </div>

            <div v-else>
              <input
                type="text"
                class="standard-text-input"
                aria-labelledby="pair-ip-label"
                v-model="inputTargetIp"
                :class="{ 'is-invalid': !!pairTargetIpHint }"
                :aria-invalid="!!pairTargetIpHint"
                :aria-describedby="pairTargetIpHint ? 'pair-ip-hint' : undefined"
                placeholder="例如 192.168.1.120"
              />
              <p v-if="pairTargetIpHint" id="pair-ip-hint" class="field-hint is-error">
                <i class="ph ph-warning-circle"></i>
                {{ pairTargetIpHint }}
              </p>
            </div>
          </div>

          <div v-if="pairErrorMessage" class="error-notice-card">
            <i class="ph-fill ph-warning-circle"></i> {{ pairErrorMessage }}
          </div>

          <div class="modal-actions-tray">
            <button class="btn-fluent-secondary" @click="isPairingSubmit ? cancelPairing() : (showPairingDialog = false)">
              {{ isPairingSubmit ? '取消连接' : '取消' }}
            </button>
            <button
              class="btn-fluent-primary"
              :disabled="isPairingSubmit || inputPin.replace(/\s+/g, '').length < 6 || !inputTargetIp.trim() || !!pairTargetIpHint"
              @click="submitPairing"
            >
              <i class="ph-bold ph-arrows-clockwise spinning" v-if="isPairingSubmit"></i>
              <i class="ph-bold ph-link" v-else></i>
              <span>{{ isPairingSubmit ? '正在验证连接...' : '立即配对' }}</span>
            </button>
          </div>
        </div>

        <!-- 展示本机配对码 -->
        <div v-if="pairMode === 'show'" class="pair-view-content">
          <div class="local-pin-display-box">
            <span class="box-tiny-label">本机配对码（30 秒内有效，仅可使用一次）</span>
            <div class="box-giant-pin">{{ localPairPin }}</div>
            <div class="box-pin-actions">
              <button class="btn-mini-action" @click="copyText(localPairPin)">
                <i class="ph ph-copy"></i> 复制
              </button>
              <button class="btn-mini-action" @click="refreshLocalPin">
                <i class="ph ph-arrows-clockwise"></i> 刷新
              </button>
            </div>
            <div class="pin-countdown-tray">
              <div class="pin-countdown-text">
                <i class="ph-bold ph-hourglass-simple"></i>
                <span>{{ pinSecondsLeft }}s 后自动刷新</span>
              </div>
              <div class="pin-countdown-track">
                <div class="pin-countdown-fill" :style="{ transform: `scaleX(${pinSecondsLeft / 30})` }"></div>
              </div>
            </div>
          </div>

          <!--
            这里原来画了一个「配对二维码」—— 一段**手写死的 SVG 矩形**，
            9 个固定 <rect> + 6 个固定小方块，**不编码任何配对信息**：
            没有那 6 位码、没有设备 ID、没有地址。长得像 QR，用任何扫码 App
            扫出来都是「无内容」。

            而 Android 端**完全没有扫码**：没有 CAMERA 权限、没有 zxing/mlkit
            任何依赖、没有任何扫描代码（`mobile/` 全目录检索零命中）。

            所以它不是"功能没做完"，而是**一个会误导人的假入口**：用户会
            理所当然地拿手机去扫，然后扫不出任何东西，界面上也不解释原因。

            真实且唯一可用的配对方式是**看那 6 位数字、手输**（上面的复制按钮
            可以粘贴）。保留它比删掉它更糟。

            将来真要做扫码，两件事必须**同时**完成，缺一就是上面这个局面：
            1. 这里改成真 QR，编码 `feisuo://pair?host=…&port=…&pin=…`；
            2. Android 加 CAMERA 权限 + zxing（或 ML Kit）+ 扫码界面。

            ⚠️ 但要注意方向：扫码会**降低**安全性。PIN 30 秒有效、同 IP 限速
            5 次，"念 6 位数字"至少要求人在场；二维码一旦被旁人拍到，配对权
            就直接交出去了。所以扫码不是纯粹的体验改进。
          -->

          <div class="modal-actions-tray">
            <button class="btn-fluent-primary" @click="showPairingDialog = false">关闭</button>
          </div>
        </div>

        <!-- 配对成功 -->
        <div v-if="pairMode === 'success'" class="pair-view-content pair-success-view">
          <div class="success-glyph-badge">
            <i class="ph-fill ph-check-circle"></i>
          </div>
          <h3 class="success-headline">配对成功</h3>

          <div class="success-device-card">
            <div class="success-card-row">
              <span class="label">设备名称</span>
              <strong class="value">{{ pairedSuccessDevice?.device_name || '新配对设备' }}</strong>
            </div>
            <div class="success-card-row">
              <span class="label">局域网 IP</span>
              <span class="value font-mono">{{ pairedSuccessDevice?.last_ip }}</span>
            </div>
            <div class="success-card-row">
              <span class="label">状态</span>
              <span class="status-tag-trusted"><i class="ph-bold ph-shield-check"></i> 已信任</span>
            </div>
          </div>

          <div class="modal-actions-tray success-actions">
            <button class="btn-fluent-secondary" @click="finishPairingSuccess">完成</button>
            <button class="btn-fluent-primary" @click="jumpToSendWithPairedDevice">
              <i class="ph-bold ph-paper-plane-tilt"></i> 立即发送文件
            </button>
          </div>
        </div>
      </div>
    </div>

    <!-- 7. 临时接入审批 -->
    <div v-if="pendingApproval" class="fluent-modal-overlay">
      <div
        v-modal-focus
        class="fluent-modal-dialog approval-dialog"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="approval-dialog-title"
        aria-describedby="approval-dialog-desc"
        tabindex="-1"
      >
        <div class="modal-dialog-titlebar">
          <h3 id="approval-dialog-title">设备请求传输文件</h3>
        </div>
        <div class="pair-view-content">
          <div class="approval-card">
            <div class="approval-row">
              <span class="label">来源设备</span>
              <strong class="value">{{ pendingApproval.sender_name }}</strong>
            </div>
            <div class="approval-row">
              <span class="label">局域网 IP</span>
              <span class="value font-mono">{{ pendingApproval.sender_ip }}</span>
            </div>
            <div class="approval-row">
              <span class="label">传输内容</span>
              <span class="value">{{ pendingApproval.file_count }} 个文件（共 {{ pendingApproval.total_size_formatted }}）</span>
            </div>
            <div class="approval-row" v-if="pendingApproval.first_file_name">
              <span class="label">首个文件</span>
              <span class="value font-mono truncate" :title="pendingApproval.first_file_name">
                {{ pendingApproval.first_file_name }}
              </span>
            </div>
          </div>

          <!--
            「每次匹配码」：**本机出示**的码（§2.3）。

            这是用户的原始需求（"每设备可选永久信任或每次匹配码"）。
            早先的实现在 RequireGrant 分支直接拒绝，而界面上**照样有**
            那个等级开关 —— 给了开关却不生效比不给更糟。

            ## 为什么是"显示"而不是"输入"

            早先这里是**输入框**，让本机用户去敲对方屏幕上的码 ——
            方向是反的。对方是**发起方**，而发起方才是需要自证"我人在"的
            那一侧。让不信任对方的人去替对方抄答案，等于让接收方核对一个
            **对方自选的数字**：发起方知道那个数字，接收方手上没有任何
            独立信息可用于判断。

            现在的分工：**接收方出题并显示，发起方应答**。
            码只出现在本机屏幕上，所以一台被入侵的旧设备读不到它，
            也就无法在不惊动本机用户的情况下把文件推过来。
          -->
          <div v-if="pendingApproval.requires_grant_code" class="grant-code-panel">
            <div class="grant-code-title">
              <i class="ph-bold ph-key"></i>
              该设备被设置为「每次匹配码」
            </div>
            <p class="grant-code-hint">
              对方要求核对。把下面这
              <strong>6 位码</strong>念给对方，让对方在飞梭里输入后重试。
            </p>
            <div class="grant-code-display" :title="'本次匹配码：' + formatGrantChallenge(pendingApproval.grant_challenge)">
              {{ formatGrantChallenge(pendingApproval.grant_challenge) }}
            </div>
            <p v-if="approvalCodeRejected" class="grant-code-error">
              <i class="ph ph-warning"></i>
              对方输入的码不匹配。码没变，请再念一遍让对方重输。
            </p>
            <p v-else class="grant-code-hint grant-code-attempts">
              对方最多可输错 3 次。码在本次请求内不变，念一遍就行。
            </p>
          </div>

          <!--
            三个动作（§2.4 + §2.3.1），刻意把"这次允许"、"短期免重复确认"
            与"建立长期信任"**分成三档**。
            旧实现只有一个"通过"，而它会把设备写进长期信任库 ——
            等于"我点了这一次允许"被静默升级成"这台设备从此永久免密"。

            第三个按钮「N 分钟内免重复确认」只在**本次需要匹配码**时出现：
            它的前提就是"已经要比码了"，不要比码时它没有意义，
            而一个无意义的按钮只会稀释另外两个的权重。
          -->
          <div class="modal-actions-tray approval-actions">
            <button class="btn-approval-allow" @click="handleApproval('allow_once')">
              <i class="ph-bold ph-check"></i> 允许一次
            </button>
            <button
              v-if="pendingApproval?.grant_challenge"
              class="btn-approval-grant"
              @click="handleApproval('allow_with_grant')"
            >
              <i class="ph-bold ph-timer"></i>
              {{ grantWindowMinutes }} 分钟内免重复确认
            </button>
            <button class="btn-approval-trust" @click="handleApproval('allow_and_trust')">
              <i class="ph-bold ph-shield-check"></i> 允许并永久信任
            </button>
            <button class="btn-approval-reject" @click="handleApproval('reject')">
              <i class="ph-bold ph-x"></i> 拒绝
            </button>
          </div>
          <p class="approval-hint">
            <i class="ph ph-info"></i>
            「允许一次」只放行本次传输，<strong>不会</strong>把该设备加入长期信任。
            <template v-if="pendingApproval?.grant_challenge">
              <br />
              「{{ grantWindowMinutes }} 分钟免确认」只对 <strong>本次这类操作</strong> 生效，到期自动失效，可随时取消。
            </template>
          </p>
        </div>
      </div>
    </div>

    <!-- 7.5 剪贴板预览（D5：默认开启；预览阶段不落盘） -->
    <div
      v-if="clipboardPreview && (clipboardPreview.kind === 'image' || clipboardPreview.kind === 'text')"
      class="fluent-modal-overlay"
      @click.self="clipboardPreview = null"
    >
      <div
        v-modal-focus
        class="fluent-modal-dialog clipboard-preview-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="clipboard-preview-title"
        tabindex="-1"
      >
        <div class="modal-dialog-titlebar">
          <h3 id="clipboard-preview-title">剪贴板内容预览</h3>
        </div>
        <div class="pair-view-content">
          <div class="approval-card">
            <div class="approval-row">
              <span class="label">类型</span>
              <strong class="value">
                {{ clipboardPreview.kind === "image" ? "图片（将转为 PNG）" : "文本" }}
              </strong>
            </div>
            <div class="approval-row">
              <span class="label">将作为</span>
              <span class="value font-mono truncate" :title="clipboardPreview.file_name">
                {{ clipboardPreview.file_name }}
              </span>
            </div>
            <div class="approval-row">
              <span class="label">大小</span>
              <span class="value">{{ formatBytes(clipboardPreview.size) }}</span>
            </div>
          </div>

          <div class="clipboard-preview-body">
            <img
              v-if="clipboardPreview.kind === 'image'"
              class="clipboard-preview-image"
              :src="'data:image/png;base64,' + clipboardPreview.preview_base64"
              alt="剪贴板图片预览"
            />
            <pre
              v-else
              class="clipboard-preview-text"
            >{{ clipboardPreview.preview }}</pre>
          </div>

          <div class="modal-actions-tray">
            <button class="btn-fluent-secondary" @click="clipboardPreview = null">取消</button>
            <button class="btn-fluent-primary" @click="confirmClipboardPreview">
              <i class="ph-bold ph-paper-plane-tilt"></i> 加入待发清单
            </button>
          </div>
        </div>
      </div>
    </div>

    <!-- 7.6 直发撤销（D4：5 秒撤销窗口） -->
    <transition name="toast-fade">
      <div
        v-if="undoableSend"
        class="fluent-toast undo-toast"
        :class="{ committed: undoableSend.committed }"
      >
        <i class="ph-fill ph-paper-plane-tilt"></i>
        <span class="undo-toast-text">
          {{ undoableSend.committed ? "已开始传输" : "已发起发送" }} ·
          {{ undoableSend.deviceName }} · {{ undoableSend.fileCount }} 个文件
          <template v-if="!undoableSend.committed">，5 秒内可撤销</template>
        </span>
        <button
          v-if="!undoableSend.committed"
          class="undo-toast-btn"
          @click="undoDirectSend"
        >
          撤销
        </button>
      </div>
    </transition>

    <!-- 7.7 传输记录：文件完整路径清单弹窗 -->
    <div v-if="showPathsModal && selectedLogForPaths" class="fluent-modal-overlay" @click.self="showPathsModal = false">
      <div
        v-modal-focus
        class="fluent-modal-dialog paths-list-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="paths-dialog-title"
        tabindex="-1"
      >
        <div class="modal-dialog-titlebar">
          <div class="paths-dialog-title-group">
            <h3 id="paths-dialog-title">
              <i class="ph ph-folder-open"></i>
              传输文件清单 ({{ selectedLogForPaths.file_paths?.length || 0 }} 项)
            </h3>
            <span class="paths-dialog-sub">
              {{ selectedLogForPaths.file_name }} ·
              {{ selectedLogForPaths.direction === 'recv' ? '来自 ' + selectedLogForPaths.peer_name : '发送至 ' + selectedLogForPaths.peer_name }} ·
              {{ selectedLogForPaths.time_formatted }}
            </span>
          </div>
          <button class="modal-close-icon" @click="showPathsModal = false" title="关闭"><i class="ph ph-x"></i></button>
        </div>

        <div class="paths-dialog-toolbar">
          <div class="paths-search-box">
            <i class="ph ph-magnifying-glass"></i>
            <input
              v-model="pathsSearchKeyword"
              type="text"
              placeholder="搜索路径或文件名…"
              aria-label="搜索路径或文件名"
            />
            <button v-if="pathsSearchKeyword" class="btn-clear-search" @click="pathsSearchKeyword = ''" title="清空搜索">
              <i class="ph ph-x-circle"></i>
            </button>
          </div>
          <button class="btn-fluent-secondary copy-all-btn" @click="copyAllSelectedPaths" title="一键复制全部文件的绝对路径">
            <i class="ph" :class="copyAllSuccess ? 'ph-check-bold text-success' : 'ph-copy'"></i>
            <span>{{ copyAllSuccess ? '已复制全部' : '复制全部路径' }}</span>
          </button>
        </div>

        <div class="paths-list-container">
          <div v-if="filteredSelectedPaths.length === 0" class="empty-paths-box">
            <i class="ph ph-tray"></i>
            <span>没有匹配的文件路径</span>
          </div>
          <div
            v-for="(pathItem, pIdx) in filteredSelectedPaths"
            :key="pIdx"
            class="path-item-row"
          >
            <div class="path-item-icon">
              <i class="ph ph-file-text"></i>
            </div>
            <div class="path-item-content" :title="pathItem">
              <div class="path-item-basename">{{ getBasename(pathItem) }}</div>
              <div class="path-item-fullpath">{{ pathItem }}</div>
            </div>
            <div class="path-item-actions">
              <button
                class="btn-path-action"
                @click="copySinglePath(pathItem, pIdx)"
                :title="'复制完整路径: ' + pathItem"
              >
                <i class="ph" :class="copiedPathIndex === pIdx ? 'ph-check-bold text-success' : 'ph-copy'"></i>
                <span>{{ copiedPathIndex === pIdx ? '已复制' : '复制' }}</span>
              </button>
              <button
                class="btn-path-action"
                @click="openFileInFolder(pathItem)"
                title="在资源管理器中定位并打开文件"
              >
                <i class="ph ph-arrow-square-out"></i>
                <span>定位</span>
              </button>
            </div>
          </div>
        </div>

        <div class="paths-dialog-footer">
          <span class="paths-footer-hint">
            共 {{ selectedLogForPaths.file_paths?.length || 0 }} 个文件 · 点击「定位」直接在文件夹中选中文件
          </span>
          <button class="btn-fluent-secondary" @click="showPathsModal = false">关闭</button>
        </div>
      </div>
    </div>

    <!-- 7.8 正在进行的传输任务列表弹窗 -->
    <div v-if="showActiveTransfersModal" class="fluent-modal-overlay" @click.self="showActiveTransfersModal = false">
      <div
        v-modal-focus
        class="fluent-modal-dialog active-transfers-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="active-transfers-title"
        tabindex="-1"
      >
        <div class="modal-dialog-titlebar">
          <div class="active-dialog-title-group">
            <h3 id="active-transfers-title">
              <i class="ph-bold ph-arrows-clockwise spinning"></i>
              正在进行的传输任务 ({{ runningCount }})
            </h3>
            <span class="active-dialog-sub">
              最大并发限制: {{ maxConcurrency }} · 当前活跃任务数: {{ Array.from(activeTransfers.values()).length }}
            </span>
          </div>
          <button class="modal-close-icon" @click="showActiveTransfersModal = false" title="关闭"><i class="ph ph-x"></i></button>
        </div>

        <div class="active-transfers-list-container">
          <div v-if="Array.from(activeTransfers.values()).length === 0" class="empty-active-box">
            <i class="ph ph-check-circle"></i>
            <span>当前没有正在进行的传输任务</span>
          </div>
          <div
            v-for="item in Array.from(activeTransfers.values())"
            :key="item.transfer_id"
            class="active-transfer-card"
            :class="{ 'card-settled': !!item.settled }"
          >
            <div class="card-header-row">
              <div class="card-direction-tag" :class="item.direction === 'Receive' ? 'recv' : 'send'">
                <i :class="item.direction === 'Receive' ? 'ph-bold ph-arrow-down-left' : 'ph-bold ph-arrow-up-right'"></i>
                <span>{{ item.direction === 'Receive' ? '接收自' : '发送至' }} {{ item.peer_device_name || '对端设备' }}</span>
              </div>
              <span class="card-speed-badge">{{ item.speedFormatted }}</span>
            </div>
            <div class="card-file-row">
              <i class="ph ph-file-text"></i>
              <span class="card-filename" :title="item.current_file">{{ item.current_file }}</span>
            </div>
            <div class="card-progress-track">
              <div class="card-progress-fill" :style="{ width: item.progress_percent + '%' }"></div>
            </div>
            <div class="card-footer-row">
              <span class="card-bytes-text">
                {{ formatBytes(item.bytes_transferred) }} / {{ formatBytes(item.total_bytes) }} ({{ item.progress_percent }}%)
              </span>
              <div class="card-action-wrap">
                <button
                  v-if="!item.settled"
                  class="btn-card-cancel"
                  @click="cancelActiveTransfer(item.transfer_id)"
                  title="单独取消此任务"
                >
                  <i class="ph ph-x"></i>
                  取消
                </button>
                <span v-else class="card-settled-text" :class="item.settled">
                  {{ item.settled === 'ok' ? '已完成' : (item.settled === 'cancelled' ? '已取消' : '失败') }}
                </span>
              </div>
            </div>
          </div>
        </div>

        <div class="active-dialog-footer">
          <button
            v-if="runningCount > 0"
            class="btn-danger-solid"
            @click="cancelActiveTransfer()"
            title="一键取消所有进行中的任务"
          >
            <i class="ph ph-stop-circle"></i> 全部取消 ({{ runningCount }})
          </button>
          <div class="footer-spacer"></div>
          <button class="btn-fluent-secondary" @click="showActiveTransfersModal = false">关闭</button>
        </div>
      </div>
    </div>

    <!-- 8. 全局提示 -->
    <transition name="toast-fade">
      <div v-if="toastMessage" class="fluent-toast" :class="{ error: toastIsError }">
        <i :class="toastIsError ? 'ph-fill ph-warning-circle' : 'ph-fill ph-check-circle'"></i>
        <span>{{ toastMessage }}</span>
      </div>
    </transition>
  </div>
</template>

<script setup lang="ts">
import {
  ref,
  reactive,
  computed,
  watch,
  onMounted,
  onUnmounted,
  onBeforeUnmount,
  nextTick
} from "vue";
import { useDeviceStore } from "./stores/deviceStore";
import CustomSelect, { type SelectOption } from "./components/CustomSelect.vue";
import {
  FeisuoBridge,
  parseBrowseFailure,
  parseTransferStatus,
  LocalDeviceInfo,
  DiscoveredDevice,
  DeviceRosterEntry,
  DiskFileInfo,
  RemoteFileEntry,
  KnownPlace,
  RootExpansion,
  VolumeInfo,
  TrustedDevice,
  ApprovalRequest,
  ClipboardContent,
  DeviceEndpoint,
  AccessScope,
  TransferRecord,
  TransferProgress,
  UpdateStatus,
  VOLUME_ANY,
  CAPS,
  ActiveGrant,
} from "./api/feisuoBridge";

const store = useDeviceStore();

/** 一个已加入待发清单的本地文件（必须带真实磁盘路径才能被后端读取） */
interface StagedFile {
  name: string;
  path: string;
  size: number;
  sizeFormatted: string;
  /** 由剪贴板临时落盘生成, 发送后需要清理 */
  temporary?: boolean;
  /**
   * 这一项展开后**实际会发送的文件数**（普通文件恒为 1）。
   *
   * ## 为什么需要它
   *
   * 拖一个文件夹进来，清单上只有「ai-memory-kit 4.0 KB」这一行，
   * 而 4.0 KB 是 **NTFS 目录项自身的大小**，不是里面 7 个文件的 17.8 MB。
   * 同一件事在提示里写 17.8 MB、在清单里写 4.0 KB —— 用户只看得见清单，
   * 于是他以为程序弄丢了 6 个文件。
   *
   * 不把目录**真的展开成 7 行**是故意的：`folder_scan` 上限 20000 个文件，
   * 一次拖一个 `node_modules` 就能在清单里生成两万行把界面卡死。
   * 发送时本来就会再展开一次（`send_files_to_dest_inner`），
   * 所以清单保留目录本身不丢信息 —— 前提是**显示的数是真的**。
   */
  expandedFiles?: number;
  /** 原始路径是目录。用来决定显示「N 个文件」还是「4.0 KB」 */
  isDir?: boolean;
  /** 目录内部被跳过的条目数（隐藏项 / 符号链接 / 内部目录） */
  skippedInside?: number;
}

type TabName = "send" | "shuttle" | "log" | "settings";

const currentTab = ref<TabName>("send");
const isRefreshing = ref(false);
const isDragOver = ref(false);
/** Tauri 原生拖放拿到的真实磁盘路径（网页层拿不到，§7.2 拖放的两条通路） */
const lastNativeDropPaths = ref<string[]>([]);
const toastMessage = ref("");
const toastIsError = ref(false);
const engineOnline = ref(false);
/** 引擎启动失败原因 (例如端口被占用), 常驻提示直到引擎恢复 */
const engineError = ref("");
let toastTimer: any = null;

const localInfo = reactive<LocalDeviceInfo>({
  device_id: "",
  device_name: "本机电脑",
  local_ip: "127.0.0.1",
  transfer_port: 42100,
  discovery_port: 42101,
  receive_dir: "",
  autostart: false,
  auto_receive: true,
  log_level: "INFO",
  max_log_size_mb: 5,
  max_history_records: 500,
  record_retention_days: 30,
  max_concurrent_transfers: 3,
  close_action: "ask",
  theme: "dark",
  device_count: 0,
  trusted_count: 0,
  app_version: "",
  auto_check_update: true,
  // 初始空数组 = 未限制。等 `loadLocalInfo()` 拿到真值再覆盖。
  allowed_peer_subnets: [],
});

const stagedFiles = ref<StagedFile[]>([]);
const localDiskFiles = ref<DiskFileInfo[]>([]);
const remoteDiskFiles = ref<RemoteFileEntry[]>([]);
const isLoadingLocal = ref(false);
const isLoadingRemote = ref(false);
const localMessage = ref("");
/** 本机当前浏览的相对路径 (空串 = 落盘根目录) */
const localPath = ref("");
const localParentPath = ref<string | null>(null);
const localTruncated = ref(false);
/** ---- v2：本机左栏也走真实文件系统（§7.1），与右栏同构 ---- */
/** 本机当前浏览的卷。空串 = 收件目录镜像（兼容模式） */
const localVolume = ref("");

/**
 * 收件根的显示名。
 *
 * ## 为什么不是"兼容模式"
 *
 * 早先这里写的是「收件目录（兼容模式）」，把一个**正常功能**说成了
 * "为了兼容老版本"。这是错的，而且会误导决策：
 *
 * - 收件目录是 `receive_dir` 配的那个路径，是**配置的根**，不是历史包袱；
 * - 这个下拉里的每一项都是"根"，没有哪个是兼容的；
 * - 用户看到"兼容模式"会以为该功能将来要删，于是不敢用。
 *
 * ## 为什么按配置取名而不是写死"收件目录"
 *
 * `receive_dir` 是可配的（设置里能改）。如果用户把它指到 `D:\飞梭收件`，
 * 地址栏却写着"收件目录"，那它就在说一件**不是真的事**。
 * 资源管理器的地址栏显示的是真实路径，这里同理。
 */
const receiveRootLabel = computed(() => {
  const dir = (localInfo.receive_dir || "").trim();
  if (!dir) return "收件目录";
  // 只取最后一段，完整路径由地址栏的 `localAddressDisplay` 负责
  const segs = dir.split(/[\\/]+/).filter(Boolean);
  return segs.length ? segs[segs.length - 1] : dir;
});
/** 本机可浏览的卷 */
const localVolumes = ref<VolumeInfo[]>([]);
/**
 * 本机常用位置（桌面 / 下载 / 文档…），来自 `get_known_places`。
 *
 * 路径由 Rust 侧用 `SHGetKnownFolderPath` 解析（经 `dirs` crate），
 * **不要在前端拼 `USERPROFILE + "\\Desktop"`** —— 桌面/文档/图片常被
 * 重定向到 OneDrive，且目录名本地化，两种情况拼出来都不存在。
 */
const localPlaces = ref<KnownPlace[]>([]);
/** 本机是否走真实卷模式（由 localVolume 派生，不单独存） */
const localVolumeMode = computed(() => localVolume.value !== "");
/** 本目录总条目数（分页用） */
const localTotal = ref(0);
/** 下一次 loadLocalFiles() 是"加载更多"而非"换目录" */
let localLoadMorePending = false;
const remoteMessage = ref("");
/** 对端当前浏览的相对路径 (空串 = 落盘根目录) */
const remotePath = ref("");
/** 上一级路径; 已在根目录时为 null */
const remoteParentPath = ref<string | null>(null);
/** 条目是否被条数上限截断 —— 必须提示, 否则用户以为文件丢了 */
const remoteTruncated = ref(false);
/** ---- v2 真实文件系统（§7.2 / §7.3）---- */
/** 当前浏览的卷（`C:` / `internal`）。空串 = 1.x 语义（对端收件目录） */
const remoteVolume = ref("");
/** 对端开放的可浏览卷。空数组 = 对端是旧版本，不显示下拉 */
const remoteVolumes = ref<VolumeInfo[]>([]);
/** 对端常用位置，随浏览响应一起回来，已按该对端的访问范围过滤过 */
const remotePlaces = ref<KnownPlace[]>([]);
/** 对端是否真的走了真实卷模式（探测结果，不是我们想用的模式） */
const remoteVolumeMode = ref(false);
/** 本目录总条目数（分页用） */
const remoteTotal = ref(0);
/** 下一次 loadRemoteFiles() 是"加载更多"而非"换目录" */
let remoteLoadMorePending = false;
/** 浏览对端时的本次传输码（对端处于「每次匹配码」等级时用，§2.3） */
const remoteGrantCode = ref("");
const selectedLocalNames = ref<Set<string>>(new Set());
/** 选中项用"相对落盘根目录的完整路径"作 key, 而不是只有文件名,
 *  否则在子目录里选中 `a/1.png` 会与根目录的 `1.png` 撞车。 */
const selectedRemoteNames = ref<Set<string>>(new Set());

const transferLogs = ref<TransferRecord[]>([]);
/** 最近诊断（含归因）。默认**不展开** —— 见模板里的说明。 */
const diagnostics = ref<import("./api/feisuoBridge").TransferDiagnostic[]>([]);
const showDiagnostics = ref(false);
/** 诊断拉取失败。区别于"没有记录"，要在界面上说出来而不是显示一个空列表。 */
const diagnosticsFailed = ref(false);
/** 传输历史路径清单查看弹窗 */
const showPathsModal = ref(false);
const selectedLogForPaths = ref<TransferRecord | null>(null);
const pathsSearchKeyword = ref("");
const copiedPathIndex = ref<number | null>(null);
const copyAllSuccess = ref(false);

function openPathsModal(log: TransferRecord) {
  selectedLogForPaths.value = log;
  pathsSearchKeyword.value = "";
  copiedPathIndex.value = null;
  copyAllSuccess.value = false;
  showPathsModal.value = true;
}

const filteredSelectedPaths = computed(() => {
  if (!selectedLogForPaths.value || !selectedLogForPaths.value.file_paths) return [];
  const list = selectedLogForPaths.value.file_paths;
  const kw = pathsSearchKeyword.value.trim().toLowerCase();
  if (!kw) return list;
  return list.filter((p) => p.toLowerCase().includes(kw));
});

function getBasename(path: string): string {
  if (!path) return "";
  const normalized = path.replace(/\\/g, "/");
  const segs = normalized.split("/");
  return segs[segs.length - 1] || path;
}

async function copySinglePath(path: string, index: number) {
  try {
    await navigator.clipboard.writeText(path);
    copiedPathIndex.value = index;
    showToast("路径已复制");
    setTimeout(() => {
      if (copiedPathIndex.value === index) copiedPathIndex.value = null;
    }, 2000);
  } catch (_e) {
    showToast("复制失败", true);
  }
}

async function copyAllSelectedPaths() {
  if (!selectedLogForPaths.value || !selectedLogForPaths.value.file_paths) return;
  try {
    const text = selectedLogForPaths.value.file_paths.join("\n");
    await navigator.clipboard.writeText(text);
    copyAllSuccess.value = true;
    showToast(`已复制全部 ${selectedLogForPaths.value.file_paths.length} 条路径`);
    setTimeout(() => {
      copyAllSuccess.value = false;
    }, 2000);
  } catch (_e) {
    showToast("复制失败", true);
  }
}

async function openFileInFolder(path: string) {
  try {
    await FeisuoBridge.openPathInFolder(path);
  } catch (e: any) {
    showToast(`定位失败: ${e?.message || e}`, true);
  }
}
const trustedDevices = ref<TrustedDevice[]>([]);
const pendingApproval = ref<ApprovalRequest | null>(null);
/** 审批界面里用户输入的本次传输码（§2.3） —— **已删**。
 *
 *  方向改成"接收方出码、发起方输入"之后，审批人不需要输入任何东西，
 *  所以这个状态连同它的输入框一起删掉了。留着会让下一个人以为
 *  审批人仍然要敲码，从而把方向改回去。 */
/**
 * 上一次提交后，**对方输入的码**是否不匹配。
 *
 * 保留弹窗而不是关掉，是刻意的：本机用户要看到"对方输错了"，
 * 然后把**同一个码**再念一遍。方向调过来之后，输错的人变成了
 * 发起方那一侧 —— 早先这里写的是"请重新填写"，因为那时是本机用户
 * 自己敲错。
 */
const approvalCodeRejected = ref(false);
/** 上一次已提交审批的 id（用于识别 core 发来的"重输码"请求） */
const lastSubmittedApprovalId = ref("");

// ---------------- 应用内更新 ----------------
const updateStatus = ref<UpdateStatus>({
  phase: "idle",
  message: "",
  currentVersion: "",
  latestVersion: "",
  tagName: "",
  pendingPath: "",
  bytesReceived: 0,
  bytesTotal: 0,
  releasePage: "",
  busy: false,
  isInstalled: false,
});

/**
 * 更新状态文案。
 *
 * 相较 Rust 侧 `message` 多一层兜底: 后端 message 为空时**绝不能**显示空白 ——
 * 托盘常驻下这块区域是用户判断"程序是不是在偷偷联网"的唯一依据,
 * 空白等于"有话没说"。
 */
const updateStatusText = computed(() => {
  const s = updateStatus.value;
  if (s.message) return s.message;
  switch (s.phase) {
    case "checking":
      return "正在检查更新…";
    case "downloading":
      return s.bytesTotal > 0
        ? `正在下载 ${Math.round((s.bytesReceived / s.bytesTotal) * 100)}%`
        : "正在下载新版本…";
    case "ready":
      return "新版本已下载就绪";
    case "uptodate":
      return `当前已是最新版本 v${s.currentVersion || localInfo.app_version}`;
    case "failed":
      return "检查更新失败，可点右侧「前往下载页」手动下载";
    default:
      return "尚未检查，可点击「检查更新」";
  }
});

const updatePercent = computed(() => {
  const { bytesReceived, bytesTotal } = updateStatus.value;
  if (bytesTotal <= 0) return 0;
  return Math.min(100, Math.round((bytesReceived / bytesTotal) * 100));
});

// ---------------- 传输任务池（支持多任务并发传输） ----------------
export interface ActiveTransferItem {
  transfer_id: string;
  current_file: string;
  direction: "Send" | "Receive";
  progress_percent: number;
  speed_bytes_per_sec: number;
  speedFormatted: string;
  bytes_transferred: number;
  total_bytes: number;
  peer_device_id: string;
  peer_device_name: string;
  settled: "ok" | "failed" | "cancelled" | null;
  last_updated_at: number;
  cleanupTimer?: any;
}

const activeTransfers = ref<Map<string, ActiveTransferItem>>(new Map());
/** 进行中传输任务管理面板 */
const showActiveTransfersModal = ref(false);
let transferWatchdogTimer: any = null;
let lastProgressAt = 0;

const runningTransfers = computed(() => {
  const list: ActiveTransferItem[] = [];
  for (const item of activeTransfers.value.values()) {
    if (item.settled === null) {
      list.push(item);
    }
  }
  return list;
});

const runningCount = computed(() => runningTransfers.value.length);
const isTransferring = computed(() => runningCount.value > 0);
// 供组件与生命周期跟踪
void isTransferring;

const maxConcurrency = computed(() => {
  const val = Number(localInfo.max_concurrent_transfers);
  return val > 0 ? val : 3;
});

const isConcurrencyFull = computed(() => runningCount.value >= maxConcurrency.value);

/** 汇总/单项进度（供底部进度条消费，向下兼容） */
const activeProgress = computed(() => {
  const running = runningTransfers.value;
  if (running.length === 0) {
    const all = Array.from(activeTransfers.value.values());
    if (all.length === 0) return null;
    return all[all.length - 1];
  }
  if (running.length === 1) {
    return running[0];
  }
  // 多任务并行汇总
  let totalBytes = 0;
  let transferredBytes = 0;
  let totalSpeed = 0;
  for (const t of running) {
    totalBytes += t.total_bytes;
    transferredBytes += t.bytes_transferred;
    totalSpeed += t.speed_bytes_per_sec;
  }
  const pct = totalBytes > 0 ? Math.min(100, Math.round((transferredBytes / totalBytes) * 100)) : 0;
  return {
    transfer_id: "aggregate",
    current_file: `${running.length} 个传输任务并行中`,
    direction: "Send" as const,
    progress_percent: pct,
    speed_bytes_per_sec: totalSpeed,
    speedFormatted: formatSpeed(totalSpeed),
    bytes_transferred: transferredBytes,
    total_bytes: totalBytes,
    peer_device_id: "",
    peer_device_name: "",
    settled: null,
    last_updated_at: Date.now(),
  };
});

/** 底部进度条文案。结束后不能继续说「正在」。 */
const progressDockLabel = computed(() => {
  const p = activeProgress.value;
  if (!p) return "";
  const sending = p.direction === "Send";
  if (p.settled === "ok") return sending ? "发送完成" : "接收完成";
  if (p.settled === "failed") return sending ? "发送失败" : "接收失败";
  if (p.settled === "cancelled") return sending ? "发送已取消" : "接收已取消";
  if (runningCount.value > 1) return `正在传输 (${runningCount.value} 个并发)`;
  return sending ? "正在发送" : "正在接收";
});

const concurrentOptions: SelectOption[] = [
  { value: "1", label: "1 个任务（单任务串行）" },
  { value: "2", label: "2 个任务并发" },
  { value: "3", label: "3 个任务并发（推荐）" },
  { value: "5", label: "5 个任务并发" },
  { value: "8", label: "8 个任务并发" },
];

const concurrentTransfersSelect = computed({
  get: () => String(localInfo.max_concurrent_transfers || 3),
  set: (v: string) => {
    localInfo.max_concurrent_transfers = Number(v) || 3;
    void saveSettings();
  },
});

const themeSelect = computed({
  get: () => store.theme,
  set: (v: "dark" | "light") => store.applyTheme(v),
});

const closeActionOptions: SelectOption[] = [
  { value: "ask", label: "每次询问（最小化 / 退出）" },
  { value: "tray", label: "直接最小化到托盘" },
  { value: "exit", label: "直接退出程序" },
];

const themeOptions: SelectOption[] = [
  { value: "dark", label: "深色主题" },
  { value: "light", label: "浅色主题" },
];

const logLevelOptions: SelectOption[] = [
  { value: "INFO", label: "INFO（常规运行）" },
  { value: "DEBUG", label: "DEBUG（详细排障）" },
  { value: "WARN", label: "WARN（仅告警）" },
];

const maxHistoryOptions: SelectOption[] = [
  { value: 200, label: "保留最近 200 条" },
  { value: 500, label: "保留最近 500 条" },
  { value: 1000, label: "保留最近 1000 条" },
];

const retentionDaysOptions: SelectOption[] = [
  { value: 7, label: "保留 7 天" },
  { value: 30, label: "保留 30 天" },
  { value: 90, label: "保留 90 天" },
  { value: 0, label: "永久保留（仅受条数限制）" },
];

/** 设备名称编辑框的独立副本: 只有失焦/回车才提交, 避免每敲一个字就写一次磁盘 */
const deviceNameInput = ref(localInfo.device_name);
const nameSaved = ref(false);
let nameSavedTimer: any = null;

const sendButtonText = computed(() => {
  if (isConcurrencyFull.value) return `并发已满 (${runningCount.value}/${maxConcurrency.value})`;
  if (!store.selectedDevice) return "请先选择目标设备";
  return `发送至 ${store.selectedDevice.device_name}`;
});

const totalStagedSize = computed(() => formatBytes(stagedFiles.value.reduce((a, c) => a + c.size, 0)));

/**
 * 待发清单里的**真实文件数**（目录按展开后的文件数算）。
 *
 * ## 为什么不能用 `stagedFiles.length`
 *
 * 拖一个含 7 个文件的文件夹进来，清单上只有 1 行，但**实际会发 7 个文件**。
 * 头部写「1 项」时用户点发送，看着像只发了 1 个 —— 而发送时
 * `send_files_to_dest_inner` 会再展开一次，于是真发了 7 个。
 * 结果是"清单说 1、实际 7"，两个都是真的，指向不同的东西。
 *
 * 决定：**按会真正发送的文件数显示**。清单的职责是让用户知道
 * 这次传输的实际规模，不是复述列表行数。
 */
const stagedFileCount = computed(() =>
  stagedFiles.value.reduce((a, f) => a + (f.expandedFiles ?? 1), 0)
);

/** 目录行上的「N 个文件」。普通文件返回空串（模板里只对目录显示）。 */
function stagedRowSize(f: StagedFile): string {
  if (!f.isDir) return "";
  return `${f.expandedFiles ?? 0} 个文件`;
}

/** 目录行的悬浮提示：把跳过项也说清楚，别让用户以为是"漏了"。 */
function stagedRowTitle(f: StagedFile): string {
  if (!f.isDir) return f.sizeFormatted;
  const skipped = f.skippedInside ?? 0;
  return `${f.expandedFiles ?? 0} 个文件 · ${f.sizeFormatted}` +
    (skipped > 0 ? `（内部跳过 ${skipped} 项：隐藏文件 / 符号链接 / node_modules 等）` : "");
}

/**
 * 待发清单这一批的落点，**显示在清单里**。
 *
 * ## 为什么必须显示
 *
 * 落点有两种来源：拖进窗口 / 剪贴板 → 对方收件根；穿梭里拖文件夹 →
 * 对方地址栏当前那一层。用户点「发送」之前无从知道是哪一种，
 * 而两者落到**完全不同的目录**。发出去之后再去收件目录找，
 * 找不到的人会去怀疑文件丢了，而不是怀疑落点不同。
 *
 * 一行字的成本，换掉一次"文件呢？"，值得。
 */
const stagedDestLabel = computed(() => {
  const dev = store.selectedDevice;
  if (!dev) return "尚未选择设备";
  const sub = pendingDestSubPath.value;
  return sub ? `${dev.device_name} 的 ${sub}` : `${dev.device_name} 的收件目录`;
});

/** 落点那一行的完整说法，挂在发送按钮的 title 上（D21） */
const stagedDestTitle = computed(() => {
  const dev = store.selectedDevice;
  if (!dev) return "请先在左侧选择目标设备";
  const sub = pendingDestSubPath.value;
  return sub
    ? `发送到 ${dev.device_name} 的 ${sub}（对方收件目录下的这一层）`
    : `发送到 ${dev.device_name} 的收件目录根层`;
});

// ---------------- 配对 ----------------
const pinSecondsLeft = ref(30);
let pinCountdownTimer: any = null;
const showPairingDialog = ref(false);
/** 「已隐藏」抽屉展开状态（§3.1） */
const showHiddenDrawer = ref(false);

/**
 * 是否显示防火墙引导。
 *
 * 只在「一台设备都没搜到，且已经搜了 8 秒以上」时出现。
 *
 * ## 为什么要有 8 秒延迟
 *
 * 局域网里通常 1~3 秒就能搜到（mDNS/广播的收敛时间），立刻显示
 * 这段文字会变成"正常的启动过程里闪一条警告"，而它其实只在
 * 真的有问题时有用。延迟把"还在正常收敛"和"确实收不到"分开。
 *
 * ## 为什么不做成"检测到被防火墙拦截才显示"
 *
 * 客户端**没有可靠的检测手段**：
 * - 引擎起来了、端口也绑上了（本机侧一切正常）；
 * - 被拦的是**入站**，从本机看不出来；
 * - Windows 不会通知应用"你的规则被拒绝了"。
 *
 * 与其做一个假检测（误报比漏报更糟），不如用一个**保守的时间条件**：
 * 只在"确实没搜到任何设备"时才提示，而搜不到本身就是最强的信号。
 */
const FIREWALL_HINT_AFTER_MS = 8000;const showFirewallHint = ref(false);
let firewallHintTimer: ReturnType<typeof setTimeout> | null = null;

function armFirewallHint() {
  if (firewallHintTimer) clearTimeout(firewallHintTimer);
  showFirewallHint.value = false;
  firewallHintTimer = setTimeout(() => {
    // 到点了还一台都没有才提示；搜到了就当无事发生
    if (store.roster.length === 0) showFirewallHint.value = true;
  }, FIREWALL_HINT_AFTER_MS);
}

function disarmFirewallHint() {
  if (firewallHintTimer) clearTimeout(firewallHintTimer);
  firewallHintTimer = null;
  showFirewallHint.value = false;
}

// 搜到设备就立刻撤掉提示 —— 它已经没有意义了
watch(
  () => store.roster.length,
  (n) => {
    if (n > 0) disarmFirewallHint();
  }
);
onBeforeUnmount(disarmFirewallHint);

// =======================================================================
// 来源网段白名单（设置页）
// =======================================================================
//
// 可编辑副本 + 原始快照，用 `subnetsDirty` 判断"有没有未保存的改动"。
// 不直接 v-model 绑 localInfo —— 那会让"编辑到一半离开页面"也把配置改了。
const savedSubnets = ref<string[]>([]);
const allowedPeerSubnets = ref<string[]>([]);

const subnetsDirty = computed(
  () => JSON.stringify(allowedPeerSubnets.value) !== JSON.stringify(savedSubnets.value)
);

/** 局域网 IP 的**格式**校验（前端提示用）。
 *
 *  与 `isValidCidr` 同一条原则：这里只做格式判断，真正的判定在后端
 * （`probe_device` / `pair_with_device` 会自己解析并失败）。前端漏判
 * 不会造成错误连接 —— 最多是让一次注定失败的探测早半秒被拦下来。
 *
 *  为什么要加：此前两个入口（直连 IP、配对方 IP）只判空不判格式，
 *  于是输入 `abc` 或 `999.1.1.1` 也能按下按钮，然后等一次**必然失败**
 *  的网络探测才把后端的原始错误显示出来。用户看到的是"连接失败"，
 *  而不是"你输的不是 IP" —— 后者才是他能改的那件事。
 *
 *  刻意**只接受 IPv4 点分十进制**：这是局域网工具，mDNS 发现出来的
 *  地址形态就是这样。接受主机名会诱使用户输 `my-pc.local`，而传输端口
 *  依赖对端设置、无法靠 DNS 猜出来，失败原因更难解释。
 */
function isValidIpv4(raw: string): boolean {
  const t = (raw || "").trim();
  if (!t) return false;
  const m = /^(\d{1,3}(?:\.\d{1,3}){3})$/.exec(t);
  if (!m) return false;
  // 逐段判 <= 255；也顺手挡掉 "01.2.3.4" 这种前导零（部分解析器当八进制）
  return m[1].split(".").every((part) => {
    if (part.length > 1 && part.startsWith("0")) return false;
    const n = Number(part);
    return n >= 0 && n <= 255;
  });
}

/** 输入框下方的行内提示：为空时不提示（还没开始输）。 */
const directConnectIpHint = computed(() => {
  const v = directConnectIp.value.trim();
  if (!v) return "";
  return isValidIpv4(v) ? "" : "这不像一个 IPv4 地址，应该形如 192.168.1.120";
});

const pairTargetIpHint = computed(() => {
  const v = inputTargetIp.value.trim();
  // 自动填充的地址来自 mDNS 发现，不需要提示
  if (!v || isManualIpMode.value === false) return "";
  return isValidIpv4(v) ? "" : "这不像一个 IPv4 地址，应该形如 192.168.1.120";
});

/** 能不能被后端 `parse_cidr` 认出来。
 *
 *  这里只做**格式**校验（前端提示用）。真正的判定在后端
 *  `auth_policy::subnet_verdict`，而且那边是 fail-closed ——
 *  前端漏判不会造成放行，只会让提示晚一点出现。
 */
function isValidCidr(raw: string): boolean {
  const t = (raw || "").trim();
  if (!t) return false;
  const m = /^(\d{1,3}(?:\.\d{1,3}){3})(?:\/(\d{1,3}))?$/.exec(t);
  if (!m) return false;
  const octets = m[1].split(".").map(Number);
  if (octets.some((o) => o > 255)) return false;
  if (m[2] !== undefined) {
    const bits = Number(m[2]);
    if (bits > 32) return false;
  }
  return true;
}

const badSubnets = computed(() => allowedPeerSubnets.value.filter((s) => !isValidCidr(s)));

async function saveAllowedSubnets() {
  const clean = allowedPeerSubnets.value.map((s) => s.trim()).filter(Boolean);
  try {
    await FeisuoBridge.updateAppConfig({ allowed_peer_subnets: clean });
    savedSubnets.value = [...clean];
    allowedPeerSubnets.value = [...clean];
    showToast(
      clean.length
        ? `已保存：只接受来自 ${clean.join("、")} 的连接`
        : "已保存：不限制来源网段"
    );
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "保存网段设置失败"), true);
  }
}
/** 剪贴板预览浮层内容（D5：预览默认开启；预览阶段不落盘） */
const clipboardPreview = ref<ClipboardContent | null>(null);
const pairMode = ref<"input" | "show" | "success">("input");
const inputPin = ref("");
const inputTargetIp = ref("");
const targetDeviceForPairing = ref<DiscoveredDevice | null>(null);
const localPairPin = ref("------");
const isPairingSubmit = ref(false);
const pairErrorMessage = ref("");
const pinInputRef = ref<HTMLInputElement | null>(null);
const isManualIpMode = ref(false);
const pairedSuccessDevice = ref<TrustedDevice | null>(null);
let pairingAbortFlag = false;

/** 配对目标候选：本机之外的可见设备（离线设备不能配对，故只看在线） */
const discoveredPeerDevices = computed<DeviceRosterEntry[]>(() =>
  store.roster.filter(
    (d) => d.device_id !== localInfo.device_id && d.visible && d.presence === "online"
  )
);

const discoveredPeerDeviceOptions = computed<SelectOption[]>(() => {
  return discoveredPeerDevices.value.map((dev) => ({
    value: dev.ip || "",
    label: `${dev.device_name} (${dev.ip})`,
  }));
});

/**
 * 目标设备的可达端点。
 *
 * 名册里 `ip` / `transfer_port` 只在**在线**时有值；离线设备只有 `last_ip`。
 * 这里给出带兜底的解析，并在离线时明确返回不可用 —— 调用方据此
 * 提示"目标离线"而不是拿一个空 IP 去连接（§3.8）。
 *
 * ## 端点覆盖（§3.9 / P4 ⑧）
 *
 * 用户可以给某台设备指定走哪条路（覆盖网 / 局域网）。
 * 覆盖了才用覆盖值 —— 否则按"覆盖网优先"自动选（D6）。
 * 自动选的结果来自后端 `preferred_endpoint`，它按端点性质排序：
 * 一台同时有 ZeroTier 与物理网卡的设备会被选中 ZeroTier 那条。
 */
const targetEndpoint = computed(() => {
  const dev = store.selectedDevice;
  if (!dev) return null;
  // 用户显式指定的端点优先
  const pinned = pinnedEndpoints[dev.device_id];
  const ip = pinned?.ip || dev.ip || dev.last_ip || preferredEndpoint.value?.ip || "";
  const port =
    pinned?.port || dev.transfer_port || preferredEndpoint.value?.port || 0;
  if (!ip || !port) return null;
  return {
    ip,
    port,
    offline: dev.presence !== "online",
    /** 实际走的这条路（用于界面显示"经虚拟网卡"） */
    viaOverlay: pinned
      ? pinned.kind === "overlay"
      : (dev.ip ? dev.ip.startsWith("100.") : (preferredEndpoint.value?.ip ?? "").startsWith("100.")),
  };
});

/** 用户手动指定的端点：`device_id -> { ip, port, kind }` */
const pinnedEndpoints = reactive<Record<string, { ip: string; port: number; kind: string }>>({});

/** 后端算出的首选端点（覆盖网优先） */
const preferredEndpoint = ref<DeviceEndpoint | null>(null);

/** 当前选中设备的全部已知端点（供路径切换 UI 用） */
const deviceEndpoints = ref<DeviceEndpoint[]>([]);

/** 端点的"多久之前"人话 */
function humanizeEndpoint(ts: number): string {
  if (ts <= 0) return "未知";
  const d = Math.floor(Date.now() / 1000) - ts;
  if (d < 60) return "刚刚";
  if (d < 3600) return `${Math.floor(d / 60)} 分钟前`;
  if (d < 86400) return `${Math.floor(d / 3600)} 小时前`;
  return `${Math.floor(d / 86400)} 天前`;
}

/** 选中设备变化时刷新它的端点列表与首选端点 */
watch(
  () => store.selectedDeviceId,
  (id) => {
    deviceEndpoints.value = [];
    preferredEndpoint.value = null;
    if (!id) return;
    void refreshEndpoint(id);
    void FeisuoBridge.listDeviceEndpoints(id)
      .then((eps) => {
        if (store.selectedDeviceId === id) deviceEndpoints.value = eps;
      })
      .catch(() => {
        /* 拿不到就只显示"自动" */
      });
  },
  { immediate: true }
);

/** 切换某台设备的端点（§3.9）：`null` = 交回自动选择 */
async function pinDeviceEndpoint(deviceId: string, ep: DeviceEndpoint | null) {
  if (ep === null) {
    delete pinnedEndpoints[deviceId];
    showToast("已交回自动选择（覆盖网优先）");
  } else {
    if (!ep.port) {
      showToast("该端点还没有已知的传输端口，无法直接使用；请等它在线一次", true);
      return;
    }
    pinnedEndpoints[deviceId] = { ip: ep.ip, port: ep.port, kind: ep.kind };
    showToast(
      `已固定走「${endpointKindLabel(ep.kind)}」(${ep.ip})；` +
        "仅对这台设备生效"
    );
  }
  await refreshEndpoint(deviceId);
}

async function refreshEndpoint(deviceId: string) {
  try {
    const ep = await FeisuoBridge.getPreferredEndpoint(deviceId);
    // 返回时可能已经换成另一台。preferredEndpoint 不按设备分开存，
    // 写上去之后，新设备还没自己的地址时会拿上一台的 IP 去连。
    if (store.selectedDeviceId !== deviceId) return;
    preferredEndpoint.value = ep;
  } catch {
    if (store.selectedDeviceId === deviceId) preferredEndpoint.value = null;
  }
}

function endpointKindLabel(kind: string): string {
  if (kind === "overlay") return "虚拟网卡";
  if (kind === "lan") return "局域网";
  if (kind === "loopback") return "本机回环";
  if (kind === "public") return "公网";
  return "未知";
}

// ---------------- 直连 ----------------
const showDirectConnectDialog = ref(false);
const directConnectIp = ref("");
const isConnectingIp = ref(false);
const directConnectMsg = ref("");
const isDirectConnectSuccess = ref(false);

// ---------------- 关闭确认 ----------------
const showCloseDialog = ref(false);

// ---------------- 事件解绑 ----------------
let unlisteners: Array<() => void> = [];

// =======================================================================
// 可点击非按钮元素的键盘激活
// =======================================================================
//
// ## 为什么需要它
//
// 界面里有几处"看起来能点、实际鼠标专享"的元素: 设备行、本机卡片、
// 「其他设备」折叠标题、保存目录路径、信任等级徽章。它们都是
// `<div>` + `@click`, 而 `<div>` 默认不可聚焦 —— 键盘用户 Tab
// 永远到不了, 按 Enter 也没反应。这些恰好是**主操作入口**
// (选设备 = 这一整个应用的核心动作), 不是边角装饰。
//
// ## 为什么是指令而不是给每个元素手写 @keydown
//
// 手写会在 5 个元素上重复 5 遍同样的判断, 而且迟早会漏掉一处。
// 指令把"Enter 和 Space 都算激活"这条规则收在一处 —— 注意
// **Space 必须处理**: 按钮默认行为是 Space 激活、Enter 激活,
// 而 `<div>` 上 Space 会滚动页面, 不显式拦掉就变成"按空格页面
// 滚走了, 设备没选中"。
//
// 用 capture 阶段是为了抢在页面滚动之前 —— 滚动是默认动作,
// 在冒泡阶段已经发生得太晚。
const vKeyActivate = {
  mounted(el: HTMLElement) {
    el.addEventListener("keydown", (e: KeyboardEvent) => {
      if (e.key !== "Enter" && e.key !== " " && e.key !== "Spacebar") return;
      e.preventDefault();
      e.stopPropagation();
      el.click();
    });
  }
};

// =======================================================================
// 弹窗栈：Escape 关闭 + 焦点归还 + Tab 焦点陷阱
// =======================================================================
//
// ## 为什么需要它
//
// 改版前 7 个弹窗**全部**只能靠"点遮罩空白处"或"点右上角 ×"关闭。
// 对鼠标用户没问题, 对键盘用户是两条死路：
//
//   1. **Escape 无效**。焦点在弹窗内某处时按 Esc 什么都没发生,
//      而遮罩的 `@click.self` 要求鼠标落在遮罩**自己**身上 ——
//      Tab 导航永远到不了那里, 于是键盘用户可能被关在里面。
//   2. **焦点会跑到弹窗后面**。Tab 走完弹窗里的按钮后继续往后,
//      焦点落回被遮住的侧栏/工作区, 用户看着"弹窗还在, 但我操作的
//      是背后的界面"。屏幕阅读器用户还会因为焦点落在
//      `aria-hidden` 之外而彻底失去上下文。
//
// ## 为什么是"栈"而不是每个弹窗各写一遍
//
// 这 7 个弹窗可以**叠加**(配对框里再弹审批框), 每层各自监听
// Escape 会一次关掉全部 —— 用户以为关掉了最上面那个, 结果
// 底下的配对框也跟着消失了, 正在填的 6 位码全丢。
// 栈保证"只关最上面那层", 这才是用户按下 Escape 时的预期。
//
// ## 为什么 Tab 循环要包成环
//
// 弹窗是模态的: 焦点既不能跑到它外面(见上), 也不能在它里面
// 转圈丢失。首尾相接的环是标准做法 —— 也正好是原生 `<dialog>`
// 的行为, 这里手写是因为要跨多个自定义遮罩容器统一处理。

/** 当前打开的弹窗, 从底到顶; 顶层是最后一项。 */
const modalStack = ref<HTMLElement[]>([]);

/**
 * 打开弹窗前聚焦的那个元素 —— 关掉后要还回去。
 *
 * 少了这一句, 键盘用户关掉弹窗后焦点掉回 `<body>`, 下一次 Tab
 * 又得从页面第一个控件重新走一遍 —— 配对框连开三次就得按几十下
 * Tab 才能回到刚才的位置。原生 `<dialog>` 关闭时也这么做。
 */
let lastDialogOpener: HTMLElement | null = null;

/**
 * 弹窗打开后把焦点移进去。
 *
 * 优先找 `[autofocus]`(Vue 渲染后浏览器才会处理它), 找不到就取
 * 第一个可聚焦控件。这样键盘用户打开弹窗时焦点已经在里面,
 * 可以直接输入 —— 配对框的 6 位码、IP 输入框都依赖这一点。
 *
 * 找不到可聚焦元素(比如"正在探测…"的瞬时态)就退回到弹窗容器本身:
 * 至少焦点进来了, 不会留在背后的界面上。
 */
function focusFirstInDialog(dialog: HTMLElement) {
  const first = dialog.querySelector<HTMLElement>(
    "[autofocus], input:not([type=hidden]):not([disabled]), select:not([disabled]), textarea:not([disabled]), button:not([disabled]), [tabindex]:not([tabindex='-1'])"
  );
  (first ?? dialog).focus({ preventScroll: true });
}

/**
 * `v-modal-focus` 指令: 弹窗挂载即接管焦点, 卸载即归还栈。
 *
 * 为什么用指令而不是 `ref`:
 * 模板里同时有 6 个弹窗都标了这个标记, 而 `<script setup>` 的
 * `ref("modalEl")` 只有一个槽位 —— 后挂载的会把先挂载的顶掉,
 * 栈里只剩最后一个, 于是"只关最上层"的 Escape 逻辑会在
 * 多层叠加时关错弹窗。指令按**每个元素实例**走
 * mounted/unmounted, 天生支持任意多个。
 */
const vModalFocus = {
  mounted(el: HTMLElement) {
    // 只记**第一层**的来源: 叠加弹窗关掉后应该回到最初那个按钮,
    // 而不是回到上一层弹窗里某个刚被卸掉的元素。
    if (modalStack.value.length === 0) {
      lastDialogOpener = document.activeElement as HTMLElement | null;
    }
    modalStack.value = [...modalStack.value, el];
    nextTick(() => focusFirstInDialog(el));
  },
  unmounted(el: HTMLElement) {
    modalStack.value = modalStack.value.filter((m) => m !== el);
    // 最后一层关掉才归还焦点: 中间层关闭时上层还开着, 此时把焦点
    // 抢回按钮会直接把用户踢出仍然打开的弹窗。
    if (modalStack.value.length === 0) {
      lastDialogOpener?.focus({ preventScroll: true });
      lastDialogOpener = null;
    }
  }
};

/** 顶层弹窗里的可聚焦元素, 按 tab 顺序排列。 */
function focusablesIn(root: HTMLElement): HTMLElement[] {
  return Array.from(
    root.querySelectorAll<HTMLElement>(
      "input:not([type=hidden]):not([disabled]), select:not([disabled]), textarea:not([disabled]), button:not([disabled]), a[href], [tabindex]:not([tabindex='-1'])"
    )
  ).filter((el) => el.offsetParent !== null || el === document.activeElement);
}

/** 当前应该响应 Escape 的弹窗。审批框不参与 —— 它必须由人决定。 */
const ESCAPABLE_MODALS: Array<{ ref: () => unknown; close: () => void }> = [
  { ref: () => codePrompt.value, close: () => cancelCodePrompt() },
  { ref: () => showCloseDialog.value, close: () => (showCloseDialog.value = false) },
  {
    ref: () => showDirectConnectDialog.value,
    close: () => (showDirectConnectDialog.value = false)
  },
  { ref: () => showPairingDialog.value, close: () => (showPairingDialog.value = false) },
  { ref: () => clipboardPreview.value, close: () => (clipboardPreview.value = null) }
];

function handleDialogKeydown(e: KeyboardEvent) {
  // Escape: 只关最上面那个可关的弹窗, 而不是全部
  if (e.key === "Escape") {
    for (let i = ESCAPABLE_MODALS.length - 1; i >= 0; i--) {
      if (ESCAPABLE_MODALS[i].ref()) {
        e.stopPropagation();
        e.preventDefault();
        ESCAPABLE_MODALS[i].close();
        return;
      }
    }
    return;
  }

  // Tab: 把焦点锁在顶层弹窗内
  if (e.key !== "Tab") return;
  const top = modalStack.value[modalStack.value.length - 1];
  if (!top) return;
  const items = focusablesIn(top);
  if (items.length === 0) {
    // 全是不可聚焦元素(纯展示型弹窗): 焦点留在容器上, 别让它跑出去
    e.preventDefault();
    top.focus({ preventScroll: true });
    return;
  }
  const first = items[0];
  const last = items[items.length - 1];
  const active = document.activeElement as HTMLElement | null;
  // 焦点在弹窗之外(初始态 / 刚被外部脚本抢走): 直接收进第一个
  if (!active || !top.contains(active)) {
    e.preventDefault();
    first.focus({ preventScroll: true });
    return;
  }
  // 在首尾处回绕, 而不是让焦点掉出弹窗跑到被遮住的界面上
  if (e.shiftKey && active === first) {
    e.preventDefault();
    last.focus({ preventScroll: true });
  } else if (!e.shiftKey && active === last) {
    e.preventDefault();
    first.focus({ preventScroll: true });
  }
}

// 弹窗打开时锁掉背景滚动: `overflow:hidden` 加在 <body> 上,
// 否则 Tab 到底部时窗口会跟着滚, 弹窗却纹丝不动。
watch(
  () => modalStack.value.length,
  (count, prev) => {
    if (count > 0 && (!prev || prev === 0)) document.body.style.overflow = "hidden";
    else if (count === 0) document.body.style.overflow = "";
  }
);

function handleWindowClickForDropdowns(e: MouseEvent) {
  const t = e.target as HTMLElement | null;
  if (!t?.closest(".custom-dropdown-wrap")) {
    localDropdownOpen.value = false;
    remoteDropdownOpen.value = false;
  }
}

onMounted(() => {
  window.addEventListener("keydown", handleDialogKeydown, true);
  window.addEventListener("click", handleWindowClickForDropdowns, true);
});

onBeforeUnmount(() => {
  window.removeEventListener("keydown", handleDialogKeydown, true);
  window.removeEventListener("click", handleWindowClickForDropdowns, true);
  document.body.style.overflow = "";
});

// =======================================================================
// 生命周期
// =======================================================================
onMounted(async () => {
  // 主题以本地配置为准, 避免与后端 config.json 不一致
  const info = await FeisuoBridge.getLocalInfo();
  Object.assign(localInfo, info);
  deviceNameInput.value = info.device_name;
  store.applyTheme(info.theme === "light" ? "light" : "dark");

  await Promise.all([
    refreshDevices(),
    loadLocalFiles(),
    loadSettingsData(),
    loadTrustedDevices(),
    loadUpdateStatus(),
    // 常用位置是纯本机的（不依赖设备列表），放进来一起取，省一次往返。
    loadKnownPlaces(),
    // §2.3.1：授权窗口秒数**必须**由 core 报上来。前端自己写死
    // 会在用户调大配置时让界面上写着 5 分钟、实际给 10 分钟。
    FeisuoBridge.getSessionGrantWindow().then((s) => (grantWindowSecs.value = s)),
    // 生效中的短期授权要在设备徽标上**看得见** —— 看不见的免确认
    // 就是用户以为关掉了、其实还开着。
    loadActiveGrants(),
  ]);

  // 防火墙引导的计时从**首次刷新完成**起算，而不是从 onMounted 起算 ——
  // 上面这批 await 本身要花时间（读配置、列目录、查更新），
  // 从 onMounted 起算会把"程序刚启动"误判成"搜了很久还没搜到"。
  armFirewallHint();

  // 清理超期剪贴板暂存（§6.5）。
  // 剪贴板里常混着密码/验证码, 抓了没发就永久留在磁盘上是隐私漏洞。
  // 失败只提示不阻断启动 —— 这是一次 housekeeping, 不是关键路径。
  try {
    const purged = await FeisuoBridge.purgeClipboardStaging(24);
    if (purged > 0) {
      console.info(`[飞梭] 已清理 ${purged} 个超期剪贴板暂存文件`);
    }
  } catch {
    /* 忽略 */
  }

  // 后端事件 -> UI
  unlisteners.push(
    await FeisuoBridge.onTransferProgress(handleTransferProgress),
    await FeisuoBridge.onApprovalRequest((req) => {
      // 审批弹窗是**必须由人决定**的事件, 而 core 侧只等 60 秒。
      // 窗口隐藏时这个弹窗用户看不见, 结果就是静默等满 60 秒然后被拒 ——
      // 对方以为传输成功, 本机什么都没发生, 双方都对不上账。
      // 所以这里必须把窗口唤出来。
      FeisuoBridge.ensureWindowVisible().catch(() => {});
      // 同一个 approval_id 再次到达 = core 检测到**对方出示的码**不匹配，
      // 在同一窗口里要求重试（§2.3）。本机码不变，用户只需再念一遍。
      // 判定要跟"上次提交的 id"比而不是跟 `pendingApproval` 比 ——
      // 提交时弹窗已经被置空了, 拿它比永远不相等。
      if (lastSubmittedApprovalId.value === req.approval_id) {
        approvalCodeRejected.value = true;
        pendingApproval.value = req;
        lastSubmittedApprovalId.value = "";
        return;
      }
      // 全新请求: 清掉上一次"对方输错"的标记。
      approvalCodeRejected.value = false;
      lastSubmittedApprovalId.value = "";
      pendingApproval.value = req;
    }),
    await FeisuoBridge.onWindowCloseRequested(requestClose),
    // 发现事件只做"有新设备"的提示节拍, 列表本身仍以主动拉取为准, 避免频繁重排
    await FeisuoBridge.onDeviceDiscovered(() => scheduleDeviceRefresh()),
    await FeisuoBridge.onConfigUpdated(async () => {
      await loadLocalInfo();
    }),
    await FeisuoBridge.onEngineError((msg) => {
      // 这是同类问题里最严重的一处: 引擎启动失败意味着**所有**传输都不会工作。
      // 托盘常驻 + 开机自启下, 窗口根本不显示, 用户只看到一个托盘图标,
      // 以为程序在正常运行 —— 直到某天需要传文件才发现永远传不了。
      // 托盘图标在、功能全无, 是最难自查的一类故障, 必须主动告知。
      FeisuoBridge.ensureWindowVisible().catch(() => {});
      engineError.value = msg;
      engineOnline.value = false;
      showToast("局域网守护引擎启动失败", true);
    }),
    // 后台调度可能在窗口不可见时完成检查/下载, 事件是唯一能把结果
    // 推到界面上的通路。**不能**在这里唤窗: 用户没要求看更新,
    // 为此把托盘常驻程序的主界面弹出来是打扰。
    await FeisuoBridge.onUpdateStatus((s) => {
      updateStatus.value = s;
    }),
    // 全局热键 Ctrl+Alt+V（§6.4）。窗口隐藏时也能触发 ——
    // 这是"托盘常驻"这个形态下唯一还能用的快捷键通路。
    await FeisuoBridge.onOpenClipboardRequest(() => {
      currentTab.value = "send";
      void handleSendClipboard();
    })
  );

  // 原生拖放: WebView2 下浏览器拿不到真实磁盘路径, 必须靠这个事件
  unlisteners.push(await FeisuoBridge.onNativeDragDrop(handleNativeDrop));

  // 兜底轮询: mDNS 信标 3 秒一次, 保证列表最终收敛
  deviceTimer = setInterval(() => refreshDevices(true), 5000);
});

onUnmounted(() => {
  stopPinCountdown();
  clearTransferWatchdog();
  if (deviceTimer) clearInterval(deviceTimer);
  if (deviceRefreshTimer) clearTimeout(deviceRefreshTimer);
  for (const item of activeTransfers.value.values()) {
    if (item.cleanupTimer) clearTimeout(item.cleanupTimer);
  }
  if (toastTimer) clearTimeout(toastTimer);
  if (nameSavedTimer) clearTimeout(nameSavedTimer);
  unlisteners.forEach((fn) => {
    try {
      fn();
    } catch {
      /* 忽略解绑异常 */
    }
  });
});

let deviceTimer: any = null;
let deviceRefreshTimer: any = null;

/** 发现事件很密集 (每台设备 3 秒一发), 这里做 1.2 秒防抖 */
function scheduleDeviceRefresh() {
  if (deviceRefreshTimer) clearTimeout(deviceRefreshTimer);
  deviceRefreshTimer = setTimeout(() => {
    deviceRefreshTimer = null;
    void refreshDevices(true);
  }, 1200);
}

watch(currentTab, async (tab) => {
  if (tab === "settings") {
    // 切回设置页时必须重新拉快照: 托盘常驻下窗口可能挂了很久,
    // 期间后台调度可能已经下载完, 只靠事件会一直显示旧状态。
    await Promise.all([loadSettingsData(), loadUpdateStatus()]);
  } else if (tab === "log") {
    await loadTransferHistory();
  } else if (tab === "shuttle") {
    bootstrapRemoteBrowseMode();
    await Promise.all([loadLocalFiles(), loadRemoteFiles()]);
  }
});

// =======================================================================
// 数据加载
// =======================================================================
/**
 * 读本机常用位置。
 *
 * 失败**不弹提示**：常用位置是"锦上添花"的入口，取不到只是下拉里少一组
 * 选项，而穿梭、发送、取回全都不受影响。为此打断用户操作、给一个红条，
 * 是把可选功能当成必需功能来报。
 */
async function loadKnownPlaces() {
  try {
    localPlaces.value = await FeisuoBridge.getKnownPlaces();
  } catch (e) {
    localPlaces.value = [];
    console.warn("读取本机常用位置失败（不影响穿梭）:", e);
  }
}

async function loadLocalInfo() {
  const info = await FeisuoBridge.getLocalInfo();
  Object.assign(localInfo, info);
  // 网段白名单的编辑副本也要跟着刷新 —— 但**不能覆盖用户正在编辑的内容**，
  // 否则每 30 秒的静默刷新（`refreshDevices(true)` 那条链）会把输入框里
  // 刚敲到一半的网段冲掉。
  if (!subnetsDirty.value) {
    savedSubnets.value = [...(info.allowed_peer_subnets ?? [])];
    allowedPeerSubnets.value = [...savedSubnets.value];
  }
  // 用户正在编辑时不要覆盖输入框
  if (document.activeElement?.classList?.contains("name-input")) return;
  deviceNameInput.value = info.device_name;
}

async function refreshDevices(silent = false) {
  if (!silent) isRefreshing.value = true;
  try {
    await store.refreshDevices();
  } catch (e) {
    if (!silent) showToast(FeisuoBridge.describeError(e, "获取设备列表失败"), true);
  } finally {
    if (!silent) setTimeout(() => (isRefreshing.value = false), 300);
  }
  // 在线状态与错误横幅一律以**引擎真实状态**为准。
  // 旧写法是"设备列表调用没抛异常就算在线" —— 但引擎没启动时那张表本来
  // 就是空的, 调用会成功返回 [], 于是界面顶着绿灯说"在线", 还顺手把
  // 引擎启动失败的横幅清掉。托盘常驻下这等于完全静默地坏掉。
  await syncEngineStatus();
}

async function syncEngineStatus() {
  try {
    const st = await FeisuoBridge.getEngineStatus();
    engineOnline.value = st.running;
    engineError.value = st.error ?? "";
    // 引擎没起来时必须主动唤出窗口。
    // 这里才是**可靠**的那条路径: 事件 `feisuo://engine-error` 是在 setup()
    // 里 spawn 出去的引擎报错时发出的, 实测比前端挂载监听器早 8ms,
    // 事件必被丢弃。所以不能只依赖事件 —— 状态查询才是兜底,
    // 唤窗也必须放在这里, 否则事件一丢就又变成静默故障。
    if (!st.running) {
      FeisuoBridge.ensureWindowVisible().catch(() => {});
    }
  } catch {
    // 命令本身调不通(例如后端在重启): 保守地标为离线, 但不要覆盖已有错误文案
    engineOnline.value = false;
  }
}

async function loadLocalFiles(subPath?: string) {
  isLoadingLocal.value = true;
  localMessage.value = "";
  try {
    const relPath = subPath ?? localPath.value;
    const append = subPath === undefined && localLoadMorePending;
    const listing = await FeisuoBridge.listDirectoryFiles({
      volume: localVolume.value,
      rel_path: relPath,
      offset: append ? localDiskFiles.value.length : 0,
      limit: 0,
    });
    localLoadMorePending = false;
    // "加载更多"要追加而不是替换 —— 替换会把用户已选中的文件清掉
    if (append) {
      const seen = new Set(localDiskFiles.value.map((f) => `${f.name}/${f.modified}`));
      for (const f of listing.files) {
        if (!seen.has(`${f.name}/${f.modified}`)) localDiskFiles.value.push(f);
      }
    } else {
      localDiskFiles.value = listing.files;
    }
    localPath.value = listing.current_path;
    localParentPath.value = listing.parent_path;
    localTruncated.value = listing.truncated;
    localTotal.value = listing.total;
    // 卷列表每次都按这次应答覆盖，包括空数组。
    //
    // 早先「只在非空时写入」会把上一轮的盘符留在下拉里。收件目录模式
    // 或这台机器当前没有可浏览的盘时，用户仍能点到那个盘，请求却已经
    // 不在卷模式里。右栏已经按「空也要写」处理，左栏必须同一套。
    localVolumes.value = listing.volumes ?? [];
    localMessage.value = localDiskFiles.value.length === 0
      ? "该目录下没有可发送的文件（文件夹要进入后再选里面的文件）。"
      : listing.truncated
        ? `共 ${listing.total} 项，已显示 ${localDiskFiles.value.length} 项`
        : "";
    // 换目录后原有选择不再有意义, 全部清掉（"加载更多"是同一目录, 不能清）
    if (!append) selectedLocalNames.value.clear();
  } catch (e) {
    localLoadMorePending = false;
    localDiskFiles.value = [];
    localMessage.value = FeisuoBridge.describeError(e, "读取本机目录失败");
  } finally {
    isLoadingLocal.value = false;
  }
}

/** 进入本机子文件夹 */
function enterLocalDir(name: string) {
  void loadLocalFiles(localPath.value ? `${localPath.value}/${name}` : name);
}

/** 返回上一级; 已在卷根时忽略 */
function goLocalParent() {
  if (localParentPath.value === null) return;
  void loadLocalFiles(localParentPath.value);
}

function goLocalCrumb(path: string) {
  void loadLocalFiles(path);
}


/** 分页：加载本机目录的下一页 */
function loadMoreLocal() {
  if (!localTruncated.value || isLoadingLocal.value) return;
  localLoadMorePending = true;
  void loadLocalFiles();
}

// ---------------------------------------------------------------------------
// 常用位置：token 与归属判定
// ---------------------------------------------------------------------------
//
// 下拉里现在有三类根，**不能**只用卷 id 表达 —— 常用位置要带上卷内相对
// 路径（桌面是 `C:` + `Users/<u>/Desktop`），所以引入带前缀的 token：
//
//   `""`     收件目录（volume 为空的根）
//   `v:C:`   真实卷
//   `p:desktop` 常用位置
//
// 前缀不是为了好看：`desktop` 与卷 id 撞车的话，`@change` 就分不清
// 该按卷处理还是按位置处理 —— 而这个区分错了就是"点桌面进了 C:\ 根目录"。

const PLACE_PREFIX = "p:";
const VOLUME_PREFIX = "v:";

/**
 * 当前 (volume, relPath) 落在哪个"根"上。
 *
 * ## 为什么要**取最长匹配**
 *
 * 常用位置是**互相包含**的：桌面 = `Users/<u>/Desktop`，而用户目录 =
 * `Users/<u>`。停在 `Users/<u>/Desktop` 时两者都"匹配"。
 * 选最长（最具体）的那个才对 —— 否则按数组顺序取第一个的话，
 * 一旦有人调整了 BUILTIN 顺序，界面就会把"桌面"显示成"用户目录"。
 *
 * ## 前缀比较为什么要带 `/`
 *
 * `Users/<u>/Desktop` 与 `Users/<u>/DesktopBackup` 是两个目录。
 * 直接 `startsWith(p.rel_path)` 会把后者也认成前者 ——
 * 表现为"在 D:\ 里点用户目录，实际停在 Users\<u>\DesktopBackup"。
 */
function rootTokenFor(
  volume: string,
  relPath: string,
  places: KnownPlace[]
): string {
  // 收件目录是独立的一类根：volume 为空，不属于任何常用位置。
  if (!volume) return "";
  const norm = relPath.replace(/\\/g, "/").replace(/^\/+|\/+$/g, "");
  let best: { key: string; len: number } | null = null;
  for (const p of places) {
    if (p.volume !== volume) continue;
    const pr = p.rel_path.replace(/\\/g, "/").replace(/^\/+|\/+$/g, "");
    if (pr !== "" && norm !== pr && !norm.startsWith(pr + "/")) continue;
    const len = pr.length;
    if (!best || len > best.len) best = { key: p.key, len };
  }
  return best ? PLACE_PREFIX + best.key : VOLUME_PREFIX + volume;
}

/** 本栏当前所在的"根"。进到常用位置里面时仍然保持那个位置高亮。 */
const localRootToken = computed(() =>
  rootTokenFor(localVolume.value, localPath.value, localPlaces.value)
);
const remoteRootToken = computed(() =>
  rootTokenFor(remoteVolume.value, remotePath.value, remotePlaces.value)
);

/**
 * 下拉框里选中某一项。
 *
 * 三类 token 分派到"设卷 + 设卷内路径"，与既有的
 * `switchLocalVolume` 走同一套状态，不新增第二条导航通路 ——
 * 两条通路迟早会长出不同的行为（这正是 audit-ui-invariant 盯的那类问题）。
 */
function onRootTokenPick(side: "local" | "remote", token: string) {
  const places = side === "local" ? localPlaces.value : remotePlaces.value;
  const volRef = side === "local" ? localVolume : remoteVolume;
  const pathRef = side === "local" ? localPath : remotePath;
  const parentRef = side === "local" ? localParentPath : remoteParentPath;

  if (token.startsWith(PLACE_PREFIX)) {
    const key = token.slice(PLACE_PREFIX.length);
    const place = places.find((p) => p.key === key);
    if (!place) return; // 列表已经变了，选中项失效
    volRef.value = place.volume;
    pathRef.value = "";
    parentRef.value = null;
    if (side === "local") selectedLocalNames.value.clear();
    void (side === "local" ? loadLocalFiles(place.rel_path) : loadRemoteFiles(place.rel_path));
    return;
  }
  const volume = token.startsWith(VOLUME_PREFIX) ? token.slice(VOLUME_PREFIX.length) : token;
  volRef.value = volume;
  pathRef.value = "";
  parentRef.value = null;
  if (side === "local") selectedLocalNames.value.clear();
  void (side === "local" ? loadLocalFiles("") : loadRemoteFiles(""));
}

const localDropdownOpen = ref(false);
const remoteDropdownOpen = ref(false);

const localCurrentLabel = computed(() => {
  const tok = localRootToken.value;
  if (!tok) return receiveRootLabel.value || "收件目录";
  if (tok.startsWith(PLACE_PREFIX)) {
    const key = tok.slice(PLACE_PREFIX.length);
    return localPlaces.value.find((p) => p.key === key)?.label || key;
  }
  if (tok.startsWith(VOLUME_PREFIX)) {
    const id = tok.slice(VOLUME_PREFIX.length);
    const v = localVolumes.value.find((vol) => vol.id === id);
    return v?.label || id;
  }
  return tok;
});

const remoteCurrentLabel = computed(() => {
  const tok = remoteRootToken.value;
  if (!tok) return "收件目录";
  if (tok.startsWith(PLACE_PREFIX)) {
    const key = tok.slice(PLACE_PREFIX.length);
    return remotePlaces.value.find((p) => p.key === key)?.label || key;
  }
  if (tok.startsWith(VOLUME_PREFIX)) {
    const id = tok.slice(VOLUME_PREFIX.length);
    const v = remoteVolumes.value.find((vol) => vol.id === id);
    return v?.label || id;
  }
  return tok;
});

function handlePickLocalToken(tok: string) {
  localDropdownOpen.value = false;
  onRootTokenPick("local", tok);
}

function handlePickRemoteToken(tok: string) {
  remoteDropdownOpen.value = false;
  onRootTokenPick("remote", tok);
}

/**
 * 本机面包屑。
 *
 * 根的语义随"当前落在哪个位置"变化 —— 在 `C:\Users\<u>\Desktop` 里面时
 * 根就该是「桌面」而不是「C:」，否则面包屑会显示成
 * 「C: > Users > <u> > Desktop」，第一级点下去就是 `C:\Users`，
 * 与"我在桌面里"的语义直接矛盾。
 *
 * 位置内的层级仍然完整展开（Explorer 展开的是真实路径），
 * 这样复制面包屑文本得到的仍是可用的真实路径。
 */
const localCrumbs = computed(() => {
  const rel = localPath.value;
  const segs = rel ? rel.split("/").filter(Boolean) : [];
  const token = localRootToken.value;
  if (token.startsWith(PLACE_PREFIX)) {
    const key = token.slice(PLACE_PREFIX.length);
    const place = localPlaces.value.find((p) => p.key === key);
    if (place) {
      const pr = place.rel_path.replace(/^\/+|\/+$/g, "");
      const below = pr
        ? segs.slice(pr.split("/").filter(Boolean).length)
        : segs;
      const root = [{ label: place.label, path: pr }];
      return [
        ...root,
        ...below.map((s, i) => ({
          label: s,
          path: [pr, ...below.slice(0, i + 1)].filter(Boolean).join("/"),
        })),
      ];
    }
  }
  const rootLabel = localVolume.value
    ? localVolumes.value.find((v) => v.id === localVolume.value)?.label ?? localVolume.value
    : receiveRootLabel.value;
  return [
    { label: rootLabel, path: "" },
    ...segs.map((s, i) => ({ label: s, path: segs.slice(0, i + 1).join("/") })),
  ];
});

/** 本机地址栏的完整路径展示（`D:\项目\2026`） */
const localAddressDisplay = computed(() => {
  if (localVolume.value) {
    const rest = localPath.value ? `\\${localPath.value.replace(/\//g, "\\")}` : "\\";
    return `${localVolume.value}${rest}`;
  }
  return localPath.value
    ? `收件目录\\${localPath.value.replace(/\//g, "\\")}`
    : "收件目录";
});

/**
 * 左栏当前浏览的**绝对基准路径**（不含 rel_path）。
 *
 * 穿梭发送要靠它把选中项还原成绝对路径。卷模式下是 `D:\`，
 * 兼容模式下是收件目录 —— 两者不能混用，见 `shuttleSend` 的注释。
 */
function localBrowseBase(): string {
  if (localVolume.value) {
    return localVolume.value.endsWith("\\") || localVolume.value.endsWith("/")
      ? localVolume.value
      : localVolume.value + "\\";
  }
  return localInfo.receive_dir.replace(/[\\/]+$/, "");
}

/** 本机选中项用"相对落盘根目录的完整路径"作 key, 与右栏保持一致 */
function localEntryKey(item: { name: string }): string {
  return localPath.value ? `${localPath.value}/${item.name}` : item.name;
}

async function loadRemoteFiles(subPath?: string) {
  const dev = store.selectedDevice;
  if (!dev) {
    remoteDiskFiles.value = [];
    remoteMessage.value = "请先在左侧选择一个对端设备。";
    return;
  }
  // 请求发出去之后用户可能已经换了设备。回来时对不上就整份丢掉：
  // 写进去的话，右栏文件、路径、盘符都属于上一台，地址栏却写着这一台的名字。
  const deviceId = dev.device_id;
  isLoadingRemote.value = true;
  remoteMessage.value = "";
  // 要码之后会递归重试。外层不能在重试还没回来时把转圈关掉，
  // 但也不能靠「码还在不在」判断 —— 成功后码会被清掉，取消时码本来就是空的。
  let retrying = false;
  // ---- 自愈：不要静默落到收件目录去 ----
  //
  // 卷下拉里有内容 = 对端**已经声明支持**真实卷浏览。此时若 `remoteVolume`
  // 却是空的，说明 `bootstrapRemoteBrowseMode` 在设备列表还没刷新完时提前
  // return 了（`store.selectedDevice` 此刻还是 undefined）。
  //
  // 不检查就直接把空 volume 发出去，服务端会按"根 = 收件目录"处理，于是：
  //   · 面包屑根从盘符变成收件目录，用户以为功能坏了；
  //   · 真的去点子目录时撞上路径解析的 Windows 前缀问题，
  //     报一个跟"路径逃逸"毫无关系的错。
  // 直接恢复卷模式，比事后解释清楚得多。
  //
  // ⚠️ 这里**不是**"兼容旧版本"：空 volume 的语义是"根 = 本机的收件目录"，
  // 那是 `receive_dir` 配的那个路径，是正常功能。两者的区别只是**根在哪**。
  if (!remoteVolume.value && remoteVolumes.value.length > 0) {
    remoteVolume.value = VOLUME_ANY;
  }
  try {
    // 离线设备没有实时 IP/端口；先提示而不是拿空地址去连（§3.8）
    if (!targetEndpoint.value) {
      remoteDiskFiles.value = [];
      remoteMessage.value = "目标设备当前不可达（无有效地址），请等它上线后再浏览。";
      return;
    }
    if (targetEndpoint.value.offline) {
      remoteMessage.value = "目标设备当前离线，下面是本地缓存的收件目录视图，刷新可能失败。";
    }
    // 面包屑跳转时用显式目标路径; 刷新 / 加载更多时沿用当前状态
    const relPath = subPath ?? remotePath.value;
    const append = subPath === undefined && remoteLoadMorePending;
    const listing = await FeisuoBridge.listRemoteFiles(
      targetEndpoint.value.ip,
      targetEndpoint.value.port,
      {
        volume: remoteVolume.value,
        rel_path: relPath,
        offset: append ? remoteDiskFiles.value.length : 0,
        limit: 0, // 0 = 服务端默认单页条数
        grant_code: remoteGrantCode.value,
      }
    );
    if (store.selectedDeviceId !== deviceId) return;
    remoteLoadMorePending = false;
    // 浏览已通过（说明这次带了码或对端是永久信任）—— 清掉码，
    // 下次再需要时重新弹。
    remoteGrantCode.value = "";
    // "加载更多"要追加而不是替换 —— 替换会把用户已选中的文件清掉
    if (append) {
      const seen = new Set(remoteDiskFiles.value.map((f) => `${f.name}/${f.modified}`));
      for (const f of listing.files) {
        if (!seen.has(`${f.name}/${f.modified}`)) remoteDiskFiles.value.push(f);
      }
    } else {
      remoteDiskFiles.value = listing.files;
    }
    remotePath.value = listing.currentPath;
    remoteParentPath.value = listing.parentPath;
    remoteTruncated.value = listing.truncated;
    remoteTotal.value = listing.total;
    remoteVolumeMode.value = listing.volumeMode;
    // 服务端回显真实卷（我们可能发的是通配 `*`）。不接住它的话，
    // 地址栏会一直显示 `*`，用户无法确认自己在哪个盘上。
    if (listing.volumeMode && listing.volume) {
      remoteVolume.value = listing.volume;
    }
    // 卷列表每次都按这次应答覆盖，包括空数组。
    //
    // 早先「只在非空时写入」是想省一次赋值：服务端在卷模式里每次都会带回
    // 当前可访问的盘，空数组只出现在收件目录模式。但换设备时下拉不会跟着清，
    // 于是上一台的 C/D 盘留在这一台的地址栏里。用户点那个盘，请求发到新设备，
    // 要么打开一个碰巧同名的盘，要么报「未知的卷」。
    // 常用位置已经按「空也要写」处理，卷列表必须同一套。
    remoteVolumes.value = listing.volumes;
    remotePlaces.value = listing.places ?? [];
    // 对端不支持真实卷，但我们已经在卷模式 → 退回 1.x 语义并说清楚。
    // 静默退回会让用户以为"这台设备的 C 盘是空的"。
    //
    // 判据用 `remoteVolumes.length > 0` 而不是 `remoteVolume.value`：
    // 后者在"本来就没设卷"的情况下也是空，于是**最需要解释的那种降级
    // 恰好一句都不说** —— 面包屑悄悄从 "C:\" 变成"落盘目录"，
    // 用户完全不知道发生了什么。
    if (!listing.volumeMode && (remoteVolume.value || remoteVolumes.value.length > 0)) {
      const hadVolumes = remoteVolumes.value.length > 0;
      remoteVolume.value = "";
      remoteMessage.value = hadVolumes
        ? "对端本次没有按真实卷回应，已退回浏览它的收件目录。刷新或重新选择设备可恢复。"
        : "对端版本较旧，只支持浏览其收件目录（不支持真实磁盘浏览）。";
    }
    if (remoteMessage.value === "") {
      remoteMessage.value = remoteDiskFiles.value.length === 0
        ? "该目录暂无文件。"
        : listing.truncated
          ? `共 ${listing.total} 项，已显示 ${remoteDiskFiles.value.length} 项`
          : "";
    }
    // 换目录后原有选择不再有意义, 全部清掉
    // （"加载更多"是同一目录，不能清）
    if (!append) selectedRemoteNames.value.clear();
  } catch (e) {
    if (store.selectedDeviceId !== deviceId) return;
    remoteLoadMorePending = false;
    remoteDiskFiles.value = [];
    const denied = parseBrowseFailure(e, "无法浏览对端目录");
    const msg = denied.message;
    // 「每次匹配码」等级下浏览失败 = 需要出示码（§2.3）。
    // 认的是结构化字段。文案里的「匹配码」只留给还没升级的旧桌面端。
    // 认对端给的信号，不认本机花名册上的信任等级。
    // 花名册可能还是上一次的「永久信任」，对端已经改成每次匹配码。
    // 用本地等级当门槛，用户只会看到一句失败，不知道该去对方窗口抄码。
    if (denied.grantCodeRequired) {
      const entered = await promptGrantCodeFromPeer(dev.device_name, "浏览它的文件", msg);
      // 抄码期间换了设备：码是给上一台的，不能拿去打开这一台的目录。
      if (store.selectedDeviceId !== deviceId) return;
      if (entered) {
        remoteGrantCode.value = entered;
        // 重试自己会管加载状态。这里先返回，finally 里不能把转圈关掉 ——
        // 否则用户在重试还没回来时又能点刷新，两份目录抢着写右栏。
        retrying = true;
        return loadRemoteFiles(subPath);
      }
    }
    remoteMessage.value = msg;
  } finally {
    // 换了设备，或正在用刚抄的码重试：加载状态留给当前那次请求。
    if (store.selectedDeviceId === deviceId && !retrying) {
      isLoadingRemote.value = false;
    }
  }
}

/** 进入对端子文件夹 */
function enterRemoteDir(name: string) {
  const next = remotePath.value ? `${remotePath.value}/${name}` : name;
  void loadRemoteFiles(next);
}

/** 返回上一级; 已在卷根时忽略 */
function goRemoteParent() {
  if (remoteParentPath.value === null) return;
  void loadRemoteFiles(remoteParentPath.value);
}

/** 面包屑: 点根目录或任意一级都能直达 */
function goRemoteCrumb(path: string) {
  void loadRemoteFiles(path);
}


/** 分页：加载下一页（§7.3） */
function loadMoreRemote() {
  if (!remoteTruncated.value || isLoadingRemote.value) return;
  remoteLoadMorePending = true;
  void loadRemoteFiles();
}

/** 面包屑各段 (卷根 + 每级) */
const remoteCrumbs = computed(() => {
  const segs = remotePath.value ? remotePath.value.split("/").filter(Boolean) : [];
  const rootLabel = remoteVolumeMode.value
    ? remoteVolumes.value.find((v) => v.id === remoteVolume.value)?.label ?? remoteVolume.value
    : "落盘目录";
  return [
    { label: rootLabel, path: "" },
    ...segs.map((s, i) => ({ label: s, path: segs.slice(0, i + 1).join("/") })),
  ];
});

/** 当前地址栏的完整路径展示（`C:\项目\2026`） */
const remoteAddressDisplay = computed(() => {
  if (remoteVolumeMode.value) {
    const vol = remoteVolume.value;
    const rest = remotePath.value ? `\\${remotePath.value.replace(/\//g, "\\")}` : "\\";
    return `${vol}${rest}`;
  }
  return remotePath.value ? `收件目录\\${remotePath.value.replace(/\//g, "\\")}` : "收件目录";
});

/** 卷剩余空间的人类可读格式（地址栏下拉用） */
function formatRemoteVolumeSize(bytes: number): string {
  if (!bytes || bytes <= 0) return "";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = bytes;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v >= 100 || i === 0 ? Math.round(v) : v.toFixed(1)} ${units[i]}`;
}

/** 取回 / 穿梭发送都用相对落盘根目录的完整路径作为标识, 而不是只有文件名 */
function remoteEntryKey(item: { name: string }): string {
  return remotePath.value ? `${remotePath.value}/${item.name}` : item.name;
}

/** 浏览路径状态复位 (换设备 / 传输结束) */
function resetRemotePath() {
  remotePath.value = "";
  remoteParentPath.value = null;
  remoteTruncated.value = false;
  remoteTotal.value = 0;
  remoteVolume.value = "";
  remoteVolumeMode.value = false;
  remoteLoadMorePending = false;
  // 卷和常用位置属于**上一台设备**。只清路径不清这两份，
  // 下拉里还能点到上一台的盘，下一次浏览会拿那个盘符去问新设备。
  remoteVolumes.value = [];
  remotePlaces.value = [];
}

async function loadTransferHistory() {
  try {
    transferLogs.value = await FeisuoBridge.getTransferHistory(200);
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "读取传输记录失败"), true);
  }
  // 诊断与历史**分开拉、且不因失败而报错**。
  //
  // 诊断只在这一节里用，它挂了不该把整个「传输记录」页变成红色报错 ——
  // 那是用户最常看的列表。反过来，**折叠条上的条数必须是真的**，
  // 所以即使没展开也要拉；拉失败就把条数置 0 并让小节显示"暂无"。
  try {
    diagnostics.value = await FeisuoBridge.getTransferDiagnostics(50);
  } catch (e) {
    diagnostics.value = [];
    diagnosticsFailed.value = true;
  }
}

function toggleDiagnostics() {
  showDiagnostics.value = !showDiagnostics.value;
}

function diagOutcomeText(outcome: string): string {
  if (outcome === "completed") return "已完成";
  if (outcome === "cancelled") return "已取消";
  return "失败";
}

/** 传输记录的状态。不认识的值不能显示成「已完成」。 */
function historyStatusText(status: string): string {
  if (status === "completed") return "已完成";
  if (status === "failed") return "失败";
  if (status === "cancelled") return "已取消";
  return "未知";
}

async function loadSettingsData() {
  await Promise.all([loadLocalInfo(), loadTransferHistory()]);
}

async function loadTrustedDevices() {
  try {
    trustedDevices.value = await FeisuoBridge.getTrustedDevices();
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "读取受信设备失败"), true);
  }
}

/**
 * 解除配对（§14.5）。
 *
 * ## 为什么必须**如实区分**几种结果
 *
 * 旧实现（`removeTrustedDevice`，纯本地 DELETE）永远只说
 * 「已解除信任，对方需重新配对」。但那只在**对端也解除了**时才成立 ——
 * 而旧实现根本不通知对端，所以这句话从来就不准确：对方仍是永久信任，
 * 还会继续给本机静默投文件，而两边界面都显示「永久信任」。
 * 那条通路已整体删除（bridge / Tauri 命令 / core 方法 / `remove_device`），
 * 现在**只有** `unpairDevice` 一个入口。
 *
 * 现在按 `reason_code` 分支：
 * - `applied` / `already_unpaired` ⇒ 真的双向断开了；
 * - `epoch_mismatch` ⇒ 对方已重新绑定，属正常，不必重试；
 * - `no_shared_epoch` ⇒ 对方版本过旧，**需要升级**才能真正断开；
 * - 其余（离线 / 拒绝）⇒ **本机已断开、对方未断开**，必须说清楚。
 *
 * 按 reason_code 分支而不是 `includes("...")`：文案一改就静默失效，
 * 而这正是本轮在 core 里刚修掉的反模式（`DenyCode` / `requires_grant_code`）。
 */
async function removeTrusted(deviceId: string) {
  try {
    const r = await FeisuoBridge.unpairDevice(deviceId);
    await Promise.all([loadTrustedDevices(), refreshDevices()]);

    switch (r.reason_code) {
      case "applied":
      case "already_unpaired":
        showToast("已解除配对：双方都不再信任，需重新配对才能再传文件");
        return;
      case "epoch_mismatch":
        // 对方已经和我们重新绑定了 —— 这不是失败，别让用户反复重试。
        showToast("本机已解除配对；对方已重新绑定，这条请求对它无效（属正常）");
        return;
      case "no_shared_epoch":
        // 这一条最需要说清楚：光点没用，得让对方升级。
        showToast("本机已解除配对，但对方版本过旧无法同步解除 —— 请升级对方后再解除一次", true);
        return;
      default:
        // 本机一定已经降级了（core 保证），所以**不能**报"失败" ——
        // 那会让用户以为什么都没发生，于是反复点。
        //
        // 也不能一律说「对方不在线」。签名不过、时钟对不上、目标不符
        // 都会落到这里，对方其实在线并且明确拒绝了。有对方的话就用它。
        showToast(
          r.local_applied
            ? r.user_message?.trim() ||
              "本机已解除配对，但未能同步通知对方；对方仍可能给你推送文件"
            : "本机未做任何改动（该设备可能已解除）",
          true,
        );
    }
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "解除配对失败"), true);
  }
}

/**
 * 导出传输诊断报告（§9.7）。
 *
 * 这是"复现问题后把数据交给开发者"的标准通路：报告里每次传输都有
 * 分段耗时（建连/握手/审批/清单/数据流/校验/回执）、链路画像
 * （同子网? 覆盖网? 建连耗时）、吞吐采样与一段人话归因。
 */
async function exportDiagnostics() {
  try {
    const path = await FeisuoBridge.exportTransferDiagnostics(200);
    if (!path) {
      showToast("暂无诊断记录，请先完成至少一次传输", true);
      return;
    }
    showToast("诊断报告已导出：" + path, false);
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "导出诊断报告失败"), true);
  }
}

async function clearHistory() {
  try {
    await FeisuoBridge.clearTransferHistory();
    transferLogs.value = [];
    showToast("已清空传输历史记录");
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "清空记录失败"), true);
  }
}

// =======================================================================
// 传输进度事件
// =======================================================================
function formatBytes(bytes: number): string {
  if (bytes >= 1024 ** 3) return (bytes / 1024 ** 3).toFixed(2) + " GB";
  if (bytes >= 1024 ** 2) return (bytes / 1024 ** 2).toFixed(1) + " MB";
  if (bytes >= 1024) return (bytes / 1024).toFixed(1) + " KB";
  return bytes + " B";
}

function formatSpeed(bytesPerSec: number): string {
  if (bytesPerSec <= 0) return "—";
  return formatBytes(bytesPerSec) + "/s";
}

function clearTransferWatchdog() {
  if (transferWatchdogTimer) {
    clearInterval(transferWatchdogTimer);
    transferWatchdogTimer = null;
  }
}

function resetTransferWatchdog() {
  lastProgressAt = Date.now();
  if (!transferWatchdogTimer) {
    transferWatchdogTimer = setInterval(() => {
      const running = runningTransfers.value;
      if (running.length > 0) {
        const idleSec = (Date.now() - lastProgressAt) / 1000;
        // 超过 45 秒无任何进度事件更新，判定为底层连接卡死，看门狗自动中止
        if (idleSec >= 45) {
          console.warn(`[飞梭] 传输已超过 ${idleSec} 秒无响应，看门狗强制重置所有卡顿任务`);
          for (const t of running) {
            t.settled = "failed";
            t.speedFormatted = "—";
            setTimeout(() => {
              activeTransfers.value.delete(t.transfer_id);
            }, 2500);
          }
          showToast("传输超时无响应，已自动中止", true);
          clearTransferWatchdog();
        }
      } else {
        clearTransferWatchdog();
      }
    }, 3000);
  }
}

function handleTransferProgress(p: TransferProgress) {
  const { kind, reason } = parseTransferStatus(p.status);
  const id = p.transfer_id || (p.peer_device_id ? `dev-${p.peer_device_id}` : "default");

  // 回填 transfer_id：撤销必须带它，而它只能从进度事件里拿到
  if (p.direction === "Send" && p.transfer_id) {
    const u = undoableSend.value;
    if (u && !u.transferId && u.deviceId === p.peer_device_id) {
      u.transferId = p.transfer_id;
    }
  }

  const existing = activeTransfers.value.get(id);
  const settled =
    kind === "Completed" ? "ok" : kind === "Failed" ? "failed" : kind === "Cancelled" ? "cancelled" : null;

  const item: ActiveTransferItem = {
    transfer_id: id,
    current_file: p.current_file || existing?.current_file || (p.total_files > 1 ? `${p.total_files} 个文件` : "文件"),
    direction: p.direction,
    progress_percent: Math.max(0, Math.min(100, Number(p.progress_percent) || 0)),
    speed_bytes_per_sec: p.speed_bytes_per_sec || 0,
    speedFormatted: formatSpeed(p.speed_bytes_per_sec),
    bytes_transferred: p.bytes_transferred || 0,
    total_bytes: p.total_bytes || 0,
    peer_device_id: p.peer_device_id,
    peer_device_name: p.peer_device_name,
    settled,
    last_updated_at: Date.now(),
  };

  activeTransfers.value.set(id, item);

  if (!settled) {
    resetTransferWatchdog();
  } else {
    // 该任务已结单，安排 2.5 秒后从 Map 中清理该任务
    setTimeout(() => {
      activeTransfers.value.delete(id);
      if (runningCount.value === 0) {
        clearTransferWatchdog();
        loadTransferHistory();
        if (p.direction === "Receive") {
          loadLocalFiles();
        }
      }
    }, 2500);

    if (kind === "Completed") {
      FeisuoBridge.ensureWindowVisible().catch(() => {});
      showToast(`${item.current_file} ${p.direction === "Send" ? "发送完成" : "接收完成"}`);
    } else if (kind === "Failed") {
      FeisuoBridge.ensureWindowVisible().catch(() => {});
      showToast(`${item.current_file} 传输失败: ${reason || "未知原因"}`, true);
    } else if (kind === "Cancelled") {
      showToast(`${item.current_file} 传输已取消`, true);
    }
  }
}

/** 主动取消当前进行中的传输（来自底部进度栏或超时应急） */
async function cancelActiveTransfer(transferId?: string) {
  const running = runningTransfers.value;
  if (running.length === 0) return;

  const targets = transferId ? running.filter((t) => t.transfer_id === transferId) : running;
  if (targets.length === 0) return;

  showToast(targets.length > 1 ? `已取消 ${targets.length} 个传输任务` : "已取消传输", true);
  closeUndoWindow();

  for (const t of targets) {
    t.settled = "cancelled";
    t.speedFormatted = "—";
    const devId = t.peer_device_id || undoableSend.value?.deviceId || store.selectedDevice?.device_id || "";
    if (devId) {
      undoneTransferIds.add(devId);
      FeisuoBridge.cancelIncomingTransfer(
        devId,
        t.transfer_id && t.transfer_id !== "aggregate" ? t.transfer_id : undefined
      ).catch((e) => {
        console.warn("通知底层取消传输异常:", e);
      });
    }
    setTimeout(() => {
      activeTransfers.value.delete(t.transfer_id);
    }, 2500);
  }

  if (runningTransfers.value.length === 0) {
    clearTransferWatchdog();
  }
}

function settleTransfersForDevice(deviceId: string, status: "failed" | "cancelled") {
  for (const [id, item] of activeTransfers.value.entries()) {
    if (item.peer_device_id === deviceId && !item.settled) {
      item.settled = status;
      item.speedFormatted = "—";
      setTimeout(() => {
        activeTransfers.value.delete(id);
      }, 2500);
    }
  }
  if (runningCount.value === 0) {
    clearTransferWatchdog();
  }
}

function afterTransferSettled() {
  clearTransferWatchdog();
  loadTransferHistory();
}

// =======================================================================
// 发送
// =======================================================================
/**
 * 待发清单里那一批文件的**落点子路径**（相对对方收件目录）。
 *
 * ## 为什么需要单独记
 *
 * 直发路径把落点当参数一路传下去（`sendFiles(..., destSubPath)`），
 * 但**文件夹必须先确认**（§5.1），而确认的方式是"进待发清单、点发送"。
 * 那一刻调用链已经断了：清单只存了路径，存不下"要落到对方哪一层"。
 *
 * 于是用户拖一个文件夹到对方某个子目录 → 确认 → 文件落到收件根。
 * 落点对用户**不可见地丢了**，而且看不出任何异常 —— 他会在对方收件根
 * 没找到，然后在地址栏那层也没找到。
 *
 * 空串 = 收件根（默认）。**必须跟着清单一起清**，
 * 否则上一批的落点会漏到下一批身上。
 */
const pendingDestSubPath = ref("");

/**
 * 清单被清空 ⇒ 落点失效。**在这里兜住所有清空路径**，而不是逐处赋值。
 *
 * 清单一共有 4 处会被清空（清空按钮 / 直发成功 / 带码重试成功 / 列表发送成功），
 * 漏掉任何一处的后果都一样且很难查：用户清空清单 → 拖一批新文件 →
 * 上一批的落点还挂着 → 文件发到那个他早就忘了的地方。
 *
 * `flush: "sync"`：让"清单为空 ⇒ 落点为空"在**每一瞬间**都成立。
 * 用默认的 `'pre'` 就得推理 tick 边界 —— 而这个不变量的全部价值
 * 就在于它不需要推理。
 */
watch(
  () => stagedFiles.value.length,
  (n) => {
    if (n === 0) pendingDestSubPath.value = "";
  },
  { flush: "sync" }
);

function pushStaged(paths: string[], expanded?: RootExpansion[]) {
  const fresh: string[] = [];
  for (const p of paths) {
    if (!p) continue;
    if (stagedFiles.value.some((f) => f.path === p)) continue;
    fresh.push(p);
  }
  if (fresh.length === 0) {
    showToast("这些文件已在待发清单中", true);
    return;
  }

  // 清单里所有项**共用一个落点**（见 `pendingDestSubPath`）。
  // 往一个**空**清单里加东西 = 换了一批，而调用方未必知道落点
  // （拖进窗口、剪贴板这两条路都是收件根）。这时先把落点清成默认值，
  // 免得沿用上一个已经不在清单里的批次的落点。
  // 往已有清单里追加 = 还是那一批，保留原落点 ——
  // 新旧混在同一个清单里，没有"这次"与"那批"可分。
  // （清单被清空的那一半由上面的 watch 兜住，这里只管"换批"。）
  const wasEmpty = stagedFiles.value.length === 0;

  for (const p of fresh) {
    const name = p.split(/[\\/]/).filter(Boolean).pop() || p;
    // 优先用**预览给出的按根统计**：目录的真实文件数与字节数在这里就有，
    // 不必等 refreshStagedSizes —— 那一趟量的是**目录项自身**的大小，
    // 对文件夹毫无意义（NTFS 恒为 4.0 KB 左右）。
    const exp = expanded?.find((e) => e.path === p);
    stagedFiles.value.push(
      exp
        ? {
            name,
            path: p,
            size: exp.totalBytes,
            sizeFormatted: formatBytes(exp.totalBytes),
            expandedFiles: exp.fileCount,
            isDir: exp.isDir,
            skippedInside: exp.skipped,
          }
        : { name, path: p, size: 0, sizeFormatted: "读取中…" }
    );
  }
  if (wasEmpty) pendingDestSubPath.value = "";
  showToast(`已添加 ${fresh.length} 个待发文件`);
  // 只对"没有预览统计"的那些去补大小 —— 目录的补了也是错的，
  // 覆盖掉真值反而更糟。
  if (fresh.some((p) => !expanded?.some((e) => e.path === p))) {
    void refreshStagedSizes();
  }
}

/** 异步补齐待发清单里的真实文件大小 */
async function refreshStagedSizes() {
  const paths = stagedFiles.value.map((f) => f.path);
  if (paths.length === 0) return;
  try {
    const sizes = await FeisuoBridge.probeFileSizes(paths);
    stagedFiles.value.forEach((f) => {
      // 目录已经有真实的展开统计了，**不能**用目录项自身的大小覆盖。
      // 覆盖后清单会从「7 个文件 17.8 MB」变回「4.0 KB」——
      // 这正是本条修复要消灭的那种不一致，不能在补大小的路上又装回去。
      if (f.isDir) return;
      const size = sizes[f.path];
      if (typeof size === "number") {
        f.size = size;
        f.sizeFormatted = formatBytes(size);
      } else {
        f.sizeFormatted = "不可读";
      }
    });
  } catch {
    stagedFiles.value.forEach((f) => (f.sizeFormatted = "未知大小"));
  }
}

function clearStaged() {
  // 先把"临时暂存"的路径取出来，再清列表 —— 顺序反了就拿不到了。
  //
  // 为什么要单独删: 剪贴板内容落在 **app 私有目录**(clipboard-staging)，
  // 而 `cleanupTempPayloads` 清的是**收件目录**下的旧位置
  // (.feisuo-staging)。两者不是同一个目录，所以只调后者等于什么都没删 ——
  // 用户点了「清空全部」，剪贴板里的明文截图/密码却还留在磁盘上，
  // 只能等 24h TTL 启动清理才消失。
  //
  // 这与发送成功后的清理逻辑(L4998 附近)是同一套：两条路径都调，
  // 且**按路径精确删** —— 全量清空会把用户排队等待发送的内容一起删掉。
  const tempPaths = stagedFiles.value.filter((f) => f.temporary).map((f) => f.path);

  stagedFiles.value = [];
  selectedLocalNames.value.clear();

  FeisuoBridge.cleanupTempPayloads().catch(() => undefined);
  if (tempPaths.length > 0) {
    FeisuoBridge.cleanupClipboardStaging(tempPaths).catch(() => undefined);
  }
}

async function pickFilesToSend() {
  try {
    const paths = await FeisuoBridge.pickFilesForSend();
    if (paths.length === 0) return;
    pushStaged(paths);
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "打开文件选择框失败"), true);
  }
}

// =======================================================================
// 拖到设备卡片 = 直发（§5.1 / §5.2）
// =======================================================================

/** 正在被悬停的设备卡片（用于高亮反馈） */
const dragOverDeviceId = ref("");

/** 直发撤销窗口内的最近一次发送（用于 toast 的「撤销」按钮） */
/**
 * 5 秒撤销窗口的状态。
 *
 * ## 这里刻意**只有**四个字段
 *
 * 早先还有 `totalSize` / `paths` / `tempPaths` 三个，只被赋值、从不被读 ——
 * 而其中两个还**取错了来源**：`totalSize` 与 `tempPaths` 取自
 * `stagedFiles`（待发清单），而 `fileCount` / `paths` 取自本次实际发送的
 * `files`。在"直发"路径出现之前这两者恰好一致（直发只用清单里的文件），
 * 所以看不出问题；一旦文件不是从清单来的（拖进窗口直发、穿梭拖拽），
 * 它们就描述的是**另一批文件**。
 *
 * 留着它们的真实风险不是"占内存"，而是**下一个人会顺着去用**：
 * "撤销时顺便把剪贴板暂存文件清掉"是个很自然的后续需求，而
 * `u.tempPaths` 里装的是清单里**与本次传输无关**的临时文件 ——
 * 照着删就是静默丢数据（用户会以为那个剪贴板内容还在待发清单里）。
 *
 * 死字段 + 取错来源，比字段不存在更危险。所以删掉：以后真需要时再加，
 * 届时类型会逼着人先想清楚"这个值该从哪来"。
 */
interface UndoableSend {
  deviceId: string;
  deviceName: string;
  /** 本次**实际发送**的条目数（不是清单里的总数） */
  fileCount: number;
  /** 传输真正开始后置 true —— 那时已无法撤回（§5.2 的诚实约束） */
  committed: boolean;
  /**
   * 本次传输的 `transfer_id`，撤销时必须带上。
   *
   * 对端按它匹配暂存目录，而它是**内容哈希**、不是设备 id ——
   * 拿设备 id 去撤销永远匹配不上，表现为"撤销成功但文件照收"。
   * 由第一条进度事件回填（`handleTransferProgress`）。
   */
  transferId?: string;
}
const undoableSend = ref<UndoableSend | null>(null);
const undoTimer = ref<any>(null);

/** 该设备能否作为拖放直发目标 */
function canDropTo(dev: DeviceRosterEntry): boolean {
  if (dev.is_self) return false;
  if (dev.trust_level === "pending") return false; // 未配对：先配对（§4.1 发送侧门禁）
  return dev.presence === "online" || dev.presence === "reconnecting";
}

function onDeviceDragOver(e: DragEvent, dev: DeviceRosterEntry) {
  if (isConcurrencyFull.value) return;
  if (!canDropTo(dev)) return;
  dragOverDeviceId.value = dev.device_id;
  const dt = e.dataTransfer;
  if (dt) {
    dt.dropEffect = "copy";
    // Firefox 需要 setData 才会允许 drop
    dt.setData("text/plain", "");
  }
}

function onDeviceDragLeave(dev: DeviceRosterEntry) {
  if (dragOverDeviceId.value === dev.device_id) dragOverDeviceId.value = "";
}

/**
 * 直发前的准入检查。**三个入口共用**（拖到设备卡片 / 拖进窗口 / 穿梭按钮与拖拽）。
 *
 * ## 为什么要抽出来
 *
 * 早先这套检查只写在 `onDeviceDrop`（拖到设备卡片）里。后来新增的
 * "拖进窗口直发"和"穿梭"两条路径**各自复制了一份不完整的**：
 * 只有 `is_self` 和 `isTransferring`，缺 `pending` / `offline`。
 *
 * 后果不是报错，而是**报错发生在不该发生的地方**：拖一个未配对的设备时，
 * 请求照样发出去，由对端在协议层拒绝，用户看到的是
 * 「Security error: 未配对」这种**内部措辞**，
 * 而他真正需要知道的是「这台设备还没配对，点右侧的『配对』」。
 *
 * 边界判定在 UI 这一侧做，是因为"能不能发"这件事用户能自己决定
 * （比如他就是想先看看错误），而协议层拒绝是无条件的。
 */
async function canDirectSendTo(dev: DeviceRosterEntry): Promise<{
  ok: boolean;
  endpoint?: { ip: string; port: number };
}> {
  if (isConcurrencyFull.value) {
    showToast(`当前已有 ${runningCount.value} 个传输任务在进行，已达最大并发数上限 (${maxConcurrency.value})`, true);
    return { ok: false };
  }
  if (dev.is_self) {
    showToast("不能发送到本机", true);
    return { ok: false };
  }
  if (dev.trust_level === "pending") {
    showToast(`「${dev.device_name}」尚未配对，请先完成配对再发送`, true);
    return { ok: false };
  }
  if (dev.presence === "offline") {
    showToast(`「${dev.device_name}」当前离线，请等它上线后再发送`, true);
    return { ok: false };
  }
  // §5.2: 松手时以**实际探测**为准, 不信 UI 的在线态
  const probed = await probeDeviceEndpoint(dev);
  if (!probed) {
    showToast(`「${dev.device_name}」当前无法连接，文件未发送`, true);
    return { ok: false };
  }
  return { ok: true, endpoint: probed };
}

async function onDeviceDrop(e: DragEvent, dev: DeviceRosterEntry) {
  dragOverDeviceId.value = "";
  const dt = e.dataTransfer;

  // ---- 用户拖进来的是什么，就发什么 ----
  //
  // ⚠️ 早先这里是 `stagedFiles.length > 0 ? stagedFiles : 本次拖入的`，
  // 于是**清单非空时会忽略用户这次拖的东西**：想发一个文件、清单里躺着
  // 50 个，就发出那 50 个。界面上完全看不出差别（横幅只写"N 个文件"），
  // 而"发错"的方向恰好是"发得更多"—— 5 秒撤销根本来不及反应。
  //
  // 「拖到设备卡片」没有地址栏概念，所以这里恒为收件根。
  const dropped = await collectDroppedFiles(dt);
  const files = dropped.length > 0
    ? dropped
    : stagedFiles.value.map((f) => f.path);
  if (files.length === 0) {
    showToast("没有可发送的内容", true);
    return;
  }

  // ---- 与「拖进窗口」同一套分派：文件直发，文件夹要确认 ----
  //
  // ⚠️ 这里早先**完全没有目录判定**，直接 `directSendTo`。于是把一个
  // 几万个文件的项目目录拖到设备卡片上会**直接发走**，而同样的内容拖进
  // 窗口则会展开预览等你确认 —— 同一份内容，两个入口两种结果，
  // 而"文件夹要先看一眼"这条规则存在的理由正是防住这一下。
  let preview: Awaited<ReturnType<typeof FeisuoBridge.previewSendPaths>>;
  try {
    preview = await FeisuoBridge.previewSendPaths(files);
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "无法读取拖入的内容"), true);
    return;
  }
  if (preview.fileCount === 0) {
    showToast("拖入的内容里没有可发送的文件", true);
    return;
  }
  if (preview.hasDirectory) {
    // 进清单由用户点「发送」，并**指明发给谁** —— 拖到卡片就是一个明确的
    // 目标设备，清单上的「发送」默认发给当前选中设备，两者不同时要说明，
    // 否则用户点下去发给了另一台机器。
    await previewAndStage(files, preview);
    showToast(
      `已加入待发清单（${preview.fileCount} 个文件）。` +
        `在清单点「发送」即发给 ${dev.device_name}`,
      true
    );
    return;
  }
  // 准入
  const gate = await canDirectSendTo(dev);
  if (!gate.ok || !gate.endpoint) {
    // 不可达时留在清单里：用户稍后可以在清单里重试，
    // 而拖这一个动作不该因为他暂时没配对 / 离线就白费。
    pushStaged(files);
    return;
  }
  await directSendTo(dev, gate.endpoint, files);
}

/** 用 Tauri 原生拖放事件取真实磁盘路径 */
async function collectDroppedFiles(dt: DataTransfer | null): Promise<string[]> {
  if (!dt) return [];
  // Tauri v2 在 web 层拿不到真实路径；原生路径由 onNativeDragDrop 事件补齐。
  // 这里优先用已经由原生事件填好的 lastNativeDropPaths。
  return lastNativeDropPaths.value.slice();
}

/** 主动探测目标端点，返回可用地址或 null */
async function probeDeviceEndpoint(
  dev: DeviceRosterEntry
): Promise<{ ip: string; port: number } | null> {
  const ip = dev.ip || dev.last_ip;
  if (!ip) return null;
  if (dev.presence === "online" && dev.transfer_port) {
    return { ip, port: dev.transfer_port };
  }
  // 重连中 / 离线但有历史 IP：探测一次（1.2s 超时，由后端控制）
  try {
    const probed = await FeisuoBridge.probeDevice(ip);
    if (probed.transfer_port > 0) return { ip, port: probed.transfer_port };
  } catch {
    /* 探测失败 = 不可达 */
  }
  return null;
}

/**
 * 开启 5 秒撤销窗口。
 *
 * 抽出来是因为**两个入口**都要开，而它们早先各写一份 ——
 * 于是"拖拽直发有撤销、穿梭拖拽没有"这种不一致就出现了，
 * 而两个入口在界面上是并列的（同一个穿梭框里，一个点按钮一个拖）。
 * 用户从穿梭框拖错一个文件时完全无法挽回。
 *
 * 撤销本身不需要知道路径：`cancel_incoming_transfer(deviceId, transferId)`
 * 按**对端**匹配暂存目录，而 `transferId` 由第一条进度事件按 deviceId
 * 回填（见 `handleTransferProgress`）。所以这里只记设备与条数。
 */
function openUndoWindow(dev: DeviceRosterEntry, fileCount: number) {
  undoableSend.value = {
    deviceId: dev.device_id,
    deviceName: dev.device_name,
    fileCount,
    committed: false,
    // 撤销必须带**真实的 transfer_id**（对端按它匹配暂存目录）。
    // 先留空，收到第一条进度事件后由 onTransferProgress 回填。
    transferId: undefined,
  };
  if (undoTimer.value) clearTimeout(undoTimer.value);
  undoTimer.value = setTimeout(() => {
    undoableSend.value = null;
    undoTimer.value = null;
  }, 5000);
}

/** 立即关掉撤销窗口（发送失败时用）。 */
function closeUndoWindow() {
  if (undoTimer.value) clearTimeout(undoTimer.value);
  undoTimer.value = null;
  undoableSend.value = null;
}

/** 直发并开 5 秒撤销窗口 */
async function directSendTo(
  dev: DeviceRosterEntry,
  endpoint: { ip: string; port: number },
  files: string[]
) {
  // 立即开启撤销窗口
  openUndoWindow(dev, files.length);

  try {
    const outcome = await FeisuoBridge.sendFiles(
      dev.device_id,
      endpoint.ip,
      endpoint.port,
      dev.device_name,
      files
    );
    // 「每次匹配码」是**协商回合**不是失败：待发队列必须原样保留,
    // 由 promptGrantCode() 弹码后重新调用本函数。
    if (abortedSend(outcome)) return;
    if (outcome.grantCodeRequired) {
      // 协商回合里一个字节都还没传。撤销窗口留着会让用户点一个
      // 此刻对不上任何传输的「撤销」。弹码期间关掉，重试成功再开。
      closeUndoWindow();
      const retried = await promptGrantCodeAndRetry(
        dev, endpoint, files, outcome.message
      );
      if (retried) openUndoWindow(dev, files.length);
      return;
    }
    showSendResult(dev.device_name, files.length, outcome);
    // 发送成功：清空待发清单。
    //
    // ⚠️ 这条直发路径的 `files` 可能**根本不在清单里**（拖进窗口的文件、
    // 穿梭拖拽的文件都不经过 `pushStaged`）。而 `promptGrantCodeAndRetry`
    // 与下面的成功分支都无条件清空清单 —— 于是"顺带"把用户**另外**
    // 排队中、还没发出去的剪贴板内容一起丢掉，用户界面上它凭空消失。
    //
    // 正确做法：只清掉本次真正发出去的那些条目，其余原样保留。
    stagedFiles.value = stagedFiles.value.filter((f) => !files.includes(f.path));
    const u = undoableSend.value;
    if (u) u.committed = true;
  } catch (e) {
    if (undoTimer.value) clearTimeout(undoTimer.value);
    undoTimer.value = null;
    const undone = undoneTransferIds.delete(dev.device_id);
    undoableSend.value = null;
    settleTransfersForDevice(dev.device_id, isLocalAbort(e) ? "cancelled" : "failed");
    if (activeProgress.value && !activeProgress.value.settled) {
      activeProgress.value.settled = isLocalAbort(e) ? "cancelled" : "failed";
      afterTransferSettled();
    }
    // 撤销成功后的失败是**预期结果**，不是错误。
    // 照样弹红色报错的话，用户会以为撤销没生效 —— 于是再点一次撤销，
    // 或者干脆重发（那就把刚撤销的文件又发了一遍）。
    if (undone || isLocalAbort(e)) return;
    showToast(FeisuoBridge.describeError(e, "发送失败"), true);
  }
}

/**
 * 「每次匹配码」协商回合：请用户输入**对方窗口上**的 6 位码，
 * 然后**自动带上码重试**（§2.3）。
 *
 * 码由**接收方**生成并显示在它的审批窗口上，方向的理由见
 * `core/src/transport/server.rs` 的 `ApprovalManager::challenge_for`。
 *
 * ## 用户按取消时**不清空待发队列**
 *
 * 用户可能只是暂时不方便（对端不在旁边）。清空队列等于让他重新拖一次。
 */
async function promptGrantCodeAndRetry(
  dev: DeviceRosterEntry,
  endpoint: { ip: string; port: number },
  files: string[],
  reason: string,
  destSubPath = ""
): Promise<boolean> {
  const entered = await promptGrantCodeFromPeer(dev.device_name, "发送文件", reason);
  if (!entered) {
    showToast(`已保留待发清单，${dev.device_name} 需要每次匹配码`);
    return false;
  }
  const trimmed = entered;
  // 码是对方（接收方）生成、显示在它审批窗口上的，用户把它敲回来。
  // 本地不做任何"看起来对不对"的判断 —— 那是对方该判的事。
  try {
    const outcome = await FeisuoBridge.sendFiles(
      dev.device_id,
      endpoint.ip,
      endpoint.port,
      dev.device_name,
      files,
      trimmed,
      destSubPath
    );
    if (abortedSend(outcome, dev.device_id)) return false;
    if (outcome.grantCodeRequired) {
      // 用户已经敲过一次码了还不行 —— 说明是**对端那边**还有一轮
      // （同一个码、同一个请求指纹，理论上应该能过）。
      //
      // 这里必须说清楚是哪一边的问题。早先只说"请再试一次"，
      // 而用户已经**照做了**一次 —— 于是他以为自己没输对，
      // 开始反复重敲，把一个可能的对端问题当成自己的错。
      showToast(
        `${dev.device_name} 仍未接受这个码。请确认你念的是它审批窗口上` +
          "现在显示的那 6 位（不是上一次那个 —— 换了请求就会换码）",
        true
      );
      return false;
    }
    showSendResult(dev.device_name, files.length, outcome);
    // 只清本次发出去的条目，理由见 `directSendTo` 里的同名注释：
    // 穿梭 / 拖进窗口的直发路径里，待发清单可能装着**另一批**文件
    // （比如用户先前暂存的剪贴板内容），整体清空会把它们一起弄丢。
    const sent = new Set(files);
    const leftover = stagedFiles.value.filter((f) => !sent.has(f.path));
    const sentTemps = stagedFiles.value
      .filter((f) => sent.has(f.path) && f.temporary)
      .map((f) => f.path);
    stagedFiles.value = leftover;
    if (sentTemps.length > 0) {
      FeisuoBridge.cleanupTempPayloads(sentTemps).catch(() => undefined);
      FeisuoBridge.cleanupClipboardStaging(sentTemps).catch(() => undefined);
    }
    return true;
  } catch (e) {
    settleTransfersForDevice(dev.device_id, isLocalAbort(e) ? "cancelled" : "failed");
    if (activeProgress.value && !activeProgress.value.settled) {
      activeProgress.value.settled = isLocalAbort(e) ? "cancelled" : "failed";
      afterTransferSettled();
    }
    showToast(FeisuoBridge.describeError(e, "带码重试失败"), true);
    return false;
  }
}

/**
 * 本机撤销走的是成功返回，不能再落到「已发送」。
 *
 * 待发清单、暂存文件、选中项都留给调用方不动。这里只关撤销窗口并提示。
 */
function abortedSend(outcome: import("./api/feisuoBridge").SendOutcome, deviceId?: string): boolean {
  if (!outcome.locallyAborted) return false;
  closeUndoWindow();
  if (deviceId) {
    settleTransfersForDevice(deviceId, "cancelled");
  }
  if (activeProgress.value && !activeProgress.value.settled) {
    activeProgress.value.settled = "cancelled";
    afterTransferSettled();
  }
  showToast(outcome.message || "已撤销", true);
  return true;
}

/**
 * 发送结果的统一措辞（P1 ⑪ / §7.6）。
 *
 * 三个数必须**分别**说清楚，因为每个都会导致不同的用户误解：
 * - 发出去几个（目录展开后的真实数量，不是"选中了几个"）；
 * - 断点续传跳过几个（对端已存在）；
 * - 展开时跳过几个（符号链接 / 隐藏 / 内部目录）。
 *
 * 混成一句话的后果：用户拖一个含 `.git` 的项目，收到 20 个文件，
 * 界面上说"已发送 1 个文件" —— 他既不知道发了多少，也不知道少了什么。
 */
function showSendResult(
  deviceName: string,
  selectedCount: number,
  outcome: import("./api/feisuoBridge").SendOutcome
) {
  const sent = outcome.expandedFiles > 0 ? outcome.expandedFiles : selectedCount;
  const isFolder = sent !== selectedCount;
  const head = isFolder
    ? `已发送 ${sent} 个文件（来自 ${selectedCount} 个选中项）到 ${deviceName}`
    : `已发送 ${sent} 个文件到 ${deviceName}`;

  const notes: string[] = [];
  if (outcome.skipped > 0) {
    notes.push(`${outcome.skipped} 个对端已存在（断点续传跳过）`);
  }
  if (outcome.expandSymlinksSkipped > 0) {
    notes.push(`${outcome.expandSymlinksSkipped} 个符号链接（不跟随）`);
  }
  if (outcome.expandSkipped > outcome.expandSymlinksSkipped) {
    notes.push(
      `${outcome.expandSkipped - outcome.expandSymlinksSkipped} 个隐藏/内部条目`
    );
  }
  if (outcome.expandLimitHit) notes.push(outcome.expandLimitHit);

  showToast(notes.length > 0 ? `${head}；跳过 ${notes.join("、")}` : head, notes.length > 0);
}

/**
 * 取回对端的**整个文件夹**（右栏目录行的按钮，§7.6）。
 *
 * 复用 `shuttleFetch` 的全部流程（含「每次匹配码」重试与落点询问），
 * 只是把选择集临时换成"这一个目录"。
 *
 * ## 为什么要单独一个入口
 *
 * 点目录默认是**进入**目录，所以目录永远无法被选中 —— 不给这个按钮，
 * "取回整个文件夹"在界面上根本不存在。用户只能一级级进去逐个勾：
 * 目录深一点就是几十次点击，而漏掉一个文件的代价是他根本不知道漏了。
 */
// ===========================================================================
// 穿梭框内拖拽（§7.8）
// ===========================================================================

/** 正在被拖拽的条目。用模块状态而不是 dataTransfer 存**内容**。
 *
 *  为什么两者都要：`dataTransfer.setData` 是 HTML5 拖放的**协议要求**
 *  （Firefox 尤其严格，不 setData 就不允许 drop），但它只在 dragstart
 *  到 drop 之间有效，而且值只能是字符串。真正要传的是"哪一侧 + 哪个条目"，
 *  用字符串编码再解析既脆弱又难读。这里分工：
 *  `dataTransfer` 只负责满足浏览器协议，内容走这个 ref。
 */
const shuttleDrag = ref<{
  side: "local" | "remote";
  names: string[];
  isDir: boolean;
  /** 本次手势开始时刻；用于在 drop 时判断状态是否已过期 */
  startedAt: number;
} | null>(null);

/**
 * 拖拽状态的最长有效期。
 *
 * 只为兜住"拖出窗口后 `dragend` 没派发"这一种情况，所以给得宽松
 * （正常一次窗内拖拽远不到这个量级），但必须有上限 ——
 * `shuttleDrag` 不清理就等于把上一次的名字留在那儿等下一次用。
 * 12 秒：足够慢的拖拽也用不完，又短到用户不会察觉"要重新拖一次"。
 */
const SHUTTLE_DRAG_MAX_AGE_MS = 12_000;

/** 拖拽中的条目集合：在当前选中里就拖整个选中，否则只拖这一个。
 *
 *  这是文件管理器的通行行为（Nautilus / Finder 都是），
 *  也是"用户勾了 5 个文件然后拖其中一个"的直觉答案。
 */
function dragPayloadFor(
  side: "local" | "remote",
  item: { name: string; is_dir: boolean }
): { side: "local" | "remote"; names: string[]; isDir: boolean } {
  const selected = side === "local" ? selectedLocalNames.value : selectedRemoteNames.value;
  const key = side === "local" ? localEntryKey(item) : remoteEntryKey(item);
  const names =
    selected.has(key) && selected.size > 0 ? Array.from(selected) : [key];
  return { side, names, isDir: item.is_dir };
}

function onShuttleRowDragStart(
  e: DragEvent,
  side: "local" | "remote",
  item: { name: string; is_dir: boolean }
) {
  if (isConcurrencyFull.value) {
    showToast(`当前已有 ${runningCount.value} 个传输任务在运行，已达并发上限`, true);
    e.preventDefault();
    return;
  }
  const payload = dragPayloadFor(side, item);
  // 名字**存在 ref 里**，而不是只放进 dataTransfer：到 drop 时还要用，
  // 而那一次读 dataTransfer 只能拿到字符串、还得重新解析分隔符。
  shuttleDrag.value = {
    side,
    names: payload.names,
    isDir: payload.isDir,
    startedAt: Date.now(),
  };
  // 拖多个文件时把名字列出来，取消拖拽时系统会显示"取消拖拽 N 项"
  if (e.dataTransfer) {
    e.dataTransfer.effectAllowed = "move";
    e.dataTransfer.setData(
      "text/plain",
      payload.names.length > 1
        ? payload.names.join("\n")
        : payload.names[0]
    );
  }
}

function onShuttleRowDragEnd() {
  shuttleDrag.value = null;
}

/** 某一栏是否接受当前的拖拽（用于高亮落点） */
function shuttleDropArmed(side: "local" | "remote"): boolean {
  if (!shuttleDrag.value || isConcurrencyFull.value) return false;
  // 只能往**对面**拖：自己拖自己没意义
  return shuttleDrag.value.side !== side;
}

function onShuttlePaneDragOver(e: DragEvent, side: "local" | "remote") {
  if (!shuttleDropArmed(side)) return;
  e.preventDefault();
  if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
}

async function onShuttlePaneDrop(e: DragEvent, side: "local" | "remote") {
  if (!shuttleDropArmed(side)) return;
  e.preventDefault();
  const drag = shuttleDrag.value;
  shuttleDrag.value = null;
  if (!drag) return;

  // 拖拽必须**只跟着本次手势**。
  //
  // 依赖 `dragend` 清 `shuttleDrag` 是不够的：拖到**窗口外**松手时，
  // 某些 WebView 不派发 `dragend`，`shuttleDrag` 就留在上一次的值上。
  // 下一个手势会先把这次的值覆盖掉，看起来没事；但如果用户是在**同一次
  // 拖拽序列**里先出了窗口、又折回某一栏松手，那一次 drop 就会带着
  // **上一次**的名字发出去 —— 发的是用户此刻没选中的文件。
  //
  // 同一手势内 drop 只可能发生一次（这里已把 `shuttleDrag` 置空），
  // 所以"距上次 dragstart 太久"本身就是过期的判据。
  if (Date.now() - drag.startedAt > SHUTTLE_DRAG_MAX_AGE_MS) {
    showToast("这次拖拽已超时，请重新拖一次", true);
    return;
  }

  if (drag.side === "local") {
    // 左 → 右：发送到对方**当前地址栏目录**
    const base = localBrowseBase();
    const sep = base.includes("\\") ? "\\" : "/";
    const paths = drag.names.map((rel) => base + sep + rel.split("/").join(sep));
    await sendPathsIntoRemote(paths);
  } else {
    // 右 → 左：取回到**本机当前地址栏目录**
    const devId = store.selectedDeviceId;
    const saved = new Set(selectedRemoteNames.value);
    selectedRemoteNames.value = new Set(drag.names);
    try {
      await shuttleFetch();
    } finally {
      // 取回期间换了设备：saved 是上一台的选中，不能盖到这一台上。
      if (store.selectedDeviceId !== devId) return;
      if (selectedRemoteNames.value.size > 0) selectedRemoteNames.value = saved;
    }
  }
}

/** 把本机一批路径发送到对方当前目录（穿梭发送的共用实现） */
async function sendPathsIntoRemote(paths: string[]) {
  // ---- 与「拖进窗口」同一套分派：文件直发，文件夹要确认（§5.1）----
  //
  // ⚠️ 早先这里**直接** sendFiles，跳过了目录判定，于是把左栏里的
  // **文件夹**拖到右栏会绕过"文件夹要先看一眼"这条规则 ——
  // 而那条规则正是为了保护用户不误发整个项目目录。
  // 更糟的是它还**根本不报错**：预览展开发生在后端读文件时，
  // 用户看到的是"发送成功 0 个文件"。
  let preview: Awaited<ReturnType<typeof FeisuoBridge.previewSendPaths>>;
  try {
    preview = await FeisuoBridge.previewSendPaths(paths);
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "无法读取要发送的内容"), true);
    return;
  }
  const dest = remoteDestSubPath();
  if (preview.fileCount === 0) {
    showToast("选中的内容里没有可发送的文件", true);
    return;
  }
  if (preview.hasDirectory) {
    // 确认走待发清单那条路。**落点必须一起带过去** ——
    // 早先这里只 pushStaged，于是用户点「发送」时文件落到对方收件根，
    // 而他拖的时候明明停在对方地址栏的某个子目录里。
    await previewAndStage(paths, preview);
    // 放在 pushStaged **之后**：`pushStaged` 在"加入空清单"时会清空落点
    // （那一批换了），先设后清就等于自己把自己抹掉。
    pendingDestSubPath.value = dest.path;
    showToast(
      `落点：${dest.fallback ? "对方收件目录" : "对方/" + (dest.path || "收件目录")}` +
        "，在待发清单点「发送」即落到这里",
      true
    );
    return;
  }

  const dev = store.selectedDevice;
  if (!dev) {
    showToast("请先在左侧选择目标设备", true);
    return;
  }
  // 与「拖到设备卡片」「拖进窗口」共用同一份准入检查。
  // 这里原本只有 `targetEndpoint` 判空，缺 trust_level / presence ——
  // 于是对一台未配对的设备点「发送到对方」，用户看到的是协议层的
  // 「未配对」而不是「点右侧的配对按钮」。
  const gate = await canDirectSendTo(dev);
  if (!gate.ok || !gate.endpoint) return;
  if (dest.fallback) {
    showToast(
      "右栏正在浏览对方的真实磁盘，文件只能落在对方收件目录内。" +
        "本次落到收件根目录（在右栏切到「收件目录」即可直接落到地址栏所在的那一层）",
      true
    );
  }
  // 撤销窗口必须在 `sendFiles` **之前**开。
  //
  // sendFiles 是 await 到**传输结束**才返回的（core 侧
  // send_files_to_dest_with_code 一路 await 到分块跑完）。
  // 早先把 openUndoWindow 放在它之后，于是：传输已完成 -> 窗口才弹出 ->
  // 紧接着 committed = true -> 横幅写着「已开始传输」、撤销按钮**永远不出现**。
  // 那个 5 秒撤销对穿梭这条路等于不存在。
  //
  // 正确顺序：开窗 -> 发 -> 传输中由进度事件回填 transferId -> 成功后置 committed。
  // 与 directSendTo 完全一致。
  openUndoWindow(dev, paths.length);
  try {
    // 用 **gate.endpoint**（刚探测过的那个），不是 `targetEndpoint`。
    // 两者可能不是同一个地址：UI 的端点来自信标与端点表，探测是当场
    // TCP connect 出来的。一台同时有 ZeroTier 与局域网地址的机器上，
    // "探测通了覆盖网、却发往局域网"是真实可能的 —— 于是可达性检查
    // 检的是一个地址、实际发的是另一个，检查就白做了。
    //
    // ⚠️ 落点 `dest.path` 必须在**每一轮**都带上，包括匹配码重试。
    // 带落点的发送与不带落点的发送，接收端 `compute_resume_key` 算出
    // 不同的 `transfer_id` —— 少带一次就等于换了另一笔传输，
    // 撤销窗口里回填的 id 也会对不上。
    const outcome = await FeisuoBridge.sendFiles(
      dev.device_id,
      gate.endpoint.ip,
      gate.endpoint.port,
      dev.device_name,
      paths,
      "",
      dest.path
    );
    if (abortedSend(outcome, dev.device_id)) return;
    if (outcome.grantCodeRequired) {
      closeUndoWindow();
      const retried = await promptGrantCodeAndRetry(
        dev, gate.endpoint, paths, outcome.message, dest.path
      );
      if (retried) {
        openUndoWindow(dev, paths.length);
        selectedLocalNames.value.clear();
      }
      return;
    }
    showSendResult(dev.device_name, paths.length, outcome);
    // 刷新的是发送时那一台的目录。传输期间换了设备，
    // 这个子路径属于上一台，不能拿去打开现在选中的那一台。
    if (!dest.fallback && store.selectedDeviceId === dev.device_id) {
      void loadRemoteFiles(dest.path);
    }
    // 穿梭按钮与拖拽都经由这里，所以左栏选中也在这里清。
    // 放在函数末尾而不是调用方：两个调用方都会漏掉其中一个。
    selectedLocalNames.value.clear();
  } catch (e) {
    // 撤销窗口已经开了：发送失败时必须**关掉**它，否则用户会看到一个
    // "已发起发送 N 个文件" 的横幅，而实际上一个都没发出去，
    // 5 秒后自行消失 —— 界面上在说一件没发生的事。
    const undone = undoneTransferIds.delete(dev.device_id);
    closeUndoWindow();
    settleTransfersForDevice(dev.device_id, isLocalAbort(e) ? "cancelled" : "failed");
    if (activeProgress.value && !activeProgress.value.settled) {
      activeProgress.value.settled = isLocalAbort(e) ? "cancelled" : "failed";
      afterTransferSettled();
    }
    if (!undone && !isLocalAbort(e)) {
      showToast(FeisuoBridge.describeError(e, "穿梭发送失败"), true);
    }
  }
}

async function pullRemoteDir(dirName: string) {
  const key = remotePath.value ? `${remotePath.value}/${dirName}` : dirName;
  const devId = store.selectedDeviceId;
  // 目录**不进** selectedRemoteNames：那里的每一项都会被当作"要取回的文件"，
  // 而展开发生在**对端**。这里只在调用期间借用它。
  const saved = new Set(selectedRemoteNames.value);
  selectedRemoteNames.value = new Set([key]);
  try {
    await shuttleFetch();
  } finally {
    // 取回期间换了设备：saved 是上一台的选中。这时再写回去，
    // 会把这一台刚选中的文件换成上一台的路径。
    if (store.selectedDeviceId !== devId) return;
    // 失败时保留原选中项，用户改一下就能重试
    if (selectedRemoteNames.value.size === 1) {
      selectedRemoteNames.value = saved;
    }
  }
}

// ===========================================================================
// 访问范围配置（需求 ④）
// ===========================================================================

const scopeEditorOpen = ref(false);
const scopeSaving = ref(false);
/** 编辑中的草稿。**不直接改** `scopeForDevice` —— 否则用户点"取消"也改掉了。 */
const scopeDraft = ref<AccessScope | null>(null);
/** 已保存的范围，用于在头部显示"这台设备被收窄过" */
const scopeForDevice = ref<AccessScope>({
  mode: "all",
  allow_volumes: [],
  allow_paths: [],
  deny_paths: [],
  can_pull: true,
  can_push: true,
  updated_at: 0,
});

/** 是否处于"被收窄"状态 —— 头部按钮据此变色提醒 */
const scopeIsRestricted = computed(
  () =>
    scopeForDevice.value.mode !== "all" ||
    !scopeForDevice.value.can_pull ||
    !scopeForDevice.value.can_push
);

const scopeModes: Array<{ value: AccessScope["mode"]; label: string; tip: string }> = [
  { value: "all", label: "全部", tip: "所有内容均可访问，不区分系统目录" },
  { value: "allowlist", label: "白名单", tip: "仅允许访问勾选的盘符或添加的目录" },
  { value: "denylist", label: "排除名单", tip: "允许所有盘符，除了排除的目录" },
  { value: "receive_only", label: "仅收件目录", tip: "仅可访问接收目录，不可浏览其他位置" },
];

const newScopeAllowPath = ref("");
const newScopeDenyPath = ref("");

async function pickFolderForAllow() {
  try {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const res = await open({ directory: true, multiple: false });
    if (typeof res === "string" && res.trim()) {
      addScopeAllowPath(res.trim());
    }
  } catch (e) {
    console.warn("选取目录失败:", e);
  }
}

function addScopeAllowPath(pathToAdd?: string) {
  const p = (pathToAdd ?? newScopeAllowPath.value).trim();
  if (!p || !scopeDraft.value) return;
  if (!scopeDraft.value.allow_paths) scopeDraft.value.allow_paths = [];
  if (!scopeDraft.value.allow_paths.includes(p)) {
    scopeDraft.value.allow_paths.push(p);
  }
  newScopeAllowPath.value = "";
}

function removeScopeAllowPath(index: number) {
  if (scopeDraft.value?.allow_paths) {
    scopeDraft.value.allow_paths.splice(index, 1);
  }
}

async function pickFolderForDeny() {
  try {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const res = await open({ directory: true, multiple: false });
    if (typeof res === "string" && res.trim()) {
      addScopeDenyPath(res.trim());
    }
  } catch (e) {
    console.warn("选取目录失败:", e);
  }
}

function addScopeDenyPath(pathToAdd?: string) {
  const p = (pathToAdd ?? newScopeDenyPath.value).trim();
  if (!p || !scopeDraft.value) return;
  if (!scopeDraft.value.deny_paths) scopeDraft.value.deny_paths = [];
  if (!scopeDraft.value.deny_paths.includes(p)) {
    scopeDraft.value.deny_paths.push(p);
  }
  newScopeDenyPath.value = "";
}

function removeScopeDenyPath(index: number) {
  if (scopeDraft.value?.deny_paths) {
    scopeDraft.value.deny_paths.splice(index, 1);
  }
}

/** 读当前选中设备的范围（打开弹层时 + 切换设备时都会调） */
async function refreshScope(deviceId: string) {
  try {
    const scope = await FeisuoBridge.getAccessScope(deviceId);
    if (store.selectedDeviceId !== deviceId) return;
    scopeForDevice.value = scope;
  } catch {
    // 读失败保持默认
  }
}

async function openScopeEditor() {
  const dev = store.selectedDevice;
  if (!dev) return;
  const deviceId = dev.device_id;
  scopeEditorOpen.value = true;
  scopeDraft.value = null;
  newScopeAllowPath.value = "";
  newScopeDenyPath.value = "";
  await refreshScope(deviceId);
  if (store.selectedDeviceId !== deviceId) {
    scopeEditorOpen.value = false;
    return;
  }
  // 深拷贝，避免草稿与已保存值共享同一个对象
  const d = JSON.parse(JSON.stringify(scopeForDevice.value));
  if (!d.allow_volumes) d.allow_volumes = [];
  if (!d.allow_paths) d.allow_paths = [];
  if (!d.deny_paths) d.deny_paths = [];
  scopeDraft.value = d;
}

function closeScopeEditor() {
  scopeEditorOpen.value = false;
  scopeDraft.value = null;
}

function toggleScopeVolume(id: string) {
  const d = scopeDraft.value;
  if (!d) return;
  if (!d.allow_volumes) d.allow_volumes = [];
  const i = d.allow_volumes.indexOf(id);
  if (i >= 0) d.allow_volumes.splice(i, 1);
  else d.allow_volumes.push(id);
}

function resetScopeToDefault() {
  scopeDraft.value = {
    mode: "all",
    allow_volumes: [],
    allow_paths: [],
    deny_paths: [],
    can_pull: true,
    can_push: true,
    updated_at: 0,
  };
  newScopeAllowPath.value = "";
  newScopeDenyPath.value = "";
}

async function saveScope() {
  const dev = store.selectedDevice;
  const d = scopeDraft.value;
  if (!dev || !d) return;
  const deviceId = dev.device_id;
  const deviceName = dev.device_name;
  // 白名单模式下，若盘符和目录均未指定，则给出提示
  if (
    d.mode === "allowlist" &&
    (!d.allow_volumes || d.allow_volumes.length === 0) &&
    (!d.allow_paths || d.allow_paths.length === 0)
  ) {
    showToast("白名单模式下请至少勾选一个盘符或添加一个目录", true);
    return;
  }
  scopeSaving.value = true;
  try {
    await FeisuoBridge.setAccessScope(deviceId, d);
    if (store.selectedDeviceId === deviceId) {
      scopeForDevice.value = JSON.parse(JSON.stringify(d));
    }
    scopeEditorOpen.value = false;
    scopeDraft.value = null;
    showToast(`已更新 ${deviceName} 的访问范围`);
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "保存失败"), true);
  } finally {
    scopeSaving.value = false;
  }
}

// 切换设备时同步刷新范围
watch(
  () => store.selectedDeviceId,
  (id) => {
    scopeForDevice.value = {
      mode: "all",
      allow_volumes: [],
      allow_paths: [],
      deny_paths: [],
      can_pull: true,
      can_push: true,
      updated_at: 0,
    };
    if (id) void refreshScope(id);
  },
  { immediate: true },
);

/** 撤销直发 */
async function undoDirectSend() {
  const u = undoableSend.value;
  if (!u) return;
  closeUndoWindow();
  if (u.committed) {
    showToast("传输已开始，无法撤回；如需删除请在目标设备的收件目录操作", true);
    return;
  }
  // 即使还没拿到 transfer_id 也必须去问后端。
  //
  // 进度事件要等第一个分块发出去才有。用户在「正在算哈希 / 等对方点允许」
  // 这几秒里点撤销，是这个 5 秒窗口最常见的用法。早先这里直接弹
  // 「已撤销（尚未开始传输）」然后返回 —— 发送任务完全不知道，
  // 文件照样传完。绿横幅 + 文件落地，比没有撤销更糟。
  //
  // 后端在编号还没出来时会先停本机发送，等编号一算出来再补发取消。
  // 通知对端丢弃该暂存传输。
  //
  // ⚠️ 必须**看对端的回答**，不能一律显示"已撤销"。
  // `cancel_incoming_transfer` 返回 false = 对端已开始落盘、撤不掉。
  // 早先这里忽略返回值直接弹绿色提示，于是用户看到"已撤销"，
  // 而文件几秒后照样出现在收件目录里 —— 比不提供撤销更糟，
  // 因为用户会把那个文件从"待办"里划掉。
  try {
    const ok = await FeisuoBridge.cancelIncomingTransfer(
      u.deviceId,
      u.transferId
    );
    if (ok) {
      // 撤销成功后，正在飞行的 sendFiles 会以"对方撤销了本次传输"失败。
      // 那不是新问题，别再弹一个红色报错吓人 —— 标记一下让 catch 识别。
      undoneTransferIds.add(u.deviceId);
      showToast(`已撤销发送给 ${u.deviceName} 的请求`);
    } else {
      showToast(
        `${u.deviceName} 已经开始落盘，无法撤销；` +
          "如需删除请到那台设备的收件目录里处理",
        true
      );
    }
  } catch (e) {
    showToast(
      `撤销请求未送达（${FeisuoBridge.describeError(e, "对方可能已开始接收")}）；` +
        "若对方已收到文件，请在目标设备收件目录删除",
      true
    );
  }
}

/**
 * 本次会话内已被成功撤销的传输，按设备记。
 *
 * 用途只有一个：让 `directSendTo` 的 catch 分支认出
 * "这个失败是我自己刚点的撤销造成的"，从而**不再弹红色报错**。
 *
 * 为什么需要它：撤销成功后对端会中断传输，发送方必然收到失败。
 * 而用户刚刚亲手点的撤销 —— 再告诉他"发送失败"只会让人以为撤销没生效，
 * 于是又点一次撤销，或者干脆重发（那就把刚撤销的文件再发一遍）。
 */
const undoneTransferIds = new Set<string>();

/** 后端把「本机撤销」当成了错误抛出来时，不要再弹红色失败。 */
function isLocalAbort(e: unknown): boolean {
  const msg = FeisuoBridge.describeError(e, "");
  return msg.includes("已撤销") || msg.includes("已在本机撤销");
}

function handleNativeDrop(paths: string[]) {
  isDragOver.value = false;
  if (paths.length === 0) return;
  // 必须先把窗口唤出来: 关窗后窗口处于隐藏状态, 而 Tauri 的原生拖放在
  // 隐藏时照样触发 —— 不唤出的话文件确实进了发送清单, 但用户完全看不到,
  // 表现为"把文件拖过去, 什么都没发生"。托盘常驻是本产品的核心形态,
  // 所以这条路径必须自己保证可见性。
  FeisuoBridge.ensureWindowVisible().catch(() => {});
  if (isConcurrencyFull.value) {
    showToast(`当前已有 ${runningCount.value} 个传输任务在运行，请等部分完成后再添加`, true);
    return;
  }
  // 跳过隐藏的**顶层**条目。目录内部的过滤交给后端递归展开
  // （§7.6), 它有真实的文件系统视图 —— 前端靠"有没有扩展名"猜
  // 会把 "我的照片.2024" 这类目录误判成文件。
  const files = paths.filter((p) => {
    const name = p.split(/[\\/]/).filter(Boolean).pop() || "";
    return name.length > 0 && !name.startsWith(".");
  });
  if (files.length === 0) {
    showToast("拖入的内容不是文件", true);
    return;
  }
  // 缓存真实路径, 供"拖到设备卡片直发"取用（网页层 DataTransfer 拿不到磁盘路径）
  lastNativeDropPaths.value = files;
  void routeDroppedPaths(files);
}

/**
 * 拖入窗口后的分派：**文件直发，文件夹才要确认**（§5.1）
 *
 * ## 为什么要分派，而不是一律进待发清单
 *
 * 产品要求是"拖拽直接发出"，而早先的实现**无条件**走
 * `previewAndStage` → 进待发清单 → 还要再点一次「发送」。
 * 于是同一个拖拽动作有三种结果：拖到设备卡片会直发、拖进窗口要再点一次、
 * 拖到发送页的大框又要再点一次。**行为不一致本身就是 bug**，
 * 哪怕每一种单看都"说得通"。
 *
 * ## 判据是"有没有目录"，不是"有几个文件"
 *
 * 见 `SendPreviewDto.has_directory` 的注释：靠"拖入数 vs 展开后文件数"
 * 推断会把**只含 1 个文件的文件夹**误判成普通文件。而"文件夹要先看一眼"
 * 正是要保护用户的地方（拖一个项目目录可能展开成几千个文件）。
 *
 * ## 不能直发的三种情况，以及为什么要单独说清楚
 *
 * 用户抱怨的是"凭什么还要确认"，所以**每一次**要停下来都必须有理由，
 * 而且理由要写进提示里 —— 不能只是静默地多了一步。
 */
async function routeDroppedPaths(paths: string[]) {
  let preview: Awaited<ReturnType<typeof FeisuoBridge.previewSendPaths>>;
  try {
    preview = await FeisuoBridge.previewSendPaths(paths);
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "无法读取拖入的内容"), true);
    return;
  }
  if (preview.fileCount === 0) {
    showToast(
      "拖入的内容里没有可发送的文件" +
        (preview.skipped > 0 ? `（跳过了 ${preview.skipped} 项）` : ""),
      true
    );
    return;
  }

  // ---- 有目录：展开预览后入待发清单（要确认的那一类）----
  if (preview.hasDirectory) {
    await previewAndStage(paths, preview);
    return;
  }

  // ---- 纯文件：满足条件就直发 ----
  const dev = store.selectedDevice;
  if (!dev) {
    // 没有目标就没有"直发"可言，进清单等用户选设备。
    pushStaged(paths);
    showToast(`已加入待发清单（${preview.fileCount} 个文件）：请先在左侧选择目标设备`);
    return;
  }
  // 准入检查与「拖到设备卡片」**共用同一个函数**（`canDirectSendTo`）。
  // 不可达时把文件留在清单里 —— 用户稍后可以在清单里重试，
  // 而拖进窗口这个动作不该因为他暂时没配对 / 对方离线就白费。
  const gate = await canDirectSendTo(dev);
  if (!gate.ok || !gate.endpoint) {
    pushStaged(paths);
    return;
  }
  await directSendTo(dev, gate.endpoint, paths);
}

/** 递归展开预览 → 入待发清单（§7.6）
 *
 *  `preview` 由调用方传入（`routeDroppedPaths` 已经算过一次）。
 *  不在这里重算：展开一个几千文件的目录要扫磁盘，重算一次是白等。
 */
async function previewAndStage(
  paths: string[],
  preview: Awaited<ReturnType<typeof FeisuoBridge.previewSendPaths>>
) {
  // 把跳过的项**说出来**。这是"用户拖了 3000 个文件只收到 20 个"的
  // 唯一解释来源 —— 静默跳过会让人以为是传输丢数据。
  const parts: string[] = [];
  if (preview.symlinksSkipped > 0) {
    parts.push(`${preview.symlinksSkipped} 个符号链接（不跟随，避免传出去指向别处）`);
  }
  if (preview.skipped > preview.symlinksSkipped) {
    parts.push(`${preview.skipped - preview.symlinksSkipped} 个隐藏/内部条目`);
  }
  if (preview.limitHit) parts.push(preview.limitHit);
  const skippedNote = parts.length > 0 ? `；跳过 ${parts.join("、")}` : "";
  // 把按根统计一起传进去：清单要显示真实的文件数与字节数，
  // 而不是目录项自己的 4.0 KB。
  pushStaged(paths, preview.expanded);
  // 确认这一类**必须说清为什么**：用户抱怨的就是"凭什么还要确认"。
  // 答案要出现在提示里 —— "因为里面有目录，展开成了 N 个文件"，
  // 而不是让他猜。
  // ⚠️ 第二个参数是"是否报错"，这里**不能**传 true。
  // 早先传了 true，于是「已展开为 7 个文件」这种纯说明被渲染成红色错误条 ——
  // 用户刚拖完一个正常文件夹就被告知出错了，而实际上什么都没坏。
  // 红色的含义必须是"有事发生了"，而这件事恰恰是预期内的。
  showToast(
    `拖入的内容含目录，已展开为 ${preview.fileCount} 个文件` +
      `（${formatBytes(preview.totalBytes)}）${skippedNote}。` +
      `确认后在待发清单点「发送」`
  );
}

/**
 * 抓取剪贴板（§6）。
 *
 * 走**原生剪贴板 API** 而不是 `navigator.clipboard`：后者在 WebView2 下
 * 需要用户手势 + 窗口焦点 + 显式授权，托盘常驻时经常直接返回空，
 * 而且**完全拿不到 `CF_HDROP`** —— "在资源管理器里复制几个文件直接发"
 * 是这里价值最高的一条（§6.3）。
 *
 * 拆成"先预览（不落盘）→ 用户确认 → 再落盘"两步：
 * "看了没发"同样会在磁盘上留下明文，而剪贴板里常混着密码与验证码。
 */
async function handleSendClipboard() {
  try {
    const content = await FeisuoBridge.readClipboardPreview();
    clipboardPreview.value = content;
    switch (content.kind) {
      case "empty":
        showToast("剪贴板里没有可发送的文件、图片或文本", true);
        clipboardPreview.value = null;
        return;
      case "rejected":
        // SVG / 整份 HTML: 明确拒绝并说清原因，不静默丢弃
        showToast(content.reason, true);
        clipboardPreview.value = null;
        return;
      case "files":
        // 早先这里写"将直接发送原文件"，而实际执行的是 `pushStaged` ——
        // 用户看到"直接发送"，点了「发送文件」按钮发现还要再点一次发送，
        // 只能理解为界面坏了。文案与行为不符是比缺文案更糟的。
        //
        // 而且**这里本来就不该直发**：剪贴板里的东西用户自己不一定知道
        // 是什么（从密码管理器、聊天窗口复制的都可能是敏感内容），
        // 所以"剪贴板内容要过一道确认"是有理由的，与"拖进来的文件直发"
        // 并不矛盾。
        pushStaged(content.paths);
        clipboardPreview.value = null;
        showToast(
          `剪贴板里有 ${content.paths.length} 个文件，已加入待发清单。` +
            "剪贴板内容需要你确认后再发，避免复制到的东西被误传"
        );
        return;
      case "image":
      case "text":
        // 默认弹预览（D5），用户点「加入待发」才落盘
        showHiddenDrawer.value = false;
        return;
    }
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "读取剪贴板失败"), true);
  }
}

/** 用户在预览浮层里确认 → 落盘并加入待发清单 */
async function confirmClipboardPreview() {
  const content = clipboardPreview.value;
  if (!content || (content.kind !== "image" && content.kind !== "text")) return;
  try {
    const paths = await FeisuoBridge.stageClipboardPayload();
    if (paths.length === 0) {
      showToast("暂存剪贴板内容失败", true);
      return;
    }
    const name =
      content.kind === "image"
        ? content.file_name
        : content.file_name;
    stagedFiles.value.push({
      name,
      path: paths[0],
      size: content.size,
      sizeFormatted: formatBytes(content.size),
      temporary: true,
    });
    clipboardPreview.value = null;
    showToast(`已加入待发：${name}`);
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "暂存剪贴板内容失败"), true);
  }
}

async function startSendTransfer() {
  const dev = store.selectedDevice;
  if (!dev) {
    showToast("请先选择目标设备", true);
    return;
  }
  if (stagedFiles.value.length === 0) {
    showToast("请先添加要发送的文件", true);
    return;
  }
  // 与其余三条发送路径**共用**准入检查（信任等级 / 离线 / 探测）。
  // 这是最常用的发送入口，早先只判了 `targetEndpoint` 非空 ——
  // 于是对一台未配对的设备点发送，用户要等到协议层拒绝才知道。
  const gate = await canDirectSendTo(dev);
  if (!gate.ok || !gate.endpoint) {
    showToast("文件已保留在待发清单，条件满足后可再发送", true);
    return;
  }

  // 记下本次实际提交的临时项, 只在发送成功后精确删除它们。
  // 全量删除暂存目录会把"没参与本次发送、只是排队中"的剪贴板文件一起删掉。
  const tempPaths = stagedFiles.value.filter((f) => f.temporary).map((f) => f.path);
  const files = stagedFiles.value.map((f) => f.path);
  // 落点跟着清单一起走（`pendingDestSubPath`）。
  // 走「穿梭里拖文件夹」这条路时它是对方地址栏当前那一层；
  // 走剪贴板 / 拖进窗口那条路时是空串 = 对方收件根。
  const destSub = pendingDestSubPath.value;
    // 待发清单的「发送」按钮也开撤销窗口。
    //
    // 三个发送入口里只有这一个没有：拖进窗口有、穿梭拖拽有（上一轮补上），
    // 而这个是**用户主动点了「发送」之后**才发出的 —— 恰恰是最该给
    // 反悔机会的一步。用户填错了落点、传错了设备，点下去的一瞬间
    // 就想撤回，而界面上没有这个选项。
    //
    // 拖放区那句「文件松手即发，5 秒内可撤销」也是这么写的：
    // 规则对**所有**直发路径成立，不只是松手那一种。
    openUndoWindow(dev, files.length);
  try {
    const outcome = await FeisuoBridge.sendFiles(
      dev.device_id,
      gate.endpoint.ip,
      gate.endpoint.port,
      dev.device_name,
      files,
      "",
      destSub
    );
    if (abortedSend(outcome, dev.device_id)) return;
    if (outcome.grantCodeRequired) {
      // 「每次匹配码」是**协商回合**，不是发送失败。
      //
      // 此时**必须先关掉撤销窗口**再弹码：协商回合里一个字节都还没传，
      // 横幅上却写着「已发起发送 · N 个文件」并给一个「撤销」按钮。
      // 用户点它只会得到"对方已经开始落盘，无法撤销"（transfer_id 还没回填），
      // 而真相是"什么都没发"。谎报比不给撤销更糟。
      //
      // 弹码期间也不该有撤销窗口：那时候用户要做的决定是"念码还是取消"，
      // 不是"撤回传输"。给他两个都在变化的按钮只会让人犹豫。
      closeUndoWindow();
      // 保留队列与暂存文件: 用户可能只是暂时不方便
      const retried = await promptGrantCodeAndRetry(
        dev, gate.endpoint, files, outcome.message, destSub
      );
      if (retried) {
        // 重试成功：这时才真的发出去了，重开一次撤销窗口。
        // 暂存文件由 `promptGrantCodeAndRetry` 按本次发出去的路径清掉。
        openUndoWindow(dev, files.length);
      }
      return;
    }
    showSendResult(dev.device_name, files.length, outcome);
    // 发送成功: 清空暂存队列
    stagedFiles.value = [];
    // 撤销窗口转为"已开始传输"：进度条已经在跑，撤不掉了，
    // 横幅必须如实说，而不是继续给一个点不动的「撤销」按钮。
    const u = undoableSend.value;
    if (u) u.committed = true;
    if (tempPaths.length > 0) {
      // 两条清理路径都调：旧的在收件目录（.feisuo-staging），
      // 新的在 app 私有目录（clipboard-staging）。都必须**按路径精确删**,
      // 全量清空会把用户排队等待发送的剪贴板内容一起删掉（静默丢数据）。
      FeisuoBridge.cleanupTempPayloads(tempPaths).catch(() => undefined);
      FeisuoBridge.cleanupClipboardStaging(tempPaths).catch(() => undefined);
    }
  } catch (e) {
    // 撤销窗口已经开了：发送失败时必须**关掉**它，否则横幅上写着
    // 「已发起发送 · N 个文件」，而一个字节都没发出去。
    const undone = undoneTransferIds.delete(dev.device_id);
    closeUndoWindow();
    settleTransfersForDevice(dev.device_id, isLocalAbort(e) ? "cancelled" : "failed");
    if (activeProgress.value && !activeProgress.value.settled) {
      activeProgress.value.settled = isLocalAbort(e) ? "cancelled" : "failed";
      afterTransferSettled();
    }
    // 失败时保留队列与暂存文件, 便于用户直接重试
    if (!undone && !isLocalAbort(e)) showToast(FeisuoBridge.describeError(e, "发送失败"), true);
  } finally {
    loadTransferHistory();
  }
}

// =======================================================================
// 双栏穿梭
// =======================================================================
function toggleLocalSelection(item: DiskFileInfo) {
  // 文件夹现在**可以**整夹发送（§7.6）—— 后端 `expand_all` 会递归展开，
  // 目标路径带上文件夹名，所以对端收到的结构与本机一致。
  // 右栏（对端）仍不支持：那是"取回"，而取回一个目录需要在对端
  // 指定落点语义，与 `dest_sub_path` 的设计冲突，暂不做。
  const key = localEntryKey(item);
  if (selectedLocalNames.value.has(key)) selectedLocalNames.value.delete(key);
  else selectedLocalNames.value.add(key);
}

function toggleRemoteSelection(item: RemoteFileEntry) {
  if (item.is_dir) {
    showToast("取回暂不支持整个文件夹；请进入文件夹后逐个选择", true);
    return;
  }
  const key = remoteEntryKey(item);
  if (selectedRemoteNames.value.has(key)) selectedRemoteNames.value.delete(key);
  else selectedRemoteNames.value.add(key);
}

/**
 * 对方收件目录之下的落点子目录（穿梭右栏 → 发送方向，§7.7）。
 *
 * 规则只有一条：**对方地址栏在收件目录镜像里时，才把它作为落点。**
 * 对方在浏览真实卷（C:/D:）时返回空串（落收件根），因为那属于
 * "写到收件目录之外"，是另一个决策（§2.4），不由这里顺带打开。
 */
function remoteDestSubPath(): { path: string; fallback: boolean } {
  if (remoteVolumeMode.value) return { path: "", fallback: true };
  return { path: remotePath.value, fallback: false };
}

/** 「发送到对方」的落点，人类可读，写进按钮提示。
 *
 *  存在的理由：落点规则改成了"跟着对方地址栏走"，而**提示不跟着改**
 *  就等于这个行为对用户不可见 —— 他不会知道自己该去对方机器的哪一层
 *  找文件，于是要么找不到、要么以为没发成功。
 */
const sendDestLabel = computed(() => {
  const d = remoteDestSubPath();
  if (d.fallback) return "的收件根目录（对方地址栏在真实磁盘上，无法指定更深的落点）";
  return d.path ? "的 " + d.path + "/" : "的收件根目录";
});

/** 「取回到本机」的落点，人类可读。 */
const fetchDestLabel = computed(() => {
  if (localVolumeMode.value) return "的收件根目录（本机地址栏在真实磁盘上，无法指定更深的落点）";
  return localPath.value ? "的 " + localPath.value + "/" : "的收件根目录";
});

async function shuttleSend() {
  if (selectedLocalNames.value.size === 0) return;

  // 选中项是"相对**当前浏览基准**的路径"(可含子目录), 必须拼回绝对路径。
  //
  // ⚠️ 基准随左栏卷下拉变化: 浏览 D 盘时基准是 `D:\`, 不是收件目录。
  // 旧实现硬编码 `receive_dir + rel` —— 在真实卷模式下会把
  // `D:\项目\2026\1月.csv` 拼成 `C:\Users\...\feisuo\D:\项目\...`,
  // 一个根本不存在的路径。表现为"点了发送没反应"。
  const base = localBrowseBase();
  const sep = base.includes("\\") ? "\\" : "/";
  const paths = Array.from(selectedLocalNames.value).map(
    (rel) => base + sep + rel.split("/").join(sep)
  );
  // 委托给共用实现，而不是**再抄一遍**。
  //
  // 早先这里是独立的一份完整实现，于是它和"拖进窗口直发"那条路径
  // 各有一份**不完整**的准入检查（缺 trust_level / presence），
  // 也各有一份"用 targetEndpoint 而非探测端点"的地址。
  // 重复是这类漂移的根源，所以现在只保留一份。
  await sendPathsIntoRemote(paths);
}

/**
 * 取回成功时优先用对端原文。
 *
 * 原文里有实际要推的文件数。写死「已受理」会让用户分不出取回了几个文件。
 * 原文为空（旧对端）才退回「已受理」，并带上本机落点。
 */
function pullAcceptedText(deviceName: string, destSub: string, peerMessage: string): string {
  const peer = peerMessage.trim();
  const where = destSub
    ? `将取回到 ${localInfo.receive_dir}\\${destSub}`
    : `将取回到 ${localInfo.receive_dir}`;
  if (!peer) return `${deviceName} 已受理，${where}`;
  return `${deviceName}：${peer}。${where}`;
}

async function shuttleFetch() {
  const dev = store.selectedDevice;
  if (!dev || selectedRemoteNames.value.size === 0) return;
  // 探测和等对方确认都要时间。回来时可能已经换成另一台，
  // 不能把这一台新选中的文件清掉，也不能把左栏刷成无关的一次刷新。
  const deviceId = dev.device_id;
  // 准入与发送方向**共用** `canDirectSendTo`。
  //
  // 方向不同但准入条件相同：能不能向这台设备**发起一次操作**，取决于
  // 信任等级与可达性，与这次是"发过去"还是"要回来"无关。
  // 分成两份写就必然漂移 —— 早先取回方向就缺了 `pending` 判断，
  // 于是对一台未配对的设备点「取回」，用户看到的是协议层的「未配对」。
  const gate = await canDirectSendTo(dev);
  if (!gate.ok || !gate.endpoint) return;

  const files = Array.from(selectedRemoteNames.value);
  // 落点 = **左栏地址栏当前所在的目录**。
  //
  // 早先这里弹一个 `window.prompt` 要用户手打子目录，那是本末倒置：
  // 左栏本来就是一个可以导航的文件系统，用户想去哪一层直接导航过去就行，
  // 还要再手打一遍路径，既多一步又打错（而打错的后果是文件落在别处）。
  //
  // 唯一的例外：左栏在浏览**真实卷**（C:/D:）时，"当前目录"不在收件目录之内，
  // 而取回的落点**必须**在收件目录之内（写入权限的语义：允许写，但落点
  // 强制在收件目录，见 §2.4）。这时退回收件根并**说清楚为什么** ——
  // 静默落到别处，用户会在别的地方找不到文件。
  let destSub = "";
  if (localVolumeMode.value) {
    showToast(
      "左栏正在浏览本机真实磁盘，取回的文件只能落在收件目录内。" +
        `本次落到 ${localInfo.receive_dir}` +
        "（在左栏切到「收件目录」即可直接落到地址栏所在的那一层）",
      true
    );
  } else {
    destSub = localPath.value;
  }
  try {
    // 必须带上**当前浏览的卷**。不传的话对端会按 1.x 语义去收件目录里
    // 找 `项目/2026/a.csv` —— 于是"右栏能浏览整个真实卷，点取回却说
    // 文件不存在"。volume 空串只在本机收件目录镜像下才是对的。
    const curVolume = remoteVolumeMode.value ? remoteVolume.value : "";
    showToast(`正在从 ${dev.device_name} 取回 ${files.length} 项…`);
    // 用**探测过的端点**，与发送方向同一原则：可达性检查检的是哪个地址，
    // 就往哪个地址发。早先这里用 `targetEndpoint`，于是变成
    // "检查覆盖网、发往局域网"，可达性检查形同虚设。
    const outcome = await FeisuoBridge.requestPull(
      gate.endpoint.ip,
      gate.endpoint.port,
      files,
      destSub,
      "",
      curVolume
    );
    if (outcome.grantCodeRequired) {
      // 协商回合: 不是失败, 待取清单必须保留
      const entered = await promptGrantCodeFromPeer(dev.device_name, "取回文件", "");
      if (store.selectedDeviceId !== deviceId) return;
      if (!entered) {
        showToast("已保留选中项, 随时可以再试");
        return;
      }
      const retry = await FeisuoBridge.requestPull(
        gate.endpoint.ip,
        gate.endpoint.port,
        files,
        destSub,
        entered,
        curVolume
      );
      if (store.selectedDeviceId !== deviceId) return;
      if (retry.grantCodeRequired) {
        showToast("对方还需要核对匹配码, 请重试一次", true);
        return;
      }
      showToast(pullAcceptedText(dev.device_name, destSub, retry.message));
      selectedRemoteNames.value.clear();
      // 刷新左栏：与发送方向对称 —— "落到地址栏当前目录"这件事
      // 必须**看得见**，否则用户会在别处找文件，或以为没生效。
      if (destSub) void loadLocalFiles(destSub);
      return;
    }
    if (store.selectedDeviceId !== deviceId) return;
    showToast(pullAcceptedText(dev.device_name, destSub, outcome.message));
    selectedRemoteNames.value.clear();
    if (destSub) void loadLocalFiles(destSub);
  } catch (e) {
    if (activeProgress.value && !activeProgress.value.settled) {
      activeProgress.value.settled = "failed";
      afterTransferSettled();
    }
    if (store.selectedDeviceId !== deviceId) return;
    showToast(FeisuoBridge.describeError(e, "取回请求失败"), true);
  }
}

// =======================================================================
// 设置
// =======================================================================
/** 提交设备名称; 空值回退为后端返回的名字, 而不是静默保存一个空名字 */
async function commitDeviceName() {
  const next = deviceNameInput.value.trim();
  if (!next) {
    deviceNameInput.value = localInfo.device_name;
    showToast("设备名称不能为空", true);
    return;
  }
  if (next === localInfo.device_name) return;

  // 先记下已经生效的名字。保存失败时输入框必须回到它，
  // 不能先把 localInfo 改成新名字再拿它回滚 —— 那样回滚的就是被拒绝的名字。
  const previous = localInfo.device_name;
  try {
    await FeisuoBridge.updateAppConfig({ device_name: next });
    // 后端会去掉控制字符并截到 32 字。以回读到的为准，
    // 否则输入框显示的是完整原文，局域网里看到的是截断后的名字。
    await loadLocalInfo();
    nameSaved.value = true;
    if (nameSavedTimer) clearTimeout(nameSavedTimer);
    nameSavedTimer = setTimeout(() => (nameSaved.value = false), 2200);
  } catch (e) {
    localInfo.device_name = previous;
    deviceNameInput.value = previous;
    showToast(FeisuoBridge.describeError(e, "保存设备名称失败"), true);
  }
}

async function saveSettings() {
  try {
    await FeisuoBridge.updateAppConfig({
      auto_receive: localInfo.auto_receive,
      log_level: localInfo.log_level,
      max_history_records: localInfo.max_history_records,
      record_retention_days: localInfo.record_retention_days,
      close_action: localInfo.close_action,
      theme: store.theme,
    });
    showToast("设置已保存");
    await loadLocalInfo();
  } catch (e) {
    // 开关在点下去时已经改了界面上的值。保存失败时这份值既没进内存也没进磁盘，
    // 不读回来的话，界面显示已关闭，实际仍是旧设置。关窗、自动接收都会按旧的来。
    try {
      await loadLocalInfo();
    } catch {
      /* 读不回来就留着这次的选择，至少错误提示还在 */
    }
    showToast(FeisuoBridge.describeError(e, "保存设置失败"), true);
  }
}

async function onThemeSelect() {
  // 下拉框的 setter 已经把新主题画上了。这里只负责写进配置。
  // 保存失败时 loadLocalInfo 会把 localInfo.theme 读回正在生效的值，
  // 但画面和下拉框还停在没写上的那一档，下次打开才会跳回去。
  await saveSettings();
  const saved = localInfo.theme === "light" ? "light" : "dark";
  if (store.theme !== saved) store.applyTheme(saved);
}

async function chooseReceiveDir() {
  try {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({ directory: true, multiple: false, title: "选择文件保存目录" });
    if (!picked || Array.isArray(picked)) return;
    await FeisuoBridge.updateAppConfig({ receive_dir: picked });
    showToast("保存目录已更新");
    await Promise.all([loadLocalInfo(), loadLocalFiles()]);
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "更改目录失败"), true);
  }
}

async function toggleAutostart() {
  try {
    await FeisuoBridge.setAutostart(localInfo.autostart);
    if (localInfo.autostart) {
      if (updateStatus.value?.isInstalled) {
        showToast("已开启开机自启");
      } else {
        showToast("已开启开机自启（提示：便携版请勿移动或删除程序）");
      }
    } else {
      showToast("已关闭开机自启");
    }
  } catch (e) {
    localInfo.autostart = !localInfo.autostart;
    showToast(FeisuoBridge.describeError(e, "设置开机自启失败"), true);
  }
}

async function openReceiveFolder() {
  try {
    await FeisuoBridge.openReceiveFolder();
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "打开接收目录失败"), true);
  }
}

async function openLogFolder() {
  try {
    await FeisuoBridge.openLogFolder();
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "打开日志目录失败"), true);
  }
}

// =======================================================================
// 应用内更新
// =======================================================================

/** 主动拉一次状态快照 —— 只靠事件会在"窗口挂机期间完成下载"时丢失结果 */
async function loadUpdateStatus() {
  try {
    updateStatus.value = await FeisuoBridge.getUpdateStatus();
  } catch (e) {
    // 拿不到状态不是致命错误, 保留上一次的值即可
    console.warn("[update] 读取更新状态失败:", e);
  }
}

async function checkForUpdate() {
  try {
    // 手动点击: 失败要让用户看见, 不能安静吞掉
    updateStatus.value = await FeisuoBridge.checkForUpdate(true);
    if (updateStatus.value.phase === "uptodate") {
      showToast("已是最新版本");
    } else if (updateStatus.value.phase === "ready") {
      showToast("新版本已下载，可点击「立即更新」");
    } else if (updateStatus.value.phase === "failed") {
      showToast(FeisuoBridge.describeError(updateStatus.value, "检查更新失败"), true);
    }
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "检查更新失败"), true);
  }
}

async function applyUpdate() {
  try {
    showToast("正在重启安装，请稍候…");
    await FeisuoBridge.applyUpdate();
  } catch (e) {
    // 唯一需要在这里兜底的失败: 换名接力走不通(目录只读 / 文件被占用)
    showToast(FeisuoBridge.describeError(e, "更新失败"), true);
    await loadUpdateStatus();
  }
}

async function openUpdatePage() {
  try {
    await FeisuoBridge.openUpdatePage(updateStatus.value.releasePage);
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "打开下载页失败"), true);
  }
}

async function toggleAutoUpdate() {
  try {
    await FeisuoBridge.updateAppConfig({ auto_check_update: localInfo.auto_check_update });
    showToast(localInfo.auto_check_update ? "已开启自动检查更新" : "已关闭自动检查更新");
  } catch (e) {
    localInfo.auto_check_update = !localInfo.auto_check_update;
    showToast(FeisuoBridge.describeError(e, "设置自动检查更新失败"), true);
  }
}

// =======================================================================
// 窗口控制
// =======================================================================
async function toggleTheme() {
  const previous = store.theme;
  store.applyTheme();
  // 主题偏好也写回后端配置, 保证两端一致。
  // 写失败不能只吞掉：画面已经换成新主题，配置里仍是旧的，
  // 下次打开会跳回去，用户以为已经换上了。
  try {
    await FeisuoBridge.updateAppConfig({ theme: store.theme });
    localInfo.theme = store.theme;
  } catch (e) {
    store.applyTheme(previous);
    localInfo.theme = previous;
    showToast(FeisuoBridge.describeError(e, "切换外观失败"), true);
  }
}

async function minimizeWindow() {
  await FeisuoBridge.minimizeWindow();
}

async function toggleMaximize() {
  await FeisuoBridge.toggleMaximizeWindow();
}

/**
 * 关闭主窗口: 依据设置决定是弹选择框还是直接执行。
 * 旧实现无论如何都只是隐藏窗口, 用户永远无法从窗口关掉程序。
 */
function requestClose() {
  switch (localInfo.close_action) {
    case "tray":
      closeToTray();
      break;
    case "exit":
      quitApp();
      break;
    default:
      showCloseDialog.value = true;
  }
}

async function closeToTray() {
  showCloseDialog.value = false;
  try {
    await FeisuoBridge.hideWindow();
    showToast("已最小化到托盘，后台继续守护");
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "最小化到托盘失败"), true);
  }
}

async function quitApp() {
  showCloseDialog.value = false;
  await FeisuoBridge.quitApp();
}

// =======================================================================
// 直连 / 配对
// =======================================================================
function openDirectConnectModal() {
  directConnectIp.value = "";
  directConnectMsg.value = "";
  isDirectConnectSuccess.value = false;
  showDirectConnectDialog.value = true;
}

async function submitDirectConnect() {
  const ip = directConnectIp.value.trim();
  if (!ip) return;
  // 按钮已禁用格式错误的输入，这里再挡一道是为了 Enter 键 ——
  // @keyup.enter 直接调本函数，不经过按钮的 disabled 判断。
  if (!isValidIpv4(ip)) {
    directConnectMsg.value = "这不像一个 IPv4 地址，应该形如 192.168.1.120";
    isDirectConnectSuccess.value = false;
    return;
  }
  isConnectingIp.value = true;
  directConnectMsg.value = "";
  isDirectConnectSuccess.value = false;
  try {
    const dev = await FeisuoBridge.probeDevice(ip);
    await refreshDevices();
    store.selectedDeviceId = dev.device_id;
    isDirectConnectSuccess.value = true;
    directConnectMsg.value = `已识别设备：${dev.device_name}（${dev.ip}）`;
    showToast(`已连接 ${dev.device_name}`);
    setTimeout(() => (showDirectConnectDialog.value = false), 800);
  } catch (e) {
    directConnectMsg.value = FeisuoBridge.describeError(e, "直连探测失败");
  } finally {
    isConnectingIp.value = false;
  }
}

function openPairWithSpecificDevice(dev: DiscoveredDevice) {
  targetDeviceForPairing.value = dev;
  inputTargetIp.value = dev.ip;
  isManualIpMode.value = false;
  void openPairModal("input");
}

/** 从名册条目发起配对（名册里 `ip` 可能是 null —— 离线设备没有实时地址） */
function pairDevice(dev: DeviceRosterEntry) {
  openPairWithSpecificDevice({
    device_id: dev.device_id,
    device_name: dev.device_name,
    os_type: dev.os_type,
    ip: dev.ip || dev.last_ip,
    transfer_port: dev.transfer_port || 0,
    is_trusted: dev.trust_level === "permanent",
    last_seen_secs: Math.floor(Date.now() / 1000),
    caps: dev.peer_caps,
    app_version: dev.peer_version,
  });
}

/** 在「永久信任」与「每次匹配码」之间切换（§2.3 / D 决策） */
/**
 * 「N 分钟内免重复确认」窗口的分钟数（§2.3.1）。
 *
 * **显示值必须由 core 报上来的实际 TTL 推导，不能在前端另存一份。**
 * core 那边有个 10 分钟的**硬上界**（`SESSION_GRANT_MAX_SECS`，
 * 它是安全属性不是偏好）—— 前端如果自己写死"5 分钟"，
 * 那么用户把配置调大时，界面会**说谎**：说 5 分钟、实际给 10 分钟。
 * 界面上说的时间必须等于实际生效的时间。
 */
const grantWindowMinutes = computed(() => {
  const secs = grantWindowSecs.value;
  if (!secs) return 5;
  return Math.max(1, Math.round(secs / 60));
});

/** 某台设备当前生效中的短期授权（key = device_id）。 */
const activeGrants = ref<Record<string, ActiveGrant[]>>({});

/** 实际生效的授权窗口秒数（0 = 功能关闭）。由 core 回报，不在前端硬编码。 */
const grantWindowSecs = ref(0);

/** 把 epoch 秒数说成"还有多久"，用于授权到期提示。 */
function humanizeUntil(epochSecs: number): string {
  const left = epochSecs - Math.floor(Date.now() / 1000);
  if (left <= 0) return "刚刚已到期";
  if (left < 60) return `${left} 秒后`;
  const m = Math.ceil(left / 60);
  if (m < 60) return `${m} 分钟后`;
  return `${Math.ceil(m / 60)} 小时后`;
}

/**
 * 撤销某台设备的全部短期授权（§2.3.1 的「可撤销」）。
 *
 * **可撤销与短期是同一件安全性质的两半**：短期保证"最坏情况有限"，
 * 可撤销保证"用户随时能把最坏情况归零"。只做前者的话，
 * 用户在那几分钟里无能为力。
 */
async function revokeDeviceGrants(deviceId: string, deviceName: string) {
  try {
    const n = await FeisuoBridge.revokeDeviceGrants(deviceId);
    await loadActiveGrants();
    showToast(
      n > 0
        ? `已取消 ${deviceName} 的免重复确认（${n} 项）`
        : `${deviceName} 当前没有生效中的免确认`
    );
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "取消免确认失败"), true);
  }
}

/** 刷新所有设备的生效授权列表。 */
async function loadActiveGrants() {
  try {
    activeGrants.value = await FeisuoBridge.listActiveGrants();
  } catch (e) {
    // 拉不到**不能**让整页崩掉，也不能假装"没有授权"——
    // 假装没有会让用户以为已经关掉了。保留上一份并记一条日志。
    console.warn("读取生效中的短期授权失败", e);
  }
}

async function toggleTrustLevel(deviceId: string) {
  const dev = store.roster.find((d) => d.device_id === deviceId);
  if (!dev) return;
  const next = dev.trust_level === "permanent" ? "session" : "permanent";
  try {
    await FeisuoBridge.setDeviceTrustLevel(deviceId, next);
    await store.refreshDevices();
    showToast(
      next === "permanent"
        ? `已将 ${dev.device_name} 设为永久信任`
        : `已设为每次匹配码：${dev.device_name} 每次发文件时，` +
          `你都要输入它屏幕上显示的 6 位码`
    );
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "修改信任等级失败"), true);
  }
}

/** 隐藏设备：只是不再显示，信任与推送保留（§3.1~§3.2） */
async function hideDevice(deviceId: string) {
  const dev = store.roster.find((d) => d.device_id === deviceId);
  try {
    await store.toggleHidden(deviceId);
    showToast(`已隐藏 ${dev?.device_name || "该设备"}；如不想再收它的文件，在已隐藏里解除配对即可`);
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "隐藏失败"), true);
  }
}



async function openPairModal(mode: "input" | "show") {
  pairMode.value = mode;
  pairErrorMessage.value = "";
  inputPin.value = "";
  pairingAbortFlag = false;
  pairedSuccessDevice.value = null;
  try {
    localPairPin.value = await FeisuoBridge.generatePairPin();
  } catch {
    localPairPin.value = "------";
  }
  showPairingDialog.value = true;

  if (mode === "show") {
    startPinCountdown();
  } else {
    stopPinCountdown();
  }

  // 智能识别局域网 IP
  if (targetDeviceForPairing.value?.ip) {
    inputTargetIp.value = targetDeviceForPairing.value.ip;
    isManualIpMode.value = false;
  } else if (discoveredPeerDevices.value.length === 1) {
    inputTargetIp.value = discoveredPeerDevices.value[0].ip || "";
    isManualIpMode.value = false;
  } else if (discoveredPeerDevices.value.length > 1) {
    if (!inputTargetIp.value || !discoveredPeerDevices.value.some((d) => d.ip === inputTargetIp.value)) {
      inputTargetIp.value = discoveredPeerDevices.value[0].ip || "";
    }
    isManualIpMode.value = false;
  } else {
    isManualIpMode.value = true;
    if (!inputTargetIp.value) inputTargetIp.value = "";
  }

  await nextTick();
  if (mode === "input" && pinInputRef.value) pinInputRef.value.focus();
}

function formatPinInput(e: Event) {
  const target = e.target as HTMLInputElement;
  let digits = target.value.replace(/\D/g, "").slice(0, 6);
  if (digits.length > 3) digits = digits.slice(0, 3) + " " + digits.slice(3);
  inputPin.value = digits;
}

function startPinCountdown() {
  stopPinCountdown();
  pinSecondsLeft.value = 30;
  pinCountdownTimer = setInterval(async () => {
    if (pinSecondsLeft.value > 1) {
      pinSecondsLeft.value--;
    } else {
      await refreshLocalPin();
    }
  }, 1000);
}

function stopPinCountdown() {
  if (pinCountdownTimer) {
    clearInterval(pinCountdownTimer);
    pinCountdownTimer = null;
  }
}

async function refreshLocalPin() {
  try {
    localPairPin.value = await FeisuoBridge.generatePairPin();
  } catch {
    /* 保持旧码 */
  }
  pinSecondsLeft.value = 30;
}

function cancelPairing() {
  pairingAbortFlag = true;
  isPairingSubmit.value = false;
  pairErrorMessage.value = "已取消连接";
}

function finishPairingSuccess() {
  stopPinCountdown();
  showPairingDialog.value = false;
  pairMode.value = "input";
}

function jumpToSendWithPairedDevice() {
  stopPinCountdown();
  if (pairedSuccessDevice.value) {
    store.selectedDeviceId = pairedSuccessDevice.value.device_id;
  }
  showPairingDialog.value = false;
  currentTab.value = "send";
  pairMode.value = "input";
  showToast(`已选择「${pairedSuccessDevice.value?.device_name || "受信设备"}」，可直接拖拽文件发送`);
}

async function submitPairing() {
  const pin = inputPin.value.replace(/\s+/g, "");
  if (pin.length < 6) {
    pairErrorMessage.value = "请输入 6 位有效数字配对码";
    return;
  }
  const ip = inputTargetIp.value.trim();
  if (!ip) {
    pairErrorMessage.value = "请指定对方设备的局域网 IP";
    return;
  }

  // 手动填的 IP 先做格式校验: mDNS 自动填的地址必然合法, 而手打的
  // 192.168.1 / 999.1.1.1 只会换来一次注定失败的连接。
  // 后端仍会自己解析一遍(fail-closed), 这里纯粹是为了给出能照着改的提示。
  if (!isValidIpv4(ip)) {
    pairErrorMessage.value = "这不像一个 IPv4 地址，应该形如 192.168.1.120";
    return;
  }
  pairErrorMessage.value = "";
  pairingAbortFlag = false;

  // 优先使用发现到的真实端口。名册里离线设备的端口是空的，
  // 不能拿本机端口去顶 —— 两边端口可以不同，顶上去就会连错。
  // 从设备卡片点进来时，端口记在配对目标上，即使名册刚好刷新掉也还在。
  const dev = store.roster.find((d) => (d.ip || d.last_ip) === ip);
  const remembered =
    targetDeviceForPairing.value?.ip === ip ? targetDeviceForPairing.value.transfer_port : 0;
  const port = dev?.transfer_port || remembered || 0;
  if (!port) {
    pairErrorMessage.value =
      "还不知道对方的传输端口。请等它出现在设备列表里再配对，或先用「直连对端 IP」探测一次";
    return;
  }

  isPairingSubmit.value = true;
  try {
    const paired = await FeisuoBridge.pairWithDevice(ip, port, pin);
    if (pairingAbortFlag) return;

    pairedSuccessDevice.value = paired;
    pairMode.value = "success";
    showToast(`配对成功！「${paired.device_name}」已加入受信列表`);
    await Promise.all([refreshDevices(), loadTrustedDevices()]);
  } catch (e) {
    if (!pairingAbortFlag) {
      pairErrorMessage.value = FeisuoBridge.describeError(e, "配对握手失败");
    }
  } finally {
    isPairingSubmit.value = false;
  }
}

async function handleApproval(
  action: "allow_once" | "allow_with_grant" | "allow_and_trust" | "reject"
) {
  if (!pendingApproval.value) return;
  const id = pendingApproval.value.approval_id;
  const name = pendingApproval.value.sender_name;
  // 审批人**不再输入任何码**：「每次匹配码」等级下要出示的码是本机生成、
  // 显示在窗口上的，由发起方敲回来。点「允许」就是允许。
  pendingApproval.value = null;
  lastSubmittedApprovalId.value = "";
  try {
    await FeisuoBridge.respondApproval(id, action);
    if (action === "allow_once") showToast(`已允许 ${name} 的本次传输（未建立长期信任）`);
    else if (action === "allow_with_grant") {
      // 措辞必须说清**范围**与**到期**—— 少了任一条，用户就以为
      // "这台设备以后都不用码了"，而实际只是这一类操作、到点就失效。
      showToast(
        `已允许本次；${grantWindowMinutes} 分钟内「${name}」发文件不必再出示匹配码（到期自动失效，可随时取消）`
      );
      await loadActiveGrants();
    } else if (action === "allow_and_trust") {
      showToast(`已允许并永久信任 ${name}`);
      await loadTrustedDevices();
    } else if (action === "reject") showToast("已拒绝该传输请求");
  } catch (e) {
    showToast(FeisuoBridge.describeError(e, "处理请求失败"), true);
  }
}

// =======================================================================
// 「每次匹配码」的两端交互
// =======================================================================

/** 把本机生成的码排成好念好读的样子：`123456` -> `123 456`。
 *
 *  念码是这个等级**唯一**的传导方式，所以排版直接决定它好不好用：
 *  6 位连读极易听错（3/3 停顿是必要的），而界面上少一个空格就少一次
 *  核对机会。
 */
function formatGrantChallenge(code: string): string {
  const t = (code || "").trim();
  if (t.length !== 6) return t;
  return `${t.slice(0, 3)} ${t.slice(3)}`;
}

/** 请用户输入**对方窗口上显示**的匹配码。
 *
 *  方向：**对方（接收方）出码并显示，本机（发起方）输入**。
 *  早期实现是反的 —— 本机生成码、念给对方、让对方在自己的审批窗口里敲。
 *  那样接收方核对的是**发起方自选的数字**，它手上没有任何独立信息，
 *  这道门等于没锁。详见
 *  `core/src/transport/server.rs` 的 `ApprovalManager::challenge_for`。
 *
 *  @param deviceName 对端设备名，用于把话说清楚是谁在等
 *  @param opLabel    本次要做的操作（发送 / 浏览 / 取回）
 *  @param reason     对端给的提示（例如"码不匹配"），有就复述出来
 *  @returns 规范化后的 6 位码；用户取消或没填则返回 `null`
 */
function promptGrantCodeFromPeer(
  deviceName: string,
  opLabel: string,
  reason = ""
): Promise<string | null> {
  return askCodeFromPeer(deviceName, opLabel, reason);
}

// ---------------------------------------------------------------------------
// 「输入对方窗口上的匹配码」弹窗
//
// 为什么不用 `window.prompt`：
//   1. 它是**浏览器原生**对话框，在 Tauri 窗口里样式与整个应用割裂，
//      而且无法置顶到模态层之上（用户可能看不到它）。
//   2. 原生 prompt 的文本是**纯文本**，早先这里写了 `**加粗**` 标记，
//      用户会看到**字面的星号**。
//   3. 没法做输入校验、没法显示"对方提示"的层级、没法统一按钮位置。
// ---------------------------------------------------------------------------
const codePrompt = ref<{
  deviceName: string;
  opLabel: string;
  reason: string;
  value: string;
  resolve: (v: string | null) => void;
} | null>(null);

function askCodeFromPeer(
  deviceName: string,
  opLabel: string,
  reason: string
): Promise<string | null> {
  return new Promise((resolve) => {
    codePrompt.value = { deviceName, opLabel, reason, value: "", resolve };
  });
}

function submitCodePrompt() {
  const p = codePrompt.value;
  if (!p) return;
  // 去掉空格与连字符：用户很可能照着 "123 456" 敲。
  const t = p.value.replace(/[\s-]/g, "");
  if (!t) {
    showToast("需要对方窗口上的 6 位码才能继续", true);
    return;
  }
  p.resolve(t);
  codePrompt.value = null;
}

function cancelCodePrompt() {
  const p = codePrompt.value;
  if (!p) return;
  p.resolve(null);
  codePrompt.value = null;
}

// =======================================================================
// 工具
// =======================================================================
function showToast(msg: string, isError = false) {
  toastMessage.value = msg;
  toastIsError.value = isError;
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    toastMessage.value = "";
    toastIsError.value = false;
  }, isError ? 4200 : 2400);
}

async function copyText(text: string) {
  if (!text) return;
  try {
    await navigator.clipboard.writeText(text);
    showToast("已复制到剪贴板");
  } catch {
    showToast("复制失败，请手动选择文本", true);
  }
}

function selectDevice(id: string) {
  const previousId = store.selectedDeviceId;
  store.selectedDeviceId = id;
  remoteDiskFiles.value = [];
  selectedRemoteNames.value.clear();
  // 待发清单的落点是相对上一台收件目录的子路径。
  // 只在真的换了一台时清：重复点当前设备不该把用户刚设好的落点抹掉。
  if (previousId && previousId !== id && pendingDestSubPath.value) {
    pendingDestSubPath.value = "";
    if (stagedFiles.value.length > 0) {
      showToast("已换成另一台设备，待发清单改落到它的收件目录");
    }
  }
  // 换设备必须把浏览路径退回根目录
  resetRemotePath();
  // 新设备的能力位可能不同（新版/旧版），重算初始浏览模式
  bootstrapRemoteBrowseMode();

  // 联动优化 1：若用户在系统设置页点击了某台设备，自动切回主工作视图（发送文件）以呈现该设备
  if (currentTab.value === "settings") {
    currentTab.value = "send";
  }
  // 联动优化 2：若当前处于双栏穿梭页面，切换设备后立即重新拉取新选中设备的文件列表
  if (currentTab.value === "shuttle") {
    void loadRemoteFiles();
  }
}

/** 穿梭右栏：按对端能力位决定初始浏览模式（§7.2）
 *
 *  为什么不是"先试卷模式，失败再退"？
 *  试错方案会让**每一次**打开穿梭都先发一个注定被拒的请求（旧对端
 *  会回"未声明支持真实卷浏览"），用户看到的是列表闪一下 + 报错提示。
 *  而信标里已经带了 caps，零成本就能知道答案。
 */
function bootstrapRemoteBrowseMode() {
  const dev = store.selectedDevice;
  if (!dev || dev.is_self) return;
  if ((dev.peer_caps & CAPS.BROWSE_VOLUMES) === 0) return;
  // 已经有明确选择就不要覆盖。
  // 这个函数在**每次切入穿梭页**时都会跑，早先无条件写 `*` —— 于是用户
  // 刚在右栏选中 D 盘，切到"传输记录"再切回来，地址栏就被弹回第一块盘，
  // 正在看的东西整个换掉。`loadRemoteFiles` 里的自愈分支负责兜住
  // "还没协商过"的初始态，这里只负责"已经有选择就别动"。
  if (remoteVolume.value && remoteVolumeMode.value) return;
  remoteVolume.value = VOLUME_ANY;
}

function getOsIcon(osType: string) {
  const t = (osType || "").toLowerCase();
  if (t.includes("android")) return "ph-fill ph-device-mobile";
  if (t.includes("mac") || t.includes("ios")) return "ph-fill ph-apple-logo";
  return "ph-fill ph-desktop";
}

function getFileIcon(name: string) {
  const lower = (name || "").toLowerCase();
  if (lower.endsWith(".pdf")) return "ph-fill ph-file-pdf file-glyph pdf";
  if (lower.endsWith(".doc") || lower.endsWith(".docx")) return "ph-fill ph-file-doc file-glyph word";
  if (lower.match(/\.(jpg|jpeg|png|webp|gif|bmp)$/)) return "ph-fill ph-file-image file-glyph img";
  if (lower.match(/\.(mp4|mov|mkv|avi|webm)$/)) return "ph-fill ph-file-video file-glyph video";
  if (lower.match(/\.(zip|tar|7z|rar|gz)$/)) return "ph-fill ph-file-archive file-glyph zip";
  if (lower.match(/\.(mp3|wav|flac|m4a|aac)$/)) return "ph-fill ph-file-audio file-glyph audio";
  return "ph-fill ph-file-text file-glyph";
}
</script>

<style scoped>
/* ==========================================================
 * 所有颜色一律取自 style.css 中的主题令牌 (var(--xxx))。
 * 严禁在组件内硬编码 #hex / rgba()，否则浅色主题必然出现
 * "按钮纯白、标题黑字" 之类的错位。
 * ========================================================== */

/* 1. 根容器 */
.desktop-shell {
  width: 100vw;
  height: 100vh;
  display: grid;
  grid-template-rows: 40px 1fr auto;
  background-color: var(--bg-window);
  color: var(--text-primary);
  overflow: hidden;
  box-sizing: border-box;
}

/* 2. 标题栏 */
.app-titlebar {
  height: 40px;
  background: var(--titlebar-bg);
  color: var(--titlebar-fg);
  border-bottom: 1px solid var(--border-subtle);
  display: flex;
  justify-content: space-between;
  align-items: center;
  user-select: none;
  z-index: var(--z-titlebar);
}
.titlebar-drag-area {
  display: flex;
  align-items: center;
  gap: 10px;
  padding-left: 14px;
  flex: 1;
  height: 100%;
}
.brand-badge {
  width: 22px;
  height: 22px;
  border-radius: var(--radius-md);
  display: flex;
  align-items: center;
  justify-content: center;
  overflow: hidden;
}
.brand-logo-img {
  width: 100%;
  height: 100%;
  object-fit: contain;
  display: block;
}
.brand-text { font-size: 13px; font-weight: 700; letter-spacing: 0.02em; }
.lan-network-chip {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  background: var(--titlebar-chip-bg);
  border: 1px solid var(--border-subtle);
  padding: 2px 10px;
  border-radius: var(--radius-pill);
  font-size: 11px;
  color: var(--text-secondary);
}
.live-dot { width: 6px; height: 6px; border-radius: 50%; background: var(--accent); }

.caption-controls {
  display: flex;
  align-items: center;
  height: 100%;
  position: relative;
  z-index: var(--z-titlebar-caption);
  pointer-events: auto;
}
.caption-btn {
  width: 46px;
  height: 40px;
  background: transparent;
  border: 0;
  color: var(--text-secondary);
  display: grid;
  place-items: center;
  font-size: 15px;
  cursor: pointer;
  transition: background 0.15s, color 0.15s;
}
.caption-btn:hover { background: var(--titlebar-hover); color: var(--text-primary); }
.caption-btn.close-caption:hover { background: var(--win-close-hover); color: var(--text-on-solid); }

/* 3. 主区域 */
.app-container {
  display: grid;
  grid-template-columns: 262px 1fr;
  min-height: 0;
  height: 100%;
  overflow: hidden;
}

/* 4. 左侧边栏 */
.app-sidebar {
  background: var(--bg-sidebar);
  border-right: 1px solid var(--border-subtle);
  display: grid;
  grid-template-rows: auto auto 1fr auto;
  min-height: 0;
  overflow: hidden;
}

.local-machine-card {
  margin: 12px 12px 6px;
  padding: 10px 12px;
  border-radius: var(--radius-md);
  background: var(--bg-card);
  border: 1px solid var(--border-subtle);
  display: flex;
  align-items: center;
  gap: 10px;
  cursor: pointer;
  transition: background 0.18s, border-color 0.18s;
}
.local-machine-card:hover { background: var(--bg-card-hover); border-color: var(--border-strong); }
.local-machine-card.selected { border-color: var(--accent); background: var(--bg-active); }
.local-icon {
  width: 34px;
  height: 34px;
  border-radius: var(--radius-md);
  background: var(--accent);
  color: var(--accent-contrast);
  display: grid;
  place-items: center;
  font-weight: 800;
  font-size: 14px;
  flex-shrink: 0;
}
.local-meta { flex: 1; min-width: 0; }
.machine-name {
  display: block;
  font-size: 13px;
  font-weight: 650;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--text-primary);
}
.daemon-status {
  font-size: 11px;
  color: var(--text-secondary);
  margin-top: 1px;
  display: flex;
  align-items: center;
  gap: 4px;
}
.mini-status-dot { width: 5px; height: 5px; border-radius: 50%; background: var(--text-tertiary); }
.mini-status-dot.active { background: var(--accent); }
/* .btn-gear 已随齿轮按钮一起删除（与卡片点击重复，且页签里已有「系统设置」） */

/* ---- 来源网段编辑器 ---- */
.setting-item-block-col { flex-direction: column; align-items: stretch; gap: 10px; }
.setting-desc-text code.path-mono { font-size: 11.5px; padding: 1px 4px; }
.subnet-editor {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  align-items: center;
}
.subnet-chip {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 3px 4px 3px 8px;
  background: var(--bg-inset);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-pill);
}
.subnet-chip > i { font-size: 13px; color: var(--text-tertiary); }
.subnet-chip.is-invalid {
  border-color: var(--danger-text);
  background: var(--danger-soft);
}
.subnet-chip.is-invalid > i { color: var(--danger-text); }
.subnet-chip-input {
  width: 15ch;
  padding: 2px 2px;
  font-size: 12px;
  color: var(--text-primary);
  background: none;
  border: 0;
  outline: none;
}
.subnet-chip-del {
  border: 0;
  background: none;
  color: var(--text-tertiary);
  cursor: pointer;
  font-size: 13px;
  padding: 2px 4px;
  border-radius: var(--radius-pill);
}
.subnet-chip-del:hover { color: var(--danger-text); background: var(--bg-hover-soft); }
.subnet-add { padding: 5px 10px; font-size: 12px; }
/* 压在 **soft 底色** 上的文字必须用 `--*-on-soft`，不是 `--*-text`。
   项目已有既定写法（App.vue 里 .status-chip.untrusted 与 .btn-approval-block），
   这里照抄而不是自己配色 —— 两套主题下 `-on-soft` 都是为压 soft 底专门调深的，
   而 `--*-text` 是压在**实心容器**上的。
   （第一版误用 `--danger-text`，浅色主题实测 4.43:1，压线不过。） */
.subnet-warn-hint {
  display: flex;
  gap: 8px;
  align-items: flex-start;
  margin: 0;
  padding: 9px 11px;
  font-size: 12px;
  line-height: 1.6;
  color: var(--warn-on-soft);
  background: var(--warn-soft);
  border: 1px solid var(--warn-border);
  border-radius: var(--radius-md);
}
.subnet-warn-hint > i { flex: 0 0 auto; margin-top: 1px; font-size: 15px; }
.subnet-warn-hint.is-error {
  color: var(--danger-on-soft);
  background: var(--danger-soft);
  border-color: var(--danger-border);
}
.subnet-actions { margin-top: 2px; align-items: center; gap: 10px; }
.subnet-dirty-hint { font-size: 12px; color: var(--text-tertiary); }
.sidebar-title-bar {
  padding: 10px 16px 6px;
  display: flex;
  justify-content: space-between;
  align-items: center;
  font-size: 11.5px;
  font-weight: 700;
  color: var(--text-tertiary);
  letter-spacing: 0.04em;
}
.sidebar-header-actions { display: flex; align-items: center; gap: 6px; }
.icon-action-btn {
  background: none;
  border: 0;
  color: var(--text-secondary);
  cursor: pointer;
  font-size: 14px;
  padding: 3px;
  border-radius: var(--radius-sm);
  display: flex;
  align-items: center;
  justify-content: center;
  transition: color 0.15s, background 0.15s;
}
.icon-action-btn:hover { color: var(--accent-text); background: var(--bg-hover-soft); }
.spinning { animation: spin 1s linear infinite; }
@keyframes spin { from { transform: rotate(0); } to { transform: rotate(360deg); } }

.devices-scroll-area {
  overflow-y: auto;
  padding: 4px 10px;
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.device-card-row {
  display: grid;
  grid-template-columns: 32px 1fr auto;
  gap: 10px;
  align-items: center;
  padding: 8px 10px;
  border-radius: var(--radius-md);
  cursor: pointer;
  transition: background 0.15s, border-color 0.15s;
  border: 1px solid transparent;
}
.device-card-row:hover { background: var(--bg-hover-soft); }
.device-card-row.active { background: var(--bg-active); border-color: var(--accent); }
.os-icon-box {
  width: 32px;
  height: 32px;
  border-radius: var(--radius-md);
  background: var(--bg-card);
  color: var(--accent-text);
  display: grid;
  place-items: center;
  font-size: 16px;
}
.device-text-col { min-width: 0; }
.device-line1 { display: flex; align-items: center; gap: 6px; }
.device-title {
  font-size: 12.5px;
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--text-primary);
}
.device-line2 { font-size: 11px; color: var(--text-secondary); margin-top: 1px; }

.device-action-badge { display: flex; align-items: center; }
.trust-badge {
  font-size: 10px;
  background: var(--accent-soft);
  color: var(--accent-on-soft);
  padding: 2px 6px;
  border-radius: var(--radius-sm);
  font-weight: 600;
  display: inline-flex;
  align-items: center;
  gap: 3px;
}
.btn-quick-pair {
  background: var(--accent);
  color: var(--accent-contrast);
  border: 0;
  border-radius: var(--radius-sm);
  font-size: 11px;
  font-weight: 700;
  padding: 3px 8px;
  cursor: pointer;
  transition: filter 0.15s;
}
.btn-quick-pair:hover { filter: brightness(1.12); }

.discovery-empty-state { padding: 24px 14px; text-align: center; color: var(--text-tertiary); }
.radar-pulse-ring {
  width: 48px;
  height: 48px;
  border-radius: 50%;
  background: var(--accent-soft);
  color: var(--accent-on-soft);
  display: grid;
  place-items: center;
  font-size: 22px;
  margin: 0 auto 10px;
  animation: pulse 2.5s infinite;
}
@keyframes pulse {
  0% { box-shadow: 0 0 0 0 var(--accent-glow); }
  70% { box-shadow: 0 0 0 12px transparent; }
  100% { box-shadow: 0 0 0 0 transparent; }
}
/* 选择器跟着标签一起改：原来写的是 `.discovery-empty-state h4`，
 * 元素从 h4 换成 p 之后这条规则会**静默失效**，字号退回浏览器默认 16px。
 * 四个属性都是显式声明，所以换标签后视觉完全一致。 */
.discovery-empty-state .discovery-status-text {
  font-size: 12.5px;
  color: var(--text-secondary);
  margin-bottom: 12px;
  font-weight: 600;
}

/* 防火墙引导。
 *
 * 用 --warn-* 那一族而不是中性色：它说的是"有个东西挡住了你"，
 * 而中性灰会读成"又一条说明文字"，用户扫过去就忘了。
 * 底色用 -soft 变体，保证在两套主题下都不刺眼
 * （check-contrast.py 覆盖这两枚前景/底色组合）。 */
.firewall-hint {
  display: flex;
  gap: 10px;
  align-items: flex-start;
  margin-top: 16px;
  padding: 12px 14px;
  max-width: 420px;
  text-align: left;
  background: var(--warn-soft);
  border: 1px solid var(--warn-border);
  border-radius: var(--radius-md);
}
.firewall-hint > i {
  flex: 0 0 auto;
  margin-top: 1px;
  font-size: 18px;
  color: var(--warn-text);
}
.firewall-hint-body { min-width: 0; }
.firewall-hint-body strong {
  display: block;
  font-size: 12.5px;
  color: var(--warn-text);
  margin-bottom: 4px;
}
.firewall-hint-body p {
  margin: 0;
  font-size: 12px;
  line-height: 1.6;
  color: var(--text-secondary);
}
.firewall-hint-how { margin-top: 6px !important; }
.firewall-hint-body code {
  font-family: Consolas, "Cascadia Mono", monospace;
  font-size: 11.5px;
  padding: 1px 4px;
  border-radius: var(--radius-sm);
  background: var(--bg-inset);
  color: var(--text-primary);
}
.empty-actions-row { display: flex; gap: 8px; justify-content: center; flex-wrap: wrap; }
.btn-clean-primary {
  background: var(--accent-soft);
  border: 1px solid var(--accent-border);
  color: var(--accent-on-soft);
  padding: 6px 12px;
  border-radius: var(--radius-md);
  font-size: 11.5px;
  font-weight: 600;
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  gap: 4px;
  transition: background 0.15s, color 0.15s;
}
.btn-clean-primary:hover { background: var(--accent); color: var(--accent-contrast); }
.btn-clean-subtle {
  background: var(--bg-card);
  border: 1px solid var(--border-subtle);
  color: var(--text-secondary);
  padding: 6px 10px;
  border-radius: var(--radius-md);
  font-size: 11.5px;
  font-weight: 600;
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  gap: 4px;
  transition: background 0.15s, color 0.15s;
}
.btn-clean-subtle:hover { color: var(--text-primary); background: var(--bg-card-hover); }

/* ===================================================================
   名册分组 / 离线灰显 / 已隐藏抽屉（§3）
   =================================================================== */
.roster-group-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 10px 12px 6px;
  font-size: 11px;
  letter-spacing: 0.04em;
  color: var(--text-dim);
  user-select: none;
}
/* 分组标题的折叠态。
 *
 * ⚠️ 这一行**曾经是残缺的**：迁移硬编码颜色时，脚本按行号替换把同一条规则里
 * 的 `color: var(--text-main, #e8eaed);` 整行删掉了，只剩下一条孤零零的
 * `color: var(--text-main);` 飘在规则外面 —— 变成一条**非法的顶层声明**。
 *
 * 症状是两层，且都不明显：
 *   1. `vite build` 报 `css-syntax-error: Expected identifier but found
 *      whitespace` —— 混在几十条 warning 里，没人看；
 *   2. 折叠时分组标题**不会变亮**（那条 color 从来没生效过）。
 *
 * 之所以危险：CSS 解析器遇到非法顶层声明会**丢弃它并继续往下解析**，
 * 于是整份样式表其余部分照常工作 —— 表现是"只有这一个交互不对"，
 * 极难联想到是语法坏了。
 */
.roster-group-head.collapsible { cursor: pointer; }
.roster-group-head.collapsible:hover { color: var(--text-main); }
.roster-group-title { display: inline-flex; align-items: center; gap: 6px; }
.roster-group-head .ph-caret-right {
  transition: transform 0.15s ease;
  display: inline-block;
}
.roster-group-head .ph-caret-right.rotated { transform: rotate(90deg); }
.roster-group-count {
  font-variant-numeric: tabular-nums;
  background: var(--bg-hover-soft);
  border-radius: var(--radius-pill);
  padding: 1px 7px;
}

/* 离线：灰显但**保留在列表里**（§3.7 核心） */
.device-card-row.offline { opacity: 0.55; }
.device-card-row.offline:hover { opacity: 0.8; }
.device-card-row.offline .device-title { text-decoration: none; }

.self-tag {
  font-size: 10px;
  padding: 0 5px;
  border-radius: var(--radius-sm);
  background: var(--bg-hover-soft);
  color: var(--text-dim);
  margin-left: 6px;
}

.trust-badge.clickable { cursor: pointer; border: none; }
.trust-badge.session {
  background: var(--info-soft);
  color: var(--info-on-soft);
}
.hide-device-btn {
  background: transparent;
  border: none;
  color: var(--text-dim);
  cursor: pointer;
  padding: 3px 4px;
  border-radius: var(--radius-md);
  opacity: 0;
  transition: opacity 0.12s ease;
}
.device-card-row:hover .hide-device-btn { opacity: 1; }
.hide-device-btn:hover { background: var(--bg-hover-soft); color: var(--text-main); }

.roster-empty-hint {
  padding: 8px 12px 12px;
  font-size: 12px;
  line-height: 1.6;
  color: var(--text-dim);
}

.hidden-drawer {
  margin: 0 8px 8px;
  padding: 8px;
  border-radius: var(--radius-md);
  background: var(--bg-row-soft);
  border: 1px solid var(--border-subtle);
  max-height: 220px;
  overflow-y: auto;
}
.hidden-drawer-hint {
  margin: 0 0 8px;
  font-size: 11px;
  line-height: 1.55;
  color: var(--text-dim);
}
.hidden-device-row {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 4px;
}
.hidden-device-row .device-text-col { flex: 1; min-width: 0; }

.status-chip.session { background: var(--info-soft); color: var(--info-on-soft); }
.status-chip.offline { background: var(--bg-hover-soft); color: var(--text-tertiary); }

/* 审批：三按钮 + 语义提示（§2.4 / §2.3.1） */
.btn-approval-trust {
  background: var(--success-soft);
  color: var(--success-on-soft);
  border: 1px solid var(--success-border);
  border-radius: var(--radius-md);
  padding: 7px 12px;
  cursor: pointer;
  font-size: 12px;
  display: inline-flex;
  align-items: center;
  gap: 5px;
}
.btn-approval-trust:hover { background: var(--success-soft-hover); }

/* 「N 分钟内免重复确认」（§2.3.1）
 *
 * 刻意用 **info** 而不是 success 那一档：`permanent` 才是"长期免打扰"，
 * 而这一档只是**临时的同类免确认**。给它上"成功色"会让它看起来
 * 和「允许并永久信任」一样重 —— 而它弱得多。
 */
.btn-approval-grant {
  background: var(--info-soft);
  color: var(--info-on-soft);
  /* ⚠️ 只许用**已定义**的令牌。`--info-border` / `--info-soft-hover`
   * 看起来完全合理（`--success-border` / `--success-soft-hover` 就存在），
   * 但 info 这一族**没有**这两个 —— 写上去浏览器会把它当成
   * `color: var(--info-border)` 的非法值，于是 border 整个消失，
   * **构建照样绿**。`--info` 同理：它只是前缀，
   * 真正存在的是 `--info-soft` / `--info-on-soft` / `--info-text`。
   * 令牌守卫 `no_undefined_css_tokens` 会红。 */
  border: 1px solid color-mix(in srgb, var(--info-text) 32%, transparent);
  border-radius: var(--radius-md);
  padding: 7px 12px;
  cursor: pointer;
  font-size: 12px;
  display: inline-flex;
  align-items: center;
  gap: 5px;
}
.btn-approval-grant:hover {
  background: color-mix(in srgb, var(--info-text) 14%, var(--info-soft));
}

/* 生效中的短期授权指示（§2.3.1 的「用户看得见」）
 *
 * 必须**明显**但不能抢眼：它的作用是让用户知道"这台设备现在不用码了"，
 * 并且**能点掉**。做成一个安静的小计时器图标而不是角标数字，
 * 因为它承载的是"可以撤销"这个动作，不是"剩余数量"这个状态。
 */
.grant-active-dot {
  margin-left: 3px;
  padding: 0 2px;
  /* 圆角必须用令牌：相邻容器差 1px 会被用户看成"圆角不一致"而不是层级 */
  border-radius: var(--radius-sm);
  font-size: 11px;
  color: var(--info-text);
  cursor: pointer;
}
.grant-active-dot:hover {
  background: var(--info-soft);
  color: var(--info-on-soft);
}
.approval-hint {
  margin: 10px 0 0;
  font-size: 11px;
  line-height: 1.6;
  color: var(--text-dim);
  display: flex;
  align-items: flex-start;
  gap: 5px;
}

/* 剪贴板预览（§6.4） */
.clipboard-preview-dialog { max-width: 560px; }
.clipboard-preview-body {
  margin: 10px 0 4px;
  border-radius: var(--radius-md);
  background: var(--bg-inset-strong);
  border: 1px solid var(--border-subtle);
  overflow: hidden;
}
.clipboard-preview-image {
  display: block;
  max-width: 100%;
  /* 长边限高: 4000px 的截图直接铺开会卡住 WebView */
  max-height: 320px;
  object-fit: contain;
  margin: 0 auto;
}
.clipboard-preview-text {
  margin: 0;
  padding: 12px;
  max-height: 300px;
  overflow: auto;
  font-size: 12px;
  line-height: 1.6;
  white-space: pre-wrap;
  word-break: break-word;
  font-family: ui-monospace, "Cascadia Mono", Consolas, monospace;
  color: var(--text-main);
}

/* 拖到设备卡片：目标即动作（§5.1） */
.device-card-row.drop-target {
  opacity: 1;
  outline: 2px solid var(--accent);
  outline-offset: -2px;
  background: var(--accent-soft);
  border-radius: var(--radius-md);
}
.device-card-row.drop-blocked {
  opacity: 0.5;
  outline: 2px dashed var(--border-strong);
  outline-offset: -2px;
  cursor: not-allowed;
}

/* 直发撤销 toast（D4：5 秒窗口） */
.undo-toast {
  display: flex;
  align-items: center;
  gap: 10px;
  pointer-events: auto;
}
.undo-toast-text { flex: 1; }
.undo-toast-btn {
  /* 底色用 --bg-elevated 而不是半透明叠层。
   *
   * 撤销 toast 浮在**窗口底色**上（不是卡片上），而 `--bg-press-soft`
   * 是"亮色叠在卡片上"那个量级：浅色主题下它叠在 #f3f5f9 上得到
   * 接近白的浅灰，`--text-on-solid`（纯白）压上去只有 1.33:1 ——
   * 按钮上的字**完全看不见**。而这是 5 秒内唯一能挽回误发的入口。
   *
   * 用实心的 --bg-elevated（两套主题都是卡片白 / 深灰）后，
   * 文字改用 --text-primary，两套主题分别是 15.9:1 与 12.6:1。
   * 代价是按钮从"半透明"变成"实心"——而它本来就需要比周围更实，
   * 否则用户不会认为它可点。 */
  background: var(--bg-elevated);
  border: 1px solid var(--border-strong);
  color: var(--text-primary);
  border-radius: var(--radius-md);
  padding: 4px 10px;
  cursor: pointer;
  font-size: 12px;
  font-weight: 600;
}
.undo-toast-btn:hover { background: var(--bg-press-strong); }
.undo-toast.committed { opacity: 0.85; }

/* 地址栏（§7.2）：卷下拉 + 完整路径 */
.pane-address-bar {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 0 10px 6px;
  font-size: 11.5px;
  color: var(--text-secondary);
  min-width: 0;
}
/* 自定义深色下拉位置组件（替代系统原生 <select>） */
.custom-dropdown-wrap {
  position: relative;
  display: inline-flex;
  align-items: center;
  flex: 0 0 auto;
}
.custom-dropdown-trigger {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  background: var(--bg-card);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-sm);
  padding: 2px 7px;
  color: var(--text-primary);
  font-size: 11.5px;
  font-family: inherit;
  cursor: pointer;
  outline: none;
  transition: all 0.15s ease;
  min-width: 96px;
  max-width: 175px;
}
.custom-dropdown-trigger:hover,
.custom-dropdown-trigger.is-active {
  background: var(--bg-hover-soft);
  border-color: var(--border-strong);
}
.custom-dropdown-trigger:disabled {
  opacity: 0.6;
  cursor: not-allowed;
}
.dropdown-trigger-text {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  text-align: left;
}
.dropdown-trigger-caret {
  font-size: 10px;
  color: var(--text-tertiary);
  transition: transform 0.2s ease;
}
.dropdown-trigger-caret.is-open {
  transform: rotate(180deg);
}

.custom-dropdown-panel {
  position: absolute;
  top: calc(100% + 4px);
  left: 0;
  z-index: var(--z-overlay);
  min-width: 210px;
  max-height: 280px;
  overflow-y: auto;
  background: var(--bg-card);
  backdrop-filter: blur(12px);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-md);
  box-shadow: var(--shadow-modal);
  padding: 4px;
}
.dropdown-menu-item {
  width: 100%;
  border: none;
  background: transparent;
  text-align: left;
  font-family: inherit;
  display: flex;
  align-items: center;
  gap: 7px;
  padding: 6px 8px;
  border-radius: var(--radius-sm);
  font-size: 12px;
  color: var(--text-primary);
  cursor: pointer;
  transition: background 0.12s ease;
}
.dropdown-menu-item:hover {
  background: var(--bg-hover-soft);
}
.dropdown-menu-item.is-selected {
  background: color-mix(in srgb, var(--accent) 15%, transparent);
  color: var(--accent-text, var(--accent));
  font-weight: 600;
}
.item-main-text {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.item-tag-text,
.item-sub-text {
  font-size: 10.5px;
  color: var(--text-tertiary);
  font-family: var(--font-mono, monospace);
}
.dropdown-group-header {
  padding: 6px 8px 2px;
  font-size: 10.5px;
  font-weight: 600;
  color: var(--text-tertiary);
  text-transform: uppercase;
  letter-spacing: 0.5px;
}
.address-text {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-family: var(--font-mono, ui-monospace, Consolas, monospace);
  color: var(--text-tertiary);
}

/* 分页加载（§7.3）：显式按钮而不是滚动到底自动加载 */
.load-more-btn {
  display: block;
  width: calc(100% - 24px);
  margin: 8px 12px;
  padding: 7px 0;
  background: var(--bg-card);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-md);
  color: var(--text-secondary);
  font-size: 12px;
  font-family: inherit;
  cursor: pointer;
}
.load-more-btn:hover:not(:disabled) { background: var(--bg-hover-soft); color: var(--text-primary); }
.load-more-btn:disabled { opacity: 0.5; cursor: default; }

/* 连接路径切换条（§3.9 / P4 ⑧） */
.endpoint-bar {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 6px;
  padding: 6px 2px 2px;
}
.endpoint-label {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  font-size: 11.5px;
  color: var(--text-tertiary);
  margin-right: 2px;
}
.endpoint-chip {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  padding: 3px 9px;
  font-size: 11.5px;
  font-family: inherit;
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-pill);
  background: var(--bg-card);
  color: var(--text-secondary);
  cursor: pointer;
  max-width: 240px;
}
.endpoint-chip:hover:not(:disabled) { background: var(--bg-hover-soft); color: var(--text-primary); }
.endpoint-chip.is-active {
  border-color: var(--accent-text);
  color: var(--accent-on-soft);
  background: color-mix(in srgb, var(--accent) 12%, transparent);
}
.endpoint-chip:disabled { opacity: 0.45; cursor: not-allowed; }
.endpoint-ip {
  font-family: var(--font-mono, ui-monospace, Consolas, monospace);
  font-size: 11px;
  color: var(--text-tertiary);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* 访问范围配置（需求 ④） */
.scope-open-btn {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 3px 9px;
  font-size: 11.5px;
  font-family: inherit;
  color: var(--text-tertiary);
  background: transparent;
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-pill);
  cursor: pointer;
}
.scope-open-btn:hover { color: var(--text-primary); background: var(--bg-hover-soft); }
/* 被收窄过时用暖色提醒 —— "这台设备看不到全部"是用户需要知道的状态，
   而默认（全部可见）不需要任何视觉噪音。 */
.scope-open-btn.is-warn {
  color: var(--warn-text);
  border-color: var(--warn-border);
}
.scope-open-btn.is-warn:hover { background: var(--warn-soft); }

.modal-mask {
  position: fixed;
  inset: 0;
  z-index: var(--z-overlay-nested);
  display: flex;
  align-items: center;
  justify-content: center;
  background: var(--overlay-scrim);
  backdrop-filter: blur(2px);
}
.scope-modal {
  width: min(560px, calc(100vw - 32px));
  max-height: calc(100vh - 64px);
  display: flex;
  flex-direction: column;
  background: var(--bg-card);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-lg);
  box-shadow: var(--shadow-modal);
  font-size: 13px;
  color: var(--text-primary);
}
.scope-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 12px 14px;
  border-bottom: 1px solid var(--border-subtle);
}
.scope-head h3 { margin: 0; font-size: 14px; font-weight: 600; }
.scope-head h3 i { margin-right: 5px; color: var(--text-tertiary); }
.scope-close {
  background: transparent;
  border: none;
  color: var(--text-tertiary);
  font-size: 15px;
  cursor: pointer;
  padding: 2px 5px;
  border-radius: var(--radius-sm);
}
.scope-close:hover { background: var(--bg-hover-soft); color: var(--text-primary); }

.scope-body { padding: 14px; overflow-y: auto; }
.scope-field { margin-bottom: 16px; }
.scope-field:last-child { margin-bottom: 0; }
.scope-label {
  display: block;
  font-size: 12px;
  font-weight: 600;
  color: var(--text-secondary);
  margin-bottom: 7px;
}
.scope-label-hint {
  display: block;
  font-weight: 400;
  font-size: 11px;
  color: var(--text-tertiary);
  margin-top: 2px;
}

.scope-modes {
  display: grid;
  grid-template-columns: repeat(4, 1fr);
  gap: 8px;
}
.scope-mode {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 6px;
  padding: 8px 10px;
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-md);
  cursor: pointer;
  background: var(--bg-hover-soft);
  transition: all 0.15s ease;
}
.scope-mode:hover {
  background: var(--bg-card-hover, var(--bg-elevated));
  border-color: var(--border-strong);
}
.scope-mode.is-on {
  border-color: var(--accent);
  background: color-mix(in srgb, var(--accent) 15%, transparent);
  color: var(--accent-text, var(--accent));
  font-weight: 600;
}
.scope-mode input {
  margin: 0;
  accent-color: var(--accent);
  cursor: pointer;
}
.scope-mode-title {
  font-size: 12.5px;
}

.scope-status-chip {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 10px 12px;
  background: color-mix(in srgb, var(--accent) 10%, transparent);
  border: 1px solid color-mix(in srgb, var(--accent) 25%, transparent);
  border-radius: var(--radius-md);
  font-size: 12.5px;
  color: var(--text-primary);
}
.scope-status-chip i {
  font-size: 18px;
  color: var(--accent-text, var(--accent));
}

.scope-sub-label {
  font-size: 12px;
  font-weight: 600;
  color: var(--text-secondary);
  margin-bottom: 6px;
}

.path-input-group {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-bottom: 8px;
}
.path-input-group .standard-text-input {
  flex: 1;
  font-size: 12px;
  padding: 6px 8px;
}

.path-tags-list {
  display: flex;
  flex-direction: column;
  gap: 4px;
  max-height: 140px;
  overflow-y: auto;
  padding-right: 4px;
  margin-top: 6px;
}
.path-tag-item {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 4px 8px;
  background: var(--bg-hover-soft);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-sm);
  font-size: 11.5px;
  font-family: var(--font-mono, monospace);
  color: var(--text-primary);
}
.path-tag-item.is-deny {
  border-color: var(--warn-border);
}
.path-tag-item.is-deny i {
  color: var(--warn-text);
}
.path-tag-text {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.path-tag-remove {
  background: transparent;
  border: none;
  color: var(--text-tertiary);
  cursor: pointer;
  padding: 2px;
  border-radius: var(--radius-sm);
  display: inline-flex;
}
.path-tag-remove:hover {
  color: var(--danger-text);
  background: var(--bg-hover-soft);
}

.quick-preset-row {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 6px;
  margin-bottom: 6px;
}
.quick-preset-label {
  font-size: 11px;
  color: var(--text-tertiary);
}
.quick-preset-btn {
  background: var(--bg-hover-soft);
  border: 1px dashed var(--border-subtle);
  color: var(--text-secondary);
  font-size: 11px;
  font-family: var(--font-mono, monospace);
  padding: 2px 7px;
  border-radius: var(--radius-pill);
  cursor: pointer;
  transition: all 0.15s ease;
}
.quick-preset-btn:hover {
  border-color: var(--accent);
  color: var(--text-primary);
}

.scope-toggles-row {
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.scope-toggle-clean {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 12.5px;
  cursor: pointer;
  color: var(--text-primary);
}
.scope-toggle-clean input {
  accent-color: var(--accent);
  cursor: pointer;
}

.scope-volumes { display: flex; flex-wrap: wrap; gap: 6px; }
.scope-vol {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  padding: 4px 9px;
  font-size: 12px;
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-pill);
  cursor: pointer;
}
.scope-vol:hover { background: var(--bg-hover-soft); }
.scope-vol.is-on {
  border-color: var(--accent-text);
  color: var(--accent-on-soft);
  background: color-mix(in srgb, var(--accent) 10%, transparent);
}
.scope-vol-free { font-size: 10.5px; color: var(--text-tertiary); }

.drop-sub-tip {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 5px;
  font-size: 12px;
  color: var(--text-tertiary);
  margin-top: 4px;
}

/* 「输入对方窗口上的匹配码」弹窗
 *
 * 复用 `.scope-head` / `.scope-foot` / `.scope-btn` —— 它们已经定义了
 * 弹层的边框、圆角、按钮形态。**形状一致性**：如果这个弹窗自己另立一套
 * 圆角/按钮样式，同一个应用里就会有两种弹窗长相，那比"样式不统一"更糟
 * 的是用户会以为是两个不同的功能。
 */
.code-prompt-modal {
  width: min(420px, calc(100vw - 32px));
  background: var(--bg-card);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-lg);
  box-shadow: var(--shadow-modal);
  font-size: 13px;
  color: var(--text-primary);
  overflow: hidden;
}
.code-prompt-body {
  padding: 14px;
}
.code-prompt-line {
  margin: 0 0 8px;
  line-height: 1.6;
}
/* 这一段是**唯一**必须在输入框上方说清楚的事：码在对方屏幕上。
 * 说得不够清楚，用户会低头在自己的窗口上找那个码。 */
.code-prompt-hint {
  margin: 0 0 10px;
  font-size: 12px;
  line-height: 1.6;
  color: var(--text-secondary);
}
.code-prompt-hint strong {
  color: var(--text-primary);
  font-weight: 600;
}
.code-prompt-reason {
  margin: 0 0 10px;
  padding: 7px 9px;
  font-size: 11.5px;
  line-height: 1.55;
  color: var(--warn-on-soft);
  background: var(--warn-soft);
  border-radius: var(--radius-md);
}
.code-prompt-field {
  display: block;
}
/* 标签在输入框**上方**，不是用 placeholder 代替。
 * placeholder 一消失，字段就没有名字了 —— 而这里字段名本身是信息
 * （"对方审批窗口上的码"，不是"随便输个 6 位"）。 */
.code-prompt-field-label {
  display: block;
  margin-bottom: 6px;
  font-size: 12px;
  font-weight: 600;
  color: var(--text-secondary);
}
.code-prompt-input {
  width: 100%;
  box-sizing: border-box;
  padding: 9px 10px;
  font-size: 22px;
  letter-spacing: 7px;
  /* 字距会把最后一个字符推出去，用等量内边距把它拉回来居中 */
  padding-left: calc(10px + 7px);
  text-align: center;
  font-family: var(--font-mono, ui-monospace, Consolas, monospace);
  background: var(--bg-window);
  color: var(--text-primary);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-md);
  outline: none;
}
.code-prompt-input:focus {
  border-color: var(--accent);
  box-shadow: 0 0 0 2px color-mix(in srgb, var(--accent) 25%, transparent);
}
.code-prompt-note {
  margin: 8px 0 0;
  font-size: 11.5px;
  line-height: 1.55;
  color: var(--text-tertiary);
}

/* 弹层底部操作条。
 *
 * 这两个类**早先完全没有 CSS** —— 模板里写了 `class="scope-btn primary"`
 * 却没定义，于是浏览器按原生 button 渲染：灰底、圆角、聚焦环，跟整个
 * 应用的设计语言完全不搭。缺样式不会有任何编译期或运行期报错，
 * 只会"看起来很丑"，所以必须在这里显式补齐。
 */
.scope-foot {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 11px 14px;
  border-top: 1px solid var(--border-subtle);
  background: var(--bg-card);
  /* 贴底圆角：底部操作条是弹层最后一个子元素，不继承圆角会漏出
   * 遮罩背景形成四个小缺口。
   *
   * ⚠️ 这里**写死数值**而不是 var(--radius-lg)：CSS 的多值简写
   * `0 0 X X` 里插不进变量（`0 0 var(--radius-lg) var(--radius-lg)`
   * 语法非法，var() 整体是一个值）。数值必须与 --radius-lg 保持
   * 一致（10px）—— 圆角收敛到四个令牌时，这一条是唯一手写值。 */
  border-radius: 0 0 10px 10px;
}
/* 左侧的"恢复默认"是次要操作，右边两个是主流程。
 * 用 margin-left:auto 把主按钮推到右边，而不是靠一堆空 div 撑。 */
.scope-foot .scope-btn.ghost:first-child { margin-right: auto; }
.scope-btn {
  appearance: none;
  font: inherit;
  font-size: 12.5px;
  line-height: 1;
  padding: 7px 14px;
  border-radius: var(--radius-md);
  border: 1px solid var(--border-subtle);
  background: transparent;
  color: var(--text-secondary);
  cursor: pointer;
  white-space: nowrap;
  transition: background 0.12s, border-color 0.12s, color 0.12s;
}
.scope-btn:hover { background: var(--bg-hover-soft); color: var(--text-primary); }
.scope-btn:focus-visible {
  outline: 2px solid var(--accent);
  outline-offset: 1px;
}
.scope-btn.primary {
  background: var(--accent);
  border-color: var(--accent);
  /* 必须是 --accent-contrast 而不是 --text-on-solid。
     浅色主题的 --accent 是 #0e9a6e，白字压上去只有 3.58:1 ——
     而这里正是「允许并信任」这类**要点击的按钮**，12px 文字看不清
     就等于用户不敢点。--accent-contrast 是与 --accent 配套的那一支
     （深绿 #042417 = 4.62:1），项目里另几处主按钮本来就是这么配的。 */
  color: var(--accent-contrast);
  font-weight: 600;
}
.scope-btn.primary:hover {
  filter: brightness(1.08);
  color: var(--text-on-solid);
}
.scope-btn:disabled { opacity: 0.5; cursor: not-allowed; }

/* 文件夹行的"整夹发送"按钮（§7.6）
 * 默认隐藏，悬停/选中时出现 —— 否则长列表右侧全是图标，很吵。 */
.dir-send-btn {
  flex: 0 0 auto;
  width: 20px;
  height: 20px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  border: none;
  border-radius: var(--radius-sm);
  background: transparent;
  color: var(--text-tertiary);
  cursor: pointer;
  opacity: 0;
  transition: opacity 0.12s;
}
.shuttle-file-line:hover .dir-send-btn,
.dir-send-btn.is-on,
.dir-send-btn:focus-visible {
  opacity: 1;
}
.dir-send-btn:hover { background: var(--bg-hover-soft); color: var(--text-primary); }
.dir-send-btn.is-on { background: var(--bg-active); color: var(--accent-on-soft); }
.dir-send-btn:disabled { opacity: 0.3; cursor: not-allowed; }

/* 「每次匹配码」审批面板（§2.3） */
.grant-code-panel {
  margin: 10px 0 4px;
  padding: 10px 12px;
  border: 1px solid var(--warn-border);
  background: var(--warn-soft);
  border-radius: var(--radius-md);
}
.grant-code-title {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 12.5px;
  font-weight: 600;
  color: var(--warn-text);
  margin-bottom: 6px;
}
.grant-code-hint {
  margin: 0 0 8px;
  font-size: 12px;
  line-height: 1.6;
  color: var(--text-secondary);
}
/* 本机生成、要念给对方的匹配码。
 *
 * 排版按"**念给别人听**"来设计，而不是按"好看"：
 *  · 字号给到 32px —— 隔着桌子念码时对方要能一次看清，
 *    20px 在 27 寸以外就开始费劲，而看清一次就省掉一次输错重试；
 *  · 等宽 + 大字距 —— "123 456" 的两个 3 位不能看成 "1234 56"；
 *  · **禁止选中**（user-select: none）—— 用户双击选中想复制时，
 *    没有任何用途（对面要的是"念"不是"贴"），只会误触全选样式。
 */
.grant-code-display {
  margin: 4px 0 8px;
  padding: 12px 10px;
  font-size: 32px;
  font-weight: 700;
  line-height: 1.1;
  letter-spacing: 8px;
  /* 字距会把最后一个字符也推出去，用等量内边距把它拉回来居中 */
  padding-left: calc(10px + 8px);
  text-align: center;
  font-family: var(--font-mono, ui-monospace, Consolas, monospace);
  color: var(--text-primary);
  background: var(--bg-card);
  border: 1px solid var(--warn);
  border-radius: var(--radius-md);
  user-select: none;
}
.grant-code-error {
  margin: 8px 0 0;
  font-size: 12px;
  line-height: 1.5;
  color: var(--danger-text);
}

.approval-actions button:disabled {
  opacity: 0.45;
  cursor: not-allowed;
  filter: none;
}

.header-actions { display: flex; align-items: center; gap: 8px; }
.entry-speed-text {
  font-variant-numeric: tabular-nums;
  font-size: 12px;
  color: var(--success-text);
  min-width: 62px;
  text-align: right;
}
.entry-speed-text.unknown { color: var(--text-dim); }
.entry-sub.dimmed { opacity: 0.7; font-size: 11px; }
.link-tag {
  display: inline-block;
  margin-left: 6px;
  padding: 0 5px;
  font-size: 10px;
  border-radius: var(--radius-sm);
  background: var(--bg-hover-soft);
  color: var(--text-tertiary);
}

.sidebar-pinned-footer {
  padding: 12px;
  border-top: 1px solid var(--border-subtle);
  display: flex;
  flex-direction: column;
  gap: 10px;
  background: var(--bg-sidebar);
}
.btn-open-pairing {
  width: 100%;
  height: 38px;
  border-radius: var(--radius-md);
  background: var(--bg-card);
  border: 1px solid var(--border-subtle);
  color: var(--text-primary);
  font-size: 12.5px;
  font-weight: 650;
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 8px;
  cursor: pointer;
  transition: background 0.15s, border-color 0.15s, color 0.15s;
}
.btn-open-pairing:hover {
  background: var(--bg-card-hover);
  border-color: var(--accent-text);
  color: var(--accent-text);
}
.save-dir-card { font-size: 11px; }
.dir-header { display: flex; justify-content: space-between; color: var(--text-tertiary); margin-bottom: 3px; }
/* 原来是 `<a href="javascript:void(0)">`: 那是**假链接** ——
 * 没有真实目的地, 却带着链接的全部视觉暗示(可点、有 hover、
 * 中键能开新页)。屏幕阅读器会把它念成链接, 用户按 Enter 却什么
 * 都不会发生。换成真 <button>, 语义和视觉对得上。
 * 样式从 `.dir-header a` 搬过来, 视觉零变化。 */
.dir-open-btn {
  background: none;
  border: 0;
  padding: 0;
  color: var(--accent-text);
  font: inherit;
  font-weight: 600;
  cursor: pointer;
}
.dir-open-btn:hover { text-decoration: underline; }
.dir-path-string {
  color: var(--text-secondary);
  word-break: break-all;
  line-height: 1.35;
  font-size: 10.5px;
  cursor: pointer;
}
.dir-path-string:hover { color: var(--accent-text); }

/* 5. 右侧工作区 */
.app-workspace {
  display: grid;
  /* `auto` 而不是固定 46px：顶部这一行会**换行**（见 .workspace-tabs-bar），
   * 固定高度会把第二行裁掉 —— 父级是 overflow:hidden，裁掉的部分用户
   * 完全看不到，表现为"某个按钮凭空消失"。minmax 保住最小观感高度。 */
  grid-template-rows: minmax(46px, auto) 1fr;
  min-height: 0;
  height: 100%;
  background: var(--bg-window);
  overflow: hidden;
}

.workspace-tabs-bar {
  display: flex;
  /* 允许换行。这是"选中设备后内容被挤压"的正解：
   * 选中设备会让右侧多出「目标 + 设备名 + 信任徽章 + 访问范围 + 连接路径」
   * 好几个元素，窗口不够宽时**整块换到第二行**，而不是把每个元素都压扁。
   * 不换行的话 flex 会去压缩每个子项，文字被逐字折行
   * （截图里「已信任」竖着排成三行就是这么来的）。 */
  flex-wrap: wrap;
  justify-content: space-between;
  align-items: center;
  /* 换行后第二行不能贴着第一行，视觉上会像两排挤在一起 */
  row-gap: 6px;
  padding: 6px 20px;
  border-bottom: 1px solid var(--border-subtle);
  column-gap: 12px;
}
/* 标签栏永远不参与压缩：它被压扁是最伤的一种表现（按钮里的字被切掉），
 * 而窗口变窄时让右侧的目标块换行就好看得多。 */
.segmented-tabs { display: flex; gap: 4px; flex: 0 0 auto; }
.segmented-tabs button {
  height: 32px;
  padding: 0 14px;
  border-radius: var(--radius-md);
  background: transparent;
  border: 0;
  color: var(--text-secondary);
  font-size: 12.5px;
  font-weight: 600;
  cursor: pointer;
  display: flex;
  align-items: center;
  gap: 6px;
  transition: background 0.15s, color 0.15s;
}
.segmented-tabs button:hover { background: var(--bg-hover-soft); color: var(--text-primary); }
.segmented-tabs button.active {
  background: var(--bg-card);
  color: var(--text-primary);
  box-shadow: var(--shadow-card);
}
.current-target-pill {
  display: inline-flex;
  align-items: center;
  gap: 8px;
  font-size: 12px;
  background: var(--bg-card);
  padding: 4px 10px;
  border-radius: var(--radius-md);
  border: 1px solid var(--border-subtle);
  /* 不再限死 45%：外层已经能换行了，硬上限只会逼得这一块自己内部折行。
   * 留一个上限是为了防止「系统设置」页签下设备名超长时把整行撑爆。 */
  max-width: min(60%, 720px);
  /* 块内也允许换行：设备名 + 徽章 + 访问范围 + 连接路径在窄窗口里
   * 必然放不下，让它们换行比压扁好。 */
  flex-wrap: wrap;
  row-gap: 4px;
}
/* 徽章、按钮、设备名都不参与压缩。
 * flex 子项默认 `min-width: auto`，被压缩时不会缩到内容宽度以下，
 * 而是**溢出**——但在 nowrap + 固定 padding 的组合下，实际表现是
 * 文本被逐字折行（「已信任」竖排三行）。显式 nowrap + 不收缩即可根治。 */
.target-prefix,
.target-name,
.status-chip,
.scope-open-btn {
  flex: 0 0 auto;
  white-space: nowrap;
}
.target-prefix { color: var(--text-tertiary); font-size: 11px; }
.target-name {
  color: var(--text-primary);
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  /* 设备名是这一块里唯一"可以无限长"的内容，给它 min-width:0 才能真正
   * 触发 ellipsis（否则它会顶着 min-width:auto 拒绝收缩）。 */
  min-width: 0;
}
.status-chip {
  font-size: 10.5px;
  padding: 2px 7px;
  border-radius: var(--radius-sm);
  font-weight: 600;
  border: 0;
}
.status-chip.trusted { background: var(--accent-soft); color: var(--accent-on-soft); }
.status-chip.untrusted { background: var(--warn-soft); color: var(--warn-on-soft); cursor: pointer; }
.status-chip.untrusted:hover { filter: brightness(1.1); }

.tab-page {
  padding: 20px 24px;
  overflow-y: auto;
  height: 100%;
  box-sizing: border-box;
}

/* 6. 发送页 */
.view-send { display: flex; flex-direction: column; gap: 16px; }
.drag-drop-card {
  flex: 1;
  min-height: 250px;
  border: 2px dashed var(--drop-border);
  border-radius: var(--radius-lg);
  background: var(--drop-bg);
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  text-align: center;
  padding: 24px;
  transition: border-color 0.2s, background 0.2s;
}
.drag-drop-card:hover, .drag-drop-card.is-dragging {
  border-color: var(--accent);
  background: var(--drop-bg-hover);
}
.drop-illustration-icon {
  width: 56px;
  height: 56px;
  border-radius: var(--radius-lg);
  background: var(--accent-soft);
  color: var(--accent-on-soft);
  display: grid;
  place-items: center;
  font-size: 26px;
  margin-bottom: 12px;
}
.drag-drop-card h3 { font-size: 18px; font-weight: 700; margin-bottom: 10px; color: var(--text-primary); }
/* 规则行：图标 + 一句话，`b` 只加粗规则里那个关键词。
 * 不用彩色徽章 —— 两条规则是并列的，一条特殊一条不特殊会暗示它们等级不同。 */
.drop-rule {
  display: flex;
  align-items: baseline;
  justify-content: center;
  gap: 7px;
  margin: 0 0 5px;
  font-size: 12.5px;
  line-height: 1.5;
  color: var(--text-secondary);
}
.drop-rule i {
  color: var(--text-tertiary);
  font-size: 13px;
  /* 图标是 inline 元素，baseline 对齐会让它比文字低一点点 */
  transform: translateY(1px);
}
.drop-rule b {
  color: var(--text-primary);
  font-weight: 600;
}
.drop-hint {
  font-size: 12px;
  color: var(--text-tertiary);
  max-width: 46ch;
  line-height: 1.5;
  margin: 10px 0 18px;
}
.drop-cta-buttons { display: flex; gap: 12px; flex-wrap: wrap; justify-content: center; }

.btn-fluent-primary {
  background: var(--accent);
  color: var(--accent-contrast);
  font-weight: 700;
  border: 0;
  border-radius: var(--radius-md);
  padding: 9px 20px;
  font-size: 13px;
  display: inline-flex;
  align-items: center;
  gap: 8px;
  cursor: pointer;
  transition: filter 0.15s, transform 0.1s;
}
.btn-fluent-primary:hover { filter: brightness(1.08); }
.btn-fluent-primary:active { transform: scale(0.98); }
.btn-fluent-primary:disabled { opacity: 0.45; cursor: not-allowed; }

.btn-fluent-secondary {
  background: var(--bg-card);
  color: var(--text-primary);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-md);
  padding: 9px 18px;
  font-size: 13px;
  font-weight: 600;
  display: inline-flex;
  align-items: center;
  gap: 8px;
  cursor: pointer;
  transition: background 0.15s, border-color 0.15s;
}
.btn-fluent-secondary:hover { background: var(--bg-card-hover); border-color: var(--border-strong); }
.btn-fluent-secondary:disabled { opacity: 0.5; cursor: not-allowed; }

.btn-danger-solid {
  /* 必须用 --danger-solid 而不是 --danger: 白字压 #ff6b6b(深色主题的
     --danger) 只有 2.78:1, 按钮文字几乎看不清。--danger-solid 4.69:1。 */
  background: var(--danger-solid);
  color: var(--text-on-solid);
  border: 0;
  border-radius: var(--radius-md);
  padding: 9px 18px;
  font-size: 13px;
  font-weight: 650;
  display: inline-flex;
  align-items: center;
  gap: 8px;
  cursor: pointer;
  transition: filter 0.15s;
}
.btn-danger-solid:hover { filter: brightness(1.08); }

.staged-file-tray {
  background: var(--bg-card);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-lg);
  padding: 14px 16px;
}
.tray-title-bar {
  display: flex;
  justify-content: space-between;
  align-items: center;
  font-size: 12.5px;
  font-weight: 650;
  color: var(--text-primary);
  margin-bottom: 10px;
}
.clear-all-link { background: none; border: 0; color: var(--danger-text); cursor: pointer; font-size: 12px; }
.clear-all-link:hover { text-decoration: underline; }
/*
  落点行：紧贴标题栏，比标题小一档、字重更轻 ——
  它是"发出去会怎样"的说明，不是清单本身的内容。
  用 --text-secondary 而不是 --danger/警告色：落点不同是**正常**行为
  （穿梭拖文件夹 vs 拖进窗口就该落在不同地方），不是错误。
  染成警告色等于每天都在报警。
*/
.tray-dest-line {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  color: var(--text-secondary);
  margin: -4px 0 8px;
  min-width: 0;
}
.tray-dest-line i { flex: none; opacity: 0.8; }
.tray-dest-line span {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.staged-scroll-rows { max-height: 130px; overflow-y: auto; display: flex; flex-direction: column; gap: 6px; }
.staged-row {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 7px 12px;
  background: var(--bg-row-soft);
  border-radius: var(--radius-md);
  font-size: 12.5px;
}
.row-fname { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--text-primary); }
/* 目录的「N 个文件」：放在名字后面而不是大小那一栏。
   大小那栏留给真实字节数（4.0 KB 是目录项自身，17.8 MB 才是要发的），
   两个信息挤在一处会让人以为"4.0 KB 之外还有别的"。 */
.row-dir-count {
  margin-left: 6px;
  padding: 0 6px;
  font-size: 10.5px;
  font-style: normal;
  color: var(--text-secondary);
  background: var(--bg-inset);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-pill);
  white-space: nowrap;
}
.row-fsize { color: var(--text-secondary); font-size: 11px; }
.row-remove-btn {
  background: none;
  border: 0;
  color: var(--text-tertiary);
  cursor: pointer;
  font-size: 14px;
  padding: 0 2px;
}
.row-remove-btn:hover { color: var(--danger-text); }
.tray-action-bottom { margin-top: 12px; }
.execute-send-btn { width: 100%; height: 42px; justify-content: center; }

/* 7. 双栏穿梭 */
.view-shuttle { display: grid; grid-template-columns: 1fr 132px 1fr; gap: 16px; height: 100%; }
.shuttle-side-pane {
  background: var(--bg-card);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-lg);
  display: flex;
  flex-direction: column;
  overflow: hidden;
}
.shuttle-pane-header {
  padding: 10px 14px;
  background: var(--bg-card-hover);
  border-bottom: 1px solid var(--border-subtle);
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 8px;
}
.pane-name {
  font-size: 12px;
  font-weight: 700;
  display: flex;
  align-items: center;
  gap: 6px;
  color: var(--text-primary);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
/* 设备名/卷标：必须是**独立的元素**，不能是 .pane-name 的裸文本。
 *
 * `text-overflow: ellipsis` 只作用于元素自己的 inline 内容。父元素一旦是
 * flex 容器，裸文本会被拆成匿名 flex item —— 父元素上的 ellipsis 完全失效，
 * 而匿名 flex item 的 `min-width: auto` 又禁止它收缩，于是长设备名把后面的
 * 角标挤出容器、再被 .pane-name 的 `overflow: hidden` 裁掉。
 *
 * 四象限实测（规则从本文件原样抽取，容器右边界 x=310，看 .pane-count 右缘）：
 *   A 改动前：裸文本、count 无 flex          → 438，角标不可见
 *   B 只给 .pane-count 加 flex:0 0 auto      → 438，**仍然不可见**
 *   C 只把文本包进 span、count 仍无 flex     → 271，角标可见
 *   D 两者都做（= 现在这份）                 → 271，角标可见
 * B 与 A 完全一致这一点很关键：真正起作用的只有「包一层元素」，
 * flex 属性单独加是**无效**的。所以下面 `.pane-count` 的 flex:0 0 auto
 * 不能被当成本缺陷的修复依据，它只是防将来再加一个可伸缩兄弟节点时的
 * 纵深防御 —— 今天它对结果没有任何影响。
 *
 * `min-width: 0` 不能省：省略号的前提是元素肯收缩，而 flex 子项默认
 * `min-width: auto`（= 内容宽度）会拒绝收缩。 */
.pane-name-text {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.pane-name > i { flex: 0 0 auto; }
.pane-count {
  font-size: 10px;
  background: var(--bg-active);
  color: var(--accent-on-soft);
  padding: 1px 6px;
  border-radius: var(--radius-pill);
  font-weight: 700;
  /* 纵深防御，不是上面那个缺陷的修复手段（四象限 B 已证明它单独无效） */
  flex: 0 0 auto;
}
.pane-refresh-btn {
  background: none;
  border: 0;
  color: var(--text-tertiary);
  cursor: pointer;
  font-size: 14px;
  padding: 3px;
  border-radius: var(--radius-sm);
  flex-shrink: 0;
}
.pane-refresh-btn:hover { color: var(--accent-text); background: var(--bg-hover-soft); }

/* ===== 对端目录面包屑 (子文件夹导航) ===== */
.pane-breadcrumb {
  display: flex;
  align-items: center;
  gap: 2px;
  padding: 5px 10px;
  background: var(--bg-card);
  border-bottom: 1px solid var(--border-subtle);
  font-size: 11px;
  overflow-x: auto;
  scrollbar-width: none;
}
.pane-breadcrumb::-webkit-scrollbar { display: none; }
.crumb-up,
.crumb-item {
  background: none;
  border: 0;
  color: var(--text-secondary);
  cursor: pointer;
  font-size: 11px;
  padding: 2px 6px;
  border-radius: var(--radius-sm);
  white-space: nowrap;
  font-family: inherit;
}
.crumb-up { color: var(--text-tertiary); flex-shrink: 0; }
.crumb-up:hover:not(:disabled),
.crumb-item:hover { color: var(--accent-text); background: var(--bg-hover-soft); }
.crumb-up:disabled { opacity: 0.35; cursor: default; }
.crumb-item.is-current { color: var(--text-primary); font-weight: 700; cursor: default; }
.crumb-sep { color: var(--text-tertiary); font-size: 9px; flex-shrink: 0; }
.pane-truncated-hint {
  display: flex;
  align-items: center;
  gap: 5px;
  padding: 5px 12px;
  font-size: 11px;
  color: var(--warn-on-soft);
  background: var(--warn-soft);
  border-bottom: 1px solid var(--border-subtle);
}
.shuttle-items-scroll {
  flex: 1;
  overflow-y: auto;
  padding: 6px;
  display: flex;
  flex-direction: column;
  gap: 2px;
  /* 拖拽落点的可见反馈。
   *
   * 为什么必须**可见**：一个看不见的放置区等于没有 —— 用户会把文件拖到
   * 面板中间、松手、什么都没发生，然后以为功能坏了。而 HTML5 拖放**没有**
   * "松手后被拒绝"这回事（不像把文件拖进回收站会变图标），所以唯一能让
   * 用户确认"这里能放"的就是拖拽过程中的高亮。
   *
   * 用 inset box-shadow 而不是 border：加 border 会把整列文件挤动 2px
   * （grid 行高由内容决定），拖拽过程中列表会抖一下。
   */
  border-radius: var(--radius-md);
  transition: box-shadow 0.12s, background 0.12s;
}
.shuttle-items-scroll.is-drop-armed {
  box-shadow: inset 0 0 0 2px var(--accent);
  background: color-mix(in srgb, var(--accent) 7%, transparent);
}
/* 行本身也给一点反馈：被拖起的那一行压暗，避免和"落点高亮"抢注意力 */
.shuttle-file-line.is-drop-armed {
  opacity: 0.55;
}
.shuttle-file-line[draggable="true"] {
  cursor: grab;
}
.shuttle-file-line[draggable="true"]:active {
  cursor: grabbing;
}
.shuttle-file-line {
  display: grid;
  /* 穿梭面板只有 ~250px 宽, 塞不下"名称 + 大小 + 完整日期"三列 ——
     实测名称只剩 28px, 变成 "项..." 这种没法辨认的截断。
     这里只保留"勾选 + 图标 + 名称 + 大小"(与资源管理器紧凑视图一致),
     修改时间并入名称的 tooltip。 */
  grid-template-columns: 18px 20px minmax(0, 1fr) auto;
  gap: 8px;
  align-items: center;
  padding: 7px 10px;
  border-radius: var(--radius-md);
  font-size: 12.5px;
  cursor: pointer;
  transition: background 0.15s;
}
.shuttle-file-line:hover { background: var(--bg-hover-soft); }
.shuttle-file-line.is-selected { background: var(--bg-active); }
.shuttle-file-line.is-disabled { cursor: not-allowed; opacity: 0.55; }

/* ---- 骨架加载 ----
 *
 * 形状与 `.shuttle-file-line` 完全一致（同样的 grid 列定义），只是把
 * 文字/图标换成一块占位色 —— 这样文件加载完成时列表不"跳"。
 *
 * 骨架本身**不可聚焦、不响应指针**：它不是内容，只是"正在取"的提示。
 * 模板里已加 `aria-hidden`，样式上再压掉指针事件，避免用户去点一个
 * 还不存在的文件。
 */
.shuttle-skeleton-stack {
  display: flex;
  flex-direction: column;
  pointer-events: none;
  user-select: none;
}
.shuttle-file-line.is-skeleton { cursor: default; }
.shuttle-file-line.is-skeleton:hover { background: none; }
.skeleton-glyph { opacity: 0.28; }
.skeleton-block {
  display: block;
  height: 9px;
  border-radius: var(--radius-sm);
  background: var(--bg-hover-soft);
  /* 脉动而非闪烁: 亮度缓慢起伏比高频闪烁刺眼得多, 长时间盯着也不累。
   * prefers-reduced-motion 下停掉 —— 见 style.css 的降级块。 */
  animation: skeleton-breathe 1.6s ease-in-out infinite;
}
.check-box-square.skeleton-block { height: 15px; }
@keyframes skeleton-breathe {
  0%, 100% { opacity: 0.45; }
  50% { opacity: 0.85; }
}
.check-box-square {
  width: 15px;
  height: 15px;
  border: 1.5px solid var(--border-strong);
  border-radius: var(--radius-sm);
  display: grid;
  place-items: center;
  font-size: 10px;
  color: var(--accent-text);
}
.file-glyph { font-size: 16px; color: var(--text-secondary); }
.file-glyph.dir { color: var(--warn-text); }
.file-glyph.pdf { color: var(--danger-text); }
.file-glyph.word { color: var(--info-text); }
.file-glyph.img { color: var(--green-text); }
.file-glyph.video { color: var(--warn-text); }
.file-glyph.zip { color: var(--purple-text); }
.file-glyph.audio { color: var(--purple-text); }
.file-label-col {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--text-primary);
}
.file-size-col { color: var(--text-secondary); font-size: 11px; margin-right: 6px; white-space: nowrap; }
.shuttle-empty-tip { padding: 30px 16px; text-align: center; color: var(--text-tertiary); font-size: 12px; line-height: 1.6; }
.shuttle-busy-hint {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 6px;
  padding: 12px;
  margin: 6px;
  font-size: 11.5px;
  color: var(--text-secondary);
  background: var(--accent-soft);
  border-radius: var(--radius-md);
}

.shuttle-control-divider { display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 12px; }
/* 拖拽提示：必须有。
 * HTML5 拖放没有"松手被拒绝"的反馈，而穿梭框的拖拽**平时完全不可见** ——
 * 用户不知道能拖，就永远只会用中间那两个按钮，于是新加的拖拽等于白做。
 * 一行字换一条可发现的交互路径。 */
.shuttle-drag-hint {
  margin: 0;
  max-width: 15ch;
  text-align: center;
  font-size: 11px;
  line-height: 1.45;
  color: var(--text-tertiary);
}
.shuttle-drag-hint strong {
  color: var(--text-secondary);
  font-weight: 600;
}
.shuttle-op-button {
  width: 100%;
  height: 52px;
  border-radius: var(--radius-md);
  font-size: 12px;
  font-weight: 700;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 2px;
  cursor: pointer;
  border: 0;
  transition: filter 0.15s, background 0.15s;
}
.shuttle-op-button.send-op { background: var(--accent); color: var(--accent-contrast); }
.shuttle-op-button.fetch-op { background: var(--bg-card); color: var(--text-primary); border: 1px solid var(--border-subtle); }
.shuttle-op-button:disabled { opacity: 0.4; cursor: not-allowed; }
.shuttle-op-button:not(:disabled):hover { filter: brightness(1.08); }

/* 8. 记录 / 设置面板 */
.fluent-card-panel {
  background: var(--bg-card);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-lg);
  padding: 22px 24px;
}
.panel-header-row { display: flex; justify-content: space-between; align-items: flex-start; margin-bottom: 6px; }
/* 选择器跟着标签走：h3→h2 之后这里若还写 h3，整条规则静默失效，
 * 16px 会退回浏览器默认的 1.5em，字会突然变大。 */
.header-titles h2 { font-size: 16px; font-weight: 700; color: var(--text-primary); margin-bottom: 4px; }
.header-titles p { font-size: 12px; color: var(--text-secondary); }
.btn-clear-history {
  background: none;
  border: 0;
  color: var(--text-tertiary);
  cursor: pointer;
  font-size: 12px;
  display: flex;
  align-items: center;
  gap: 4px;
}
.btn-clear-history:hover { color: var(--danger-text); }

.log-entries-list { display: flex; flex-direction: column; gap: 8px; margin-top: 14px; }
.log-entry-row {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 10px 14px;
  background: var(--bg-row-soft);
  border-radius: var(--radius-md);
}
.direction-badge { width: 34px; height: 34px; border-radius: var(--radius-md); display: grid; place-items: center; font-size: 16px; }
.direction-badge.recv { background: var(--accent-soft); color: var(--accent-on-soft); }
.direction-badge.send { background: var(--info-soft); color: var(--info-on-soft); }
.log-body-info { flex: 1; min-width: 0; }
.entry-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--text-primary);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.entry-sub { font-size: 11px; color: var(--text-secondary); margin-top: 2px; }
.entry-meta-col { text-align: right; flex-shrink: 0; }
.entry-size-text { font-size: 11.5px; color: var(--text-secondary); display: block; }
.entry-status-badge { font-size: 10.5px; color: var(--accent-text); font-weight: 600; }
.entry-status-badge.failed { color: var(--danger-text); }
.empty-log-box {
  padding: 40px;
  text-align: center;
  color: var(--text-tertiary);
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 8px;
  font-size: 13px;
}
.empty-log-box i { font-size: 32px; }

/* ---- 最近诊断（默认折叠的次级小节）----
 *
 * 视觉层级刻意**低于**上面的传输记录：它是排障入口，不是主内容。
 * 所以用分隔线而不是卡片，标题不带强调色，行距也比记录更紧。
 */
.diag-section {
  margin-top: 14px;
  padding-top: 12px;
  border-top: 1px solid var(--border-subtle);
}
.diag-section-toggle {
  display: flex;
  align-items: center;
  gap: 7px;
  width: 100%;
  background: none;
  border: 0;
  padding: 2px 0;
  font: inherit;
  font-size: 12.5px;
  color: var(--text-secondary);
  cursor: pointer;
  text-align: left;
}
.diag-section-toggle:hover { color: var(--text-primary); }
.diag-list {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin-top: 10px;
  max-height: 320px;
  overflow-y: auto;
}
.diag-row {
  padding: 9px 11px;
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-md);
  background: var(--bg-row-soft);
}
.diag-row-head {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 12.5px;
  color: var(--text-primary);
  min-width: 0;
}
.diag-peer {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.diag-speed {
  font-variant-numeric: tabular-nums;
  color: var(--success-text);
  font-weight: 600;
}
/*
  归因用 --text-secondary 而不是 --text-primary：
  它是**解释**，不是**结果**。做成和结果一样重，用户会以为那才是状态。
  左边留一条竖线把它和结果分开，扫读时一眼能分清"是什么"与"为什么"。
*/
.diag-attribution {
  margin-top: 5px;
  padding-left: 9px;
  border-left: 2px solid var(--accent-border);
  font-size: 12px;
  line-height: 1.55;
  color: var(--text-secondary);
}
.diag-error {
  margin-top: 4px;
  padding-left: 9px;
  font-size: 12px;
  line-height: 1.5;
  color: var(--danger-text);
}
.diag-phases {
  display: flex;
  flex-wrap: wrap;
  gap: 4px 10px;
  margin-top: 6px;
  font-size: 11px;
  color: var(--text-tertiary);
  font-variant-numeric: tabular-nums;
}

.setting-item-block {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 20px;
  padding: 15px 0;
  border-bottom: 1px solid var(--border-subtle);
}
.setting-desc-text { min-width: 0; flex: 1; }
.setting-desc-text strong { display: block; font-size: 13px; font-weight: 650; color: var(--text-primary); }
.setting-desc-text span {
  font-size: 12px;
  color: var(--text-secondary);
  margin-top: 3px;
  display: block;
  max-width: 56ch;
  line-height: 1.45;
}
.path-mono { color: var(--accent-text) !important; font-size: 11.5px !important; word-break: break-all; }
.setting-action-group { display: flex; gap: 8px; flex-shrink: 0; align-items: center; }
.name-input { width: 220px; }
.saved-hint {
  display: inline-flex;
  align-items: center;
  gap: 3px;
  font-size: 11px;
  color: var(--accent-text);
  white-space: nowrap;
}
.channel-badge {
  display: inline-block !important;
  font-size: 11px !important;
  padding: 1px 6px;
  border-radius: var(--radius-sm);
  margin-left: 6px;
  font-weight: 600;
  vertical-align: middle;
}
.badge-installed {
  background: var(--success-soft);
  color: var(--success-on-soft) !important;
}
.badge-portable {
  background: var(--warn-soft);
  color: var(--warn-on-soft) !important;
}
.setting-sub-hint {
  font-size: 11.5px !important;
  color: var(--text-tertiary) !important;
  margin-top: 3px;
  display: block;
}
.warn-hint {
  color: var(--warn-text) !important;
}

.panel-section-divider {
  padding: 18px 0 4px 0;
  border-top: 1px solid var(--border-subtle);
  margin-top: 10px;
}
.panel-section-divider.first { border-top: 0; padding-top: 10px; }
.panel-section-divider h3 { font-size: 13px; font-weight: 700; color: var(--text-secondary); margin: 0; }
/* 视觉隐藏但保留在无障碍树里的视图页标题。
 *
 * 发送 / 穿梭这两个视图没有可见页标题（页签名已经在页签上写着，
 * 再抄一遍是重复信息），但缺了它，按标题跳转的用户就只能在
 * 「传输记录」和「系统设置」之间跳 —— 会以为另外两个页不存在。
 *
 * 用 clip-path 而不是 display:none / visibility:hidden：后两者
 * 会把元素从无障碍树里摘掉，那就白加了。1px + 溢出隐藏是
 * 视觉隐藏的标准做法，不会被看到也不会被读到两次。 */
.view-heading-sr {
  position: absolute;
  width: 1px;
  height: 1px;
  margin: -1px;
  padding: 0;
  overflow: hidden;
  clip-path: inset(50%);
  white-space: nowrap;
  border: 0;
}

.fluent-switch { position: relative; display: inline-block; width: 44px; height: 24px; flex-shrink: 0; }
.fluent-switch input { opacity: 0; width: 0; height: 0; }
.fluent-slider {
  position: absolute;
  cursor: pointer;
  inset: 0;
  background: var(--switch-off);
  border-radius: var(--radius-pill);
  transition: 0.2s;
}
.fluent-slider:before {
  position: absolute;
  content: "";
  height: 18px;
  width: 18px;
  left: 3px;
  bottom: 3px;
  background: var(--switch-knob);
  /* 浅色主题下关闭态轨道是浅灰, 白滑块必须靠描边才界定得清楚。
     深色主题该值是全透明, 视觉零变化。 */
  box-shadow: 0 0 0 1px var(--switch-knob-ring);
  border-radius: 50%;
  transition: 0.2s;
}
input:checked + .fluent-slider { background: var(--accent); }
input:checked + .fluent-slider:before { transform: translateX(20px); }

.trust-table-wrap {
  margin-top: 10px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.empty-trust-hint {
  display: flex;
  align-items: center;
  gap: 8px;
  color: var(--text-tertiary);
  font-size: 12px;
  padding: 12px;
  background: var(--bg-row-soft);
  border-radius: var(--radius-md);
  line-height: 1.5;
}
.trust-item-row {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 10px 14px;
  background: var(--bg-row-soft);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-md);
}
.trust-meta { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
.trust-meta .dev-name { font-size: 13px; color: var(--text-primary); }
.trust-meta .dev-ip { font-size: 11px; color: var(--text-tertiary); word-break: break-all; }
.font-mono { font-family: var(--font-mono); }
.btn-unblock {
  background: transparent;
  border: 1px solid var(--border-subtle);
  color: var(--text-secondary);
  padding: 4px 10px;
  border-radius: var(--radius-sm);
  font-size: 12px;
  cursor: pointer;
  flex-shrink: 0;
  transition: color 0.15s, border-color 0.15s;
}
.btn-unblock:hover { color: var(--text-primary); border-color: var(--accent); }
.select-compact,
.fluent-select-wrapper.select-compact {
  width: 220px;
  flex: 0 0 220px;
}
.pair-select-wrapper {
  width: 100%;
}
.danger-action { color: var(--danger-text); }
.danger-action:hover { background: var(--danger-soft); color: var(--danger-on-soft); border-color: var(--danger-border); }

/* 9. 应用内更新: 进度条 + 失败态 */
.update-block .setting-action-group {
  align-items: center;
  gap: 10px;
}
/* 固定宽度而不是 flex:1 —— 否则进度条会把并排的按钮挤变形 */
.update-progress-track {
  width: 96px;
  height: 4px;
  border-radius: var(--radius-pill);
  background: var(--border-subtle);
  overflow: hidden;
  flex: 0 0 auto;
}
.update-progress-fill {
  height: 100%;
  width: 100%;
  background: var(--accent);
  /* 同 .speed-dock-fill: 用合成层而不是布局属性 */
  transform-origin: left center;
  transform: scaleX(0);
  transition: transform 0.2s ease;
  /* 这里**刻意不设** border-radius。
   *
   * `scaleX` 是**非等比**缩放: 圆角会跟着横向拉伸 —— 进度 10% 时
   * 胶囊的左端被压成椭圆, 看起来像渲染出错。圆角必须留在 track 上,
   * 由它的 `overflow: hidden` 把 fill 裁成胶囊, 形状才与进度无关。
   * 另外 track 本身已有同样的 radius, fill 上再写一份本来就是冗余。 */
}
/* 失败必须一眼可辨: 用户往往只扫一眼这块, 灰色文字会被直接略过 */
.update-failed {
  color: var(--danger-text);
}

.settings-footer-note {
  margin-top: 18px;
  padding-top: 14px;
  border-top: 1px solid var(--border-subtle);
  font-size: 11px;
  color: var(--text-tertiary);
  display: flex;
  align-items: center;
  gap: 6px;
  line-height: 1.6;
}

/* 9. 底部传输进度条 */
.desktop-speed-dock {
  height: 52px;
  background: var(--bg-sidebar);
  border-top: 1px solid var(--border-subtle);
  padding: 8px 24px;
  display: flex;
  flex-direction: column;
  justify-content: center;
  gap: 6px;
}
.desktop-speed-dock.failed .speed-bandwidth-rate { color: var(--danger-text); }
.speed-dock-meta { display: flex; justify-content: space-between; font-size: 12px; gap: 16px; }
.speed-file-title {
  border: none;
  background: transparent;
  padding: 0;
  font: inherit;
  text-align: left;
  display: flex;
  align-items: center;
  gap: 8px;
  color: var(--text-secondary);
  min-width: 0;
}
.speed-file-title strong {
  color: var(--text-primary);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  max-width: 46ch;
}
.speed-bandwidth-rate {
  color: var(--accent-text);
  font-family: var(--font-mono);
  font-size: 13px;
  font-weight: 700;
  flex-shrink: 0;
}
.speed-dock-actions {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-shrink: 0;
}
.speed-dock-cancel-btn {
  background: transparent;
  color: var(--danger-text);
  border: 1px solid var(--danger-border);
  border-radius: var(--radius-sm);
  padding: 1px 8px;
  font-size: 11px;
  cursor: pointer;
  line-height: 18px;
  transition: all 0.15s ease;
}
.speed-dock-cancel-btn:hover {
  background: var(--danger-solid);
  color: var(--text-on-solid);
}
.speed-dock-track { height: 4px; background: var(--track-bg); border-radius: var(--radius-pill); overflow: hidden; }
/* 用 `transform: scaleX()` 而不是 `width`。
 *
 * `width` 是**布局属性**: 每一帧都要重新计算整个文档的排版。
 * core 每个 4MB 分片发一次进度事件(protocol.rs: `CHUNK_SIZE`),
 * 按 110MB/s 算就是**每秒约 27 次** —— 也就是说这条进度条本来
 * 就在"一直在动"的状态下逼着浏览器不停重排。
 *
 * `transform` 走的是合成层, 只改 GPU 上的一层贴图, 不触发布局。
 * 传输大文件时进度条从"跟着掉帧"变成"完全顺滑"。
 *
 * `transform-origin: left` 让它从左往右长 —— 与 `width` 增长的
 * 视觉方向一致, 否则会从中心往两边撑。
 *
 * 注意: 圆角在这一层会跟着缩放而变形, 所以圆角必须留在 **track**
 * 上(靠 overflow:hidden 裁), fill 自己不设圆角。
 */
.speed-dock-fill {
  height: 100%;
  width: 100%;
  background: var(--accent);
  transform-origin: left center;
  transform: scaleX(0);
  transition: transform 0.18s linear;
}
.desktop-speed-dock.failed .speed-dock-fill { background: var(--danger); }

/* 10. 弹窗 */
.fluent-modal-overlay {
  position: fixed;
  inset: 0;
  background: var(--overlay-scrim);
  backdrop-filter: blur(8px);
  z-index: var(--z-overlay);
  display: grid;
  place-items: center;
  padding: 24px;
}
.fluent-modal-dialog {
  width: 440px;
  max-width: 100%;
  max-height: 90vh;
  overflow-y: auto;
  background: var(--bg-elevated);
  color: var(--text-primary);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-lg);
  box-shadow: var(--shadow-modal);
  padding: 22px;
}
.modal-dialog-titlebar { display: flex; justify-content: space-between; align-items: center; margin-bottom: 14px; }
.modal-dialog-titlebar h3 { font-size: 16px; font-weight: 700; color: var(--text-primary); }
.modal-close-icon { background: none; border: 0; color: var(--text-secondary); cursor: pointer; font-size: 18px; }
.modal-close-icon:hover { color: var(--text-primary); }

.close-dialog-hint { font-size: 12.5px; color: var(--text-secondary); line-height: 1.6; margin-bottom: 18px; }
.close-dialog-actions { display: flex; gap: 10px; justify-content: flex-end; flex-wrap: wrap; }

/* 配对流程说明。放在标签页**上方**而不是下方：它是"选哪个之前"
 * 就该知道的前提，排在标签页之后就成了"选完再解释你选错了"。 */
.pair-flow-hint {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  margin: 0 0 12px;
  padding: 9px 11px;
  font-size: 12px;
  line-height: 1.6;
  color: var(--text-secondary);
  background: var(--bg-hover-soft);
  border-radius: var(--radius-md);
}
.pair-flow-hint i {
  flex: 0 0 auto;
  margin-top: 1px;
  font-size: 13px;
  color: var(--text-tertiary);
}
.pair-mode-selector {
  display: flex;
  background: var(--bg-inset);
  padding: 4px;
  border-radius: var(--radius-md);
  margin-bottom: 16px;
  gap: 4px;
}
.pair-mode-selector button {
  flex: 1;
  height: 34px;
  border-radius: var(--radius-md);
  background: none;
  border: 0;
  color: var(--text-secondary);
  font-size: 12px;
  font-weight: 600;
  cursor: pointer;
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 6px;
  transition: background 0.15s, color 0.15s;
}
.pair-mode-selector button.active { background: var(--bg-card); color: var(--text-primary); box-shadow: var(--shadow-card); }

.pair-guide-hint { font-size: 12px; color: var(--text-secondary); margin-bottom: 12px; line-height: 1.5; }

.pin-input-container { margin-bottom: 14px; }
.code-pin-input {
  width: 100%;
  height: 52px;
  background: var(--bg-inset);
  border: 1.5px solid var(--border-subtle);
  border-radius: var(--radius-md);
  color: var(--accent-text);
  font-size: 26px;
  font-weight: 800;
  letter-spacing: 0.15em;
  text-align: center;
  font-family: var(--font-mono);
  box-sizing: border-box;
  user-select: text;
  -webkit-user-select: text;
}
.code-pin-input:focus { border-color: var(--accent); outline: 0; box-shadow: 0 0 14px var(--accent-glow); }
.code-pin-input::placeholder {
  font-size: 14px;
  font-weight: normal;
  letter-spacing: normal;
  color: var(--text-tertiary);
}

.form-input-group { margin-bottom: 14px; display: flex; flex-direction: column; gap: 5px; }
.form-input-group label { font-size: 11.5px; color: var(--text-secondary); font-weight: 600; }
.standard-text-input {
  height: 38px;
  background: var(--bg-inset);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-md);
  color: var(--text-primary);
  padding: 0 12px;
  font-size: 13px;
  width: 100%;
  box-sizing: border-box;
  user-select: text;
  -webkit-user-select: text;
}
.standard-text-input:focus { border-color: var(--accent); outline: 0; }
/* 格式校验失败态。描边用 --danger 而非"文字变红" ——
 * 只改文字色的话, 空输入框里根本没有文字, 用户看不到任何异常。 */
.standard-text-input.is-invalid {
  border-color: var(--danger-text);
}
.standard-text-input.is-invalid:focus {
  border-color: var(--danger-text);
}
/* 输入框下方的行内提示。文字色用 `--danger-on-soft` 那一族里的实心版
 * (--danger-text)：这行字直接压在弹窗的 --bg-elevated 上，属于"实心容器"。 */
.field-hint {
  display: flex;
  align-items: center;
  gap: 5px;
  font-size: 11px;
  line-height: 1.45;
  color: var(--danger-text);
}
.field-hint > i { flex: 0 0 auto; font-size: 13px; }
.standard-text-input::placeholder { color: var(--text-tertiary); }

.error-notice-card {
  font-size: 11.5px;
  color: var(--danger-on-soft);
  background: var(--danger-soft);
  border-radius: var(--radius-md);
  padding: 8px 12px;
  margin-bottom: 12px;
  display: flex;
  align-items: flex-start;
  gap: 6px;
  line-height: 1.5;
}
.error-notice-card.success { background: var(--accent-soft); color: var(--accent-on-soft); border: 1px solid var(--accent-border); }

.modal-actions-tray { display: flex; justify-content: flex-end; gap: 10px; margin-top: 16px; flex-wrap: wrap; }

.ip-select-header { display: flex; justify-content: space-between; align-items: center; margin-bottom: 2px; }
.btn-text-link {
  background: none;
  border: 0;
  color: var(--accent-text);
  font-size: 11px;
  cursor: pointer;
  padding: 0;
  text-decoration: underline;
}

.fluent-select-wrapper { position: relative; width: 100%; }
.standard-select {
  width: 100%;
  height: 38px;
  background: var(--bg-inset);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-md);
  color: var(--text-primary);
  padding: 0 32px 0 12px;
  font-size: 13px;
  appearance: none;
  cursor: pointer;
  box-sizing: border-box;
}
.standard-select:focus { border-color: var(--accent); outline: 0; }
.standard-select option { background: var(--bg-elevated); color: var(--text-primary); }
.select-caret {
  position: absolute;
  right: 12px;
  top: 50%;
  transform: translateY(-50%);
  color: var(--text-tertiary);
  pointer-events: none;
  font-size: 12px;
}

.auto-ip-box { position: relative; display: flex; align-items: center; }
.auto-filled-input { width: 100%; padding-right: 140px; box-sizing: border-box; }
.auto-recognized-badge {
  position: absolute;
  right: 8px;
  background: var(--accent-soft);
  color: var(--accent-on-soft);
  font-size: 11px;
  font-weight: 600;
  padding: 3px 8px;
  border-radius: var(--radius-sm);
  display: flex;
  align-items: center;
  gap: 4px;
  max-width: 130px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  pointer-events: none;
}

.dialog-success-mode { width: 460px; }
.pair-success-view { display: flex; flex-direction: column; align-items: center; text-align: center; padding: 8px 4px; }
.success-glyph-badge {
  width: 58px;
  height: 58px;
  border-radius: 50%;
  background: var(--accent-soft);
  color: var(--accent-on-soft);
  display: grid;
  place-items: center;
  font-size: 32px;
  margin-bottom: 12px;
  box-shadow: 0 0 24px var(--accent-glow);
}
.success-headline { font-size: 18px; font-weight: 750; color: var(--text-primary); margin-bottom: 12px; }
.success-device-card {
  width: 100%;
  background: var(--bg-inset);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-lg);
  padding: 14px 16px;
  display: flex;
  flex-direction: column;
  gap: 10px;
  text-align: left;
  box-sizing: border-box;
  margin-bottom: 6px;
}
.success-card-row { display: flex; justify-content: space-between; align-items: center; font-size: 12.5px; gap: 12px; }
.success-card-row .label { color: var(--text-tertiary); font-size: 12px; }
.success-card-row .value { color: var(--text-primary); font-weight: 600; }
.success-card-row .truncate {
  max-width: 220px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--accent-text);
}
.status-tag-trusted {
  background: var(--accent-soft);
  color: var(--accent-on-soft);
  font-size: 11px;
  font-weight: 650;
  padding: 2px 8px;
  border-radius: var(--radius-sm);
  display: inline-flex;
  align-items: center;
  gap: 4px;
}
.success-actions { width: 100%; }

.local-pin-display-box {
  background: var(--bg-inset);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-lg);
  padding: 16px;
  text-align: center;
}
.box-tiny-label { font-size: 11px; color: var(--text-tertiary); }
.box-giant-pin {
  font-size: 34px;
  font-weight: 850;
  color: var(--accent-text);
  letter-spacing: 0.22em;
  font-family: var(--font-mono);
  margin: 8px 0;
  user-select: text;
  -webkit-user-select: text;
}
.box-pin-actions { display: flex; justify-content: center; gap: 8px; margin-bottom: 8px; }
.btn-mini-action {
  background: var(--bg-row-soft);
  border: 1px solid var(--border-subtle);
  color: var(--text-secondary);
  border-radius: var(--radius-md);
  padding: 4px 10px;
  font-size: 11px;
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  gap: 4px;
  transition: background 0.15s, color 0.15s;
}
.btn-mini-action:hover { color: var(--text-primary); background: var(--bg-card-hover); }
.qr-canvas-holder { display: flex; justify-content: center; margin: 16px 0; }

.pin-countdown-tray {
  margin-top: 10px;
  display: flex;
  flex-direction: column;
  gap: 6px;
  align-items: center;
}
.pin-countdown-text {
  font-size: 11px;
  color: var(--text-tertiary);
  display: inline-flex;
  align-items: center;
  gap: 5px;
}
.pin-countdown-track { width: 170px; height: 4px; background: var(--track-bg); border-radius: var(--radius-pill); overflow: hidden; }
.pin-countdown-fill {
  height: 100%;
  width: 100%;
  background: var(--accent);
  /* 同 .speed-dock-fill: 用合成层而不是布局属性。
   * 这条每秒跳一次(倒计时), 30 秒里 30 次重排 —— 数字看着不大,
   * 但它和传输进度条会同时跑, 两边都在逼浏览器重排。 */
  transform-origin: left center;
  transform: scaleX(0);
  transition: transform 0.3s ease;
}

.approval-card {
  background: var(--bg-inset);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-md);
  padding: 14px 16px;
  margin-bottom: 6px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.approval-row { display: flex; justify-content: space-between; align-items: center; font-size: 13px; gap: 12px; }
.approval-row .label { color: var(--text-tertiary); font-size: 12px; flex-shrink: 0; }
.approval-row .value { color: var(--text-primary); }
.approval-actions { display: flex; gap: 10px; justify-content: flex-end; }
.btn-approval-allow {
  background: var(--success-solid);
  color: var(--text-on-solid);
  border: 0;
  padding: 8px 16px;
  border-radius: var(--radius-md);
  font-size: 13px;
  font-weight: 600;
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  gap: 6px;
  transition: background 0.15s;
}
.btn-approval-allow:hover { background: var(--success-solid-hover); }
.btn-approval-reject {
  background: var(--bg-row-soft);
  color: var(--text-secondary);
  border: 1px solid var(--border-subtle);
  padding: 8px 16px;
  border-radius: var(--radius-md);
  font-size: 13px;
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  gap: 6px;
  transition: background 0.15s, color 0.15s;
}
.btn-approval-reject:hover { background: var(--bg-card-hover); color: var(--text-primary); }
.btn-approval-block {
  background: var(--danger-soft);
  color: var(--danger-on-soft);
  border: 1px solid var(--danger-border);
  padding: 8px 16px;
  border-radius: var(--radius-md);
  font-size: 13px;
  font-weight: 600;
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  gap: 6px;
  transition: background 0.15s;
}
.btn-approval-block:hover { background: var(--danger-strong); color: var(--text-on-solid); }

/* 引擎异常横幅 */
.engine-error-banner {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 10px 24px;
  background: var(--danger-soft);
  border-bottom: 1px solid var(--danger-border);
  color: var(--danger-on-soft);
  font-size: 12.5px;
}
.engine-error-banner > i { font-size: 18px; flex-shrink: 0; }
.engine-error-text { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 2px; }
.engine-error-text strong { font-size: 12.5px; }
.engine-error-text span { color: var(--text-secondary); line-height: 1.45; }
.banner-close {
  background: none;
  border: 0;
  color: var(--text-secondary);
  cursor: pointer;
  font-size: 14px;
  padding: 4px;
  border-radius: var(--radius-sm);
  flex-shrink: 0;
}
.banner-close:hover { color: var(--text-primary); background: var(--bg-hover-soft); }

/* 页面级布局微调 */
.view-log, .view-settings { display: block; }
.pair-view-content { display: block; }
.approval-dialog { max-width: 460px; }
.close-dialog { max-width: 480px; }
.truncate { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

/* 11. 全局提示 */
.fluent-toast {
  position: fixed;
  bottom: 24px;
  left: 50%;
  transform: translateX(-50%);
  background: var(--bg-elevated);
  border: 1px solid var(--accent);
  color: var(--text-primary);
  padding: 10px 18px;
  border-radius: var(--radius-md);
  font-size: 13px;
  font-weight: 600;
  display: flex;
  align-items: center;
  gap: 8px;
  box-shadow: var(--shadow-toast);
  z-index: var(--z-toast);
  max-width: 80vw;
}
.fluent-toast.error { border-color: var(--danger); }
.fluent-toast.error i { color: var(--danger-text); }
.fluent-toast i { color: var(--accent-text); font-size: 16px; }
.toast-fade-enter-active, .toast-fade-leave-active { transition: opacity 0.2s, transform 0.2s; }
.toast-fade-enter-from, .toast-fade-leave-to { opacity: 0; transform: translate(-50%, 10px); }

/* 12. 传输历史路径清单 & 正在传输任务面板 */
.entry-title-wrap {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}
.btn-view-paths-badge {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 1px 7px;
  font-size: 11px;
  font-weight: 500;
  color: var(--accent-text);
  background: var(--bg-hover-soft);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-sm);
  cursor: pointer;
  transition: all 0.15s ease;
  line-height: 1.6;
}
.btn-view-paths-badge:hover {
  background: var(--accent);
  color: var(--accent-contrast);
  border-color: var(--accent);
}

.speed-dock-list-btn {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  padding: 4px 10px;
  font-size: 11.5px;
  font-weight: 500;
  color: var(--text-primary);
  background: var(--bg-hover-soft);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-sm);
  cursor: pointer;
  transition: all 0.15s ease;
}
.speed-dock-list-btn:hover {
  background: var(--bg-hover-soft);
  border-color: var(--accent);
  color: var(--accent);
}

.paths-list-dialog {
  width: 640px;
  max-width: 95vw;
  max-height: 85vh;
  display: flex;
  flex-direction: column;
}
.paths-dialog-title-group {
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.paths-dialog-title-group h3 {
  display: flex;
  align-items: center;
  gap: 6px;
  margin: 0;
}
.paths-dialog-sub {
  font-size: 12px;
  color: var(--text-secondary);
}
.paths-dialog-toolbar {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-bottom: 12px;
}
.paths-search-box {
  flex: 1;
  display: flex;
  align-items: center;
  gap: 6px;
  background: var(--bg-inset);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-sm);
  padding: 6px 10px;
}
.paths-search-box input {
  flex: 1;
  background: transparent;
  border: none;
  outline: none;
  color: var(--text-primary);
  font-size: 12.5px;
}
.btn-clear-search {
  background: none;
  border: none;
  color: var(--text-secondary);
  cursor: pointer;
  padding: 0;
  font-size: 14px;
}
.btn-clear-search:hover { color: var(--text-primary); }
.copy-all-btn {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  font-size: 12px;
  white-space: nowrap;
}
.paths-list-container {
  flex: 1;
  max-height: 380px;
  overflow-y: auto;
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-md);
  padding: 8px;
  display: flex;
  flex-direction: column;
  gap: 6px;
  background: var(--bg-inset);
}
.empty-paths-box {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  padding: 36px 0;
  color: var(--text-secondary);
  font-size: 13px;
  gap: 8px;
}
.empty-paths-box i { font-size: 28px; }
.path-item-row {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 8px 10px;
  border-radius: var(--radius-sm);
  background: var(--bg-elevated);
  border: 1px solid var(--border-subtle);
  transition: background 0.15s;
}
.path-item-row:hover {
  background: var(--bg-hover-soft);
}
.path-item-icon {
  color: var(--accent);
  font-size: 18px;
  flex-shrink: 0;
}
.path-item-content {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.path-item-basename {
  font-size: 12.5px;
  font-weight: 600;
  color: var(--text-primary);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.path-item-fullpath {
  font-size: 11px;
  color: var(--text-secondary);
  font-family: monospace;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  user-select: all;
}
.path-item-actions {
  display: flex;
  align-items: center;
  gap: 6px;
  flex-shrink: 0;
}
.btn-path-action {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 4px 8px;
  font-size: 11px;
  font-weight: 500;
  color: var(--text-primary);
  background: var(--bg-hover-soft);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-sm);
  cursor: pointer;
  transition: all 0.15s;
}
.btn-path-action:hover {
  border-color: var(--accent);
  color: var(--accent);
}
.paths-dialog-footer {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-top: 14px;
  gap: 12px;
}
.paths-footer-hint {
  font-size: 12px;
  color: var(--text-secondary);
}

.active-transfers-dialog {
  width: 560px;
  max-width: 95vw;
  max-height: 85vh;
  display: flex;
  flex-direction: column;
}
.active-dialog-title-group {
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.active-dialog-title-group h3 {
  display: flex;
  align-items: center;
  gap: 6px;
  margin: 0;
}
.active-dialog-sub {
  font-size: 12px;
  color: var(--text-secondary);
}
.active-transfers-list-container {
  flex: 1;
  max-height: 420px;
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 4px 0;
}
.empty-active-box {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  padding: 40px 0;
  color: var(--text-secondary);
  font-size: 13px;
  gap: 8px;
}
.empty-active-box i { font-size: 32px; color: var(--accent); }
.active-transfer-card {
  padding: 12px;
  border-radius: var(--radius-md);
  border: 1px solid var(--border-subtle);
  background: var(--bg-inset);
  display: flex;
  flex-direction: column;
  gap: 8px;
  transition: opacity 0.2s;
}
.active-transfer-card.card-settled {
  opacity: 0.75;
}
.card-header-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.card-direction-tag {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  font-size: 12px;
  font-weight: 600;
}
.card-direction-tag.send { color: var(--info-text); }
.card-direction-tag.recv { color: var(--success-text); }
.card-speed-badge {
  font-size: 11.5px;
  font-weight: 600;
  color: var(--accent-text);
  background: var(--bg-hover-soft);
  padding: 2px 7px;
  border-radius: var(--radius-sm);
  font-variant-numeric: tabular-nums;
}
.card-file-row {
  display: flex;
  align-items: center;
  gap: 6px;
  color: var(--text-primary);
  font-size: 13px;
  font-weight: 500;
}
.card-file-row i { font-size: 16px; color: var(--text-secondary); flex-shrink: 0; }
.card-filename {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.card-progress-track {
  width: 100%;
  height: 6px;
  border-radius: var(--radius-pill);
  background: var(--bg-hover-soft);
  overflow: hidden;
}
.card-progress-fill {
  height: 100%;
  background: var(--accent);
  transition: width 0.2s ease;
}
.card-footer-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.card-bytes-text {
  font-size: 11.5px;
  color: var(--text-secondary);
  font-variant-numeric: tabular-nums;
}
.btn-card-cancel {
  display: inline-flex;
  align-items: center;
  gap: 3px;
  padding: 3px 8px;
  font-size: 11px;
  font-weight: 500;
  color: var(--danger-on-soft);
  background: var(--danger-soft);
  border: 1px solid var(--danger-border);
  border-radius: var(--radius-sm);
  cursor: pointer;
  transition: all 0.15s;
}
.btn-card-cancel:hover {
  background: var(--danger-solid);
  color: var(--text-on-solid);
}
.card-settled-text {
  font-size: 11.5px;
  font-weight: 600;
}
.card-settled-text.ok { color: var(--success-text); }
.card-settled-text.cancelled { color: var(--text-secondary); }
.card-settled-text.failed { color: var(--danger-text); }
.active-dialog-footer {
  display: flex;
  align-items: center;
  margin-top: 14px;
  gap: 10px;
}
.footer-spacer { flex: 1; }
.text-success { color: var(--success-text) !important; }
</style>

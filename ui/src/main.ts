import { createApp } from "vue";
import { createPinia } from "pinia";
import App from "./App.vue";
import "./style.css";

/**
 * 在 Vue 挂载之前把主题写到 <html> 上,
 * 避免首屏先用深色令牌渲染再切成浅色造成的"闪白/闪黑"。
 *
 * 顺带同步 `color-scheme`: 它决定浏览器给**原生部件**（滚动条、
 * 表控件、拼写下拉、清除按钮）用深色还是浅色皮肤。此前没声明,
 * 于是深色界面上会出现一条刺眼的系统浅色滚动条 —— 那不是样式
 * 写错了, 是浏览器不知道自己在深色环境里。
 *
 * 主题切换时同样要改, 见 stores/deviceStore.ts 的 applyTheme。
 */
function applyStoredTheme() {
  const theme = localStorage.getItem("feisuo-theme") === "light" ? "light" : "dark";
  document.documentElement.dataset.theme = theme;
  document.documentElement.style.colorScheme = theme;
}
applyStoredTheme();

const app = createApp(App);
app.use(createPinia());
app.mount("#app");

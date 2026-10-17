<template>
  <div
    ref="selectRef"
    class="custom-select-container"
    :class="{ 'is-open': isOpen, 'is-disabled': disabled, 'placement-top': placement === 'top' }"
    :style="{ width: width }"
    @keydown.esc.stop="closeDropdown"
    @keydown.down.prevent="navigateOptions(1)"
    @keydown.up.prevent="navigateOptions(-1)"
    @keydown.enter.prevent="selectHighlighted"
  >
    <!-- 触发器按钮 -->
    <button
      type="button"
      class="custom-select-trigger"
      :class="{ 'is-active': isOpen }"
      :disabled="disabled"
      :aria-label="ariaLabel"
      :aria-expanded="isOpen"
      role="combobox"
      aria-haspopup="listbox"
      @click="toggleDropdown"
    >
      <span class="custom-select-label" :class="{ 'is-placeholder': !selectedOption }">
        {{ selectedOption ? selectedOption.label : placeholder }}
      </span>
      <i class="ph ph-caret-down custom-select-caret" :class="{ 'is-rotated': isOpen }"></i>
    </button>

    <!-- 下拉选项浮层 (完全受控的暗色毛玻璃程序级面板) -->
    <transition name="select-pop">
      <div
        v-if="isOpen"
        ref="panelRef"
        class="custom-select-panel"
        :class="placement === 'top' ? 'panel-top' : 'panel-bottom'"
        role="listbox"
        :aria-label="ariaLabel"
      >
        <button
          v-for="(opt, idx) in options"
          :key="String(opt.value)"
          type="button"
          class="custom-select-option"
          :class="{
            'is-selected': isOptionSelected(opt.value),
            'is-highlighted': highlightedIndex === idx,
            'is-disabled': opt.disabled
          }"
          role="option"
          :aria-selected="isOptionSelected(opt.value)"
          @click.stop="handleSelect(opt)"
          @mouseenter="highlightedIndex = idx"
        >
          <span class="option-text">{{ opt.label }}</span>
          <i v-if="isOptionSelected(opt.value)" class="ph-bold ph-check option-check"></i>
        </button>
      </div>
    </transition>
  </div>
</template>

<script setup lang="ts">
import { ref, computed, onMounted, onBeforeUnmount, nextTick } from "vue";

export interface SelectOption {
  value: string | number;
  label: string;
  disabled?: boolean;
}

const props = withDefaults(
  defineProps<{
    modelValue: string | number;
    options: SelectOption[];
    disabled?: boolean;
    placeholder?: string;
    ariaLabel?: string;
    width?: string;
  }>(),
  {
    disabled: false,
    placeholder: "请选择",
    ariaLabel: "下拉选择",
    width: "100%",
  }
);

const emit = defineEmits<{
  (e: "update:modelValue", value: string | number): void;
  (e: "change", value: string | number): void;
}>();

const selectRef = ref<HTMLElement | null>(null);
const panelRef = ref<HTMLElement | null>(null);
const isOpen = ref(false);
const placement = ref<"bottom" | "top">("bottom");
const highlightedIndex = ref(-1);

const selectedOption = computed(() => {
  return props.options.find((opt) => opt.value === props.modelValue);
});

function isOptionSelected(val: string | number): boolean {
  return val === props.modelValue;
}

/**
 * 决定向上还是向下展开。
 *
 * ## 为什么要在打开**之后**再算一次，且滚动时也要重算
 *
 * 早先只在 `openDropdown` 的 `nextTick` 里算一次方向就再也不管了。
 * 但设置页本身就是可滚动的：用户展开一个靠近底部的下拉、往上滚两下，
 * 那个面板仍固执地向上弹 —— 因为它盖住了它自己的触发器。
 *
 * 判据用面板**实际**需要的空间（`panelRef.scrollHeight`），而不是
 * 写死的 220：选项多的时候宁可向上，也不要被裁掉一半。
 */
function updatePlacement() {
  if (!isOpen.value) return;
  const el = selectRef.value;
  if (!el) return;
  const rect = el.getBoundingClientRect();

  // 面板还没有渲染时用选项数估一个高度：每项约 34px + 12px 内边距。
  const estimated = Math.min(320, props.options.length * 34 + 12);
  const wanted = panelRef.value?.scrollHeight || estimated;

  const spaceBelow = window.innerHeight - rect.bottom - 12;
  const spaceAbove = rect.top - 12;

  // 只有"下面确实放不下、且上面更宽裕"时才向上翻。
  placement.value = spaceBelow < wanted && spaceAbove > spaceBelow ? "top" : "bottom";
}

function toggleDropdown() {
  if (props.disabled) return;
  if (isOpen.value) {
    closeDropdown();
  } else {
    openDropdown();
  }
}

function openDropdown() {
  isOpen.value = true;
  // 聚焦当前选中的项索引
  const currentIdx = props.options.findIndex((opt) => opt.value === props.modelValue);
  highlightedIndex.value = currentIdx >= 0 ? currentIdx : 0;

  nextTick(updatePlacement);
  // 滚动 / 窗口尺寸变化时重新判方向。用 passive 避免拖慢滚动。
  window.addEventListener("scroll", updatePlacement, true);
  window.addEventListener("resize", updatePlacement);
}

function closeDropdown() {
  isOpen.value = false;
  highlightedIndex.value = -1;
  window.removeEventListener("scroll", updatePlacement, true);
  window.removeEventListener("resize", updatePlacement);
}

function handleSelect(opt: SelectOption) {
  if (opt.disabled) return;
  emit("update:modelValue", opt.value);
  emit("change", opt.value);
  closeDropdown();
}

function navigateOptions(delta: number) {
  if (!isOpen.value) {
    openDropdown();
    return;
  }
  const len = props.options.length;
  if (len === 0) return;
  let next = highlightedIndex.value + delta;
  if (next < 0) next = len - 1;
  if (next >= len) next = 0;
  highlightedIndex.value = next;
}

function selectHighlighted() {
  if (!isOpen.value) {
    openDropdown();
    return;
  }
  if (highlightedIndex.value >= 0 && highlightedIndex.value < props.options.length) {
    const opt = props.options[highlightedIndex.value];
    handleSelect(opt);
  }
}

function handleClickOutside(e: MouseEvent) {
  if (!isOpen.value) return;
  const target = e.target as Node | null;
  if (selectRef.value && target && !selectRef.value.contains(target)) {
    closeDropdown();
  }
}

onMounted(() => {
  document.addEventListener("mousedown", handleClickOutside, true);
});

onBeforeUnmount(() => {
  document.removeEventListener("mousedown", handleClickOutside, true);
  // 组件在下拉展开状态被卸载时（切页签、列表重排），
  // 那两个 window 监听器不会自己消失 —— 它们闭包着已经销毁的
  // 组件实例，之后每次滚动都会白跑一次。
  window.removeEventListener("scroll", updatePlacement, true);
  window.removeEventListener("resize", updatePlacement);
});
</script>

<style scoped>
.custom-select-container {
  position: relative;
  display: inline-block;
  user-select: none;
  font-family: var(--font-family);
  box-sizing: border-box;
}

.custom-select-trigger {
  width: 100%;
  height: 38px;
  /* 底色必须与 `.standard-text-input` 一致（`--bg-inset`）。
   *
   * 早先这里是 `--bg-card`(#20222b)，而同一列里的设备名输入框是
   * `--bg-inset`(#16181f) —— 两个并排的控件深浅不同，看起来像
   * 一个是可编辑的、一个不是，而它们其实是同一层级的设置项。 */
  background: var(--bg-inset);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-md, 8px);
  color: var(--text-primary);
  padding: 0 12px;
  font-size: 13px;
  /* `text-align` 必须显式给 left：按钮的 UA 默认是 center，
   * 它会**穿透**到 .custom-select-label 之前的那一层盒子，
   * 在窄宽度下把选项文字推到中间。 */
  text-align: left;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  cursor: pointer;
  outline: none;
  transition: border-color 0.15s ease, background-color 0.15s ease, box-shadow 0.15s ease;
  box-sizing: border-box;
}

.custom-select-trigger:hover:not(:disabled) {
  background: var(--bg-card-hover);
  border-color: var(--border-strong);
}

/* 展开态用**柔和**的焦点环，与输入框的 focus 视觉统一。
 *
 * 早先这里是 `0 0 0 2px var(--accent-glow)`：2px 实心品牌色外扩环
 * 在深色背景上过于抢眼，展开一个下拉就让整页视线被拽过去。
 * 输入框的 focus 用的是 `--focus-ring` 描边（不改变盒模型尺寸），
 * 两者应当看起来是同一个系统。 */
.custom-select-trigger.is-active,
.custom-select-trigger:focus-visible {
  border-color: var(--focus-ring);
  box-shadow: 0 0 0 1px var(--focus-ring);
}

.custom-select-trigger:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.custom-select-label {
  flex: 1;
  text-align: left;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  color: var(--text-primary);
}

.custom-select-label.is-placeholder {
  color: var(--text-tertiary);
}

.custom-select-caret {
  flex: 0 0 auto;
  font-size: 12px;
  color: var(--text-tertiary);
  transition: transform 0.2s cubic-bezier(0.16, 1, 0.3, 1), color 0.15s ease;
}

.custom-select-caret.is-rotated {
  transform: rotate(180deg);
  color: var(--accent);
}

/* 下拉选项浮层 */
.custom-select-panel {
  position: absolute;
  left: 0;
  width: 100%;
  min-width: 100%;
  /* 高度自适应，但留出上下各 12px 的呼吸空间。
   *
   * 早先写死 `max-height: 220px`：选项一多就出现内部滚动条，而滚动条
   * 只在鼠标移上去时才浮现（`::-webkit-scrollbar-thumb` 是深色的），
   * 用户容易以为列表就这么几项。改成跟随视口，最多到 `min(320px, ...)`，
   * 同时靠 `overflow-y: auto` 兜住极端情况。 */
  max-height: min(320px, calc(100vh - 24px));
  overflow-y: auto;
  overscroll-behavior: contain;
  background: var(--bg-elevated);
  border: 1px solid var(--border-strong);
  border-radius: var(--radius-lg);
  /* 内边距与选项的圆角必须**同源**：padding 4px + option 圆角 4px
   * 时，选中项的高亮块会贴着面板边缘显得局促；6px 让高亮块四周
   * 都有均等的空隙。 */
  padding: 6px;
  box-shadow: var(--shadow-modal);
  /* 刻意**不用** backdrop-filter。
   *
   * 它在这里完全无效：面板底色 `--bg-elevated` 是不透明色，
   * 模糊没有可透出的内容 —— 浏览器仍会为它分配一个合成层并每帧
   * 重算，纯粹白烧 GPU。要真正做毛玻璃，底色必须先带透明度
   * （如 rgba），而那会让长中文列表的可读性下降。 */
  z-index: var(--z-overlay-nested);
  box-sizing: border-box;
}

.custom-select-panel.panel-bottom {
  top: calc(100% + 6px);
}

.custom-select-panel.panel-top {
  bottom: calc(100% + 6px);
}

/* 选项项 */
.custom-select-option {
  width: 100%;
  border: none;
  background: transparent;
  text-align: left;
  padding: 8px 10px;
  border-radius: var(--radius-sm);
  font-size: 13px;
  color: var(--text-primary);
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  cursor: pointer;
  transition: background-color 0.12s ease, color 0.12s ease;
}

.custom-select-option:hover,
.custom-select-option.is-highlighted {
  background: var(--bg-hover-soft);
  color: var(--text-primary);
}

.custom-select-option.is-selected {
  background: var(--bg-active);
  color: var(--accent-on-soft, var(--accent));
  font-weight: 500;
}

.custom-select-option.is-disabled {
  opacity: 0.4;
  cursor: not-allowed;
}

.option-text {
  flex: 1;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.option-check {
  font-size: 14px;
  color: var(--accent);
  flex: 0 0 auto;
}

/* 下拉弹层微动效 */
.select-pop-enter-active,
.select-pop-leave-active {
  transition: opacity 0.15s ease, transform 0.15s cubic-bezier(0.16, 1, 0.3, 1);
}

.select-pop-enter-from,
.select-pop-leave-to {
  opacity: 0;
  transform: scaleY(0.95);
}

.panel-bottom.select-pop-enter-from,
.panel-bottom.select-pop-leave-to {
  transform-origin: top center;
}

.panel-top.select-pop-enter-from,
.panel-top.select-pop-leave-to {
  transform-origin: bottom center;
}

/* 专属纤细滚动条 */
.custom-select-panel::-webkit-scrollbar {
  width: 5px;
}
.custom-select-panel::-webkit-scrollbar-thumb {
  background: var(--scrollbar-thumb);
  border-radius: var(--radius-pill);
}
.custom-select-panel::-webkit-scrollbar-thumb:hover {
  background: var(--scrollbar-thumb-hover);
}
</style>

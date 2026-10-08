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

  // 自适应判断展开方向（若下方空间不足则向上弹出）
  nextTick(() => {
    if (selectRef.value) {
      const rect = selectRef.value.getBoundingClientRect();
      const spaceBelow = window.innerHeight - rect.bottom;
      const spaceAbove = rect.top;
      if (spaceBelow < 220 && spaceAbove > spaceBelow) {
        placement.value = "top";
      } else {
        placement.value = "bottom";
      }
    }
  });
}

function closeDropdown() {
  isOpen.value = false;
  highlightedIndex.value = -1;
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
  background: var(--bg-card);
  border: 1px solid var(--border-subtle);
  border-radius: var(--radius-md, 8px);
  color: var(--text-primary);
  padding: 0 12px;
  font-size: 13px;
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

.custom-select-trigger.is-active,
.custom-select-trigger:focus-visible {
  border-color: var(--accent);
  box-shadow: 0 0 0 2px var(--accent-glow);
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
  min-width: 160px;
  max-height: 220px;
  overflow-y: auto;
  background: var(--bg-elevated);
  border: 1px solid var(--border-strong);
  border-radius: var(--radius-lg);
  padding: 4px;
  box-shadow: var(--shadow-modal);
  backdrop-filter: blur(20px);
  -webkit-backdrop-filter: blur(20px);
  z-index: var(--z-overlay-nested);
  box-sizing: border-box;
}

.custom-select-panel.panel-bottom {
  top: calc(100% + 5px);
}

.custom-select-panel.panel-top {
  bottom: calc(100% + 5px);
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

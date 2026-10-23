/**
 * 插件页入口：标签页切换 + 全量重绘 + 定时刷新。
 *
 * ── 重绘策略 ────────────────────────────────────────────────
 * 没有虚拟 DOM，所以「全量重绘」要克制：只在 store 变了时重画**当前**标签页
 * （另两个保持原样，切过去时再画）。重绘会丢输入焦点，所以编辑中的表单在
 * 重绘后重新聚焦（`data-focus` 标记，见 `restoreFocus`）。
 *
 * ── 零轮询 ──────────────────────────────────────────────────
 * 页面打开时拉一次快照；签到 / 保存等动作完成后由 state 层自己重拉。
 * 定时签到正在执行时（`auto.running`）才开一个短轮询 —— 那是唯一「后台在
 * 变化」的场景。
 */

import { renderCenter } from './center.js';
import { renderRecords } from './records.js';
import { renderSchedule } from './schedule.js';
import { badge, button, el, fill } from './dom.js';
import { createRoot, requireKit } from './kit.js';
import { requestTheme } from './api.js';
import { loadCenter, loadRecords, loadSchedules, patch, store, subscribe } from './state.js';

const TABS = ['center', 'schedule', 'records'];

/** Tab 条的 React root（只建一次，之后复用 —— 每次重绘新建会丢焦点） */
let tabsRoot = null;

const containers = {
  center: document.getElementById('tab-center'),
  schedule: document.getElementById('tab-schedule'),
  records: document.getElementById('tab-records'),
};

/** 记录重绘前的焦点位置（`data-focus` 标记的 selector 与光标位置） */
function captureFocus() {
  const active = document.activeElement;
  if (!active || !(active instanceof HTMLElement)) return null;
  const key = active.dataset.focus;
  if (!key) return null;
  const selection = typeof active.selectionStart === 'number'
    ? { start: active.selectionStart, end: active.selectionEnd }
    : null;
  return { key, selection };
}

/** 重绘后把焦点放回同一个 `data-focus` 元素（输入到一半不被打断） */
function restoreFocus(saved) {
  if (!saved) return;
  const node = document.querySelector(`[data-focus="${saved.key}"]`);
  if (!(node instanceof HTMLElement)) return;
  node.focus();
  if (saved.selection && typeof node.setSelectionRange === 'function') {
    try {
      node.setSelectionRange(saved.selection.start, saved.selection.end);
    } catch {
      /* 某些 input 类型不支持 setSelectionRange，忽略 */
    }
  }
}

/** 画当前标签页 */
function renderActiveTab() {
  const container = containers[store.tab];
  if (!container) return;
  const saved = captureFocus();
  if (store.tab === 'center') renderCenter(container);
  else if (store.tab === 'schedule') renderSchedule(container);
  else renderRecords(container);
  restoreFocus(saved);
}

/** 切换标签页（只显示一个 body，另两个隐藏） */
function showTab(tab) {
  if (!TABS.includes(tab)) return;
  patch({ tab });
  for (const name of TABS) {
    const node = containers[name];
    if (node) node.hidden = name !== tab;
  }
  // 选中态由 SegmentedControl 自己管（它是受控组件，`value` 变了就重画滑块），
  // 这里只需把新的 tab 告诉它一次
  paintTabs();
  renderActiveTab();
}

/**
 * 总览卡点一下切到对应标签页。
 *
 * 走自定义事件而不是把 `showTab` 导出给 `center.js`：渲染层（center）不该
 * 反过来依赖入口层（main）的私有函数 —— 那会形成 `main → center → main`
 * 的循环导入，ESM 下表现为「center 拿到 undefined」。事件是单向的，
 * 与宿主 `wb:sidebar-layout` 那一套同一取向。
 */
window.addEventListener('ck:switch-tab', event => {
  const tab = event.detail?.tab;
  if (typeof tab === 'string') showTab(tab);
});

/** Tab 条：三个档位用组件库的 `SegmentedControl` 渲染 */
function paintTabs() {
  const host = document.getElementById('tabs-host');
  if (!host) return;
  const { React, SegmentedControl } = requireKit();
  const root = tabsRoot || (tabsRoot = createRoot(host));
  root.render(React.createElement(SegmentedControl, {
    options: [
      { value: 'center', label: '签到中心' },
      { value: 'schedule', label: '定时签到' },
      { value: 'records', label: '签到记录' },
    ],
    value: store.tab || 'center',
    onValueChange: next => showTab(next),
    'aria-label': '签到中心分区',
  }));
}

/**
 * 顶部「自动签到」徽章：反映总开关与今日执行状态。
 *
 * 用组件库的 `Badge`（与旧实现同一个组件），档位映射：
 * 不可用 → destructive、执行中 → warning、已关闭 → outline、正常 → success。
 */
function paintAutoBadge() {
  const host = document.getElementById('auto-badge');
  if (!host) return;
  const auto = store.schedules || store.snapshot?.auto;
  if (!auto) {
    host.replaceChildren();
    return;
  }
  const tone = auto.running ? 'warn'
    : auto.enabled !== true ? 'plain'
      : 'ok';
  const label = auto.running ? '执行中…'
    : auto.enabled !== true ? '自动签到已关闭'
      : auto.lastFiredToday ? '今日已执行' : '自动签到已开启';
  host.replaceChildren(badge(label, tone));
}

/** 页面级错误条（读快照失败时显示） */
function paintError() {
  const box = document.getElementById('page-error');
  if (!box) return;
  const message = store.loadError || store.schedulesError || store.recordsError;
  box.hidden = !message;
  if (message) {
    fill(box,
      el('span', { text: message }),
      // 走 `button()` 而不是手拼一个 <button>：这样它也是组件库的按钮，
      // 与页面里其它按钮同一尺寸 / 配色（手拼会漏掉 Tailwind 类名）
      button('重试', {
        tone: 'outline',
        onClick: () => {
          void loadCenter();
          void loadSchedules();
          void loadRecords();
        },
      }),
    );
  }
}

/* ─── 启动 ───────────────────────────────────── */

/** 顶部「刷新」按钮：用组件库渲染（旧实现里它是 `Button variant='ghost' size='sm'`） */
function paintReload() {
  const host = document.getElementById('reload-host');
  if (!host) return;
  host.replaceChildren(button('刷新', {
    tone: 'ghost',
    onClick: () => {
      void loadCenter();
      void loadSchedules();
      void loadRecords();
    },
  }));
}

subscribe(() => {
  paintTabs();
  paintAutoBadge();
  paintError();
  renderActiveTab();
});

// 首屏：tab 条与刷新按钮先画出来（数据还没到，徽章留给 subscribe）
paintTabs();
paintReload();

// 首屏：快照 + 定时状态 + 记录总数（总览卡要读它）
void (async () => {
  await Promise.all([loadCenter(), loadSchedules(), loadRecords()]);
})();

/**
 * 自动签到正在执行时短轮询：那是唯一「后台会自己变」的场景。
 *
 * 间隔 5 秒 —— 一轮签到可能要几十秒（各家上游串行、中间留间隔），太快只会
 * 空打网关。执行结束后自动停。
 */
let runningTimer = 0;
subscribe(() => {
  const running = (store.schedules || store.snapshot?.auto)?.running === true;
  if (running && !runningTimer) {
    runningTimer = window.setInterval(() => {
      void loadCenter();
      void loadSchedules();
    }, 5000);
  } else if (!running && runningTimer) {
    window.clearInterval(runningTimer);
    runningTimer = 0;
  }
});

/**
 * 主题：向宿主问一次当前模式（`light` / `dark` / `system`）。
 *
 * 消息处理在 `api.js`（与凭证同一条通道、常驻监听）—— 这里只负责在首帧
 * 主动要一次，免得「宿主推送」输给「页面首帧」。拿不到时页面保持
 * `<html data-theme="system">`，由 `prefers-color-scheme` 兜底。
 */
requestTheme();

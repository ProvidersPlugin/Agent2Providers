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
import { el, fill } from './dom.js';
import { loadCenter, loadRecords, loadSchedules, patch, store, subscribe } from './state.js';

const TABS = ['center', 'schedule', 'records'];

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
  document.querySelectorAll('.ck-tab').forEach(button => {
    button.classList.toggle('active', button.dataset.tab === tab);
  });
  renderActiveTab();
}

/** 顶部「自动签到」徽章：反映总开关与今日执行状态 */
function paintAutoBadge() {
  const badge = document.getElementById('auto-badge');
  if (!badge) return;
  const auto = store.schedules || store.snapshot?.auto;
  if (!auto) {
    badge.hidden = true;
    return;
  }
  badge.hidden = false;
  if (auto.running) {
    badge.textContent = '执行中…';
    badge.dataset.tone = 'warn';
  } else if (auto.enabled !== true) {
    badge.textContent = '自动签到已关闭';
    badge.dataset.tone = 'plain';
  } else {
    badge.textContent = auto.lastFiredToday ? '今日已执行' : '自动签到已开启';
    badge.dataset.tone = 'ok';
  }
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
      el('button', {
        type: 'button',
        class: 'ck-btn ck-btn-outline',
        text: '重试',
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

subscribe(() => {
  paintAutoBadge();
  paintError();
  renderActiveTab();
});

// 标签页切换（顶栏按钮）
document.querySelectorAll('.ck-tab').forEach(button => {
  button.addEventListener('click', () => showTab(button.dataset.tab));
});

// 顶部「刷新」：三块数据一起重拉
document.getElementById('reload')?.addEventListener('click', () => {
  void loadCenter();
  void loadSchedules();
  void loadRecords();
});

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

// 宿主切主题时通知（WebView2 的 prefers-color-scheme 会跟着变，CSS 自动生效；
// 这里只处理需要 JS 参与的场合 —— 目前没有，保留钩子方便将来加）
window.addEventListener('message', event => {
  const data = event.data;
  if (data && data.type === 'agent2api:theme') {
    document.documentElement.dataset.theme = String(data.theme || '');
  }
});

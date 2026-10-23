/**
 * 「签到记录」标签页：每账号一行的明细台账。
 *
 * ── 这页回答什么 ────────────────────────────────────────────
 * 「到底领到了没、失败的话错在哪、领了多少」—— 每执行一次签到（定时 / 启动
 * 补签 / 手动批量 / 单账号 / 国际版活动），每个账号落一行。判据在后端唯一
 * （复用签到执行体的同一对函数），这里只按 `outcome` 上色，不自己再判一遍。
 *
 * ── 分页 ────────────────────────────────────────────────────
 * 服务端真分页（`limit` / `offset` + 同条件 `total`）。筛选变化时 offset 归零。
 */

import { badge, button, el, fill, formatTime, textInput } from './dom.js';
import { loadRecords, patch, store } from './state.js';

/** 结果档位 → 文案与徽章色（`outcome` 由后端判） */
const OUTCOME_VIEW = {
  success: { label: '成功领取', tone: 'ok' },
  already: { label: '今日已领', tone: 'brand' },
  pending: { label: '进行中', tone: 'plain' },
  failed: { label: '失败', tone: 'err' },
};

/** 触发来源的展示名 */
const SOURCE_LABEL = {
  schedule: '定时条目',
  startup: '启动补签',
  manual: '手动批量',
  single: '单账号',
  activity: '活动保活',
};

/** 时间档位 → 毫秒下界（含今天在内的 N 个自然日） */
function rangeStart(value) {
  const now = new Date();
  const midnight = date => new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
  switch (value) {
    case 'today': return midnight(now);
    case '7': return midnight(new Date(now.getFullYear(), now.getMonth(), now.getDate() - 6));
    case '30': return midnight(new Date(now.getFullYear(), now.getMonth(), now.getDate() - 29));
    case 'month': return midnight(new Date(now.getFullYear(), now.getMonth(), 1));
    default: return null;
  }
}

const RANGES = [
  { value: 'today', label: '今天' },
  { value: '7', label: '7 天' },
  { value: '30', label: '30 天' },
  { value: 'month', label: '本月' },
  { value: 'all', label: '全部' },
];

const PAGE_SIZE = 50;

/** 改筛选条件后重拉（offset 归零） */
function applyFilter(patchBody) {
  patch({ recordsQuery: { ...store.recordsQuery, ...patchBody, offset: 0 } });
  void loadRecords();
}

function filtersBar() {
  const query = store.recordsQuery;
  const range = query.range || 'all';

  const seg = el('div', { class: 'ck-seg' });
  for (const option of RANGES) {
    seg.append(el('button', {
      type: 'button',
      class: option.value === range ? 'ck-seg-item active' : 'ck-seg-item',
      text: option.label,
      onClick: () => {
        const start = rangeStart(option.value);
        patch({ recordsQuery: { ...query, range: option.value, start: start || '', offset: 0 } });
        void loadRecords();
      },
    }));
  }

  const outcomeSelect = el('select', {
    class: 'ck-input ck-input-select',
    onChange: event => applyFilter({ outcome: event.target.value }),
  },
  el('option', { value: '', text: '全部结果' }),
  el('option', { value: 'success', text: '成功领取' }),
  el('option', { value: 'already', text: '今日已领' }),
  el('option', { value: 'failed', text: '失败' }),
  );
  outcomeSelect.value = query.outcome || '';

  const keyword = textInput({
    type: 'search',
    value: query.keyword || '',
    placeholder: '搜索账号 / 说明',
    // 输入框失焦或回车才查：每敲一个字都打一次网关没必要
    onKeydown: event => {
      if (event.key === 'Enter') applyFilter({ keyword: event.target.value.trim() });
    },
    onInput: value => {
      if (!value.trim()) applyFilter({ keyword: '' });
    },
  });
  keyword.classList.add('ck-input-search');

  return el('div', { class: 'ck-filters' },
    seg,
    outcomeSelect,
    keyword,
    el('span', { class: 'ck-filters-count', text: store.records ? `共 ${store.records.total} 条` : '' }),
    button('刷新', {
      tone: 'ghost',
      icon: 'refresh',
      onClick: () => void loadRecords(),
    }),
  );
}

/** 一行的「说明」列：收益 / 上游文案优先，失败时用错误原文 */
function detailText(row) {
  if (row.message) return row.message;
  if (row.error) return row.error;
  if (row.keptActive) return '保活成功';
  return '';
}

function recordsTable() {
  const rows = store.records?.records || [];
  if (!rows.length) {
    return el('div', { class: 'ck-empty', text: '这段时间里还没有签到记录。' });
  }
  const table = el('table', { class: 'ck-table ck-records-table' });
  // 列宽由 `table-layout: fixed` + 这里的 <col> 分配（见 page.css 的说明）：
  // 时间列定宽 160px，其余按百分数吃掉剩余宽度。
  table.append(el('colgroup', {},
    el('col', { class: 'ck-col-time' }),
    el('col', { class: 'ck-col-account' }),
    el('col', { class: 'ck-col-provider' }),
    el('col', { class: 'ck-col-source' }),
    el('col', { class: 'ck-col-outcome' }),
    el('col', { class: 'ck-col-detail' }),
  ));
  table.append(el('thead', {},
    el('tr', {},
      el('th', { text: '时间' }),
      el('th', { text: '账号' }),
      el('th', { text: '提供商' }),
      el('th', { text: '来源' }),
      el('th', { text: '结果' }),
      el('th', { text: '说明' }),
    ),
  ));
  const body = el('tbody');
  for (const row of rows) {
    const view = OUTCOME_VIEW[row.outcome] || { label: row.outcome || '—', tone: 'plain' };
    const timeCell = el('td', { class: 'ck-col-time ck-nowrap' },
      el('span', { text: formatTime(row.ts) }),
      // 北京时间的自然日（上游口径）：与本地时间可能差一天，单独一行给出来
      row.date ? el('span', { class: 'ck-record-date', text: row.date }) : null,
    );
    const detail = detailText(row);
    body.append(el('tr', { class: row.ok ? '' : 'ck-row-fail' },
      timeCell,
      el('td', { text: row.accountName || '—' }),
      el('td', { class: 'ck-record-source', text: row.provider || '—' }),
      el('td', {},
        el('span', { text: SOURCE_LABEL[row.source] || row.source || '—' }),
        row.reason ? el('span', { class: 'ck-record-reason', text: row.reason }) : null,
      ),
      el('td', {}, badge(view.label, view.tone)),
      el('td', { class: detail && !row.ok ? 'ck-record-detail-fail' : '', text: detail || '—' }),
    ));
  }
  table.append(body);
  return el('div', { class: 'ck-records-scroll' }, table);
}

function pager() {
  const total = store.records?.total || 0;
  const query = store.recordsQuery;
  if (total <= PAGE_SIZE) return null;
  const offset = query.offset || 0;
  const page = Math.floor(offset / PAGE_SIZE) + 1;
  const pages = Math.ceil(total / PAGE_SIZE);
  const go = next => {
    patch({ recordsQuery: { ...query, offset: next } });
    void loadRecords();
  };
  return el('div', { class: 'ck-pager' },
    button('上一页', {
      tone: 'ghost',
      disabled: offset <= 0,
      onClick: () => go(Math.max(0, offset - PAGE_SIZE)),
    }),
    el('span', { class: 'ck-pager-label', text: `第 ${page} / ${pages} 页` }),
    button('下一页', {
      tone: 'ghost',
      disabled: offset + PAGE_SIZE >= total,
      onClick: () => go(offset + PAGE_SIZE),
    }),
  );
}

export function renderRecords(container) {
  if (!store.recordsLoaded) {
    fill(container, el('div', { class: 'ck-loading' }, '正在读取签到记录…'));
    void loadRecords();
    return;
  }
  fill(container, el('div', { class: 'ck-main' },
    el('section', { class: 'panel' },
      el('div', { class: 'panel-head' },
        el('h2', { text: '签到记录' }),
        el('span', { class: 'panel-sub', text: '每账号一条明细 · 失败时给上游原文' }),
      ),
      filtersBar(),
      recordsTable(),
      pager(),
    ),
  ));
}

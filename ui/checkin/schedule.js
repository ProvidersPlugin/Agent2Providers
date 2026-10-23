/**
 * 「定时签到」标签页：总开关 + 条目列表 + 保活模型链 + 猫猫旅行排程。
 *
 * ── 编排在网关，页面只是视图 ────────────────────────────────
 * 一条定时条目可能横跨九家提供商；卸载插件不能让用户丢排程。所以排程、去重、
 * 错过补签全在网关（`core::auto_checkin` / `core::checkin_schedules`），页面
 * 只负责编辑与触发。
 *
 * ── 两块「地区专属」面板 ────────────────────────────────────
 * 保活模型链（国际版）与猫猫旅行排程（国内版）由插件申报的 `admin_panels`
 * 决定显不显示：没装插件（或装的是另一个地区的版本）时这两块不出现。
 */

import { badge, button, el, fill, formatClock, formatTime, icon, toggle } from './dom.js';
import {
  loadSchedules, patch, removeScheduleEntry, runScheduleEntry, saveKeepaliveModels,
  saveScheduleEntry, saveTravel, runTravelNowOnce, store, toggleMaster,
} from './state.js';

/** 这一条条目覆盖的提供商展示名（`id → label` 来自 `providerOptions`） */
function providerNames(entry, options) {
  const lookup = new Map(options.map(item => [item.id, item.label]));
  return entry.providers.map(id => lookup.get(id) || id);
}

/* ─── 编辑表单 ───────────────────────────────── */

/**
 * 条目编辑表单。
 *
 * `entry` 为 null = 新建。时刻用 `<input type="time">`：浏览器自带校验与
 * 键盘支持，比自绘选择器省事，而且移动端会弹原生时间轮。
 */
function editorForm(entry, options) {
  const isNew = !entry;
  const draft = {
    label: entry?.label || '',
    time: entry?.time || '09:00',
    enabled: entry ? entry.enabled : true,
    providers: entry ? [...entry.providers] : options.map(item => item.id),
  };
  const form = el('div', { class: 'ck-editor' });

  const labelInput = el('input', {
    type: 'text',
    class: 'ck-input',
    value: draft.label,
    placeholder: '条目名（留空则显示时刻）',
    onInput: event => { draft.label = event.target.value; },
  });
  const timeInput = el('input', {
    type: 'time',
    class: 'ck-input ck-input-time',
    value: draft.time,
    onInput: event => { draft.time = event.target.value; },
  });
  const enabledInput = toggle(draft.enabled, {
    onChange: value => { draft.enabled = value; },
  });

  form.append(el('div', { class: 'ck-form-line' },
    el('span', { class: 'ck-form-key', text: '名称' }), labelInput,
  ));
  form.append(el('div', { class: 'ck-form-line' },
    el('span', { class: 'ck-form-key', text: '时刻' }), timeInput,
    el('span', { class: 'ck-form-note', text: '北京时间 · 错过时点开机后自动补签' }),
  ));
  form.append(el('div', { class: 'ck-form-line' },
    el('span', { class: 'ck-form-key', text: '启用' }), enabledInput,
  ));

  // 签到范围：逐家勾选（默认全选）。全选 / 清空是文字按钮，不抢「保存」的注意力
  const rangeBox = el('div', { class: 'ck-range' });
  const repaintRange = () => {
    fill(rangeBox, ...options.map(option => {
      const checked = draft.providers.includes(option.id);
      const input = el('input', {
        type: 'checkbox',
        class: 'ck-check',
        checked,
        onChange: event => {
          if (event.target.checked) {
            if (!draft.providers.includes(option.id)) draft.providers.push(option.id);
          } else {
            draft.providers = draft.providers.filter(id => id !== option.id);
          }
          repaintNote();
        },
      });
      return el('label', { class: 'ck-range-item' }, input,
        el('span', { class: 'ck-range-name', text: option.label, title: option.id }));
    }));
  };
  const note = el('span', { class: 'ck-mini-note' });
  const repaintNote = () => {
    note.textContent = `已选 ${draft.providers.length} / ${options.length} 家`;
  };
  repaintRange();
  repaintNote();

  form.append(el('div', { class: 'ck-form-line ck-form-top' },
    el('span', { class: 'ck-form-key', text: '范围' }),
    el('div', { class: 'ck-form-body' },
      el('div', { class: 'ck-mini-row' },
        button('全选', {
          tone: 'ghost',
          onClick: () => {
            draft.providers = options.map(item => item.id);
            repaintRange();
            repaintNote();
          },
        }),
        button('清空', {
          tone: 'ghost',
          onClick: () => {
            draft.providers = [];
            repaintRange();
            repaintNote();
          },
        }),
        note,
      ),
      rangeBox,
      draft.providers.length === 0
        ? el('div', { class: 'ck-mini-warn', text: '一家都不选时这条不会签到任何账号。' })
        : null,
    ),
  ));

  form.append(el('div', { class: 'ck-form-actions' },
    button('取消', { tone: 'ghost', onClick: () => patch({ editingId: null }) }),
    button(store.saving ? '保存中…' : '保存', {
      tone: 'default',
      disabled: store.saving,
      onClick: () => {
        if (!draft.time) {
          form.append(el('div', { class: 'ck-mini-warn', text: '时刻不能为空。' }));
          return;
        }
        void saveScheduleEntry({
          ...(isNew ? {} : { id: entry.id }),
          label: draft.label,
          time: draft.time,
          enabled: draft.enabled,
          providers: draft.providers,
        });
      },
    }),
  ));
  return form;
}

/* ─── 条目行 ─────────────────────────────────── */

function entryRow(entry, options) {
  const editing = store.editingId === entry.id;
  const running = store.runningScheduleId === entry.id;
  const names = providerNames(entry, options);
  const wrap = el('div', { class: editing ? 'ck-sched-row open' : 'ck-sched-row' });

  const main = el('div', { class: 'ck-sched-main' },
    el('span', { class: 'ck-sched-time', text: entry.time || '—' }),
    el('div', { class: 'ck-row-info' },
      el('div', { class: 'ck-row-name' },
        entry.label || entry.displayLabel || entry.time,
        entry.enabled ? null : badge('已停用', 'plain'),
        entry.lastFiredToday ? badge('今日已执行', 'ok') : null,
      ),
      el('div', {
        class: 'ck-row-desc ck-ellipsis',
        text: names.length ? names.join('、') : '未选任何提供商',
        title: names.join('、'),
      }),
    ),
    el('div', { class: 'ck-row-right' },
      entry.nextRunAt
        ? el('span', { class: 'ck-sched-next', text: `下次 ${formatClock(entry.nextRunAt)}` })
        : null,
      button(running ? '执行中…' : '立即执行', {
        tone: 'ghost',
        disabled: running || store.runningScheduleId !== null,
        onClick: () => void runScheduleEntry(entry.id),
      }),
      button(editing ? '收起' : '编辑', {
        tone: 'ghost',
        disabled: store.saving,
        onClick: () => patch({ editingId: editing ? null : entry.id }),
      }),
      button('', {
        tone: 'ghost',
        icon: 'trash',
        title: '删除这条条目',
        disabled: store.saving,
        onClick: () => {
          // 删除是破坏性动作：这里用页面内确认（宿主确认弹窗在 iframe 之外）
          if (!window.confirm(`删除定时条目「${entry.label || entry.displayLabel || entry.time}」？`)) return;
          void removeScheduleEntry(entry.id);
        },
      }),
    ),
  );
  wrap.append(main);

  // 上一次执行结果（有才显示）：失败列表直接列出账号名
  if (entry.lastResult && typeof entry.lastResult === 'object') {
    const result = entry.lastResult;
    const parts = [];
    if (result.succeeded !== undefined) parts.push(`成功 ${Number(result.succeeded) || 0}`);
    if (result.active) parts.push(`保活 ${Number(result.active) || 0}`);
    if (result.skipped) parts.push(`跳过 ${Number(result.skipped) || 0}`);
    if (result.failedCount) parts.push(`失败 ${Number(result.failedCount) || 0}`);
    const failed = Array.isArray(result.failed) && result.failed.length
      ? ` · 失败账号：${result.failed.join('、')}`
      : '';
    wrap.append(el('div', {
      class: 'ck-sched-last',
      text: `上次（${formatTime(result.at)}）${parts.join(' · ')}${failed}`,
    }));
  }

  if (editing) wrap.append(editorForm(entry, options));
  return wrap;
}

/* ─── 保活模型链（国际版）────────────────────── */

/**
 * 模型链编辑器：一行一个模型，顺序就是语义（按序尝试、第一个成功即止）。
 *
 * 拖动排序没有做：这页在 iframe 里、还要兼容触屏，拖拽的坑（长按、滚动
 * 抢占）比收益大。上移 / 下移两颗箭头足够，且对键盘与触屏都可靠。
 */
function keepalivePanel(snapshot) {
  const current = snapshot.keepalive;
  const models = store.keepaliveDraft ?? current.models;
  const defaultModels = current.defaultModels;
  const dirty = store.keepaliveDraft !== null
    && store.keepaliveDraft.join(',') !== current.models.join(',');

  const list = el('div', { class: 'ck-chain' });
  if (!models.length) {
    list.append(el('div', {
      class: 'ck-chain-empty',
      text: defaultModels.length
        ? `空清单 = 使用缺省链（${defaultModels.join(' → ')}）`
        : '空清单 = 使用缺省链',
    }));
  }
  models.forEach((model, index) => {
    const move = offset => {
      const next = [...models];
      const target = index + offset;
      if (target < 0 || target >= next.length) return;
      [next[index], next[target]] = [next[target], next[index]];
      patch({ keepaliveDraft: next });
    };
    list.append(el('div', { class: 'ck-chain-row' },
      el('span', { class: 'ck-chain-index', text: `#${index + 1}` }),
      el('span', { class: 'ck-chain-name', text: model, title: model }),
      el('span', { class: 'ck-chain-move' },
        button('', { tone: 'ghost', icon: 'chevron', title: '上移', disabled: index === 0, onClick: () => move(-1) }),
        button('', { tone: 'ghost', icon: 'chevron', title: '下移', disabled: index === models.length - 1, onClick: () => move(1) }),
      ),
      button('', {
        tone: 'ghost',
        icon: 'trash',
        title: '移除',
        onClick: () => patch({ keepaliveDraft: models.filter((_, item) => item !== index) }),
      }),
    ));
  });

  const input = el('input', {
    type: 'text',
    class: 'ck-input',
    placeholder: '模型名（回车添加）',
    onKeydown: event => {
      if (event.key !== 'Enter') return;
      const value = event.target.value.trim();
      if (!value) return;
      if (models.includes(value)) {
        event.target.value = '';
        return;
      }
      patch({ keepaliveDraft: [...models, value] });
      event.target.value = '';
    },
  });

  const body = el('div', { class: 'ck-panel-body' },
    list,
    el('div', { class: 'ck-chain-add' }, input,
      dirty
        ? el('span', { class: 'ck-chain-dirty', text: '有未保存的改动' })
        : null,
      dirty
        ? button('保存', {
          tone: 'default',
          onClick: () => void saveKeepaliveModels(models),
        })
        : null,
      dirty
        ? button('还原', { tone: 'ghost', onClick: () => patch({ keepaliveDraft: null }) })
        : null,
    ),
  );

  return el('section', { class: 'ck-panel ck-panel-half' },
    el('div', { class: 'ck-panel-head' },
      el('h2', { text: '保活模型链' }),
      el('span', {
        class: 'ck-panel-sub',
        text: '按顺序逐个尝试，第一个成功的即止 · 空清单用缺省链',
      }),
    ),
    body,
  );
}

/* ─── 猫猫旅行排程（国内版）──────────────────── */

function travelPanel(snapshot) {
  const travel = snapshot.travel;
  const times = store.travelTimesDraft ?? travel.times;
  const dirty = store.travelTimesDraft !== null
    && store.travelTimesDraft.join(',') !== travel.times.join(',');

  const timesRow = el('div', { class: 'ck-range' },
    ...times.map((time, index) => el('span', { class: 'ck-range-item' },
      el('span', { class: 'ck-time-chip', text: time }),
      button('', {
        tone: 'ghost',
        icon: 'trash',
        title: '移除',
        onClick: () => patch({ travelTimesDraft: times.filter((_, item) => item !== index) }),
      }),
    )),
  );
  const timeInput = el('input', {
    type: 'time',
    class: 'ck-input ck-input-time',
    onChange: event => {
      const value = event.target.value;
      if (!value) return;
      if (times.includes(value)) return;
      patch({ travelTimesDraft: [...times, value].sort() });
      event.target.value = '';
    },
  });

  const last = travel.lastResult;
  const lastText = last
    ? `上一轮（${last.lastFiredTime || formatTime(last.at)}）账号 ${Number(last.accounts) || 0} · `
      + `动作 ${Array.isArray(last.actions) ? last.actions.length : 0} · 积分 ${Number(last.credit) || 0}`
      + (last.failed ? ` · 失败 ${Number(last.failed) || 0}` : '')
    : '还没有执行过';

  return el('section', { class: 'ck-panel ck-panel-half' },
    el('div', { class: 'ck-panel-head' },
      el('h2', { text: '猫猫旅行' }),
      el('span', { class: 'ck-panel-sub', text: '与签到排程相互独立 · 北京时间' }),
      el('span', { class: 'ck-panel-actions' },
        toggle(travel.enabled, {
          onChange: value => void saveTravel({ enabled: value }),
        }),
      ),
    ),
    el('div', { class: 'ck-panel-body' },
      el('div', { class: 'ck-mini-row' },
        el('span', { class: 'ck-form-key', text: '时刻' }),
        timesRow,
        timeInput,
      ),
      dirty
        ? el('div', { class: 'ck-mini-row' },
          el('span', { class: 'ck-chain-dirty', text: '有未保存的改动' }),
          button('保存', { tone: 'default', onClick: () => void saveTravel({ times }) }),
          button('还原', { tone: 'ghost', onClick: () => patch({ travelTimesDraft: null }) }),
        )
        : null,
      el('div', { class: 'ck-fold-note', text: lastText }),
      el('div', { class: 'ck-mini-row' },
        button(travel.running ? '执行中…' : '立即执行一轮', {
          tone: 'outline',
          disabled: travel.running,
          onClick: () => void runTravelNowOnce(),
        }),
      ),
    ),
  );
}

/* ─── 渲染入口 ───────────────────────────────── */

export function renderSchedule(container) {
  if (!store.schedulesLoaded) {
    fill(container, el('div', { class: 'ck-loading' }, '正在读取定时签到数据…'));
    void loadSchedules();
    return;
  }
  const data = store.schedules;
  const snapshot = store.snapshot;
  const options = data?.providerOptions || [];
  const entries = data?.schedules || [];
  const panels = new Set(snapshot?.adminPanels || []);

  const head = el('div', { class: 'ck-panel-head' },
    el('h2', { text: '定时签到' }),
    el('span', { class: 'ck-panel-sub', text: '每条条目各有时刻与范围 · 各自当天去重 · 错过时点开机自动补签' }),
    el('span', { class: 'ck-panel-actions' },
      button(store.editingId === 'new' ? '收起新建' : '添加条目', {
        tone: 'outline',
        icon: 'plus',
        disabled: store.saving,
        onClick: () => patch({ editingId: store.editingId === 'new' ? null : 'new' }),
      }),
    ),
  );

  const masterLine = el('div', { class: 'ck-set-line' },
    el('span', { class: 'ck-set-k', text: '总开关' }),
    el('span', { class: 'ck-set-v' },
      toggle(data?.enabled === true, {
        disabled: store.saving,
        onChange: value => void toggleMaster(value),
      }),
      el('span', {
        class: 'ck-sub',
        text: data?.enabled === true ? '开着：启用的条目按各自时刻执行' : '关着：所有条目都不执行',
      }),
    ),
  );

  const list = el('div', {});
  if (store.editingId === 'new') list.append(editorForm(null, options));
  for (const entry of entries) list.append(entryRow(entry, options));
  if (!entries.length && store.editingId !== 'new') {
    list.append(el('div', {
      class: 'ck-empty',
      text: '还没有定时条目 —— 点「添加条目」建一条，或到「签到中心」手动签到。',
    }));
  }

  const children = [
    el('section', { class: 'ck-panel' }, head, el('div', { class: 'ck-panel-body' }, masterLine), list),
  ];
  // 两块地区专属面板：由插件申报的 admin_panels 决定
  if (snapshot && panels.has('daily_keepalive')) children.push(keepalivePanel(snapshot));
  if (snapshot && panels.has('growth_tasks')) children.push(travelPanel(snapshot));
  fill(container, el('div', { class: 'ck-main' }, ...children));
}

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

import { badge, button, checkbox, el, fill, formatClock, formatTime, setDisabled, textInput, toggle } from './dom.js';
import {
  loadSchedules, patch, removeScheduleEntry, runScheduleEntry, saveKeepaliveModels,
  saveScheduleEntry, saveTravel, runTravelNowOnce, store, toast, toggleMaster,
} from './state.js';

/** 这一条条目覆盖的提供商展示名（`id → label` 来自 `providerOptions`） */
function providerNames(entry, options) {
  const lookup = new Map(options.map(item => [item.id, item.label]));
  return entry.providers.map(id => lookup.get(id) || id);
}

/**
 * 范围的**摘要**文案（条目行第二行）。
 *
 * 全选时说「全部 N 家」而不是把十几个名字串起来 —— 旧实现如此
 * （`全部 {n} 家`）。名字一长，这一行会被挤成两三行，而全选是最常见的情况。
 */
function scopeSummary(entry, options) {
  const names = providerNames(entry, options);
  if (!names.length) return '未勾选提供商';
  if (options.length > 0 && names.length >= options.length) return `全部 ${options.length} 家`;
  return names.join('、');
}

/** 条目行第二行（范围全貌，用于 title 悬停） */
function scopeFull(entry, options) {
  const names = providerNames(entry, options);
  return names.length ? names.join('、') : '未勾选提供商';
}

/**
 * 「上次执行」那一行。
 *
 * 旧实现的形状：`时间 · 原因 —— 成功 N · 保活 N · 失败 N`。
 * 只列**有值**的计数（`保活 0` 不该出现在没跑保活的条目上），失败为 0 时整段省略。
 * 没跑过给「还没执行过」而不是留空 —— 空行会让人以为结果丢了。
 */
function lastResultLine(entry) {
  const result = entry.lastResult;
  if (!result || typeof result !== 'object') return '还没执行过';
  const count = key => Number(result[key]) || 0;
  const parts = [`成功 ${count('succeeded')}`];
  if (count('active') > 0) parts.push(`保活 ${count('active')}`);
  if (count('failedCount') > 0) parts.push(`失败 ${count('failedCount')}`);
  const at = result.at ? formatTime(result.at) : '—';
  const reason = result.reason ? ` · ${result.reason}` : '';
  return `${at}${reason} —— ${parts.join(' · ')}`;
}

/* ─── 编辑表单 ───────────────────────────────── */

/**
 * 条目编辑表单。
 *
 * `entry` 为 null = 新建。时刻用 `<input type="time">`：浏览器自带校验与
 * 键盘支持，比自绘选择器省事，而且移动端会弹原生时间轮。
 *
 * ── 与旧实现（checkin-schedule-tab.tsx 的 ScheduleEditor）逐项对齐 ──
 *   · 三行 `ck-set-line`：名称 / 每天时刻 / 签到范围（第三行顶对齐）；
 *   · 名称与时刻的宽度上限写在 `className` 上（`max-w-[220px]` / `w-[96px]`），
 *     与旧实现同一组工具类；
 *   · 范围那行先一行 `ck-mini-row`（计数 + 全选 + 清空两个文字按钮），
 *     再是复选框网格；网格是 `repeat(auto-fill, minmax(132px, 1fr))`，
 *     不是 flex-wrap —— 后者每项宽度随文字长短不齐，扫读时列不对齐；
 *   · 底部 `ck-sched-actions`（右对齐两个按钮），不是 `ck-form-actions`；
 *   · 一家都没勾时给 `ck-mini-warn`，且**保存按钮禁用**（旧实现的 `canSave`）。
 */
function editorForm(entry, options) {
  const isNew = !entry;
  const draft = {
    label: entry?.label || '',
    // 新建默认 00:01：与后端 `DEFAULT_TIME` 同一个值，不在这里另立一个
    time: entry?.time || '00:01',
    enabled: entry ? entry.enabled : true,
    providers: entry ? [...entry.providers] : options.map(item => item.id),
  };
  const form = el('div', { class: 'ck-sched-editor' });

  const labelInput = textInput({
    type: 'text',
    value: draft.label,
    placeholder: '可选 · 缺省显示时刻',
    className: 'ck-sched-name',
    disabled: store.saving,
    onInput: value => { draft.label = value; },
  });
  const timeInput = textInput({
    type: 'time',
    value: draft.time,
    className: 'ck-sched-time-input font-mono tabular-nums',
    disabled: store.saving,
    onInput: value => { draft.time = value; },
  });

  form.append(el('div', { class: 'ck-set-line' },
    el('span', { class: 'ck-set-k', text: '名称' }),
    el('span', { class: 'ck-set-v' }, labelInput),
  ));
  form.append(el('div', { class: 'ck-set-line' },
    el('span', { class: 'ck-set-k', text: '每天时刻' }),
    el('span', { class: 'ck-set-v' }, timeInput,
      el('span', { class: 'text-subtle text-xs', text: '北京时间 · 错过时点开机后会自动补签' })),
  ));

  // 签到范围：逐家勾选（默认全选）。全选 / 清空是文字按钮，不抢「保存」的注意力
  const rangeBox = el('div', { class: 'ck-range' });
  const repaintRange = () => {
    fill(rangeBox, ...options.map(option => el('label', { class: 'ck-range-item' },
      checkbox(draft.providers.includes(option.id), {
        onChange: checked => {
          if (checked) {
            if (!draft.providers.includes(option.id)) draft.providers.push(option.id);
          } else {
            draft.providers = draft.providers.filter(id => id !== option.id);
          }
          repaintNote();
        },
      }),
      el('span', { class: 'ck-range-name', text: option.label, title: option.label }),
    )));
  };
  const note = el('span', { class: 'ck-mini-note' });
  const warn = el('div', { class: 'ck-mini-warn', text: '至少勾一家才能保存（不勾等于这条条目不签到）' });
  const saveButton = button('保存', {
    tone: 'default',
    disabled: store.saving,
    onClick: () => {
      void saveScheduleEntry({
        ...(isNew ? {} : { id: entry.id }),
        label: draft.label.trim(),
        time: draft.time.trim(),
        enabled: draft.enabled,
        providers: options.map(item => item.id).filter(id => draft.providers.includes(id)),
      });
    },
  });
  /** 计数 / 警告 / 保存按钮三处的可用性一起刷新（旧实现的 `canSave` 一处判定） */
  const repaintNote = () => {
    note.textContent = `到点时逐个签到，已勾 ${draft.providers.length} / ${options.length} 家`;
    warn.hidden = draft.providers.length > 0;
    setDisabled(saveButton, store.saving || draft.providers.length === 0);
  };
  repaintRange();
  repaintNote();

  form.append(el('div', { class: 'ck-set-line ck-set-line-top' },
    el('span', { class: 'ck-set-k', text: '签到范围' }),
    el('span', { class: 'ck-set-v' },
      el('div', { class: 'ck-mini-row' },
        note,
        el('button', {
          type: 'button',
          class: 'ck-mini-btn',
          disabled: store.saving,
          text: '全选',
          onClick: () => {
            draft.providers = options.map(item => item.id);
            repaintRange();
            repaintNote();
          },
        }),
        el('button', {
          type: 'button',
          class: 'ck-mini-btn',
          disabled: store.saving || draft.providers.length === 0,
          text: '清空',
          onClick: () => {
            draft.providers = [];
            repaintRange();
            repaintNote();
          },
        }),
      ),
      rangeBox,
      warn,
    ),
  ));

  form.append(el('div', { class: 'ck-sched-actions' },
    button('取消', { tone: 'outline', disabled: store.saving, onClick: () => patch({ editingId: null }) }),
    saveButton,
  ));
  return form;
}

/* ─── 条目行 ─────────────────────────────────── */

/**
 * 一条已存条目。
 *
 * ── 结构照旧实现（checkin-schedule-tab.tsx 的 ScheduleRow）────────
 *   · 行首是**条目自己的开关**（不是「时刻」大字）—— 一条条目最常改的就是
 *     开关，藏在编辑表单里要多点两步；
 *   · 三行文字：名称（+「已停用」标）→ `每天 HH:MM · 范围` → `上次执行：…`；
 *   · 右侧：下次执行时刻（胶囊）+ 立即执行 / 编辑 / 删除三个按钮；
 *   · 删除是带 `text-destructive` 的文字按钮，不是垃圾桶图标。
 */
function entryRow(entry, options) {
  const editing = store.editingId === entry.id;
  const running = store.runningScheduleId === entry.id;
  const masterOn = store.schedules?.enabled === true;
  const wrap = el('div', { class: editing ? 'ck-sched-row open' : 'ck-sched-row' });

  const nextRun = Number(entry.nextRunAt);
  const nextText = masterOn && entry.enabled && Number.isFinite(nextRun) && nextRun > 0
    ? `下次 ${formatClock(nextRun)}`
    : '—';

  const main = el('div', { class: 'ck-sched-main' },
    toggle(entry.enabled, {
      disabled: store.saving,
      // 只改开关：其余字段原样回传（后端 upsert 是全量覆盖）。
      // `closeEditor: false` —— 这是行上的快捷开关，不是「编辑完成」，
      // 不该把用户正开着的编辑表单一起收起来；也不弹提示（开关本身就是反馈）。
      onChange: value => void saveScheduleEntry({
        id: entry.id,
        label: entry.label ?? '',
        enabled: value,
        time: entry.time ?? '',
        providers: Array.isArray(entry.providers) ? entry.providers : [],
      }, { closeEditor: false, toast: false }),
    }),
    el('div', { class: 'ck-prov-info' },
      el('div', { class: 'ck-prov-name' },
        entry.displayLabel || entry.time || entry.id,
        entry.enabled ? null : badge('已停用', 'plain', '', 'tag'),
      ),
      el('div', {
        class: 'ck-prov-desc ck-sched-scope',
        text: `每天 ${entry.time || '—'} · ${scopeSummary(entry, options)}`,
        title: scopeFull(entry, options),
      }),
      el('div', { class: 'ck-prov-desc', text: `上次执行：${lastResultLine(entry)}` }),
    ),
    el('div', { class: 'ck-prov-right' },
      el('span', { class: 'ck-sched-next', text: nextText }),
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
      button('删除', {
        tone: 'ghost',
        className: 'text-destructive',
        disabled: store.saving,
        onClick: () => {
          // 删除是破坏性动作：这里用页面内确认（宿主确认弹窗在 iframe 之外）
          if (!window.confirm(`确定删除「${entry.displayLabel || entry.time || entry.id}」？删除后到点不再执行。`)) return;
          void removeScheduleEntry(entry.id);
        },
      }),
    ),
  );
  wrap.append(main);

  if (editing) wrap.append(editorForm(entry, options));
  return wrap;
}

/**
 * 新建条目那一行。
 *
 * 落在列表**首位**（不是末尾）：点「添加条目」后视线停在卡头，新行跟着出现在
 * 它下面才接得上；落到底部时用户会以为没反应。左侧一枚虚线 `＋` 与已存条目区分。
 */
function newEntryRow(options) {
  return el('div', { class: 'ck-sched-row open ck-sched-row-new' },
    el('div', { class: 'ck-sched-main' },
      el('span', { class: 'ck-new-dot', 'aria-hidden': 'true', text: '＋' }),
      el('div', { class: 'ck-prov-info' },
        el('div', { class: 'ck-prov-name', text: '新建定时条目' }),
        el('div', {
          class: 'ck-prov-desc',
          text: '填好时刻与签到范围后点「保存」；名称可以留空（界面显示时刻）',
        }),
      ),
    ),
    editorForm(null, options),
  );
}

/* ─── 保活模型链（国际版）────────────────────── */

/** 猫猫旅行的问号提示全文（与旧实现同一段话） */
const TRAVEL_TIP = '每天按这里的时刻自动巡检一遍 WorkBuddy 国内版账号：没有猫就领养、'
  + '在家就派出、到站就领奖。一趟只推进一步，所以建议留两个时刻'
  + '（早上派出、晚上领奖）。企业版与国际版账号自动跳过。';

/** 「定时签到」卡头的问号提示：逐家说明到点时走哪条链路 */
const SCHEDULE_TIP = '到点后自动签到勾选提供商的可用账号'
  + '（WorkBuddy 国内版走每日签到接口，国际版走每日活跃任务：探测活动、领日活奖励、'
  + '再用免费模型保活并在网页通道建会话点亮日活；小浣熊走桌面端每日积分链路；'
  + 'AutoClaw 走官方客户端的每日签到任务；Qoder 中国版走活动领取，'
  + '没被下发活动的账号会得到中性提示；Qoder 国际版要先带设备风控身份'
  + '（本机 Qoder 客户端 / qodercli 的 UMID 组件生成）才能看到「每日 100 Credits」活动，'
  + '缺组件时会得到明确的说明而不是报错；Trae 领 SOLO 的每日签到积分'
  + '（按自然日 0 点重置），上游没对该账号开活动、或凭据里没有设备号时'
  + '得到的是「未领取 + 具体原因」而不是报错；Loomy 每天替账号打一次首次登录积分；'
  + 'KukuAI 领「免费领积分」的每日任务；CodeArts 领当日的运营福利'
  + '（领取 → 确认 → 回读核实，自动执行受每天 6 次 / 间隔 10 分钟的写请求限流）；'
  + 'OfficeAce 果办一次领当天全部奖励（每日签到 + 新人礼））。'
  + '错过时点开机后会自动补签，不会因为当时没开机而漏掉。'
  + '各家签到接口都是幂等的，重复执行不会重复领取。';

/** 两份清单是否逐项相同（判「草稿有没有真改过」） */
function sameModels(a, b) {
  return a.length === b.length && a.every((item, index) => item === b[index]);
}

/**
 * 拖动状态放在模块级：指针监听挂在 window 上，回调里拿不到闭包。
 *
 * ── 为什么拖动期间不动草稿 ──────────────────────────────────
 * 改草稿会 `patch()` → 整页重绘 → 清单的 DOM 被整块替换。而拖动的落点判定
 * 要拿**当前这棵** DOM 的行位置来比 —— 一旦重绘，手里那个行节点就成了脱离
 * 文档的孤儿（`getBoundingClientRect()` 全 0），后续每次移动都算错位。
 * 早先的实现就是每移动一次就重绘一次，结果只有第一次交换生效、之后拖动失灵。
 *
 * 现在拖动期间只做一件事：把被拖那一行在 DOM 里 `insertBefore` 到新位置。
 * 指针松开时才把最终顺序交给 `onChange` 写进草稿 —— 那一次重绘发生在收尾，
 * 不影响任何后续判定。
 */
let chainDrag = null;

/** 指针落在第几位：拿其余各行的中线比 */
function chainInsertIndex(others, clientY) {
  for (let index = 0; index < others.length; index += 1) {
    const rect = others[index].getBoundingClientRect();
    if (clientY < rect.top + rect.height / 2) return index;
  }
  return others.length;
}

function onChainPointerMove(event) {
  const state = chainDrag;
  if (!state) return;
  // 指针中途被放开（没接到 pointerup）时收尾，别留一个卡住的拖动态
  if (event.buttons === 0) { finishChainDrag(); return; }
  const rows = [...state.container.querySelectorAll('.ck-chain-row')];
  const others = rows.filter(row => row !== state.row);
  const target = chainInsertIndex(others, event.clientY);
  // 落点没变就不动 DOM：`insertBefore` 到原地会触发一次无谓的重排，
  // 而且会把被拖行从它当前的位置摘掉再插回（视觉上抖一下）
  const reference = others[target] || null;
  if (reference === state.row.nextElementSibling) return;
  if (reference === null && state.row === state.container.lastElementChild) return;
  state.container.insertBefore(state.row, reference);
}

function finishChainDrag() {
  const state = chainDrag;
  chainDrag = null;
  window.removeEventListener('pointermove', onChainPointerMove);
  window.removeEventListener('pointerup', finishChainDrag);
  window.removeEventListener('pointercancel', finishChainDrag);
  document.body.classList.remove('ck-chain-dragging');
  if (!state) return;
  state.row.classList.remove('dragging');
  // 行上记着它在**原数组**里的下标：DOM 顺序读回来就是新的模型顺序
  const order = [...state.container.querySelectorAll('.ck-chain-row')]
    .map(row => Number(row.dataset.chainIndex))
    .filter(index => Number.isInteger(index))
    .map(index => state.models[index]);
  const changed = order.length === state.models.length
    && order.some((model, index) => model !== state.models[index]);
  if (changed) state.onChange(order);
}

/**
 * 按住把手起拖。把手是**唯一**的拖动起点 —— 行里还有移除按钮，
 * 整行可拖会把「点移除」变成「拖走一行」。
 */
function startChainDrag(event, index, models, onChange) {
  if (event.button !== 0 || chainDrag) return;
  if (models.length < 2) return;
  const row = event.currentTarget.closest('.ck-chain-row');
  const container = row?.parentElement;
  if (!row || !container) return;
  event.preventDefault();
  chainDrag = { container, models: [...models], row, onChange };
  row.classList.add('dragging');
  document.body.classList.add('ck-chain-dragging');
  window.addEventListener('pointermove', onChainPointerMove);
  window.addEventListener('pointerup', finishChainDrag);
  window.addEventListener('pointercancel', finishChainDrag);
}

/** 拖动把手图标（六点两列，与账号页顺序拖拽同一枚） */
function gripIcon() {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('width', '14');
  svg.setAttribute('height', '14');
  svg.setAttribute('fill', 'currentColor');
  svg.setAttribute('aria-hidden', 'true');
  // 六个圆点：拖动把手的通用记号
  svg.innerHTML = '<circle cx="9" cy="6" r="1.5"/><circle cx="15" cy="6" r="1.5"/>'
    + '<circle cx="9" cy="12" r="1.5"/><circle cx="15" cy="12" r="1.5"/>'
    + '<circle cx="9" cy="18" r="1.5"/><circle cx="15" cy="18" r="1.5"/>';
  return svg;
}

/**
 * 保活模型链面板。
 *
 * ── 结构照旧实现（checkin-schedule-tab.tsx 的 KeepalivePanel）────
 *   · 半幅宽（`.ck-chain-panel` = 50% / max 640px）：里面只有一列短清单，
 *     拉满整行时那排灰条会长得没有内容；
 *   · 保存按钮在**卡头**右侧，不是清单底部；
 *   · 清单上方一行：计数 + 「填入缺省链」文字按钮；
 *   · 每行：拖动把手 → 模型名（等宽）→ `#N` → `×`；
 *   · 底部一块 `ck-note` 说明（缺省链是谁、清空 = 恢复缺省）。
 */
function keepalivePanel(snapshot) {
  const current = snapshot.keepalive;
  const models = store.keepaliveDraft ?? current.models;
  const defaultModels = current.defaultModels || [];
  const dirty = store.keepaliveDraft !== null && !sameModels(store.keepaliveDraft, current.models);
  const saving = store.keepaliveSaving === true;

  const setDraft = next => patch({ keepaliveDraft: next });

  const list = el('div', { class: 'ck-chain' });
  if (!models.length) {
    list.append(el('div', { class: 'ck-chain-empty', text: '清单为空 —— 保存后恢复缺省链' }));
  }
  models.forEach((model, index) => {
    const grip = el('span', {
      class: 'ck-chain-grip',
      role: 'button',
      title: '按住拖动，调整这个模型在链里的顺序',
      'aria-label': '按住拖动调整顺序',
    }, gripIcon());
    grip.addEventListener('pointerdown', event => startChainDrag(event, index, models, setDraft));
    // `data-chain-index` 记的是**原数组**下标：拖动收尾时按 DOM 顺序读回来，
    // 就得到新的模型顺序（见 finishChainDrag）
    list.append(el('div', { class: 'ck-chain-row', 'data-chain-index': index },
      grip,
      el('span', { class: 'ck-chain-name', text: model, title: model }),
      el('span', { class: 'ck-chain-index', text: `#${index + 1}` }),
      el('button', {
        type: 'button',
        class: 'ck-chain-del',
        disabled: saving,
        title: '从清单里移除',
        'aria-label': `移除 ${model}`,
        text: '×',
        onClick: () => setDraft(models.filter((_, keep) => keep !== index)),
      }),
    ));
  });

  // 「添加模型」按钮的可用性跟着输入框内容走。旧实现是受控组件（每次按键重渲染），
  // 这里是命令式 DOM —— 输入框内容不触发重绘，所以手动同步这一处。
  const addButton = button('添加模型', { tone: 'outline', disabled: true, onClick: () => addModel() });
  const input = textInput({
    type: 'text',
    placeholder: 'WorkBuddy 国际版保活模型，按顺序回退',
    className: 'ck-chain-input font-mono text-xs',
    disabled: saving,
    onInput: value => setDisabled(addButton, saving || value.trim() === ''),
    onKeydown: event => {
      if (event.key !== 'Enter') return;
      event.preventDefault();
      addModel();
    },
  });

  function addModel() {
    const name = input.value.trim();
    if (!name) return;
    if (models.includes(name)) {
      toast(`清单里已经有「${name}」`, 'err');
      return;
    }
    setDraft([...models, name]);
    input.value = '';
  }

  return el('section', { class: 'panel ck-chain-panel' },
    el('div', { class: 'panel-head' },
      el('h2', { text: '保活模型链' }),
      el('span', { class: 'panel-sub', text: 'WorkBuddy 国际版日活任务的免费模型 · 全局一份，与条目无关' }),
      el('span', { class: 'head-actions' },
        button(saving ? '保存中…' : '保存', {
          tone: 'default',
          disabled: !dirty || saving,
          onClick: () => void saveKeepaliveModels(models),
        }),
      ),
    ),
    el('div', { class: 'ck-panel-pad' },
      el('div', { class: 'ck-set-line ck-set-line-top' },
        el('span', { class: 'ck-set-k', text: '模型清单' }),
        el('span', { class: 'ck-set-v' },
          el('div', { class: 'ck-mini-row' },
            el('span', { class: 'ck-mini-note', text: `按序逐个尝试，共 ${models.length} 个` }),
            el('button', {
              type: 'button',
              class: 'ck-mini-btn',
              disabled: saving || defaultModels.length === 0,
              title: '把缺省链填进清单，再点「保存」生效',
              text: '填入缺省链',
              onClick: () => setDraft([...defaultModels]),
            }),
          ),
          list,
          el('div', { class: 'ck-chain-add' },
            input,
            addButton,
            dirty ? el('span', { class: 'ck-chain-dirty', text: '有未保存的改动' }) : null,
          ),
        ),
      ),
      el('div', { class: 'ck-note' },
        '「保活模型链」是 WorkBuddy 国际版领日活时用来保活的免费模型，按顺序逐个尝试、'
        + '第一个成功即止；清空清单并保存 = 恢复缺省链（当前缺省：'
        + (defaultModels.length ? defaultModels.join('、') : '（无）')
        + '）。当天去重 + 幂等领取：重复执行只会拿到「已领取」，不会重复加分。',
      ),
    ),
  );
}

/* ─── 猫猫旅行排程（国内版）──────────────────── */

/**
 * 猫猫旅行定时排程面板（国内版）。
 *
 * ── 结构照旧实现（checkin-schedule-tab.tsx 的 TravelPanel）────
 *   · 卡头：标题 + `tip-q` 问号（悬停看长说明）+ 副标题 + 「立即巡检全部」；
 *   · 两行 `ck-set-line`：开关、时刻；
 *   · **时刻是一个逗号分隔的文本框**，不是逐个时刻的清单 —— 旧实现就是这么做的：
 *     `09:00, 21:00` 一行写完，保存时后端归一化（排序去重、校验范围）。
 *     早先这里改成了「逐个时刻 chip + 删除」，比原来多好几步操作，
 *     而且没有对应的旧样式可依；
 *   · 底部两块 `ck-fold-note`：规则说明（含今天已执行的时刻）、上一轮摘要。
 *
 * ── 为什么与「定时签到」分卡 ────────────────────────────────
 * 两个排程完全独立（`autoCheckin` vs `growthSchedule`）：用户可能只想自动
 * 签到、不想自动派猫，或反过来。合成一张卡会让「关掉旅行」变成「关掉签到」
 * 的副作用。
 */
function travelPanel(snapshot) {
  const travel = snapshot.travel;
  const times = travel.times || [];
  const fired = travel.firedToday || [];
  const saving = store.travelSaving === true;
  const running = store.travelRunning === true;

  // 草稿：null = 跟随快照（用户没在编辑）。与旧实现同一套判定 ——
  // 去掉空白后与快照相同就不算改动。
  let draft = store.travelTimesDraft;
  const text = draft ?? times.join(', ');
  const dirty = draft !== null && draft.replace(/\s+/g, '') !== times.join(',');

  const input = textInput({
    type: 'text',
    value: text,
    placeholder: '09:00, 21:00',
    className: 'ck-travel-times',
    disabled: saving,
    onInput: value => patch({ travelTimesDraft: value }),
  });

  const submit = () => {
    const list = String(input.value || '')
      .split(/[,，\s]+/)
      .map(item => item.trim())
      .filter(Boolean);
    // 草稿由 `saveTravel` 自己清（成功时清、失败时留着让用户改）
    void saveTravel({ times: list });
  };

  const last = travel.lastResult;
  const actions = Array.isArray(last?.actions) ? last.actions : [];

  return el('section', { class: 'panel' },
    el('div', { class: 'panel-head' },
      el('h2', { text: '猫猫旅行定时排程' }),
      // 问号提示：`data-tip` 是宿主 tooltip.js 的契约（那个脚本不进 iframe），
      // 所以同时给一个原生 `title` —— 悬停一样能看到全文，只是气泡样式朴素些。
      el('span', {
        class: 'tip-q',
        'data-tip': TRAVEL_TIP,
        title: TRAVEL_TIP,
        text: '?',
      }),
      el('span', { class: 'panel-sub', text: '每天定点巡检 · 一趟推进一步 · 与签到排程互相独立' }),
      el('span', { class: 'head-actions' },
        button(running ? '巡检中…' : '立即巡检全部', {
          tone: 'outline',
          disabled: running,
          onClick: () => void runTravelNowOnce(),
        }),
      ),
    ),
    el('div', { class: 'ck-panel-pad' },
      el('div', { class: 'ck-set-line' },
        el('span', { class: 'ck-set-k', text: '开关' }),
        el('span', { class: 'ck-set-v ck-set-row' },
          toggle(travel.enabled, {
            disabled: saving,
            onChange: value => void saveTravel({ enabled: value }),
          }),
          el('span', {
            class: 'ck-sub',
            text: travel.enabled
              ? '开着：按下面的时刻自动巡检'
              : '关着：不自动巡检（仍可手动巡检）',
          }),
        ),
      ),
      el('div', { class: 'ck-set-line' },
        el('span', { class: 'ck-set-k', text: '时刻' }),
        el('span', { class: 'ck-set-v ck-set-row' },
          input,
          button('保存', {
            tone: 'outline',
            disabled: !dirty || saving,
            onClick: submit,
          }),
        ),
      ),
      // 两块说明用 `ck-note`（卡内下沉块）而不是 `ck-fold-note`（贴卡片下缘的
      // 条带）：它们**在** `.ck-panel-pad` 里，而条带样式自带 16px 左右内边距 +
      // 上边框，套在内边距里会多缩进一层、还多画一条横线。
      el('div', { class: 'ck-note' },
        '北京时间 · 逗号分隔 · 最多 6 条 · 到点后最多晚 30 秒执行；错过时点开机后会补跑一轮。'
          + (fired.length ? ` 今天已执行：${fired.join(' / ')}` : ''),
      ),
      last && actions.length
        ? el('div', { class: 'ck-note' },
          `上一轮（${last.at ? formatTime(Number(last.at)) : ''}）：`
            + `${actions.length} 个动作，+${Number(last.credit) || 0} 积分`
            + actions.slice(0, 6)
              .map(action => ` · ${action.name ?? action.accountId ?? ''}: ${action.message ?? action.outcome ?? ''}`)
              .join(''),
        )
        : null,
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

  const head = el('div', { class: 'panel-head' },
    el('h2', { text: '定时签到' }),
    el('span', { class: 'tip-q', 'data-tip': SCHEDULE_TIP, title: SCHEDULE_TIP, text: '?' }),
    el('span', { class: 'panel-sub', text: '每条条目各有时刻与范围 · 各自当天去重 · 错过时点开机自动补签' }),
    el('span', { class: 'head-actions' },
      button(store.editingId === 'new' ? '收起新建' : '添加条目', {
        tone: 'outline',
        disabled: store.saving,
        onClick: () => patch({ editingId: store.editingId === 'new' ? null : 'new' }),
      }),
    ),
  );

  const masterLine = el('div', { class: 'ck-set-line' },
    el('span', { class: 'ck-set-k', text: '总开关' }),
    el('span', { class: 'ck-set-v ck-set-row' },
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
  // 新建行落在列表首位：点「添加条目」后视线停在卡头，新行跟着出现在它下面
  // 才接得上（落到底部时用户会以为没反应）
  if (store.editingId === 'new') list.append(newEntryRow(options));
  for (const entry of entries) list.append(entryRow(entry, options));
  if (!entries.length && store.editingId !== 'new') {
    list.append(el('div', {
      class: 'ck-empty',
      text: '还没有定时条目 —— 点「添加条目」建一条，或到「签到中心」手动签到。',
    }));
  }

  const children = [
    el('section', { class: 'panel' }, head, el('div', { class: 'ck-panel-pad' }, masterLine), list),
  ];
  // 两块地区专属面板：由插件申报的 admin_panels 决定
  if (snapshot && panels.has('daily_keepalive')) children.push(keepalivePanel(snapshot));
  if (snapshot && panels.has('growth_tasks')) children.push(travelPanel(snapshot));
  fill(container, el('div', { class: 'ck-main' }, ...children));
}

/**
 * 「签到中心」标签页：总览卡 + 每日签到 + 新手福利 + 活动福利 + 成长任务。
 *
 * ── 这一页的数据全部来自快照 ────────────────────────────────
 * 分组、已签判定、范围外账号的原因，都在网关侧算好（`api::checkin_center`）。
 * 页面只画。**不要**在这里按 provider id 写死任何判断 —— 那些是插件申报的
 * （`description` / `modes` / `uiHooks` / `onboarding` / `plans` / `growth`）。
 *
 * ── 惰性块 ──────────────────────────────────────────────────
 * 新手福利与成长任务的**任务明细**要打上游，快照刻意不带。所以行上有一颗
 * 「查询任务」按钮，点了才查；查到的结果按账号缓存在 store 里。
 */

import { badge, button, el, fill, formatClock, formatTime, icon, providerBadge } from './dom.js';
import { renderKit, requireKit } from './kit.js';
import {
  claimOnboardingAll, growthOf, onboardingOf, patch, queryGrowth, queryOnboarding,
  runAllCheckin, runGrowth, runSchool, runTravel, signSingle, store, toast,
} from './state.js';

/** 请求切到某个标签页（监听在 main.js 的 ck:switch-tab 事件里） */
function switchTab(tab) {
  window.dispatchEvent(new CustomEvent('ck:switch-tab', { detail: { tab } }));
}

/** 一组账号是否已全部签完（决定徽章） */
function groupTone(group) {
  if (!group.totalCount) return 'none';
  if (group.doneCount >= group.totalCount) return 'done';
  if (group.doneCount > 0) return 'part';
  return 'todo';
}

function groupBadge(group) {
  const tone = groupTone(group);
  if (tone === 'none') return badge('无账号', 'plain', '', 'tag');
  if (tone === 'done') return badge('已签', 'ok', '', 'tag');
  if (tone === 'part') return badge('部分', 'warn', '', 'tag');
  return badge('待签', 'brand', '', 'tag');
}

/** 这一组申报了某个附加钩子（`CheckinInfo::ui_hooks`） */
function hasHook(group, hook) {
  return group.uiHooks.includes(hook);
}

/* ─── 账号明细表（展开区）────────────────────── */

function accountRows(group) {
  const table = el('table', { class: 'ck-table' });
  table.append(el('thead', {},
    el('tr', {},
      el('th', { text: '账号' }),
      el('th', { text: '今日签到' }),
      el('th', { text: '上次签到时间' }),
      el('th', { text: '' }),
    ),
  ));
  const body = el('tbody');
  // 分档执行由插件申报：keepalive（保活）/ claim（领取）/ full（保活+领取）
  // 三档分别对应三颗按钮 —— 领取才加积分、保活只维持活跃，分开是刻意的。
  const dailyActivity = group.modes.includes('keepalive')
    || group.modes.includes('claim')
    || group.modes.includes('full');
  for (const account of group.accounts) {
    const busy = mode => store.signing.has(`${account.id}:${mode}`);
    const actions = el('td', { class: 'ck-right' });
    if (dailyActivity) {
      const row = el('span', { class: 'ck-inline' });
      if (group.modes.includes('keepalive')) {
        row.append(button(busy('keepalive') ? '保活中…' : '保活', {
          tone: 'ghost',
          disabled: busy('keepalive'),
          title: '用免费模型与网页会话维持账号活跃 · 不领取奖励',
          onClick: () => void signSingle(account.id, 'keepalive'),
        }));
      }
      if (group.modes.includes('claim')) {
        row.append(button(busy('claim') ? '领取中…' : '领取', {
          tone: 'ghost',
          disabled: busy('claim'),
          title: '探测活动并领取日活奖励 · 不做保活',
          onClick: () => void signSingle(account.id, 'claim'),
        }));
      }
      if (group.modes.includes('full')) {
        row.append(button(busy('full') ? '执行中…' : '保活+领取', {
          tone: 'ghost',
          disabled: busy('full'),
          title: '探测 + 领取 + 保活 · 与定时签到同一套组合',
          onClick: () => void signSingle(account.id, 'full'),
        }));
      }
      actions.append(row);
    } else {
      actions.append(button(
        busy('checkin') ? '签到中…' : account.checkedInToday ? '重签' : '签到',
        {
          tone: 'ghost',
          disabled: busy('checkin'),
          onClick: () => void signSingle(account.id),
        },
      ));
    }
    const nameCell = el('td', {}, account.name);
    // 连登天数（WorkBuddy 国内版；0 = 还没读到过，不显示成「连登 0 天」）
    if (account.streakDays > 0) {
      nameCell.append(el('span', { class: 'ck-sub', text: ` · 连登 ${account.streakDays} 天` }));
    }
    body.append(el('tr', {},
      nameCell,
      el('td', {}, account.checkedInToday ? badge('已签', 'ok', '', 'tag') : badge('待签', 'brand', '', 'tag')),
      el('td', { class: 'ck-sub', text: formatTime(account.checkinAt) }),
      actions,
    ));
  }
  table.append(body);
  return table;
}

/* ─── UMID 提示（Qoder 国际版）───────────────── */

/**
 * Qoder 国际版的签到依赖本机的设备风控身份（UMID 组件）。
 *
 * 组件是**机器级**的（与有没有账号无关），未装就常驻这一条，装好整条消失。
 * 状态查询与安装都走网关的 `/api/qoder-umid*`（安装会下载并落地可执行文件，
 * 那是机器级动作，不属于页面）。
 */
function umidHint() {
  const box = el('div', { class: 'ck-hint' });
  const text = el('span', { text: '国际版签到需要 UMID 组件生成设备风控身份（本机未检测到）。' });
  const action = el('span', { class: 'ck-hint-action' });
  box.append(text, action);

  const load = async () => {
    let state = null;
    try {
      const { getUmid } = await import('./api.js');
      state = await getUmid();
    } catch {
      // 状态是锦上添花：拉不到就整条隐藏，不挡签到
      box.hidden = true;
      return;
    }
    if (!state || state.available) {
      box.hidden = true;
      return;
    }
    box.hidden = false;
    fill(action, state.installSupported
      ? button(state.installing ? '安装中…' : '一键安装组件', {
        tone: 'outline',
        disabled: state.installing === true,
        onClick: async () => {
          try {
            const { installUmid } = await import('./api.js');
            await installUmid();
            toast('✅ UMID 组件已安装，下轮签到即可领取国际版积分');
          } catch (error) {
            toast(`安装失败：${error instanceof Error ? error.message : String(error)}`, 'err');
          }
          void load();
        },
      })
      : el('span', { text: '安装 Qoder 客户端或 qodercli 后即可（无需重启网关）。' }));
    // 安装耗时以分钟计：装到一半刷新页面也要能自己恢复
    if (state.installing === true) window.setTimeout(() => void load(), 3000);
  };
  void load();
  return box;
}

/* ─── 每日签到的提供商行 ─────────────────────── */

function providerRow(group) {
  const hasAccounts = group.totalCount > 0;
  const expanded = store.expanded.has(group.id);
  const row = el('div', { class: expanded ? 'ck-row open' : 'ck-row' });
  const main = el('div', {
    class: 'ck-row-main',
    role: hasAccounts ? 'button' : null,
    tabindex: hasAccounts ? '0' : null,
    onClick: hasAccounts
      ? () => {
        if (store.expanded.has(group.id)) store.expanded.delete(group.id);
        else store.expanded.add(group.id);
        patch({});
      }
      : null,
  });
  main.append(providerBadge(group.id, group.label));
  const info = el('div', { class: 'ck-row-info' },
    el('div', { class: 'ck-row-name' }, group.label),
    // 链路说明由插件申报；没申报就不占位（不编一句通用话）
    el('div', { class: 'ck-row-desc', text: group.description }),
  );
  main.append(info);
  const right = el('div', { class: 'ck-row-right' },
    el('span', {
      class: 'ck-row-count',
      text: hasAccounts ? `${group.totalCount} 账号 · ${group.doneCount} 已签` : '—',
    }),
    groupBadge(group),
  );
  if (hasAccounts) right.append(el('span', { class: 'ck-chev' }, icon('chevron', 14)));
  main.append(right);
  row.append(main);
  if (hasHook(group, 'umid_component')) row.append(umidHint());
  if (expanded) row.append(el('div', { class: 'ck-row-detail' }, accountRows(group)));
  return row;
}

/* ─── 新手福利行 ─────────────────────────────── */

function onboardingRow(row) {
  const cache = onboardingOf(row.id);
  const hasDetail = cache.status === 'loaded' && cache.tasks.length > 0;
  const expanded = hasDetail && store.expanded.has(`onboarding:${row.id}`);
  const unclaimed = cache.unclaimed;

  const wrap = el('div', { class: expanded ? 'ck-row open' : 'ck-row' });
  const main = el('div', { class: 'ck-row-main' });
  main.append(providerBadge(row.provider || 'loomy', row.provider || 'Loomy'));
  const desc = cache.status === 'loaded'
    ? `已领 ${cache.tasks.length - unclaimed}/${cache.tasks.length} · 累计 ${cache.earned} 积分`
    : '尚未查询任务状态';
  const stamp = cache.settled && cache.settledAt
    ? ` · 结算于 ${formatTime(cache.settledAt)}`
    : cache.checkedAt ? ` · 查询于 ${formatTime(cache.checkedAt)}` : '';
  main.append(el('div', { class: 'ck-row-info' },
    el('div', { class: 'ck-row-name' }, row.name),
    el('div', { class: 'ck-row-desc', text: desc + stamp }),
  ));

  const right = el('div', { class: 'ck-row-right' });
  if (cache.status === 'loaded' && unclaimed > 0) right.append(badge(`${unclaimed} 项待领`, 'warn'));
  else if (cache.status === 'loaded') right.append(badge('全部领取', 'ok'));
  else if (cache.status === 'error') right.append(badge('查询失败', 'err'));

  right.append(button(cache.status === 'loading' ? '查询中…' : '查询任务', {
    tone: 'outline',
    disabled: cache.status === 'loading' || cache.claiming,
    // 手点 = 强制实查上游（结算过的账号进页面走记忆，零上游请求）
    onClick: () => void queryOnboarding(row.id, { expand: true, refresh: true }),
  }));
  // 「一键领取」的可点性：OfficeAce 的奖励由上游按天自动发放、账本一律已到账
  // （待领数恒 0），它的领取是「触发今天的结算」；别家仍是「有待领才可点」。
  const canClaim = cache.status === 'loaded'
    && !cache.claiming
    && (row.provider === 'officeace' || unclaimed > 0);
  right.append(button(
    cache.claiming ? '领取中…' : unclaimed > 0 ? `一键领取（${unclaimed}）` : '领取',
    { tone: 'outline', disabled: !canClaim, onClick: () => void claimOnboardingAll(row.id) },
  ));
  if (hasDetail) {
    right.append(el('span', {
      class: 'ck-chev',
      onClick: event => {
        event.stopPropagation();
        const key = `onboarding:${row.id}`;
        if (store.expanded.has(key)) store.expanded.delete(key);
        else store.expanded.add(key);
        patch({});
      },
    }, icon('chevron', 14)));
  }
  main.append(right);
  wrap.append(main);

  if (cache.status === 'error') {
    wrap.append(el('div', { class: 'ck-fold-note', text: `查询失败：${cache.error}` }));
  }
  if (expanded) {
    const detail = el('div', { class: 'ck-row-detail' });
    for (const task of cache.tasks) {
      detail.append(el('div', { class: 'ck-task-line' },
        el('span', { class: task.done ? 'ck-tick done' : 'ck-tick', text: task.done ? '✓' : '·' }),
        el('span', { class: 'ck-task-title', text: task.title }),
        el('span', { class: 'ck-task-pts', text: `+${task.points}` }),
        el('span', { class: 'ck-task-st' }, task.claiming
          ? badge('领取中', 'warn')
          : task.done
            ? badge('已领取', 'ok')
            : task.blocked
              ? badge('暂不可领', 'plain')
              : task.error
                ? badge(task.error, 'err')
                : badge('可领取', 'brand')),
      ));
    }
    wrap.append(detail);
  }
  return wrap;
}

/* ─── 活动福利行（ZCode 套餐）────────────────── */

/**
 * 这一行只提供**入口**：套餐明细与逐份领取在宿主页的弹窗里（`wbZcodePlans`）。
 *
 * iframe 里拿不到那个弹窗（它在宿主的 React 岛里），所以这里改成「点开宿主
 * 的弹窗」—— 通过 `postMessage` 请宿主打开。宿主没响应时退化成一句提示，
 * 让用户知道去账号页操作，而不是点了没反应。
 */
function planRow(row) {
  const claimedIds = claimedPlansToday(row.claimPlans);
  const wrap = el('div', { class: 'ck-row' });
  const main = el('div', { class: 'ck-row-main' });
  main.append(providerBadge(row.provider || 'zcode', 'ZCode'));
  const desc = claimedIds.length
    ? '点「套餐明细」看领到了哪几份、什么时候生效；活动期内每天可再领'
    : row.claimAt
      ? `上次领取 ${formatTime(row.claimAt)} · 领取需通过滑块验证码`
      : '限时体验套餐（start-plan），活动期内每天一份 · 领取需滑块验证码';
  main.append(el('div', { class: 'ck-row-info' },
    el('div', { class: 'ck-row-name' }, row.name,
      claimedIds.length
        ? badge(`今天已领 ${claimedIds.length} 份`, 'ok', `本地台账里今天（北京时间）领过的套餐：${claimedIds.join('、')}`)
        : badge('今天未领', 'plain', '本地台账里今天还没领过；活动期内每天发一份新套餐')),
    el('div', { class: 'ck-row-desc', text: desc }),
  ));
  main.append(el('div', { class: 'ck-row-right' },
    button(claimedIds.length ? '套餐明细' : '去领取', {
      tone: 'outline',
      onClick: () => requestZcodePlans(row.id, row.name),
    }),
  ));
  wrap.append(main);
  return wrap;
}

/** 本地台账里今天（北京时间）领过的套餐 id */
function claimedPlansToday(claimPlans) {
  if (!claimPlans || typeof claimPlans !== 'object') return [];
  const now = new Date();
  // 北京时间的自然日：上游按北京判「今天」，与宿主 `claimedPlanIdsToday` 同口径
  const today = new Date(now.getTime() + 8 * 3600 * 1000).toISOString().slice(0, 10);
  return Object.entries(claimPlans)
    .filter(([, at]) => {
      const stamp = Number(at);
      if (!Number.isFinite(stamp) || stamp <= 0) return false;
      return new Date(stamp + 8 * 3600 * 1000).toISOString().slice(0, 10) === today;
    })
    .map(([planId]) => planId);
}

/**
 * 请宿主打开套餐明细弹窗。
 *
 * 宿主（桌面端 `ui/app.js`、手机端）监听这条消息并调自己的 `wbZcodePlans`。
 * 没监听（老版本宿主 / 页面被单独打开）时给一句指路，别让按钮看起来坏了。
 */
function requestZcodePlans(accountId, name) {
  window.parent?.postMessage(
    { type: 'agent2api:open-zcode-plans', accountId, name },
    '*',
  );
  toast('已请求打开套餐明细；若没有反应，请到「账号」页点该账号的「领套餐」');
}

/* ─── 成长任务行 ─────────────────────────────── */

function growthRow(row) {
  const cache = growthOf(row.id);
  const tasks = cache.tasks || [];
  const supported = tasks.filter(task => task.supported && !task.unforgeable);
  const pending = supported.filter(task => task.status !== 'claimed' && task.current < task.target);
  const hasDetail = cache.status === 'loaded' && tasks.length > 0;
  const expanded = hasDetail && store.expanded.has(`growth:${row.id}`);
  const travel = cache.travel;
  const school = cache.school;

  const wrap = el('div', { class: expanded ? 'ck-row open' : 'ck-row' });
  const main = el('div', { class: 'ck-row-main' });
  main.append(providerBadge(row.provider || 'workbuddy', row.provider || 'WorkBuddy'));
  const desc = cache.status === 'loaded'
    ? `${supported.length} 项任务 · ${pending.length} 项可完成`
    : '查询任务树并一键点亮 / 领奖（含开学季与猫猫旅行）';
  main.append(el('div', { class: 'ck-row-info' },
    el('div', { class: 'ck-row-name' }, row.name),
    el('div', {
      class: 'ck-row-desc',
      text: desc + (cache.checkedAt ? ` · 查询于 ${formatTime(cache.checkedAt)}` : ''),
    }),
  ));
  const right = el('div', { class: 'ck-row-right' });
  if (cache.status === 'loaded') {
    right.append(pending.length ? badge(`${pending.length} 项可完成`, 'warn') : badge('全部完成', 'ok'));
  } else if (cache.status === 'error') {
    right.append(badge('查询失败', 'err'));
  }
  right.append(button(cache.status === 'loading' ? '查询中…' : '查询任务', {
    tone: 'outline',
    disabled: cache.status === 'loading' || cache.running,
    onClick: () => void queryGrowth(row.id),
  }));
  right.append(button(cache.running ? '执行中…' : '一键完成', {
    tone: 'outline',
    disabled: cache.status !== 'loaded' || cache.running,
    title: '接单 + 点亮 + 领奖（只处理已实测沉淀的任务）',
    onClick: () => void runGrowth(row.id, 'full'),
  }));
  right.append(button('领取', {
    tone: 'ghost',
    disabled: cache.status !== 'loaded' || cache.running,
    title: '只领取已完成任务的奖励（幂等，重复点不会重复入账）',
    onClick: () => void runGrowth(row.id, 'claim'),
  }));
  if (hasDetail) {
    right.append(el('span', {
      class: 'ck-chev',
      onClick: event => {
        event.stopPropagation();
        const key = `growth:${row.id}`;
        if (store.expanded.has(key)) store.expanded.delete(key);
        else store.expanded.add(key);
        patch({});
      },
    }, icon('chevron', 14)));
  }
  main.append(right);
  wrap.append(main);

  if (cache.status === 'error') {
    wrap.append(el('div', { class: 'ck-fold-note', text: `查询失败：${cache.error}` }));
  }
  if (expanded) {
    const detail = el('div', { class: 'ck-row-detail' });
    const readout = el('div', { class: 'ck-fold-note' });
    if (cache.energy !== null) readout.append(el('span', { text: `能量 ${cache.energy}` }));
    if (cache.streak !== null) readout.append(el('span', { text: ` · 连登 ${cache.streak} 天` }));
    if (cache.energy === null && cache.streak === null) {
      readout.append(el('span', { text: '（能量 / 连登读数本次未取到）' }));
    }
    detail.append(readout);
    for (const task of tasks) {
      const result = (cache.results || []).find(item => item.code === task.code);
      detail.append(el('div', { class: 'ck-task-line' },
        el('span', { class: task.status === 'claimed' ? 'ck-tick done' : 'ck-tick', text: task.status === 'claimed' ? '✓' : '·' }),
        el('span', {
          class: 'ck-task-title',
          text: task.title + (task.target > 1 ? ` (${task.current}/${task.target})` : ''),
          title: task.description || task.source || task.code,
        }),
        result ? el('span', { class: 'ck-task-pts', text: result.message || '' }) : null,
        el('span', { class: 'ck-task-st' },
          task.status === 'claimed'
            ? badge('已领取', 'ok')
            : task.status === 'completed' || (task.target > 0 && task.current >= task.target)
              ? badge('可领取', 'brand')
              : task.unforgeable
                ? badge('需真实操作', 'plain')
                : !task.supported
                  ? badge('暂不支持', 'plain')
                  : task.locked
                    ? badge('未解锁', 'plain')
                    : badge('可完成', 'warn')),
      ));
    }
    // 猫猫旅行：WorkBuddy 国内版的养成玩法（能力由插件申报 `growth_tasks`）
    const travelText = travel && travel.hasBuddy === true
      ? `${travel.buddyName || '已领养'} · ${
        travel.state === 'traveling' ? '在途'
          : travel.state === 'arrived' ? '可领奖'
            : travel.state === 'idle' ? (travel.dailyLimitReached ? '今日已派出' : '可派出')
              : travel.state || ''}`
      : travel && travel.hasBuddy === false
        ? '未领养（领养需先完成当日对话量）'
        : (travel?.error ? String(travel.error) : '尚未查询');
    detail.append(el('div', { class: 'ck-task-line' },
      el('span', { class: 'ck-task-tick' }, icon('paw', 14)),
      el('span', {}, '猫猫旅行：', travelText),
      el('span', { class: 'ck-task-st' },
        button(cache.runningTravel ? '执行中…' : '巡检一次', {
          tone: 'ghost',
          disabled: cache.runningTravel || cache.status !== 'loaded',
          onClick: () => void runTravel(row.id),
        })),
    ));
    // 开学季活动
    const schoolText = school && school.inPeriod === true
      ? `进行中${school.lottery?.balance !== undefined && school.lottery?.balance !== null
        ? ` · 抽奖机会 ${school.lottery.balance} 次` : ''}`
      : school && school.inPeriod === false
        ? '非活动期'
        : (school?.error ? String(school.error) : '尚未查询');
    detail.append(el('div', { class: 'ck-task-line' },
      el('span', { class: 'ck-task-tick' }, icon('cap', 14)),
      el('span', {}, '开学季活动：', schoolText),
      el('span', { class: 'ck-task-st' },
        button(cache.runningSchool ? '执行中…' : '执行并抽奖', {
          tone: 'ghost',
          disabled: cache.runningSchool || cache.status !== 'loaded',
          title: '执行开学季任务并抽空抽奖机会（分享 / 对话 / 专家事件）',
          onClick: () => void runSchool(row.id, true),
        })),
    ));
    wrap.append(detail);
  }
  return wrap;
}

/* ─── 面板容器 ───────────────────────────────── */

/** 一块面板：标题 + 副标题 + 右侧操作 + 内容 */
function panel(title, subtitle, actions, content) {
  const head = el('div', { class: 'panel-head' },
    el('h2', { text: title }),
    subtitle ? el('span', { class: 'panel-sub', text: subtitle }) : null,
    actions ? el('span', { class: 'head-actions' }, actions) : null,
  );
  return el('section', { class: 'panel' }, head, content);
}

/** 总览卡 */
function statsCard() {
  const snapshot = store.snapshot;
  const daily = snapshot?.daily;
  const auto = store.schedules || snapshot?.auto;
  const entries = auto?.schedules || [];
  const enabled = entries.filter(entry => entry.enabled).length;
  const nextRun = entries
    .filter(entry => entry.enabled && entry.nextRunAt)
    .map(entry => entry.nextRunAt)
    .sort((left, right) => left - right)[0];
  const outCount = (daily?.outOfScope || []).reduce((sum, item) => sum + item.count, 0);
  const onboardingRows = snapshot?.extras.onboarding || [];
  const onboardingUnclaimed = onboardingRows
    .reduce((sum, row) => sum + (onboardingOf(row.id).unclaimed || 0), 0);

  // 总览卡。`link` 非空时整张卡可点（跳到对应标签页）——旧实现里
  // 「定时签到」「累计签到记录」两张是这样，另两张是纯读数。
  // `pending` = 数据还没到：显示灰色 `—` 而不是 `0`，否则用户会以为
  // 「今天一个都没签」，实际是还没读到。
  const card = (label, value, unit, foot, options = {}) => {
    const valueNode = el('div', { class: 'ck-stat-value' },
      options.pending
        ? el('span', { class: 'ck-pending', text: '—' })
        : String(value),
      !options.pending && unit ? el('small', { text: unit }) : null,
    );
    const box = el('div', { class: options.link ? 'ck-stat ck-stat-link' : 'ck-stat' },
      el('div', { class: 'ck-stat-label', text: label }),
      valueNode,
      el('div', { class: 'ck-stat-foot', text: foot }),
    );
    if (options.link) {
      box.setAttribute('role', 'button');
      box.setAttribute('tabindex', '0');
      box.addEventListener('click', options.link);
      box.addEventListener('keydown', event => {
        if (event.key === 'Enter') options.link();
      });
    }
    return box;
  };

  const total = daily?.todayEligible || 0;
  const done = daily?.todayDone || 0;
  // 进度条直接用组件库的 `Progress`（旧实现就是这样），不再自绘 ——
  // 它的高度 / 轨道色 / 过渡曲线都由组件自己定义，抄数值总会差一点。
  const { React, Progress } = requireKit();
  const bar = renderKit(React.createElement(Progress, {
    className: 'ck-stat-bar',
    value: total > 0 ? (done / total) * 100 : 0,
  }));
  const progress = card(
    '今日签到进度',
    done,
    ` / ${total}`,
    outCount > 0 ? `另有 ${outCount} 个账号不参与每日签到` : '全部账号均可签到',
    { pending: !daily },
  );
  progress.append(bar);

  return el('div', { class: 'ck-stats' },
    progress,
    card('定时签到', enabled, ` / ${entries.length} 条启用`,
      auto?.enabled !== true
        ? '总开关已关闭 · 在「定时签到」页打开'
        : nextRun ? `下次 ${formatClock(nextRun)} · 点此管理条目` : '暂无启用的条目 · 点此添加',
      { link: () => switchTab('schedule'), pending: !auto }),
    card('待领新手福利',
      onboardingRows.length ? onboardingUnclaimed : '',
      onboardingRows.length ? ' 项' : '',
      onboardingRows.length ? `共 ${onboardingRows.length} 个账号` : '没有支持新手福利的账号',
      { pending: !onboardingRows.length }),
    card('累计签到记录', store.records?.total ?? 0, '',
      '每账号一条明细 · 点此查看「签到记录」',
      { link: () => switchTab('records'), pending: store.records == null }),
  );
}

/* ─── 渲染入口 ───────────────────────────────── */

export function renderCenter(container) {
  if (!store.loaded) {
    fill(container, el('div', { class: 'ck-loading' }, '正在读取签到数据…'));
    return;
  }
  const snapshot = store.snapshot;
  if (!snapshot) {
    fill(container, el('div', { class: 'ck-empty', text: '还没有读到签到数据。' }));
    return;
  }
  const providers = snapshot.daily.providers;
  const onboardingRows = snapshot.extras.onboarding;
  const planRows = snapshot.extras.plans;
  const growthRows = snapshot.extras.growth;
  // 管理面板由插件申报：`growth_tasks` 决定「成长任务」整节显不显示
  const showGrowth = snapshot.adminPanels.includes('growth_tasks');

  const dailyPanel = panel(
    '每日签到',
    '点击行展开账号明细 · 单账号签到不受范围限制',
    button(store.runningAll ? '签到中…' : '立即全部签到', {
      tone: 'default',
      disabled: store.runningAll || store.schedules?.running === true,
      onClick: () => void runAllCheckin(),
    }),
    el('div', {},
      ...providers.map(group => providerRow(group)),
      providers.length === 0
        ? el('div', {
          class: 'ck-fold-note',
          text: '还没有可签到的提供商。签到能力随提供商插件包提供，装好并添加账号后这里就会出现。',
        })
        : null,
      snapshot.daily.outOfScope.length > 0
        ? el('div', {
          class: 'ck-fold-note',
          text: '不参与每日签到：' + snapshot.daily.outOfScope
            .map(item => `${item.label} ×${item.count}（${item.reason}）`)
            .join('；'),
        })
        : null,
    ),
  );

  const onboardingPanel = panel(
    '新手福利',
    '一次性福利 · 签到后自动查询并领取，领完记在本机不再查上游 · 点击行展开任务清单',
    onboardingRows.length > 0
      ? button('全部查询', {
        tone: 'outline',
        onClick: () => {
          for (const row of onboardingRows) {
            // 已结算的账号不进这一轮（结论不会变）；要实查走逐行的「查询任务」
            if (row.settled) continue;
            void queryOnboarding(row.id);
          }
        },
      })
      : null,
    onboardingRows.length
      ? el('div', {}, ...onboardingRows.map(row => onboardingRow(row)))
      : el('div', { class: 'ck-empty', text: '没有支持新手福利的账号。' }),
  );

  const planPanel = panel(
    '活动福利',
    '限时体验套餐 · 领取需滑块验证码 · 逐份查看与领取',
    null,
    planRows.length
      ? el('div', {}, ...planRows.map(row => planRow(row)))
      : el('div', { class: 'ck-empty', text: '没有可领套餐的账号。' }),
  );

  const children = [statsCard(), dailyPanel, onboardingPanel, planPanel];
  if (showGrowth) {
    children.push(panel(
      '成长任务',
      '查询任务树后一键点亮 / 领奖 · 含开学季与猫猫旅行 · 点击行展开明细',
      null,
      growthRows.length
        ? el('div', {}, ...growthRows.map(row => growthRow(row)))
        : el('div', { class: 'ck-empty', text: '没有支持成长任务的账号。' }),
    ));
  }
  fill(container, el('div', { class: 'ck-main' }, ...children));
}

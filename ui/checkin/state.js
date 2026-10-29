/**
 * 签到页的数据层：归一化 + 模块级 store。
 *
 * ── 为什么自己归一而不直接用后端形状 ────────────────────────
 * 后端字段可能缺（旧版本、上游没给、可选能力），界面直接读会让「一个字段
 * 缺失」变成整块渲染崩掉。这里逐字段兜底，页面只管画。
 *
 * ── store 的形状 ────────────────────────────────────────────
 * 一份快照 + 若干按账号索引的惰性缓存（新手任务 / 成长任务）。惰性缓存是
 * 上游查询的结果，**刻意不进快照**：聚合快照必须保持「打开页面零上游请求」
 * （见网关 `api::checkin_center` 的模块头）。
 *
 * ── 订阅 ────────────────────────────────────────────────────
 * 页面只有一份，用简单的 `subscribe` + `render()` 全量重绘。没有虚拟 DOM，
 * 所以重绘要克制：输入框在重绘时保持焦点靠 `data-focus` 标记（见 center.js）。
 */

import {
  claimOnboarding, deleteSchedule, getCheckinCenter, getGrowth, getOnboarding,
  getRecords, getSchedules, runActivity, runCheckin, runGrowthSchool, runGrowthTask,
  runGrowthTravel, runSchedule, runTravelNow, saveAutoCheckin, saveKeepalive,
  saveSchedule, saveTravelSchedule,
} from './api.js';

/* ─── 归一化 ─────────────────────────────────── */

const text = value => (typeof value === 'string' ? value : '');
const num = value => {
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : 0;
};
const bool = value => value === true;

/** 一条每日签到账号行 */
function accountRow(raw) {
  const row = raw || {};
  return {
    id: text(row.id),
    name: text(row.name) || text(row.id),
    available: row.available !== false,
    checkinAt: Number.isFinite(Number(row.checkinAt)) ? Number(row.checkinAt) : null,
    checkedInToday: bool(row.checkedInToday),
    edition: text(row.edition) || null,
    streakDays: num(row.streakDays),
  };
}

/** 一个提供商分组（`daily.providers` 元素） */
function providerGroup(raw) {
  const group = raw || {};
  const accounts = Array.isArray(group.accounts) ? group.accounts.map(accountRow) : [];
  return {
    id: text(group.id),
    label: text(group.label) || text(group.id),
    accounts,
    doneCount: num(group.doneCount),
    totalCount: num(group.totalCount) || accounts.length,
    // 插件申报的链路说明 / 执行档位 / 附加钩子（没申报 = 空）
    description: text(group.description),
    modes: Array.isArray(group.modes) ? group.modes.map(text).filter(Boolean) : [],
    uiHooks: Array.isArray(group.uiHooks) ? group.uiHooks.map(text).filter(Boolean) : [],
  };
}

/** 快照归一化（字段缺失全部兜成空数组 / 空串 / 0） */
export function normalizeSnapshot(raw) {
  const data = raw || {};
  const daily = data.daily || {};
  const extras = data.extras || {};
  return {
    daily: {
      providers: Array.isArray(daily.providers) ? daily.providers.map(providerGroup) : [],
      outOfScope: Array.isArray(daily.outOfScope)
        ? daily.outOfScope.map(item => ({
          label: text(item?.label),
          reason: text(item?.reason),
          count: num(item?.count),
        }))
        : [],
      todayDone: num(daily.todayDone),
      todayEligible: num(daily.todayEligible),
    },
    extras: {
      onboarding: Array.isArray(extras.onboarding)
        ? extras.onboarding.map(item => ({
          id: text(item?.id),
          name: text(item?.name) || text(item?.id),
          provider: text(item?.provider),
          settled: item?.settled && typeof item.settled === 'object' ? item.settled : null,
        }))
        : [],
      plans: Array.isArray(extras.plans)
        ? extras.plans.map(item => ({
          id: text(item?.id),
          name: text(item?.name) || text(item?.id),
          provider: text(item?.provider),
          claimAt: Number.isFinite(Number(item?.claimAt)) ? Number(item.claimAt) : null,
          claimPlans: item?.claimPlans && typeof item.claimPlans === 'object' ? item.claimPlans : null,
        }))
        : [],
      growth: Array.isArray(extras.growth)
        ? extras.growth.map(item => ({
          id: text(item?.id),
          name: text(item?.name) || text(item?.id),
          provider: text(item?.provider),
        }))
        : [],
    },
    auto: normalizeAuto(data.auto),
    keepalive: normalizeKeepalive(data.keepalive),
    travel: normalizeTravel(data.travel),
    history: Array.isArray(data.history) ? data.history : [],
    adminPanels: Array.isArray(data.adminPanels) ? data.adminPanels.map(text) : [],
    checkinFeatures: data.checkinFeatures && typeof data.checkinFeatures === 'object'
      ? data.checkinFeatures
      : {},
  };
}

/** 定时签到状态（`/api/auto-checkin` 与 `/api/checkin-schedules` 同形） */
export function normalizeAuto(raw) {
  const data = raw || {};
  return {
    enabled: bool(data.enabled),
    lastFiredToday: bool(data.lastFiredToday),
    running: bool(data.running),
    providerOptions: Array.isArray(data.providerOptions)
      ? data.providerOptions.map(item => ({ id: text(item?.id), label: text(item?.label) || text(item?.id) }))
      : [],
    schedules: Array.isArray(data.schedules)
      ? data.schedules.map(entry => ({
        id: text(entry?.id),
        label: text(entry?.label),
        displayLabel: text(entry?.displayLabel) || text(entry?.time),
        enabled: bool(entry?.enabled),
        time: text(entry?.time),
        providers: Array.isArray(entry?.providers) ? entry.providers.map(text).filter(Boolean) : [],
        lastFiredDate: text(entry?.lastFiredDate) || null,
        lastFiredToday: bool(entry?.lastFiredToday),
        nextRunAt: Number.isFinite(Number(entry?.nextRunAt)) ? Number(entry.nextRunAt) : null,
        lastResult: entry?.lastResult || null,
      }))
      : [],
  };
}

/** 保活模型链（`{models, defaultModels}`，读接口与保存接口同形） */
export function normalizeKeepalive(raw) {
  const data = raw || {};
  return {
    models: Array.isArray(data.models) ? data.models.map(text).filter(Boolean) : [],
    defaultModels: Array.isArray(data.defaultModels) ? data.defaultModels.map(text).filter(Boolean) : [],
  };
}

/** 猫猫旅行定时排程 */
export function normalizeTravel(raw) {
  const data = raw || {};
  return {
    enabled: bool(data.enabled),
    available: data.available !== false,
    pausedReason: text(data.pausedReason) || null,
    times: Array.isArray(data.times) ? data.times.map(text).filter(Boolean) : [],
    firedToday: Array.isArray(data.firedToday) ? data.firedToday.map(text) : [],
    lastFiredDate: text(data.lastFiredDate) || null,
    lastFiredTime: text(data.lastFiredTime) || null,
    lastResult: data.lastResult || null,
    running: bool(data.running),
  };
}

/** 新手任务的一条 */
function normalizeTask(raw) {
  const row = raw || {};
  const key = text(row.key);
  if (!key) return null;
  return {
    key,
    title: text(row.title) || key,
    group: text(row.group),
    points: num(row.points),
    done: bool(row.done),
    blocked: bool(row.blocked),
    claiming: bool(row.claiming),
    error: text(row.error),
  };
}

function normalizeTasks(raw) {
  return (Array.isArray(raw) ? raw : []).map(normalizeTask).filter(Boolean);
}

/** 成长任务的一条 */
function normalizeGrowthTask(raw) {
  const row = raw || {};
  const code = text(row.code);
  if (!code) return null;
  return {
    code,
    title: text(row.title) || code,
    description: text(row.description),
    status: text(row.status),
    current: num(row.current),
    target: num(row.target),
    locked: bool(row.locked),
    supported: bool(row.supported),
    unforgeable: bool(row.unforgeable),
    kind: text(row.kind),
    source: text(row.source),
  };
}

/* ─── store ──────────────────────────────────── */

/** 签到记录的一行 */
function recordRow(raw) {
  const row = raw || {};
  return {
    rowId: Number.isFinite(Number(row.rowId)) ? Number(row.rowId) : null,
    ts: Number.isFinite(Number(row.ts)) ? Number(row.ts) : null,
    date: text(row.date),
    source: text(row.source),
    reason: text(row.reason),
    accountName: text(row.accountName),
    provider: text(row.provider),
    outcome: text(row.outcome),
    ok: bool(row.ok),
    message: text(row.message),
    error: text(row.error),
    keptActive: bool(row.keptActive),
  };
}

export const store = {
  /** 当前标签页：center / schedule / records */
  tab: 'center',
  /** 聚合快照（null = 还没读到） */
  snapshot: null,
  loaded: false,
  loadError: '',
  /** 批量签到在途 */
  runningAll: false,
  /** 单账号动作在途：`<id>:<mode>` */
  signing: new Set(),
  /** 展开的提供商分组 id */
  expanded: new Set(),
  /** 新手任务缓存：账号 id → 缓存 */
  onboarding: new Map(),
  /** 成长任务缓存：账号 id → 缓存 */
  growth: new Map(),
  /** 定时签到状态（独立于快照，写动作的响应直接覆盖它） */
  schedules: null,
  schedulesLoaded: false,
  schedulesError: '',
  saving: false,
  /** 正在执行「立即跑一次」的条目 id */
  runningScheduleId: null,
  /** 展开编辑的条目 id；'new' = 新建表单 */
  editingId: null,
  /** 签到记录：当前页数据 */
  records: null,
  recordsLoaded: false,
  recordsError: '',
  recordsQuery: { limit: 50, offset: 0, outcome: '', provider: '', keyword: '' },
  /** 猫猫旅行草稿（时刻串），null = 跟随快照 */
  travelTimesDraft: null,
  /** 保活模型链草稿，null = 跟随快照 */
  keepaliveDraft: null,
  /** 保活模型链保存中（按钮显示「保存中…」并禁用整块清单） */
  keepaliveSaving: false,
  /** 猫猫旅行保存中 */
  travelSaving: false,
  /** 猫猫旅行正在手动巡检 */
  travelRunning: false,
};

const listeners = new Set();

export function subscribe(listener) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** 改 store 并通知（所有写路径都走这里，避免有人直接赋值忘了通知） */
export function patch(next) {
  Object.assign(store, next);
  for (const notify of listeners) notify();
}

/* ─── 读 ─────────────────────────────────────── */

/** 拉聚合快照（页面打开 / 签到动作之后调） */
export async function loadCenter() {
  try {
    const data = await getCheckinCenter();
    patch({
      snapshot: normalizeSnapshot(data),
      loaded: true,
      loadError: '',
      // 草稿跟着后端值走：首次加载 / 别处改过之后都以服务端为准
      keepaliveDraft: null,
      travelTimesDraft: null,
    });
    seedSettledCaches();
  } catch (error) {
    patch({ loaded: true, loadError: errorMessage(error) });
  }
}

/**
 * 快照带回了「一次性福利已结清」的记忆 ⇒ 直接铺成缓存。
 *
 * 后端把结算结论落了盘（`extras.onboarding[].settled`），所以这些账号不必
 * 再查一次上游：页面一打开就能显示「已领完」，且不再发查询。
 */
function seedSettledCaches() {
  const rows = store.snapshot?.extras.onboarding || [];
  for (const row of rows) {
    if (!row.settled) continue;
    const current = store.onboarding.get(row.id);
    if (current?.status === 'loaded') continue;
    const tasks = normalizeTasks(row.settled.tasks);
    store.onboarding.set(row.id, {
      status: 'loaded',
      checkedAt: 0,
      settled: true,
      settledAt: num(row.settled.at),
      tasks,
      unclaimed: 0,
      earned: num(row.settled.earned),
      total: num(row.settled.total),
      claiming: false,
      error: '',
    });
  }
}

/**
 * 在途标记：同一份数据同时只发一条请求。
 *
 * 首屏并发拉三块、切标签页时渲染函数又会兜底拉一次 —— 没有这道闸，
 * 「打开页面」这一下会打出两条相同的请求（`no-store`，浏览器不会合并）。
 */
const inflight = new Set();

/** 拉定时签到状态 */
export async function loadSchedules() {
  if (inflight.has('schedules')) return;
  inflight.add('schedules');
  try {
    const data = await getSchedules();
    patch({ schedules: normalizeAuto(data), schedulesLoaded: true, schedulesError: '' });
  } catch (error) {
    patch({ schedulesLoaded: true, schedulesError: errorMessage(error) });
  } finally {
    inflight.delete('schedules');
  }
}

/** 拉一页签到记录 */
export async function loadRecords() {
  if (inflight.has('records')) return;
  inflight.add('records');
  try {
    const data = await getRecords(store.recordsQuery);
    patch({
      records: {
        records: Array.isArray(data?.records) ? data.records.map(recordRow) : [],
        total: num(data?.total),
      },
      recordsLoaded: true,
      recordsError: '',
    });
  } catch (error) {
    patch({ recordsLoaded: true, recordsError: errorMessage(error) });
  } finally {
    inflight.delete('records');
  }
}

/* ─── 写：签到 ───────────────────────────────── */

export function errorMessage(error) {
  if (error instanceof Error && error.message) return error.message;
  const text_ = String(error ?? '').trim();
  return text_ || '未知错误';
}

/** 批量签到（全部可签账号） */
export async function runAllCheckin() {
  if (store.runningAll) return;
  patch({ runningAll: true });
  try {
    const result = await runCheckin();
    reportCheckinResult(result);
  } catch (error) {
    toast(`签到失败：${errorMessage(error)}`, 'err');
  } finally {
    patch({ runningAll: false });
    await loadCenter();
  }
}

/** 单账号签到 / 分档执行 */
export async function signSingle(accountId, mode) {
  const key = `${accountId}:${mode || 'checkin'}`;
  if (store.signing.has(key)) return;
  store.signing.add(key);
  patch({});
  try {
    const result = mode ? await runActivity(accountId, mode) : await runCheckin(accountId);
    reportCheckinResult(result, true);
  } catch (error) {
    toast(`执行失败：${errorMessage(error)}`, 'err');
  } finally {
    store.signing.delete(key);
    patch({});
    await loadCenter();
  }
}

/** 签到结果的统一提示（后端的 results 数组形状见 `run_checkin`） */
function reportCheckinResult(result, single) {
  if (!result || typeof result !== 'object') return;
  const succeeded = num(result.succeeded);
  const active = num(result.active);
  const skipped = num(result.skipped);
  const total = num(result.total);
  if (single) {
    const rows = Array.isArray(result.results) ? result.results : [];
    const first = rows[0] || {};
    const message = text(first.message) || text(first.error);
    if (first.ok === false) {
      toast(`执行失败：${message || '上游未接受'}`, 'err');
    } else {
      toast(message || '✅ 执行完成');
    }
    return;
  }
  const parts = [`成功 ${succeeded}`];
  if (active > 0) parts.push(`保活 ${active}`);
  if (skipped > 0) parts.push(`跳过 ${skipped}`);
  parts.push(`共 ${total}`);
  toast(`✅ 签到完成：${parts.join(' · ')}`);
}

/* ─── 写：新手福利 ───────────────────────────── */

export function onboardingOf(id) {
  return store.onboarding.get(id) || {
    status: 'idle', checkedAt: 0, settled: false, settledAt: 0,
    tasks: [], unclaimed: 0, earned: 0, total: 0, claiming: false, error: '',
  };
}

function patchOnboarding(id, next) {
  store.onboarding.set(id, { ...onboardingOf(id), ...next });
  patch({});
}

/** 查询一个账号的新手任务（`refresh` = 强制实查上游） */
export async function queryOnboarding(id, options = {}) {
  const current = onboardingOf(id);
  if (current.status === 'loading' || current.claiming) return;
  patchOnboarding(id, { status: 'loading', error: '', settled: false });
  try {
    const data = await getOnboarding(id, options.refresh === true);
    const tasks = normalizeTasks(data?.tasks);
    const settled = bool(data?.settled);
    patchOnboarding(id, {
      status: 'loaded',
      checkedAt: Date.now(),
      tasks,
      unclaimed: tasks.filter(task => !task.done && !task.blocked).length,
      earned: num(data?.earned),
      total: num(data?.total),
      settled,
      settledAt: settled ? num(data?.settledAt) : 0,
    });
    if (options.expand === true && tasks.length > 0) {
      store.expanded.add(`onboarding:${id}`);
      patch({});
    }
  } catch (error) {
    patchOnboarding(id, { status: 'error', error: errorMessage(error), settled: false });
  }
}

/** 一键领取（服务端幂等，重复点不会重复加分） */
export async function claimOnboardingAll(id) {
  const current = onboardingOf(id);
  if (current.claiming || current.status !== 'loaded') return;
  patchOnboarding(id, { claiming: true });
  try {
    const data = await claimOnboarding(id);
    const failed = new Map();
    for (const row of Array.isArray(data?.results) ? data.results : []) {
      const key = text(row?.key);
      if (key && row?.ok !== true) failed.set(key, text(row?.error) || '领取失败');
    }
    const server = normalizeTasks(data?.tasks);
    const merged = (server.length ? server : current.tasks).map(task => ({
      ...task,
      claiming: false,
      error: failed.get(task.key) || '',
    }));
    const settled = bool(data?.settled);
    patchOnboarding(id, {
      claiming: false,
      checkedAt: Date.now(),
      tasks: merged,
      unclaimed: merged.filter(task => !task.done && !task.blocked).length,
      earned: num(data?.earned),
      total: num(data?.total),
      settled,
      settledAt: settled ? num(data?.settledAt) : 0,
    });
    const failures = merged.filter(task => task.error && !task.done).length;
    const message = text(data?.message);
    if (failures) toast(`新手福利有 ${failures} 项领取失败，可重试`, 'err');
    else if (message) toast(message);
    else toast(`✅ 新手福利领取完成，累计 ${num(data?.earned)} 积分`);
  } catch (error) {
    patchOnboarding(id, {
      claiming: false,
      tasks: current.tasks.map(task => ({
        ...task, claiming: false, error: task.done ? '' : errorMessage(error),
      })),
    });
    toast(`领取失败：${errorMessage(error)}`, 'err');
  } finally {
    await loadCenter();
  }
}

/* ─── 写：成长任务 ───────────────────────────── */

export function growthOf(id) {
  return store.growth.get(id) || {
    status: 'idle', checkedAt: 0, energy: null, streak: null,
    tasks: [], travel: null, school: null, running: false, runningTravel: false,
    runningSchool: false, results: [], error: '',
  };
}

function patchGrowth(id, next) {
  store.growth.set(id, { ...growthOf(id), ...next });
  patch({});
}

/** 查询一个账号的成长任务盘点（只读） */
export async function queryGrowth(id) {
  const current = growthOf(id);
  if (current.status === 'loading' || current.running) return;
  patchGrowth(id, { status: 'loading', error: '' });
  try {
    const data = await getGrowth(id);
    const tasksBlock = data?.tasks || {};
    patchGrowth(id, {
      status: 'loaded',
      checkedAt: Date.now(),
      energy: data?.energy === null || data?.energy === undefined ? null : num(data.energy),
      streak: data?.streak === null || data?.streak === undefined ? null : num(data.streak),
      tasks: (Array.isArray(tasksBlock.tasks) ? tasksBlock.tasks : [])
        .map(normalizeGrowthTask).filter(Boolean),
      travel: data?.travel || null,
      school: data?.school || null,
      // 任务列表本身的失败挂在行级（其余三块有数据可看，不该整行变红）
      error: text(tasksBlock.error),
    });
  } catch (error) {
    patchGrowth(id, { status: 'error', error: errorMessage(error) });
  }
}

/**
 * 执行成长任务（`full` = 点亮 + 领奖；`claim` = 只领已完成奖励）。
 *
 * 逐条 await 单任务端点：全量一次请求会超过管理 API 的 60 秒转发超时
 * （后端逐条留 1s+ 写间隔，见网关 `api::growth` 的模块头）。
 */
export async function runGrowth(id, mode) {
  const current = growthOf(id);
  if (current.running) return;
  const targets = current.tasks.filter(task => {
    if (!task.supported || task.locked || task.status === 'claimed') return false;
    const completed = task.status === 'completed' || (task.target > 0 && task.current >= task.target);
    return mode === 'claim' ? completed : !task.unforgeable || completed;
  });
  if (!targets.length) {
    toast('没有需要执行的任务');
    return;
  }
  patchGrowth(id, { running: true, results: [], error: '' });
  let failed = 0;
  const results = [];
  for (const task of targets) {
    try {
      const row = await runGrowthTask(id, task.code, mode);
      results.push(row || { code: task.code, outcome: 'done', message: '' });
      if (row?.outcome === 'failed') failed += 1;
    } catch (error) {
      failed += 1;
      results.push({ code: task.code, outcome: 'failed', message: errorMessage(error) });
    }
    patchGrowth(id, { results: [...results] });
  }
  patchGrowth(id, { running: false });
  toast(failed ? `执行完成，${failed} 项失败` : '✅ 执行完成', failed ? 'err' : 'ok');
  await queryGrowth(id);
}

/** 开学季任务 + 抽奖 */
export async function runSchool(id, lottery) {
  const current = growthOf(id);
  if (current.runningSchool) return;
  patchGrowth(id, { runningSchool: true });
  try {
    const data = await runGrowthSchool(id, lottery);
    const summary = text(data?.message) || (lottery ? '✅ 开学季任务已执行并抽奖' : '✅ 开学季任务已执行');
    toast(summary);
  } catch (error) {
    toast(`执行失败：${errorMessage(error)}`, 'err');
  } finally {
    patchGrowth(id, { runningSchool: false });
    await queryGrowth(id);
  }
}

/** 猫猫旅行巡检一次 */
export async function runTravel(id) {
  const current = growthOf(id);
  if (current.runningTravel) return;
  patchGrowth(id, { runningTravel: true });
  try {
    const data = await runGrowthTravel(id);
    toast(text(data?.message) || '✅ 已巡检一次');
  } catch (error) {
    toast(`巡检失败：${errorMessage(error)}`, 'err');
  } finally {
    patchGrowth(id, { runningTravel: false });
    await queryGrowth(id);
  }
}

/* ─── 写：定时签到 ───────────────────────────── */

/**
 * 保存一条定时条目（成功返回整份状态）。
 *
 * `options.closeEditor` 决定保存后要不要收起编辑表单：表单里的「保存」要收，
 * 条目行上的开关不要 —— 后者不是「编辑完成」，把用户正在看的表单关掉
 * 是纯副作用（旧实现的两个入口也是分开的）。
 */
export async function saveScheduleEntry(payload, options = {}) {
  if (store.saving) return;
  patch({ saving: true });
  try {
    const data = await saveSchedule(payload);
    patch({
      schedules: normalizeAuto(data),
      schedulesError: '',
      ...(options.closeEditor === false ? {} : { editingId: null }),
    });
    void loadCenter();
    if (options.toast !== false) toast('✅ 已保存');
  } catch (error) {
    toast(`保存失败：${errorMessage(error)}`, 'err');
  } finally {
    patch({ saving: false });
  }
}

export async function removeScheduleEntry(id) {
  if (store.saving) return;
  patch({ saving: true });
  try {
    await deleteSchedule(id);
    // 删除的响应不带整份状态（只有 `{success}`），重拉一次
    await loadSchedules();
    void loadCenter();
    toast('已删除');
  } catch (error) {
    toast(`删除失败：${errorMessage(error)}`, 'err');
  } finally {
    patch({ saving: false });
  }
}

/** 立即执行一条条目 */
export async function runScheduleEntry(id) {
  if (store.runningScheduleId) return;
  patch({ runningScheduleId: id });
  try {
    const data = await runSchedule(id);
    patch({ schedules: normalizeAuto(data) });
    reportCheckinResult(data);
  } catch (error) {
    toast(`执行失败：${errorMessage(error)}`, 'err');
  } finally {
    patch({ runningScheduleId: null });
    await loadCenter();
  }
}

/** 定时签到总开关 */
export async function toggleMaster(enabled) {
  if (store.saving) return;
  patch({ saving: true });
  try {
    const data = await saveAutoCheckin({ enabled });
    patch({ schedules: normalizeAuto(data) });
    void loadCenter();
    toast(enabled ? '✅ 定时签到已开启' : '定时签到已关闭');
  } catch (error) {
    toast(`保存失败：${errorMessage(error)}`, 'err');
  } finally {
    patch({ saving: false });
  }
}

/** 保活模型链（国际版的领日活玩法） */
export async function saveKeepaliveModels(models) {
  if (store.keepaliveSaving) return;
  patch({ keepaliveSaving: true });
  try {
    // 保存接口直接返回新的 `{models, defaultModels}`：就地覆盖快照里的
    // keepalive 段，草稿与快照同一帧对齐。不必再跑一趟 loadCenter ——
    // 那会让面板先闪回旧清单（草稿已清、快照还是旧的），再跳到新清单。
    const data = await saveKeepalive(models);
    const snapshot = store.snapshot;
    patch({
      keepaliveDraft: null,
      snapshot: snapshot && data ? { ...snapshot, keepalive: normalizeKeepalive(data) } : snapshot,
    });
    toast('✅ 模型链已保存');
  } catch (error) {
    toast(`保存失败：${errorMessage(error)}`, 'err');
  } finally {
    patch({ keepaliveSaving: false });
  }
}

/** 猫猫旅行排程 */
export async function saveTravel(patchBody) {
  if (store.travelSaving) return;
  patch({ travelSaving: true });
  try {
    // 同 saveKeepaliveModels：接口回的是保存后的整份排程，直接覆盖
    const data = await saveTravelSchedule(patchBody);
    const snapshot = store.snapshot;
    patch({
      travelTimesDraft: null,
      snapshot: snapshot && data ? { ...snapshot, travel: normalizeTravel(data) } : snapshot,
    });
    toast('✅ 已保存');
  } catch (error) {
    toast(`保存失败：${errorMessage(error)}`, 'err');
  } finally {
    patch({ travelSaving: false });
  }
}

export async function runTravelNowOnce() {
  if (store.travelRunning) return;
  patch({ travelRunning: true });
  try {
    const result = await runTravelNow();
    if (result?.outcome === 'skipped') {
      toast(`本轮已跳过：${result.message || ''}`);
    } else {
      const summary = result?.summary || {};
      const failed = Number(summary.failed) || 0;
      toast(
        `猫猫旅行：${summary.actions?.length || 0} 个动作，+${Number(summary.credit) || 0} 积分，${failed} 个失败`,
        failed > 0 ? 'err' : 'ok',
      );
    }
  } catch (error) {
    toast(`执行失败：${errorMessage(error)}`, 'err');
  } finally {
    patch({ travelRunning: false });
    // 巡检会改账号状态与积分，快照要整份重读（与「立即执行」条目同一口径）
    await loadCenter();
  }
}

/* ─── 提示 ───────────────────────────────────── */

let toastTimer = 0;

/** 页面内提示（不依赖宿主：iframe 里没有 wbApp.toast） */
export function toast(message, tone = 'ok') {
  const element = document.getElementById('toast');
  if (!element) return;
  element.textContent = message;
  element.dataset.tone = tone;
  element.hidden = false;
  if (toastTimer) window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => {
    element.hidden = true;
  }, 2600);
}

/**
 * 插件页与网关之间的 HTTP 客户端。
 *
 * ── 凭证从哪来 ──────────────────────────────────────────────
 * 页面跑在 iframe 里，两种形态的取凭证方式不同：
 *   · **web 面板**（页面由网关托管）：与网关同源，HttpOnly cookie 直接生效，
 *     什么都不用做；
 *   · **桌面端 / 手机 APK**：页面 origin 是 `tauri://localhost` 或
 *     `https://localhost`，网关在另一处 —— 跨源、带不上 cookie。宿主在 iframe
 *     的 `load` 之后用 `postMessage` 递一份 `{ headers }` 进来（见桌面端
 *     `ui/app.js` 的 `pushPanelToken` 与手机端 `PluginPage.tsx`）。
 *
 * 所以这里先等一小段时间的消息，拿到就用；等不到就当同源处理（web 面板）。
 * 两种形态都不需要页面自己知道「我在哪」。
 *
 * ── 响应信封有三种，必须都认 ────────────────────────────────
 * 网关的历史遗留，同一个管理面里并存：
 *   · `{ success: true, data: … }`  —— `ok_json`（多数接口）
 *   · `{ success: false, error: "…" }` —— `management_error`（插件 / 签到记录）
 *   · `{ error: { message, type } }` —— `GatewayError`（OpenAI 风格，billing 那一族）
 * 页面只关心「有没有拿到 data」与「错误文案是什么」，所以统一在 `unwrap` 里
 * 归一。少认一种的后果是错误提示变成 `[object Object]`。
 */

/** 宿主递进来的凭证头（空对象 = 同源形态，走 cookie） */
let panelHeaders = {};

/** 凭证是否已经就位（同源形态永远「就位」） */
let headersReady = false;

/**
 * 应用宿主告知的主题模式（`light` / `dark` / `system`）。
 *
 * ── 为什么要宿主告诉我们 ────────────────────────────────────
 * 主题选择存在宿主的 localStorage 里（键 `workbuddy-desktop-theme`），iframe
 * 是**独立文档**，读不到它 —— 而 `prefers-color-scheme` 只在宿主把窗口主题
 * 交回系统时才跟着变（用户显式选了深色而系统是浅色时，它读到的仍是浅色）。
 * 所以模式必须由宿主递进来，写到自己 `<html data-theme>` 上，交给
 * `/plugins/theme.css` 里那套 `:root[data-theme="…"]` 选择器生效。
 *
 * 拿不到时保持 HTML 上的初值 `system`：那条路径靠 `prefers-color-scheme`
 * 兜底（与宿主 `applyTheme` 交回系统时同一条通道），最坏情况是「用户显式
 * 选了深色但系统是浅色」时页面偏亮 —— 比整页没有背景色好得多。
 */
function applyTheme(mode) {
  const value = mode === 'dark' || mode === 'light' ? mode : 'system';
  document.documentElement.dataset.theme = value;
  // 原生控件（滚动条、输入框、下拉箭头）的深浅由 color-scheme 决定，
  // 不跟着设就会在深色页面上出现一条白滚动条
  const systemDark = window.matchMedia?.('(prefers-color-scheme: dark)')?.matches === true;
  document.documentElement.style.colorScheme =
    value === 'dark' || (value === 'system' && systemDark) ? 'dark' : 'light';
}

/**
 * 监听宿主的消息：凭证与主题走同一条通道（`agent2api:*` 命名空间）。
 *
 * 监听**常驻不摘**：主题随时可能被用户在设置里改，凭证也可能迟到
 * （见 `waitForCredentials` 的时序说明）。两条消息互不依赖，谁先到都行。
 */
window.addEventListener('message', event => {
  const data = event.data;
  if (!data || typeof data !== 'object') return;
  if (data.type === 'agent2api:panel-theme') {
    applyTheme(data.theme);
  }
});

/**
 * 等宿主递凭证的上限。
 *
 * 页面**主动**向父窗口要一次（`agent2api:request-credentials`），而不是干等
 * 宿主的 `load` 钩子 —— 那条路有个时序洞：宿主在 iframe 的 `load` 里发，
 * 而模块脚本在 `load` 之前就执行完了，`directCredentials()` 慢一点（桌面壳
 * 要走一次 IPC）就会落空。主动要 + 监听兜底，两条路任一成功即可。
 */
const CREDENTIAL_TIMEOUT_MS = 3000;

/**
 * 等一次 `agent2api:panel-credentials`。
 *
 * 超时后按「同源形态」继续：web 面板的 cookie 本来就够，请求不会因为等不到
 * 消息而卡住。消息迟到时仍会更新 `panelHeaders`（监听不摘），后续请求自动
 * 带上 —— 用户点一次「刷新」就恢复正常。
 */
function waitForCredentials() {
  if (headersReady) return Promise.resolve();
  return new Promise(resolve => {
    let settled = false;
    const finish = () => {
      if (settled) return;
      settled = true;
      headersReady = true;
      resolve();
    };
    const onMessage = event => {
      const data = event.data;
      if (!data || data.type !== 'agent2api:panel-credentials') return;
      const headers = data.headers;
      if (headers && typeof headers === 'object') {
        for (const [name, value] of Object.entries(headers)) {
          if (typeof value === 'string' && value) panelHeaders[name] = value;
        }
      }
      finish();
    };
    window.addEventListener('message', onMessage);
    // 主动要一次：宿主收到后会直接把凭证回给这个 frame
    try {
      window.parent?.postMessage({ type: 'agent2api:request-credentials' }, '*');
    } catch {
      /* 父窗口不可达（页面被单独打开）：按同源处理 */
    }
    window.setTimeout(finish, CREDENTIAL_TIMEOUT_MS);
  });
}

/**
 * 主动向宿主问一次当前主题（页面首帧就用上正确的深浅）。
 *
 * 与凭证同一个模式：宿主**会**在 iframe `load` 后推一次，但那条路可能输给
 * 页面的首帧（模块脚本先于 `load` 执行完），所以页面自己再要一次。
 * 父窗口不可达（页面被单独打开调试）时保持 `system`，`prefers-color-scheme`
 * 仍然够用。
 */
export function requestTheme() {
  try {
    window.parent?.postMessage({ type: 'agent2api:request-theme' }, '*');
  } catch {
    /* 单独打开：保持 system */
  }
}

/** 把三种信封归一成 `{ data, error }`；`error` 非空即失败 */
function unwrap(payload) {
  if (!payload || typeof payload !== 'object') {
    return { data: payload, error: '' };
  }
  // ① ok_json / management_error 那一族
  if (Object.prototype.hasOwnProperty.call(payload, 'success')) {
    if (payload.success === false) {
      return { data: null, error: String(payload.error || '请求失败') };
    }
    return { data: payload.data, error: '' };
  }
  // ② OpenAI 风格（billing / 上游转发）
  if (payload.error) {
    const error = payload.error;
    const message = typeof error === 'string'
      ? error
      : String(error.message || error.type || '请求失败');
    return { data: null, error: message };
  }
  // ③ 裸对象（少数接口直接回 data）
  return { data: payload, error: '' };
}

/**
 * 发一条管理 API 请求。
 *
 * `path` 是网关侧的绝对路径（`/api/...`）。**不要**拼基址：同源形态下相对
 * 路径天然正确；跨源形态下 iframe 的文档本身就在网关 origin 上，相对路径
 * 同样指向网关。这与「宿主页面」的处境不同（那里相对路径会打到壳的资源），
 * 所以插件页不需要知道网关地址。
 */
export async function request(path, options = {}) {
  await waitForCredentials();
  const method = options.method || 'GET';
  const headers = { Accept: 'application/json', ...panelHeaders };
  const init = { method, headers, credentials: 'include' };
  if (options.body !== undefined || method !== 'GET') {
    headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(options.body ?? {});
  }
  let response;
  try {
    response = await fetch(path, init);
  } catch (error) {
    throw new Error(`无法连接网关：${error instanceof Error ? error.message : String(error)}`);
  }
  const text = await response.text();
  let payload = null;
  if (text) {
    try {
      payload = JSON.parse(text);
    } catch {
      throw new Error(text.trim() || `网关返回了非 JSON 响应（HTTP ${response.status}）`);
    }
  }
  const { data, error } = unwrap(payload);
  if (error) throw new Error(error);
  if (!response.ok) throw new Error(`请求失败（HTTP ${response.status}）`);
  return data;
}

/* ─── 具体端点（页面只调这一层，不直接拼 URL）────────────── */

const accountPath = id => `/api/accounts/${encodeURIComponent(String(id || ''))}`;

/** 聚合快照：每日分组 / 新手福利 / 活动福利 / 定时状态 / 记录时间线 */
export const getCheckinCenter = () => request('/api/checkin-center');

/** 批量签到（不传 id = 全部可签账号）；单账号不受范围限制 */
export const runCheckin = id => request('/api/accounts/checkin', {
  method: 'POST',
  body: id ? { id } : {},
});

/** 分档执行（保活 / 领取 / 保活+领取），只有申报了这些档位的家才有 */
export const runActivity = (id, mode) => request('/api/checkin-activity', {
  method: 'POST',
  body: { id, mode },
});

/** 定时签到条目（总开关 + 条目数组 + 提供商清单） */
export const getSchedules = () => request('/api/checkin-schedules');

/** 保存一条定时条目（按 id upsert；响应是整份最新状态） */
export const saveSchedule = payload => request('/api/checkin-schedules', {
  method: 'POST',
  body: payload,
});

export const deleteSchedule = id =>
  request(`/api/checkin-schedules/${encodeURIComponent(id)}/delete`, { method: 'POST', body: {} });

export const runSchedule = id =>
  request(`/api/checkin-schedules/${encodeURIComponent(id)}/run`, { method: 'POST', body: {} });

/** 定时签到的总开关（老接口，形状与 /api/checkin-schedules 一致） */
export const saveAutoCheckin = patch => request('/api/auto-checkin', {
  method: 'POST',
  body: patch,
});

/** 签到记录明细（真分页 + 筛选） */
export function getRecords(params = {}) {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined && value !== null && value !== '') query.set(key, String(value));
  }
  const text = query.toString();
  return request(`/api/checkin-records${text ? `?${text}` : ''}`);
}

/** 新手任务状态（`refresh` = 强制实查上游，不吃结算记忆） */
export const getOnboarding = (id, refresh) =>
  request(`${accountPath(id)}/onboarding${refresh ? '?refresh=1' : ''}`);

export const claimOnboarding = id =>
  request(`${accountPath(id)}/onboarding/claim`, { method: 'POST', body: {} });

/** 成长任务盘点（只读；任务 / 能量 / 旅行 / 开学季四块） */
export const getGrowth = id => request(`${accountPath(id)}/growth`);

export const runGrowthTask = (id, code, mode) =>
  request(`${accountPath(id)}/growth/task`, { method: 'POST', body: { code, mode: mode || 'full' } });

export const runGrowthSchool = (id, lottery) =>
  request(`${accountPath(id)}/growth/school`, { method: 'POST', body: { lottery: lottery === true } });

export const runGrowthTravel = id =>
  request(`${accountPath(id)}/growth/travel`, { method: 'POST', body: {} });

/** 保活模型链（国际版的领日活玩法；空清单回落缺省链） */
export const saveKeepalive = models => request('/api/checkin-keepalive', {
  method: 'POST',
  body: { models },
});

/** 猫猫旅行定时排程（国内版；与签到排程是两个独立排程） */
export const saveTravelSchedule = patch => request('/api/travel-schedule', {
  method: 'POST',
  body: patch,
});

export const runTravelNow = () => request('/api/travel-schedule/run', { method: 'POST', body: {} });

/** Qoder 国际版的 UMID 组件（签到依赖它生成本机设备风控身份） */
export const getUmid = () => request('/api/qoder-umid');
export const installUmid = () => request('/api/qoder-umid/install', { method: 'POST', body: {} });

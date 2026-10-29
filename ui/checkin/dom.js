/**
 * 极小的 DOM 构造工具。
 *
 * ── 为什么不用框架 ──────────────────────────────────────────
 * 这一页的**结构**由命令式代码拼（三个标签页、十几处交互，全量重绘足够快，
 * 还省掉「状态与视图同步」这一整类问题）。
 *
 * 但**控件**不自己画：`badge` / `button` / `toggle` 三个函数把组件库的
 * `Badge` / `Button` / `Switch` 渲染进 DOM（见 `kit.js` 的说明）。早先这几个
 * 也是自绘的，结果与宿主差着一档尺寸（按钮矮 4px、开关滑块小 4px、徽章圆角
 * 不同）—— 迁移过来的页面因此一眼看出不一样。现在它们就是同一个组件。
 *
 * ── 为什么全部走 textContent ────────────────────────────────
 * 页面里的文字大量来自上游（账号名、错误原文、任务标题）。用 innerHTML 拼接
 * 意味着上游一句话里带个 `<` 就能破坏版面 —— 这里一律 `textContent`，需要
 * 结构就建节点。唯一的例外是 `svg()`：图标是本地常量，不含数据。
 */

import { renderKit, requireKit } from './kit.js';

/**
 * 建一个元素。
 *
 * `props` 支持的键：
 *   · `class` / `id` / `title` / `type` / `value` / `placeholder` / `href` …
 *     —— 直接赋给同名属性（`class` 走 className）；
 *   · `on<Event>` —— 事件监听（`onClick` → `click`）；
 *   · `dataset` —— 逐个写 `data-*`；
 *   · `style` —— 对象，逐键赋给 `style`；
 *   · `text` —— 文本子节点（等价于 append 一个文本节点）。
 * 其余键按 attribute 写（`hidden` / `disabled` 这类布尔属性传 true / false）。
 */
export function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props || {})) {
    if (value === undefined || value === null || value === false) continue;
    if (key === 'class') {
      node.className = value;
    } else if (key === 'text') {
      node.textContent = String(value);
    } else if (key === 'dataset') {
      for (const [name, item] of Object.entries(value)) {
        if (item !== undefined && item !== null) node.dataset[name] = String(item);
      }
    } else if (key === 'style' && typeof value === 'object') {
      for (const [name, item] of Object.entries(value)) node.style[name] = item;
    } else if (key.startsWith('on') && typeof value === 'function') {
      node.addEventListener(key.slice(2).toLowerCase(), value);
    } else if (value === true) {
      node.setAttribute(key, '');
    } else {
      node.setAttribute(key, String(value));
    }
  }
  append(node, children);
  return node;
}

/** 把子节点铺进父节点（字符串转文本节点，null / false 跳过） */
export function append(parent, children) {
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false) continue;
    parent.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return parent;
}

/** 清空并重填一个容器 */
export function fill(container, ...children) {
  if (!container) return container;
  container.replaceChildren();
  append(container, children);
  return container;
}

/** 本地图标（24×24、currentColor、描边 1.8 —— 与宿主那套同一画法） */
const ICONS = {
  check: '<path d="m4.5 12.5 5 5 10-11"/>',
  chevron: '<path d="M9.5 5.5 16 12l-6.5 6.5"/>',
  refresh: '<path d="M20 12a8 8 0 1 1-2.6-5.9"/><path d="M20 4v4.5h-4.5"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  trash: '<path d="M5 7h14M9.5 7V5.2h5V7M7 7l.9 12.4h8.2L17 7"/>',
  play: '<path d="M8 5.2c0-.9 1-1.5 1.8-1l9.4 6.1c.8.5.8 1.7 0 2.2L9.8 18.6c-.8.5-1.8-.1-1.8-1z"/>',
  gift: '<rect x="3.4" y="8.6" width="17.2" height="12" rx="2.4"/><path d="M3.4 12.6h17.2M12 8.6v12"/><path d="M12 8.6C10.4 5.2 8.8 3.6 7 3.6a2.6 2.6 0 0 0 0 5.2h10a2.6 2.6 0 0 0 0-5.2c-1.8 0-3.4 1.6-5 5.2z"/>',
  spark: '<path d="M12 3.2 13.8 9l5.8 1.8-5.8 1.8L12 18.4l-1.8-5.8L4.4 10.8 10.2 9z"/>',
  clock: '<circle cx="12" cy="12" r="8.4"/><path d="M12 7.4V12l3.2 2"/>',
  alert: '<path d="M12 4.4 21 20H3z"/><path d="M12 10v4M12 17h.01"/>',
  paw: '<circle cx="7" cy="9" r="2"/><circle cx="12" cy="6.6" r="2"/><circle cx="17" cy="9" r="2"/><path d="M12 11c-3 0-5 2.4-5 4.6 0 1.6 1.2 2.6 2.8 2.6 1 0 1.6-.4 2.2-.4s1.2.4 2.2.4c1.6 0 2.8-1 2.8-2.6C17 13.4 15 11 12 11z"/>',
  cap: '<path d="M3 9.4 12 5l9 4.4-9 4.4z"/><path d="M7 11.6V16c0 1.4 2.2 2.6 5 2.6s5-1.2 5-2.6v-4.4"/>',
};

/** 建一个 SVG 图标节点 */
export function icon(name, size = 16) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('width', String(size));
  svg.setAttribute('height', String(size));
  svg.setAttribute('fill', 'none');
  svg.setAttribute('stroke', 'currentColor');
  svg.setAttribute('stroke-width', '1.8');
  svg.setAttribute('stroke-linecap', 'round');
  svg.setAttribute('stroke-linejoin', 'round');
  // 图标是本地常量（上面那张表），不含用户数据，这里用 innerHTML 是安全的
  svg.innerHTML = ICONS[name] || ICONS.spark;
  return svg;
}

/**
 * 提供商图标：优先用真图标（网关 `/plugins/runtime/providers/<file>`），
 * 没有则回落首字母。
 *
 * ── 图标从哪来 ──────────────────────────────────────────────
 * 图标是**客户端**的资源（宿主 `ui/assets/providers/` 下那一批），插件包拿不到。
 * 网关因此专门开了一条 `/plugins/runtime/providers/*` 把它们出给 iframe
 * （见 `api::plugin_theme`）—— 于是这一页画出来的图标与宿主账号页是同一张图，
 * 而不是「一个灰底字母」。
 *
 * ── 哪些 id 没有专属图 ──────────────────────────────────────
 * 国际版与国内版共用同一张（`workbuddy-intl` → `workbuddy`、`qoder-intl` →
 * `qoder` …），与宿主 `PROVIDER_ICONS` 的回落口径一致：两家本来就是一条
 * 产品线，硬凑一张不同的图反而让人以为是两个东西。
 */
const ICON_FALLBACK = {
  'workbuddy-intl': 'workbuddy',
  'autoclaw-intl': 'autoclaw',
  'qoder-intl': 'qoder',
  'zcode-intl': 'zcode',
  'monkeycode-intl': 'monkeycode',
  'modelscope-intl': 'modelscope',
  'cline-pass': 'cline',
  'cline-free': 'cline',
  'accio-cn': 'accio',
};
const ICON_FILES = {
  'minimax-code': 'minimax-code.svg',
  'minimax-code-global': 'minimax-code.svg',
};

export function providerBadge(id, label) {
  const name = String(label || id || '·');
  const initial = name.trim().slice(0, 1).toUpperCase() || '·';
  const file = ICON_FILES[id] || `${ICON_FALLBACK[id] || id}.png`;
  const node = el('span', { class: 'ck-badge', title: name });
  // `<img>` 的 onError 回落到首字母：网关没这张图时不留一个破图图标
  const img = el('img', {
    src: `/plugins/runtime/providers/${file}`,
    alt: '',
    loading: 'lazy',
  });
  img.addEventListener('error', () => {
    img.remove();
    node.textContent = initial;
  });
  node.append(img);
  return node;
}

/** 时间戳（毫秒）→ 本地可读时间；缺失 / 非法给 `—` */
export function formatTime(value) {
  const stamp = Number(value);
  if (!Number.isFinite(stamp) || stamp <= 0) return '—';
  const date = new Date(stamp);
  if (Number.isNaN(date.getTime())) return '—';
  return date.toLocaleString('zh-CN', { hour12: false });
}

/** 毫秒 → `HH:MM`（下次执行时刻这类只关心当天时间的场合） */
export function formatClock(value) {
  const stamp = Number(value);
  if (!Number.isFinite(stamp) || stamp <= 0) return '';
  const date = new Date(stamp);
  if (Number.isNaN(date.getTime())) return '';
  return `${String(date.getHours()).padStart(2, '0')}:${String(date.getMinutes()).padStart(2, '0')}`;
}

/**
 * 徽章 —— 直接用组件库的 `Badge`。
 *
 * `tone` 是本页的语义档（ok / warn / err / brand / plain），映射到组件库的
 * `variant`：success / warning / destructive / brand / outline。`shape` 照原实现
 * 传（`tag` 是行内状态标，`pill` 是默认胶囊）。
 */
const BADGE_VARIANT = {
  ok: 'success',
  warn: 'warning',
  err: 'destructive',
  brand: 'brand',
  plain: 'outline',
};

export function badge(label, tone = 'plain', title = '', shape = 'pill') {
  const { React, Badge } = requireKit();
  return renderKit(React.createElement(Badge, {
    variant: BADGE_VARIANT[tone] || 'outline',
    shape: shape === 'tag' ? 'tag' : 'pill',
    title: title || undefined,
  }, label));
}

/**
 * 按钮 —— 直接用组件库的 `Button`。
 *
 * `tone` 映射：default → `default`、outline → `outline`、ghost → `ghost`、
 * danger → `destructive`。尺寸统一 `sm`（26px 高），与原实现的用法一致。
 *
 * ── 图标怎么传 ──────────────────────────────────────────────
 * 调用方给的是图标**名**（`options.icon`），这里翻成 React 的 `<svg>` 当
 * children —— 组件库的 `Button` 布局是 `inline-flex gap-1.5`，图标与文字
 * 的间距由它自己管，不必在调用点拼 span。
 *
 * 为什么不复用上面的 `icon()`：那个返回的是 DOM 节点，React 不接受把已有
 * DOM 节点当 children（会当成未知对象渲染）。同一个图标表，两种产出形态。
 */
const BUTTON_VARIANT = {
  default: 'default',
  outline: 'outline',
  ghost: 'ghost',
  danger: 'destructive',
};

/** 图标名 → React `<svg>` 元素（图标表见 `ICONS`） */
function iconElement(React, name, size = 14) {
  // 图标是本地常量（`ICONS` 表），不含用户数据 —— 用 dangerouslySetInnerHTML
  // 是安全的，也与 `icon()` 的 innerHTML 写法一致
  return React.createElement('svg', {
    viewBox: '0 0 24 24',
    width: size,
    height: size,
    fill: 'none',
    stroke: 'currentColor',
    strokeWidth: 1.8,
    strokeLinecap: 'round',
    strokeLinejoin: 'round',
    'aria-hidden': 'true',
    dangerouslySetInnerHTML: { __html: ICONS[name] || ICONS.spark },
  });
}

export function button(label, options = {}) {
  const { React, Button } = requireKit();
  const children = [];
  if (options.icon) children.push(iconElement(React, options.icon, options.iconSize || 14));
  if (label) children.push(label);
  return renderKit(React.createElement(Button, {
    size: 'sm',
    variant: BUTTON_VARIANT[options.tone] || 'outline',
    disabled: options.disabled === true,
    title: options.title || undefined,
    // `className` 交给组件库的 `cn()` 合并：它用 tailwind-merge，后写的同类
    // 工具类赢过组件默认类（例如 `text-destructive` 盖掉 ghost 的 `text-subtle`）
    className: options.className || undefined,
    onClick: options.onClick,
  }, ...children));
}

/**
 * 改一颗按钮的禁用态。
 *
 * ── 为什么不能直接 `host.disabled = flag` ──────────────────
 * `button()` 返回的是 `renderKit()` 的包装 span（`display: contents`），
 * 真按钮在它里面 —— 在 span 上写 `disabled` 只是个无人理会的自有属性。
 *
 * ── 为什么要等一帧 ─────────────────────────────────────────
 * `createRoot().render()` 在 React 18 里是**异步提交**的，调用方拿到 span 时
 * 里面常常还是空的。这里先试一次，没找到就排到下一帧 —— 那一次提交一定已经
 * 完成。调用方（表单里随勾选变化的「保存」）不需要知道这件事。
 */
export function setDisabled(host, flag) {
  const apply = () => {
    const handle = host?.querySelector?.('button');
    if (!handle) return false;
    handle.disabled = flag === true;
    return true;
  };
  if (apply()) return;
  requestAnimationFrame(() => { apply(); });
}

/**
 * 复选框 —— 直接用组件库的 `Checkbox`。
 *
 * 原生 `<input type=checkbox>` + `accent-color` 只能改填充色：尺寸、圆角、
 * 勾形都由浏览器决定（Windows 上是 13px 方块 + 系统勾）。组件库那份是 14px、
 * 3px 圆角、选中填主色 —— 签到范围一屏十几个框并排，差一档就看得出。
 *
 * `onChange` 收的是新值（不是事件）：调用方要的是「勾上没有」。
 */
export function checkbox(checked, options = {}) {
  const { React, Checkbox } = requireKit();
  return renderKit(React.createElement(Checkbox, {
    checked: checked === true,
    disabled: options.disabled === true,
    onCheckedChange: next => options.onChange?.(next === true),
  }));
}

/**
 * 开关 —— 直接用组件库的 `Switch`。
 *
 * 受控组件：`checked` 由调用方给（每次重绘都传当前值），`onChange` 里改状态。
 * 原实现是「原生 checkbox + 自绘轨道」，尺寸与组件库差 2px（19 vs 21 高）、
 * 滑块差 4px（13 vs 17），一行里几个开关并排时肉眼可见。
 */
export function toggle(checked, options = {}) {
  const { React, Switch } = requireKit();
  return renderKit(React.createElement(Switch, {
    checked: checked === true,
    disabled: options.disabled === true,
    onCheckedChange: next => options.onChange?.(next === true),
  }));
}

/**
 * 文本输入框 —— 组件库的 `Input`。
 *
 * ── 为什么是「非受控 + 回调」而不是受控 ──────────────────────
 * 调用方（`schedule.js` / `records.js`）的用法是「拿一个节点、读它的
 * `.value`」：草稿对象在事件里就地改，不触发重绘。若做成受控组件，每次
 * 按键都要把值写回 React state 并重画 —— 而这一页的重绘是**整块替换**，
 * 光标会跳走。
 *
 * 所以这里用 `defaultValue` + `onChange` 把值交给调用方，React 不持有状态。
 * 需要程序化改值时调用方自己拿节点写 `.value`（与原生 input 完全一致）。
 *
 * ── 为什么返回的是 `<input>` 本身 ────────────────────────────
 * 调用方会直接 `input.value = …`、`input.classList.add(…)`，所以必须给真
 * input。`renderKit()` 返回的是 `display:contents` 的包装 span，读不到
 * `.value` —— 这里改用 `flushSync` 强制同步提交，渲染完再取出那个节点。
 * 没有 `flushSync` 的话 `createRoot().render()` 是异步的，函数返回时
 * 容器还是空的。
 */
export function textInput(options = {}) {
  const { React, Input, createRoot, flushSync } = requireKit();
  const host = document.createElement('div');
  host.style.display = 'contents';
  const root = createRoot(host);
  flushSync(() => {
    root.render(React.createElement(Input, {
      type: options.type || 'text',
      defaultValue: options.value === undefined ? '' : String(options.value),
      placeholder: options.placeholder || undefined,
      disabled: options.disabled === true,
      // 宽度约束走组件库的 `cn()`：`w-full` 是组件默认类，`className` 里的
      // `max-w-[220px]` 与它不同属性，两者并存即可（收窄而不塌成 0）
      className: options.className || undefined,
      onChange: event => options.onInput?.(event.target.value),
      onKeyDown: options.onKeydown,
    }));
  });
  const node = host.firstElementChild;
  // React 没渲染出东西（不该发生）时退回一个原生 input，至少不返回 null
  if (node) return node;
  return el('input', {
    type: options.type || 'text',
    value: options.value === undefined ? '' : String(options.value),
    placeholder: options.placeholder || null,
    onInput: event => options.onInput?.(event.target.value),
  });
}

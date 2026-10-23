/**
 * 极小的 DOM 构造工具。
 *
 * ── 为什么不用框架 ──────────────────────────────────────────
 * 这一页要装进插件包，随包分发。引 React 会让包体积翻几倍，而这一页只有
 * 三个标签页、十几处交互 —— 全量重绘（`replaceChildren`）已经够快，还省掉了
 * 「状态与视图同步」这一整类问题。
 *
 * ── 为什么全部走 textContent ────────────────────────────────
 * 页面里的文字大量来自上游（账号名、错误原文、任务标题）。用 innerHTML 拼接
 * 意味着上游一句话里带个 `<` 就能破坏版面 —— 这里一律 `textContent`，需要
 * 结构就建节点。唯一的例外是 `svg()`：图标是本地常量，不含数据。
 */

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
 * 提供商徽标：首字母 + 按 id 定的底色。
 *
 * 为什么不显示各家真图标：图标是**客户端**的资源（宿主 `ui/assets/providers/`
 * 下那一批），插件包拿不到它们，而网关的 `/assets/*` 只有 headless 形态才托管
 * —— 桌面端 iframe 里会 404。首字母徽标两端都成立，也不必为二十几家各塞一张图。
 */
export function providerBadge(id, label) {
  const name = String(label || id || '·');
  const initial = name.trim().slice(0, 1).toUpperCase() || '·';
  // 稳定的色相：同一个 id 永远同一个颜色（换机器、重装都一样）
  let hash = 0;
  for (let index = 0; index < id.length; index += 1) {
    hash = (hash * 31 + id.charCodeAt(index)) % 360;
  }
  return el('span', {
    class: 'ck-badge',
    text: initial,
    style: { background: `oklch(0.66 0.11 ${hash})` },
    title: name,
  });
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

/** 徽章（语义色由 `tone` 定：ok / warn / err / brand / plain） */
export function badge(label, tone = 'plain', title = '') {
  return el('span', { class: `ck-pill ck-pill-${tone}`, text: label, title: title || null });
}

/** 按钮（`tone`：default / outline / ghost / danger） */
export function button(label, options = {}) {
  const node = el('button', {
    type: 'button',
    class: `ck-btn ck-btn-${options.tone || 'outline'}`,
    disabled: options.disabled === true,
    title: options.title || null,
    onClick: options.onClick,
  });
  if (options.icon) node.append(icon(options.icon, options.iconSize || 14));
  if (label) node.append(el('span', { text: label }));
  return node;
}

/** 开关（原生 checkbox 加样式，不引组件库） */
export function toggle(checked, options = {}) {
  const input = el('input', {
    type: 'checkbox',
    class: 'ck-switch',
    checked: checked === true,
    disabled: options.disabled === true,
    onChange: event => options.onChange?.(event.target.checked),
  });
  return el('label', { class: 'ck-switch-wrap' }, input, el('span', { class: 'ck-switch-track' }));
}

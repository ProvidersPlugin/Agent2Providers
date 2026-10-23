/**
 * 组件库桥接：把 `window.wbUiKit` 里的 React 组件渲染成可直接插入 DOM 的节点。
 *
 * ── 为什么是「桥接」而不是重写成 React 应用 ──────────────────
 * 这一页原本是命令式 DOM（`dom.js` 的 `el()` + 全量重绘），2600 行里绝大部分
 * 是「读快照 → 拼结构」，与框架无关。整页重写成 React 要连数据层一起动，
 * 收益只是「写法统一」；而真正的痛点是**控件观感与宿主不一致**（自绘的按钮
 * 比组件库矮 4px、徽章圆角差一档、开关滑块小 4px）。
 *
 * 所以只把控件那一层换成真组件：每个 `renderKit()` 用 `createRoot` 渲染进一个
 * 容器元素，调用方拿到的仍是 DOM 节点，`append` / `textContent` 照旧。
 * 组件库的样式、状态属性、键盘行为因此与宿主**逐字相同** —— 因为它们就是
 * 同一个组件。
 *
 * ── 组件库拿不到时为什么直接报错 ────────────────────────────
 * 早先这里有一个「降级成原生控件」的分支。那是错的：降级渲染出来的界面
 * 与宿主**长得不一样**，而「长得不一样」正是这次要消灭的东西 —— 一个悄悄
 * 变成朴素样式的页面，比一条明确的错误更难查（用户只会说「还是不对」）。
 *
 * 所以拿不到就抛：页面顶部会显示一条「组件库没加载上」的错误，直接指向
 * `/plugins/runtime/islands/ui.js` 这条链路。**能坏得这么明显，是因为它
 * 本来就不该坏** —— 那个文件由网关出（见 `api::plugin_theme`），只要网关
 * 版本对得上就一定在。
 *
 * ── 组件库从哪来 ────────────────────────────────────────────
 * 页面在 `<head>` 里同步加载 `<script src="/plugins/runtime/islands/ui.js">`，
 * 那个文件把 React 与组件挂到 `window.wbUiKit`（见 ui-islands/src/uikit.tsx）。
 * 普通脚本先于 module 执行，所以 `main.js` 跑起来时它一定已经就位。
 */

/** 组件库句柄（null = 还没读过；false = 读过但没有） */
let kit = null;

/** 读取组件库；只读一次，之后走缓存 */
function uiKit() {
  if (kit !== null) return kit;
  const handle = typeof window !== 'undefined' ? window.wbUiKit : null;
  kit = handle && typeof handle.createRoot === 'function' ? handle : false;
  return kit || null;
}

/**
 * 取组件库，拿不到就抛。
 *
 * 错误文案里带上实际看到的东西（`typeof window.wbUiKit`）—— 排查时这一句
 * 就能区分「脚本 404」与「脚本加载了但没挂上」。
 */
export function requireKit() {
  const api = uiKit();
  if (api) return api;
  throw new Error(
    '组件库未加载：/plugins/runtime/islands/ui.js 没有生效'
    + `（window.wbUiKit = ${typeof window === 'undefined' ? 'no-window' : typeof window.wbUiKit}）。`
    + '请确认网关版本与插件包匹配。',
  );
}

/**
 * 把一个 React 元素渲染成 DOM 节点。
 *
 * 返回的容器是 `<span style="display:contents">`：它本身不生成盒子，于是
 * 组件直接参与父级 flex / grid 布局 —— 与「这个控件本来就在这里」等价，
 * 不会因为多包一层而改变行高或对齐。
 */
export function renderKit(element) {
  const api = requireKit();
  const host = document.createElement('span');
  host.style.display = 'contents';
  api.createRoot(host).render(element);
  return host;
}

/**
 * 透出 `createRoot`：需要**复用同一个 root** 的地方用它（Tab 条这类会反复
 * 重画的控件）。每次 `renderKit()` 都新建 root 在那种场景下会丢焦点 ——
 * 用户在 tab 上按方向键、焦点会跟着节点一起被替换掉。
 */
export function createRoot(host) {
  return requireKit().createRoot(host);
}

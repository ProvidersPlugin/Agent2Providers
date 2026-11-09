import { execFileSync } from 'node:child_process'
import { createHash, createPrivateKey, sign } from 'node:crypto'
import { existsSync, lstatSync, mkdirSync, readdirSync, readFileSync, realpathSync, writeFileSync } from 'node:fs'
import { dirname, extname, isAbsolute, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { zip } from './zip.mjs'

function collectIcons(source, directory) {
  if (directory == null) return []
  if (typeof directory !== 'string' || !directory || isAbsolute(directory) || /[\\:\0]/.test(directory)
    || directory.split('/').some(part => !part || part === '.' || part === '..')) {
    throw new Error('pack.json 的 icons 必须是包内相对目录')
  }
  const root = realpathSync(source)
  const folder = join(root, directory)
  if (!existsSync(folder) || !lstatSync(folder).isDirectory()) throw new Error('插件声明的图标目录不存在或不是普通目录')
  const location = relative(root, realpathSync(folder))
  if (isAbsolute(location) || location.split(/[\\/]/).includes('..')) throw new Error('图标目录不能指向插件目录之外')
  const extensions = new Set(['.png', '.webp', '.jpg', '.jpeg', '.svg'])
  const files = []
  let hasCover = false
  const walk = current => {
    for (const item of readdirSync(current, { withFileTypes: true })) {
      const path = join(current, item.name)
      if (item.isSymbolicLink() || /[\\:\0]/.test(item.name)) throw new Error('图标目录不能包含链接或非法文件名')
      if (item.isDirectory()) {
        walk(path)
      } else if (item.isFile() && extensions.has(extname(item.name).toLowerCase())) {
        const size = lstatSync(path).size
        if (size === 0 || size > 2 * 1024 * 1024) throw new Error(`图标不能为空或超过 2 MiB：${item.name}`)
        files.push([relative(root, path).split('\\').join('/'), readFileSync(path)])
        if (current === folder) hasCover = true
      }
    }
  }
  walk(folder)
  if (!hasCover) throw new Error('图标目录下缺少可显示的图片，请放入 logo.png 或其他受支持的图片')
  return files.sort(([a], [b]) => a.localeCompare(b))
}

const root = dirname(fileURLToPath(import.meta.url))
const provider = process.argv[2] || 'atomcode'
if (!/^[a-z][a-z0-9-]*$/.test(provider)) throw new Error('提供商目录名称无效')
const source = join(root, provider)
const manifest = JSON.parse(readFileSync(join(source, 'pack.json'), 'utf8'))
const permissions = readFileSync(join(source, 'wasm.json'))
const icons = collectIcons(source, manifest.icons)
const target = 'wasm32-unknown-unknown'
const entry = manifest.entry?.[target]
if (!entry || entry.includes('..') || entry.includes(':') || entry.startsWith('/') || entry.includes('\\')) throw new Error('Wasm 入口路径无效')
const crate = `agent2api-wasm-${provider}`
execFileSync('cargo', ['build', '--manifest-path', join(root, 'Cargo.toml'), '-p', crate, '--target', target, '--release', '--locked'], { cwd: root, stdio: 'inherit' })
const buildRoot = process.env.CARGO_TARGET_DIR ? resolve(root, process.env.CARGO_TARGET_DIR) : join(root, 'target')
const module = readFileSync(join(buildRoot, target, 'release', `${crate.replaceAll('-', '_')}.wasm`))
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex')
if (!module.subarray(0, 8).equals(Buffer.from([0, 97, 115, 109, 1, 0, 0, 0]))) throw new Error('入口不是 Wasm 模块')
if (process.env.AGENT2API_WASM_SIGNING_KEY) {
  const providers = manifest.providers.map(item => `${item.id}:${item.order || 0}:${item.default_enabled === false ? 0 : 1}`).sort().join(',')
  const entries = Object.entries(manifest.entry).sort(([a], [b]) => a.localeCompare(b)).map(([key, value]) => `${key}=${value}`).join(',')
  const payload = `agent2api-pack-v1\nid=${manifest.id}\nversion=${manifest.version}\nprotocol=${manifest.protocol}\ntarget=${target}\nproviders=${providers}\nentry=${entries}\nbinary=${sha256(module)}\nui=\nwasm=${sha256(permissions)}\n`
  const key = createPrivateKey(readFileSync(process.env.AGENT2API_WASM_SIGNING_KEY))
  const keyId = process.env.AGENT2API_WASM_SIGNING_KEY_ID
  if (!keyId) throw new Error('签名必须指定密钥标识，不能冒用官方签名')
  manifest.signature = { algorithm: 'ed25519', keyId, target, value: sign(null, Buffer.from(payload), key).toString('base64') }
} else {
  delete manifest.signature
}
const output = process.env.AGENT2API_WASM_OUTPUT_DIR ? resolve(root, process.env.AGENT2API_WASM_OUTPUT_DIR) : join(root, 'dist')
mkdirSync(output, { recursive: true })
const name = `${manifest.id}-${manifest.version}-${target}.zip`
const file = join(output, name)
const bytes = zip([
  ['pack.json', Buffer.from(`${JSON.stringify(manifest, null, 2)}\n`)],
  ['wasm.json', permissions],
  [entry, module],
  ...icons,
])
writeFileSync(file, bytes)
writeFileSync(`${file}.sha256`, `${sha256(bytes)}  ${name}\n`)
console.log(`已构建：${file}（${bytes.length} 字节，${manifest.signature ? '已签名' : '未签名的本地开发包'}）`)

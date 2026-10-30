import { copyFile, mkdir, readFile } from 'node:fs/promises'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const version = '1.12.0'
const entry = fileURLToPath(import.meta.resolve('pele/global'))
const root = dirname(dirname(entry))
const installed = JSON.parse(await readFile(join(root, 'package.json'), 'utf8'))
if (installed.version !== version) throw new Error(`Expected Pele ${version}, found ${installed.version}`)
const output = join(import.meta.dir, '../assets/mermaid')
await mkdir(output, { recursive: true })
await copyFile(entry, join(output, 'pele.global.js'))
await copyFile(join(root, 'LICENSE'), join(output, 'LICENSE'))
console.log(`Vendored Pele ${version} for offline native previews`)

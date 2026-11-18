import { constants, closeSync, fstatSync, openSync, readFileSync } from 'node:fs'
import { spawnSync } from 'node:child_process'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')

// Only use a fixed identity for macOS desktop bundles. Other platforms,
// mobile builds and explicit environment choices retain their normal behavior.
export function signingEnvironment({ platform, args, env, projectRoot }) {
  const result = { ...env }
  if (platform !== 'darwin' || !['build', 'bundle'].includes(args[0]) ||
      Object.hasOwn(env, 'APPLE_SIGNING_IDENTITY')) return result
  const file = join(projectRoot, '.macos-signing-identity')
  let fd
  try {
    fd = openSync(file, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK)
  } catch (error) {
    if (error.code === 'ENOENT') return result
    throw new Error('Cannot open local macOS signing identity as a regular file', { cause: error })
  }
  try {
    const stat = fstatSync(fd)
    if (!stat.isFile() || ![40, 41].includes(stat.size)) throw new Error('Invalid local macOS signing identity')
    const value = readFileSync(fd, 'utf8')
    if (!/^[0-9a-fA-F]{40}\n?$/.test(value)) throw new Error('Expected one 40-digit certificate SHA-1 fingerprint')
    result.APPLE_SIGNING_IDENTITY = value.trim()
  } finally {
    closeSync(fd)
  }
  return result
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    const args = process.argv.slice(2)
    const env = signingEnvironment({ platform: process.platform, args, env: process.env, projectRoot: root })
    const child = spawnSync(process.execPath, [join(root, 'node_modules/@tauri-apps/cli/tauri.js'), ...args], { stdio: 'inherit', env })
    if (child.error) throw child.error
    process.exitCode = child.status ?? 1
  } catch (error) {
    console.error(`Tauri signing: ${error.message}`)
    process.exitCode = 1
  }
}

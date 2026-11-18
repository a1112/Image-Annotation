import assert from 'node:assert/strict'
import { mkdtempSync, rmSync, symlinkSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { test } from 'node:test'
import { signingEnvironment } from './tauri-with-signing.mjs'

test('macOS desktop bundles use a valid local identity and preserve explicit overrides', () => {
  const projectRoot = mkdtempSync(join(tmpdir(), 'rmd-signing-'))
  try {
    const args = ['build', '--bundles', 'app']
    const call = (env = {}, platform = 'darwin', input = args) => signingEnvironment({ platform, args: input, env, projectRoot })
    assert.equal(call().APPLE_SIGNING_IDENTITY, undefined)
    const identity = 'A'.repeat(40)
    writeFileSync(join(projectRoot, '.macos-signing-identity'), `${identity}\n`)
    assert.equal(call().APPLE_SIGNING_IDENTITY, identity)
    assert.equal(call({ APPLE_SIGNING_IDENTITY: '-' }).APPLE_SIGNING_IDENTITY, '-')
    assert.equal(call({}, 'linux').APPLE_SIGNING_IDENTITY, undefined)
    assert.equal(call({}, 'darwin', ['ios', 'build']).APPLE_SIGNING_IDENTITY, undefined)
    assert.equal(call({}, 'darwin', ['dev']).APPLE_SIGNING_IDENTITY, undefined)
  } finally { rmSync(projectRoot, { recursive: true, force: true }) }
})

test('local signing identity rejects malformed content and symlinks', () => {
  const projectRoot = mkdtempSync(join(tmpdir(), 'rmd-signing-'))
  const file = join(projectRoot, '.macos-signing-identity')
  const call = () => signingEnvironment({ platform: 'darwin', args: ['bundle'], env: {}, projectRoot })
  try {
    for (const bad of ['A'.repeat(39), `${'A'.repeat(40)}\nextra`, '$(touch file)', `${'A'.repeat(39)}\0`]) {
      writeFileSync(file, bad)
      assert.throws(call)
    }
    rmSync(file)
    const target = join(projectRoot, 'real')
    writeFileSync(target, 'B'.repeat(40))
    symlinkSync(target, file)
    assert.throws(call)
  } finally { rmSync(projectRoot, { recursive: true, force: true }) }
})

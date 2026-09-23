// 端点契约测试：把 adminApi / authApi / repoApi 发出的 URL、方法与请求体钉住。
//
// 这些路径是前后端唯一的耦合点，改错了不会编译报错、只会在运行时 404。
// 完整端点清单见方案 §7.2 / §8.3。

import { beforeEach, describe, expect, it } from 'vitest'

import { adminApi, authApi, repoApi } from '@/api'
import { fakeResponse, queryOf, stubFetch } from './helpers/fetch'

beforeEach(() => {
  localStorage.clear()
})

function ok() {
  return stubFetch(() => fakeResponse({ items: [], total: 0 }))
}

describe('认证端点', () => {
  it('登录带 skipAuthRedirect 语义（路径与请求体正确）', async () => {
    const calls = stubFetch(() => fakeResponse({ token: 't', user: {}, expires_at: '' }))
    await authApi.login('admin', 'pw')
    expect(calls[0]!.url).toBe('/api/v1/auth/login')
    expect(calls[0]!.method).toBe('POST')
    expect(calls[0]!.body).toEqual({ username: 'admin', password: 'pw' })
  })

  it('me / logout / providers / changePassword 路径正确', async () => {
    const calls = ok()
    await authApi.me()
    await authApi.logout()
    await authApi.providers()
    await authApi.changePassword('old', 'new')
    expect(calls.map((c) => `${c.method} ${c.url}`)).toEqual([
      'GET /api/v1/auth/me',
      'POST /api/v1/auth/logout',
      'GET /api/v1/auth/providers',
      'POST /api/v1/auth/password',
    ])
    expect(calls[3]!.body).toEqual({ old_password: 'old', new_password: 'new' })
  })
})

describe('仓库端点', () => {
  it('仓库名做 URL 编码（名字里可能有空格或斜杠）', async () => {
    const calls = ok()
    await repoApi.info('my repo')
    await repoApi.tree('a/b', { prefix: 'src', depth: 2 })
    expect(calls[0]!.url).toBe('/api/v1/repos/my%20repo/info')
    expect(calls[1]!.url.startsWith('/api/v1/repos/a%2Fb/tree?')).toBe(true)
    expect(queryOf(calls[1]!.url)).toEqual({ prefix: 'src', depth: '2' })
  })

  it('tree 支持 rev 查询（浏览历史修订）', async () => {
    const calls = ok()
    await repoApi.tree('art', { rev: 12 })
    expect(queryOf(calls[0]!.url)).toEqual({ rev: '12' })
  })

  it('log 的分页参数只有显式给出时才出现', async () => {
    const calls = ok()
    await repoApi.log('art')
    await repoApi.log('art', { limit: 50, offset: 100 })
    expect(calls[0]!.url).toBe('/api/v1/repos/art/log')
    expect(queryOf(calls[1]!.url)).toEqual({ limit: '50', offset: '100' })
  })

  it('锁列表默认不带 query，include_broken 才带', async () => {
    const calls = ok()
    await repoApi.locks('art')
    await repoApi.locks('art', { include_broken: true, path: 'a.psd' })
    expect(calls[0]!.url).toBe('/api/v1/repos/art/locks')
    expect(queryOf(calls[1]!.url)).toEqual({ include_broken: 'true', path: 'a.psd' })
  })

  it('释放锁走 DELETE，force/reason 作为 query（强制解锁留痕）', async () => {
    const calls = stubFetch(() => fakeResponse({ broken: 1, path: 'a.psd', reason: 'x' }))
    await repoApi.releaseLock('art', 'a.psd', { force: true, reason: '人已离职' })
    expect(calls[0]!.method).toBe('DELETE')
    expect(calls[0]!.url.startsWith('/api/v1/repos/art/locks/a.psd?')).toBe(true)
    expect(queryOf(calls[0]!.url)).toEqual({ force: 'true', reason: '人已离职' })
  })

  it('创建仓库只带 name / description（v0.4.17 起不再有锁策略）', async () => {
    const calls = ok()
    await repoApi.create('art', '美术资源')
    expect(calls[0]!.method).toBe('POST')
    expect(calls[0]!.body).toEqual({ name: 'art', description: '美术资源' })
    expect(Object.keys(calls[0]!.body as object)).not.toContain('lock_policy')
  })

  it('仓库设置只提交 description', async () => {
    const calls = ok()
    await adminApi.updateRepoSettings('art', { description: '改了描述' })
    expect(calls[0]!.method).toBe('PUT')
    expect(calls[0]!.body).toEqual({ description: '改了描述' })
  })

  it('用户创建/更新请求里没有 email 字段（v0.4.17 去掉了用户邮箱）', async () => {
    const calls = ok()
    await adminApi.createUser({ username: 'u1', password: 'Abcdefg1!', display_name: 'u' })
    await adminApi.updateUser(2, { display_name: '改名' })
    for (const c of calls) {
      expect(Object.keys(c.body as object)).not.toContain('email')
    }
  })
})

describe('用户与组端点', () => {
  it('用户 CRUD 路径与方法', async () => {
    const calls = ok()
    await adminApi.users()
    await adminApi.createUser({ username: 'bob', password: 'pw' })
    await adminApi.updateUser(3, { display_name: 'Bob' })
    await adminApi.resetPassword(3, 'newpw')
    await adminApi.deleteUser(3)
    expect(calls.map((c) => `${c.method} ${c.url}`)).toEqual([
      'GET /api/v1/admin/users',
      'POST /api/v1/admin/users',
      'PUT /api/v1/admin/users/3',
      'POST /api/v1/admin/users/3/password',
      'DELETE /api/v1/admin/users/3',
    ])
    expect(calls[3]!.body).toEqual({ new_password: 'newpw' })
  })

  it('组成员整批替换走 PUT，body 是 user_ids', async () => {
    const calls = ok()
    await adminApi.members(4)
    await adminApi.setMembers(4, [1, 2, 3])
    expect(calls[0]!.url).toBe('/api/v1/admin/groups/4/members')
    expect(calls[1]!.method).toBe('PUT')
    expect(calls[1]!.body).toEqual({ user_ids: [1, 2, 3] })
  })

  it('空成员列表也要发出去（清空组是合法操作，不能被 query 过滤逻辑吃掉）', async () => {
    const calls = ok()
    await adminApi.setMembers(4, [])
    expect(calls[0]!.body).toEqual({ user_ids: [] })
  })
})

describe('ACL 端点', () => {
  it('规则列表与增删改的路径一致（同一个端点用不同方法）', async () => {
    const calls = ok()
    await adminApi.acl('art')
    await adminApi.setAcl('art', { path_prefix: 'vault', subject_type: 'everyone', subject_id: 0, level: 'read', inherit: true })
    await adminApi.updateAcl('art', { id: 5, level: 'write' })
    await adminApi.deleteAcl('art', 5)
    expect(calls.map((c) => `${c.method} ${c.url}`)).toEqual([
      'GET /api/v1/admin/repos/art/acl',
      'POST /api/v1/admin/repos/art/acl',
      'PUT /api/v1/admin/repos/art/acl',
      'DELETE /api/v1/admin/repos/art/acl?id=5',
    ])
  })

  it('删除规则用 query 传 id（不是路径参数）', async () => {
    const calls = ok()
    await adminApi.deleteAcl('art', 9)
    expect(queryOf(calls[0]!.url)).toEqual({ id: '9' })
  })

  it('预览器带 user_id 与 path', async () => {
    const calls = stubFetch(() => fakeResponse({ path: '', level: 'none', reason: '', steps: [], user: {} }))
    await adminApi.aclPreview('art', 2, 'vault/a.bin')
    expect(calls[0]!.url.startsWith('/api/v1/admin/repos/art/acl/preview?')).toBe(true)
    expect(queryOf(calls[0]!.url)).toEqual({ user_id: '2', path: 'vault/a.bin' })
  })

  it('预览器 path 留空时不下发该参数（表示仓库根）', async () => {
    const calls = stubFetch(() => fakeResponse({}))
    await adminApi.aclPreview('art', 2, '')
    expect(queryOf(calls[0]!.url)).toEqual({ user_id: '2' })
  })

  it('"谁有权限"反查带 path 与 level', async () => {
    const calls = stubFetch(() => fakeResponse({ items: [], total: 0 }))
    await adminApi.aclWho('art', 'vault', 'write')
    expect(calls[0]!.url.startsWith('/api/v1/admin/repos/art/acl/who?')).toBe(true)
    expect(queryOf(calls[0]!.url)).toEqual({ path: 'vault', level: 'write' })
  })

  it('目录列表带 prefix（空前缀不发参数，由服务端理解成仓库根）', async () => {
    const calls = ok()
    await adminApi.dirs('art', 'src/assets')
    await adminApi.dirs('art', '')
    expect(queryOf(calls[0]!.url)).toEqual({ prefix: 'src/assets' })
    expect(calls[1]!.url).toBe('/api/v1/admin/repos/art/dirs')
  })
})

describe('仓库设置与 purge', () => {
  it('设置走 PUT /settings，只送给定字段', async () => {
    const calls = ok()
    await adminApi.updateRepoSettings('art', { lock_policy: 'strict', needs_lock: '**/*.psd' })
    expect(calls[0]!.method).toBe('PUT')
    expect(calls[0]!.url).toBe('/api/v1/admin/repos/art/settings')
    expect(calls[0]!.body).toEqual({ lock_policy: 'strict', needs_lock: '**/*.psd' })
  })

  it('purge 走 POST /purge，三要素原样送出', async () => {
    const calls = stubFetch(() => fakeResponse({ prefix: 'secret' }))
    await adminApi.purge('art', { prefix: 'secret', reason: '合规要求', confirm_name: 'art' })
    expect(calls[0]!.method).toBe('POST')
    expect(calls[0]!.url).toBe('/api/v1/admin/repos/art/purge')
    expect(calls[0]!.body).toEqual({ prefix: 'secret', reason: '合规要求', confirm_name: 'art' })
  })

  it('删除仓库走 DELETE', async () => {
    const calls = ok()
    await adminApi.deleteRepo('art')
    expect(calls[0]!.method).toBe('DELETE')
    expect(calls[0]!.url).toBe('/api/v1/admin/repos/art')
  })
})

describe('运维端点', () => {
  it('rebuild-refcount 与 gc 都是 POST', async () => {
    const calls = ok()
    await adminApi.maintenance()
    await adminApi.rebuildRefcount()
    await adminApi.runGc(500)
    expect(calls.map((c) => `${c.method} ${c.url}`)).toEqual([
      'GET /api/v1/admin/maintenance',
      'POST /api/v1/admin/maintenance/rebuild-refcount',
      'POST /api/v1/admin/maintenance/gc',
    ])
    expect(calls[2]!.body).toEqual({ limit: 500 })
  })

  it('audit 查询条件透传，导出附带 format=csv 与文件名', async () => {
    const calls = ok()
    await adminApi.audit({ action: 'lock.break', limit: 20 })
    expect(calls[0]!.url.startsWith('/api/v1/admin/audit?')).toBe(true)
    expect(queryOf(calls[0]!.url)).toEqual({ action: 'lock.break', limit: '20' })
  })

  it('settings 是只读 GET', async () => {
    const calls = ok()
    await adminApi.settings()
    expect(calls[0]!.method).toBe('GET')
    expect(calls[0]!.url).toBe('/api/v1/admin/settings')
  })
})

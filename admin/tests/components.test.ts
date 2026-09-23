// 组件级测试：驱动真实的挂载与交互（点击、输入），断言**发给后端的请求**与界面可见状态。
//
// Element Plus 组件被替换成轻量替身（见 helpers/stubs.ts），只保留真实 DOM 语义。

import { flushPromises, mount } from '@vue/test-utils'
import { ElMessage } from 'element-plus'
import { beforeEach, describe, expect, it, vi } from 'vitest'

import { adminApi } from '@/api'
import { ApiError } from '@/api/http'
import AclEditor from '@/components/AclEditor.vue'
import PurgeDialog from '@/components/PurgeDialog.vue'
import { aclStubs, purgeStubs } from './helpers/stubs'

vi.mock('@/api', () => ({
  adminApi: {
    acl: vi.fn(),
    setAcl: vi.fn(),
    updateAcl: vi.fn(),
    deleteAcl: vi.fn(),
    aclPreview: vi.fn(),
    aclWho: vi.fn(),
    dirs: vi.fn(),
    users: vi.fn(),
    groups: vi.fn(),
    purge: vi.fn(),
  },
}))

const repo = 'art'

function dirsResp(paths: string[], hasChildren: string[] = []) {
  return {
    items: paths.map((p) => ({
      path: p,
      name: p.split('/').pop() ?? '/',
      kind: 'dir' as const,
      has_rules: false,
      has_children: hasChildren.includes(p),
    })),
    total: paths.length,
  }
}

beforeEach(() => {
  vi.mocked(adminApi.acl).mockReset()
  vi.mocked(adminApi.setAcl).mockReset()
  vi.mocked(adminApi.updateAcl).mockReset()
  vi.mocked(adminApi.deleteAcl).mockReset()
  vi.mocked(adminApi.aclPreview).mockReset()
  vi.mocked(adminApi.aclWho).mockReset()
  vi.mocked(adminApi.dirs).mockReset()
  vi.mocked(adminApi.users).mockReset()
  vi.mocked(adminApi.groups).mockReset()
  vi.mocked(adminApi.purge).mockReset()
})

// ---------------------------------------------------------------- PurgeDialog

describe('PurgeDialog（不可恢复操作的三道闸）', () => {
  function mountDialog(props: Partial<{ modelValue: boolean; repo: string; prefix: string }> = {}) {
    return mount(PurgeDialog, {
      props: { modelValue: true, repo, ...props },
      global: { stubs: purgeStubs },
    })
  }

  /** 三个输入框顺序：前缀 / 原因 / 确认名。 */
  async function fill(w: ReturnType<typeof mountDialog>, prefix: string, reason: string, confirm: string) {
    const inputs = w.findAll('input')
    await inputs[0]!.setValue(prefix)
    await inputs[1]!.setValue(reason)
    await inputs[2]!.setValue(confirm)
    await flushPromises()
  }

  function submitButton(w: ReturnType<typeof mountDialog>) {
    return w.findAll('button').find((b) => b.text().includes('我确认'))!
  }

  it('初始状态不能提交', async () => {
    const w = mountDialog()
    await flushPromises()
    expect(submitButton(w).attributes('disabled')).toBeDefined()
  })

  it('只填前缀与原因、确认名不对 → 仍不能提交', async () => {
    const w = mountDialog()
    await fill(w, 'secret', '合规要求', 'wrong-name')
    expect(submitButton(w).attributes('disabled')).toBeDefined()
  })

  it('原因不足 4 字 → 不能提交', async () => {
    const w = mountDialog()
    await fill(w, 'secret', '误删', repo)
    expect(submitButton(w).attributes('disabled')).toBeDefined()
  })

  it('三条件齐备 → 可提交，且提交体已去空白', async () => {
    vi.mocked(adminApi.purge).mockResolvedValue({ prefix: 'secret' } as never)
    const w = mountDialog()
    await fill(w, '  secret  ', '  合规要求  ', repo)
    expect(submitButton(w).attributes('disabled')).toBeUndefined()

    await submitButton(w).trigger('click')
    await flushPromises()

    expect(adminApi.purge).toHaveBeenCalledWith(repo, {
      prefix: 'secret',
      reason: '合规要求',
      confirm_name: repo,
    })
    expect(w.emitted('done')).toHaveLength(1)
  })

  it('初始前缀可由调用方预填（从仓库详情页的目录进来）', async () => {
    const w = mountDialog({ prefix: 'src/internal' })
    await flushPromises()
    expect((w.findAll('input')[0]!.element as HTMLInputElement).value).toBe('src/internal')
    // 只有前缀还不够
    expect(submitButton(w).attributes('disabled')).toBeDefined()
  })

  it('提交失败（服务端拒绝）时不发 done、按钮恢复可用', async () => {
    vi.mocked(adminApi.purge).mockRejectedValue(new ApiError(400, { code: 'BAD_REQUEST', message: '确认名不匹配' }))
    const errSpy = vi.spyOn(ElMessage, 'error').mockImplementation(() => ({}) as never)
    const w = mountDialog()
    await fill(w, 'secret', '合规要求', repo)
    await submitButton(w).trigger('click')
    await flushPromises()

    expect(w.emitted('done')).toBeUndefined()
    expect(errSpy).toHaveBeenCalled()
    expect(submitButton(w).attributes('disabled')).toBeUndefined()
  })

  it('重新打开时表单被重置（不会把上一次的仓库名残留下来）', async () => {
    const w = mountDialog({ modelValue: false })
    await fill(w, 'secret', '合规要求', repo)
    await w.setProps({ modelValue: true })
    await flushPromises()
    for (const input of w.findAll('input')) {
      expect((input.element as HTMLInputElement).value).toBe('')
    }
  })
})

// ---------------------------------------------------------------- AclEditor

describe('AclEditor（权限矩阵）', () => {
  function seedApi(over: { rules?: unknown[]; users?: unknown[]; groups?: unknown[] } = {}) {
    vi.mocked(adminApi.acl).mockResolvedValue({
      items: over.rules ?? [],
      total: (over.rules ?? []).length,
    } as never)
    vi.mocked(adminApi.users).mockResolvedValue({
      items: over.users ?? [{ id: 1, username: 'admin' }],
      total: (over.users ?? [{}]).length,
    } as never)
    vi.mocked(adminApi.groups).mockResolvedValue({ items: over.groups ?? [], total: 0 } as never)
    vi.mocked(adminApi.dirs).mockImplementation(async (_repo: string, prefix?: string) => {
      if (!prefix) return dirsResp(['src', 'docs'], ['src']) as never
      if (prefix === 'src') return dirsResp(['src/assets']) as never
      return dirsResp([]) as never
    })
  }

  async function mountEditor() {
    const w = mount(AclEditor, {
      props: { repo },
      global: { stubs: aclStubs },
    })
    await flushPromises()
    return w
  }

  /**
   * 直接读写成组件内部状态。
   *
   * 目录前缀用的是 `el-select allow-create`，在替身里没有"可自由输入"的等价 DOM，
   * 所以这两个用例绕过下拉去改状态——被测的是 **submit 的校验与请求体组装**，
   * 不是 Element Plus 的下拉行为（那部分由 Playwright E2E 覆盖）。
   */
  interface EditorVm {
    form: { path_prefix: string; subject_type: string; subject_id?: number }
  }
  const vmOf = (w: ReturnType<typeof mountEditor> extends Promise<infer T> ? T : never) =>
    w.vm as unknown as EditorVm

  it('挂载后并发拉规则 / 用户 / 组，并按层展开目录下拉', async () => {
    seedApi()
    await mountEditor()

    expect(adminApi.acl).toHaveBeenCalledWith(repo)
    expect(adminApi.users).toHaveBeenCalledTimes(1)
    expect(adminApi.groups).toHaveBeenCalledTimes(1)
    // BFS：根一层 + src 一层
    expect(vi.mocked(adminApi.dirs).mock.calls.map((c) => c[1])).toEqual(['', 'src'])
  })

  it('顶部显示被压过的规则条数（屏障可视化的一部分）', async () => {
    seedApi({
      rules: [
        { id: 1, path_prefix: 'vault', subject_type: 'everyone', subject_id: 0, level: 'read', inherit: true, shadowed: true },
        { id: 2, path_prefix: 'vault', subject_type: 'user', subject_id: 1, level: 'admin', inherit: true, shadowed: false },
      ],
    })
    const w = await mountEditor()
    expect(w.text()).toContain('1 条规则被同层更具体的规则压过')
  })

  it('没有被压过的规则时不显示警示 tag', async () => {
    seedApi({ rules: [{ id: 1, path_prefix: '', subject_type: 'everyone', subject_id: 0, level: 'read', inherit: true }] })
    const w = await mountEditor()
    expect(w.text()).not.toContain('被同层更具体的规则压过')
  })

  it('加载失败时提示错误而不是白屏', async () => {
    const errSpy = vi.spyOn(ElMessage, 'error').mockImplementation(() => ({}) as never)
    vi.mocked(adminApi.acl).mockRejectedValue(new ApiError(403, { code: 'PERMISSION_DENIED', message: '需要系统管理员' }))
    vi.mocked(adminApi.users).mockResolvedValue({ items: [], total: 0 } as never)
    vi.mocked(adminApi.groups).mockResolvedValue({ items: [], total: 0 } as never)
    vi.mocked(adminApi.dirs).mockResolvedValue(dirsResp([]) as never)

    const w = await mountEditor()
    expect(errSpy).toHaveBeenCalledWith('需要系统管理员')
    expect(w.find('button').exists()).toBe(true)
  })

  it('新增规则：表单合法时 POST，根路径的 subject_id 补 0', async () => {
    seedApi()
    vi.mocked(adminApi.setAcl).mockResolvedValue({} as never)
    const w = await mountEditor()

    const createBtn = w.findAll('button').find((b) => b.text().includes('新增规则'))!
    await createBtn.trigger('click')
    await flushPromises()

    vmOf(w).form.path_prefix = 'vault'
    await flushPromises()

    const saveBtn = w.findAll('button').find((b) => b.text().trim() === '保存')!
    await saveBtn.trigger('click')
    await flushPromises()

    expect(adminApi.setAcl).toHaveBeenCalledWith(repo, {
      path_prefix: 'vault',
      subject_type: 'everyone',
      subject_id: 0,
      level: 'read',
      inherit: true,
    })
    // 保存成功后重新加载
    expect(adminApi.acl).toHaveBeenCalledTimes(2)
  })

  it('校验失败时不发请求，只提示（非「所有人」未选主体）', async () => {
    seedApi()
    const warnSpy = vi.spyOn(ElMessage, 'warning').mockImplementation(() => ({}) as never)
    const w = await mountEditor()

    const createBtn = w.findAll('button').find((b) => b.text().includes('新增规则'))!
    await createBtn.trigger('click')
    await flushPromises()

    // 模拟用户选了"用户"但没选具体人
    vmOf(w).form.subject_type = 'user'
    await flushPromises()

    const saveBtn = w.findAll('button').find((b) => b.text().trim() === '保存')!
    await saveBtn.trigger('click')
    await flushPromises()

    expect(adminApi.setAcl).not.toHaveBeenCalled()
    expect(warnSpy).toHaveBeenCalledWith('请选择主体')
  })

  it('谁有权限：查询条件透传到接口，结果落到表格源数据', async () => {
    seedApi()
    vi.mocked(adminApi.aclWho).mockResolvedValue({
      path: 'vault',
      level: 'write',
      items: [{ id: 1, username: 'alice', display_name: 'Alice', disabled: false, level: 'admin', via: '直接规则' }],
      total: 1,
    } as never)
    const w = await mountEditor()

    const whoBtn = w.findAll('button').find((b) => b.text().includes('谁有权限'))!
    await whoBtn.trigger('click')
    await flushPromises()

    const queryBtn = w.findAll('button').find((b) => b.text().includes('查询'))!
    await queryBtn.trigger('click')
    await flushPromises()

    expect(adminApi.aclWho).toHaveBeenCalledWith(repo, '', 'read')
  })

  it('有效权限预览器：解析结果映射成图标与文案', async () => {
    seedApi()
    vi.mocked(adminApi.aclPreview).mockResolvedValue({
      path: 'vault/a.bin',
      level: 'none',
      reason: '屏障截断于 vault',
      steps: [],
      user: { id: 1, username: 'alice', is_admin: false, groups: [] },
    } as never)
    const w = await mountEditor()

    const previewBtn = w.findAll('button').find((b) => b.text().includes('有效权限预览器'))!
    await previewBtn.trigger('click')
    await flushPromises()

    const runBtn = w.findAll('button').find((b) => b.text().includes('解析'))!
    await runBtn.trigger('click')
    await flushPromises()

    expect(adminApi.aclPreview).toHaveBeenCalledWith(repo, 1, '')
    expect(w.text()).toContain('最终权限：无（none）')
    expect(w.text()).toContain('屏障截断于 vault')
    expect(w.text()).toContain('所属组：（无）')
  })
})

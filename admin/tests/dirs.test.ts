import { describe, expect, it, vi } from 'vitest'

import type { DirNode } from '@/api/types'
import { collectDirs, DIR_MAX_DEPTH, DIR_ROOT } from '@/utils/dirs'

function node(path: string, hasChildren = false): DirNode {
  return {
    path,
    name: path.split('/').pop() ?? '/',
    kind: 'dir',
    has_rules: false,
    has_children: hasChildren,
  }
}

/** 按前缀返回子目录的假 API（模拟服务端"只给一层"的行为）。 */
function layerApi(layers: Record<string, DirNode[]>) {
  return vi.fn(async (prefix: string) => ({ items: layers[prefix] ?? [] }))
}

describe('collectDirs', () => {
  it('第 0 项恒为仓库根，界面上显示 /', () => {
    const api = layerApi({})
    return collectDirs(api).then((dirs) => {
      expect(dirs[0]).toEqual(DIR_ROOT)
      expect(dirs[0]!.path).toBe('')
      expect(dirs[0]!.name).toBe('/')
    })
  })

  it('按层展开，父目录先于子目录出现（下拉顺序即层级顺序）', async () => {
    const api = layerApi({
      '': [node('src', true), node('docs')],
      src: [node('src/assets', true)],
      'src/assets': [node('src/assets/models')],
    })
    const dirs = await collectDirs(api)
    expect(dirs.map((d) => d.path)).toEqual([
      '',
      'src',
      'docs',
      'src/assets',
      'src/assets/models',
    ])
  })

  it('只对 has_children 的目录继续下钻（叶子目录不再发请求）', async () => {
    const api = layerApi({ '': [node('a', false), node('b', true)], b: [] })
    await collectDirs(api)
    expect(api.mock.calls.map((c) => c[0])).toEqual(['', 'b'])
  })

  it('同一层内并发取数（不串行等待）', async () => {
    let inFlight = 0
    let maxInFlight = 0
    const dirs = await collectDirs(async (prefix) => {
      inFlight += 1
      maxInFlight = Math.max(maxInFlight, inFlight)
      await new Promise((r) => setTimeout(r, 5))
      inFlight -= 1
      return { items: prefix === '' ? [node('a', true), node('b', true)] : [] }
    })
    expect(maxInFlight).toBeGreaterThan(1)
    expect(dirs.map((d) => d.path)).toEqual(['', 'a', 'b'])
  })

  it('层级超过 maxDepth 就停止（默认 4 层，避免深仓库把下拉撑爆）', async () => {
    // 一条无底洞式的链：每层都有一个带子目录的节点
    const api = vi.fn(async (prefix: string) => ({
      items: [node(prefix ? `${prefix}/d` : 'd', true)],
    }))
    const dirs = await collectDirs(api)
    expect(api).toHaveBeenCalledTimes(DIR_MAX_DEPTH)
    // 根 + 4 层 = 5 个节点
    expect(dirs).toHaveLength(1 + DIR_MAX_DEPTH)
    expect(dirs.at(-1)!.path).toBe('d/d/d/d')
  })

  it('maxDepth 可显式指定', async () => {
    const api = vi.fn(async (prefix: string) => ({
      items: [node(prefix ? `${prefix}/d` : 'd', true)],
    }))
    const dirs = await collectDirs(api, 2)
    expect(dirs.map((d) => d.path)).toEqual(['', 'd', 'd/d'])
  })

  it('重复路径只保留一条（下拉里出现重复项是纯干扰）', async () => {
    const api = layerApi({
      '': [node('src', true), node('src')],
      src: [node('src'), node('docs')],
    })
    const dirs = await collectDirs(api)
    expect(dirs.map((d) => d.path)).toEqual(['', 'src', 'docs'])
  })

  it('空仓库只返回根节点', async () => {
    const dirs = await collectDirs(layerApi({}))
    expect(dirs).toEqual([DIR_ROOT])
  })

  it('后端出错时向上抛，由调用方提示（不静默吞掉）', async () => {
    const api = vi.fn(async () => {
      throw new Error('boom')
    })
    await expect(collectDirs(api)).rejects.toThrow('boom')
  })
})

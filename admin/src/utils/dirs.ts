// 目录下拉的取数逻辑。
//
// `GET /admin/repos/{repo}/dirs?prefix=` 只返回**一层**子目录（§8.3），
// 所以下拉要自己按层展开。展开过程与 API 解耦（注入 fetchLayer），便于单测。

import type { DirNode } from '@/api/types'

/** 仓库根的伪节点，界面上显示为 `/`。 */
export const DIR_ROOT: DirNode = {
  path: '',
  name: '/',
  kind: 'dir',
  has_rules: false,
  has_children: true,
}

export const DIR_MAX_DEPTH = 4

export type FetchDirsLayer = (prefix: string) => Promise<{ items: DirNode[] }>

/**
 * 按层展开目录树，返回可直接喂给下拉的扁平列表（第 0 项恒为仓库根）。
 *
 * @param maxDepth 最多向下展开几层，默认 4 层——管理端的路径前缀选择不需要更深。
 */
export async function collectDirs(
  fetchLayer: FetchDirsLayer,
  maxDepth: number = DIR_MAX_DEPTH,
): Promise<DirNode[]> {
  const out: DirNode[] = [DIR_ROOT]
  const seen = new Set<string>([''])
  let frontier: string[] = ['']

  for (let depth = 0; depth < maxDepth && frontier.length > 0; depth++) {
    const batch = await Promise.all(frontier.map((p) => fetchLayer(p)))
    const next: string[] = []
    for (const b of batch) {
      for (const d of b.items) {
        // 同一路径只出现一次：下拉里出现重复项是纯粹的干扰
        if (seen.has(d.path)) continue
        seen.add(d.path)
        out.push(d)
        if (d.has_children) next.push(d.path)
      }
    }
    frontier = next
  }

  return out
}

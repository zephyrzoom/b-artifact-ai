/**
 * 锁列表的本地乐观更新（§6.5）。
 *
 * 界面上的锁标记完全依赖 `GET /locks` 的结果，而那次请求偶发失败时（网络抖动 / 主进程忙），
 * 标记会停在过期状态 —— 用户看到"锁已经解了、树上还显示已锁"，怎么点都不变。
 *
 * 所以：**我方操作成功之后立刻在本地生效**（我知道自己刚锁了什么、刚解了什么），
 * 下一次成功的 `GET /locks` 再对账。界面的正确性因此不再取决于某一次请求的成败。
 *
 * 抽成纯函数是为了能直接单测（含"重复加锁不产生重复条目"这类边界）。
 */

import type { LockInfo } from '@shared/dto';

/** 本地乐观条目的 id（负数，服务端列表一到就会被真实条目替换）。 */
export const LOCAL_LOCK_ID = -1;

export function applyLocalLocks(
  current: readonly LockInfo[],
  add: readonly string[],
  remove: readonly string[],
  me: string,
  now = new Date().toISOString(),
): LockInfo[] {
  if (add.length === 0 && remove.length === 0) return [...current];

  const gone = new Set(remove);
  const kept = current.filter((l) => !gone.has(l.path));
  const known = new Set(kept.map((l) => l.path));

  for (const path of add) {
    if (known.has(path) || gone.has(path)) continue;
    known.add(path);
    kept.push({
      id: LOCAL_LOCK_ID,
      path,
      kind: 'file',
      owner_id: 0,
      owner: me,
      comment: null,
      created_at: now,
      expires_at: null,
    });
  }
  return kept;
}

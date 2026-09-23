/**
 * 锁列表的本地乐观更新（§6.5）。
 *
 * 这条逻辑存在的理由很具体：界面上的锁标记完全依赖 `GET /locks` 的结果，那次请求偶发失败
 * （网络抖动 / 主进程忙）时，标记会停在过期状态 —— 用户看到"锁已经解了、树上还显示已锁"，
 * 而且怎么点都不变。所以"我方操作成功后立刻本地生效"必须是**确定性的**，这批用例把它钉住。
 */

import { describe, expect, it } from 'vitest';

import type { LockInfo } from '../../src/shared/dto.js';
import { applyLocalLocks, LOCAL_LOCK_ID } from '../../src/renderer/utils/locks.js';

function lock(path: string, owner: string, id = 1): LockInfo {
  return {
    id,
    path,
    kind: 'file',
    owner_id: 1,
    owner,
    comment: null,
    created_at: '2026-09-18T00:00:00Z',
    expires_at: null,
  };
}

describe('applyLocalLocks', () => {
  it('刚锁上的路径立刻出现在列表里（不用等服务端返回）', () => {
    const out = applyLocalLocks([], ['a.psd'], [], 'alice');
    expect(out.map((l) => l.path)).toEqual(['a.psd']);
    expect(out[0]!.owner).toBe('alice');
    expect(out[0]!.id).toBe(LOCAL_LOCK_ID); // 负数 = 本地乐观条目
  });

  it('刚解开的路径立刻从列表里消失（这是"标记不消失"那条事故的回归）', () => {
    const cur = [lock('a.psd', 'alice'), lock('b.psd', 'bob', 2)];
    const out = applyLocalLocks(cur, [], ['a.psd'], 'alice');
    expect(out.map((l) => l.path)).toEqual(['b.psd']);
  });

  it('重复加锁不产生重复条目；已有条目保持原样（不覆盖服务端给的 owner/id）', () => {
    const cur = [lock('a.psd', 'alice', 7)];
    const out = applyLocalLocks(cur, ['a.psd'], [], 'alice');
    expect(out).toHaveLength(1);
    expect(out[0]!.id).toBe(7);
  });

  it('同一次调用里"加 A、解 B"互不干扰', () => {
    const cur = [lock('b.psd', 'alice', 2)];
    const out = applyLocalLocks(cur, ['a.psd'], ['b.psd'], 'alice');
    expect(out.map((l) => l.path)).toEqual(['a.psd']);
  });

  it('什么都不动时返回等值的新数组（触发 Vue 更新，而不是原地改）', () => {
    const cur = [lock('a.psd', 'alice')];
    const out = applyLocalLocks(cur, [], [], 'alice');
    expect(out).toEqual(cur);
    expect(out).not.toBe(cur);
  });
});

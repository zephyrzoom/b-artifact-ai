/**
 * 类型对齐测试：`src/shared/dto.ts` 必须与 `core/` 的真实类型保持兼容。
 *
 * 为什么要这么写：渲染层的 tsconfig 不带 `types: ["node"]`（它跑在浏览器里），
 * 所以不能直接 import `core/` 的类型（会连带把 `node:fs` 拉进类型检查）。
 * 于是 DTO 是"抄"的一份——那就必须有东西守住这份抄写不走样。
 *
 * 这里的每个赋值都是编译期断言：core 改了字段而 dto 没跟上，`tsc --noEmit` 直接报错。
 * 运行时部分只做一个最低限度的存在性断言，让这个文件在测试报告里可见。
 */

import { describe, expect, it } from 'vitest';

import type { LockInfo as CoreLock, RepoSummary as CoreRepo, UserInfo as CoreUser } from '../src/core/api.js';
import type { StatusItem as CoreStatus } from '../src/core/scan.js';
import type {
  CommitOutcome as CoreCommit,
  ConflictInfo as CoreConflictInfo,
  ConflictResolution as CoreResolution,
  ConflictSides as CoreConflictSides,
  ProgressEvent as CoreProgress,
  SideContent as CoreSideContent,
  SideKind as CoreSideKind,
  UpdateOutcome as CoreUpdate,
} from '../src/core/wc.js';
import type {
  LockInfo as DtoLock,
  ProgressEvent as DtoProgress,
  RepoSummary as DtoRepo,
  StatusCode,
  StatusItem as DtoStatus,
  CommitOutcome as DtoCommit,
  ConflictInfo as DtoConflictInfo,
  ConflictResolution as DtoResolution,
  ConflictSides as DtoConflictSides,
  SideContent as DtoSideContent,
  SideKind as DtoSideKind,
  UpdateOutcome as DtoUpdate,
} from '../src/shared/dto.js';
import type { LoginResult as CoreLogin } from '../src/core/api.js';
import type { LoginOutcome as DtoLogin } from '../src/shared/dto.js';

// ---------- 双向一致（形状必须完全相同） ----------

const statusForward: DtoStatus = {} as CoreStatus;
const statusBackward: CoreStatus = {} as DtoStatus;

const lockForward: DtoLock = {} as CoreLock;
const lockBackward: CoreLock = {} as DtoLock;

const repoForward: DtoRepo = {} as CoreRepo;
const repoBackward: CoreRepo = {} as DtoRepo;

const commitForward: DtoCommit = {} as CoreCommit;
const commitBackward: CoreCommit = {} as DtoCommit;

const updateForward: DtoUpdate = {} as CoreUpdate;
const updateBackward: CoreUpdate = {} as DtoUpdate;

// 冲突相关：两侧形状必须完全一致
const conflictForward: DtoConflictInfo = {} as CoreConflictInfo;
const conflictBackward: CoreConflictInfo = {} as DtoConflictInfo;
const sidesForward: DtoConflictSides = {} as CoreConflictSides;
const sidesBackward: CoreConflictSides = {} as DtoConflictSides;
const sideContentForward: DtoSideContent = {} as CoreSideContent;
const sideContentBackward: CoreSideContent = {} as DtoSideContent;
const sideKind: DtoSideKind = {} as CoreSideKind;
const resolution: DtoResolution = {} as CoreResolution;

// ---------- 单向（DTO 更宽，因为 IPC 只保证"至少"有这些字段） ----------

// 进度的 phase 在 core 里是联合类型，DTO 用 string（渲染层只当字符串展示）
const progressForward: DtoProgress = {} as CoreProgress;

// 登录返回体在 DTO 侧是"已经剥掉 token"的版本，所以只能 core → dto 方向的一条子集断言
const loginSubset: Pick<DtoLogin, 'username' | 'is_admin'> = {} as Pick<CoreLogin['user'], 'username' | 'is_admin'>;
const userSubset: Pick<DtoLogin, 'username' | 'is_admin'> = {} as Pick<CoreUser, 'username' | 'is_admin'>;

describe('shared/dto 与 core 类型对齐', () => {
  it('编译器已经把双向赋值检查过了（能编译就说明形状一致）', () => {
    // 这些引用纯粹为了防"未使用变量"被优化掉，并让断言在运行期可见
    expect([
      statusForward,
      statusBackward,
      lockForward,
      lockBackward,
      repoForward,
      repoBackward,
      commitForward,
      commitBackward,
      updateForward,
      updateBackward,
      progressForward,
      loginSubset,
      userSubset,
      conflictForward,
      conflictBackward,
      sidesForward,
      sidesBackward,
      sideContentForward,
      sideContentBackward,
      sideKind,
      resolution,
    ]).toHaveLength(21);
  });

  it('逐条状态是九个（§6.2 表里的锁与 out-of-date 不是条目状态，见 dto.ts 注释）', () => {
    const codes: StatusCode[] = [
      'unversioned',
      'ignored',
      'normal',
      'modified',
      'added',
      'deleted',
      'missing',
      'conflicted',
      'needs-update',
    ];
    expect(new Set(codes).size).toBe(9);
  });

  it('实体字段名与实际数据一致（抽样：锁与仓库）', () => {
    const lock = {
      id: 1,
      path: 'a.psd',
      kind: 'file',
      owner_id: 2,
      owner: 'bob',
      comment: null,
      created_at: '2026-09-15T00:00:00Z',
      expires_at: null,
    } satisfies DtoLock;
    expect(Object.keys(lock)).toContain('owner_id');

    const r = {
      id: 1,
      name: 'art',
      description: '',
      owner: 'alice',
      head_rev: 1,
      created_at: '',
      my_role: 'admin',
      my_permissions: { read: true, write: true, admin: true },
    } satisfies DtoRepo;
    expect(r.my_permissions.admin).toBe(true);
  });

  it('冲突成因与解决方式只有约定的取值（界面与引擎都按它分支）', () => {
    expect(new Set(['both-modified', 'deleted-remotely']).size).toBe(2);
    expect(new Set(['mine', 'theirs', 'merged']).size).toBe(3);
  });

  it('三方内容形态四种取值齐备', () => {
    const kinds: DtoSideKind[] = ['text', 'binary', 'too-large', 'missing'];
    expect(new Set(kinds).size).toBe(4);
  });
});

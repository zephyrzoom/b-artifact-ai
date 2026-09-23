import { existsSync, writeFileSync } from 'node:fs';
import { mkdir, mkdtemp, rm } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  SCHEMA_VERSION,
  Wc,
  acquireProcessLock,
  isWorkingCopy,
  type EntryRow,
  type WcMeta,
} from '../src/core/db.js';
import { WcError } from '../src/core/errors.js';

// 同一个运行时加载技巧（Vite 5 不认识 node:sqlite）
const { DatabaseSync } = createRequire(import.meta.url)('node:sqlite') as {
  DatabaseSync: new (path: string) => { exec(sql: string): void; close(): void };
};

let dir: string;

beforeEach(async () => {
  dir = await mkdtemp(join(tmpdir(), 'ba-wcdb-'));
});

afterEach(async () => {
  await rm(dir, { recursive: true, force: true });
});

function meta(over: Partial<WcMeta> = {}): WcMeta {
  return {
    server: 'http://127.0.0.1:18321/api/v1',
    repo: 'art',
    revision: 0,
    sparse_paths: [],
    user: 'alice',
    cache_dir: '/tmp/ba-cache',
    ...over,
  };
}

function file(path: string, over: Partial<EntryRow> = {}): EntryRow {
  return {
    path,
    kind: 'file',
    base_rev: 1,
    base_hash: 'sha$' + path,
    size: 10,
    mtime_ms: 1000,
    mode: 0,
    status: 'normal',
    case_conflict: false,
    ...over,
  };
}

describe('wc.db 生命周期', () => {
  it('init 建库并写入 meta；open 能读回', () => {
    const wc = Wc.init(dir, meta({ revision: 7, sparse_paths: ['char'] }));
    try {
      expect(isWorkingCopy(dir)).toBe(true);
      expect(wc.revision).toBe(7);
      expect(wc.sparsePaths).toEqual(['char']);
      const m = wc.getMeta();
      expect(m.repo).toBe('art');
      expect(m.user).toBe('alice');
    } finally {
      wc.close();
    }

    const again = Wc.open(dir);
    try {
      expect(again.revision).toBe(7);
    } finally {
      again.close();
    }
  });

  it('open 非工作副本抛 NOT_A_WORKING_COPY', () => {
    expect(() => Wc.open(dir)).toThrow(WcError);
    try {
      Wc.open(dir);
    } catch (e) {
      expect((e as WcError).code).toBe('NOT_A_WORKING_COPY');
    }
  });

  it('重复 open 是幂等的（迁移不重跑）', () => {
    const wc = Wc.init(dir, meta());
    wc.upsertEntry(file('a.psd'));
    wc.close();

    const w2 = Wc.open(dir);
    expect(w2.allEntries()).toHaveLength(1);
    w2.close();

    const w3 = Wc.open(dir);
    expect(w3.allEntries()).toHaveLength(1);
    expect(
      w3.get<{ version: number }>('SELECT version FROM schema_version')?.version,
    ).toBe(SCHEMA_VERSION);
    w3.close();
  });

  it('迁移在已有数据上不丢数据', () => {
    const wc = Wc.init(dir, meta());
    wc.upsertEntry(file('keep.psd'));
    wc.close();
    const w2 = Wc.open(dir); // 触发 migrate，应识别已是 v1 而跳过
    expect(w2.getEntry('keep.psd')?.base_hash).toBe('sha$keep.psd');
    w2.close();
  });
});

describe('entries 增删查', () => {
  let wc: Wc;

  beforeEach(() => {
    wc = Wc.init(dir, meta());
  });

  afterEach(() => wc.close());

  it('upsert 幂等（同 path 覆盖而非报错）', () => {
    wc.upsertEntry(file('a.psd', { base_hash: 'v1', status: 'normal' }));
    wc.upsertEntry(file('a.psd', { base_hash: 'v2', status: 'modified' }));
    expect(wc.allEntries()).toHaveLength(1);
    const e = wc.getEntry('a.psd')!;
    expect(e.base_hash).toBe('v2');
    expect(e.status).toBe('modified');
  });

  it('boolean 字段被正确绑定与读回', () => {
    wc.upsertEntry(file('a.psd', { case_conflict: true }));
    expect(wc.getEntry('a.psd')!.case_conflict).toBe(true);
    wc.upsertEntry(file('a.psd', { case_conflict: false }));
    expect(wc.getEntry('a.psd')!.case_conflict).toBe(false);
  });

  it('hasEntry / deleteEntry', () => {
    wc.upsertEntry(file('a.psd'));
    expect(wc.hasEntry('a.psd')).toBe(true);
    wc.deleteEntry('a.psd');
    expect(wc.hasEntry('a.psd')).toBe(false);
    expect(wc.getEntry('a.psd')).toBeUndefined();
  });

  it('entriesUnder：含自身、含子树，排除同级同名前缀', () => {
    for (const p of [
      'char',
      'char/hero.psd',
      'char/deep/tex.png',
      'char.psd', // 同级同名前缀：不应命中
      'chars/a.psd', // 前缀扩展：不应命中
      'char2', // 字节序在 'char0' 之后：不应命中
      'other/x.psd',
    ]) {
      wc.upsertEntry(file(p, { kind: p.includes('.') ? 'file' : 'dir' }));
    }
    const got = wc.entriesUnder('char').map((e) => e.path);
    expect(got).toEqual(['char', 'char/deep/tex.png', 'char/hero.psd']);
  });

  it('entriesUnder 空前缀等价于全量', () => {
    wc.upsertEntry(file('a.psd'));
    wc.upsertEntry(file('b/c.psd'));
    expect(wc.entriesUnder('').map((e) => e.path)).toEqual(['a.psd', 'b/c.psd']);
  });
});

describe('pending 待提交集', () => {
  let wc: Wc;

  beforeEach(() => {
    wc = Wc.init(dir, meta());
  });

  afterEach(() => wc.close());

  it('stage 覆盖同 path 的 op；unstage / clearPending 生效', () => {
    wc.stage('a.psd', 'add');
    expect(wc.pendingOp('a.psd')).toBe('add');
    wc.stage('a.psd', 'modify');
    expect(wc.allPending()).toHaveLength(1);
    expect(wc.pendingOp('a.psd')).toBe('modify');

    wc.stage('b.psd', 'delete');
    expect(wc.allPending()).toHaveLength(2);
    wc.unstage('b.psd');
    expect(wc.pendingOp('b.psd')).toBeUndefined();

    wc.clearPending();
    expect(wc.allPending()).toEqual([]);
  });

  it('allPending 按 path 排序', () => {
    wc.stage('z.psd', 'add');
    wc.stage('a.psd', 'add');
    expect(wc.allPending().map((p) => p.path)).toEqual(['a.psd', 'z.psd']);
  });
});

describe('locks 表', () => {
  let wc: Wc;

  beforeEach(() => {
    wc = Wc.init(dir, meta());
  });

  afterEach(() => wc.close());

  it('putLock / getLock / removeLock，nullable 字段正常', () => {
    wc.putLock({
      path: 'char',
      kind: 'dir',
      token: 'tok-1',
      owner: 'alice',
      comment: null,
      expires_at: null,
    });
    const l = wc.getLock('char')!;
    expect(l.token).toBe('tok-1');
    expect(l.comment).toBeNull();
    expect(l.expires_at).toBeNull();

    wc.removeLock('char');
    expect(wc.getLock('char')).toBeUndefined();
  });
});

describe('tx 事务', () => {
  it('抛错回滚，不留下半成品', () => {
    const wc = Wc.init(dir, meta());
    try {
      expect(() =>
        wc.tx(() => {
          wc.upsertEntry(file('a.psd'));
          throw new Error('boom');
        }),
      ).toThrow('boom');
      expect(wc.allEntries()).toEqual([]);
    } finally {
      wc.close();
    }
  });
});

describe('acquireProcessLock', () => {
  it('同 pid 可重入；引用计数归零后才真正摘锁', () => {
    Wc.init(dir, meta()).close();
    const lockPath = join(dir, '.b-artifact', 'wc.lock');

    const inner = acquireProcessLock(dir);
    const outer = acquireProcessLock(dir); // 同 pid，不是真冲突
    expect(typeof outer).toBe('function');

    inner(); // 内层释放不应摘掉外层还持有的锁
    expect(existsSync(lockPath)).toBe(true);
    outer();
    expect(existsSync(lockPath)).toBe(false);
  });

  it('存活的他人 pid 会抛 WC_BUSY', async () => {
    Wc.init(dir, meta()).close();
    // 必须用一个确实存活的 pid：沙箱里对 pid 1 发 0 信号会 EPERM，
    // 被 pidAlive 当成"已死"，所以这里真起一个子进程。
    const child = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], {
      stdio: 'ignore',
    });
    try {
      writeFileSync(join(dir, '.b-artifact', 'wc.lock'), String(child.pid), 'utf8');
      let caught: unknown;
      try {
        acquireProcessLock(dir);
      } catch (e) {
        caught = e;
      }
      expect(caught).toBeInstanceOf(WcError);
      expect((caught as WcError).code).toBe('WC_BUSY');
    } finally {
      child.kill('SIGKILL');
    }
  });

  it('残留的死 pid 锁不阻塞（崩溃恢复）', () => {
    Wc.init(dir, meta()).close();
    // 4194303 远超 macOS 默认 pid 上限，必然不存在
    writeFileSync(join(dir, '.b-artifact', 'wc.lock'), '4194303', 'utf8');
    const r = acquireProcessLock(dir);
    expect(typeof r).toBe('function');
    r();
  });

  it('release 幂等：重复调用不误删他人锁', () => {
    Wc.init(dir, meta()).close();
    const release = acquireProcessLock(dir);
    release();
    // 另一个「进程」接管
    writeFileSync(join(dir, '.b-artifact', 'wc.lock'), '1', 'utf8');
    release(); // 再调一次：此时文件内容是别人的 pid，不应被删
    expect(existsSync(join(dir, '.b-artifact', 'wc.lock'))).toBe(true);
  });
});

describe('迁移 v1 → v2（conflicts 表）', () => {
  /** v1 的建表语句原样快照——迁移测试必须钉住"当时的样子"，不能跟着实现一起漂。 */
  const V1_DDL = `
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE entries (
      path TEXT PRIMARY KEY, kind TEXT NOT NULL CHECK (kind IN ('file','dir')),
      base_rev INTEGER NOT NULL, base_hash TEXT, size INTEGER NOT NULL DEFAULT 0,
      mtime_ms INTEGER NOT NULL DEFAULT 0, mode INTEGER NOT NULL DEFAULT 0,
      status TEXT NOT NULL DEFAULT 'normal', case_conflict INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX idx_entries_status ON entries(status);
    CREATE TABLE pending (path TEXT PRIMARY KEY, op TEXT NOT NULL CHECK (op IN ('add','modify','delete')), staged_at INTEGER NOT NULL);
    CREATE TABLE locks (path TEXT PRIMARY KEY, kind TEXT NOT NULL CHECK (kind IN ('file','dir')), token TEXT NOT NULL, owner TEXT, comment TEXT, expires_at TEXT);
    CREATE TABLE schema_version (version INTEGER NOT NULL);
    INSERT INTO schema_version (version) VALUES (1);
    INSERT INTO entries (path, kind, base_rev, base_hash, size, mtime_ms, mode, status, case_conflict)
      VALUES ('keep.psd', 'file', 3, 'sha$keep', 10, 111, 0, 'modified', 0);
    INSERT INTO meta (key, value) VALUES ('repo', '"art"');
  `;

  it('旧库（v1）打开后自动补上 conflicts 表，存量数据一条不丢', async () => {
    const wcDir = join(dir, '.b-artifact');
    await mkdir(wcDir, { recursive: true });
    const raw = new DatabaseSync(join(wcDir, 'wc.db'));
    raw.exec(V1_DDL);
    raw.close();

    const wc = Wc.open(dir);
    try {
      expect(wc.getEntry('keep.psd')?.base_hash).toBe('sha$keep');
      expect(wc.getEntry('keep.psd')?.status).toBe('modified');
      expect(wc.getMeta().repo).toBe('art');
      expect(wc.get<{ version: number }>('SELECT version FROM schema_version')?.version).toBe(
        SCHEMA_VERSION,
      );
      // 新表可用
      expect(() => wc.allConflicts()).not.toThrow();
      expect(wc.allConflicts()).toEqual([]);
    } finally {
      wc.close();
    }
  });

  it('新库直接建到最新版本，conflicts 表就绪', () => {
    const wc = Wc.init(dir, meta());
    try {
      expect(wc.get<{ version: number }>('SELECT version FROM schema_version')?.version).toBe(
        SCHEMA_VERSION,
      );
      expect(SCHEMA_VERSION).toBe(2);
    } finally {
      wc.close();
    }
  });
});

describe('conflicts 表读写', () => {
  let wc: Wc;
  beforeEach(() => {
    wc = Wc.init(dir, meta());
  });
  afterEach(() => {
    wc.close();
  });

  const row = {
    path: 'a.psd',
    kind: 'file' as const,
    base_hash: 'sha$base',
    theirs_hash: 'sha$theirs',
    mine_hash: 'sha$mine',
    theirs_rev: 4,
    reason: 'both-modified' as const,
    created_at: 1234,
  };

  it('put → get → all 保真', () => {
    wc.putConflict(row);
    expect(wc.getConflict('a.psd')).toEqual(row);
    expect(wc.allConflicts()).toEqual([row]);
  });

  it('同一路径重复 put 是覆盖（再次冲突不会留两条）', () => {
    wc.putConflict(row);
    wc.putConflict({ ...row, theirs_rev: 9, mine_hash: 'sha$mine2' });
    expect(wc.allConflicts()).toHaveLength(1);
    expect(wc.getConflict('a.psd')!.theirs_rev).toBe(9);
  });

  it('delete-remotely 允许 theirs_hash 为空', () => {
    wc.putConflict({ ...row, path: 'gone.psd', theirs_hash: null, reason: 'deleted-remotely' });
    expect(wc.getConflict('gone.psd')!.theirs_hash).toBeNull();
    expect(wc.getConflict('gone.psd')!.reason).toBe('deleted-remotely');
  });

  it('remove 与 clear 都到位', () => {
    wc.putConflict(row);
    wc.putConflict({ ...row, path: 'b.psd' });
    wc.removeConflict('a.psd');
    expect(wc.allConflicts().map((c) => c.path)).toEqual(['b.psd']);
    wc.clearConflicts();
    expect(wc.allConflicts()).toEqual([]);
  });
});

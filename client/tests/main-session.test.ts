import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  electronSecretBox,
  NO_SECRET_BOX,
  SessionStore,
  sessionPath,
  type SecretBox,
} from '../src/main/session.js';

let dir: string;
let file: string;

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), 'ba-sess-'));
  file = join(dir, 'session.enc');
});

afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
});

/**
 * 可用的加密箱替身：真的做一次变换（base64），这样"落盘不含明文"才是有意义的断言——
 * 如果替身只加个前缀，明文还在文件里，测试会变成自欺欺人。
 */
function fakeBox(): SecretBox {
  return {
    available: true,
    encrypt: (plain) => Buffer.from(Buffer.from(plain, 'utf8').toString('base64'), 'utf8'),
    decrypt: (buf) => {
      const s = buf.toString('utf8');
      if (!/^[A-Za-z0-9+/=]+$/.test(s)) throw new Error('密文格式不对');
      return Buffer.from(s, 'base64').toString('utf8');
    },
  };
}

const state = { server: 'http://127.0.0.1:8080', username: 'alice', token: 'tok-123' };

describe('sessionPath', () => {
  it('默认落在 ~/.b-artifact/session.enc', () => {
    expect(sessionPath('/home/u/.b-artifact')).toBe('/home/u/.b-artifact/session.enc');
  });
});

describe('加密箱可用时', () => {
  it('save → 落盘；密文不包含明文 token', () => {
    const s = new SessionStore(fakeBox(), file);
    s.save(state);

    expect(existsSync(file)).toBe(true);
    const raw = readFileSync(file, 'utf8');
    // 明文里的任何一项都不该在磁盘上出现
    for (const secret of ['tok-123', 'alice', 'http://127.0.0.1:8080']) {
      expect(raw, `${secret} 以明文落盘了`).not.toContain(secret);
    }
  });

  it('重新构造（模拟重启）能读回同一份会话', () => {
    new SessionStore(fakeBox(), file).save(state);
    const s2 = new SessionStore(fakeBox(), file);
    expect(s2.get()).toEqual(state);
  });

  it('writeFile 后不留 .tmp', () => {
    new SessionStore(fakeBox(), file).save(state);
    expect(existsSync(`${file}.tmp`)).toBe(false);
  });

  it('密文损坏（换机器 / 密钥环重置）→ 清文件并当作未登录', () => {
    writeFileSync(file, 'garbage-not-enc');
    const s = new SessionStore(fakeBox(), file);
    expect(s.get()).toBeNull();
    expect(existsSync(file)).toBe(false);
  });

  it('解密出来但缺 token 字段 → 视为无效并清理', () => {
    writeFileSync(file, 'ENC:{"server":"http://x","username":"a"}');
    const s = new SessionStore(fakeBox(), file);
    expect(s.get()).toBeNull();
    expect(existsSync(file)).toBe(false);
  });

  it('clear 同时清内存与磁盘', () => {
    const s = new SessionStore(fakeBox(), file);
    s.save(state);
    s.clear();
    expect(s.get()).toBeNull();
    expect(existsSync(file)).toBe(false);
  });
});

describe('加密箱不可用时（安全默认：不落盘）', () => {
  it('save 不写任何文件', () => {
    const s = new SessionStore(NO_SECRET_BOX, file);
    s.save(state);
    expect(existsSync(file)).toBe(false);
  });

  it('本次进程内仍然可用（内存保留）', () => {
    const s = new SessionStore(NO_SECRET_BOX, file);
    s.save(state);
    expect(s.get()).toEqual(state);
  });

  it('重启后必须重新登录（内存态不持久化）', () => {
    const s = new SessionStore(NO_SECRET_BOX, file);
    s.save(state);
    s.reset(); // 模拟进程重启
    expect(s.get()).toBeNull();
  });

  it('reset 不影响磁盘上的旧文件（如果之前有过可用加密箱）', () => {
    new SessionStore(fakeBox(), file).save(state);
    const s = new SessionStore(NO_SECRET_BOX, file);
    expect(s.get()).toBeNull();
    expect(existsSync(file)).toBe(true);
  });
});

describe('electronSecretBox', () => {
  it('safeStorage 可用时映射到 encryptString / decryptString', () => {
    const box = electronSecretBox({
      isEncryptionAvailable: () => true,
      encryptString: (s) => Buffer.from(`S:${s}`),
      decryptString: (b) => b.toString().slice(2),
    });
    expect(box.available).toBe(true);
    expect(box.decrypt(box.encrypt('hi'))).toBe('hi');
  });

  it('isEncryptionAvailable 抛异常时视为不可用（不阻断启动）', () => {
    const box = electronSecretBox({
      isEncryptionAvailable: () => {
        throw new Error('keychain locked');
      },
      encryptString: () => Buffer.alloc(0),
      decryptString: () => '',
    });
    expect(box.available).toBe(false);
  });

  it('没传 safeStorage（非 Electron 环境）也视为不可用', () => {
    expect(electronSecretBox(undefined).available).toBe(false);
  });
});

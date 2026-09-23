/**
 * 登录会话（token 只在主进程，绝不进渲染层，§6.4 安全基线）。
 *
 * 落盘策略是**安全默认**：只有拿到可用的加密箱（Electron `safeStorage`，
 * 底层是 macOS Keychain / Windows DPAPI / Linux libsecret）才写盘；
 * 拿不到就只在内存里留着——宁可让用户下次启动重新登录，
 * 也不把明文 token 写到磁盘上。
 */

import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';

import { artifactHome } from '../core/home.js';

export interface SecretBox {
  /** 加密后端是否可用（不可用时调用 encrypt/decrypt 视为错误）。 */
  readonly available: boolean;
  encrypt(plain: string): Buffer;
  decrypt(payload: Buffer): string;
}

export interface SessionState {
  server: string;
  username: string;
  token: string;
}

/** 会话文件路径。 */
export function sessionPath(home: string = artifactHome()): string {
  return join(home, 'session.enc');
}

interface SafeStorageLike {
  isEncryptionAvailable(): boolean;
  encryptString(plain: string): Buffer;
  decryptString(payload: Buffer): string;
}

/** 把 Electron 的 `safeStorage` 适配成 `SecretBox`。 */
export function electronSecretBox(safeStorage: SafeStorageLike | undefined): SecretBox {
  return {
    get available(): boolean {
      try {
        return !!safeStorage && safeStorage.isEncryptionAvailable();
      } catch {
        return false;
      }
    },
    encrypt(plain: string): Buffer {
      if (!safeStorage) throw new Error('加密后端不可用');
      return safeStorage.encryptString(plain);
    },
    decrypt(payload: Buffer): string {
      if (!safeStorage) throw new Error('加密后端不可用');
      return safeStorage.decryptString(payload);
    },
  };
}

/** 加密后端不可用时的替身：任何加解密都失败，逼调用方走"不落盘"分支。 */
export const NO_SECRET_BOX: SecretBox = {
  available: false,
  encrypt() {
    throw new Error('加密后端不可用');
  },
  decrypt() {
    throw new Error('加密后端不可用');
  },
};

export class SessionStore {
  /** 不落盘时（或刚落盘后）的内存副本。 */
  private mem: SessionState | null = null;

  constructor(
    private readonly box: SecretBox,
    readonly file: string = sessionPath(),
  ) {}

  /** 当前会话（含 token）。启动时 lazy 读盘一次。 */
  get(): SessionState | null {
    if (this.mem) return this.mem;
    this.mem = this.readDisk();
    return this.mem;
  }

  private readDisk(): SessionState | null {
    if (!this.box.available || !existsSync(this.file)) return null;
    try {
      const payload = readFileSync(this.file);
      const parsed = JSON.parse(this.box.decrypt(payload)) as Partial<SessionState>;
      if (
        typeof parsed.token !== 'string' ||
        parsed.token === '' ||
        typeof parsed.server !== 'string' ||
        parsed.server === ''
      ) {
        // 能解开但结构不对（旧版本写的 / 手工改过）：同样清掉，免得每次启动都白读一遍
        this.removeFile();
        return null;
      }
      return {
        server: parsed.server,
        username: typeof parsed.username === 'string' ? parsed.username : '',
        token: parsed.token,
      };
    } catch {
      // 换过机器 / 密钥环重置 / 文件损坏：清掉，当作未登录
      this.removeFile();
      return null;
    }
  }

  save(state: SessionState): SessionState {
    this.mem = state;
    if (!this.box.available) {
      // 安全默认：拿不到加密箱就不落盘，重启后需重新登录
      this.removeFile();
      return state;
    }
    mkdirSync(dirname(this.file), { recursive: true });
    const tmp = `${this.file}.tmp`;
    writeFileSync(tmp, this.box.encrypt(JSON.stringify(state)), { mode: 0o600 });
    renameSync(tmp, this.file);
    return state;
  }

  clear(): void {
    this.mem = null;
    this.removeFile();
  }

  /** 只清内存（模拟进程重启），用于测试与"锁定界面"场景。 */
  reset(): void {
    this.mem = null;
  }

  private removeFile(): void {
    for (const p of [this.file, `${this.file}.tmp`]) {
      try {
        rmSync(p, { force: true });
      } catch {
        /* 不存在就算了 */
      }
    }
  }
}

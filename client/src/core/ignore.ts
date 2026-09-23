/**
 * `.b-artifactignore` 解析（gitignore 语法子集，§6.2 `ignored` 状态）。
 *
 * 只实现实际会用到的部分：注释、取反、目录限定、锚定、glob 通配与字符类。
 * 不做 `svn:ignore` 那套每目录属性——单文件 + 级联子目录文件已经够用。
 */

import { readFileSync } from 'node:fs';

export const IGNORE_FILE = '.b-artifactignore';

interface Rule {
  /** 规则生效的目录（'' 表示工作副本根）。 */
  base: string;
  re: RegExp;
  negated: boolean;
  dirOnly: boolean;
}

function escapeRe(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/** glob → 正则片段（只处理 `*`、`**`、`?`、`[abc]`）。 */
function globToRegExp(pattern: string): string {
  let re = '';
  let i = 0;
  while (i < pattern.length) {
    const c = pattern[i]!;
    if (c === '*') {
      let j = i;
      while (pattern[j] === '*') j++;
      const stars = j - i;
      if (stars >= 2) {
        if (pattern[j] === '/') {
          // `**/foo`：任意层级前缀（含零层）
          re += '(?:.*/)?';
          i = j + 1;
        } else {
          re += '.*';
          i = j;
        }
      } else {
        re += '[^/]*';
        i = j;
      }
    } else if (c === '?') {
      re += '[^/]';
      i++;
    } else if (c === '[') {
      const end = pattern.indexOf(']', i + 1);
      if (end < 0) {
        re += '\\[';
        i++;
      } else {
        let cls = pattern.slice(i + 1, end);
        if (cls.startsWith('!')) cls = '^' + cls.slice(1);
        re += `[${cls}]`;
        i = end + 1;
      }
    } else {
      re += escapeRe(c);
      i++;
    }
  }
  return re;
}

/**
 * 忽略规则集合。判定遵循 git 语义：**最后匹配的规则生效**，取反可覆盖先前的忽略。
 */
export class IgnoreRules {
  private readonly rules: Rule[] = [];

  /** 从文本载入规则；`base` 为规则相对目录（'' 表示根）。 */
  add(base: string, text: string): this {
    for (const rawLine of text.split(/\r?\n/)) {
      const line = rawLine.trim();
      if (!line || line.startsWith('#')) continue;

      let negated = false;
      let pattern = line;
      if (pattern.startsWith('!')) {
        negated = true;
        pattern = pattern.slice(1);
      }
      // 行尾空格已被 trim 掉；git 里需要用 `\ ` 转义，这里不支持（注释说明即可）。
      let dirOnly = false;
      if (pattern.endsWith('/')) {
        dirOnly = true;
        pattern = pattern.slice(0, -1);
      }
      if (!pattern) continue;

      let anchored = false;
      if (pattern.startsWith('/')) {
        anchored = true;
        pattern = pattern.slice(1);
      } else if (pattern.includes('/')) {
        // 含分隔符（尾部 `/` 已剥离）的规则锚定到 base。
        anchored = true;
      }

      const body = globToRegExp(pattern);
      const prefix = base === '' ? '' : `${base}/`;
      const re = new RegExp(
        anchored ? `^${escapeRe(prefix)}${body}$` : `^${escapeRe(prefix)}(?:.*/)?${body}$`,
      );
      this.rules.push({ base, re, negated, dirOnly });
    }
    return this;
  }

  /** 从文件载入；文件不存在则忽略（等价于空规则集）。 */
  addFile(base: string, absPath: string): this {
    let text: string;
    try {
      text = readFileSync(absPath, 'utf8');
    } catch {
      return this;
    }
    return this.add(base, text);
  }

  /** 路径是否被忽略。 */
  matches(relPath: string, isDir: boolean): boolean {
    let ignored = false;
    for (const r of this.rules) {
      if (r.dirOnly && !isDir) continue;
      if (r.re.test(relPath)) ignored = !r.negated;
    }
    return ignored;
  }

  get size(): number {
    return this.rules.length;
  }
}

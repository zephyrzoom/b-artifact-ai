/**
 * 用户级根目录（`~/.b-artifact`）。
 *
 * 只有一处需要可覆盖：冒烟/自动化测试不能写开发者真实的凭据与全局缓存。
 * 设 `B_ARTIFACT_HOME` 即可整体重定向（凭据、缓存都跟着走）。
 */

import { homedir } from 'node:os';
import { resolve } from 'node:path';

export function artifactHome(): string {
  const override = process.env['B_ARTIFACT_HOME'];
  return resolve(override && override.length > 0 ? override : homedir(), '.b-artifact');
}

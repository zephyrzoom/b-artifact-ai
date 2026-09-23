/**
 * 内容哈希（§6.3 提交流程步骤 2）。
 *
 * 状态判定以内容哈希为准（mtime 只作快速跳过的启发式），因此哈希必须流式——
 * 美术资源单个文件动辄几百 MB，不能整个读进内存。
 */

import { createHash } from 'node:crypto';
import { createReadStream, readFileSync } from 'node:fs';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';

/** 空内容的 sha256（与 git empty blob 同款语义，便于对齐心智模型）。 */
export const EMPTY_SHA256 =
  'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855';

/** 分块大小：与 blob 上传的分块默认对齐。 */
export const DEFAULT_CHUNK_SIZE = 8 * 1024 * 1024;
/** 整块上传上限（§7.2）：超过则走分块协议。 */
export const WHOLE_BLOB_LIMIT = 64 * 1024 * 1024;

/** 内存数据哈希。 */
export function hashBuffer(data: Uint8Array | string): string {
  return createHash('sha256').update(data).digest('hex');
}

/** 流式哈希任意可读流。 */
export async function hashStream(rs: Readable): Promise<string> {
  const h = createHash('sha256');
  await pipeline(rs, h);
  return h.digest('hex');
}

/** 文件哈希（流式，大文件不爆内存）。 */
export async function hashFile(absPath: string): Promise<string> {
  return hashStream(createReadStream(absPath));
}

/** 同步版本：仅用于单测与小文件（pristine 校验等）。 */
export function hashFileSync(absPath: string): string {
  return hashBuffer(readFileSync(absPath));
}

/** 是否需要走分块上传。 */
export function needsChunkedUpload(size: number): boolean {
  return size > WHOLE_BLOB_LIMIT;
}

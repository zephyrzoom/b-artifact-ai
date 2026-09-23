#!/usr/bin/env node
/**
 * 生成应用图标 `build/icon.png`（1024×1024）。
 *
 * 为什么自己画而不是找个现成的图：仓库里只有一个 `admin/public/favicon.svg`，
 * 而 electron-builder 要的是 ≥512×512 的 **PNG**（macOS 的 .icns 它会自己转），
 * 环境里又没有 ImageMagick / librsvg 可依赖。与其塞一个来路不明的二进制进仓库，
 * 不如用 30 行纯 Node 画一个：形状与登录页那个 logo 一致（蓝底圆角方块 + 白色 "b"）。
 *
 * 依赖只有 node:zlib —— 手写 PNG 容器（签名 + IHDR + IDAT + IEND），
 * 逐像素算距离场做抗锯齿（3×3 超采样），够用且不引任何包。
 *
 * 用法：node scripts/make-icon.mjs [尺寸，默认 1024]
 */

import { deflateSync } from 'node:zlib';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const OUT = join(ROOT, 'build', 'icon.png');
const SIZE = Number(process.argv[2] ?? 1024);

const BG = [47, 111, 237]; // #2f6fed，与登录页 logo 同色
const FG = [255, 255, 255];

/** 圆角矩形的"内部程度"：返回 0（外）~1（内），用于抗锯齿。 */
function roundRectCoverage(x, y, rx, ry, w, h, r) {
  const cx = Math.max(rx + r, Math.min(x, rx + w - r));
  const cy = Math.max(ry + r, Math.min(y, ry + h - r));
  const dx = x - cx;
  const dy = y - cy;
  const d = Math.hypot(dx, dy);
  return clamp01(r - d + 0.5);
}

/** 圆环（"b" 的肚子）覆盖度。 */
function ringCoverage(x, y, cx, cy, outer, thickness) {
  const d = Math.hypot(x - cx, y - cy);
  const inner = outer - thickness;
  return Math.min(clamp01(outer - d + 0.5), clamp01(d - inner + 0.5));
}

function clamp01(v) {
  return v < 0 ? 0 : v > 1 ? 1 : v;
}

function blend(dst, i, rgb, alpha) {
  if (alpha <= 0) return;
  dst[i] = Math.round(dst[i] * (1 - alpha) + rgb[0] * alpha);
  dst[i + 1] = Math.round(dst[i + 1] * (1 - alpha) + rgb[1] * alpha);
  dst[i + 2] = Math.round(dst[i + 2] * (1 - alpha) + rgb[2] * alpha);
  dst[i + 3] = Math.round(dst[i + 3] * (1 - alpha) + 255 * alpha);
}

/** 逐像素渲染（3×3 超采样做抗锯齿）。 */
function render(size) {
  const px = new Uint8Array(size * size * 4);
  const s = size / 1024; // 以 1024 为设计基准，按比例缩放
  const S = 3; // 超采样倍数

  // 形状（按 1024 设计坐标）
  const bgRect = { x: 64, y: 64, w: 896, h: 896, r: 200 };
  const stem = { x: 330, y: 250, w: 92, h: 524 };
  const bowl = { cx: 560, cy: 628, outer: 196, thickness: 92 };

  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      let bgA = 0;
      let fgA = 0;
      for (let sy = 0; sy < S; sy++) {
        for (let sx = 0; sx < S; sx++) {
          const px1024 = ((x + (sx + 0.5) / S) / s) * 1;
          const py1024 = ((y + (sy + 0.5) / S) / s) * 1;
          bgA += roundRectCoverage(px1024, py1024, bgRect.x, bgRect.y, bgRect.w, bgRect.h, bgRect.r);
          const inStem =
            px1024 >= stem.x && px1024 <= stem.x + stem.w && py1024 >= stem.y && py1024 <= stem.y + stem.h ? 1 : 0;
          const inBowl = ringCoverage(px1024, py1024, bowl.cx, bowl.cy, bowl.outer, bowl.thickness);
          fgA += Math.max(inStem, inBowl);
        }
      }
      const n = S * S;
      const i = (y * size + x) * 4;
      blend(px, i, BG, bgA / n);
      blend(px, i, FG, (fgA / n) * (bgA / n)); // 字形只画在底色之内
    }
  }
  return px;
}

// ---------- 最小 PNG 编码器 ----------

const CRC_TABLE = (() => {
  const t = new Int32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c;
  }
  return t;
})();

function crc32(buf) {
  let c = 0xffffffff;
  for (let i = 0; i < buf.length; i++) c = CRC_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const body = Buffer.concat([Buffer.from(type, 'ascii'), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body), 0);
  return Buffer.concat([len, body, crc]);
}

function encodePng(px, size) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  ihdr[10] = 0;
  ihdr[11] = 0;
  ihdr[12] = 0;

  // 每行前面加一个 filter 字节（0 = None）
  const raw = Buffer.alloc(size * (size * 4 + 1));
  for (let y = 0; y < size; y++) {
    const off = y * (size * 4 + 1);
    raw[off] = 0;
    Buffer.from(px.buffer, px.byteOffset + y * size * 4, size * 4).copy(raw, off + 1);
  }

  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', deflateSync(raw, { level: 9 })),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

const px = render(SIZE);
mkdirSync(dirname(OUT), { recursive: true });
writeFileSync(OUT, encodePng(px, SIZE));
console.log(`已生成 ${OUT}（${SIZE}×${SIZE}）`);

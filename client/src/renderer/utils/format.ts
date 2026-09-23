// 渲染层格式化工具（与管理端 `admin/src/utils/format.ts` 同一套语义）。

export function formatBytes(n: number | undefined | null): string {
  if (n === undefined || n === null) return '-';
  if (n < 1024) return `${n} B`;
  const units = ['KB', 'MB', 'GB', 'TB', 'PB'];
  let v = n;
  let i = -1;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  // 只有"还是字节"（i < 0）与"数值已经很大"（≥100）才不保留小数
  return `${v.toFixed(v >= 100 || i < 0 ? 0 : 1)} ${units[i]}`;
}

/** ISO8601 → `YYYY-MM-DD HH:mm`（本地时区）。 */
export function formatTime(s: string | null | undefined): string {
  if (!s) return '-';
  const d = new Date(s);
  if (Number.isNaN(d.getTime())) return s;
  const p = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/** 相对时间（"3 分钟前"），7 天以上退化为绝对时间。 */
export function formatRelative(ts: number, now: number = Date.now()): string {
  const diff = Math.max(0, now - ts);
  const min = Math.floor(diff / 60000);
  if (min < 1) return '刚刚';
  if (min < 60) return `${min} 分钟前`;
  const hour = Math.floor(min / 60);
  if (hour < 24) return `${hour} 小时前`;
  const day = Math.floor(hour / 24);
  if (day < 7) return `${day} 天前`;
  return formatTime(new Date(ts).toISOString());
}

/** 进度百分比（0–100 的整数；total 为 0 时给 0，避免 NaN 泄进 UI）。 */
export function percent(done: number, total: number): number {
  if (!Number.isFinite(total) || total <= 0) return 0;
  return Math.min(100, Math.max(0, Math.round((done / total) * 100)));
}

/** 进度条文案："12 / 30（40%）"。 */
export function progressText(done: number, total: number): string {
  return `${done} / ${total}（${percent(done, total)}%）`;
}

/** 取路径最后一段（空路径显示为 /）。 */
export function basename(p: string | null | undefined): string {
  if (!p) return '/';
  return p.split('/').filter(Boolean).pop() ?? '/';
}

/** 仓库名/路径的安全显示（空串显示为 /）。 */
export function displayPath(p: string | null | undefined): string {
  return !p ? '/' : p;
}

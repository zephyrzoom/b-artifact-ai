// 通用格式化与小工具。

export function formatBytes(n: number | undefined | null): string {
  if (n === undefined || n === null) return '-'
  if (n < 1024) return `${n} B`
  const units = ['KB', 'MB', 'GB', 'TB', 'PB']
  let v = n
  let i = -1
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024
    i += 1
  }
  // 只有"还是字节"（i < 0）和"数值已经很大"（≥100）才不保留小数。
  // 注意这里**不能**写成 `i <= 0`：i=0 是 KB 档，那样会把 1.5 KB 显示成 2 KB。
  return `${v.toFixed(v >= 100 || i < 0 ? 0 : 1)} ${units[i]}`
}

export function formatPercent(r: number, digits = 1): string {
  return `${(r * 100).toFixed(digits)}%`
}

/** ISO8601 → `YYYY-MM-DD HH:mm`（本地时区） */
export function formatTime(s: string | null | undefined): string {
  if (!s) return '-'
  const d = new Date(s)
  if (Number.isNaN(d.getTime())) return s
  const p = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(
    d.getMinutes(),
  )}`
}

export const LEVEL_LABEL: Record<string, string> = {
  none: '无',
  read: '只读',
  write: '读写',
  admin: '管理',
}

export const LEVEL_TAG: Record<string, 'info' | 'success' | 'warning' | 'danger'> = {
  none: 'info',
  read: 'success',
  write: 'warning',
  admin: 'danger',
}

export const SUBJECT_LABEL: Record<string, string> = {
  everyone: '所有人',
  group: '用户组',
  user: '用户',
}

/** 审计动作 → 中文短标签（未列出的原样显示） */
export const ACTION_LABEL: Record<string, string> = {
  'user.login': '登录',
  'user.logout': '登出',
  'user.create': '创建用户',
  'user.create_admin': '创建管理员',
  'user.update': '修改用户',
  'user.delete': '删除用户',
  'user.password_reset': '重置密码',
  'group.create': '创建组',
  'group.update': '修改组',
  'group.delete': '删除组',
  'group.members': '调整组成员',
  'repo.create': '创建仓库',
  'repo.delete': '删除仓库',
  'repo.settings': '仓库设置',
  'repo.purge': '清除历史',
  'repo.commit': '提交',
  'acl.set': '设置权限',
  'acl.update': '修改权限',
  'acl.delete': '删除权限',
  'lock.acquire': '加锁',
  'lock.release': '解锁',
  'lock.break': '强制解锁',
  'maintenance.gc': '执行 GC',
  'maintenance.rebuild_refcount': '重建引用计数',
}

export function actionLabel(a: string): string {
  return ACTION_LABEL[a] ?? a
}

export function basename(p: string): string {
  if (!p) return '/'
  return p.split('/').filter(Boolean).pop() ?? '/'
}

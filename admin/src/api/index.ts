// 服务端接口封装（§7.2 / §7.3 / §8.3）。
import { http } from './http'
import type * as T from './types'

const V1 = '/api/v1'

export const authApi = {
  providers: () => http.get<T.Providers>(`${V1}/auth/providers`),
  login: (username: string, password: string) =>
    http.post<T.LoginResp>(
      `${V1}/auth/login`,
      { username, password },
      { skipAuthRedirect: true },
    ),
  me: () => http.get<T.Me>(`${V1}/auth/me`),
  logout: () => http.post<void>(`${V1}/auth/logout`),
  changePassword: (old_password: string, new_password: string) =>
    http.post<void>(`${V1}/auth/password`, { old_password, new_password }),
}

export const repoApi = {
  list: () => http.get<T.ListResp<T.Repo>>(`${V1}/repos`),
  // v0.4.17：建仓只有名称与描述（锁策略字段已废弃）
  create: (name: string, description?: string) =>
    http.post<T.Repo>(`${V1}/repos`, { name, description }),
  info: (repo: string) => http.get<T.RepoInfo>(`${V1}/repos/${encodeURIComponent(repo)}/info`),
  tree: (repo: string, query: { prefix?: string; rev?: number; depth?: number }) =>
    http.get<T.TreeResp>(`${V1}/repos/${encodeURIComponent(repo)}/tree`, query),
  log: (repo: string, query: { limit?: number; offset?: number; prefix?: string } = {}) =>
    http.get<T.ListResp<T.Revision>>(`${V1}/repos/${encodeURIComponent(repo)}/log`, query),
  locks: (repo: string, query: { path?: string; include_broken?: boolean } = {}) =>
    http.get<T.ListResp<T.Lock>>(`${V1}/repos/${encodeURIComponent(repo)}/locks`, query),
  releaseLock: (repo: string, path: string, query: { force?: boolean; reason?: string } = {}) =>
    http.del<{ broken: number; path: string; reason?: string }>(
      `${V1}/repos/${encodeURIComponent(repo)}/locks/${path}`,
      query,
    ),
}

export const adminApi = {
  stats: () => http.get<T.Stats>(`${V1}/admin/stats`),
  audit: (query: Record<string, unknown>) =>
    http.get<T.ListResp<T.AuditRow>>(`${V1}/admin/audit`, query),
  exportAudit: (query: Record<string, unknown>) =>
    http.download(`${V1}/admin/audit`, { ...query, format: 'csv' }, 'b-artifact-audit.csv'),

  users: () => http.get<T.ListResp<T.UserRow>>(`${V1}/admin/users`),
  createUser: (body: Partial<T.UserRow> & { username: string; password?: string }) =>
    http.post<{ id: number; username: string }>(`${V1}/admin/users`, body),
  updateUser: (id: number, body: Partial<T.UserRow>) =>
    http.put<{ id: number; updated: boolean }>(`${V1}/admin/users/${id}`, body),
  deleteUser: (id: number) => http.del<{ deleted: boolean; id: number }>(`${V1}/admin/users/${id}`),
  resetPassword: (id: number, new_password: string) =>
    http.post<void>(`${V1}/admin/users/${id}/password`, { new_password }),

  groups: () => http.get<T.ListResp<T.GroupRow>>(`${V1}/admin/groups`),
  createGroup: (name: string, comment?: string) =>
    http.post<{ id: number; name: string }>(`${V1}/admin/groups`, { name, comment }),
  updateGroup: (id: number, name: string, comment?: string) =>
    http.put<{ id: number; updated: boolean }>(`${V1}/admin/groups/${id}`, { name, comment }),
  deleteGroup: (id: number) =>
    http.del<{ deleted: boolean; id: number }>(`${V1}/admin/groups/${id}`),
  members: (id: number) => http.get<T.ListResp<T.GroupMember>>(`${V1}/admin/groups/${id}/members`),
  setMembers: (id: number, user_ids: number[]) =>
    http.put<{ id: number; members: number }>(`${V1}/admin/groups/${id}/members`, { user_ids }),

  acl: (repo: string) =>
    http.get<T.ListResp<T.AclRule>>(`${V1}/admin/repos/${encodeURIComponent(repo)}/acl`),
  setAcl: (
    repo: string,
    body: {
      id?: number
      path_prefix?: string
      subject_type?: T.SubjectType
      subject_id?: number
      level?: T.Level
      inherit?: boolean
    },
  ) => http.post<T.AclRule>(`${V1}/admin/repos/${encodeURIComponent(repo)}/acl`, body),
  updateAcl: (
    repo: string,
    body: { id: number; level?: T.Level; inherit?: boolean },
  ) => http.put<T.AclRule>(`${V1}/admin/repos/${encodeURIComponent(repo)}/acl`, body),
  deleteAcl: (repo: string, id: number) =>
    http.del<{ deleted: boolean; id: number }>(
      `${V1}/admin/repos/${encodeURIComponent(repo)}/acl`,
      { id },
    ),
  aclPreview: (repo: string, user_id: number, path?: string) =>
    http.get<T.AclPreview>(`${V1}/admin/repos/${encodeURIComponent(repo)}/acl/preview`, {
      user_id,
      path,
    }),
  aclWho: (repo: string, path?: string, level?: T.Level) =>
    http.get<{ path: string; level: T.Level; items: T.AclWhoItem[]; total: number }>(
      `${V1}/admin/repos/${encodeURIComponent(repo)}/acl/who`,
      { path, level },
    ),
  dirs: (repo: string, prefix?: string) =>
    http.get<T.ListResp<T.DirNode>>(`${V1}/admin/repos/${encodeURIComponent(repo)}/dirs`, {
      prefix,
    }),

  updateRepoSettings: (
    repo: string,
    // v0.4.17：仓库设置只剩描述
    body: { description?: string },
  ) => http.put<{ name: string; updated: boolean }>(
    `${V1}/admin/repos/${encodeURIComponent(repo)}/settings`,
    body,
  ),
  deleteRepo: (repo: string) =>
    http.del<{ deleted: boolean; name: string; summary: unknown }>(
      `${V1}/admin/repos/${encodeURIComponent(repo)}`,
    ),
  purge: (repo: string, body: { prefix: string; reason: string; confirm_name: string }) =>
    http.post<T.PurgeResult>(`${V1}/admin/repos/${encodeURIComponent(repo)}/purge`, body),

  settings: () => http.get<T.SystemSettings>(`${V1}/admin/settings`),
  maintenance: () => http.get<T.Maintenance>(`${V1}/admin/maintenance`),
  rebuildRefcount: () =>
    http.post<{ blobs: number; zero_ref: number; reclaimable_bytes: number; queued: number }>(
      `${V1}/admin/maintenance/rebuild-refcount`,
      {},
    ),
  runGc: (limit?: number) =>
    http.post<{ deleted: number; skipped_pending: number; bytes: number }>(
      `${V1}/admin/maintenance/gc`,
      { limit },
    ),
}

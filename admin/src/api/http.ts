// 统一请求层：附带 Bearer token、解析 §7.1 的统一错误结构。

const TOKEN_KEY = 'b-artifact.token'

export interface ApiErrorBody {
  code: string
  message: string
  details?: unknown
}

export class ApiError extends Error {
  readonly status: number
  readonly code: string
  readonly details: unknown

  constructor(status: number, body: ApiErrorBody) {
    super(body.message)
    this.name = 'ApiError'
    this.status = status
    this.code = body.code
    this.details = body.details
  }
}

export function getToken(): string {
  return localStorage.getItem(TOKEN_KEY) ?? ''
}

export function setToken(t: string) {
  localStorage.setItem(TOKEN_KEY, t)
}

export function clearToken() {
  localStorage.removeItem(TOKEN_KEY)
}

function withQuery(path: string, query?: Record<string, unknown>): string {
  if (!query) return path
  const sp = new URLSearchParams()
  for (const [k, v] of Object.entries(query)) {
    if (v === undefined || v === null || v === '') continue
    sp.append(k, String(v))
  }
  const qs = sp.toString()
  return qs ? `${path}?${qs}` : path
}

export interface RequestOptions {
  query?: Record<string, unknown>
  body?: unknown
  /** 401 时不自动跳登录（登录页自己用） */
  skipAuthRedirect?: boolean
  signal?: AbortSignal
}

async function request<T>(
  method: string,
  path: string,
  opts: RequestOptions = {},
): Promise<T> {
  const headers = new Headers()
  if (opts.body !== undefined) headers.set('Content-Type', 'application/json')
  const token = getToken()
  if (token) headers.set('Authorization', `Bearer ${token}`)

  const res = await fetch(withQuery(path, opts.query), {
    method,
    headers,
    body: opts.body === undefined ? undefined : JSON.stringify(opts.body),
    signal: opts.signal,
  })

  if (res.status === 401) {
    clearToken()
    if (!opts.skipAuthRedirect && !location.pathname.endsWith('/login')) {
      const base = import.meta.env.BASE_URL.endsWith('/')
        ? import.meta.env.BASE_URL
        : `${import.meta.env.BASE_URL}/`
      location.replace(`${base}login`)
    }
    throw new ApiError(401, { code: 'UNAUTHENTICATED', message: '登录已失效，请重新登录' })
  }

  if (!res.ok) {
    let body: ApiErrorBody = { code: 'UNKNOWN', message: `${res.status} ${res.statusText}` }
    try {
      const j = await res.json()
      if (j?.error) body = j.error
    } catch {
      /* 非 JSON 响应保持默认 */
    }
    throw new ApiError(res.status, body)
  }

  if (res.status === 204) return undefined as T
  const ct = res.headers.get('content-type') ?? ''
  if (ct.includes('application/json')) return (await res.json()) as T
  return (await res.text()) as T
}

export const http = {
  get: <T>(p: string, query?: Record<string, unknown>, o?: RequestOptions) =>
    request<T>('GET', p, { ...o, query }),
  post: <T>(p: string, body?: unknown, o?: RequestOptions) =>
    request<T>('POST', p, { ...o, body }),
  put: <T>(p: string, body?: unknown, o?: RequestOptions) =>
    request<T>('PUT', p, { ...o, body }),
  del: <T>(p: string, query?: Record<string, unknown>, o?: RequestOptions) =>
    request<T>('DELETE', p, { ...o, query }),
  /** 下载文件（CSV 导出等）：走一次 fetch 拿 blob，保证带上 token */
  download: async (p: string, query: Record<string, unknown>, filename: string) => {
    const headers = new Headers()
    const token = getToken()
    if (token) headers.set('Authorization', `Bearer ${token}`)
    const res = await fetch(withQuery(p, query), { headers })
    if (!res.ok) throw new ApiError(res.status, { code: 'DOWNLOAD_FAILED', message: '导出失败' })
    const blob = await res.blob()
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = filename
    a.click()
    URL.revokeObjectURL(url)
  },
}

// fetch 桩：记录每次调用的 URL / 方法 / 请求体，并按处理器返回伪造 Response。
//
// 为什么不用 MSW（§12.4 原定方案）：MSW 要额外装包 + 起 service worker，
// 而管理端所有请求都收敛在 `api/http.ts` 一个函数里，直接换掉 `fetch` 断言更直接、
// 少一层黑盒。契约仍然按 §7.2 手工构造，mock 数据与真实响应字段一一对应。

import { vi } from 'vitest'

export interface RecordedCall {
  url: string
  method: string
  body: unknown
  headers: Headers
}

export interface FakeResponseInit {
  status?: number
  statusText?: string
  contentType?: string
}

/** 手工构造 Response 子集，避免依赖 happy-dom 的 Response 实现细节。 */
export function fakeResponse(body: unknown, init: FakeResponseInit = {}): Response {
  const status = init.status ?? 200
  const contentType = init.contentType ?? 'application/json'
  return {
    status,
    ok: status >= 200 && status < 300,
    statusText: init.statusText ?? 'OK',
    headers: {
      get: (k: string) => (k.toLowerCase() === 'content-type' ? contentType : null),
    },
    json: async () => body,
    text: async () => (typeof body === 'string' ? body : JSON.stringify(body)),
    blob: async () => new Blob([JSON.stringify(body)]),
  } as unknown as Response
}

export function stubFetch(
  handler: (call: RecordedCall) => Response | Promise<Response>,
): RecordedCall[] {
  const calls: RecordedCall[] = []
  vi.stubGlobal(
    'fetch',
    async (input: unknown, init: RequestInit = {}): Promise<Response> => {
      const call: RecordedCall = {
        url: String(input),
        method: (init.method ?? 'GET').toUpperCase(),
        body: typeof init.body === 'string' ? JSON.parse(init.body) : undefined,
        headers: new Headers(init.headers as HeadersInit | undefined),
      }
      calls.push(call)
      return handler(call)
    },
  )
  return calls
}

/** 永远返回同一个伪造响应。 */
export function stubFetchAlways(body: unknown, init: FakeResponseInit = {}): RecordedCall[] {
  return stubFetch(() => fakeResponse(body, init))
}

/** 解析 `?a=1&b=2`，便于断言 query 的过滤与编码行为。 */
export function queryOf(url: string): Record<string, string> {
  const i = url.indexOf('?')
  if (i < 0) return {}
  const out: Record<string, string> = {}
  for (const [k, v] of new URLSearchParams(url.slice(i + 1))) out[k] = v
  return out
}

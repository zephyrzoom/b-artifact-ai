import { beforeEach, describe, expect, it, vi } from 'vitest'

import { ApiError, clearToken, getToken, http, setToken } from '@/api/http'
import { fakeResponse, queryOf, stubFetch } from './helpers/fetch'

beforeEach(() => {
  localStorage.clear()
})

describe('token 存取', () => {
  it('set / get / clear 走同一个 key', () => {
    expect(getToken()).toBe('')
    setToken('t-1')
    expect(getToken()).toBe('t-1')
    expect(localStorage.getItem('b-artifact.token')).toBe('t-1')
    clearToken()
    expect(getToken()).toBe('')
  })
})

describe('请求构造', () => {
  it('无 token 时不带 Authorization，有 token 时带 Bearer', async () => {
    let calls = stubFetch(() => fakeResponse({ ok: true }))
    await http.get('/api/v1/repos')
    expect(calls[0]!.headers.get('Authorization')).toBeNull()

    localStorage.clear()
    setToken('abc')
    calls = stubFetch(() => fakeResponse({ ok: true }))
    await http.get('/api/v1/repos')
    expect(calls[0]!.headers.get('Authorization')).toBe('Bearer abc')
  })

  it('只 GET 不带 Content-Type，带 body 的请求才设置', async () => {
    let calls = stubFetch(() => fakeResponse({}))
    await http.post('/api/v1/x', { a: 1 })
    expect(calls[0]!.method).toBe('POST')
    expect(calls[0]!.headers.get('Content-Type')).toBe('application/json')
    expect(calls[0]!.body).toEqual({ a: 1 })

    calls = stubFetch(() => fakeResponse({}))
    await http.get('/api/v1/x')
    expect(calls[0]!.headers.get('Content-Type')).toBeNull()
  })

  it('显式传 undefined 作为 body 时不发送请求体', async () => {
    const calls = stubFetch(() => fakeResponse({}))
    await http.post('/api/v1/x')
    expect(calls[0]!.body).toBeUndefined()
  })

  it('put / del 方法正确，del 带 query', async () => {
    const calls = stubFetch(() => fakeResponse({}))
    await http.put('/api/v1/x', { b: 2 })
    await http.del('/api/v1/x', { id: 7 })
    expect(calls.map((c) => c.method)).toEqual(['PUT', 'DELETE'])
    expect(queryOf(calls[1]!.url)).toEqual({ id: '7' })
  })

  it('query 跳过 undefined / null / 空串，避免出现空参数', async () => {
    const calls = stubFetch(() => fakeResponse({}))
    await http.get('/api/v1/log', { limit: 20, offset: 0, prefix: '', path: undefined, x: null })
    expect(calls[0]!.url).toBe('/api/v1/log?limit=20&offset=0')
  })

  it('query 值做 URL 编码（中文与前缀斜杠）', async () => {
    const calls = stubFetch(() => fakeResponse({}))
    await http.get('/api/v1/dirs', { prefix: 'src/美术 资源' })
    expect(calls[0]!.url).toContain('prefix=src%2F')
    expect(queryOf(calls[0]!.url)['prefix']).toBe('src/美术 资源')
  })

  it('完全没有 query 时不追加问号', async () => {
    const calls = stubFetch(() => fakeResponse({}))
    await http.get('/api/v1/repos', {})
    expect(calls[0]!.url).toBe('/api/v1/repos')
  })
})

describe('响应解析', () => {
  it('JSON 响应直接解析', async () => {
    stubFetch(() => fakeResponse({ items: [1, 2], total: 2 }))
    await expect(http.get<{ total: number }>('/x')).resolves.toEqual({ items: [1, 2], total: 2 })
  })

  it('204 返回 undefined', async () => {
    stubFetch(() => fakeResponse(null, { status: 204, statusText: 'No Content' }))
    await expect(http.post('/x')).resolves.toBeUndefined()
  })

  it('非 JSON 响应退化返回文本（CSV 等）', async () => {
    stubFetch(() => fakeResponse('a,b\n1,2', { contentType: 'text/csv' }))
    await expect(http.get<string>('/x')).resolves.toBe('a,b\n1,2')
  })
})

describe('错误处理（§7.1）', () => {
  it('解析服务端统一错误结构，保留 status / code / details', async () => {
    stubFetch(() =>
      fakeResponse(
        { error: { code: 'PATH_INVALID', message: '路径非法', details: { path: 'a/../b' } } },
        { status: 400, statusText: 'Bad Request' },
      ),
    )
    const err = await http.get('/x').catch((e) => e)
    expect(err).toBeInstanceOf(ApiError)
    expect(err.status).toBe(400)
    expect(err.code).toBe('PATH_INVALID')
    expect(err.message).toBe('路径非法')
    expect(err.details).toEqual({ path: 'a/../b' })
    expect(err.name).toBe('ApiError')
  })

  it('错误体不是 JSON 时退化为状态码文案，不抛解析异常', async () => {
    stubFetch(() => fakeResponse('<html>502 Bad Gateway</html>', { contentType: 'text/html', status: 502, statusText: 'Bad Gateway' }))
    const err = await http.get('/x').catch((e) => e)
    expect(err).toBeInstanceOf(ApiError)
    expect(err.status).toBe(502)
    expect(err.code).toBe('UNKNOWN')
    expect(err.message).toBe('502 Bad Gateway')
  })

  it('错误体是 JSON 但没有 error 字段时同样退化', async () => {
    stubFetch(() => fakeResponse({ message: 'nope' }, { status: 500, statusText: 'Internal Server Error' }))
    const err = await http.get('/x').catch((e) => e)
    expect(err.code).toBe('UNKNOWN')
    expect(err.message).toBe('500 Internal Server Error')
  })

  it('403 保留服务端 code（强制解锁权限不足等要靠它分支）', async () => {
    stubFetch(() =>
      fakeResponse({ error: { code: 'PERMISSION_DENIED', message: '不能释放' } }, { status: 403 }),
    )
    const err = await http.del('/x').catch((e) => e)
    expect(err.code).toBe('PERMISSION_DENIED')
  })
})

describe('401 处理', () => {
  it('401 清掉 token 并抛 UNAUTHENTICATED', async () => {
    setToken('stale')
    stubFetch(() => fakeResponse({}, { status: 401 }))
    const err = await http.get('/api/v1/admin/users').catch((e) => e)
    expect(err).toBeInstanceOf(ApiError)
    expect(err.status).toBe(401)
    expect(err.code).toBe('UNAUTHENTICATED')
    expect(getToken()).toBe('')
  })

  /** 换掉整只 location（happy-dom 的 location.replace 无法直接 spy）。 */
  function stubLocation(pathname: string) {
    const replace = vi.fn()
    vi.stubGlobal('location', { pathname, replace })
    return replace
  }

  it('401 会跳到 /admin/login（base 前缀 + login）', async () => {
    const replace = stubLocation('/admin/repos')
    setToken('stale')
    stubFetch(() => fakeResponse({}, { status: 401 }))
    await http.get('/api/v1/admin/users').catch(() => {})
    expect(replace).toHaveBeenCalledWith('/admin/login')
  })

  it('已经在登录页时不重复跳转（避免刷新循环）', async () => {
    const replace = stubLocation('/admin/login')
    setToken('stale')
    stubFetch(() => fakeResponse({}, { status: 401 }))
    await http.get('/api/v1/auth/me').catch(() => {})
    expect(replace).not.toHaveBeenCalled()
  })

  it('skipAuthRedirect 时只清 token 不跳转（登录页自己处理）', async () => {
    const replace = stubLocation('/admin/repos')
    setToken('stale')
    stubFetch(() => fakeResponse({}, { status: 401 }))
    await http
      .post('/api/v1/auth/login', { username: 'a', password: 'b' }, { skipAuthRedirect: true })
      .catch(() => {})
    expect(replace).not.toHaveBeenCalled()
    expect(getToken()).toBe('')
  })
})

describe('download', () => {
  it('带上 token 与 query，并触发一次 <a download> 点击', async () => {
    setToken('tok')
    const calls = stubFetch(() => fakeResponse('a,b\n', { contentType: 'text/csv' }))
    const clicks: string[] = []
    const origCreate = document.createElement.bind(document)
    vi.spyOn(document, 'createElement').mockImplementation((tag: string) => {
      const el = origCreate(tag)
      if (tag === 'a') {
        ;(el as HTMLAnchorElement).click = () => {
          clicks.push((el as HTMLAnchorElement).download)
        }
      }
      return el
    })
    vi.stubGlobal('URL', {
      ...URL,
      createObjectURL: () => 'blob:fake',
      revokeObjectURL: () => {},
    })

    await http.download('/api/v1/admin/audit', { action: 'user.login', format: 'csv' }, 'audit.csv')

    expect(calls[0]!.headers.get('Authorization')).toBe('Bearer tok')
    expect(queryOf(calls[0]!.url)).toEqual({ action: 'user.login', format: 'csv' })
    expect(clicks).toEqual(['audit.csv'])
  })

  it('下载失败抛 DOWNLOAD_FAILED', async () => {
    stubFetch(() => fakeResponse('nope', { status: 500, statusText: 'Internal Server Error' }))
    const err = await http.download('/x', {}, 'f.csv').catch((e) => e)
    expect(err).toBeInstanceOf(ApiError)
    expect(err.code).toBe('DOWNLOAD_FAILED')
  })
})

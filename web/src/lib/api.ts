import type { ApiResponse, JobRecord } from '../types'

type ParamValue = string | number | boolean
type Params = Record<string, ParamValue | readonly ParamValue[] | undefined>

export class ApiError extends Error {
  constructor(message: string, public status = 0) {
    super(message)
  }
}

let refreshPromise: Promise<boolean> | null = null

export async function refreshSession(): Promise<boolean> {
  if (!refreshPromise) {
    refreshPromise = fetch('/api/token/refresh/', {
      method: 'POST',
      credentials: 'same-origin',
    })
      .then((response) => response.ok)
      .catch(() => false)
      .finally(() => {
        refreshPromise = null
      })
  }
  return refreshPromise
}

export async function request<T>(path: string, init: RequestInit = {}, retry = true): Promise<T> {
  const response = await fetch(path, {
    ...init,
    credentials: 'same-origin',
    headers: {
      ...(init.body instanceof FormData ? {} : { 'Content-Type': 'application/json' }),
      ...init.headers,
    },
  })
  if (response.status === 401 && retry && !path.includes('/api/token/')) {
    if (await refreshSession()) return request<T>(path, init, false)
  }
  const body = await response.json().catch(() => ({}))
  if (!response.ok) throw new ApiError(body.message || body.error || `请求失败 (${response.status})`, response.status)
  return body as T
}

export async function management<T>(path: string, init: RequestInit = {}): Promise<T> {
  const response = await request<ApiResponse<T>>(path, init)
  if (!response.result) throw new ApiError(response.message || '操作失败')
  return response.data
}

export function post<T>(path: string, body: unknown): Promise<T> {
  return management<T>(path, { method: 'POST', body: JSON.stringify(body) })
}

export function get<T>(path: string): Promise<T> {
  return management<T>(path)
}

export function subscribeJobs(
  pageSize: number,
  onJobs: (jobs: JobRecord[]) => void,
  onError?: (message: string) => void,
): () => void {
  let source: EventSource | undefined
  let closed = false
  let refreshing = false

  const connect = () => {
    if (closed) return
    source = new EventSource(`/api/events/jobs/?page_size=${pageSize}`)
    source.addEventListener('jobs', (event) => {
      const data = JSON.parse(event.data) as { items?: JobRecord[] }
      onJobs(data.items || [])
    })
    source.addEventListener('jobs-error', (event) => {
      onError?.(event.data || '任务事件读取失败')
    })
    source.onerror = () => {
      if (closed || refreshing) return
      refreshing = true
      void refreshSession().then((refreshed) => {
        if (!refreshed || closed) return
        source?.close()
        connect()
      }).finally(() => {
        refreshing = false
      })
    }
  }

  connect()
  return () => {
    closed = true
    source?.close()
  }
}

export async function subsonic<T>(method: string, params: Params = {}, retry = true): Promise<T> {
  const search = new URLSearchParams({ f: 'json', v: '1.16.1', c: 'mNest' })
  appendParams(search, params)
  const payload = await request<Record<string, any>>(`/rest/${method}?${search.toString()}`)
  const envelope = payload['subsonic-response']
  if (!envelope || envelope.status !== 'ok') {
    if (retry && envelope?.error?.code === 40 && await refreshSession()) {
      return subsonic<T>(method, params, false)
    }
    throw new ApiError(envelope?.error?.message || 'OpenSubsonic 请求失败')
  }
  return envelope as T
}

export function mediaUrl(method: 'stream' | 'getCoverArt' | 'download', params: Params): string {
  const search = new URLSearchParams({ v: '1.16.1', c: 'mNest' })
  appendParams(search, params)
  return `/rest/${method}?${search.toString()}`
}

function appendParams(search: URLSearchParams, params: Params) {
  Object.entries(params).forEach(([key, value]) => {
    if (Array.isArray(value)) {
      value.forEach((item) => search.append(key, String(item)))
    } else if (value !== undefined) {
      search.set(key, String(value))
    }
  })
}

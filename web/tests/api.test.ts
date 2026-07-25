import { afterEach, describe, expect, it, vi } from 'vitest'
import { request, subsonic } from '../src/lib/api'

afterEach(() => vi.unstubAllGlobals())

describe('API session handling', () => {
  it('refreshes an expired management request once', async () => {
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(new Response('{}', { status: 401 }))
      .mockResolvedValueOnce(new Response('{}', { status: 200 }))
      .mockResolvedValueOnce(new Response(JSON.stringify({ ok: true }), { status: 200 }))
    vi.stubGlobal('fetch', fetchMock)

    await expect(request<{ ok: boolean }>('/user/info/')).resolves.toEqual({ ok: true })
    expect(fetchMock).toHaveBeenCalledTimes(3)
  })

  it('refreshes OpenSubsonic error code 40 and retries', async () => {
    const expired = { 'subsonic-response': { status: 'failed', error: { code: 40, message: 'expired' } } }
    const success = { 'subsonic-response': { status: 'ok', ping: true } }
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(new Response(JSON.stringify(expired), { status: 200 }))
      .mockResolvedValueOnce(new Response('{}', { status: 200 }))
      .mockResolvedValueOnce(new Response(JSON.stringify(success), { status: 200 }))
    vi.stubGlobal('fetch', fetchMock)

    await expect(subsonic<{ ping: boolean }>('ping')).resolves.toMatchObject({ ping: true })
  })

  it('sends repeated OpenSubsonic parameters without flattening them', async () => {
    const fetchMock = vi.fn().mockResolvedValue(new Response(JSON.stringify({
      'subsonic-response': { status: 'ok' },
    }), { status: 200 }))
    vi.stubGlobal('fetch', fetchMock)

    await subsonic('createPlaylist', { name: '夜航', songId: ['track-1', 'track-2'] })

    const url = new URL(String(fetchMock.mock.calls[0][0]), 'http://localhost')
    expect(url.searchParams.getAll('songId')).toEqual(['track-1', 'track-2'])
  })
})

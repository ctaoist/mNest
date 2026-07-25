import { fireEvent, render, screen, waitFor } from '@solidjs/testing-library'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { PlayerBar } from '../src/components/PlayerBar'
import { PlayerProvider, usePlayer } from '../src/context/player'
import type { Track } from '../src/types'

class MockAudio extends EventTarget {
  static latest: MockAudio
  src = ''
  currentTime = 0
  duration = 180
  volume = 1
  paused = true
  preload = ''
  constructor() { super(); MockAudio.latest = this }
  load() {}
  removeAttribute() { this.src = '' }
  play() { this.paused = false; this.dispatchEvent(new Event('play')); return Promise.resolve() }
  pause() { this.paused = true; this.dispatchEvent(new Event('pause')) }
}

const track: Track = { id: 'track-1', title: '夜航', artists: [{ id: 'artist-1', name: '测试艺术家' }], album: '远方', duration: 180 }

function PlayerHarness() {
  const player = usePlayer()
  return <><span>{player.current()?.title || 'empty'}</span><button onClick={() => player.playTracks([track])}>play</button></>
}

describe('PlayerProvider', () => {
  beforeEach(() => {
    vi.stubGlobal('Audio', MockAudio)
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(JSON.stringify({
      'subsonic-response': { status: 'ok', playQueue: { entry: [] } },
    }), { status: 200 })))
  })
  afterEach(() => vi.unstubAllGlobals())

  it('activates a selected track', async () => {
    render(() => <PlayerProvider><PlayerHarness /></PlayerProvider>)
    await fireEvent.click(screen.getByRole('button', { name: 'play' }))
    await waitFor(() => expect(screen.getByText('夜航')).toBeTruthy())
  })

  it('reports now playing and scrobbles after the listening threshold', async () => {
    render(() => <PlayerProvider><PlayerHarness /></PlayerProvider>)
    await fireEvent.click(screen.getByRole('button', { name: 'play' }))
    const fetchMock = vi.mocked(fetch)
    await waitFor(() => expect(fetchMock.mock.calls.some(([input]) => String(input).includes('/rest/scrobble?') && String(input).includes('submission=false'))).toBe(true))

    for (const position of [30, 60, 90]) {
      MockAudio.latest.currentTime = position
      MockAudio.latest.dispatchEvent(new Event('timeupdate'))
    }

    await waitFor(() => expect(fetchMock.mock.calls.some(([input]) => String(input).includes('/rest/scrobble?') && String(input).includes('submission=true'))).toBe(true))
  })

  it('does not count seeking as listened time', async () => {
    render(() => <PlayerProvider><PlayerHarness /></PlayerProvider>)
    await fireEvent.click(screen.getByRole('button', { name: 'play' }))
    await waitFor(() => expect(vi.mocked(fetch).mock.calls.some(([input]) => String(input).includes('submission=false'))).toBe(true))

    MockAudio.latest.currentTime = 100
    MockAudio.latest.dispatchEvent(new Event('seeking'))
    MockAudio.latest.dispatchEvent(new Event('timeupdate'))

    expect(vi.mocked(fetch).mock.calls.some(([input]) => String(input).includes('submission=true'))).toBe(false)
  })

  it('stops playback and removes the player bar when closed', async () => {
    render(() => <PlayerProvider><PlayerHarness /><PlayerBar /></PlayerProvider>)
    await fireEvent.click(screen.getByRole('button', { name: 'play' }))
    await waitFor(() => expect(screen.getByRole('button', { name: '关闭播放栏' })).toBeTruthy())

    await fireEvent.click(screen.getByRole('button', { name: '关闭播放栏' }))

    await waitFor(() => expect(screen.queryByRole('button', { name: '关闭播放栏' })).toBeNull())
    expect(MockAudio.latest.paused).toBe(true)
    expect(screen.getByText('empty')).toBeTruthy()
  })
})

import { fireEvent, render, screen, waitFor } from '@solidjs/testing-library'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { PlayerBar } from '../src/components/PlayerBar'
import { PlayerProvider, usePlayer } from '../src/context/player'
import type { Track } from '../src/types'

const preferenceMocks = vi.hoisted(() => ({ bitrate: 0 }))

vi.mock('../src/context/preferences', () => ({
  usePreferences: () => ({
    webPlaybackBitrate: () => preferenceMocks.bitrate,
    loading: () => false,
    saveWebPlaybackBitrate: vi.fn(),
  }),
}))

class MockAudio extends EventTarget {
  static latest: MockAudio
  src = ''
  currentTime = 0
  duration = 180
  volume = 1
  paused = true
  preload = ''
  error: MediaError | null = null
  loadCalls = 0
  constructor() { super(); MockAudio.latest = this }
  load() { this.loadCalls += 1; this.error = null }
  removeAttribute() { this.src = '' }
  play() { this.paused = false; this.dispatchEvent(new Event('play')); return Promise.resolve() }
  pause() { this.paused = true; this.dispatchEvent(new Event('pause')) }
}

const track: Track = { id: 'track-1', title: '夜航', artists: [{ id: 'artist-1', name: '测试艺术家' }], album: '远方', duration: 180 }
const radio: Track = { id: 'radio:station-1', title: '测试电台', artists: [{ id: 'radio-artist:station-1', name: '网络电台' }], album: '实时广播', duration: 0, streamUrl: '/api/internet_radio_stream.mp3?id=station-1' }

function PlayerHarness() {
  const player = usePlayer()
  return <><span>{player.current()?.title || 'empty'}</span><output aria-label="播放进度">{player.currentTime()}:{player.duration()}</output><button onClick={() => player.playTracks([track])}>play</button><button onClick={() => player.playStream(radio)}>radio</button><button onClick={() => player.seek(60)}>seek</button></>
}

describe('PlayerProvider', () => {
  beforeEach(() => {
    preferenceMocks.bitrate = 0
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

  it('uses the current user web playback bitrate for songs', async () => {
    preferenceMocks.bitrate = 128
    render(() => <PlayerProvider><PlayerHarness /></PlayerProvider>)

    await fireEvent.click(screen.getByRole('button', { name: 'play' }))

    const stream = new URL(MockAudio.latest.src, 'http://localhost')
    expect(stream.searchParams.get('format')).toBe('mp3')
    expect(stream.searchParams.get('maxBitRate')).toBe('128')

    MockAudio.latest.duration = Number.POSITIVE_INFINITY
    MockAudio.latest.dispatchEvent(new Event('durationchange'))
    MockAudio.latest.currentTime = 2
    MockAudio.latest.dispatchEvent(new Event('timeupdate'))
    expect(screen.getByLabelText('播放进度')).toHaveTextContent('2:180')

    await fireEvent.click(screen.getByRole('button', { name: 'seek' }))
    const seekStream = new URL(MockAudio.latest.src, 'http://localhost')
    expect(seekStream.searchParams.get('timeOffset')).toBe('60.000')
    expect(screen.getByLabelText('播放进度')).toHaveTextContent('60:180')
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

  it('opens the current song lyrics from the player bar', async () => {
    vi.mocked(fetch).mockImplementation(async (input) => {
      if (String(input).includes('/rest/getLyricsBySongId?')) {
        return new Response(JSON.stringify({
          'subsonic-response': {
            status: 'ok',
            lyricsList: {
              structuredLyrics: [{
                displayArtist: '测试艺术家',
                displayTitle: '夜航',
                synced: true,
                line: [{ start: 0, value: '第一句歌词' }, { start: 12_000, value: '第二句歌词' }],
              }],
            },
          },
        }), { status: 200 })
      }
      return new Response(JSON.stringify({
        'subsonic-response': { status: 'ok', playQueueByIndex: { entry: [] } },
      }), { status: 200 })
    })

    render(() => <PlayerProvider><PlayerHarness /><PlayerBar /></PlayerProvider>)
    await fireEvent.click(screen.getByRole('button', { name: 'play' }))
    await fireEvent.click(await screen.findByRole('button', { name: '显示歌词' }))

    expect(await screen.findByText('第一句歌词')).toBeTruthy()
    expect(vi.mocked(fetch).mock.calls.some(([input]) => String(input).includes('getLyricsBySongId') && String(input).includes('id=track-1'))).toBe(true)
  })

  it('shows listening identification only while a radio stream is playing', async () => {
    render(() => <PlayerProvider><PlayerHarness /><PlayerBar /></PlayerProvider>)
    await fireEvent.click(screen.getByRole('button', { name: 'play' }))
    expect(screen.queryByRole('button', { name: '听歌识曲' })).toBeNull()

    await fireEvent.click(screen.getByRole('button', { name: 'radio' }))
    expect(await screen.findByRole('button', { name: '听歌识曲' })).toBeTruthy()
    expect(screen.queryByRole('button', { name: '显示歌词' })).toBeNull()
  })

  it('reconnects an interrupted radio stream when the page regains focus', async () => {
    render(() => <PlayerProvider><PlayerHarness /><PlayerBar /></PlayerProvider>)
    await fireEvent.click(screen.getByRole('button', { name: 'radio' }))
    const audio = MockAudio.latest
    expect(audio.loadCalls).toBe(1)

    window.dispatchEvent(new Event('blur'))
    audio.paused = true
    audio.dispatchEvent(new Event('pause'))
    audio.error = { code: 2, message: 'interrupted' } as MediaError
    audio.dispatchEvent(new Event('error'))
    window.dispatchEvent(new Event('focus'))

    await waitFor(() => expect(audio.loadCalls).toBe(2))
    expect(audio.src).toContain('_mnest_reconnect=')
    expect(screen.queryByText(/电台.*中断/)).toBeNull()
    expect(screen.getByRole('button', { name: '暂停' })).toBeTruthy()
  })
})

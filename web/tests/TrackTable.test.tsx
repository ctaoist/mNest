import { fireEvent, render, screen, waitFor } from '@solidjs/testing-library'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { PlayerProvider, usePlayer } from '../src/context/player'
import type { Track } from '../src/types'
import { TrackTable } from '../src/components/TrackTable'

class MockAudio extends EventTarget {
  src = ''
  currentTime = 0
  duration = 180
  volume = 1
  paused = true
  preload = ''

  load() {}
  removeAttribute() { this.src = '' }
  play() {
    this.paused = false
    this.dispatchEvent(new Event('play'))
    return Promise.resolve()
  }
  pause() {
    this.paused = true
    this.dispatchEvent(new Event('pause'))
  }
}

const tracks: Track[] = [
  { id: 'track-1', title: '夜航', artists: [{ id: 'artist-1', name: '测试艺术家' }, { id: 'artist-2', name: '合作歌手' }], album: '远方', albumId: 'album-1', duration: 180, playCount: 12 },
  { id: 'track-2', title: '晨雾', artists: [{ id: 'artist-1', name: '测试艺术家' }], album: '远方', albumId: 'album-1', duration: 210, playCount: 3 },
]

function QueueState() {
  const player = usePlayer()
  return <output aria-label="播放队列状态">{player.queue().length}:{player.current()?.id || 'empty'}</output>
}

describe('TrackTable', () => {
  beforeEach(() => {
    vi.stubGlobal('Audio', MockAudio)
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(JSON.stringify({
      'subsonic-response': { status: 'ok', playQueue: { entry: [] } },
    }), { status: 200 })))
  })

  afterEach(() => vi.unstubAllGlobals())

  it('queues the displayed list and starts from the clicked track', async () => {
    render(() => (
      <PlayerProvider>
        <TrackTable tracks={tracks} />
        <QueueState />
      </PlayerProvider>
    ))

    await fireEvent.click(screen.getByRole('button', { name: '播放 晨雾' }))

    await waitFor(() => expect(screen.getByLabelText('播放队列状态')).toHaveTextContent('2:track-2'))
  })

  it('opens the artist and album from their own columns', async () => {
    const onArtist = vi.fn()
    const onAlbum = vi.fn()
    render(() => (
      <PlayerProvider>
        <TrackTable tracks={[tracks[0]]} onArtist={onArtist} onAlbum={onAlbum} />
      </PlayerProvider>
    ))

    await fireEvent.click(screen.getByRole('button', { name: '查看艺术家 合作歌手' }))
    await fireEvent.click(screen.getByRole('button', { name: '查看专辑 远方' }))

    expect(onArtist).toHaveBeenCalledWith(tracks[0].artists[1], tracks[0])
    expect(onAlbum).toHaveBeenCalledWith(tracks[0])
  })

  it('adds and removes the exact row through playlist actions', async () => {
    const onAddToPlaylist = vi.fn()
    const onRemove = vi.fn()
    const onDelete = vi.fn()
    render(() => (
      <PlayerProvider>
        <TrackTable tracks={tracks} onAddToPlaylist={onAddToPlaylist} onRemove={onRemove} onDelete={onDelete} />
      </PlayerProvider>
    ))

    await fireEvent.click(screen.getByRole('button', { name: '将 晨雾 添加到歌单' }))
    await fireEvent.click(screen.getByRole('button', { name: '从歌单移除 晨雾' }))
    await fireEvent.click(screen.getByRole('button', { name: '永久删除 晨雾' }))

    expect(onAddToPlaylist).toHaveBeenCalledWith(tracks[1])
    expect(onRemove).toHaveBeenCalledWith(tracks[1], 1)
    expect(onDelete).toHaveBeenCalledWith(tracks[1])
  })

  it('renders sortable headers and reports the selected column', async () => {
    const onSort = vi.fn()
    render(() => (
      <PlayerProvider>
        <TrackTable
          tracks={tracks}
          showHeader
          sortKey="title"
          sortDirection="asc"
          onSort={onSort}
        />
      </PlayerProvider>
    ))

    expect(screen.getByRole('button', { name: '按标题降序排序' })).toHaveAttribute('aria-pressed', 'true')
    expect(screen.getByLabelText('夜航 播放次数')).toHaveTextContent('12')
    await fireEvent.click(screen.getByRole('button', { name: '按艺术家升序排序' }))
    expect(onSort).toHaveBeenCalledWith('artist')
    await fireEvent.click(screen.getByRole('button', { name: '按播放次数升序排序' }))
    expect(onSort).toHaveBeenCalledWith('playCount')
  })
})

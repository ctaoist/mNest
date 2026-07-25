import {
  createContext,
  createEffect,
  createMemo,
  createSignal,
  onCleanup,
  onMount,
  ParentProps,
  useContext,
} from 'solid-js'
import { mediaUrl, subsonic } from '../lib/api'
import { trackArtistLabel } from '../lib/utils'
import type { PlayQueue, Track } from '../types'

export type RepeatMode = 'off' | 'all' | 'one'

interface PlayerContextValue {
  current: () => Track | null
  queue: () => Track[]
  index: () => number
  playing: () => boolean
  currentTime: () => number
  duration: () => number
  volume: () => number
  repeat: () => RepeatMode
  shuffled: () => boolean
  queueOpen: () => boolean
  error: () => string
  playTracks: (tracks: Track[], index?: number) => void
  playStream: (track: Track) => void
  playNow: (track: Track) => void
  enqueue: (tracks: Track | Track[]) => void
  toggle: () => void
  next: () => void
  previous: () => void
  seek: (value: number) => void
  setVolume: (value: number) => void
  cycleRepeat: () => void
  toggleShuffle: () => void
  setQueueOpen: (open: boolean) => void
  removeAt: (index: number) => void
  clear: () => void
}

const PlayerContext = createContext<PlayerContextValue>()

export function PlayerProvider(props: ParentProps) {
  const audio = new Audio()
  audio.preload = 'metadata'
  const [queue, setQueue] = createSignal<Track[]>([])
  const [index, setIndex] = createSignal(-1)
  const [playing, setPlaying] = createSignal(false)
  const [currentTime, setCurrentTime] = createSignal(0)
  const [duration, setDuration] = createSignal(0)
  const [volume, setVolumeSignal] = createSignal(Number(localStorage.getItem('player-volume') || 0.8))
  const [repeat, setRepeat] = createSignal<RepeatMode>('off')
  const [shuffled, setShuffled] = createSignal(false)
  const [queueOpen, setQueueOpen] = createSignal(false)
  const [error, setError] = createSignal('')
  const current = createMemo(() => queue()[index()] || null)
  let saveTimer = 0
  let playbackTrackId = ''
  let playbackStartedAt = 0
  let listenedSeconds = 0
  let lastPlaybackPosition = 0
  let nowPlayingSent = false
  let scrobbleSent = false

  const resetPlaybackReport = (track?: Track, position = 0) => {
    playbackTrackId = track && !track.streamUrl ? track.id : ''
    playbackStartedAt = 0
    listenedSeconds = 0
    lastPlaybackPosition = position
    nowPlayingSent = false
    scrobbleSent = false
  }

  const reportNowPlaying = () => {
    const track = current()
    if (!track || track.streamUrl || playbackTrackId !== track.id || nowPlayingSent) return
    nowPlayingSent = true
    if (!playbackStartedAt) playbackStartedAt = Date.now()
    void subsonic('scrobble', { id: track.id, submission: false }).catch(() => undefined)
  }

  const updatePlaybackReport = () => {
    const track = current()
    const position = audio.currentTime
    const delta = position - lastPlaybackPosition
    lastPlaybackPosition = position
    if (!track || track.streamUrl || playbackTrackId !== track.id || !playbackStartedAt || scrobbleSent) return
    if (!audio.paused && delta > 0 && delta <= 30) listenedSeconds += delta
    const trackDuration = track.duration > 0
      ? track.duration
      : Number.isFinite(audio.duration) && audio.duration > 0 ? audio.duration : 0
    if (trackDuration <= 30 || listenedSeconds < Math.min(trackDuration / 2, 240)) return
    scrobbleSent = true
    void subsonic('scrobble', {
      id: track.id,
      time: playbackStartedAt,
      submission: true,
    }).catch(() => undefined)
  }

  const persistQueue = () => {
    window.clearTimeout(saveTimer)
    saveTimer = window.setTimeout(() => {
      const tracks = queue()
      if (!tracks.length || tracks.some((track) => track.streamUrl)) return
      void subsonic('savePlayQueueByIndex', {
        id: tracks.map((track) => track.id).join(','),
        currentIndex: index(),
        position: Math.round(audio.currentTime * 1000),
      })
    }, 600)
  }

  const activate = (nextIndex: number, autoplay = true, position = 0, persist = true) => {
    const track = queue()[nextIndex]
    if (!track) return
    setIndex(nextIndex)
    setError('')
    resetPlaybackReport(track, position)
    audio.src = track.streamUrl || mediaUrl('stream', { id: track.id })
    if (!track.streamUrl) audio.currentTime = position
    audio.load()
    if (autoplay) void audio.play().catch(() => setPlaying(false))
    if (persist) persistQueue()
  }

  const next = () => {
    const tracks = queue()
    if (!tracks.length) return
    if (repeat() === 'one') return activate(index())
    if (shuffled() && tracks.length > 1) {
      let random = index()
      while (random === index()) random = Math.floor(Math.random() * tracks.length)
      return activate(random)
    }
    const nextIndex = index() + 1
    if (nextIndex < tracks.length) activate(nextIndex)
    else if (repeat() === 'all') activate(0)
    else setPlaying(false)
  }

  const previous = () => {
    if (audio.currentTime > 4) return (audio.currentTime = 0)
    const previousIndex = index() - 1
    activate(previousIndex >= 0 ? previousIndex : Math.max(queue().length - 1, 0))
  }

  const playTracks = (tracks: Track[], startIndex = 0) => {
    if (!tracks.length) return
    setQueue([...tracks])
    activate(Math.min(startIndex, tracks.length - 1))
  }

  const playStream = (track: Track) => {
    setQueue([track])
    activate(0, true, 0, false)
  }

  const playNow = (track: Track) => playTracks([track, ...queue().filter((item) => item.id !== track.id)])
  const enqueue = (tracks: Track | Track[]) => {
    const additions = Array.isArray(tracks) ? tracks : [tracks]
    setQueue((items) => [...items, ...additions.filter((track) => !items.some((item) => item.id === track.id))])
    persistQueue()
  }

  const toggle = () => {
    if (!current()) return
    if (audio.paused) void audio.play()
    else audio.pause()
  }

  const seek = (value: number) => {
    audio.currentTime = Math.max(0, Math.min(value, duration()))
    setCurrentTime(audio.currentTime)
  }

  const setVolume = (value: number) => {
    const nextVolume = Math.max(0, Math.min(1, value))
    audio.volume = nextVolume
    setVolumeSignal(nextVolume)
    localStorage.setItem('player-volume', String(nextVolume))
  }

  const cycleRepeat = () => setRepeat((mode) => (mode === 'off' ? 'all' : mode === 'all' ? 'one' : 'off'))
  const removeAt = (removeIndex: number) => {
    const wasCurrent = removeIndex === index()
    setQueue((items) => items.filter((_, itemIndex) => itemIndex !== removeIndex))
    if (removeIndex < index()) setIndex((value) => value - 1)
    else if (wasCurrent) {
      if (queue().length) activate(Math.min(removeIndex, queue().length - 1), playing())
      else {
        audio.pause()
        audio.removeAttribute('src')
        setIndex(-1)
        resetPlaybackReport()
      }
    }
    persistQueue()
  }

  const clear = () => {
    audio.pause()
    audio.removeAttribute('src')
    setQueue([])
    setIndex(-1)
    setPlaying(false)
    setCurrentTime(0)
    setDuration(0)
    setError('')
    setQueueOpen(false)
    resetPlaybackReport()
    if ('mediaSession' in navigator) navigator.mediaSession.metadata = null
    void subsonic('savePlayQueueByIndex', { id: '' }).catch(() => undefined)
  }

  onMount(async () => {
    audio.volume = volume()
    audio.addEventListener('play', () => {
      setPlaying(true)
      reportNowPlaying()
    })
    audio.addEventListener('pause', () => setPlaying(false))
    audio.addEventListener('timeupdate', () => {
      setCurrentTime(audio.currentTime)
      updatePlaybackReport()
    })
    audio.addEventListener('seeking', () => {
      lastPlaybackPosition = audio.currentTime
    })
    audio.addEventListener('durationchange', () => setDuration(Number.isFinite(audio.duration) ? audio.duration : 0))
    audio.addEventListener('ended', next)
    audio.addEventListener('error', () => {
      const track = current()
      setError(track?.streamUrl?.startsWith('/api/remote_download/preview/')
        ? '128k 试听加载失败，远程来源没有返回可播放音频。'
        : track?.id.startsWith('radio:')
          ? '电台播放失败，请检查流地址、服务端网络或音频格式。'
          : '音频加载失败，请检查文件或转码工具。')
    })
    try {
      const response = await subsonic<{ playQueueByIndex: PlayQueue }>('getPlayQueueByIndex')
      const saved = response.playQueueByIndex
      if (saved?.entry?.length) {
        setQueue(saved.entry)
        const savedIndex = Math.max(0, Math.min(saved.currentIndex || 0, saved.entry.length - 1))
        activate(savedIndex, false, (saved.position || 0) / 1000)
      }
    } catch {
      // Empty or unavailable queues should not block the application shell.
    }
  })

  createEffect(() => {
    const track = current()
    if (!track || !('mediaSession' in navigator)) return
    navigator.mediaSession.metadata = new MediaMetadata({
      title: track.title,
      artist: trackArtistLabel(track),
      album: track.album,
      artwork: track.coverArt ? [{ src: mediaUrl('getCoverArt', { id: track.coverArt }) }] : [],
    })
    navigator.mediaSession.setActionHandler('play', () => void audio.play())
    navigator.mediaSession.setActionHandler('pause', () => audio.pause())
    navigator.mediaSession.setActionHandler('previoustrack', previous)
    navigator.mediaSession.setActionHandler('nexttrack', next)
  })

  onCleanup(() => {
    window.clearTimeout(saveTimer)
    audio.pause()
    audio.src = ''
  })

  return (
    <PlayerContext.Provider value={{
      current, queue, index, playing, currentTime, duration, volume, repeat, shuffled, queueOpen, error,
      playTracks, playStream, playNow, enqueue, toggle, next, previous, seek, setVolume, cycleRepeat,
      toggleShuffle: () => setShuffled((value) => !value), setQueueOpen, removeAt, clear,
    }}>
      {props.children}
    </PlayerContext.Provider>
  )
}

export function usePlayer() {
  const context = useContext(PlayerContext)
  if (!context) throw new Error('PlayerProvider is missing')
  return context
}

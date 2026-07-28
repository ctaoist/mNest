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
import { usePreferences } from './preferences'
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
  captureRadioSamples: (durationSeconds?: number) => Promise<Float32Array>
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

const RADIO_SAMPLE_RATE = 8_000
const RADIO_RECORDER_WORKLET = `
class MNestRadioRecorder extends AudioWorkletProcessor {
  constructor() {
    super()
    this.buffer = new Float32Array(0)
    this.offset = 0
    this.recording = false
    this.port.onmessage = (event) => {
      if (event.data?.type === 'start') {
        this.buffer = new Float32Array(event.data.samples)
        this.offset = 0
        this.recording = true
      } else if (event.data?.type === 'cancel') {
        this.recording = false
        this.buffer = new Float32Array(0)
        this.offset = 0
      }
    }
  }
  process(inputs) {
    if (!this.recording) return true
    const channel = inputs[0]?.[0]
    if (!channel?.length) return true
    const available = Math.min(channel.length, this.buffer.length - this.offset)
    this.buffer.set(channel.subarray(0, available), this.offset)
    this.offset += available
    if (this.offset >= this.buffer.length) {
      const samples = this.buffer
      this.recording = false
      this.buffer = new Float32Array(0)
      this.offset = 0
      this.port.postMessage({ type: 'finished', samples }, [samples.buffer])
    }
    return true
  }
}
registerProcessor('mnest-radio-recorder', MNestRadioRecorder)
`

export function PlayerProvider(props: ParentProps) {
  const preferences = usePreferences()
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
  let radioCaptureContext: AudioContext | undefined
  let radioCaptureSource: MediaElementAudioSourceNode | undefined
  let radioCaptureRecorder: AudioWorkletNode | undefined
  let radioCaptureReject: ((reason?: unknown) => void) | undefined
  let radioCaptureTimer = 0
  let radioReconnectSequence = 0
  let radioReconnectInFlight = false
  let resumeRadioAfterInterruption = false
  let playbackOffset = 0
  let transcodedPlayback = false

  const cancelRadioCapture = (message = '听歌识曲已取消') => {
    window.clearTimeout(radioCaptureTimer)
    radioCaptureRecorder?.port.postMessage({ type: 'cancel' })
    radioCaptureReject?.(new Error(message))
    radioCaptureReject = undefined
  }

  const ensureRadioRecorder = async () => {
    if (!radioCaptureContext || !radioCaptureSource) {
      const context = new AudioContext()
      const source = context.createMediaElementSource(audio)
      source.connect(context.destination)
      radioCaptureContext = context
      radioCaptureSource = source
    }
    const context = radioCaptureContext
    const source = radioCaptureSource
    if (radioCaptureRecorder) {
      await context.resume()
      return { context, recorder: radioCaptureRecorder }
    }
    const workletUrl = URL.createObjectURL(new Blob([RADIO_RECORDER_WORKLET], { type: 'text/javascript' }))
    try {
      await context.audioWorklet.addModule(workletUrl)
    } finally {
      URL.revokeObjectURL(workletUrl)
    }
    const recorder = new AudioWorkletNode(context, 'mnest-radio-recorder')
    const silentOutput = context.createGain()
    silentOutput.gain.value = 0
    source.connect(recorder)
    recorder.connect(silentOutput)
    silentOutput.connect(context.destination)
    radioCaptureRecorder = recorder
    await context.resume()
    return { context, recorder }
  }

  const captureRadioSamples = async (durationSeconds = 3) => {
    const track = current()
    if (!track?.id.startsWith('radio:')) throw new Error('请先播放网络电台')
    if (audio.paused) throw new Error('请先开始播放电台')
    if (radioCaptureReject) throw new Error('正在识别当前电台')
    const { context, recorder } = await ensureRadioRecorder()
    const sampleCount = Math.ceil(durationSeconds * context.sampleRate)
    const captured = await new Promise<Float32Array>((resolve, reject) => {
      radioCaptureReject = reject
      radioCaptureTimer = window.setTimeout(() => {
        recorder.port.postMessage({ type: 'cancel' })
        radioCaptureReject = undefined
        reject(new Error('采集电台音频超时'))
      }, durationSeconds * 1_000 + 5_000)
      recorder.port.onmessage = (event: MessageEvent<{ type?: string; samples?: Float32Array }>) => {
        if (event.data?.type !== 'finished' || !event.data.samples) return
        window.clearTimeout(radioCaptureTimer)
        radioCaptureReject = undefined
        resolve(event.data.samples)
      }
      recorder.port.postMessage({ type: 'start', samples: sampleCount })
    })
    return resampleRadioSamples(captured, context.sampleRate, RADIO_SAMPLE_RATE)
  }

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
    const position = playbackOffset + audio.currentTime
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
        id: tracks.map((track) => track.id),
        currentIndex: index(),
        position: Math.round((playbackOffset + audio.currentTime) * 1000),
      }).catch(() => undefined)
    }, 600)
  }

  const loadTrackSource = (track: Track, position: number) => {
    const playbackBitrate = track.streamUrl ? 0 : preferences.webPlaybackBitrate()
    transcodedPlayback = playbackBitrate > 0
    playbackOffset = transcodedPlayback ? position : 0
    audio.src = track.streamUrl || mediaUrl('stream', {
      id: track.id,
      format: playbackBitrate ? 'mp3' : undefined,
      maxBitRate: playbackBitrate || undefined,
      timeOffset: transcodedPlayback && position > 0 ? position.toFixed(3) : undefined,
    })
    if (transcodedPlayback) audio.currentTime = 0
    else if (!track.streamUrl) audio.currentTime = position
    setCurrentTime(position)
    setDuration(track.duration > 0 ? track.duration : 0)
    audio.load()
  }

  const activate = (nextIndex: number, autoplay = true, position = 0, persist = true) => {
    const track = queue()[nextIndex]
    if (!track) return
    cancelRadioCapture('播放内容已切换')
    setIndex(nextIndex)
    setError('')
    resetPlaybackReport(track, position)
    loadTrackSource(track, position)
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

  const reconnectRadio = async (automatic: boolean) => {
    const track = current()
    if (!track?.id.startsWith('radio:') || !track.streamUrl || radioReconnectInFlight) return
    radioReconnectInFlight = true
    cancelRadioCapture('电台连接正在恢复')
    setError('')
    try {
      const url = new URL(track.streamUrl, window.location.href)
      url.searchParams.set('_mnest_reconnect', `${Date.now()}-${++radioReconnectSequence}`)
      audio.pause()
      audio.src = url.origin === window.location.origin
        ? `${url.pathname}${url.search}${url.hash}`
        : url.toString()
      audio.load()
      await audio.play()
    } catch {
      setPlaying(false)
      setError(automatic
        ? '电台播放被系统中断，请点击播放重新连接。'
        : '电台重新连接失败，请稍后重试。')
    } finally {
      radioReconnectInFlight = false
    }
  }

  const resumePlayback = () => {
    const track = current()
    if (!track) return
    if (track.id.startsWith('radio:')) {
      void reconnectRadio(false)
      return
    }
    void audio.play().catch(() => setPlaying(false))
  }

  const pausePlayback = () => {
    resumeRadioAfterInterruption = false
    audio.pause()
  }

  const toggle = () => {
    if (!current()) return
    if (audio.paused) resumePlayback()
    else pausePlayback()
  }

  const seek = (value: number) => {
    const position = Math.max(0, Math.min(value, duration()))
    const track = current()
    if (track && transcodedPlayback) {
      const autoplay = !audio.paused
      cancelRadioCapture('播放位置已改变')
      setError('')
      lastPlaybackPosition = position
      loadTrackSource(track, position)
      if (autoplay) void audio.play().catch(() => setPlaying(false))
      persistQueue()
      return
    }
    audio.currentTime = position
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
    cancelRadioCapture()
    resumeRadioAfterInterruption = false
    playbackOffset = 0
    transcodedPlayback = false
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
    const rememberRadioInterruption = () => {
      const track = current()
      if (track?.id.startsWith('radio:') && !audio.paused) {
        resumeRadioAfterInterruption = true
      }
    }
    const restoreRadioAfterInterruption = () => {
      if (document.visibilityState === 'hidden' || !resumeRadioAfterInterruption) return
      resumeRadioAfterInterruption = false
      const track = current()
      if (track?.id.startsWith('radio:')) {
        void reconnectRadio(true)
      }
    }
    const handleVisibilityChange = () => {
      if (document.visibilityState === 'hidden') rememberRadioInterruption()
      else restoreRadioAfterInterruption()
    }

    audio.volume = volume()
    audio.addEventListener('play', () => {
      resumeRadioAfterInterruption = false
      setError('')
      setPlaying(true)
      reportNowPlaying()
    })
    audio.addEventListener('pause', () => {
      setPlaying(false)
      if (radioCaptureReject) cancelRadioCapture('电台播放已暂停')
    })
    audio.addEventListener('timeupdate', () => {
      setCurrentTime(playbackOffset + audio.currentTime)
      updatePlaybackReport()
    })
    audio.addEventListener('seeking', () => {
      lastPlaybackPosition = playbackOffset + audio.currentTime
    })
    audio.addEventListener('durationchange', () => {
      const mediaDuration = Number.isFinite(audio.duration) && audio.duration > 0 ? audio.duration : 0
      const trackDuration = current()?.duration || 0
      setDuration(transcodedPlayback ? trackDuration : mediaDuration || trackDuration)
    })
    audio.addEventListener('ended', next)
    audio.addEventListener('error', () => {
      const track = current()
      if (track?.id.startsWith('radio:')) {
        setPlaying(false)
        if (!resumeRadioAfterInterruption) {
          setError('电台连接已中断，请点击播放重新连接。')
        }
        return
      }
      setError(track?.streamUrl?.startsWith('/api/remote_download/preview/')
        ? '128k 试听加载失败，远程来源没有返回可播放音频。'
        : '音频加载失败，请检查文件或转码工具。')
    })
    window.addEventListener('blur', rememberRadioInterruption)
    window.addEventListener('focus', restoreRadioAfterInterruption)
    window.addEventListener('pagehide', rememberRadioInterruption)
    window.addEventListener('pageshow', restoreRadioAfterInterruption)
    document.addEventListener('visibilitychange', handleVisibilityChange)
    onCleanup(() => {
      window.removeEventListener('blur', rememberRadioInterruption)
      window.removeEventListener('focus', restoreRadioAfterInterruption)
      window.removeEventListener('pagehide', rememberRadioInterruption)
      window.removeEventListener('pageshow', restoreRadioAfterInterruption)
      document.removeEventListener('visibilitychange', handleVisibilityChange)
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
    navigator.mediaSession.setActionHandler('play', resumePlayback)
    navigator.mediaSession.setActionHandler('pause', pausePlayback)
    navigator.mediaSession.setActionHandler('previoustrack', previous)
    navigator.mediaSession.setActionHandler('nexttrack', next)
  })

  onCleanup(() => {
    window.clearTimeout(saveTimer)
    cancelRadioCapture()
    audio.pause()
    audio.src = ''
    radioCaptureSource?.disconnect()
    radioCaptureRecorder?.disconnect()
    void radioCaptureContext?.close()
  })

  return (
    <PlayerContext.Provider value={{
      current, queue, index, playing, currentTime, duration, volume, repeat, shuffled, queueOpen, error,
      captureRadioSamples,
      playTracks, playStream, playNow, enqueue, toggle, next, previous, seek, setVolume, cycleRepeat,
      toggleShuffle: () => setShuffled((value) => !value), setQueueOpen, removeAt, clear,
    }}>
      {props.children}
    </PlayerContext.Provider>
  )
}

function resampleRadioSamples(samples: Float32Array, sourceRate: number, targetRate: number) {
  if (sourceRate === targetRate) return samples
  const outputLength = Math.max(1, Math.round(samples.length * targetRate / sourceRate))
  const output = new Float32Array(outputLength)
  const ratio = sourceRate / targetRate
  for (let index = 0; index < outputLength; index += 1) {
    const position = index * ratio
    const left = Math.floor(position)
    const right = Math.min(left + 1, samples.length - 1)
    const fraction = position - left
    output[index] = samples[left] * (1 - fraction) + samples[right] * fraction
  }
  return output
}

export function usePlayer() {
  const context = useContext(PlayerContext)
  if (!context) throw new Error('PlayerProvider is missing')
  return context
}

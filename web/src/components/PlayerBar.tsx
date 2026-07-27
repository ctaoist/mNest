import { createEffect, createMemo, createSignal, For, Show } from 'solid-js'
import {
  AudioLines,
  ChevronDown,
  ExternalLink,
  ListMusic,
  LoaderCircle,
  MicVocal,
  Pause,
  Play,
  Repeat,
  Repeat1,
  RefreshCw,
  Shuffle,
  SkipBack,
  SkipForward,
  Trash2,
  Volume1,
  X,
} from 'lucide-solid'
import { usePlayer } from '../context/player'
import { subsonic } from '../lib/api'
import { identifyNeteaseAudio, type NeteaseAudioMatchResult } from '../lib/neteaseAudioMatch'
import { formatDuration, trackArtistLabel } from '../lib/utils'
import { CoverArt } from './CoverArt'

interface LyricsLine {
  start?: number
  value: string
}

interface StructuredLyrics {
  displayArtist?: string
  displayTitle?: string
  synced: boolean
  line: LyricsLine[]
}

export function PlayerBar() {
  const player = usePlayer()
  const [lyricsOpen, setLyricsOpen] = createSignal(false)
  const [lyricsLoading, setLyricsLoading] = createSignal(false)
  const [lyricsError, setLyricsError] = createSignal('')
  const [lyrics, setLyrics] = createSignal<StructuredLyrics | null>(null)
  const [matchOpen, setMatchOpen] = createSignal(false)
  const [matchLoading, setMatchLoading] = createSignal(false)
  const [matchStage, setMatchStage] = createSignal('')
  const [matchError, setMatchError] = createSignal('')
  const [matches, setMatches] = createSignal<NeteaseAudioMatchResult[]>([])
  let lyricsRequest = 0
  let lyricsList: HTMLDivElement | undefined

  const activeLyricsLine = createMemo(() => {
    const value = lyrics()
    if (!value?.synced) return -1
    const position = player.currentTime() * 1000
    let active = -1
    value.line.forEach((line, index) => {
      if (line.start !== undefined && line.start <= position) active = index
    })
    return active
  })

  const loadLyrics = async (trackId: string) => {
    const requestId = ++lyricsRequest
    setLyricsLoading(true)
    setLyricsError('')
    setLyrics(null)
    try {
      const response = await subsonic<{ lyricsList?: { structuredLyrics?: StructuredLyrics[] } }>('getLyricsBySongId', { id: trackId })
      if (requestId !== lyricsRequest) return
      setLyrics(response.lyricsList?.structuredLyrics?.[0] || null)
    } catch (error) {
      if (requestId !== lyricsRequest) return
      setLyricsError(error instanceof Error ? error.message : '歌词加载失败')
    } finally {
      if (requestId === lyricsRequest) setLyricsLoading(false)
    }
  }

  const toggleLyrics = () => {
    if (lyricsOpen()) {
      setLyricsOpen(false)
      return
    }
    player.setQueueOpen(false)
    setMatchOpen(false)
    setLyricsOpen(true)
  }

  const openQueue = () => {
    setLyricsOpen(false)
    setMatchOpen(false)
    player.setQueueOpen(true)
  }

  const identifyRadio = async () => {
    if (matchLoading()) return
    player.setQueueOpen(false)
    setLyricsOpen(false)
    setMatchOpen(true)
    setMatchLoading(true)
    setMatchStage('正在采集3秒电台音频')
    setMatchError('')
    setMatches([])
    try {
      const samples = await player.captureRadioSamples(3)
      setMatchStage('正在生成指纹并查询网易云')
      setMatches(await identifyNeteaseAudio(samples))
    } catch (error) {
      setMatchError(error instanceof Error ? error.message : '听歌识曲失败')
    } finally {
      setMatchLoading(false)
      setMatchStage('')
    }
  }

  createEffect(() => {
    const track = player.current()
    if (!track) {
      lyricsRequest += 1
      setLyricsOpen(false)
      setLyrics(null)
      setMatchOpen(false)
      setMatches([])
      setMatchError('')
      return
    }
    if (!track.id.startsWith('radio:')) {
      setMatchOpen(false)
      setMatches([])
      setMatchError('')
    }
    if (lyricsOpen() && !track.id.startsWith('radio:')) void loadLyrics(track.id)
  })

  createEffect(() => {
    if (!lyricsOpen() || !lyricsList) return
    const active = activeLyricsLine()
    if (active < 0) return
    requestAnimationFrame(() => {
      lyricsList?.querySelector<HTMLElement>(`[data-lyrics-line="${active}"]`)?.scrollIntoView?.({ block: 'center', behavior: 'smooth' })
    })
  })

  return (
    <>
      <Show when={player.current()}>
        {(track) => (
          <div class="player-bar">
            <button class="player-track" onClick={openQueue}>
              <CoverArt id={track().coverArt} alt={track().album} kind="track" />
              <span><strong>{track().title}</strong><small class={player.error() ? 'player-track-error' : ''}>{player.error() || trackArtistLabel(track())}</small></span>
            </button>
            <div class="player-center">
              <div class="player-controls">
                <button class={`icon-button ${player.shuffled() ? 'is-active' : ''}`} onClick={player.toggleShuffle} aria-label="随机播放"><Shuffle size={16} /></button>
                <button class="icon-button" onClick={player.previous} aria-label="上一首"><SkipBack size={19} fill="currentColor" /></button>
                <button class="play-button" onClick={player.toggle} aria-label={player.playing() ? '暂停' : '播放'}>
                  <Show when={player.playing()} fallback={<Play size={19} fill="currentColor" />}><Pause size={19} fill="currentColor" /></Show>
                </button>
                <button class="icon-button" onClick={player.next} aria-label="下一首"><SkipForward size={19} fill="currentColor" /></button>
                <button class={`icon-button ${player.repeat() !== 'off' ? 'is-active' : ''}`} onClick={player.cycleRepeat} aria-label="循环模式">
                  {player.repeat() === 'one' ? <Repeat1 size={16} /> : <Repeat size={16} />}
                </button>
                <Show when={!track().id.startsWith('radio:')}>
                  <button class={`icon-button player-lyrics-trigger ${lyricsOpen() ? 'is-active' : ''}`} onClick={toggleLyrics} aria-label={lyricsOpen() ? '关闭歌词' : '显示歌词'} title="歌词"><MicVocal size={17} /></button>
                </Show>
                <Show when={track().id.startsWith('radio:')}>
                  <button
                    class={`icon-button player-radio-match ${matchOpen() ? 'is-active' : ''} ${matchLoading() ? 'is-listening' : ''}`}
                    disabled={!player.playing() || matchLoading()}
                    onClick={() => void identifyRadio()}
                    aria-label="听歌识曲"
                    title={player.playing() ? '听歌识曲' : '请先播放电台'}
                  >
                    <Show when={!matchLoading()} fallback={<LoaderCircle class="spin" size={17} />}><AudioLines size={17} /></Show>
                  </button>
                </Show>
              </div>
              <div class="progress-line">
                <span>{track().id.startsWith('radio:') ? 'LIVE' : formatDuration(player.currentTime())}</span>
                <input type="range" min="0" max={player.duration() || 1} value={track().id.startsWith('radio:') ? 1 : player.currentTime()} disabled={track().id.startsWith('radio:')} onInput={(event) => player.seek(Number(event.currentTarget.value))} aria-label={track().id.startsWith('radio:') ? '电台直播流' : '播放进度'} />
                <span>{track().id.startsWith('radio:') ? '实时' : formatDuration(player.duration())}</span>
              </div>
            </div>
            <div class="player-tools">
              <Volume1 size={17} />
              <input class="volume" type="range" min="0" max="1" step="0.02" value={player.volume()} onInput={(event) => player.setVolume(Number(event.currentTarget.value))} aria-label="音量" />
              <button class="queue-button" onClick={openQueue}><ListMusic size={18} /><span>{player.queue().length}</span></button>
            </div>
            <button class="icon-button player-close" onClick={player.clear} aria-label="关闭播放栏" title="关闭播放栏"><X size={18} /></button>
          </div>
        )}
      </Show>

      <div class={`queue-sheet lyrics-sheet ${lyricsOpen() ? 'is-open' : ''}`} aria-hidden={!lyricsOpen()}>
        <div class="sheet-backdrop" onClick={() => setLyricsOpen(false)} />
        <aside>
          <header class="lyrics-header">
            <div><span class="eyebrow">NOW SINGING</span><div class="lyrics-heading">歌词</div></div>
            <button class="icon-button" onClick={() => setLyricsOpen(false)} aria-label="关闭歌词"><X /></button>
          </header>
          <Show when={player.current()}>
            {(track) => <div class="lyrics-track"><strong>{track().title}</strong><span>{trackArtistLabel(track())}</span></div>}
          </Show>
          <div class="lyrics-lines" ref={lyricsList} classList={{ 'is-synced': !!lyrics()?.synced }}>
            <Show when={!lyricsLoading()} fallback={<div class="lyrics-state"><span class="lyrics-pulse" /><span>正在读取歌词</span></div>}>
              <Show when={!lyricsError()} fallback={<div class="lyrics-state is-error">{lyricsError()}</div>}>
                <For each={lyrics()?.line || []} fallback={<div class="lyrics-state">这首歌暂时没有歌词</div>}>
                  {(line, index) => (
                    <Show when={lyrics()?.synced && line.start !== undefined} fallback={<p class="lyrics-line">{line.value || '\u00a0'}</p>}>
                      <button
                        class="lyrics-line"
                        classList={{ 'is-active': activeLyricsLine() === index() }}
                        data-lyrics-line={index()}
                        onClick={() => player.seek((line.start || 0) / 1000)}
                      >{line.value || '\u00a0'}</button>
                    </Show>
                  )}
                </For>
              </Show>
            </Show>
          </div>
        </aside>
      </div>

      <div class={`queue-sheet radio-match-sheet ${matchOpen() ? 'is-open' : ''}`} aria-hidden={!matchOpen()}>
        <div class="sheet-backdrop" onClick={() => setMatchOpen(false)} />
        <aside>
          <header class="radio-match-header">
            <div><span class="eyebrow">LISTENING ID</span><div class="radio-match-heading">听歌识曲</div></div>
            <button class="icon-button" onClick={() => setMatchOpen(false)} aria-label="关闭听歌识曲"><X /></button>
          </header>
          <Show when={player.current()}>{(track) => <div class="radio-match-station"><span class="radio-match-live" /> <strong>{track().title}</strong><small>网易云音乐识别</small></div>}</Show>
          <Show when={!matchLoading()} fallback={
            <div class="radio-match-scanning">
              <div class="radio-match-wave" aria-hidden="true"><i /><i /><i /><i /><i /><i /><i /></div>
              <strong>{matchStage()}</strong>
              <span>保持电台继续播放，识别过程不会中断音频。</span>
            </div>
          }>
            <Show when={!matchError()} fallback={<div class="radio-match-state is-error"><AudioLines /><strong>识别未完成</strong><span>{matchError()}</span><button class="secondary-button" onClick={() => void identifyRadio()}><RefreshCw size={15} />重新识别</button></div>}>
              <div class="radio-match-results">
                <For each={matches()} fallback={<div class="radio-match-state"><AudioLines /><strong>暂未识别到歌曲</strong><span>可以等待歌曲进入副歌或人声段落后重试。</span><button class="secondary-button" onClick={() => void identifyRadio()}><RefreshCw size={15} />重新识别</button></div>}>
                  {(result, index) => (
                    <a class="radio-match-result" href={`https://music.163.com/#/song?id=${encodeURIComponent(result.id)}`} target="_blank" rel="noreferrer">
                      <span class="radio-match-index">{String(index() + 1).padStart(2, '0')}</span>
                      <span><strong>{result.title}</strong><small>{result.artists.join('; ') || '未知艺术家'} · {result.album || '未知专辑'}</small></span>
                      <Show when={result.start_time_ms > 0}><em>{(result.start_time_ms / 1000).toFixed(1)}s</em></Show>
                      <ExternalLink size={16} />
                    </a>
                  )}
                </For>
              </div>
            </Show>
          </Show>
        </aside>
      </div>

      <div class={`queue-sheet ${player.queueOpen() ? 'is-open' : ''}`} aria-hidden={!player.queueOpen()}>
        <div class="sheet-backdrop" onClick={() => player.setQueueOpen(false)} />
        <aside>
          <header>
            <div><span class="eyebrow">PLAY NEXT</span><h2>播放队列</h2></div>
            <button class="icon-button" onClick={() => player.setQueueOpen(false)} aria-label="关闭"><X /></button>
          </header>
          <div class="queue-toolbar">
            <span>{player.queue().length} 首歌曲</span>
            <button class="text-button danger" onClick={player.clear}><Trash2 size={15} />清空</button>
          </div>
          <div class="queue-list">
            <For each={player.queue()} fallback={<div class="empty-state">队列空空如也</div>}>
              {(track, index) => (
                <div class={`queue-row ${player.index() === index() ? 'is-active' : ''}`}>
                  <button class="queue-main" onClick={() => player.playTracks(player.queue(), index())}>
                    <CoverArt id={track.coverArt} alt={track.album} />
                    <span><strong>{track.title}</strong><small>{trackArtistLabel(track)}</small></span>
                  </button>
                  <span>{formatDuration(track.duration)}</span>
                  <button class="icon-button" onClick={() => player.removeAt(index())} aria-label="从队列移除"><ChevronDown class="rotate-90" size={16} /></button>
                </div>
              )}
            </For>
          </div>
        </aside>
      </div>
    </>
  )
}

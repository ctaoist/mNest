import { For, Show } from 'solid-js'
import { ArrowDown, ArrowUp, ChevronsUpDown, Heart, ListMusic, ListPlus, ListX, Pause, Play, Trash2 } from 'lucide-solid'
import { formatDuration } from '../lib/utils'
import type { TrackSortDirection, TrackSortKey } from '../lib/trackSorting'
import type { Track, TrackArtist } from '../types'
import { usePlayer } from '../context/player'

interface TrackTableProps {
  tracks: Track[]
  onFavorite?: (track: Track) => void
  onArtist?: (artist: TrackArtist, track: Track) => void
  onAlbum?: (track: Track) => void
  onAddToPlaylist?: (track: Track) => void
  onRemove?: (track: Track, index: number) => void
  onDelete?: (track: Track) => void
  actionsDisabled?: boolean
  deleteDisabled?: boolean
  compact?: boolean
  showHeader?: boolean
  sortKey?: TrackSortKey
  sortDirection?: TrackSortDirection
  onSort?: (key: TrackSortKey) => void
}

export function TrackTable(props: TrackTableProps) {
  const player = usePlayer()
  return (
    <div class="track-table" classList={{ 'has-remove-action': !!props.onRemove, 'has-delete-action': !!props.onDelete }}>
      <Show when={props.showHeader}>
        <div class="track-table-header">
          <span aria-hidden="true" />
          <span class="track-header-index">#</span>
          <TrackHeaderButton label="标题" sortKey="title" activeKey={props.sortKey} direction={props.sortDirection} onSort={props.onSort} />
          <TrackHeaderButton class="track-header-artist" label="艺术家" sortKey="artist" activeKey={props.sortKey} direction={props.sortDirection} onSort={props.onSort} />
          <TrackHeaderButton class="track-header-album" label="专辑" sortKey="album" activeKey={props.sortKey} direction={props.sortDirection} onSort={props.onSort} />
          <TrackHeaderButton class="track-header-kind" label="类型" sortKey="kind" activeKey={props.sortKey} direction={props.sortDirection} onSort={props.onSort} />
          <TrackHeaderButton class="track-header-play-count" label="播放次数" sortKey="playCount" activeKey={props.sortKey} direction={props.sortDirection} onSort={props.onSort} />
          <TrackHeaderButton class="track-header-duration" label="时长" sortKey="duration" activeKey={props.sortKey} direction={props.sortDirection} onSort={props.onSort} />
          <span class="track-header-actions">操作</span>
        </div>
      </Show>
      <For each={props.tracks} fallback={<div class="empty-state small">这里还没有歌曲</div>}>
        {(track, index) => {
          const active = () => player.current()?.id === track.id
          return (
            <div class={`track-row ${active() ? 'is-active' : ''}`}>
              <button
                class="track-play"
                aria-label={`播放 ${track.title}`}
                onClick={() => active() ? player.toggle() : player.playTracks(props.tracks, index())}
              >
                <Show when={active() && player.playing()} fallback={<Play size={15} fill="currentColor" />}>
                  <Pause size={15} fill="currentColor" />
                </Show>
              </button>
              <span class="track-index">{String(index() + 1).padStart(2, '0')}</span>
              <div class="track-primary">
                <strong>{track.title}</strong>
              </div>
              <span class="track-artists" aria-label={track.artists.map((artist) => artist.name).join(', ')}>
                <For each={track.artists} fallback={<span class="track-artist-empty">Unknown Artist</span>}>
                  {(artist, artistIndex) => (
                    <><button class="track-link track-artist" disabled={!props.onArtist} onClick={() => props.onArtist?.(artist, track)} aria-label={`查看艺术家 ${artist.name}`}>{artist.name}</button><Show when={artistIndex() < track.artists.length - 1}><span class="track-artist-separator">, </span></Show></>
                  )}
                </For>
              </span>
              <button class="track-link track-album" disabled={!props.onAlbum || !track.albumId} onClick={() => props.onAlbum?.(track)} aria-label={`查看专辑 ${track.album || '未知专辑'}`}>{track.album || '未知专辑'}</button>
              <span class="track-meta">{track.genre || track.suffix?.toUpperCase() || '音乐'}</span>
              <span class="track-play-count" aria-label={`${track.title} 播放次数`}>{(track.playCount ?? 0).toLocaleString('zh-CN')}</span>
              <span class="track-duration">{formatDuration(track.duration)}</span>
              <div class="track-actions">
                <Show when={props.onFavorite}>
                  <button class={`icon-button ${track.starred ? 'is-favorite' : ''}`} onClick={() => props.onFavorite?.(track)} aria-label="收藏">
                    <Heart size={16} fill={track.starred ? 'currentColor' : 'none'} />
                  </button>
                </Show>
                <button class="icon-button track-queue-action" onClick={() => player.enqueue(track)} aria-label="加入队列"><ListPlus size={17} /></button>
                <Show when={props.onAddToPlaylist}>
                  <button disabled={props.actionsDisabled} class="icon-button track-playlist-action" onClick={() => props.onAddToPlaylist?.(track)} aria-label={`将 ${track.title} 添加到歌单`}><ListMusic size={17} /></button>
                </Show>
                <Show when={props.onRemove}>
                  <button disabled={props.actionsDisabled} class="icon-button track-remove-action" onClick={() => props.onRemove?.(track, index())} aria-label={`从歌单移除 ${track.title}`} title="从歌单移除"><ListX size={16} /></button>
                </Show>
                <Show when={props.onDelete}>
                  <button disabled={props.deleteDisabled} class="icon-button danger track-delete-action" onClick={() => props.onDelete?.(track)} aria-label={`永久删除 ${track.title}`} title="永久删除服务器文件"><Trash2 size={16} /></button>
                </Show>
              </div>
            </div>
          )
        }}
      </For>
    </div>
  )
}

function TrackHeaderButton(props: {
  class?: string
  label: string
  sortKey: TrackSortKey
  activeKey?: TrackSortKey
  direction?: TrackSortDirection
  onSort?: (key: TrackSortKey) => void
}) {
  const active = () => props.activeKey === props.sortKey
  const nextDirection = () => active() && props.direction === 'asc' ? '降序' : '升序'
  return (
    <button
      class={`track-header-sort ${props.class || ''} ${active() ? 'is-active' : ''}`}
      onClick={() => props.onSort?.(props.sortKey)}
      aria-label={`按${props.label}${nextDirection()}排序`}
      aria-pressed={active()}
    >
      <span>{props.label}</span>
      <Show when={active()} fallback={<ChevronsUpDown size={13} />}>
        <Show when={props.direction === 'desc'} fallback={<ArrowUp size={13} />}><ArrowDown size={13} /></Show>
      </Show>
    </button>
  )
}

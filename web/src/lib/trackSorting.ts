import type { Track } from '../types'

export type TrackSortKey = 'title' | 'artist' | 'album' | 'kind' | 'duration'
export type TrackSortDirection = 'asc' | 'desc'

const collator = new Intl.Collator('zh-CN', {
  numeric: true,
  sensitivity: 'base',
})

function trackSortValue(track: Track, key: TrackSortKey): string | number {
  switch (key) {
    case 'title':
      return track.title.trim()
    case 'artist':
      return track.artists.map((artist) => artist.name.trim()).filter(Boolean).join('; ')
    case 'album':
      return track.album.trim()
    case 'kind':
      return (track.genre || track.suffix || '').trim()
    case 'duration':
      return track.duration
  }
}

export function sortTracks(
  tracks: Track[],
  key: TrackSortKey,
  direction: TrackSortDirection,
): Track[] {
  const multiplier = direction === 'asc' ? 1 : -1

  return tracks
    .map((track, index) => ({ track, index, value: trackSortValue(track, key) }))
    .sort((left, right) => {
      const leftEmpty = typeof left.value === 'string' && !left.value
      const rightEmpty = typeof right.value === 'string' && !right.value
      if (leftEmpty !== rightEmpty) return leftEmpty ? 1 : -1

      const compared = typeof left.value === 'number' && typeof right.value === 'number'
        ? left.value - right.value
        : collator.compare(String(left.value), String(right.value))
      return compared ? compared * multiplier : left.index - right.index
    })
    .map(({ track }) => track)
}

import { describe, expect, it } from 'vitest'
import { sortTracks } from '../src/lib/trackSorting'
import type { Track } from '../src/types'

const tracks: Track[] = [
  { id: '3', title: 'Song 10', artists: [{ id: 'b', name: 'Beta' }], album: '', genre: '', suffix: 'flac', duration: 180, playCount: 4 },
  { id: '1', title: 'Song 2', artists: [{ id: 'a', name: 'Alpha' }], album: 'First', genre: 'Rock', duration: 240, playCount: 9 },
  { id: '2', title: 'Song 1', artists: [], album: 'Second', genre: 'Jazz', duration: 120 },
]

describe('track sorting', () => {
  it('sorts text naturally and keeps missing metadata last', () => {
    expect(sortTracks(tracks, 'title', 'asc').map((track) => track.id)).toEqual(['2', '1', '3'])
    expect(sortTracks(tracks, 'artist', 'asc').map((track) => track.id)).toEqual(['1', '3', '2'])
    expect(sortTracks(tracks, 'album', 'desc').map((track) => track.id)).toEqual(['2', '1', '3'])
  })

  it('sorts numeric duration in both directions', () => {
    expect(sortTracks(tracks, 'duration', 'asc').map((track) => track.duration)).toEqual([120, 180, 240])
    expect(sortTracks(tracks, 'duration', 'desc').map((track) => track.duration)).toEqual([240, 180, 120])
  })

  it('sorts the current users play counts and treats missing counts as zero', () => {
    expect(sortTracks(tracks, 'playCount', 'desc').map((track) => track.id)).toEqual(['1', '3', '2'])
    expect(sortTracks(tracks, 'playCount', 'asc').map((track) => track.id)).toEqual(['2', '3', '1'])
  })
})

import type { Track } from '../types'

export function formatDuration(value = 0): string {
  if (!Number.isFinite(value)) return '0:00'
  const seconds = Math.max(0, Math.floor(value))
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`
}

export function trackArtistLabel(track: Pick<Track, 'artists'>): string {
  return track.artists.map((artist) => artist.name).join(', ') || 'Unknown Artist'
}

export function normalizeArtistMetadata(value = ''): string {
  const seen = new Set<string>()
  return value
    .split(/(?:, |; |& )/)
    .map((artist) => artist.trim())
    .filter((artist) => {
      const key = artist.toLocaleLowerCase()
      if (!artist || seen.has(key)) return false
      seen.add(key)
      return true
    })
    .join('; ')
}

export function formatBytes(value = 0): string {
  if (!value) return '—'
  const units = ['B', 'KB', 'MB', 'GB']
  const index = Math.min(Math.floor(Math.log(value) / Math.log(1024)), units.length - 1)
  return `${(value / 1024 ** index).toFixed(index > 1 ? 1 : 0)} ${units[index]}`
}

export function joinPath(base: string, name: string): string {
  return `${base.replace(/\/$/, '')}/${name}`
}

export function parentPath(path: string, roots: string[]): string {
  if (roots.includes(path)) return path
  return path.replace(/\/[^/]+\/?$/, '') || '/'
}

export function safeHttpUrl(value?: string): string {
  const candidate = value?.trim()
  if (!candidate) return ''
  try {
    const url = new URL(candidate)
    return url.protocol === 'http:' || url.protocol === 'https:' ? candidate : ''
  } catch {
    return ''
  }
}

export function safeRadioStreamUrl(value?: string): string {
  const candidate = value?.trim()
  if (!candidate) return ''
  try {
    const url = new URL(candidate)
    return url.hostname && ['http:', 'https:', 'rtsp:', 'mms:', 'mmsh:', 'mmst:'].includes(url.protocol)
      ? candidate
      : ''
  } catch {
    return ''
  }
}

export function cx(...values: Array<string | false | null | undefined>): string {
  return values.filter(Boolean).join(' ')
}

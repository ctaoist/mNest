import { describe, expect, it } from 'vitest'
import { formatBytes, formatDuration, joinPath, normalizeArtistMetadata, parentPath, safeHttpUrl, safeRadioStreamUrl } from '../src/lib/utils'

describe('format helpers', () => {
  it('formats durations and file sizes', () => {
    expect(formatDuration(125)).toBe('2:05')
    expect(formatBytes(1_048_576)).toBe('1.0 MB')
  })

  it('keeps navigation inside configured roots', () => {
    expect(joinPath('/music/', 'Album')).toBe('/music/Album')
    expect(parentPath('/music/Album', ['/music'])).toBe('/music')
    expect(parentPath('/music', ['/music'])).toBe('/music')
  })

  it('normalizes multiple artists with semicolons', () => {
    expect(normalizeArtistMetadata('Artist A, Artist B & Artist C; Artist A')).toBe('Artist A; Artist B; Artist C')
    expect(normalizeArtistMetadata('Artist A,Artist B&Artist C')).toBe('Artist A,Artist B&Artist C')
    expect(normalizeArtistMetadata('AC/DC、Guest')).toBe('AC/DC、Guest')
  })

  it('allows only absolute HTTP radio links', () => {
    expect(safeHttpUrl(' https://radio.example/live ')).toBe('https://radio.example/live')
    expect(safeHttpUrl('http://radio.example/live')).toBe('http://radio.example/live')
    expect(safeHttpUrl('javascript:alert(1)')).toBe('')
    expect(safeHttpUrl('file:///tmp/radio')).toBe('')
    expect(safeHttpUrl('/relative')).toBe('')
  })

  it('accepts server-proxied radio stream protocols', () => {
    for (const value of [
      'https://radio.example/live',
      'rtsp://radio.example/live',
      'mms://radio.example/live',
      'mmsh://radio.example/live',
      'mmst://radio.example:1755/live',
    ]) expect(safeRadioStreamUrl(` ${value} `)).toBe(value)
    expect(safeRadioStreamUrl('file:///tmp/radio')).toBe('')
    expect(safeRadioStreamUrl('javascript:alert(1)')).toBe('')
    expect(safeRadioStreamUrl('/relative')).toBe('')
  })
})

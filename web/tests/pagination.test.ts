import { describe, expect, it } from 'vitest'
import {
  buildPaginationItems,
  readStoredPageSize,
  writeStoredPageSize,
} from '../src/lib/pagination'

describe('buildPaginationItems', () => {
  it('keeps the first, last, current and adjacent pages', () => {
    expect(buildPaginationItems(6, 12)).toEqual([
      1, 'start-ellipsis', 5, 6, 7, 'end-ellipsis', 12,
    ])
  })

  it('shows one or two intermediate pages instead of an ellipsis', () => {
    expect(buildPaginationItems(4, 8)).toEqual([1, 2, 3, 4, 5, 6, 7, 8])
  })

  it('handles pages near the beginning and end', () => {
    expect(buildPaginationItems(1, 10)).toEqual([1, 2, 'end-ellipsis', 10])
    expect(buildPaginationItems(9, 10)).toEqual([1, 'start-ellipsis', 8, 9, 10])
  })

  it('persists and restores valid page sizes', () => {
    const values = new Map<string, string>()
    const storage = {
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => values.set(key, value),
    }

    writeStoredPageSize('player.songs', 50, storage)

    expect(readStoredPageSize('player.songs', 30, storage)).toBe(50)
  })

  it('ignores invalid stored page sizes', () => {
    const storage = {
      getItem: () => '37',
      setItem: () => undefined,
    }

    expect(readStoredPageSize('player.songs', 30, storage)).toBe(30)
  })
})

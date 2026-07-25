import { describe, expect, it } from 'vitest'
import { filterScraperEntries } from '../src/lib/scraper'
import type { FileNode } from '../src/types'

const entries: FileNode[] = [
  { id: 1, name: 'folder', title: 'folder', icon: 'icon-folder', size: 0, update_time: '', children: [], needs_scrape: false },
  { id: 2, name: 'pending.mp3', title: 'pending.mp3', icon: 'icon-script-file', size: 1, update_time: '', children: null, needs_scrape: true },
  { id: 3, name: 'complete.mp3', title: 'complete.mp3', icon: 'icon-script-file', size: 1, update_time: '', children: null, needs_scrape: false },
]

describe('scraper file filtering', () => {
  it('defaults to pending songs while retaining folders', () => {
    expect(filterScraperEntries(entries, 'needs_scrape', '').map((entry) => entry.name)).toEqual(['folder', 'pending.mp3'])
  })

  it('searches all songs regardless of the selected scope', () => {
    expect(filterScraperEntries(entries, 'needs_scrape', 'complete').map((entry) => entry.name)).toEqual(['complete.mp3'])
    expect(filterScraperEntries(entries, 'needs_scrape', 'folder')).toEqual([])
  })
})

import type { FileNode } from '../types'

export type ScraperFileScope = 'needs_scrape' | 'all'

export function filterScraperEntries(entries: FileNode[], scope: ScraperFileScope, keyword: string): FileNode[] {
  const query = keyword.trim().toLowerCase()
  return entries.filter((entry) => {
    if (query) {
      return entry.icon === 'icon-script-file' && entry.name.toLowerCase().includes(query)
    }
    return entry.icon === 'icon-folder' || scope === 'all' || entry.needs_scrape
  })
}

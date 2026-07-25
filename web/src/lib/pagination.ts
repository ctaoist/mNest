export type PaginationItem = number | 'start-ellipsis' | 'end-ellipsis'

export const PAGE_SIZE_OPTIONS = [12, 24, 30, 50, 100] as const

type PageSizeStorage = Pick<Storage, 'getItem' | 'setItem'>

function browserStorage(): PageSizeStorage | undefined {
  try {
    return typeof window === 'undefined' ? undefined : window.localStorage
  } catch {
    return undefined
  }
}

function isPageSize(value: number): boolean {
  return PAGE_SIZE_OPTIONS.some((option) => option === value)
}

export function readStoredPageSize(
  key: string,
  fallback: number,
  storage = browserStorage(),
): number {
  if (!storage) return fallback
  try {
    const rawValue = storage.getItem(key)
    if (rawValue === null) return fallback
    const value = Number(rawValue)
    return Number.isInteger(value) && isPageSize(value) ? value : fallback
  } catch {
    return fallback
  }
}

export function writeStoredPageSize(
  key: string,
  value: number,
  storage = browserStorage(),
): void {
  if (!storage || !isPageSize(value)) return
  try {
    storage.setItem(key, String(value))
  } catch {
    // Browsing modes that disable storage should not break pagination.
  }
}

export function buildPaginationItems(currentPage: number, totalPages: number): PaginationItem[] {
  const pageCount = Math.max(1, Math.floor(totalPages))
  const current = Math.max(1, Math.min(Math.floor(currentPage), pageCount))
  const anchorPages = [...new Set([1, current - 1, current, current + 1, pageCount])]
    .filter((page) => page >= 1 && page <= pageCount)
    .sort((left, right) => left - right)

  const items: PaginationItem[] = []

  anchorPages.forEach((page, index) => {
    const previous = anchorPages[index - 1]
    if (previous !== undefined) {
      const hiddenCount = page - previous - 1
      if (hiddenCount >= 3) {
        items.push(page < current ? 'start-ellipsis' : 'end-ellipsis')
      } else {
        for (let missing = previous + 1; missing < page; missing += 1) items.push(missing)
      }
    }
    items.push(page)
  })

  return items
}

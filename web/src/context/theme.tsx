import { createContext, createEffect, createSignal, ParentProps, useContext } from 'solid-js'

export type ThemeName = 'archive' | 'minimal' | 'studio'

const THEME_KEY = 'mNest-theme'

interface ThemeContextValue {
  theme: () => ThemeName
  setTheme: (theme: ThemeName) => void
}

const ThemeContext = createContext<ThemeContextValue>()

export function ThemeProvider(props: ParentProps) {
  const saved = localStorage.getItem(THEME_KEY) as ThemeName | null
  const [theme, setThemeSignal] = createSignal<ThemeName>(
    saved && ['archive', 'minimal', 'studio'].includes(saved) ? saved : 'minimal',
  )

  createEffect(() => {
    const value = theme()
    document.documentElement.dataset.theme = value
    localStorage.setItem(THEME_KEY, value)
    const colors: Record<ThemeName, string> = {
      archive: '#0d2b3a',
      minimal: '#f2f1ec',
      studio: '#080a0f',
    }
    document.querySelector('meta[name="theme-color"]')?.setAttribute('content', colors[value])
  })

  return (
    <ThemeContext.Provider value={{ theme, setTheme: setThemeSignal }}>
      {props.children}
    </ThemeContext.Provider>
  )
}

export function useTheme() {
  const context = useContext(ThemeContext)
  if (!context) throw new Error('ThemeProvider is missing')
  return context
}

import { fireEvent, render, screen } from '@solidjs/testing-library'
import { beforeEach, describe, expect, it } from 'vitest'
import { ThemeProvider, useTheme } from '../src/context/theme'

function ThemeHarness() {
  const theme = useTheme()
  return <button onClick={() => theme.setTheme('studio')}>{theme.theme()}</button>
}

describe('ThemeProvider', () => {
  beforeEach(() => localStorage.clear())

  it('defaults to minimal and persists theme changes', async () => {
    render(() => <ThemeProvider><ThemeHarness /></ThemeProvider>)
    expect(screen.getByRole('button')).toHaveTextContent('minimal')
    await fireEvent.click(screen.getByRole('button'))
    expect(document.documentElement.dataset.theme).toBe('studio')
    expect(localStorage.getItem('mNest-theme')).toBe('studio')
  })
})

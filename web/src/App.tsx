import { Navigate, Route, Router } from '@solidjs/router'
import { ProtectedLayout } from './components/AppShell'
import { AuthProvider } from './context/auth'
import { PlayerProvider } from './context/player'
import { PreferencesProvider } from './context/preferences'
import { ThemeProvider } from './context/theme'
import { ToastProvider } from './context/toast'
import { LoginPage } from './pages/LoginPage'
import { DownloadPage } from './pages/DownloadPage'
import { PlayerPage } from './pages/PlayerPage'
import { ScraperPage } from './pages/ScraperPage'
import { SettingsPage } from './pages/SettingsPage'

function Providers(props: { children?: unknown }) {
  return (
    <ThemeProvider>
      <ToastProvider>
        <AuthProvider>
          <PreferencesProvider>
            <PlayerProvider>{props.children as any}</PlayerProvider>
          </PreferencesProvider>
        </AuthProvider>
      </ToastProvider>
    </ThemeProvider>
  )
}

export default function App() {
  return (
    <Providers>
      <Router>
        <Route path="/login" component={LoginPage} />
        <Route path="/" component={ProtectedLayout}>
          <Route path="/" component={() => <Navigate href="/player" />} />
          <Route path="/player" component={PlayerPage} />
          <Route path="/scraper" component={ScraperPage} />
          <Route path="/download" component={DownloadPage} />
          <Route path="/settings" component={SettingsPage} />
        </Route>
        <Route path="*" component={() => <Navigate href="/player" />} />
      </Router>
    </Providers>
  )
}

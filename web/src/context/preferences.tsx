import { createContext, createEffect, createSignal, ParentProps, useContext } from 'solid-js'
import { useAuth } from './auth'
import { get, post } from '../lib/api'
import type { UserPreferences, WebPlaybackBitrate } from '../types'

export const WEB_PLAYBACK_BITRATES: readonly WebPlaybackBitrate[] = [0, 64, 96, 128, 192, 256, 320]

interface PreferencesContextValue {
  webPlaybackBitrate: () => WebPlaybackBitrate
  loading: () => boolean
  saveWebPlaybackBitrate: (value: WebPlaybackBitrate) => Promise<void>
}

const fallbackPreferences: PreferencesContextValue = {
  webPlaybackBitrate: () => 0,
  loading: () => false,
  saveWebPlaybackBitrate: async () => undefined,
}

const PreferencesContext = createContext<PreferencesContextValue>()

export function PreferencesProvider(props: ParentProps) {
  const auth = useAuth()
  const [webPlaybackBitrate, setWebPlaybackBitrate] = createSignal<WebPlaybackBitrate>(0)
  const [loading, setLoading] = createSignal(false)
  let loadSequence = 0

  createEffect(() => {
    const user = auth.user()
    const sequence = ++loadSequence
    if (!user) {
      setWebPlaybackBitrate(0)
      setLoading(false)
      return
    }
    setLoading(true)
    void get<UserPreferences>('/api/user/preferences/').then((preferences) => {
      if (sequence !== loadSequence) return
      setWebPlaybackBitrate(normalizeWebPlaybackBitrate(preferences.web_playback_bitrate))
    }).catch(() => {
      if (sequence === loadSequence) setWebPlaybackBitrate(0)
    }).finally(() => {
      if (sequence === loadSequence) setLoading(false)
    })
  })

  const saveWebPlaybackBitrate = async (value: WebPlaybackBitrate) => {
    const preferences = await post<UserPreferences>('/api/user/preferences/', {
      web_playback_bitrate: value,
    })
    setWebPlaybackBitrate(normalizeWebPlaybackBitrate(preferences.web_playback_bitrate))
  }

  return (
    <PreferencesContext.Provider value={{ webPlaybackBitrate, loading, saveWebPlaybackBitrate }}>
      {props.children}
    </PreferencesContext.Provider>
  )
}

export function usePreferences() {
  return useContext(PreferencesContext) || fallbackPreferences
}

function normalizeWebPlaybackBitrate(value: number): WebPlaybackBitrate {
  return WEB_PLAYBACK_BITRATES.includes(value as WebPlaybackBitrate)
    ? value as WebPlaybackBitrate
    : 0
}


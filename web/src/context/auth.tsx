import { createContext, createSignal, onMount, ParentProps, useContext } from 'solid-js'
import { request } from '../lib/api'
import type { ApiResponse, User } from '../types'

interface LoginResponse {
  user: User
}

interface AuthContextValue {
  user: () => User | null
  loading: () => boolean
  login: (username: string, password: string) => Promise<void>
  logout: () => Promise<void>
  reload: () => Promise<void>
}

const AuthContext = createContext<AuthContextValue>()

export function AuthProvider(props: ParentProps) {
  const [user, setUser] = createSignal<User | null>(null)
  const [loading, setLoading] = createSignal(true)

  const reload = async () => {
    setLoading(true)
    try {
      const response = await request<ApiResponse<User>>('/user/info/')
      setUser(response.data)
    } catch {
      setUser(null)
    } finally {
      setLoading(false)
    }
  }

  const login = async (username: string, password: string) => {
    const response = await request<LoginResponse>('/api/token/', {
      method: 'POST',
      body: JSON.stringify({ username, password }),
    })
    setUser(response.user)
  }

  const logout = async () => {
    await request('/api/logout/', { method: 'POST' }, false).catch(() => undefined)
    setUser(null)
  }

  onMount(() => void reload())

  return (
    <AuthContext.Provider value={{ user, loading, login, logout, reload }}>
      {props.children}
    </AuthContext.Provider>
  )
}

export function useAuth() {
  const context = useContext(AuthContext)
  if (!context) throw new Error('AuthProvider is missing')
  return context
}

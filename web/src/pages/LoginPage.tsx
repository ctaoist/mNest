import { Navigate, useNavigate, useSearchParams } from '@solidjs/router'
import { createSignal, Show } from 'solid-js'
import { ArrowRight, Disc3, LockKeyhole, UserRound } from 'lucide-solid'
import { useAuth } from '../context/auth'

export function LoginPage() {
  const auth = useAuth()
  const navigate = useNavigate()
  const [search] = useSearchParams()
  const [username, setUsername] = createSignal('')
  const [password, setPassword] = createSignal('')
  const [loading, setLoading] = createSignal(false)
  const [error, setError] = createSignal('')

  if (!auth.loading() && auth.user()) return <Navigate href="/player" />

  const submit = async (event: SubmitEvent) => {
    event.preventDefault()
    setLoading(true)
    setError('')
    try {
      await auth.login(username(), password())
      navigate(typeof search.next === 'string' && search.next.startsWith('/') ? search.next : '/player', { replace: true })
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : '登录失败')
    } finally {
      setLoading(false)
    }
  }

  return (
    <main class="login-page">
      <div class="login-orbit orbit-one" aria-hidden="true" />
      <div class="login-orbit orbit-two" aria-hidden="true" />
      <section class="login-story">
        <div class="login-brand"><span><Disc3 /></span>mNest</div>
        <div>
          <p class="eyebrow">YOUR PRIVATE LISTENING ROOM</p>
          <h1>让每一首歌，<br />回到正确的位置。</h1>
          <p>播放、整理、刮削。一个为私人曲库打造的工作台。</p>
        </div>
        <div class="login-index"><span>01</span><div /><span>03</span></div>
      </section>
      <section class="login-panel">
        <div class="login-card">
          <span class="record-label"><Disc3 /></span>
          <p class="eyebrow">WELCOME BACK</p>
          <h2>进入曲库</h2>
          <p class="muted">使用服务端配置的账号继续</p>
          <form onSubmit={submit}>
            <label class="input-wrap"><span>账号</span><div><UserRound size={18} /><input value={username()} onInput={(event) => setUsername(event.currentTarget.value)} autocomplete="username" required placeholder="admin" /></div></label>
            <label class="input-wrap"><span>密码</span><div><LockKeyhole size={18} /><input type="password" value={password()} onInput={(event) => setPassword(event.currentTarget.value)} autocomplete="current-password" required placeholder="••••••••" /></div></label>
            <Show when={error()}><div class="form-error">{error()}</div></Show>
            <button class="primary-button login-submit" disabled={loading()}>
              {loading() ? '正在验证…' : '进入控制台'}<ArrowRight size={18} />
            </button>
          </form>
          <div class="login-security"><span class="pulse-dot" />凭据通过同站加密会话保护</div>
        </div>
      </section>
    </main>
  )
}

import { A, Navigate, RouteSectionProps, useLocation, useNavigate } from '@solidjs/router'
import { Show } from 'solid-js'
import { Dynamic } from 'solid-js/web'
import { Disc3, Download, LogOut, Radio, Settings2, SlidersHorizontal } from 'lucide-solid'
import { useAuth } from '../context/auth'
import { PlayerBar } from './PlayerBar'

const items = [
  { href: '/player', label: '播放', icon: Radio },
  { href: '/scraper', label: '刮削', icon: SlidersHorizontal, admin: true },
  { href: '/download', label: '下载', icon: Download, admin: true },
  { href: '/settings', label: '设置', icon: Settings2 },
]

export function ProtectedLayout(props: RouteSectionProps) {
  const auth = useAuth()
  const navigate = useNavigate()
  const location = useLocation()

  const logout = async () => {
    await auth.logout()
    navigate('/login', { replace: true })
  }

  return (
    <Show when={!auth.loading()} fallback={<div class="app-loading"><Disc3 class="spin-slow" size={46} /><span>正在整理唱针…</span></div>}>
      <Show when={auth.user()} fallback={<Navigate href={`/login?next=${encodeURIComponent(location.pathname)}`} />}>
      <div class="app-shell">
      <aside class="sidebar">
        <A class="brand" href="/player">
          <span class="brand-disc"><Disc3 /></span>
          <span><strong>mNest</strong></span>
        </A>
        <nav>
          {items.map((item) => (
            <Show when={!item.admin || auth.user()?.role === 'admin'}>
              <A href={item.href} activeClass="is-active"><Dynamic component={item.icon} /><span>{item.label}</span></A>
            </Show>
          ))}
        </nav>
        <div class="sidebar-foot">
          <div class="user-chip"><span>{auth.user()?.username.slice(0, 1).toUpperCase()}</span><div><strong>{auth.user()?.username}</strong><small>{auth.user()?.role === 'admin' ? '管理员' : '用户'}</small></div></div>
          <button class="icon-button" onClick={logout} aria-label="退出登录"><LogOut size={18} /></button>
        </div>
      </aside>

      <header class="mobile-header">
        <nav class={`mobile-header-nav ${auth.user()?.role === 'admin' ? 'has-admin-items' : ''}`} aria-label="主要功能">
          {items.map((item) => (
            <Show when={!item.admin || auth.user()?.role === 'admin'}>
              <A href={item.href} activeClass="is-active"><Dynamic component={item.icon} /><span>{item.href === '/player' ? '播放' : item.label}</span></A>
            </Show>
          ))}
        </nav>
      </header>

      <main class="page-stage">{props.children}</main>

      <PlayerBar />
      </div>
      </Show>
    </Show>
  )
}

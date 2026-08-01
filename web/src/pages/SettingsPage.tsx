import { createMemo, createSignal, For, onCleanup, onMount, Show } from 'solid-js'
import {
  Activity,
  Check,
  CircleAlert,
  CloudDownload,
  Database,
  Edit3,
  ExternalLink,
  FolderPlus,
  Gauge,
  HardDrive,
  Headphones,
  KeyRound,
  LoaderCircle,
  Link2,
  LogIn,
  MoonStar,
  Palette,
  Play,
  Plus,
  RadioTower,
  RefreshCw,
  ServerCog,
  SunMedium,
  Trash2,
  Unlink,
  Wrench,
  X,
} from 'lucide-solid'
import { GitHubMark } from '../components/GitHubMark'
import { useAuth } from '../context/auth'
import { ThemeName, useTheme } from '../context/theme'
import { useToast } from '../context/toast'
import { usePreferences, WEB_PLAYBACK_BITRATES } from '../context/preferences'
import { get, post, request, subscribeJobs, subsonic } from '../lib/api'
import { safeHttpUrl, safeRadioStreamUrl } from '../lib/utils'
import type { ConfigStatus, DownloadFilenameFormat, DownloadSource, DownloadSourceKind, JobRecord, LastFmStatus, LibraryRoot, RadioStation, WebPlaybackBitrate } from '../types'

const themes: Array<{ id: ThemeName; name: string; description: string; icon: typeof SunMedium }> = [
  { id: 'archive', name: '唱片档案馆', description: '深墨蓝、暖金与纸张质感', icon: HardDrive },
  { id: 'minimal', name: '现代极简', description: '明亮中性、克制而清晰', icon: SunMedium },
  { id: 'studio', name: '暗色录音棚', description: '沉浸深色与高对比状态', icon: MoonStar },
]

const sourceNames: Record<DownloadSourceKind, string> = {
  netease: '网易云音乐', qq: 'QQ 音乐', qq2: 'QQ 音乐 2', subsonic: 'Subsonic',
}

const sourceProjects: Partial<Record<DownloadSourceKind, { label: string; url: string }>> = {
  netease: {
    label: 'NeteaseCloudMusicApiEnhanced/api-enhanced',
    url: 'https://github.com/NeteaseCloudMusicApiEnhanced/api-enhanced',
  },
  // /search and /song/url are provided by jsososo/QQMusicApi.
  qq: {
    label: 'jsososo/QQMusicApi',
    url: 'https://github.com/jsososo/QQMusicApi',
  },
  // /getSearchByKey and /getMusicPlay are provided by Rain120/qq-music-api.
  qq2: {
    label: 'Rain120/qq-music-api',
    url: 'https://github.com/Rain120/qq-music-api',
  },
}

type GatewaySourceKind = 'netease' | 'qq' | 'qq2'

const gatewaySourceKinds: GatewaySourceKind[] = ['netease', 'qq', 'qq2']
const sourceActionLabels: Record<GatewaySourceKind, string> = {
  netease: '网易云', qq: 'QQ 音乐', qq2: 'QQ 音乐2',
}

type DownloadSourceDraft = {
  id?: string
  kind: DownloadSourceKind
  name: string
  base_url: string
  username: string
  password: string
  enabled: boolean
}

type RadioDraft = {
  id?: string
  name: string
  streamUrl: string
  homePageUrl: string
  proxy: boolean
}

export function SettingsPage() {
  const auth = useAuth()
  const theme = useTheme()
  const preferences = usePreferences()
  const toast = useToast()
  const [status, setStatus] = createSignal<ConfigStatus | null>(null)
  const [health, setHealth] = createSignal<{ status: string; version: string } | null>(null)
  const [jobs, setJobs] = createSignal<JobRecord[]>([])
  const [downloadSources, setDownloadSources] = createSignal<DownloadSource[]>([])
  const [radioStations, setRadioStations] = createSignal<RadioStation[]>([])
  const [lastFmStatus, setLastFmStatus] = createSignal<LastFmStatus | null>(null)
  const [jobFilter, setJobFilter] = createSignal('all')
  const [rootDialog, setRootDialog] = createSignal(false)
  const [editingRootId, setEditingRootId] = createSignal('')
  const [rootName, setRootName] = createSignal('')
  const [rootPath, setRootPath] = createSignal('')
  const [rootTranscodeCacheEnabled, setRootTranscodeCacheEnabled] = createSignal(false)
  const [rootTranscodeCachePath, setRootTranscodeCachePath] = createSignal('/data/cache/transcodes')
  const [sourceDialog, setSourceDialog] = createSignal(false)
  const [sourceDraft, setSourceDraft] = createSignal<DownloadSourceDraft>({ kind: 'netease', name: sourceNames.netease, base_url: '', username: '', password: '', enabled: true })
  const [neteaseLogin, setNeteaseLogin] = createSignal<{ source_id: string; key: string; qr_image: string; message: string } | null>(null)
  const [radioDialog, setRadioDialog] = createSignal(false)
  const [radioDraft, setRadioDraft] = createSignal<RadioDraft>({ name: '', streamUrl: '', homePageUrl: '', proxy: false })
  const [lastFmApiKey, setLastFmApiKey] = createSignal('')
  const [lastFmSharedSecret, setLastFmSharedSecret] = createSignal('')
  const [busy, setBusy] = createSignal('')
  let closeJobEvents: () => void = () => undefined
  let neteaseEvents: EventSource | undefined

  const isAdmin = () => auth.user()?.role === 'admin'

  const filteredJobs = createMemo(() => jobFilter() === 'all' ? jobs() : jobs().filter((job) => job.state === jobFilter()))
  const activeJob = createMemo(() => jobs().find((job) => job.state === 'running' || job.state === 'pending'))

  const load = async () => {
    try {
      if (isAdmin()) {
        const [config, healthData, sources, stations, lastfm] = await Promise.all([
          get<ConfigStatus>('/api/config/status/'),
          request<{ status: string; version: string }>('/health'),
          get<DownloadSource[]>('/api/download_sources/'),
          get<RadioStation[]>('/api/internet_radio_stations/'),
          get<LastFmStatus>('/api/lastfm/status/'),
        ])
        setStatus(config)
        setHealth(healthData)
        setDownloadSources(sources)
        setRadioStations(stations)
        setLastFmStatus(lastfm)
        setLastFmApiKey(lastfm.api_key)
      } else {
        const [healthData, lastfm] = await Promise.all([
          request<{ status: string; version: string }>('/health'),
          get<LastFmStatus>('/api/lastfm/status/'),
        ])
        setHealth(healthData)
        setLastFmStatus(lastfm)
        setLastFmApiKey(lastfm.api_key)
      }
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '系统状态加载失败', 'error')
    }
  }

  const loadJobs = async () => {
    try {
      const data = await get<{ items: JobRecord[] }>('/api/record/?page_size=80')
      setJobs(data.items)
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '任务记录加载失败', 'error')
    }
  }

  onMount(() => {
    void load()
    if (isAdmin()) {
      closeJobEvents = subscribeJobs(80, setJobs, (message) => toast.notify(message, 'error'))
    }
  })
  onCleanup(() => {
    closeJobEvents()
    neteaseEvents?.close()
  })

  const scan = async () => {
    setBusy('scan')
    try {
      const data = await post<{ job_id: string }>('/api/scan/', {})
      toast.notify(`扫描任务已创建：${data.job_id.slice(0, 8)}`, 'success')
      await load()
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '扫描启动失败', 'error')
    } finally {
      setBusy('')
    }
  }

  const openRootDialog = (root?: LibraryRoot) => {
    setEditingRootId(root?.id || '')
    setRootName(root?.name || '')
    setRootPath(root?.path || '')
    setRootTranscodeCacheEnabled(root?.transcode_cache.enabled || false)
    setRootTranscodeCachePath(root?.transcode_cache.path || '/data/cache/transcodes')
    setRootDialog(true)
  }

  const closeRootDialog = () => {
    setRootDialog(false)
    setEditingRootId('')
    setRootName('')
    setRootPath('')
    setRootTranscodeCacheEnabled(false)
    setRootTranscodeCachePath('/data/cache/transcodes')
  }

  const saveRoot = async (event: SubmitEvent) => {
    event.preventDefault()
    setBusy('root')
    const payload = {
      name: rootName().trim(),
      path: rootPath().trim(),
      transcode_cache: {
        enabled: rootTranscodeCacheEnabled(),
        path: rootTranscodeCachePath().trim(),
      },
    }
    try {
      if (editingRootId()) {
        await post('/api/library_roots/update/', { id: editingRootId(), ...payload })
        toast.notify('曲库目录已更新，不会自动扫描', 'success')
      } else {
        await post('/api/library_roots/', payload)
        toast.notify('曲库目录已添加，已开始后台扫描', 'success')
      }
      closeRootDialog()
      await load()
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : editingRootId() ? '曲库修改失败' : '曲库添加失败', 'error')
    } finally {
      setBusy('')
    }
  }

  const removeRoot = async (id: string, name: string) => {
    if (!window.confirm(`确认删除曲库“${name}”？\n\n只会删除数据库索引，不会删除磁盘上的音乐文件。`)) return
    try {
      await post('/api/library_roots/delete/', { id })
      toast.notify('曲库索引已删除', 'success')
      await load()
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '曲库删除失败', 'error')
    }
  }

  const openSourceDialog = (kind: DownloadSourceKind, source?: DownloadSource) => {
    setSourceDraft(source ? {
      id: source.id,
      kind: source.kind,
      name: source.name,
      base_url: source.base_url,
      username: source.username,
      password: '',
      enabled: source.enabled,
    } : { kind, name: sourceNames[kind], base_url: '', username: '', password: '', enabled: true })
    setSourceDialog(true)
  }

  const saveDownloadSource = async (event: SubmitEvent) => {
    event.preventDefault()
    setBusy('download-source')
    try {
      await post('/api/download_sources/', sourceDraft())
      toast.notify('下载来源已保存', 'success')
      setSourceDialog(false)
      await load()
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '下载来源保存失败', 'error')
    } finally {
      setBusy('')
    }
  }

  const removeDownloadSource = async (source: DownloadSource) => {
    if (!window.confirm(`确认删除下载来源“${source.name}”？`)) return
    try {
      await post('/api/download_sources/delete/', { id: source.id })
      toast.notify('下载来源已删除', 'success')
      await load()
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '下载来源删除失败', 'error')
    }
  }

  const saveDownloadFilenameFormat = async (value: DownloadFilenameFormat) => {
    const previous = status()?.download_filename_format || 'artist-title'
    setStatus((current) => current ? { ...current, download_filename_format: value } : current)
    setBusy('filename-format')
    try {
      await post('/api/config/preferences/', { download_filename_format: value })
      toast.notify('下载文件名格式已保存', 'success')
    } catch (error) {
      setStatus((current) => current ? { ...current, download_filename_format: previous } : current)
      toast.notify(error instanceof Error ? error.message : '下载文件名格式保存失败', 'error')
    } finally {
      setBusy('')
    }
  }

  const updateLastFmStatus = (lastfm: LastFmStatus) => {
    setLastFmStatus(lastfm)
    setStatus((current) => current ? { ...current, lastfm } : current)
    setLastFmApiKey(lastfm.api_key)
  }

  const saveLastFmConfig = async (event: SubmitEvent) => {
    event.preventDefault()
    setBusy('lastfm-config')
    try {
      const lastfm = await post<LastFmStatus>('/api/lastfm/config/', {
        api_key: lastFmApiKey().trim(),
        shared_secret: lastFmSharedSecret().trim(),
      })
      updateLastFmStatus(lastfm)
      setLastFmSharedSecret('')
      toast.notify('Last.fm API 配置已保存', 'success')
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : 'Last.fm 配置保存失败', 'error')
    } finally {
      setBusy('')
    }
  }

  const startLastFmAuth = async () => {
    const popup = window.open('about:blank', 'lastfm-auth', 'popup,width=720,height=760')
    setBusy('lastfm-auth-start')
    try {
      const data = await post<{ authorization_url: string }>('/api/lastfm/auth/start/', {})
      setStatus((current) => current ? {
        ...current,
        lastfm: { ...current.lastfm, authorization_pending: true },
      } : current)
      if (popup) popup.location.href = data.authorization_url
      else window.open(data.authorization_url, '_blank', 'noopener,noreferrer')
      toast.notify('请在 Last.fm 页面允许访问，然后返回此处完成授权', 'success')
    } catch (error) {
      popup?.close()
      toast.notify(error instanceof Error ? error.message : 'Last.fm 授权启动失败', 'error')
    } finally {
      setBusy('')
    }
  }

  const completeLastFmAuth = async () => {
    setBusy('lastfm-auth-complete')
    try {
      const lastfm = await post<LastFmStatus>('/api/lastfm/auth/complete/', {})
      updateLastFmStatus(lastfm)
      toast.notify(`Last.fm 已连接：${lastfm.username}`, 'success')
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : 'Last.fm 授权确认失败', 'error')
    } finally {
      setBusy('')
    }
  }

  const disconnectLastFm = async () => {
    if (!window.confirm('确认断开 Last.fm？已保存的 API Key 和 Shared Secret 会保留。')) return
    setBusy('lastfm-disconnect')
    try {
      updateLastFmStatus(await post<LastFmStatus>('/api/lastfm/disconnect/', {}))
      toast.notify('Last.fm 已断开', 'success')
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : 'Last.fm 断开失败', 'error')
    } finally {
      setBusy('')
    }
  }

  const openRadioDialog = (station?: RadioStation) => {
    setRadioDraft(station ? {
      id: station.id,
      name: station.name,
      streamUrl: station.streamUrl,
      homePageUrl: station.homePageUrl || '',
      proxy: !!station.proxy,
    } : { name: '', streamUrl: '', homePageUrl: '', proxy: false })
    setRadioDialog(true)
  }

  const reloadRadios = async () => {
    setRadioStations(await get<RadioStation[]>('/api/internet_radio_stations/'))
  }

  const saveRadio = async (event: SubmitEvent) => {
    event.preventDefault()
    const draft = radioDraft()
    const streamUrl = safeRadioStreamUrl(draft.streamUrl)
    if (!streamUrl) {
      toast.notify('电台流地址必须使用 HTTP、HTTPS、RTSP、MMS、MMSH 或 MMST', 'error')
      return
    }
    setBusy('radio-save')
    try {
      await subsonic(draft.id ? 'updateInternetRadioStation' : 'createInternetRadioStation', {
        id: draft.id,
        name: draft.name.trim(),
        streamUrl,
        homepageUrl: draft.homePageUrl.trim(),
        proxy: draft.proxy,
      })
      toast.notify(draft.id ? '网络电台已更新' : '网络电台已添加', 'success')
      setRadioDialog(false)
      await reloadRadios()
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '网络电台保存失败', 'error')
    } finally {
      setBusy('')
    }
  }

  const removeRadio = async (station: RadioStation) => {
    if (!window.confirm(`确认删除网络电台“${station.name}”？`)) return
    setBusy(`radio-delete:${station.id}`)
    try {
      await post('/api/internet_radio_stations/delete/', { id: station.id })
      toast.notify('网络电台已删除', 'success')
      await reloadRadios()
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '网络电台删除失败', 'error')
    } finally {
      setBusy('')
    }
  }

  const closeNeteaseLogin = () => {
    neteaseEvents?.close()
    neteaseEvents = undefined
    setNeteaseLogin(null)
  }

  const watchNeteaseLogin = (sourceId: string, key: string) => {
    neteaseEvents?.close()
    const search = new URLSearchParams({ source_id: sourceId, key })
    const events = new EventSource(`/api/events/netease-login/?${search.toString()}`)
    neteaseEvents = events
    events.addEventListener('netease-login', (event) => {
      const status = JSON.parse(event.data) as { code: number; message: string; logged_in: boolean; account_name: string }
      setNeteaseLogin((value) => value ? { ...value, message: status.message || (status.code === 802 ? '等待手机确认…' : '等待扫码…') } : null)
      if (status.logged_in) {
        toast.notify(status.account_name ? `网易云登录成功：${status.account_name}` : '网易云登录成功，Cookie 已保存在服务器', 'success')
        closeNeteaseLogin()
        void load()
      } else if (status.code === 800) {
        events.close()
        if (neteaseEvents === events) neteaseEvents = undefined
        toast.notify('二维码已过期，请重新获取', 'error')
      }
    })
    events.addEventListener('netease-login-error', (event) => {
      events.close()
      if (neteaseEvents === events) neteaseEvents = undefined
      toast.notify(event.data || '网易云登录状态检查失败', 'error')
    })
  }

  const startNeteaseLogin = async (source: DownloadSource) => {
    setBusy(`netease:${source.id}`)
    try {
      const data = await post<{ key: string; qr_image: string }>('/api/download_sources/netease/login/start/', { source_id: source.id })
      setNeteaseLogin({ source_id: source.id, key: data.key, qr_image: data.qr_image, message: '请使用网易云音乐扫码登录' })
      watchNeteaseLogin(source.id, data.key)
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '网易云二维码获取失败', 'error')
    } finally {
      setBusy('')
    }
  }

  const renderThemeSettings = () => (
    <section class="panel theme-settings">
      <div class="section-heading"><div><span class="eyebrow">APPEARANCE</span><h2>界面主题</h2></div><span class="muted">仅保存在当前浏览器</span></div>
      <div class="theme-grid"><For each={themes}>{(item) => <button class={`theme-card theme-preview-${item.id} ${theme.theme() === item.id ? 'is-active' : ''}`} onClick={() => theme.setTheme(item.id)}><div class="theme-preview"><span /><span /><span /><div /></div><div class="theme-info"><span class="theme-icon"><item.icon /></span><div><strong>{item.name}</strong><small>{item.description}</small></div><Show when={theme.theme() === item.id}><span class="theme-check"><Check size={15} /></span></Show></div></button>}</For></div>
    </section>
  )

  const saveWebPlaybackBitrate = async (value: WebPlaybackBitrate) => {
    setBusy('web-playback-bitrate')
    try {
      await preferences.saveWebPlaybackBitrate(value)
      toast.notify('网页播放码率已保存，将从下一次播放开始生效', 'success')
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '网页播放码率保存失败', 'error')
    } finally {
      setBusy('')
    }
  }

  const renderPlaybackSettings = () => {
    const bitrate = () => preferences.webPlaybackBitrate()
    return (
      <section class="panel playback-settings">
        <div class="section-heading"><div><span class="eyebrow">WEB PLAYBACK</span><div class="settings-section-title">网页播放</div></div><span class="playback-quality-badge">{bitrate() ? `${bitrate()} kbps` : 'SOURCE'}</span></div>
        <div class="playback-setting-row">
          <span class="playback-setting-icon"><Gauge /></span>
          <div><strong>网页端播放码率</strong><small>仅影响当前用户使用 mNest 网页播放器播放的普通歌曲；网络电台和 OpenSubsonic 客户端不受影响。选择固定码率时由服务器转为 MP3。</small></div>
          <label><span>输出质量</span><select aria-label="网页端播放码率" value={bitrate()} disabled={preferences.loading() || busy() === 'web-playback-bitrate'} onChange={(event) => void saveWebPlaybackBitrate(Number(event.currentTarget.value) as WebPlaybackBitrate)}><For each={WEB_PLAYBACK_BITRATES}>{(value) => <option value={value}>{value ? `${value} kbps` : '原始音质'}</option>}</For></select></label>
        </div>
      </section>
    )
  }

  const renderLastFmSettings = (admin: boolean) => {
    const lastfm = lastFmStatus()
    if (!lastfm) return null
    return (
      <section class="panel lastfm-settings">
        <div class="section-heading"><div><span class="eyebrow">PERSONAL LISTENING</span><div class="settings-section-title">Last.fm</div></div><div class="lastfm-heading-tools"><Show when={admin}><a href="https://www.last.fm/api/account/create" target="_blank" rel="noreferrer">获取 API 凭据<ExternalLink /></a></Show><span class={`lastfm-status ${lastfm.connected ? 'connected' : lastfm.configured ? 'configured' : ''}`}><span />{lastfm.connected ? 'CONNECTED' : lastfm.configured ? 'READY' : 'NOT CONFIGURED'}</span></div></div>
        <div class={`lastfm-layout ${admin ? '' : 'personal-only'}`}>
          <div class="lastfm-account-card">
            <span class="lastfm-mark"><Headphones /></span>
            <div><strong>{lastfm.username || '尚未连接账号'}</strong><small>{lastfm.connected ? '当前用户的播放记录会发送至这个 Last.fm 账号。' : lastfm.configured ? '连接后，内置播放器和 OpenSubsonic 客户端的有效播放都会上报。' : '管理员尚未配置 Last.fm 应用凭据。'}</small></div>
            <Show when={lastfm.connected}><button class="icon-button danger" disabled={busy() === 'lastfm-disconnect'} onClick={() => void disconnectLastFm()} aria-label="断开 Last.fm">{busy() === 'lastfm-disconnect' ? <LoaderCircle class="spin" /> : <Unlink />}</button></Show>
          </div>
          <Show when={admin}>
            <form class="lastfm-config" onSubmit={saveLastFmConfig}>
              <label><span>API Key</span><div><KeyRound /><input value={lastFmApiKey()} maxlength={128} autocomplete="off" onInput={(event) => setLastFmApiKey(event.currentTarget.value)} required placeholder="Last.fm API Key" /></div></label>
              <label><span>Shared Secret</span><div><KeyRound /><input type="password" value={lastFmSharedSecret()} maxlength={256} autocomplete="new-password" onInput={(event) => setLastFmSharedSecret(event.currentTarget.value)} required={!lastfm.has_shared_secret || lastFmApiKey().trim() !== lastfm.api_key} placeholder={lastfm.has_shared_secret && lastFmApiKey().trim() === lastfm.api_key ? '留空则保留已保存的 Secret' : 'Last.fm Shared Secret'} /></div></label>
              <button class="secondary-button" disabled={busy() === 'lastfm-config' || !lastFmApiKey().trim() || ((!lastfm.has_shared_secret || lastFmApiKey().trim() !== lastfm.api_key) && !lastFmSharedSecret().trim())}>{busy() === 'lastfm-config' ? <LoaderCircle class="spin" /> : <KeyRound size={15} />}保存 API 配置</button>
            </form>
          </Show>
        </div>
        <Show when={admin}><p class="lastfm-admin-note">应用凭据属于服务器；修改后会断开所有用户现有的 Last.fm 授权。</p></Show>
        <div class="lastfm-auth-strip"><div><Link2 /><span><strong>{lastfm.authorization_pending ? '授权等待确认' : lastfm.connected ? `已连接 ${lastfm.username}` : '连接个人 Last.fm 账号'}</strong><small>每个 mNest 用户拥有独立的加密 Session Key，互不共享。</small></span></div><div><button class="secondary-button small" disabled={!lastfm.configured || busy() === 'lastfm-auth-start'} onClick={() => void startLastFmAuth()}>{busy() === 'lastfm-auth-start' ? <LoaderCircle class="spin" /> : <ExternalLink size={15} />}{lastfm.connected ? '重新授权' : '打开授权页'}</button><Show when={lastfm.authorization_pending}><button class="primary-button small" disabled={busy() === 'lastfm-auth-complete'} onClick={() => void completeLastFmAuth()}>{busy() === 'lastfm-auth-complete' ? <LoaderCircle class="spin" /> : <Check size={15} />}完成授权</button></Show></div></div>
      </section>
    )
  }

  return (
    <div class="page settings-page">
      <header class="page-header settings-header">
        <p class="eyebrow">{isAdmin() ? 'SYSTEM CONTROL ROOM' : 'PERSONAL SETTINGS'}</p>
        <div class={`health-badge ${health()?.status === 'ok' ? 'ok' : 'error'}`}><Activity size={17} /><span><strong>{health()?.status === 'ok' ? '服务运行正常' : '服务状态未知'}</strong><small>SERVER {health()?.version || '—'}</small></span></div>
      </header>

      <Show when={lastFmStatus()} fallback={<div class="loading-panel"><LoaderCircle class="spin" /><span>正在读取设置…</span></div>}>
        <Show when={isAdmin()} fallback={<>{renderPlaybackSettings()}{renderThemeSettings()}{renderLastFmSettings(false)}</>}>
          <Show when={status()} fallback={<div class="loading-panel"><LoaderCircle class="spin" /><span>正在读取管理员设置…</span></div>}>
            {(config) => (
              <>
            <section class="status-cards page-reveal">
              <StatusCard icon={Database} label="DATABASE" value={config().database.toUpperCase()} detail={`${config().library_roots.length} 个曲库目录`} />
              <StatusCard icon={Gauge} label="JOB QUEUE" value={config().queue.toUpperCase()} detail={activeJob() ? '有任务正在执行' : '当前队列空闲'} accent={!!activeJob()} />
              <StatusCard icon={ServerCog} label="PROVIDERS" value={String(config().providers.length).padStart(2, '0')} detail={config().providers.join(' · ') || '未启用'} />
              <StatusCard icon={Wrench} label="LOCAL TOOLS" value={`${[config().tools.ffmpeg, config().tools.fpcalc].filter(Boolean).length}/2`} detail="FFmpeg 与 Chromaprint" warning={!config().tools.ffmpeg || !config().tools.fpcalc} />
            </section>

            <div class="settings-grid">
              <section class="panel library-settings">
                <div class="section-heading"><div><span class="eyebrow">MUSIC FOLDERS</span><h2>曲库目录</h2></div><button class="primary-button small" onClick={() => openRootDialog()}><Plus size={16} />新增曲库</button></div>
                <div class="root-list"><For each={config().library_roots} fallback={<div class="empty-state">尚未添加曲库目录</div>}>{(root) => <article><span class="root-icon"><HardDrive /></span><div><strong>{root.name}</strong><code>{root.path}</code><small class={`root-cache-summary ${root.transcode_cache.enabled ? 'is-active' : ''}`}>{root.transcode_cache.enabled ? `转码缓存 · ${root.transcode_cache.path}` : '转码缓存未启用'}</small></div><span class="status-pill"><span class="pulse-dot" />ENABLED</span><div class="root-list-tools"><button class="icon-button" onClick={() => openRootDialog(root)} aria-label={`编辑曲库 ${root.name}`}><Edit3 size={17} /></button><button class="icon-button danger" onClick={() => void removeRoot(root.id, root.name)} aria-label={`删除曲库 ${root.name}`}><Trash2 size={17} /></button></div></article>}</For></div>
                <div class="scan-console">
                  <div><span class="eyebrow">LIBRARY SCAN</span><h3>{activeJob()?.kind === 'scan' ? '正在建立曲库索引' : '扫描音乐文件与标签'}</h3><p>{activeJob()?.kind === 'scan' ? activeJob()?.message || '扫描任务正在后台运行' : '新增文件或修改标签后，可重新扫描曲库。'}</p></div>
                  <Show when={activeJob()?.kind === 'scan'} fallback={<button class="secondary-button" disabled={busy() === 'scan'} onClick={scan}>{busy() === 'scan' ? <LoaderCircle class="spin" /> : <Play size={16} fill="currentColor" />}立即扫描</button>}>
                    <div class="scan-progress"><strong>{Math.round((activeJob()?.progress || 0) * 100)}%</strong><div><span style={{ width: `${Math.max((activeJob()?.progress || 0) * 100, 2)}%` }} /></div></div>
                  </Show>
                </div>
              </section>

              <section class="panel tool-settings">
                <div class="section-heading compact"><div><span class="eyebrow">RUNTIME TOOLS</span><h2>本机工具</h2></div></div>
                <ToolRow name="FFmpeg / FFprobe" description="音频流与转码" ready={config().tools.ffmpeg} />
                <ToolRow name="Chromaprint / fpcalc" description="AcoustID 音频指纹" ready={config().tools.fpcalc} />
                <ToolRow name="TagLib" description="特殊格式标签回退" ready={true} optional={!config().tools.taglib_configured} />
                <div class="config-note"><CircleAlert size={18} /><p>工具路径、Provider、队列与封面缓存由服务器 <code>config.yaml</code> 管理。封面缓存{config().cover_cache.enabled ? <>已启用：<code>{config().cover_cache.path}</code></> : '未启用'}。</p></div>
              </section>
            </div>

            {renderPlaybackSettings()}

            {renderThemeSettings()}

            <section class="panel radio-settings">
              <div class="section-heading"><div><span class="eyebrow">INTERNET RADIO</span><h2>网络电台</h2></div><div class="section-actions"><span class="count-label">{radioStations().length} 个</span><button class="primary-button small" onClick={() => openRadioDialog()}><Plus size={15} />添加电台</button></div></div>
              <div class="radio-setting-grid"><For each={radioStations()} fallback={<div class="empty-state small">尚未添加网络电台</div>}>{(station) => <article class="radio-setting-card"><span class="radio-setting-icon"><RadioTower /></span><div><div class="radio-setting-title"><strong>{station.name}</strong><span class={`radio-route-badge ${station.proxy ? 'is-proxied' : ''}`}>{station.proxy ? '服务端代理' : '客户端直连'}</span></div><div class="radio-original-url"><span>原始地址</span><code title={station.streamUrl}>{station.streamUrl}</code></div><small>{station.homePageUrl || '未设置电台主页'}</small></div><div class="radio-setting-tools"><Show when={safeHttpUrl(station.homePageUrl)}>{(homepage) => <a class="icon-button" href={homepage()} target="_blank" rel="noreferrer" aria-label={`打开 ${station.name} 主页`}><ExternalLink /></a>}</Show><button class="icon-button" onClick={() => openRadioDialog(station)} aria-label={`编辑 ${station.name}`}><Edit3 /></button><button class="icon-button danger" disabled={busy() === `radio-delete:${station.id}`} onClick={() => void removeRadio(station)} aria-label={`删除 ${station.name}`}>{busy() === `radio-delete:${station.id}` ? <LoaderCircle class="spin" /> : <Trash2 />}</button></div></article>}</For></div>
            </section>

            {renderLastFmSettings(true)}

            <section class="panel download-source-settings">
              <div class="section-heading"><div><span class="eyebrow">REMOTE IMPORT</span><h2>下载来源</h2></div><div class="download-source-actions"><For each={gatewaySourceKinds}>{(kind) => { const project = sourceProjects[kind]!; return <div class="download-source-action-pair"><button class="secondary-button small" onClick={() => openSourceDialog(kind, downloadSources().find((source) => source.kind === kind))}>{sourceActionLabels[kind]}</button><a class="source-project-link" href={project.url} target="_blank" rel="noreferrer" aria-label={`打开 ${sourceNames[kind]} GitHub`} title={project.label}><GitHubMark size={12} /></a></div> }}</For><button class="primary-button small" onClick={() => openSourceDialog('subsonic')}><Plus size={14} />Subsonic</button></div></div>
              <div class="download-filename-setting"><div><strong>远程下载文件名</strong><small>多位艺术家统一以英文逗号加空格连接</small></div><select aria-label="远程下载文件名格式" value={config().download_filename_format} disabled={busy() === 'filename-format'} onChange={(event) => void saveDownloadFilenameFormat(event.currentTarget.value as DownloadFilenameFormat)}><option value="artist-title">歌手 - 歌名.mp3</option><option value="title-artist">歌名 - 歌手.mp3</option></select></div>
              <div class="download-source-grid"><For each={downloadSources()} fallback={<div class="empty-state small">尚未配置远程下载来源</div>}>{(source) => <article class="download-source-card"><span class="download-source-icon"><CloudDownload /></span><div><span class="source-kind">{source.kind.toUpperCase()}</span><strong>{source.name}</strong><code>{source.base_url}</code><small>{source.kind === 'subsonic' ? `${source.username} · ${source.has_password ? '凭据已保存' : '缺少密码'}` : source.kind === 'netease' ? (source.account_name ? `${source.account_name} · 已登录` : source.has_cookie ? '服务端登录信息已保存' : '尚未登录') : '外部下载后端'}</small></div><div class="download-source-tools"><Show when={source.kind === 'netease'}><button class="secondary-button small netease-login-button" disabled={busy() === `netease:${source.id}`} onClick={() => void startNeteaseLogin(source)}>{busy() === `netease:${source.id}` ? <LoaderCircle class="spin" /> : <LogIn size={15} />}{source.account_name || source.has_cookie ? '重新登录' : '登录'}</button></Show><button class="icon-button" onClick={() => openSourceDialog(source.kind, source)} aria-label="编辑来源"><Edit3 /></button><button class="icon-button danger" onClick={() => void removeDownloadSource(source)} aria-label="删除来源"><Trash2 /></button></div></article>}</For></div>
            </section>

            <section class="panel task-history">
              <div class="section-heading"><div><span class="eyebrow">TASK HISTORY</span><h2>任务记录</h2></div><div class="table-actions"><select value={jobFilter()} onChange={(event) => setJobFilter(event.currentTarget.value)}><option value="all">全部状态</option><option value="pending">等待</option><option value="running">执行中</option><option value="completed">完成</option><option value="failed">失败</option></select><button class="icon-button" onClick={() => void loadJobs()}><RefreshCw size={16} /></button></div></div>
              <div class="task-table"><div class="task-head"><span>任务</span><span>状态</span><span>进度</span><span>更新时间</span></div><For each={filteredJobs()} fallback={<div class="empty-state small">没有符合条件的任务</div>}>{(job) => <article><div><span class={`job-state ${job.state}`} /><div><strong>{job.kind === 'scan' ? '曲库扫描' : job.kind === 'auto_tag' ? '自动刮削' : job.kind === 'remote_import' ? '远程下载入库' : job.kind}</strong><small>{job.message || job.id}</small></div></div><b>{stateLabel(job.state)}</b><div class="history-progress"><span><i style={{ width: `${Math.max(job.progress * 100, job.state === 'completed' ? 100 : 2)}%` }} /></span><small>{Math.round(job.progress * 100)}%</small></div><time>{new Date(job.updated_at).toLocaleString('zh-CN')}</time></article>}</For></div>
            </section>
              </>
            )}
          </Show>
        </Show>
      </Show>

      <Show when={rootDialog()}>
        <div class="dialog-layer"><div class="sheet-backdrop" onClick={closeRootDialog} /><section class="dialog library-root-dialog"><header><div><span class="eyebrow">MUSIC FOLDER</span><h2>{editingRootId() ? '编辑曲库目录' : '新增曲库目录'}</h2></div><button class="icon-button" onClick={closeRootDialog}><X /></button></header><form onSubmit={saveRoot}><label class="field wide"><span>曲库名称</span><input value={rootName()} onInput={(event) => setRootName(event.currentTarget.value)} required placeholder="例如：无损音乐" /></label><label class="field wide"><span>服务器绝对路径</span><input value={rootPath()} onInput={(event) => setRootPath(event.currentTarget.value)} required placeholder="例如：/mnt/music" /></label><div class="library-cache-fields"><label class={`transcode-cache-toggle ${rootTranscodeCacheEnabled() ? 'is-active' : ''}`}><input type="checkbox" checked={rootTranscodeCacheEnabled()} onChange={(event) => setRootTranscodeCacheEnabled(event.currentTarget.checked)} aria-label="缓存该曲库的转码结果" /><span class="transcode-cache-check"><Check size={14} /></span><span><strong>缓存转码结果</strong><small>命中相同源文件和参数时直接读取磁盘文件。</small></span></label><label class="transcode-cache-path"><span>该曲库的缓存目录</span><input aria-label="曲库转码缓存路径" value={rootTranscodeCachePath()} onInput={(event) => setRootTranscodeCachePath(event.currentTarget.value)} placeholder="/data/cache/transcodes" spellcheck={false} /></label></div><p class="form-tip">{editingRootId() ? '仅更新目录配置和已有歌曲的绝对路径，不会重新扫描曲库。' : 'Docker 部署时填写容器内部路径，目录必须已挂载且后端可访问。'} 转码临时文件统一写入 <code>/tmp/mnest-transcodes</code>。</p><div class="dialog-actions"><button type="button" class="secondary-button" onClick={closeRootDialog}>取消</button><button class="primary-button" disabled={busy() === 'root' || (rootTranscodeCacheEnabled() && !rootTranscodeCachePath().trim())}>{busy() === 'root' ? <LoaderCircle class="spin" /> : editingRootId() ? <Edit3 size={16} /> : <FolderPlus size={16} />}{editingRootId() ? '保存修改' : '保存目录'}</button></div></form></section></div>
      </Show>

      <Show when={sourceDialog()}>
        <div class="dialog-layer"><div class="sheet-backdrop" onClick={() => setSourceDialog(false)} /><section class="dialog"><header><div><span class="eyebrow">DOWNLOAD SOURCE</span><h2>{sourceDraft().id ? '编辑' : '新增'}{sourceNames[sourceDraft().kind]}</h2></div><button class="icon-button" onClick={() => setSourceDialog(false)}><X /></button></header><form onSubmit={saveDownloadSource}><label class="field wide"><span>来源名称</span><input value={sourceDraft().name} onInput={(event) => setSourceDraft((value) => ({ ...value, name: event.currentTarget.value }))} required /></label><label class="field wide"><span>{sourceDraft().kind === 'subsonic' ? 'Subsonic 服务器地址' : '外部后端地址'}</span><input value={sourceDraft().base_url} onInput={(event) => setSourceDraft((value) => ({ ...value, base_url: event.currentTarget.value }))} required placeholder="https://music.example.com" /></label><Show when={sourceDraft().kind === 'subsonic'}><label class="field wide"><span>用户名</span><input value={sourceDraft().username} onInput={(event) => setSourceDraft((value) => ({ ...value, username: event.currentTarget.value }))} required /></label><label class="field wide"><span>密码</span><input type="password" value={sourceDraft().password} onInput={(event) => setSourceDraft((value) => ({ ...value, password: event.currentTarget.value }))} placeholder={sourceDraft().id ? '留空则保留原密码' : ''} required={!sourceDraft().id} /></label></Show><p class="form-tip">{sourceDraft().kind === 'subsonic' ? '支持添加多个 Subsonic 实例；密码仅保存在 mNest 服务端。' : '填写该音乐源自身的后端根地址。'}</p><Show when={sourceProjects[sourceDraft().kind]}>{(project) => <a class="source-dialog-project" href={project().url} target="_blank" rel="noreferrer"><GitHubMark size={12} /><span>{project().label}</span></a>}</Show><div class="dialog-actions"><button type="button" class="secondary-button" onClick={() => setSourceDialog(false)}>取消</button><button class="primary-button" disabled={busy() === 'download-source'}>{busy() === 'download-source' ? <LoaderCircle class="spin" /> : <ServerCog size={16} />}保存来源</button></div></form></section></div>
      </Show>

      <Show when={radioDialog()}>
        <div class="dialog-layer"><div class="sheet-backdrop" onClick={() => setRadioDialog(false)} /><section class="dialog"><header><div><span class="eyebrow">INTERNET RADIO</span><h2>{radioDraft().id ? '编辑网络电台' : '添加网络电台'}</h2></div><button class="icon-button" onClick={() => setRadioDialog(false)}><X /></button></header><form onSubmit={saveRadio}><label class="field wide"><span>电台名称</span><input value={radioDraft().name} maxlength={256} onInput={(event) => setRadioDraft((value) => ({ ...value, name: event.currentTarget.value }))} required placeholder="例如：BBC Radio 6 Music" /></label><label class="field wide"><span>音频流地址</span><input type="text" inputmode="url" aria-label="音频流地址" value={radioDraft().streamUrl} maxlength={4096} onInput={(event) => setRadioDraft((value) => ({ ...value, streamUrl: event.currentTarget.value }))} required placeholder="https://、rtsp:// 或 mms://" /><small>支持 HTTP、HTTPS、RTSP、MMS、MMSH 和 MMST；非 HTTP 流由服务端转为 MP3。</small></label><label class="field wide"><span>电台主页（可选）</span><input type="url" value={radioDraft().homePageUrl} maxlength={4096} onInput={(event) => setRadioDraft((value) => ({ ...value, homePageUrl: event.currentTarget.value }))} placeholder="https://radio.example.com" /></label><label class={`radio-proxy-toggle ${radioDraft().proxy ? 'is-active' : ''}`}><input type="checkbox" checked={radioDraft().proxy} onChange={(event) => setRadioDraft((value) => ({ ...value, proxy: event.currentTarget.checked }))} aria-label="OpenSubsonic 服务端代理" /><span class="radio-proxy-check"><Check size={13} /></span><span><strong>OpenSubsonic 服务端代理</strong><small>第三方客户端获取 mNest 签名代理地址，由服务器连接并转发电台流。</small></span></label><p class="form-tip">关闭时 OpenSubsonic 返回原始流地址，由客户端直连；mNest 网页播放器始终使用服务端代理。RTSP、MMS 等协议建议开启代理。反向代理部署请正确配置 <code>server.public_url</code>。</p><div class="dialog-actions"><button type="button" class="secondary-button" onClick={() => setRadioDialog(false)}>取消</button><button class="primary-button" disabled={!radioDraft().name.trim() || !radioDraft().streamUrl.trim() || busy() === 'radio-save'}>{busy() === 'radio-save' ? <LoaderCircle class="spin" /> : <RadioTower size={16} />}保存电台</button></div></form></section></div>
      </Show>

      <Show when={neteaseLogin()}>{(login) => <div class="dialog-layer"><div class="sheet-backdrop" onClick={closeNeteaseLogin} /><section class="dialog netease-login-dialog"><header><div><span class="eyebrow">NETEASE LOGIN</span><h2>网易云扫码登录</h2></div><button class="icon-button" onClick={closeNeteaseLogin}><X /></button></header><img src={login().qr_image} alt="网易云登录二维码" /><p>{login().message}</p></section></div>}</Show>
    </div>
  )
}

function StatusCard(props: { icon: typeof Database; label: string; value: string; detail: string; accent?: boolean; warning?: boolean }) {
  return <article class={`status-card ${props.accent ? 'accent' : ''} ${props.warning ? 'warning' : ''}`}><div><props.icon /><span>{props.label}</span></div><strong>{props.value}</strong><small>{props.detail}</small></article>
}

function ToolRow(props: { name: string; description: string; ready: boolean; optional?: boolean }) {
  return <article class="tool-row"><span class={`tool-mark ${props.ready ? 'ready' : 'missing'}`}>{props.ready ? <Check /> : <X />}</span><div><strong>{props.name}</strong><small>{props.description}</small></div><b class={props.ready ? 'ready' : 'missing'}>{props.optional ? 'AUTO DETECT' : props.ready ? 'READY' : 'MISSING'}</b></article>
}

function stateLabel(state: JobRecord['state']) {
  return { pending: '等待', running: '执行中', completed: '完成', failed: '失败' }[state]
}

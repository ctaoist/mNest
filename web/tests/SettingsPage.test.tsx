import { fireEvent, render, screen, waitFor } from '@solidjs/testing-library'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { SettingsPage } from '../src/pages/SettingsPage'

const mocks = vi.hoisted(() => ({
  role: 'user',
  get: vi.fn(),
  request: vi.fn(),
  post: vi.fn(),
  subsonic: vi.fn(),
  notify: vi.fn(),
}))

vi.mock('../src/context/auth', () => ({
  useAuth: () => ({
    loading: () => false,
    user: () => ({ username: mocks.role === 'admin' ? 'admin' : 'listener', role: mocks.role }),
  }),
}))

vi.mock('../src/context/theme', () => ({
  useTheme: () => ({ theme: () => 'minimal', setTheme: vi.fn() }),
}))

vi.mock('../src/context/toast', () => ({
  useToast: () => ({ notify: mocks.notify }),
}))

vi.mock('../src/lib/api', () => ({
  get: mocks.get,
  post: mocks.post,
  request: mocks.request,
  subscribeJobs: vi.fn(() => () => undefined),
  subsonic: mocks.subsonic,
}))

const lastfm = {
  configured: true,
  connected: false,
  authorization_pending: false,
  username: '',
  api_key: '0123456789abcdef0123456789abcdef',
  has_shared_secret: true,
}

describe('SettingsPage permissions', () => {
  beforeEach(() => {
    mocks.get.mockImplementation((path: string) => {
      if (path === '/api/lastfm/status/') return Promise.resolve(lastfm)
      if (path === '/api/download_sources/') return Promise.resolve([])
      if (path === '/api/internet_radio_stations/') return Promise.resolve([])
      if (path === '/api/config/status/') return Promise.resolve({
        database: 'sqlite',
        queue: 'database',
        library_roots: [{ id: 'root-1', name: 'Music', path: '/music', enabled: 1 }],
        providers: [],
        download_filename_format: 'artist-title',
        cover_cache: { enabled: true, path: '/data/cache/covers' },
        lastfm,
        tools: { ffmpeg: true, fpcalc: true, taglib_configured: false },
      })
      throw new Error(`unexpected GET ${path}`)
    })
    mocks.request.mockResolvedValue({ status: 'ok', version: 'test' })
    mocks.post.mockResolvedValue({})
    mocks.subsonic.mockResolvedValue({ internetRadioStations: {} })
  })

  afterEach(() => {
    mocks.role = 'user'
    vi.clearAllMocks()
  })

  it('shows only personal settings to a regular user', async () => {
    mocks.role = 'user'
    render(() => <SettingsPage />)

    await waitFor(() => expect(screen.getByText('Last.fm')).toBeTruthy())
    expect(screen.getByText('界面主题')).toBeTruthy()
    expect(screen.queryByText('曲库目录')).toBeNull()
    expect(screen.queryByPlaceholderText('Last.fm API Key')).toBeNull()
    expect(mocks.get).not.toHaveBeenCalledWith('/api/config/status/')
    expect(mocks.get).not.toHaveBeenCalledWith('/api/download_sources/')
  })

  it('shows administrative and personal settings to an administrator', async () => {
    mocks.role = 'admin'
    render(() => <SettingsPage />)

    await waitFor(() => expect(screen.getByText('曲库目录')).toBeTruthy())
    expect(screen.getByPlaceholderText('Last.fm API Key')).toBeTruthy()
    expect(mocks.get).toHaveBeenCalledWith('/api/config/status/')
    expect(mocks.get).toHaveBeenCalledWith('/api/download_sources/')
    expect(screen.getByRole('link', { name: '打开 网易云音乐 GitHub' })).toHaveAttribute('href', 'https://github.com/NeteaseCloudMusicApiEnhanced/api-enhanced')
    expect(screen.getByRole('link', { name: '打开 QQ 音乐 GitHub' })).toHaveAttribute('href', 'https://github.com/jsososo/QQMusicApi')
    expect(screen.getByRole('link', { name: '打开 QQ 音乐 2 GitHub' })).toHaveAttribute('href', 'https://github.com/Rain120/qq-music-api')
  })

  it('edits a library root without requesting a scan', async () => {
    mocks.role = 'admin'
    render(() => <SettingsPage />)
    await waitFor(() => expect(screen.getByRole('button', { name: '编辑曲库 Music' })).toBeTruthy())

    await fireEvent.click(screen.getByRole('button', { name: '编辑曲库 Music' }))
    expect(screen.getByText('编辑曲库目录')).toBeTruthy()
    expect(screen.getByLabelText('曲库名称')).toHaveValue('Music')
    expect(screen.getByLabelText('服务器绝对路径')).toHaveValue('/music')
    await fireEvent.input(screen.getByLabelText('服务器绝对路径'), { target: { value: '/mnt/music' } })
    await fireEvent.click(screen.getByRole('button', { name: '保存修改' }))

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith('/api/library_roots/update/', {
      id: 'root-1', name: 'Music', path: '/mnt/music',
    }))
    expect(mocks.post).not.toHaveBeenCalledWith('/api/scan/', expect.anything())
    expect(mocks.notify).toHaveBeenCalledWith('曲库目录已更新，不会自动扫描', 'success')
  })

  it('saves the OpenSubsonic proxy option for an internet radio', async () => {
    mocks.role = 'admin'
    render(() => <SettingsPage />)
    await waitFor(() => expect(screen.getByRole('button', { name: '添加电台' })).toBeTruthy())

    await fireEvent.click(screen.getByRole('button', { name: '添加电台' }))
    await fireEvent.input(screen.getByLabelText('电台名称'), { target: { value: '代理电台' } })
    await fireEvent.input(screen.getByLabelText('音频流地址'), { target: { value: 'https://radio.example/live' } })
    await fireEvent.click(screen.getByLabelText('OpenSubsonic 服务端代理'))
    await fireEvent.click(screen.getByRole('button', { name: '保存电台' }))

    await waitFor(() => expect(mocks.subsonic).toHaveBeenCalledWith('createInternetRadioStation', {
      id: undefined,
      name: '代理电台',
      streamUrl: 'https://radio.example/live',
      homepageUrl: '',
      proxy: true,
    }))
  })
})

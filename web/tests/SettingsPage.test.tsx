import { fireEvent, render, screen, waitFor } from '@solidjs/testing-library'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { SettingsPage } from '../src/pages/SettingsPage'

const mocks = vi.hoisted(() => ({
  role: 'user',
  get: vi.fn(),
  request: vi.fn(),
  post: vi.fn(),
  del: vi.fn(),
  subsonic: vi.fn(),
  notify: vi.fn(),
  bitrate: 0,
  saveBitrate: vi.fn(),
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

vi.mock('../src/context/preferences', () => ({
  WEB_PLAYBACK_BITRATES: [0, 64, 96, 128, 192, 256, 320],
  usePreferences: () => ({
    webPlaybackBitrate: () => mocks.bitrate,
    loading: () => false,
    saveWebPlaybackBitrate: mocks.saveBitrate,
  }),
}))

vi.mock('../src/context/toast', () => ({
  useToast: () => ({ notify: mocks.notify }),
}))

vi.mock('../src/lib/api', () => ({
  get: mocks.get,
  post: mocks.post,
  del: mocks.del,
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
    mocks.bitrate = 0
    mocks.saveBitrate.mockImplementation(async (value: number) => { mocks.bitrate = value })
    mocks.get.mockImplementation((path: string) => {
      if (path === '/api/lastfm/status/') return Promise.resolve(lastfm)
      if (path === '/api/user/subsonic-api-key/') return Promise.resolve({ api_key: '0123456789abcdef0123456789abcdef', enabled: true })
      if (path === '/api/download_sources/') return Promise.resolve([])
      if (path === '/api/internet_radio_stations/') return Promise.resolve([])
      if (path === '/api/config/status/') return Promise.resolve({
        database: 'sqlite',
        queue: 'database',
        library_roots: [{ id: 'root-1', name: 'Music', path: '/music', enabled: 1, transcode_cache: { enabled: false, path: '/data/cache/transcodes' } }],
        providers: [],
        download_filename_format: 'artist-title',
        cover_cache: { enabled: true, path: '/data/cache/covers', concurrency: 4 },
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
    expect(screen.getByText('OpenSubsonic API Key')).toBeTruthy()
    expect(screen.getByLabelText('网页端播放码率')).toHaveValue('0')
    expect(screen.queryByText('曲库目录')).toBeNull()
    expect(screen.queryByPlaceholderText('Last.fm API Key')).toBeNull()
    expect(mocks.get).not.toHaveBeenCalledWith('/api/config/status/')
    expect(mocks.get).not.toHaveBeenCalledWith('/api/download_sources/')
  })

  it('saves the web playback bitrate as a personal preference', async () => {
    render(() => <SettingsPage />)
    await waitFor(() => expect(screen.getByLabelText('网页端播放码率')).toBeTruthy())

    await fireEvent.change(screen.getByLabelText('网页端播放码率'), { target: { value: '128' } })

    await waitFor(() => expect(mocks.saveBitrate).toHaveBeenCalledWith(128))
    expect(mocks.notify).toHaveBeenCalledWith('网页播放码率已保存，将从下一次播放开始生效', 'success')
  })

  it('rotates the personal OpenSubsonic API key', async () => {
    mocks.post.mockResolvedValue({ api_key: 'fedcba9876543210fedcba9876543210', enabled: true })
    vi.spyOn(window, 'confirm').mockReturnValue(true)
    render(() => <SettingsPage />)
    await waitFor(() => expect(screen.getByRole('button', { name: '轮换' })).toBeTruthy())

    await fireEvent.click(screen.getByRole('button', { name: '轮换' }))

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith('/api/user/subsonic-api-key/', {}))
    expect(screen.getByText('fedcba9876543210fedcba9876543210')).toBeTruthy()
  })

  it('masks the personal OpenSubsonic API key until explicitly revealed', async () => {
    render(() => <SettingsPage />)
    await waitFor(() => expect(screen.getByRole('button', { name: '显示 OpenSubsonic API Key' })).toBeTruthy())

    expect(screen.queryByText('0123456789abcdef0123456789abcdef')).toBeNull()
    await fireEvent.click(screen.getByRole('button', { name: '显示 OpenSubsonic API Key' }))

    expect(screen.getByText('0123456789abcdef0123456789abcdef')).toBeTruthy()
    expect(screen.getByRole('button', { name: '隐藏 OpenSubsonic API Key' })).toBeTruthy()
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
      transcode_cache: { enabled: false, path: '/data/cache/transcodes' },
    }))
    expect(mocks.post).not.toHaveBeenCalledWith('/api/scan/', expect.anything())
    expect(mocks.notify).toHaveBeenCalledWith('曲库目录已更新，不会自动扫描', 'success')
  })

  it('saves transcode cache settings with the edited library root', async () => {
    mocks.role = 'admin'
    render(() => <SettingsPage />)
    await waitFor(() => expect(screen.getByRole('button', { name: '编辑曲库 Music' })).toBeTruthy())

    await fireEvent.click(screen.getByRole('button', { name: '编辑曲库 Music' }))
    await fireEvent.click(screen.getByLabelText('缓存该曲库的转码结果'))
    await fireEvent.input(screen.getByLabelText('曲库转码缓存路径'), { target: { value: '/mnt/cache/transcodes' } })
    await fireEvent.click(screen.getByRole('button', { name: '保存修改' }))

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith('/api/library_roots/update/', {
      id: 'root-1',
      name: 'Music',
      path: '/music',
      transcode_cache: { enabled: true, path: '/mnt/cache/transcodes' },
    }))
  })

  it('saves an RTSP radio with its cover and OpenSubsonic proxy option', async () => {
    mocks.role = 'admin'
    render(() => <SettingsPage />)
    await waitFor(() => expect(screen.getByRole('button', { name: '添加电台' })).toBeTruthy())

    await fireEvent.click(screen.getByRole('button', { name: '添加电台' }))
    await fireEvent.input(screen.getByLabelText('电台名称'), { target: { value: '代理电台' } })
    await fireEvent.input(screen.getByLabelText('音频流地址'), { target: { value: 'rtsp://radio.example/live' } })
    await fireEvent.input(screen.getByLabelText('封面链接'), { target: { value: 'https://radio.example/cover.png' } })
    await fireEvent.click(screen.getByLabelText('OpenSubsonic 服务端代理'))
    await fireEvent.click(screen.getByRole('button', { name: '保存电台' }))

    await waitFor(() => expect(mocks.subsonic).toHaveBeenCalledWith('createInternetRadioStation', {
      id: undefined,
      name: '代理电台',
      streamUrl: 'rtsp://radio.example/live',
      homepageUrl: '',
      coverUrl: 'https://radio.example/cover.png',
      proxy: true,
    }))
  })
})

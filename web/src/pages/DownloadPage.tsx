import { Navigate } from '@solidjs/router'
import { createEffect, createMemo, createSignal, For, onCleanup, onMount, Show } from 'solid-js'
import { Check, CloudDownload, Download, FolderDown, LoaderCircle, Pause, Play, RefreshCw, Search, ServerCog, Upload } from 'lucide-solid'
import { useAuth } from '../context/auth'
import { usePlayer } from '../context/player'
import { useToast } from '../context/toast'
import { get, management, post, subscribeJobs } from '../lib/api'
import type { DownloadSource, JobRecord, LibraryRoot, RemoteDownloadSong } from '../types'

export function DownloadPage() {
  const auth = useAuth()
  const player = usePlayer()
  const toast = useToast()
  const [sources, setSources] = createSignal<DownloadSource[]>([])
  const [roots, setRoots] = createSignal<LibraryRoot[]>([])
  const [selectedSourceIds, setSelectedSourceIds] = createSignal<string[]>([])
  const [rootId, setRootId] = createSignal('')
  const [directory, setDirectory] = createSignal('')
  const [query, setQuery] = createSignal('')
  const [results, setResults] = createSignal<RemoteDownloadSong[]>([])
  const [jobs, setJobs] = createSignal<JobRecord[]>([])
  const [busy, setBusy] = createSignal('')
  let sourcesInitialized = false
  let lastPreviewError = ''
  let closeJobEvents: () => void = () => undefined
  let uploadInput!: HTMLInputElement

  if (!auth.loading() && auth.user()?.role !== 'admin') return <Navigate href="/player" />

  const selectedSources = createMemo(() => {
    const selected = new Set(selectedSourceIds())
    return sources().filter((source) => selected.has(source.id))
  })
  const importJobs = createMemo(() => jobs().filter((job) => job.kind === 'remote_import'))

  createEffect(() => {
    const error = player.error()
    const remotePreview = player.current()?.id.startsWith('remote-preview:')
    if (!error || !remotePreview) {
      if (!error) lastPreviewError = ''
      return
    }
    if (error !== lastPreviewError) {
      lastPreviewError = error
      toast.notify(error, 'error')
    }
  })

  const load = async () => {
    try {
      const [sourceData, rootData] = await Promise.all([
        get<DownloadSource[]>('/api/download_sources/'),
        get<LibraryRoot[]>('/api/library_roots/'),
      ])
      const enabledSources = sourceData.filter((source) => source.enabled)
      setSources(enabledSources)
      setRoots(rootData)
      setSelectedSourceIds((selected) => {
        if (!sourcesInitialized) {
          sourcesInitialized = true
          return enabledSources.map((source) => source.id)
        }
        const enabledIds = new Set(enabledSources.map((source) => source.id))
        return selected.filter((id) => enabledIds.has(id))
      })
      if (!rootId() && rootData[0]) setRootId(rootData[0].id)
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '下载工作台加载失败', 'error')
    }
  }

  onMount(() => {
    void load()
    closeJobEvents = subscribeJobs(30, setJobs, (message) => toast.notify(message, 'error'))
  })
  onCleanup(() => closeJobEvents())

  const loadJobs = async () => {
    try {
      const data = await get<{ items: JobRecord[] }>('/api/record/?page_size=30')
      setJobs(data.items)
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '下载任务加载失败', 'error')
    }
  }

  const toggleSource = (id: string, checked: boolean) => {
    setSelectedSourceIds((selected) => checked
      ? selected.includes(id) ? selected : [...selected, id]
      : selected.filter((sourceId) => sourceId !== id))
    setResults([])
  }

  const search = async (event: SubmitEvent) => {
    event.preventDefault()
    const selected = selectedSources()
    if (!selected.length) return toast.notify('请至少选择一个搜索来源', 'error')
    if (!query().trim()) return toast.notify('请输入歌曲、艺术家或专辑', 'error')
    setBusy('search')
    try {
      type SearchSong = Omit<RemoteDownloadSong, 'source_id' | 'source_name'>
      const responses = await Promise.allSettled(selected.map(async (source) => {
        const songs = await post<SearchSong[]>('/api/remote_download/search/', { source_id: source.id, query: query().trim() })
        return songs.map((song): RemoteDownloadSong => ({ ...song, source_id: source.id, source_name: source.name }))
      }))
      const songs: RemoteDownloadSong[] = []
      const failures: string[] = []
      responses.forEach((response, index) => {
        if (response.status === 'fulfilled') {
          songs.push(...response.value)
        } else {
          const reason = response.reason instanceof Error ? response.reason.message : '搜索失败'
          failures.push(`${selected[index].name}: ${reason}`)
        }
      })
      setResults(songs)
      if (failures.length) {
        const omitted = failures.length > 2 ? `；另有 ${failures.length - 2} 个来源失败` : ''
        toast.notify(`${failures.slice(0, 2).join('；')}${omitted}`, songs.length ? 'info' : 'error')
      } else if (!songs.length) {
        toast.notify('所选来源均没有找到歌曲', 'info')
      }
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '远程搜索失败', 'error')
    } finally {
      setBusy('')
    }
  }

  const importSong = async (song: RemoteDownloadSong, quality: string) => {
    if (!rootId()) return toast.notify('请先选择目标曲库', 'error')
    const key = `${song.source_id}:${song.id}:${quality}`
    setBusy(key)
    try {
      const data = await post<{ job_id: string }>('/api/remote_download/import/', {
        source_id: song.source_id, song, quality, root_id: rootId(), directory: directory().trim(),
      })
      toast.notify(`入库任务已创建：${data.job_id.slice(0, 8)}`, 'success')
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '下载入库任务创建失败', 'error')
    } finally {
      setBusy('')
    }
  }

  const uploadSongs = async (event: Event & { currentTarget: HTMLInputElement }) => {
    const files = Array.from(event.currentTarget.files || [])
    event.currentTarget.value = ''
    if (!files.length) return
    if (!rootId()) return toast.notify('请先选择目标曲库', 'error')
    const targetRootId = rootId()
    const targetDirectory = directory().trim()
    setBusy('upload')
    let uploaded = 0
    const failures: string[] = []
    for (const file of files) {
      if (file.size > 2 * 1024 * 1024 * 1024) {
        failures.push(`${file.name}: 超过 2GiB 限制`)
        continue
      }
      const form = new FormData()
      form.append('upload_file', file, file.name)
      const search = new URLSearchParams({ root_id: targetRootId, directory: targetDirectory })
      try {
        await management<{ path: string; filename: string }>(`/api/remote_download/upload/?${search.toString()}`, { method: 'POST', body: form })
        uploaded += 1
      } catch (error) {
        failures.push(`${file.name}: ${error instanceof Error ? error.message : '上传失败'}`)
      }
    }
    setBusy('')
    if (uploaded) toast.notify(`${uploaded} 首歌曲已上传并加入曲库`, 'success')
    if (failures.length) {
      const omitted = failures.length > 2 ? `；另有 ${failures.length - 2} 首失败` : ''
      toast.notify(`${failures.slice(0, 2).join('；')}${omitted}`, 'error')
    }
  }

  const previewId = (song: RemoteDownloadSong) => `remote-preview:${song.source_id}:${song.id}`

  const previewSong = (song: RemoteDownloadSong) => {
    const id = previewId(song)
    if (player.current()?.id === id) {
      player.toggle()
      return
    }
    const search = new URLSearchParams({ source_id: song.source_id, song_id: song.id })
    player.playStream({
      id,
      title: song.title,
      artists: song.artists.map((name, index) => ({ id: `${id}:artist:${index}`, name })),
      album: song.album,
      duration: 0,
      bitRate: 128,
      suffix: 'mp3',
      streamUrl: `/api/remote_download/preview/?${search.toString()}`,
    })
  }

  return (
    <div class="page download-page">
      <header class="download-page-toolbar">
        <p class="eyebrow">SERVER-SIDE IMPORT</p>
        <form class="download-search" onSubmit={search}>
          <div class="download-source-options" role="group" aria-label="搜索来源">
            <For each={sources()} fallback={<span class="download-source-empty">尚未配置来源</span>}>{(source) => {
              const selected = () => selectedSourceIds().includes(source.id)
              return <label class="download-source-checkbox" classList={{ 'is-selected': selected() }} title={`${source.name} · ${source.kind.toUpperCase()}`}><input type="checkbox" checked={selected()} onChange={(event) => toggleSource(source.id, event.currentTarget.checked)} /><span class="download-source-check"><Check size={12} /></span><span class={`download-source-dot source-dot-${source.kind}`} /><span>{source.name}</span></label>
            }}</For>
          </div>
          <label class="download-query"><Search size={17} /><input value={query()} onInput={(event) => setQuery(event.currentTarget.value)} placeholder="搜索歌曲、艺术家或专辑" /></label>
          <button class="primary-button" disabled={busy() === 'search' || !selectedSources().length}>{busy() === 'search' ? <LoaderCircle class="spin" /> : <Search size={16} />}搜索</button>
        </form>
      </header>

      <section class="panel download-destination">
        <div class="download-destination-intro"><span class="download-destination-icon"><FolderDown /></span><div><strong>服务端入库位置</strong><small>下载或上传完成后自动扫描并加入播放器曲库</small></div></div>
        <label><span>目标曲库</span><select value={rootId()} onChange={(event) => setRootId(event.currentTarget.value)}><For each={roots()}>{(root) => <option value={root.id}>{root.name} · {root.path}</option>}</For></select></label>
        <label><span>曲库内目录</span><input value={directory()} onInput={(event) => setDirectory(event.currentTarget.value)} placeholder="留空则保存到曲库根目录" /></label>
        <div class="download-upload-control"><span>本地歌曲</span><input ref={uploadInput} hidden type="file" multiple accept="audio/*,.aac,.flac,.mp3,.mp2,.ape,.wav,.aiff,.aif,.wv,.tta,.m4a,.mp4,.ogg,.mpc,.opus,.wma,.wmv,.dsf,.dff,.spx" onChange={uploadSongs} /><button type="button" class="secondary-button" disabled={!rootId() || !!busy()} onClick={() => uploadInput.click()}>{busy() === 'upload' ? <LoaderCircle class="spin" /> : <Upload size={16} />}上传歌曲</button></div>
      </section>

      <section class="panel download-results">
        <div class="section-heading compact"><div><span class="eyebrow">REMOTE CATALOGUE</span><div class="section-title">{selectedSources().length > 1 ? '多来源搜索结果' : selectedSources()[0]?.name || '远程歌曲'}</div></div><span class="count-label">{results().length} 条结果</span></div>
        <Show when={sources().length} fallback={<div class="empty-state"><ServerCog /><span>请先到系统设置配置网易云、QQ、QQ2 或 Subsonic 下载来源</span></div>}>
          <For each={results()} fallback={<div class="empty-state"><CloudDownload /><span>选择来源并搜索后，可将外部歌曲下载到服务器曲库</span></div>}>
            {(song) => <article class="remote-song-row"><span class={`remote-source-mark source-${song.source}`} title={song.source_name}>{song.source === 'netease' ? '163' : song.source === 'subsonic' ? 'SUB' : song.source.toUpperCase()}</span><div class="remote-song-main"><strong>{song.title}</strong><span>{song.artists.join('; ') || 'Unknown Artist'}</span></div><div class="remote-song-album"><span>{song.album || '未知专辑'}</span><small>{song.source_name} · {song.suffix?.toUpperCase() || 'AUDIO'}{song.bit_rate ? ` · ${song.bit_rate}k` : ''}</small></div><div class="remote-quality-actions"><button class="remote-preview-button" classList={{ 'is-active': player.current()?.id === previewId(song) }} onClick={() => previewSong(song)}>{player.current()?.id === previewId(song) && player.playing() ? <Pause size={14} fill="currentColor" /> : <Play size={14} fill="currentColor" />}128k 试听</button><For each={song.qualities}>{(quality) => { const key = () => `${song.source_id}:${song.id}:${quality.id}`; return <button disabled={!!busy()} onClick={() => void importSong(song, quality.id)}>{busy() === key() ? <LoaderCircle class="spin" /> : <Download size={14} />}{quality.label}</button> }}</For></div></article>}
          </For>
        </Show>
      </section>

      <Show when={importJobs().length}><section class="panel download-jobs"><div class="section-heading compact"><div><span class="eyebrow">IMPORT QUEUE</span><div class="section-title">最近下载任务</div></div><button class="icon-button" onClick={() => void loadJobs()}><RefreshCw /></button></div><For each={importJobs().slice(0, 8)}>{(job) => <article><span class={`job-state ${job.state}`} /><div><strong>{job.message || '等待服务端下载'}</strong><small>{job.id.slice(0, 8)} · {new Date(job.updated_at).toLocaleString('zh-CN')}</small></div><div class="download-job-progress"><span style={{ width: `${Math.max(job.progress * 100, 2)}%` }} /></div><b>{Math.round(job.progress * 100)}%</b></article>}</For></section></Show>
    </div>
  )
}

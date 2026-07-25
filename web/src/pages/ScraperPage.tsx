import { createMemo, createSignal, For, onCleanup, onMount, Show } from 'solid-js'
import { Navigate } from '@solidjs/router'
import {
  ArrowLeft,
  ChevronRight,
  Disc3,
  FileAudio,
  Folder,
  FolderInput,
  ImageIcon,
  ImagePlus,
  LoaderCircle,
  RefreshCw,
  Save,
  Search,
  Settings2,
  Sparkles,
  WandSparkles,
  X,
} from 'lucide-solid'
import { useToast } from '../context/toast'
import { useAuth } from '../context/auth'
import { get, management, post, subscribeJobs } from '../lib/api'
import { filterScraperEntries, type ScraperFileScope } from '../lib/scraper'
import { formatBytes, formatDuration, joinPath, normalizeArtistMetadata, parentPath } from '../lib/utils'
import type { AudioMetadata, FileNode, JobRecord, LibraryRoot, MetadataCandidate } from '../types'

const providers = [
  ['netease', '网易云'], ['qmusic', 'QQ 音乐'], ['migu', '咪咕'],
  ['kuwo', '酷我'], ['kugou', '酷狗'], ['acoustid', '指纹识别'],
] as const

const emptyMetadata: AudioMetadata = {
  title: '', artist: '', album: '', albumartist: '', genre: '', year: '', language: '', lyrics: '',
  comment: '', tracknumber: '', discnumber: '', duration: 0, bit_rate: 0, size: 0, suffix: '',
  filename: '', file_full_path: '', album_img: '', artwork_mime: '', artwork_w: 0, artwork_h: 0,
  artwork_size: 0, is_save_lyrics_file: false, is_save_album_cover: false, needs_scrape: false,
}

export function ScraperPage() {
  const auth = useAuth()
  if (!auth.loading() && auth.user()?.role !== 'admin') return <Navigate href="/player" />
  const toast = useToast()
  const [roots, setRoots] = createSignal<LibraryRoot[]>([])
  const [path, setPath] = createSignal('')
  const [entries, setEntries] = createSignal<FileNode[]>([])
  const [filter, setFilter] = createSignal('')
  const [fileScope, setFileScope] = createSignal<ScraperFileScope>('needs_scrape')
  const [filterMenuOpen, setFilterMenuOpen] = createSignal(false)
  const [sort, setSort] = createSignal<string[]>([])
  const [selected, setSelected] = createSignal<Set<string>>(new Set())
  const [activeFile, setActiveFile] = createSignal('')
  const [draft, setDraft] = createSignal<AudioMetadata>({ ...emptyMetadata })
  const [provider, setProvider] = createSignal('netease')
  const [candidates, setCandidates] = createSignal<MetadataCandidate[]>([])
  const [busy, setBusy] = createSignal('')
  const [artworkLoading, setArtworkLoading] = createSignal('')
  const [embeddedArtworkUrl, setEmbeddedArtworkUrl] = createSignal('')
  const [jobs, setJobs] = createSignal<JobRecord[]>([])
  const [mobileStep, setMobileStep] = createSignal<'files' | 'edit' | 'match'>('files')
  const [batchOpen, setBatchOpen] = createSignal(false)
  const [tidyOpen, setTidyOpen] = createSignal(false)
  const [batchSources, setBatchSources] = createSignal<string[]>(['netease', 'qmusic'])
  const [batchMode, setBatchMode] = createSignal('hard')
  const [tidyFirst, setTidyFirst] = createSignal('${artist}')
  const [tidySecond, setTidySecond] = createSignal('${album}')
  const metadataCache = new Map<string, AudioMetadata>()
  let metadataRequestId = 0
  let closeJobEvents: () => void = () => undefined

  const visibleEntries = createMemo(() => filterScraperEntries(entries(), fileScope(), filter()))
  const selectedData = createMemo(() => entries().filter((entry) => selected().has(entry.name)).map((entry) => ({ name: entry.name, icon: entry.icon })))
  const loadJobs = async () => {
    try {
      const response = await get<{ items: JobRecord[] }>('/api/record/?page_size=30')
      setJobs(response.items)
    } catch {
      // The workbench remains usable if task history cannot be loaded.
    }
  }

  const loadDirectory = async (nextPath = path()) => {
    if (!nextPath) return
    metadataRequestId += 1
    setArtworkLoading('')
    setEmbeddedArtworkUrl('')
    setBusy('files')
    try {
      const data = await post<Array<{ children: FileNode[] }>>('/api/file_list/', { file_path: nextPath, sorted_fields: sort() })
      setPath(nextPath)
      setEntries(data[0]?.children || [])
      setSelected(new Set<string>())
      setActiveFile('')
      setDraft({ ...emptyMetadata })
      setCandidates([])
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '目录读取失败', 'error')
    } finally {
      setBusy('')
    }
  }

  onMount(async () => {
    try {
      const data = await get<LibraryRoot[]>('/api/library_roots/')
      setRoots(data)
      if (data[0]) await loadDirectory(data[0].path)
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '曲库目录加载失败', 'error')
    }
    closeJobEvents = subscribeJobs(30, setJobs)
  })
  onCleanup(() => closeJobEvents())

  const showArtwork = (entry: FileNode, directory: string, requestId: number) => {
    if (requestId !== metadataRequestId || activeFile() !== entry.name) return
    const search = new URLSearchParams({
      file_path: directory,
      file_name: entry.name,
      v: `${entry.size}:${entry.update_time}`,
    })
    setArtworkLoading(entry.name)
    setEmbeddedArtworkUrl(`/api/music_artwork/?${search.toString()}`)
  }

  const openEntry = async (entry: FileNode) => {
    if (entry.icon === 'icon-folder') return loadDirectory(joinPath(path(), entry.name))
    const requestId = ++metadataRequestId
    const directory = path()
    const cacheKey = `${joinPath(directory, entry.name)}:${entry.size}:${entry.update_time}`
    setActiveFile(entry.name)
    setCandidates([])
    setMobileStep('edit')
    setArtworkLoading('')
    setEmbeddedArtworkUrl('')

    const cached = metadataCache.get(cacheKey)
    if (cached) {
      setDraft({ ...cached, is_save_album_cover: false, is_save_lyrics_file: false })
      setBusy('')
      showArtwork(entry, directory, requestId)
      return
    }

    setDraft(metadataPlaceholder(entry, directory))
    setBusy('metadata')
    try {
      const metadata = await post<AudioMetadata>('/api/music_id3/', {
        file_path: directory,
        file_name: entry.name,
      })
      metadataCache.set(cacheKey, metadata)
      if (requestId !== metadataRequestId || activeFile() !== entry.name) return
      setDraft({ ...metadata, is_save_album_cover: false, is_save_lyrics_file: false })
      showArtwork(entry, directory, requestId)
    } catch (error) {
      if (requestId === metadataRequestId) {
        toast.notify(error instanceof Error ? error.message : '标签读取失败', 'error')
      }
    } finally {
      if (requestId === metadataRequestId) setBusy('')
    }
  }

  const toggleSelected = (entry: FileNode) => {
    setSelected((current) => {
      const next = new Set(current)
      next.has(entry.name) ? next.delete(entry.name) : next.add(entry.name)
      return next
    })
  }

  const save = async () => {
    if (!activeFile()) return
    setBusy('save')
    try {
      const next = { ...draft(), file_full_path: joinPath(path(), activeFile()) }
      await post('/api/update_id3/', { music_id3_info: [next] })
      metadataCache.clear()
      toast.notify('标签已写入音频文件', 'success')
      await loadDirectory(path())
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '保存失败', 'error')
    } finally {
      setBusy('')
    }
  }

  const searchCandidates = async () => {
    if (!draft().title) return toast.notify('请先填写歌曲标题', 'error')
    setBusy('candidates')
    setMobileStep('match')
    try {
      const data = await post<MetadataCandidate[]>('/api/fetch_id3_by_title/', {
        title: draft().title,
        resource: provider(),
        full_path: draft().file_full_path || joinPath(path(), activeFile()),
      })
      setCandidates(data)
      if (!data.length) toast.notify('当前来源没有找到匹配项', 'info')
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '刮削搜索失败', 'error')
    } finally {
      setBusy('')
    }
  }

  const applyCandidate = async (candidate: MetadataCandidate) => {
    setDraft((value) => ({
      ...value,
      title: candidate.name || value.title,
      artist: normalizeArtistMetadata(candidate.artist) || value.artist,
      album: candidate.album || value.album,
      year: candidate.year || value.year,
      tracknumber: candidate.tracknumber || value.tracknumber,
      discnumber: candidate.discnumber || value.discnumber,
      album_img: candidate.album_img || value.album_img,
    }))
    await applyCandidateLyrics(candidate, false)
    toast.notify('候选的全部元信息已采用', 'success')
  }

  const applyCandidateLyrics = async (candidate: MetadataCandidate, notify = true) => {
    if (!candidate.id) {
      if (notify) toast.notify('这个候选没有可用的歌词标识', 'info')
      return
    }
    setBusy(`lyrics:${candidate.id}`)
    try {
      const lyrics = await post<string>('/api/fetch_lyric/', {
        song_id: candidate.id,
        resource: candidate.resource || provider(),
      })
      if (!lyrics) {
        if (notify) toast.notify('当前来源没有返回歌词', 'info')
        return
      }
      setDraft((value) => ({ ...value, lyrics }))
      if (notify) toast.notify('歌词已填入元信息', 'success')
    } catch (error) {
      if (notify) toast.notify(error instanceof Error ? error.message : '歌词获取失败', 'error')
    } finally {
      setBusy('')
    }
  }

  const applyCandidateField = (
    candidate: MetadataCandidate,
    field: 'title' | 'artist' | 'album' | 'year' | 'tracknumber' | 'discnumber' | 'album_img',
  ) => {
    const source = {
      title: candidate.name,
      artist: normalizeArtistMetadata(candidate.artist),
      album: candidate.album,
      year: candidate.year,
      tracknumber: candidate.tracknumber,
      discnumber: candidate.discnumber,
      album_img: candidate.album_img,
    }[field]
    const labels = {
      title: '曲名', artist: '艺术家', album: '专辑', year: '年份',
      tracknumber: '音轨号', discnumber: '碟号', album_img: '封面',
    }
    if (!source) return toast.notify(`候选中没有可用的${labels[field]}`, 'info')
    setDraft((value) => ({ ...value, [field]: source }))
    toast.notify(`${labels[field]}已填入元信息`, 'success')
  }

  const applyCandidateTrackDisc = (candidate: MetadataCandidate) => {
    if (!candidate.tracknumber && !candidate.discnumber) return toast.notify('候选中没有音轨或碟号', 'info')
    setDraft((value) => ({
      ...value,
      tracknumber: candidate.tracknumber || value.tracknumber,
      discnumber: candidate.discnumber || value.discnumber,
    }))
    toast.notify('音轨号和碟号已填入元信息', 'success')
  }

  const uploadCover = async (file?: File) => {
    if (!file) return
    if (file.size > 5 * 1024 * 1024) return toast.notify('封面不能超过 5 MB', 'error')
    const form = new FormData()
    form.append('upload_file', file)
    setBusy('cover')
    try {
      const data = await management<string>('/api/upload_image/', { method: 'POST', body: form })
      setDraft((value) => ({ ...value, album_img: data }))
      toast.notify('封面已载入', 'success')
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '封面上传失败', 'error')
    } finally {
      setBusy('')
    }
  }

  const runBatch = async () => {
    if (!selectedData().length) return
    setBusy('batch')
    try {
      const data = await post<{ job_id: string }>('/api/batch_auto_update_id3/', {
        file_full_path: path(), select_data: selectedData(),
        music_info: { source_list: batchSources(), select_mode: batchMode() },
      })
      toast.notify(`批量任务已创建：${data.job_id.slice(0, 8)}`, 'success')
      setBatchOpen(false)
      await loadJobs()
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '批量任务创建失败', 'error')
    } finally {
      setBusy('')
    }
  }

  const runTidy = async () => {
    if (!selectedData().length) return
    setBusy('tidy')
    try {
      await post('/api/tidy_folder/', {
        root_path: roots().find((root) => path().startsWith(root.path))?.path || roots()[0]?.path,
        first_dir: tidyFirst(), second_dir: tidySecond(), file_full_path: path(), select_data: selectedData(),
      })
      toast.notify('目录整理完成', 'success')
      setTidyOpen(false)
      await loadDirectory(path())
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '目录整理失败', 'error')
    } finally {
      setBusy('')
    }
  }

  const toggleSource = (source: string) => setBatchSources((current) => current.includes(source) ? current.filter((item) => item !== source) : [...current, source])
  const selectFileScope = (scope: ScraperFileScope) => {
    setFileScope(scope)
    setFilterMenuOpen(false)
    setSelected(new Set<string>())
  }

  return (
    <div class="page scraper-page">
      <header class="page-header scraper-header">
        <p class="eyebrow">METADATA WORKBENCH</p>
      </header>

      <div class="mobile-stepper">
        <button class={mobileStep() === 'files' ? 'is-active' : ''} onClick={() => setMobileStep('files')}><span>1</span>文件</button>
        <div /><button class={mobileStep() === 'edit' ? 'is-active' : ''} disabled={!activeFile()} onClick={() => setMobileStep('edit')}><span>2</span>编辑</button>
        <div /><button class={mobileStep() === 'match' ? 'is-active' : ''} disabled={!activeFile()} onClick={() => setMobileStep('match')}><span>3</span>匹配</button>
      </div>

      <div class="scraper-grid">
        <section class={`work-panel file-browser ${mobileStep() !== 'files' ? 'mobile-hidden' : ''}`}>
          <header><div><span class="panel-number">01</span><h2>选择文件</h2></div><select value={roots().find((root) => path().startsWith(root.path))?.path || ''} onChange={(event) => void loadDirectory(event.currentTarget.value)}>{roots().map((root) => <option value={root.path}>{root.name}</option>)}</select></header>
          <div class="path-bar"><button class="icon-button" onClick={() => void loadDirectory(parentPath(path(), roots().map((root) => root.path)))}><ArrowLeft size={17} /></button><span title={path()}>{path() || '尚未配置曲库'}</span><button class="icon-button" onClick={() => void loadDirectory()}><RefreshCw class={busy() === 'files' ? 'spin' : ''} size={16} /></button></div>
          <div class="file-toolbar">
            <div class="file-filter-combobox" onFocusOut={(event) => { if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setFilterMenuOpen(false) }}>
              <label class="file-search"><Search size={16} /><input value={filter()} onInput={(event) => setFilter(event.currentTarget.value)} onFocus={() => setFilterMenuOpen(true)} onClick={() => setFilterMenuOpen(true)} placeholder={fileScope() === 'needs_scrape' ? '需要刮削' : '全部歌曲'} role="combobox" aria-expanded={filterMenuOpen()} aria-controls="scraper-file-scope" /><span class={`file-scope-mark ${fileScope()}`} /></label>
              <Show when={filterMenuOpen()}>
                <div class="file-filter-options" id="scraper-file-scope" role="listbox">
                  <button class={fileScope() === 'needs_scrape' ? 'is-active' : ''} role="option" aria-selected={fileScope() === 'needs_scrape'} onClick={() => selectFileScope('needs_scrape')}><span class="file-filter-title">需要刮削</span></button>
                  <button class={fileScope() === 'all' ? 'is-active' : ''} role="option" aria-selected={fileScope() === 'all'} onClick={() => selectFileScope('all')}><span class="file-filter-title">全部歌曲</span></button>
                </div>
              </Show>
            </div>
            <select value={sort()[0] || 'name'} onChange={(event) => { setSort(event.currentTarget.value === 'name' ? [] : [event.currentTarget.value]); void loadDirectory() }}><option value="name">名称</option><option value="update_time">修改时间</option><option value="size">大小</option></select>
          </div>
          <div class="file-list">
            <For each={visibleEntries()} fallback={<div class="empty-state">{fileScope() === 'needs_scrape' ? '当前目录没有需要刮削的歌曲' : '这个目录里没有支持的音频'}</div>}>
              {(entry) => (
                <div class={`file-row ${activeFile() === entry.name ? 'is-active' : ''}`}>
                  <input type="checkbox" checked={selected().has(entry.name)} onChange={() => toggleSelected(entry)} aria-label={`选择 ${entry.name}`} />
                  <button onDblClick={() => void openEntry(entry)} onClick={() => entry.icon === 'icon-script-file' ? void openEntry(entry) : undefined}>
                    <span class={`file-icon ${entry.icon === 'icon-folder' ? 'folder' : ''}`}>{entry.icon === 'icon-folder' ? <Folder /> : <FileAudio />}</span>
                    <div><strong>{entry.name}</strong><small>{entry.icon === 'icon-folder' ? '文件夹' : `${formatBytes(entry.size)} · ${entry.update_time}`}</small></div>
                    <ChevronRight size={16} />
                  </button>
                </div>
              )}
            </For>
          </div>
          <Show when={selected().size}>
            <footer class="batch-bar"><span><b>{selected().size}</b> 项已选择</span><button onClick={() => setTidyOpen(true)}><FolderInput size={16} />整理</button><button class="primary-button small" onClick={() => setBatchOpen(true)}><WandSparkles size={16} />批量刮削</button></footer>
          </Show>
        </section>

        <section class={`work-panel metadata-editor ${mobileStep() !== 'edit' ? 'mobile-hidden' : ''}`}>
          <header><div><span class="panel-number">02</span><h2>编辑标签</h2></div><button class="primary-button small" onClick={save} disabled={!activeFile() || busy() === 'save' || busy() === 'metadata'}>{busy() === 'save' ? <LoaderCircle class="spin" /> : <Save />}保存</button></header>
          <Show when={activeFile()} fallback={<WorkbenchEmpty icon={Disc3} title="选择一首歌曲" text="从左侧文件列表选择音频，标签会显示在这里。" />}>
            <div class="metadata-scroll">
              <div class="cover-editor">
                <div class="editable-cover"><Show when={draft().album_img || embeddedArtworkUrl()} fallback={artworkLoading() === activeFile() ? <LoaderCircle class="spin" /> : <Disc3 />}><img src={draft().album_img || embeddedArtworkUrl()} alt="专辑封面" onLoad={() => setArtworkLoading('')} onError={() => { if (draft().album_img) setDraft((value) => ({ ...value, album_img: '' })); else setEmbeddedArtworkUrl(''); setArtworkLoading('') }} /></Show></div>
                <div><strong>{draft().title || activeFile()}</strong><span>{draft().artist || '未知艺术家'}</span><small>{busy() === 'metadata' ? '正在读取标签…' : `${formatDuration(draft().duration)} · ${draft().bit_rate || '—'} kbps · ${formatBytes(draft().size)}`}</small><label class="text-button upload-trigger"><ImagePlus size={15} />{busy() === 'cover' ? '上传中…' : '更换封面'}<input type="file" accept="image/*" onChange={(event) => void uploadCover(event.currentTarget.files?.[0])} /></label></div>
              </div>
              <div class="form-grid">
                <Field label="标题" value={draft().title} onInput={(value) => setDraft((item) => ({ ...item, title: value }))} wide />
                <Field label="艺术家" value={draft().artist} onInput={(value) => setDraft((item) => ({ ...item, artist: value }))} />
                <Field label="专辑" value={draft().album} onInput={(value) => setDraft((item) => ({ ...item, album: value }))} />
                <Field label="专辑艺术家" value={draft().albumartist} onInput={(value) => setDraft((item) => ({ ...item, albumartist: value }))} />
                <Field label="文件名" value={draft().filename} onInput={(value) => setDraft((item) => ({ ...item, filename: value }))} />
                <Field label="曲风" value={draft().genre} onInput={(value) => setDraft((item) => ({ ...item, genre: value }))} />
                <Field label="语言" value={draft().language} onInput={(value) => setDraft((item) => ({ ...item, language: value }))} />
                <Field label="年份" value={draft().year} onInput={(value) => setDraft((item) => ({ ...item, year: value }))} />
                <Field label="音轨号" value={draft().tracknumber} onInput={(value) => setDraft((item) => ({ ...item, tracknumber: value }))} />
                <Field label="碟号" value={draft().discnumber} onInput={(value) => setDraft((item) => ({ ...item, discnumber: value }))} />
                <label class="field wide"><span>歌词</span><textarea rows={8} value={draft().lyrics} onInput={(event) => setDraft((item) => ({ ...item, lyrics: event.currentTarget.value }))} /></label>
                <label class="field wide"><span>描述</span><textarea rows={3} value={draft().comment} onInput={(event) => setDraft((item) => ({ ...item, comment: event.currentTarget.value }))} /></label>
              </div>
              <div class="toggle-grid"><label><input type="checkbox" checked={draft().is_save_lyrics_file} onChange={(event) => setDraft((item) => ({ ...item, is_save_lyrics_file: event.currentTarget.checked }))} /><span>另存同名歌词文件</span></label><label><input type="checkbox" checked={draft().is_save_album_cover} onChange={(event) => setDraft((item) => ({ ...item, is_save_album_cover: event.currentTarget.checked }))} /><span>另存 cover.jpg</span></label></div>
            </div>
          </Show>
        </section>

        <section class={`work-panel candidate-panel ${mobileStep() !== 'match' ? 'mobile-hidden' : ''}`}>
          <header><div><span class="panel-number">03</span><h2>匹配来源</h2></div><Settings2 size={18} /></header>
          <Show when={activeFile()} fallback={<WorkbenchEmpty icon={Sparkles} title="等待匹配" text="选择音频后，可以从在线来源获取标签、歌词与封面。" />}>
            <div class="provider-search-bar">
              <label><span>搜索源</span><select value={provider()} onChange={(event) => setProvider(event.currentTarget.value)}>{providers.map(([id, name]) => <option value={id}>{name}</option>)}</select></label>
              <button class="scrape-button" onClick={searchCandidates} disabled={busy() === 'candidates'}>{busy() === 'candidates' ? <LoaderCircle class="spin" /> : <WandSparkles />}搜索“{draft().title || activeFile()}”</button>
            </div>
            <div class="candidate-list">
              <Show when={candidates().length} fallback={<div class="candidate-hint"><Sparkles /><strong>让元数据自己归位</strong><span>搜索后会按标题、艺术家和专辑相似度排序。</span></div>}>
                <div class="candidate-table">
                  <div class="candidate-table-row candidate-table-head" aria-hidden="true">
                    <div>应用</div><div>封面</div><div>标题 / 艺术家</div><div>专辑</div>
                    <div>歌词</div><div>年份</div><div>音轨 / 碟号</div>
                  </div>
                  <For each={candidates()}>
                    {(candidate, index) => (
                      <div class="candidate-table-row candidate-result-row">
                        <button
                          class="candidate-apply-all"
                          onClick={() => void applyCandidate(candidate)}
                          title={`全部采用候选 ${index() + 1}`}
                          aria-label={`全部采用候选 ${index() + 1}，匹配度 ${candidateScorePercent(candidate.score)}%`}
                        >
                          <ArrowLeft size={20} />
                          <CandidateScore score={candidate.score} />
                        </button>
                        <CandidateCoverButton candidate={candidate} onApply={() => applyCandidateField(candidate, 'album_img')} />
                        <CandidateTitleArtistCell
                          title={candidate.name}
                          artist={candidate.artist}
                          onTitle={() => applyCandidateField(candidate, 'title')}
                          onArtist={() => applyCandidateField(candidate, 'artist')}
                        />
                        <CandidateValueButton label="专辑" value={candidate.album} onApply={() => applyCandidateField(candidate, 'album')} />
                        <CandidateValueButton
                          label="歌词"
                          value={busy() === `lyrics:${candidate.id}` ? '正在获取…' : '加载歌词'}
                          onApply={() => void applyCandidateLyrics(candidate)}
                        />
                        <CandidateValueButton label="年份" value={candidate.year} onApply={() => applyCandidateField(candidate, 'year')} />
                        <CandidateTrackDiscCell
                          track={candidate.tracknumber}
                          disc={candidate.discnumber}
                          onApply={() => applyCandidateTrackDisc(candidate)}
                        />
                      </div>
                    )}
                  </For>
                </div>
              </Show>
            </div>
          </Show>
        </section>
      </div>

      <section class="jobs-strip panel">
        <div class="section-heading compact"><div><span class="eyebrow">BACKGROUND ACTIVITY</span><h2>后台任务</h2></div><button class="icon-button" onClick={() => void loadJobs()}><RefreshCw size={16} /></button></div>
        <div class="job-cards"><For each={jobs().slice(0, 8)} fallback={<span class="muted">暂无刮削或扫描任务</span>}>{(job) => <article><span class={`job-state ${job.state}`} /> <div><strong>{job.kind === 'auto_tag' ? '自动刮削' : job.kind === 'scan' ? '曲库扫描' : job.kind}</strong><small>{job.message || job.id.slice(0, 8)}</small></div><div class="job-progress"><span style={{ width: `${Math.max(job.progress * 100, job.state === 'completed' ? 100 : 3)}%` }} /></div><b>{job.state === 'running' ? `${Math.round(job.progress * 100)}%` : stateLabel(job.state)}</b></article>}</For></div>
      </section>

      <Dialog open={batchOpen()} title="批量自动刮削" onClose={() => setBatchOpen(false)}>
        <p class="dialog-lead">将对已选择的 {selected().size} 项音频执行后台刮削。</p>
        <div class="dialog-section"><span>匹配来源</span><div class="check-pills">{providers.filter(([id]) => id !== 'acoustid').map(([id, name]) => <label class={batchSources().includes(id) ? 'is-active' : ''}><input type="checkbox" checked={batchSources().includes(id)} onChange={() => toggleSource(id)} />{name}</label>)}</div></div>
        <div class="dialog-section"><span>覆盖策略</span><div class="radio-cards"><label class={batchMode() === 'hard' ? 'is-active' : ''}><input type="radio" name="mode" value="hard" checked={batchMode() === 'hard'} onChange={() => setBatchMode('hard')} /><strong>完整覆盖</strong><small>使用最佳候选替换现有标签</small></label><label class={batchMode() === 'soft' ? 'is-active' : ''}><input type="radio" name="mode" value="soft" checked={batchMode() === 'soft'} onChange={() => setBatchMode('soft')} /><strong>仅补空缺</strong><small>保留已有的非空标签</small></label></div></div>
        <div class="dialog-actions"><button class="secondary-button" onClick={() => setBatchOpen(false)}>取消</button><button class="primary-button" disabled={!batchSources().length || busy() === 'batch'} onClick={runBatch}>{busy() === 'batch' ? <LoaderCircle class="spin" /> : <WandSparkles />}创建任务</button></div>
      </Dialog>

      <Dialog open={tidyOpen()} title="整理文件目录" onClose={() => setTidyOpen(false)}>
        <p class="dialog-lead">文件会移动到曲库根目录下生成的新目录，音频内容不会删除。</p>
        <Field label="一级目录模板" value={tidyFirst()} onInput={setTidyFirst} wide />
        <Field label="二级目录模板" value={tidySecond()} onInput={setTidySecond} wide />
        <p class="form-tip">可用变量：${'{title}'}、${'{artist}'}、${'{album}'}、${'{year}'}、${'{genre}'}</p>
        <div class="dialog-actions"><button class="secondary-button" onClick={() => setTidyOpen(false)}>取消</button><button class="primary-button" onClick={runTidy} disabled={busy() === 'tidy'}><FolderInput size={16} />开始整理</button></div>
      </Dialog>
    </div>
  )
}

function Field(props: { label: string; value: string; onInput: (value: string) => void; wide?: boolean }) {
  return <label class={`field ${props.wide ? 'wide' : ''}`}><span>{props.label}</span><input value={props.value} onInput={(event) => props.onInput(event.currentTarget.value)} /></label>
}

function CandidateCoverButton(props: { candidate: MetadataCandidate; onApply: () => void }) {
  const [failed, setFailed] = createSignal(false)
  return (
    <button class="candidate-cover-button" onClick={props.onApply} aria-label="采用候选封面">
      <span class="candidate-cover-media" title="点击采用封面">
        <Show when={props.candidate.album_img && !failed()} fallback={<ImageIcon />}>
          <img
            src={props.candidate.album_img}
            alt={props.candidate.album || '候选专辑封面'}
            referrerPolicy="no-referrer"
            onError={() => setFailed(true)}
          />
        </Show>
      </span>
    </button>
  )
}

function CandidateScore(props: { score: number }) {
  const percent = () => candidateScorePercent(props.score)
  return (
    <span
      class="candidate-score"
      classList={{ 'is-high': percent() >= 80, 'is-medium': percent() >= 60 && percent() < 80 }}
      title={`匹配度 ${percent()}%`}
      aria-hidden="true"
    >
      <span class="candidate-score-value">{percent()}%</span>
      <span class="candidate-score-meter"><span style={{ width: `${percent()}%` }} /></span>
    </span>
  )
}

function candidateScorePercent(score: number) {
  return Number.isFinite(score) ? Math.round(Math.min(1, Math.max(0, score)) * 100) : 0
}

function CandidateValueButton(props: {
  label: string
  value?: string
  onApply: () => void
}) {
  return (
    <button class="music-item" onClick={props.onApply} aria-label={`采用${props.label}`} title={props.value || `候选中没有${props.label}`}>
      {props.value || '—'}
    </button>
  )
}

function CandidateTitleArtistCell(props: {
  title?: string
  artist?: string
  onTitle: () => void
  onArtist: () => void
}) {
  return (
    <div class="candidate-title-artist">
      <div class="candidate-cell-action" role="button" tabIndex={0} onClick={props.onTitle} onKeyDown={(event) => activateCandidateCell(event, props.onTitle)} aria-label="采用标题" title={props.title || '候选中没有标题'}><span class="candidate-cell-value">{props.title || '—'}</span></div>
      <div class="candidate-cell-action" role="button" tabIndex={0} onClick={props.onArtist} onKeyDown={(event) => activateCandidateCell(event, props.onArtist)} aria-label="采用艺术家" title={props.artist || '候选中没有艺术家'}><span class="candidate-cell-value">{props.artist || '—'}</span></div>
    </div>
  )
}

function CandidateTrackDiscCell(props: {
  track?: string
  disc?: string
  onApply: () => void
}) {
  return (
    <div class="candidate-number-pair">
      <div class="candidate-cell-action" role="button" tabIndex={0} onClick={props.onApply} onKeyDown={(event) => activateCandidateCell(event, props.onApply)} aria-label={`采用音轨号 ${props.track || '空'} 和碟号 ${props.disc || '空'}`} title="点击同时采用音轨号和碟号">
        <span>{props.track || '—'}</span>
        <span>{props.disc || '—'}</span>
      </div>
    </div>
  )
}

function activateCandidateCell(event: KeyboardEvent, action: () => void) {
  if (event.key !== 'Enter' && event.key !== ' ') return
  event.preventDefault()
  action()
}

function WorkbenchEmpty(props: { icon: typeof Disc3; title: string; text: string }) {
  return <div class="workbench-empty"><props.icon /><strong>{props.title}</strong><span>{props.text}</span></div>
}

function metadataPlaceholder(entry: FileNode, directory: string): AudioMetadata {
  const extensionIndex = entry.name.lastIndexOf('.')
  const title = extensionIndex > 0 ? entry.name.slice(0, extensionIndex) : entry.name
  const suffix = extensionIndex > 0 ? entry.name.slice(extensionIndex + 1).toLowerCase() : ''
  return {
    ...emptyMetadata,
    title,
    filename: entry.name,
    file_full_path: joinPath(directory, entry.name),
    size: entry.size,
    suffix,
  }
}

function Dialog(props: { open: boolean; title: string; onClose: () => void; children: unknown }) {
  return <Show when={props.open}><div class="dialog-layer"><div class="sheet-backdrop" onClick={props.onClose} /><section class="dialog"><header><div><span class="eyebrow">BATCH OPERATION</span><h2>{props.title}</h2></div><button class="icon-button" onClick={props.onClose}><X /></button></header>{props.children as any}</section></div></Show>
}

function stateLabel(state: JobRecord['state']) {
  return { pending: '等待', running: '执行中', completed: '完成', failed: '失败' }[state]
}

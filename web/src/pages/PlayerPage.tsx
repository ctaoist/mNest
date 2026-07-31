import { createMemo, createSignal, For, onMount, Show } from 'solid-js'
import {
  ArrowLeft,
  ChevronLeft,
  ChevronRight,
  Disc3,
  Edit3,
  ExternalLink,
  Heart,
  ListMusic,
  LoaderCircle,
  Music2,
  Pause,
  Play,
  Plus,
  RadioTower,
  Search,
  Trash2,
  UsersRound,
  X,
} from 'lucide-solid'
import { CoverArt } from '../components/CoverArt'
import { TrackTable } from '../components/TrackTable'
import { useAuth } from '../context/auth'
import { usePlayer } from '../context/player'
import { useToast } from '../context/toast'
import { post, subsonic } from '../lib/api'
import {
  PAGE_SIZE_OPTIONS,
  buildPaginationItems,
  readStoredPageSize,
  writeStoredPageSize,
} from '../lib/pagination'
import { sortTracks, type TrackSortDirection, type TrackSortKey } from '../lib/trackSorting'
import { formatDuration, safeHttpUrl, safeRadioStreamUrl } from '../lib/utils'
import type { Album, Artist, Playlist, RadioStation, SearchResult, Track, TrackArtist } from '../types'

type PlayerView = 'songs' | 'albums' | 'artists' | 'playlists' | 'radio'
type ArtistDetail = { artist: Artist; tracks: Track[] }
type PlaylistDialog = { mode: 'create'; track?: Track } | { mode: 'rename'; playlist: Playlist }
const PAGE_SIZE_STORAGE_PREFIX = 'mNest:player:page-size'
const SUBSONIC_BATCH_SIZE = 500

function playlistDialogTrack(dialog: PlaylistDialog): Track | undefined {
  return dialog.mode === 'create' ? dialog.track : undefined
}

export function PlayerPage() {
  const auth = useAuth()
  const player = usePlayer()
  const toast = useToast()
  const [view, setView] = createSignal<PlayerView>('songs')
  const [albums, setAlbums] = createSignal<Album[]>([])
  const [songs, setSongs] = createSignal<Track[]>([])
  const [favorites, setFavorites] = createSignal<Track[]>([])
  const [artists, setArtists] = createSignal<Artist[]>([])
  const [playlists, setPlaylists] = createSignal<Playlist[]>([])
  const [radios, setRadios] = createSignal<RadioStation[]>([])
  const [songPage, setSongPage] = createSignal(1)
  const [albumPage, setAlbumPage] = createSignal(1)
  const [artistPage, setArtistPage] = createSignal(1)
  const [playlistPage, setPlaylistPage] = createSignal(1)
  const [radioPage, setRadioPage] = createSignal(1)
  const [songPageSize, setSongPageSize] = createPersistentPageSize('songs', 30)
  const [songSortKey, setSongSortKey] = createSignal<TrackSortKey>()
  const [songSortDirection, setSongSortDirection] = createSignal<TrackSortDirection>('asc')
  const [albumPageSize, setAlbumPageSize] = createPersistentPageSize('albums', 24)
  const [artistPageSize, setArtistPageSize] = createPersistentPageSize('artists', 30)
  const [playlistPageSize, setPlaylistPageSize] = createPersistentPageSize('playlists', 24)
  const [radioPageSize, setRadioPageSize] = createPersistentPageSize('radio', 12)
  const [selectedAlbum, setSelectedAlbum] = createSignal<Album | null>(null)
  const [selectedArtist, setSelectedArtist] = createSignal<ArtistDetail | null>(null)
  const [selectedPlaylist, setSelectedPlaylist] = createSignal<Playlist | null>(null)
  const [artistLoading, setArtistLoading] = createSignal('')
  const [playlistLoading, setPlaylistLoading] = createSignal('')
  const [playlistTrack, setPlaylistTrack] = createSignal<Track | null>(null)
  const [playlistDialog, setPlaylistDialog] = createSignal<PlaylistDialog | null>(null)
  const [playlistName, setPlaylistName] = createSignal('')
  const [playlistBusy, setPlaylistBusy] = createSignal('')
  const [query, setQuery] = createSignal('')
  const [searching, setSearching] = createSignal(false)
  const [results, setResults] = createSignal<SearchResult | null>(null)
  const [loading, setLoading] = createSignal(true)
  const [deletingTrack, setDeletingTrack] = createSignal('')
  const canDeleteTracks = () => auth.user()?.role === 'admin'

  const sortedSongs = createMemo(() => {
    const key = songSortKey()
    return key ? sortTracks(songs(), key, songSortDirection()) : songs()
  })
  const pagedSongs = createMemo(() => pageSlice(sortedSongs(), songPage(), songPageSize()))
  const pagedAlbums = createMemo(() => pageSlice(albums(), albumPage(), albumPageSize()))
  const pagedArtists = createMemo(() => pageSlice(artists(), artistPage(), artistPageSize()))
  const pagedPlaylists = createMemo(() => pageSlice(playlists(), playlistPage(), playlistPageSize()))
  const pagedRadios = createMemo(() => pageSlice(radios(), radioPage(), radioPageSize()))
  const ownedPlaylists = createMemo(() => playlists().filter((playlist) => playlist.owner === auth.user()?.username))

  const sortSongsBy = (key: TrackSortKey) => {
    if (songSortKey() === key) {
      setSongSortDirection((direction) => direction === 'asc' ? 'desc' : 'asc')
    } else {
      setSongSortKey(key)
      setSongSortDirection('asc')
    }
    setSongPage(1)
  }

  const loadFavorites = async () => {
    const response = await subsonic<{ starred2: { song?: Track[] } }>('getStarred2')
    setFavorites(response.starred2?.song || [])
  }

  const loadPlaylists = async () => {
    const response = await subsonic<{ playlists: { playlist?: Playlist[] } }>('getPlaylists')
    setPlaylists(response.playlists?.playlist || [])
  }

  onMount(async () => {
    setLoading(true)
    try {
      const [albumResponse, songResponse, artistResponse, radioResponse] = await Promise.all([
        loadAllAlbums(),
        loadAllSongs(),
        subsonic<{ artists: { index?: Array<{ artist?: Artist[] }> } }>('getArtists'),
        subsonic<{ internetRadioStations: { internetRadioStation?: RadioStation[] } }>('getInternetRadioStations'),
        loadFavorites(),
        loadPlaylists(),
      ])
      setAlbums(albumResponse)
      setSongs(songResponse)
      setArtists((artistResponse.artists?.index || []).flatMap((group) => group.artist || []))
      setRadios(radioResponse.internetRadioStations?.internetRadioStation || [])
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '曲库加载失败', 'error')
    } finally {
      setLoading(false)
    }
  })

  const openAlbum = async (album: Pick<Album, 'id'>) => {
    try {
      const response = await subsonic<{ album: Album }>('getAlbum', { id: album.id })
      setSelectedAlbum(response.album)
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '专辑加载失败', 'error')
    }
  }

  const openArtist = async (artist: Artist) => {
    setArtistLoading(artist.id)
    try {
      const tracks = songs().filter((track) => track.artists.some((credit) => credit.id === artist.id))
      setSelectedArtist({ artist, tracks })
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '艺术家歌曲加载失败', 'error')
    } finally {
      setArtistLoading('')
    }
  }

  const openTrackArtist = (credit: TrackArtist) => {
    const artist = artists().find((item) => item.id === credit.id)
      || { id: credit.id, name: credit.name, albumCount: 0, songCount: 0 }
    void openArtist(artist)
  }

  const openTrackAlbum = (track: Track) => {
    if (track.albumId) void openAlbum({ id: track.albumId })
  }

  const loadPlaylistDetail = async (playlist: Pick<Playlist, 'id'>) => {
    const response = await subsonic<{ playlist: Playlist }>('getPlaylist', { id: playlist.id })
    return response.playlist
  }

  const openPlaylist = async (playlist: Pick<Playlist, 'id'>) => {
    setPlaylistLoading(playlist.id)
    try {
      setSelectedPlaylist(await loadPlaylistDetail(playlist))
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '歌单加载失败', 'error')
    } finally {
      setPlaylistLoading('')
    }
  }

  const playPlaylist = async (playlist: Playlist) => {
    setPlaylistLoading(playlist.id)
    try {
      const detail = selectedPlaylist()?.id === playlist.id && selectedPlaylist()?.entry
        ? selectedPlaylist()!
        : await loadPlaylistDetail(playlist)
      if (!detail.entry?.length) return toast.notify('这个歌单还没有歌曲', 'info')
      player.playTracks(detail.entry)
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '歌单播放失败', 'error')
    } finally {
      setPlaylistLoading('')
    }
  }

  const openCreatePlaylist = (track?: Track) => {
    setPlaylistName('')
    setPlaylistTrack(null)
    setPlaylistDialog({ mode: 'create', track })
  }

  const openRenamePlaylist = (playlist: Playlist) => {
    setPlaylistName(playlist.name)
    setPlaylistDialog({ mode: 'rename', playlist })
  }

  const savePlaylist = async (event: SubmitEvent) => {
    event.preventDefault()
    const dialog = playlistDialog()
    const name = playlistName().trim()
    if (!dialog || !name) return
    setPlaylistBusy('save')
    try {
      if (dialog.mode === 'create') {
        const response = await subsonic<{ playlist: Playlist }>('createPlaylist', {
          name,
          songId: dialog.track ? [dialog.track.id] : undefined,
        })
        toast.notify(dialog.track ? '歌单已创建并加入歌曲' : '歌单已创建', 'success')
        if (view() === 'playlists') setSelectedPlaylist(response.playlist)
      } else {
        await subsonic('updatePlaylist', { playlistId: dialog.playlist.id, name })
        toast.notify('歌单名称已更新', 'success')
        if (selectedPlaylist()?.id === dialog.playlist.id) {
          setSelectedPlaylist(await loadPlaylistDetail(dialog.playlist))
        }
      }
      setPlaylistDialog(null)
      await loadPlaylists()
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '歌单保存失败', 'error')
    } finally {
      setPlaylistBusy('')
    }
  }

  const deletePlaylist = async (playlist: Playlist) => {
    if (!window.confirm(`确认删除歌单“${playlist.name}”？\n\n歌曲文件不会被删除。`)) return
    setPlaylistBusy(`delete:${playlist.id}`)
    try {
      await subsonic('deletePlaylist', { id: playlist.id })
      if (selectedPlaylist()?.id === playlist.id) setSelectedPlaylist(null)
      toast.notify('歌单已删除', 'success')
      await loadPlaylists()
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '歌单删除失败', 'error')
    } finally {
      setPlaylistBusy('')
    }
  }

  const addTrackToPlaylist = async (playlist: Playlist) => {
    const track = playlistTrack()
    if (!track) return
    setPlaylistBusy(`add:${playlist.id}`)
    try {
      await subsonic('updatePlaylist', { playlistId: playlist.id, songIdToAdd: [track.id] })
      toast.notify(`已加入“${playlist.name}”`, 'success')
      setPlaylistTrack(null)
      await loadPlaylists()
      if (selectedPlaylist()?.id === playlist.id) setSelectedPlaylist(await loadPlaylistDetail(playlist))
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '歌曲添加失败', 'error')
    } finally {
      setPlaylistBusy('')
    }
  }

  const removeTrackFromPlaylist = async (_track: Track, index: number) => {
    const playlist = selectedPlaylist()
    if (!playlist) return
    setPlaylistBusy(`remove:${index}`)
    try {
      await subsonic('updatePlaylist', { playlistId: playlist.id, songIndexToRemove: [index] })
      setSelectedPlaylist(await loadPlaylistDetail(playlist))
      await loadPlaylists()
      toast.notify('歌曲已从歌单移除', 'success')
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '歌曲移除失败', 'error')
    } finally {
      setPlaylistBusy('')
    }
  }

  const search = async (event: SubmitEvent) => {
    event.preventDefault()
    if (!query().trim()) return setResults(null)
    setSearching(true)
    try {
      const response = await subsonic<{ searchResult3: SearchResult }>('search3', {
        query: query().trim(), artistCount: 20, albumCount: 30, songCount: 80,
      })
      setResults(response.searchResult3)
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '搜索失败', 'error')
    } finally {
      setSearching(false)
    }
  }

  const toggleFavorite = async (track: Track) => {
    try {
      await subsonic(track.starred ? 'unstar' : 'star', { id: track.id })
      toast.notify(track.starred ? '已取消收藏' : '已加入收藏', 'success')
      await loadFavorites()
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '收藏操作失败', 'error')
    }
  }

  const permanentlyDeleteTrack = async (track: Track) => {
    if (!canDeleteTracks() || deletingTrack()) return
    if (!window.confirm(`永久删除“${track.title}”？\n\n服务器上的歌曲文件会被立即删除，且无法恢复。`)) return
    setDeletingTrack(track.id)
    try {
      const response = await post<{ id: string; ids?: string[] }>('/api/tracks/delete/', { id: track.id })
      const deletedIds = new Set(response.ids?.length ? response.ids : [response.id])
      const keepTracks = (items: Track[] = []) => items.filter((item) => !deletedIds.has(item.id))
      const nextSongs = keepTracks(songs())
      setSongs(nextSongs)
      setFavorites((items) => keepTracks(items))
      setResults((value) => value ? { ...value, song: keepTracks(value.song || []) } : value)
      setSelectedAlbum((value) => {
        if (!value) return value
        const song = keepTracks(value.song || [])
        return song.length ? { ...value, song, songCount: song.length, duration: song.reduce((sum, item) => sum + item.duration, 0) } : null
      })
      setSelectedArtist((value) => {
        if (!value) return value
        const tracks = keepTracks(value.tracks)
        return tracks.length ? { ...value, tracks } : null
      })
      setSelectedPlaylist((value) => {
        if (!value) return value
        const entry = keepTracks(value.entry || [])
        return { ...value, entry, songCount: entry.length, duration: entry.reduce((sum, item) => sum + item.duration, 0) }
      })
      for (let index = player.queue().length - 1; index >= 0; index -= 1) {
        if (deletedIds.has(player.queue()[index].id)) player.removeAt(index)
      }
      setSongPage((page) => Math.min(page, Math.max(1, Math.ceil(nextSongs.length / songPageSize()))))
      toast.notify('歌曲文件已永久删除', 'success')

      try {
        const [nextAlbums, artistResponse] = await Promise.all([
          loadAllAlbums(),
          subsonic<{ artists: { index?: Array<{ artist?: Artist[] }> } }>('getArtists'),
          loadFavorites(),
          loadPlaylists(),
        ])
        setAlbums(nextAlbums)
        setArtists((artistResponse.artists?.index || []).flatMap((group) => group.artist || []))
      } catch (error) {
        toast.notify(error instanceof Error ? `歌曲已删除，但曲库统计刷新失败：${error.message}` : '歌曲已删除，但曲库统计刷新失败', 'info')
      }
    } catch (error) {
      toast.notify(error instanceof Error ? error.message : '歌曲永久删除失败', 'error')
    } finally {
      setDeletingTrack('')
    }
  }

  return (
    <div class="page player-page">
      <header class="player-toolbar" aria-label="播放器工具栏">
        <div class="segmented-tabs" role="tablist" aria-label="播放器分类">
          <button role="tab" aria-selected={view() === 'songs'} class={view() === 'songs' ? 'is-active' : ''} onClick={() => { setView('songs'); setResults(null) }}><Music2 size={16} />歌曲</button>
          <button role="tab" aria-selected={view() === 'albums'} class={view() === 'albums' ? 'is-active' : ''} onClick={() => { setView('albums'); setResults(null) }}><Disc3 size={16} />专辑</button>
          <button role="tab" aria-selected={view() === 'artists'} class={view() === 'artists' ? 'is-active' : ''} onClick={() => { setView('artists'); setResults(null) }}><UsersRound size={16} />艺术家</button>
          <button role="tab" aria-selected={view() === 'playlists'} class={view() === 'playlists' ? 'is-active' : ''} onClick={() => { setView('playlists'); setResults(null) }}><ListMusic size={16} />歌单</button>
          <button role="tab" aria-selected={view() === 'radio'} class={view() === 'radio' ? 'is-active' : ''} onClick={() => { setView('radio'); setResults(null) }}><RadioTower size={16} />电台</button>
        </div>
        <form class="search-box" onSubmit={search}>
          <Search size={18} />
          <input value={query()} onInput={(event) => setQuery(event.currentTarget.value)} placeholder="搜索歌曲、专辑或艺术家" />
          <Show when={searching()}><LoaderCircle class="spin" size={17} /></Show>
        </form>
      </header>

      <Show when={!loading()} fallback={<div class="loading-panel"><Disc3 class="spin-slow" /><span>正在翻阅曲库…</span></div>}>
        <Show when={results()}>
          {(searchResults) => (
            <section class="search-results page-reveal">
              <div class="section-heading"><div><span class="eyebrow">SEARCH RESULTS</span><h2>“{query()}”</h2></div><button class="text-button" onClick={() => { setResults(null); setQuery('') }}>清除搜索</button></div>
              <Show when={searchResults().album?.length}><AlbumGrid albums={searchResults().album} onOpen={openAlbum} /></Show>
              <Show when={searchResults().song?.length}><div class="panel"><h3>歌曲</h3><TrackTable tracks={searchResults().song} onFavorite={toggleFavorite} onArtist={openTrackArtist} onAlbum={openTrackAlbum} onAddToPlaylist={setPlaylistTrack} onDelete={canDeleteTracks() ? permanentlyDeleteTrack : undefined} deleteDisabled={!!deletingTrack()} /></div></Show>
              <Show when={!searchResults().album?.length && !searchResults().song?.length}><div class="empty-state">没有找到匹配内容</div></Show>
            </section>
          )}
        </Show>

        <Show when={!results() && view() === 'songs'}>
          <div class="page-reveal">
            <section class="panel">
              <div class="section-heading compact"><div><span class="eyebrow">SONG CATALOGUE</span><h2>全部歌曲</h2></div><div class="section-actions"><span class="count-label">{songs().length} 首</span><button class="primary-button small" disabled={!songs().length} onClick={() => player.playTracks(sortedSongs())}><Play size={15} fill="currentColor" />播放全部</button></div></div>
              <TrackTable tracks={pagedSongs()} onFavorite={toggleFavorite} onArtist={openTrackArtist} onAlbum={openTrackAlbum} onAddToPlaylist={setPlaylistTrack} onDelete={canDeleteTracks() ? permanentlyDeleteTrack : undefined} deleteDisabled={!!deletingTrack()} showHeader sortKey={songSortKey()} sortDirection={songSortDirection()} onSort={sortSongsBy} />
              <Pagination page={songPage()} total={songs().length} pageSize={songPageSize()} onChange={setSongPage} onPageSizeChange={(size) => { setSongPageSize(size); setSongPage(1) }} />
            </section>

            <Show when={favorites().length}>
              <section class="panel player-favorites">
                <div class="section-heading compact"><div><span class="eyebrow">YOUR COLLECTION</span><h2>收藏歌曲</h2></div><button class="round-action" onClick={() => player.playTracks(favorites())}><Play fill="currentColor" /></button></div>
                <TrackTable tracks={favorites()} onFavorite={toggleFavorite} onArtist={openTrackArtist} onAlbum={openTrackAlbum} onAddToPlaylist={setPlaylistTrack} onDelete={canDeleteTracks() ? permanentlyDeleteTrack : undefined} deleteDisabled={!!deletingTrack()} />
              </section>
            </Show>
          </div>
        </Show>

        <Show when={!results() && view() === 'albums'}>
          <section class="page-reveal">
            <div class="section-heading"><div><span class="eyebrow">ALBUM CATALOGUE</span><h2>全部专辑</h2></div><span class="count-label">{albums().length} 张</span></div>
            <AlbumGrid albums={pagedAlbums()} onOpen={openAlbum} />
            <Pagination page={albumPage()} total={albums().length} pageSize={albumPageSize()} onChange={setAlbumPage} onPageSizeChange={(size) => { setAlbumPageSize(size); setAlbumPage(1) }} />
          </section>
        </Show>

        <Show when={!results() && view() === 'artists'}>
          <section class="page-reveal">
            <div class="section-heading"><div><span class="eyebrow">ARTIST INDEX</span><h2>全部艺术家</h2></div><span class="count-label">{artists().length} 位</span></div>
            <ArtistGrid artists={pagedArtists()} loadingId={artistLoading()} onOpen={openArtist} />
            <Pagination page={artistPage()} total={artists().length} pageSize={artistPageSize()} onChange={setArtistPage} onPageSizeChange={(size) => { setArtistPageSize(size); setArtistPage(1) }} />
          </section>
        </Show>

        <Show when={!results() && view() === 'playlists'}>
          <section class="page-reveal">
            <div class="section-heading playlist-section-heading">
              <div><span class="eyebrow">PLAYLIST COLLECTION</span><div class="player-section-title">歌单</div></div>
              <div class="section-actions"><span class="count-label">{playlists().length} 个</span><button class="primary-button small" onClick={() => openCreatePlaylist()}><Plus size={16} />新建歌单</button></div>
            </div>
            <PlaylistGrid
              playlists={pagedPlaylists()}
              username={auth.user()?.username || ''}
              loadingId={playlistLoading()}
              busy={playlistBusy()}
              onOpen={openPlaylist}
              onPlay={playPlaylist}
              onRename={openRenamePlaylist}
              onDelete={deletePlaylist}
            />
            <Pagination page={playlistPage()} total={playlists().length} pageSize={playlistPageSize()} onChange={setPlaylistPage} onPageSizeChange={(size) => { setPlaylistPageSize(size); setPlaylistPage(1) }} />
          </section>
        </Show>

        <Show when={!results() && view() === 'radio'}>
          <section class="page-reveal">
            <div class="section-heading"><div><span class="eyebrow">INTERNET RADIO</span><h2>网络电台</h2></div><span class="count-label">{radios().length} 个</span></div>
            <RadioGrid stations={pagedRadios()} />
            <Pagination page={radioPage()} total={radios().length} pageSize={radioPageSize()} onChange={setRadioPage} onPageSizeChange={(size) => { setRadioPageSize(size); setRadioPage(1) }} />
          </section>
        </Show>
      </Show>

      <Show when={selectedAlbum()}>
        {(album) => (
          <div class="detail-overlay">
            <div class="sheet-backdrop" onClick={() => setSelectedAlbum(null)} />
            <section class="album-detail">
              <header><button class="icon-button" onClick={() => setSelectedAlbum(null)}><ArrowLeft /></button><span class="eyebrow">ALBUM FILE</span></header>
              <div class="album-detail-hero">
                <CoverArt id={album().coverArt} alt={album().name} />
                <div><span>{album().year || '—'} · {album().genre || '未分类'}</span><h2>{album().name}</h2><p>{album().artist}</p><small>{album().songCount} 首 · {formatDuration(album().duration)}</small>
                  <button class="primary-button" disabled={!album().song?.length} onClick={() => player.playTracks(album().song || [])}><Play size={17} fill="currentColor" />播放专辑</button>
                </div>
              </div>
              <TrackTable tracks={album().song || []} onFavorite={toggleFavorite} onArtist={openTrackArtist} onAlbum={openTrackAlbum} onAddToPlaylist={setPlaylistTrack} onDelete={canDeleteTracks() ? permanentlyDeleteTrack : undefined} deleteDisabled={!!deletingTrack()} compact />
            </section>
          </div>
        )}
      </Show>

      <Show when={selectedArtist()}>
        {(detail) => (
          <div class="detail-overlay">
            <div class="sheet-backdrop" onClick={() => setSelectedArtist(null)} />
            <section class="album-detail artist-detail">
              <header><button class="icon-button" onClick={() => setSelectedArtist(null)}><ArrowLeft /></button><span class="eyebrow">ARTIST FILE</span></header>
              <div class="album-detail-hero">
                <CoverArt id={detail().artist.coverArt} alt={detail().artist.name} />
                <div><span>{detail().artist.albumCount} 张专辑</span><h2>{detail().artist.name}</h2><p>艺术家歌曲</p><small>{detail().tracks.length} 首</small>
                  <button class="primary-button" disabled={!detail().tracks.length} onClick={() => player.playTracks(detail().tracks)}><Play size={17} fill="currentColor" />播放艺术家</button>
                </div>
              </div>
              <TrackTable tracks={detail().tracks} onFavorite={toggleFavorite} onArtist={openTrackArtist} onAlbum={openTrackAlbum} onAddToPlaylist={setPlaylistTrack} onDelete={canDeleteTracks() ? permanentlyDeleteTrack : undefined} deleteDisabled={!!deletingTrack()} compact />
            </section>
          </div>
        )}
      </Show>

      <Show when={selectedPlaylist()}>
        {(playlist) => (
          <div class="detail-overlay">
            <div class="sheet-backdrop" onClick={() => setSelectedPlaylist(null)} />
            <section class="album-detail playlist-detail">
              <header>
                <button class="icon-button" onClick={() => setSelectedPlaylist(null)} aria-label="关闭歌单"><ArrowLeft /></button>
                <span class="eyebrow">PLAYLIST FILE</span>
                <Show when={playlist().owner === auth.user()?.username}>
                  <div class="playlist-detail-tools">
                    <button class="icon-button" onClick={() => openRenamePlaylist(playlist())} aria-label={`重命名 ${playlist().name}`}><Edit3 size={17} /></button>
                    <button class="icon-button danger" onClick={() => void deletePlaylist(playlist())} aria-label={`删除 ${playlist().name}`}><Trash2 size={17} /></button>
                  </div>
                </Show>
              </header>
              <div class="album-detail-hero playlist-detail-hero">
                <div class="playlist-large-mark"><ListMusic /></div>
                <div><span>{playlist().owner === auth.user()?.username ? '我的歌单' : `来自 ${playlist().owner}`}</span><div class="playlist-detail-title">{playlist().name}</div><p>{playlist().comment || '按你的方式组织和播放音乐'}</p><small>{playlist().songCount} 首 · {formatDuration(playlist().duration)}</small>
                  <button class="primary-button" disabled={!playlist().entry?.length} onClick={() => player.playTracks(playlist().entry || [])}><Play size={17} fill="currentColor" />播放歌单</button>
                </div>
              </div>
              <TrackTable tracks={playlist().entry || []} onFavorite={toggleFavorite} onArtist={openTrackArtist} onAlbum={openTrackAlbum} onAddToPlaylist={setPlaylistTrack} onRemove={playlist().owner === auth.user()?.username ? removeTrackFromPlaylist : undefined} onDelete={canDeleteTracks() ? permanentlyDeleteTrack : undefined} actionsDisabled={!!playlistBusy()} deleteDisabled={!!deletingTrack()} compact />
            </section>
          </div>
        )}
      </Show>

      <Show when={playlistTrack()}>
        {(track) => (
          <div class="dialog-layer">
            <div class="sheet-backdrop" onClick={() => setPlaylistTrack(null)} />
            <section class="dialog playlist-picker-dialog" role="dialog" aria-modal="true" aria-label="添加到歌单">
              <header><div><span class="eyebrow">ADD TO PLAYLIST</span><div class="dialog-title">添加到歌单</div><p class="dialog-lead">{track().title}</p></div><button class="icon-button" onClick={() => setPlaylistTrack(null)} aria-label="关闭"><X /></button></header>
              <button class="playlist-create-option" onClick={() => openCreatePlaylist(track())}><span><Plus /></span><div><strong>新建歌单</strong><small>创建后自动加入这首歌曲</small></div></button>
              <div class="playlist-picker-list">
                <For each={ownedPlaylists()} fallback={<div class="empty-state small">还没有歌单，可以先新建一个</div>}>
                  {(playlist) => (
                    <button disabled={!!playlistBusy()} onClick={() => void addTrackToPlaylist(playlist)}>
                      <span class="playlist-mini-mark"><ListMusic /></span>
                      <div><strong>{playlist.name}</strong><small>{playlist.songCount} 首 · {formatDuration(playlist.duration)}</small></div>
                      <Show when={playlistBusy() === `add:${playlist.id}`} fallback={<Plus size={17} />}><LoaderCircle class="spin" size={17} /></Show>
                    </button>
                  )}
                </For>
              </div>
            </section>
          </div>
        )}
      </Show>

      <Show when={playlistDialog()}>
        {(dialog) => (
          <div class="dialog-layer">
            <div class="sheet-backdrop" onClick={() => !playlistBusy() && setPlaylistDialog(null)} />
            <form class="dialog playlist-name-dialog" role="dialog" aria-modal="true" aria-label={dialog().mode === 'create' ? '新建歌单' : '重命名歌单'} onSubmit={savePlaylist}>
              <header><div><span class="eyebrow">{dialog().mode === 'create' ? 'NEW PLAYLIST' : 'EDIT PLAYLIST'}</span><div class="dialog-title">{dialog().mode === 'create' ? '新建歌单' : '重命名歌单'}</div></div><button type="button" class="icon-button" onClick={() => setPlaylistDialog(null)} aria-label="关闭"><X /></button></header>
              <label class="input-wrap"><span>歌单名称</span><div><ListMusic size={17} /><input value={playlistName()} onInput={(event) => setPlaylistName(event.currentTarget.value)} maxlength={80} placeholder="例如：夜间驾驶" /></div></label>
              <Show when={playlistDialogTrack(dialog())}>{(track) => <p class="playlist-dialog-note">创建后将自动加入“{track().title}”。</p>}</Show>
              <div class="dialog-actions"><button type="button" class="secondary-button" onClick={() => setPlaylistDialog(null)}>取消</button><button class="primary-button" disabled={!playlistName().trim() || playlistBusy() === 'save'}>{playlistBusy() === 'save' ? <LoaderCircle class="spin" size={17} /> : <ListMusic size={17} />}{dialog().mode === 'create' ? '创建' : '保存'}</button></div>
            </form>
          </div>
        )}
      </Show>
    </div>
  )
}

function AlbumGrid(props: { albums: Album[]; onOpen: (album: Album) => void }) {
  return (
    <div class="album-grid">
      <For each={props.albums} fallback={<div class="empty-state">扫描曲库后，专辑会出现在这里</div>}>
        {(album) => (
          <button class="album-card" onClick={() => props.onOpen(album)}>
            <div class="album-cover-wrap"><CoverArt id={album.coverArt} alt={album.name} /><span class="album-play"><Play fill="currentColor" /></span></div>
            <strong>{album.name}</strong><span>{album.artist}</span><small>{album.year || '未知年份'} · {album.songCount} 首</small>
          </button>
        )}
      </For>
    </div>
  )
}

function ArtistGrid(props: { artists: Artist[]; loadingId: string; onOpen: (artist: Artist) => void }) {
  return (
    <div class="artist-grid">
      <For each={props.artists} fallback={<div class="empty-state">扫描曲库后，艺术家会出现在这里</div>}>
        {(artist) => (
          <button class="artist-card" onClick={() => props.onOpen(artist)} disabled={props.loadingId === artist.id}>
            <CoverArt id={artist.coverArt} alt={artist.name} />
            <div><strong>{artist.name}</strong><span>{artist.albumCount} 张专辑</span><small>{props.loadingId === artist.id ? '正在加载歌曲…' : `${artist.songCount} 首歌曲`}</small></div>
            <Show when={props.loadingId === artist.id}><LoaderCircle class="spin artist-loading" /></Show>
          </button>
        )}
      </For>
    </div>
  )
}

function PlaylistGrid(props: {
  playlists: Playlist[]
  username: string
  loadingId: string
  busy: string
  onOpen: (playlist: Playlist) => void
  onPlay: (playlist: Playlist) => void
  onRename: (playlist: Playlist) => void
  onDelete: (playlist: Playlist) => void
}) {
  return (
    <div class="playlist-grid">
      <For each={props.playlists} fallback={<div class="empty-state">新建歌单，把喜欢的歌曲放到一起</div>}>
        {(playlist) => {
          const loading = () => props.loadingId === playlist.id
          const owned = () => playlist.owner === props.username
          return (
            <article class="playlist-card">
              <button class="playlist-card-main" disabled={loading()} onClick={() => props.onOpen(playlist)}>
                <span class="playlist-card-mark"><ListMusic /><i /><i /><i /></span>
                <span class="playlist-card-copy"><strong>{playlist.name}</strong><span>{owned() ? '我的歌单' : playlist.owner}</span><small>{playlist.songCount} 首 · {formatDuration(playlist.duration)}</small></span>
                <Show when={loading()}><LoaderCircle class="spin playlist-card-loading" /></Show>
              </button>
              <div class="playlist-card-actions">
                <button disabled={loading() || !playlist.songCount} onClick={() => props.onPlay(playlist)} aria-label={`播放 ${playlist.name}`}><Play size={16} fill="currentColor" /></button>
                <Show when={owned()}>
                  <button onClick={() => props.onRename(playlist)} aria-label={`重命名 ${playlist.name}`}><Edit3 size={16} /></button>
                  <button disabled={props.busy === `delete:${playlist.id}`} class="danger" onClick={() => props.onDelete(playlist)} aria-label={`删除 ${playlist.name}`}><Trash2 size={16} /></button>
                </Show>
              </div>
            </article>
          )
        }}
      </For>
    </div>
  )
}

function RadioGrid(props: { stations: RadioStation[] }) {
  const player = usePlayer()
  const trackFor = (station: RadioStation, streamUrl: string): Track => ({
    id: `radio:${station.id}`,
    title: station.name,
    artists: [{ id: `radio-artist:${station.id}`, name: '网络电台' }],
    album: '实时广播',
    duration: 0,
    streamUrl,
  })
  return (
    <div class="radio-grid">
      <For each={props.stations} fallback={<div class="empty-state">系统设置中还没有配置网络电台</div>}>
        {(station) => {
          const active = () => player.current()?.id === `radio:${station.id}`
          const streamUrl = () => safeRadioStreamUrl(station.streamUrl)
          const homePageUrl = () => safeHttpUrl(station.homePageUrl)
          const proxyUrl = () => `/api/internet_radio_stream.mp3?${new URLSearchParams({ id: station.id }).toString()}`
          return (
            <article class={`radio-card ${active() ? 'is-active' : ''}`}>
              <span class="radio-mark"><RadioTower /></span>
              <div><strong>{station.name}</strong><span>{active() ? '正在播放实时流' : streamUrl() ? '互联网广播' : '流地址无效'}</span><small title={station.streamUrl}>{station.streamUrl}</small></div>
              <Show when={homePageUrl()}>{(homepage) => <a href={homepage()} target="_blank" rel="noreferrer" aria-label="打开电台主页"><ExternalLink /></a>}</Show>
              <button class="radio-play" disabled={!streamUrl()} onClick={() => active() ? player.toggle() : player.playStream(trackFor(station, proxyUrl()))} aria-label={`播放 ${station.name}`}>
                <Show when={active() && player.playing()} fallback={<Play fill="currentColor" />}><Pause fill="currentColor" /></Show>
              </button>
            </article>
          )
        }}
      </For>
    </div>
  )
}

function pageSlice<T>(items: T[], page: number, pageSize: number): T[] {
  const start = Math.max(0, page - 1) * pageSize
  return items.slice(start, start + pageSize)
}

function Pagination(props: {
  page: number
  total: number
  pageSize: number
  onChange: (page: number) => void
  onPageSizeChange: (pageSize: number) => void
}) {
  const pageCount = () => Math.max(1, Math.ceil(props.total / props.pageSize))
  const visiblePages = createMemo(() => buildPaginationItems(props.page, pageCount()))
  const change = (page: number) => {
    const next = Math.max(1, Math.min(page, pageCount()))
    props.onChange(next)
    window.scrollTo({ top: 0, behavior: 'smooth' })
  }
  return (
    <Show when={props.total > 0}>
      <nav class="pagination" aria-label="分页">
        <label class="pagination-size"><span>每页</span><select value={props.pageSize} onChange={(event) => props.onPageSizeChange(Number(event.currentTarget.value))}>{PAGE_SIZE_OPTIONS.map((size) => <option value={size}>{size}</option>)}</select><span>条</span></label>
        <button disabled={props.page <= 1} onClick={() => change(props.page - 1)} aria-label="上一页"><ChevronLeft /></button>
        <For each={visiblePages()}>{(item) => typeof item === 'number'
          ? <button class={props.page === item ? 'is-active' : ''} onClick={() => change(item)} aria-label={`第 ${item} 页`} aria-current={props.page === item ? 'page' : undefined}>{item}</button>
          : <span class="pagination-ellipsis" aria-hidden="true">…</span>
        }</For>
        <button disabled={props.page >= pageCount()} onClick={() => change(props.page + 1)} aria-label="下一页"><ChevronRight /></button>
        <span class="pagination-status">{props.page} / {pageCount()}</span>
      </nav>
    </Show>
  )
}

function createPersistentPageSize(view: PlayerView, fallback: number) {
  const storageKey = `${PAGE_SIZE_STORAGE_PREFIX}:${view}`
  const [pageSize, setPageSize] = createSignal(readStoredPageSize(storageKey, fallback))
  const updatePageSize = (value: number) => {
    setPageSize(value)
    writeStoredPageSize(storageKey, value)
  }
  return [pageSize, updatePageSize] as const
}

async function loadAllSongs(): Promise<Track[]> {
  const songs: Track[] = []
  const seen = new Set<string>()
  for (let batch = 0; ; batch += 1) {
    const response = await subsonic<{ searchResult3: SearchResult }>('search3', {
      query: '', artistCount: 0, albumCount: 0, songCount: SUBSONIC_BATCH_SIZE, songOffset: batch * SUBSONIC_BATCH_SIZE,
    })
    const page = response.searchResult3?.song || []
    let added = 0
    page.forEach((track) => {
      if (seen.has(track.id)) return
      seen.add(track.id)
      songs.push(track)
      added += 1
    })
    if (page.length < SUBSONIC_BATCH_SIZE || added === 0) break
  }
  return songs
}

async function loadAllAlbums(): Promise<Album[]> {
  const albums: Album[] = []
  const seen = new Set<string>()
  for (let batch = 0; ; batch += 1) {
    const response = await subsonic<{ albumList2: { album?: Album[] } }>('getAlbumList2', {
      type: 'alphabeticalByName', size: SUBSONIC_BATCH_SIZE, offset: batch * SUBSONIC_BATCH_SIZE,
    })
    const page = response.albumList2?.album || []
    let added = 0
    page.forEach((album) => {
      if (seen.has(album.id)) return
      seen.add(album.id)
      albums.push(album)
      added += 1
    })
    if (page.length < SUBSONIC_BATCH_SIZE || added === 0) break
  }
  return albums
}

export interface ApiResponse<T> {
  result: boolean
  code: string
  data: T
  message: string
}

export interface User {
  username: string
  role: string
  email?: string
}

export interface Track {
  id: string
  title: string
  artists: TrackArtist[]
  album: string
  albumId?: string
  coverArt?: string
  duration: number
  track?: number
  discNumber?: number
  year?: number
  genre?: string
  size?: number
  bitRate?: number
  suffix?: string
  starred?: string
  playCount?: number
  streamUrl?: string
}

export interface TrackArtist {
  id: string
  name: string
}

export interface Album {
  id: string
  name: string
  artist: string
  artistId: string
  coverArt?: string
  songCount: number
  duration: number
  year?: number
  genre?: string
  created?: string
  song?: Track[]
}

export interface Artist {
  id: string
  name: string
  coverArt?: string
  albumCount: number
  songCount: number
  album?: Album[]
}

export interface RadioStation {
  id: string
  name: string
  streamUrl: string
  homePageUrl?: string
}

export interface PlayQueue {
  current?: string
  currentIndex?: number
  position?: number
  entry: Track[]
}

export interface Playlist {
  id: string
  name: string
  comment: string
  owner: string
  public: boolean
  songCount: number
  duration: number
  created: string
  changed: string
  entry?: Track[]
}

export interface SearchResult {
  artist: Artist[]
  album: Album[]
  song: Track[]
}

export interface LibraryRoot {
  id: string
  name: string
  path: string
  enabled: number
}

export interface FileNode {
  id: number
  name: string
  title: string
  icon: 'icon-folder' | 'icon-script-file'
  size: number
  update_time: string
  children: FileNode[] | null
  needs_scrape: boolean
}

export interface AudioMetadata {
  title: string
  artist: string
  album: string
  albumartist: string
  genre: string
  year: string
  language: string
  lyrics: string
  comment: string
  tracknumber: string
  discnumber: string
  duration: number
  bit_rate: number
  size: number
  suffix: string
  filename: string
  file_full_path: string
  album_img: string
  artwork_mime: string
  artwork_w: number
  artwork_h: number
  artwork_size: number
  is_save_lyrics_file: boolean
  is_save_album_cover: boolean
  needs_scrape: boolean
}

export interface MetadataCandidate {
  id: string
  name: string
  artist: string
  artist_id: string
  album: string
  album_id: string
  album_img: string
  year: string
  tracknumber: string
  discnumber: string
  duration?: number
  resource: string
  score: number
}

export interface JobRecord {
  id: string
  kind: string
  state: 'pending' | 'running' | 'completed' | 'failed'
  progress: number
  message: string
  attempts: number
  created_at: string
  updated_at: string
}

export interface ConfigStatus {
  database: string
  queue: string
  library_roots: LibraryRoot[]
  providers: string[]
  download_filename_format: DownloadFilenameFormat
  lastfm: LastFmStatus
  tools: {
    ffmpeg: boolean
    fpcalc: boolean
    taglib_configured: boolean
  }
}

export interface LastFmStatus {
  configured: boolean
  connected: boolean
  authorization_pending: boolean
  username: string
  api_key: string
  has_shared_secret: boolean
}

export type DownloadFilenameFormat = 'artist-title' | 'title-artist'

export type DownloadSourceKind = 'netease' | 'qq' | 'qq2' | 'subsonic'

export interface DownloadSource {
  id: string
  kind: DownloadSourceKind
  name: string
  base_url: string
  username: string
  has_password: boolean
  has_cookie: boolean
  account_name: string
  enabled: boolean
}

export interface RemoteDownloadQuality {
  id: string
  label: string
}

export interface RemoteDownloadSong {
  source_id: string
  source_name: string
  source: DownloadSourceKind
  id: string
  title: string
  artists: string[]
  album: string
  suffix: string
  bit_rate?: number
  qualities: RemoteDownloadQuality[]
}

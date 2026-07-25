# OpenSubsonic compatibility

The server targets the official OpenSubsonic/Subsonic REST API version `1.16.1`.
Implemented endpoints return the standard `subsonic-response` envelope in JSON or XML.

## Authentication

- Password authentication: `u` + `p`, including the standard `enc:` hexadecimal form.
- Salted token authentication: `u` + `t` + `s`, where `t = MD5(password + salt)`.
- OpenSubsonic API key authentication: `apiKey` without `u`, `p`, `t`, or `s`.
- Conflicting authentication mechanisms return error `43`; invalid API keys return error `44`.

## Advertised OpenSubsonic extensions

- `apiKeyAuthentication` version 1
- `formPost` version 1
- `songLyrics` version 1
- `transcodeOffset` version 1
- `indexBasedQueue` version 1

Only extensions listed by `getOpenSubsonicExtensions` should be used by clients.

## Implemented domains

- System: ping, license, extension discovery and API-key token information.
- Browsing: music folders, indexes, artists, albums, songs, directories, genres and similar/top songs.
- Lists and search: album lists, random songs, genre songs, starred items and search/search2/search3.
- Media retrieval: stream, download, cover art and avatars.
- Media annotation: stars, ratings and scrobbles. Standard `submission=false` now-playing
  notifications and `submission=true` scrobbles are also forwarded to the authenticated user's
  linked Last.fm account without making Last.fm availability part of the OpenSubsonic request
  result.
- Playlists, bookmarks, play queues, internet radio, users, shares and library scanning.
- Legacy lyrics and structured lyrics by song ID.

## Intentionally unavailable domains

- Video, podcast, chat and jukebox operations.
- HLS and video conversion.
- Sonic analysis/path and transcode-profile extensions.
- Playback reporting extension; clients should use `scrobble` for play-count updates.

Unsupported endpoints return a standard failed OpenSubsonic response and are not advertised as extensions.

## Data access

OpenSubsonic is an HTTP protocol and does not require raw SQL. Normal CRUD, filtering,
pagination and subqueries use SeaORM/SeaQuery. The similar-song query retains one raw SQL
statement because it combines a self-join across track-artist credits with genre ranking;
it is isolated in that endpoint.

# OpenSubsonic 兼容性审计

本文档记录 mNest 对 OpenSubsonic/Subsonic REST API 的实际实现情况，供客户端兼容性判断和后续开发排期使用。

## 审计基线

- 审计日期：2026-08-25
- 实现基线：`194df30` 及本文同批兼容性修复
- 服务端声明的 Subsonic REST 协议版本：`1.16.1`
- 规范来源：
  - [OpenSubsonic API Reference](https://opensubsonic.netlify.app/docs/api-reference/)
  - [OpenSubsonic Endpoints](https://opensubsonic.netlify.app/docs/endpoints/)
  - [OpenSubsonic Extensions](https://opensubsonic.netlify.app/docs/extensions/)
  - [OpenSubsonic 相对原 Subsonic API 的变更](https://opensubsonic.netlify.app/docs/opensubsonic-changes/)
- 实现依据：`src/api/subsonic.rs`、`src/auth.rs` 及相关测试。

这里的 `1.16.1` 是服务端声明的基础 REST 协议版本，不代表所有可选 OpenSubsonic 扩展都已实现。客户端应以 `getOpenSubsonicExtensions` 的结果为准。

### 状态定义

| 状态 | 含义 |
| --- | --- |
| ✅ 已实现 | 存在可用的主要成功路径，核心参数和响应已落到实际数据或行为上。 |
| 🟡 部分实现 | 路由存在，但返回占位数据、遗漏核心参数/能力，或与规范存在会影响客户端的实质偏差。 |
| ❌ 未实现 | 总是返回失败响应，或进入统一的 `Endpoint ... is not implemented` 分支。 |

本次共核对官方端点目录中的 87 个端点：

| 已实现 | 部分实现 | 未实现 | 合计 |
| ---: | ---: | ---: | ---: |
| 66 | 1 | 20 | 87 |

该统计是源码级兼容性审计，不等同于通过了完整的 OpenSubsonic 一致性测试。可选响应字段、不同媒体编码和第三方客户端行为仍需通过集成测试验证。

## 通用协议能力

### 请求与响应

- REST 入口为 `/rest/{method}`，同时接受带 `.view` 的方法名。
- 支持 GET 查询参数和 POST `application/x-www-form-urlencoded` 参数；POST 请求体上限为 2 MiB。
- 支持 `f=json` 和 `f=xml`，未指定 `f` 时返回 XML。
- 成功与失败都使用标准 `subsonic-response` 包装；响应包含 `status`、`version`、`type`、`serverVersion` 和 `openSubsonic`。
- 普通协议请求要求 `v` 和 `c`。客户端版本高于 `1.16.1` 返回错误 `30`，主版本不兼容返回错误 `20`。
- `getOpenSubsonicExtensions` 按 OpenSubsonic 要求允许匿名访问。
- 不支持的 JSON/XML 端点返回 HTTP 200 和失败包装，通常使用错误码 `0`；被明确禁用的网络电台删除使用错误码 `50`。
- `stream`、`download`、`getCoverArt`、`getAvatar` 的内部错误当前统一映射为错误码 `70`，因此缺失参数等错误不一定得到规范期望的错误码 `10`。

### 认证

| 认证方式 | 状态 | 说明 |
| --- | --- | --- |
| `u` + `p` | ✅ | 支持明文密码。 |
| `u` + `p=enc:<hex>` | ✅ | 支持标准十六进制密码编码。 |
| `u` + `t` + `s` | ✅ | `t = MD5(password + salt)`。 |
| `apiKey` | ✅ | 可不带用户名认证，并实现 `tokenInfo`、错误 `43/44`；每位用户可在个人设置中查看、复制、轮换和吊销自己的 Key。Key 以带密钥查找指纹和加密密文保存，旧明文会在启动时原地升级。 |
| 冲突认证参数 | ✅ | API Key 与用户名/密码/盐值令牌混用，或密码与令牌混用时返回错误 `43`。 |

### 数据与权限模型限制

- 用户可通过 `musicFolderId` 授权一个或多个音乐文件夹；浏览、搜索、媒体读取、播放列表、队列、书签、分享和标注都会应用该访问边界。
- 支持并执行 `settingsRole`、`streamRole`、`downloadRole`、`commentRole`、`shareRole` 和用户最大码率；其余角色会被持久化并准确返回。按规范，`playlistRole` 自 1.8.0 起不再限制播放列表操作；其他角色对应的能力域尚不存在。
- 服务端没有 LDAP 认证后端，因此 `createUser` / `updateUser` 会拒绝 `ldapAuthenticated=true`，避免创建实际无法通过 LDAP 登录的账号。
- 项目当前只管理音频；视频、字幕、Podcast、聊天和 jukebox 域没有对应数据模型。
- `stream` 支持原始音频、Range、`mp3` / `opus` / `aac` / `flac` / `ogg` 转码、最大码率和转码起始偏移；不支持视频参数和 HLS。
- `scrobble` 的正式提交会更新全局及用户级播放次数；Now Playing 和正式 Scrobble 也会异步转发到用户已绑定的 Last.fm，但 Last.fm 失败不影响 OpenSubsonic 请求结果。

## OpenSubsonic 扩展

下表以官方扩展目录为基线。只有“已广告”列为“是”的扩展，客户端才应使用。

| 扩展 | 版本 | 已广告 | 状态 | 说明 |
| --- | --- | --- | --- | --- |
| `apiKeyAuthentication` | 1 | 是 | ✅ | API Key 认证、`tokenInfo`、新错误码及个人 Key 查看、轮换、吊销均已实现。 |
| `getPodcastEpisode` | 1 | 否 | ❌ | `getPodcastEpisode` 未实现。 |
| `formPost` | 1 | 是 | ✅ | REST 方法接受 URL-encoded POST 参数。 |
| `indexBasedQueue` | 1 | 是 | ✅ | `currentIndex` 独立持久化，重复歌曲及其准确当前位置可以往返。 |
| `playbackReport` | 1 | 是 | ✅ | `reportPlayback`、位置估算、一次性 Scrobble、`ignoreScrobble` 和带时间线字段的 `getNowPlaying` 已实现。 |
| `songLyrics` | 1 | 是 | ✅ | `getLyricsBySongId` 返回内嵌歌词，并将 LRC 时间标签转换为结构化行。 |
| `songLyrics` | 2 | 是 | ✅ | `enhanced=true` 返回 `kind=main`，并将 Enhanced LRC 内联时间标记转换为 `cueLine`/`cue`、毫秒起止时间及 UTF-8 `byteStart`/`byteEnd`。简单单层歌词按规范省略可选的 `agents`；当前歌词存储不包含可可靠恢复的翻译、注音或多歌手分轨，因此不会伪造这些层。 |
| `sonicSimilarity` | 1 | 否 | ❌ | `findSonicPath`、`getSonicSimilarTracks` 均未实现。 |
| `template` | 1、2 | 否 | — | 官方文档中的扩展示例模板，不是 mNest 功能目标。 |
| `topSongsByArtistId` | 1 | 是 | ✅ | `getTopSongs` 同时支持基础 `artist` 参数和优先级更高的艺术家 `id`。 |
| `transcodeOffset` | 1 | 是 | ✅ | 音乐 `stream` 接受 `timeOffset`，并从指定秒数开始转码。 |
| `transcoding` | 1 | 否 | ❌ | `getTranscodeDecision`、`getTranscodeStream` 均未实现。 |

配置了已启用的网易云下载源时，服务端还会广告私有扩展 `mnestRadioRecognition` 版本 1。它不是 OpenSubsonic 官方扩展，对应能力位于 mNest 自有 `/api` 接口，通用客户端不应依赖。

## 端点兼容矩阵

端点顺序与官方端点目录一致。表中的“已实现”表示该端点的主要用途可用，不表示每个可选响应字段都存在。

| 端点 | 状态 | 实现情况或缺口 |
| --- | --- | --- |
| `addChatMessage` | ❌ | 聊天域未启用，返回失败响应。 |
| `changePassword` | ✅ | 管理员可修改任意用户，普通用户可修改自己；支持明文和 `enc:` 密码。 |
| `createBookmark` | ✅ | 按用户创建或覆盖歌曲书签，支持位置和备注。 |
| `createInternetRadioStation` | ✅ | 管理员可创建网络电台；另支持私有的 `proxy` 参数。 |
| `createPlaylist` | ✅ | 支持新建、按 `playlistId` 覆盖，以及重复 `songId`。 |
| `createPodcastChannel` | ❌ | Podcast 域未启用。 |
| `createShare` | ✅ | 可分享一首、多首歌曲或整个专辑并设置描述/过期时间；视频随视频域一起不支持。 |
| `createUser` | ✅ | 可创建用户，并设置邮箱、密码、全部标准角色、最大码率和一个或多个 `musicFolderId`。 |
| `deleteBookmark` | ✅ | 只删除当前用户对指定歌曲的书签。 |
| `deleteInternetRadioStation` | ❌ | OpenSubsonic 路由被明确禁用并返回错误 `50`；mNest 管理接口另有删除能力。 |
| `deletePlaylist` | ✅ | 仅允许所有者删除，同时清理播放列表条目。 |
| `deletePodcastChannel` | ❌ | Podcast 域未启用。 |
| `deletePodcastEpisode` | ❌ | Podcast 域未启用。 |
| `deleteShare` | ✅ | 删除当前用户拥有的分享。 |
| `deleteUser` | ✅ | 管理员可删除其他用户，并清理其播放列表、标注、队列、分享和偏好数据。 |
| `download` | ✅ | 下载原始音频，支持 Range 和下载文件名响应头。 |
| `downloadPodcastEpisode` | ❌ | Podcast 域未启用。 |
| `findSonicPath` | ❌ | `sonicSimilarity` 扩展未实现。 |
| `getAlbum` | ✅ | 返回 ID3 专辑及按碟号、曲序排列的歌曲。 |
| `getAlbumInfo` | ✅ | 校验歌曲/专辑 ID，返回本地标签备注；没有本地备注时返回专辑与艺术家的事实性摘要。 |
| `getAlbumInfo2` | ✅ | 与 `getAlbumInfo` 相同，使用规范要求的 `albumInfo` 响应键。 |
| `getAlbumList` | ✅ | 支持规范的 10 种列表类型、分页和音乐文件夹过滤，返回目录式专辑。 |
| `getAlbumList2` | ✅ | 与 `getAlbumList` 相同，返回 ID3 专辑结构。 |
| `getArtist` | ✅ | 返回艺术家及其专辑。 |
| `getArtistInfo` | ✅ | 支持歌曲、专辑或艺术家 ID，返回本地统计摘要及按共同流派计算的相似艺术家，并处理 `count`。 |
| `getArtistInfo2` | ✅ | 与 `getArtistInfo` 相同，返回 `artistInfo2`。 |
| `getArtists` | ✅ | 按首字母分组返回 ID3 艺术家，支持音乐文件夹过滤。 |
| `getAvatar` | ✅ | 按用户名返回服务端生成的首字母 SVG 头像。 |
| `getBookmarks` | ✅ | 返回当前用户仍可解析到歌曲的全部书签。 |
| `getCaptions` | ❌ | 视频字幕域未启用。 |
| `getChatMessages` | ❌ | 聊天域未启用。 |
| `getCoverArt` | ✅ | 支持歌曲/专辑发出的 `coverArt` ID、缩放、ETag 和条件请求。 |
| `getGenres` | ✅ | 返回曲库流派及歌曲数、专辑数。 |
| `getIndexes` | ✅ | 支持 `musicFolderId` 和 `ifModifiedSince`，按艺术家首字母分组。 |
| `getInternetRadioStations` | ✅ | 返回网络电台；可按 mNest 配置生成代理流地址。 |
| `getLicense` | 🟡 | 返回固定的“有效至 2099 年”兼容响应，并非真实授权状态。 |
| `getLyrics` | ✅ | 按艺术家/标题从内嵌标签歌词中查找并返回旧版歌词。 |
| `getLyricsBySongId` | ✅ | 实现 `songLyrics` v1/v2；默认保持普通文本和 LRC 行级响应，`enhanced=true` 时为 Enhanced LRC 增加逐字/音节时间和 UTF-8 字节偏移。 |
| `getMusicDirectory` | ✅ | 可浏览音乐文件夹、艺术家、专辑和歌曲层级。 |
| `getMusicFolders` | ✅ | 返回当前用户有权访问的全部启用音乐文件夹及稳定的整型 API ID。 |
| `getNewestPodcasts` | ❌ | Podcast 域未启用。 |
| `getNowPlaying` | ✅ | 返回未停止且未过期的播放状态，并包含估算后的 `positionMs`、`state`、`playbackRate` 和客户端信息。 |
| `getOpenSubsonicExtensions` | ✅ | 无需认证即可返回扩展和版本列表。 |
| `getPlaylist` | ✅ | 所有者或任何用户可读取公开播放列表，歌曲顺序和重复项可保留。 |
| `getPlaylists` | ✅ | 返回自己的和公开播放列表；管理员可通过 `username` 查询其他用户。 |
| `getPlayQueue` | ✅ | 按用户读取传统 current-song 播放队列。 |
| `getPlayQueueByIndex` | ✅ | 以独立持久化的 `currentIndex` 返回位置，重复歌曲不会造成索引歧义。 |
| `getPodcastEpisode` | ❌ | 同名 OpenSubsonic 扩展未实现。 |
| `getPodcasts` | ❌ | Podcast 域未启用。 |
| `getRandomSongs` | ✅ | 支持数量、流派、年份范围和音乐文件夹过滤。 |
| `getScanStatus` | ✅ | 返回最近扫描任务是否为 pending/running；未返回可选的已扫描数量。 |
| `getShares` | ✅ | 返回当前用户的分享及可用歌曲条目。 |
| `getSimilarSongs` | ✅ | 按相同艺术家或流派从本地曲库生成目录式相似歌曲。 |
| `getSimilarSongs2` | ✅ | 与 `getSimilarSongs` 相同，使用 ID3 响应键。 |
| `getSong` | ✅ | 返回歌曲元数据及当前用户对该歌曲的收藏时间。 |
| `getSongsByGenre` | ✅ | 支持数量、偏移和音乐文件夹过滤。 |
| `getSonicSimilarTracks` | ❌ | `sonicSimilarity` 扩展未实现。 |
| `getStarred` | ✅ | 返回当前用户收藏的目录式歌曲、专辑和艺术家。 |
| `getStarred2` | ✅ | 返回当前用户收藏的 ID3 歌曲、专辑和艺术家。 |
| `getTopSongs` | ✅ | 支持基础 `artist` 名称和 `topSongsByArtistId` 的 `id`，按本地播放次数排序，`id` 优先。 |
| `getTranscodeDecision` | ❌ | `transcoding` 扩展未实现。 |
| `getTranscodeStream` | ❌ | `transcoding` 扩展未实现。 |
| `getUser` | ✅ | 可读取自己，管理员可读取任意用户；返回实际角色、最大码率和音乐文件夹授权。 |
| `getUsers` | ✅ | 管理员可列出用户，并返回每个用户实际保存的角色和文件夹授权。 |
| `getVideoInfo` | ❌ | 视频域未启用。 |
| `getVideos` | ❌ | 视频域未启用。 |
| `hls` | ❌ | HLS 未实现；`hls.m3u8` 会被归一化为该方法后返回未实现错误。 |
| `jukeboxControl` | ❌ | Jukebox 域未启用。 |
| `ping` | ✅ | 返回标准成功包装。 |
| `refreshPodcasts` | ❌ | Podcast 域未启用。 |
| `reportPlayback` | ✅ | 保存 starting/playing/paused/stopped 时间线，估算播放位置，支持 `ignoreScrobble`，达到播放阈值后只提交一次 Scrobble。 |
| `savePlayQueue` | ✅ | 保存传统 current-song 队列、位置和客户端名。 |
| `savePlayQueueByIndex` | ✅ | 独立持久化 `currentIndex`，可准确保存重复歌曲中的当前项。 |
| `scrobble` | ✅ | 支持批量 ID/时间和 `submission`；正式提交更新播放次数，并异步转发 Last.fm。 |
| `search` | ✅ | 实现旧版 artist/album/title/any 搜索和统一分页。 |
| `search2` | ✅ | 分别分页搜索艺术家、专辑和歌曲，支持音乐文件夹过滤。 |
| `search3` | ✅ | 与 `search2` 相同，返回 ID3 艺术家/专辑结构。 |
| `setRating` | ✅ | 支持歌曲、专辑、艺术家 0–5 分用户评分；0 表示删除评分。 |
| `star` | ✅ | 支持批量 `id`、`albumId`、`artistId`，并能识别通过目录式 `id` 传入的歌曲、专辑或艺术家。 |
| `startScan` | ✅ | 仅管理员可启动扫描，返回扫描状态和 mNest 私有 `jobId`。 |
| `stream` | ✅ | 支持原始音频、Range、音频转码、最大码率和 `timeOffset`；不支持视频参数。 |
| `tokenInfo` | ✅ | 使用有效 `apiKey` 时返回对应用户名。 |
| `unstar` | ✅ | 支持批量参数，并能取消通过目录式 `id` 收藏的歌曲、专辑或艺术家。 |
| `updateInternetRadioStation` | ✅ | 管理员可更新网络电台及私有代理配置。 |
| `updatePlaylist` | ✅ | 支持名称、备注、公开状态、按索引删除和追加歌曲。 |
| `updateShare` | ✅ | 分享所有者可更新描述和过期时间。 |
| `updateUser` | ✅ | 密码为可选参数；支持更新邮箱、管理员状态、全部标准角色、文件夹授权和最大码率。 |

## 明确未实现的能力域

- 视频：`getVideos`、`getVideoInfo`、`getCaptions`、`hls`。
- Podcast：频道、单集、刷新和下载相关的全部端点。
- 聊天：`getChatMessages`、`addChatMessage`。
- Jukebox：`jukeboxControl`。
- 声学相似度：`findSonicPath`、`getSonicSimilarTracks`。
- 新转码协商：`getTranscodeDecision`、`getTranscodeStream`。

## 本轮兼容性修复

1. `getTopSongs` 恢复 `artist` 参数并广告 `topSongsByArtistId`。
2. 播放队列持久化真实 `currentIndex`，完整支持重复歌曲。
3. 用户更新支持可选密码、细粒度角色、最大码率和文件夹授权。
4. 个人设置提供 API Key 查看、复制、轮换和吊销。
5. 分享接受专辑 ID，`star` / `unstar` 正确识别目录式 ID。
6. Artist/Album Info 返回本地元数据；`reportPlayback`、Now Playing 和一次性 Scrobble 已接通，并广告 `playbackReport`。
7. `songLyrics` v2 支持 Enhanced LRC 的逐字/音节时间轴，并在 `enhanced=true` 时返回规范的 `kind`、`cueLine` 和 UTF-8 字节偏移。

剩余视频、Podcast、聊天、Jukebox、声学相似度和新转码协商属于独立产品范围，只有在产品需要时再实现并广告对应扩展。

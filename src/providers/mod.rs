use std::{path::Path, sync::Arc, time::Duration};

use anyhow::Context;
use async_trait::async_trait;
use futures::{StreamExt, stream::FuturesUnordered};
use reqwest::{
    Client,
    header::{HeaderMap, HeaderValue, REFERER, USER_AGENT},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use strsim::normalized_levenshtein;
use tokio::process::Command;

use crate::config::Settings;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetadataCandidate {
    pub id: String,
    pub name: String,
    pub artist: String,
    pub artist_id: String,
    pub album: String,
    pub album_id: String,
    pub album_img: String,
    pub year: String,
    pub tracknumber: String,
    pub discnumber: String,
    pub duration: Option<f64>,
    pub resource: String,
    pub score: f64,
}

#[async_trait]
pub trait MetadataProvider: Send + Sync {
    fn name(&self) -> &'static str;
    async fn search(&self, query: &str) -> anyhow::Result<Vec<MetadataCandidate>>;
    async fn lyrics(&self, id: &str) -> anyhow::Result<String>;
}

pub struct ProviderRegistry {
    providers: Vec<Arc<dyn MetadataProvider>>,
}

impl ProviderRegistry {
    pub fn new(settings: Arc<Settings>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(settings.scraper.timeout_seconds))
            .default_headers(default_headers())
            .build()
            .expect("HTTP client");
        let enabled = &settings.scraper.enabled;
        let mut providers: Vec<Arc<dyn MetadataProvider>> = Vec::new();
        if enabled.iter().any(|v| v == "netease") {
            providers.push(Arc::new(NetEase {
                client: client.clone(),
            }));
        }
        if enabled.iter().any(|v| v == "qmusic") {
            providers.push(Arc::new(QMusic {
                client: client.clone(),
            }));
        }
        if enabled.iter().any(|v| v == "migu") {
            providers.push(Arc::new(Migu {
                client: client.clone(),
            }));
        }
        if enabled.iter().any(|v| v == "kuwo") {
            providers.push(Arc::new(Kuwo {
                client: client.clone(),
            }));
        }
        if enabled.iter().any(|v| v == "kugou") {
            providers.push(Arc::new(Kugou {
                client: client.clone(),
            }));
        }
        if enabled.iter().any(|v| v == "acoustid") {
            providers.push(Arc::new(AcoustId {
                client,
                api_key: settings.scraper.acoustid_api_key.clone(),
                fpcalc: settings.tools.fpcalc.clone(),
            }));
        }
        Self { providers }
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.providers.iter().map(|v| v.name()).collect()
    }

    pub async fn search(
        &self,
        resource: &str,
        query: &str,
        current_artist: &str,
        current_album: &str,
    ) -> anyhow::Result<Vec<MetadataCandidate>> {
        if resource != "smart_tag" {
            let provider = self
                .providers
                .iter()
                .find(|p| p.name() == resource)
                .context("unsupported metadata provider")?;
            return Ok(score(
                provider.search(query).await?,
                query,
                current_artist,
                current_album,
            ));
        }
        let mut tasks = FuturesUnordered::new();
        for provider in &self.providers {
            if provider.name() == "acoustid" {
                continue;
            }
            let provider = provider.clone();
            let query = query.to_owned();
            tasks.push(async move { provider.search(&query).await.unwrap_or_default() });
        }
        let mut candidates = Vec::new();
        while let Some(mut result) = tasks.next().await {
            candidates.append(&mut result);
        }
        let mut candidates = score(candidates, query, current_artist, current_album);
        candidates.truncate(15);
        Ok(candidates)
    }

    pub async fn fingerprint(&self, path: &Path) -> anyhow::Result<Vec<MetadataCandidate>> {
        let provider = self
            .providers
            .iter()
            .find(|p| p.name() == "acoustid")
            .context("AcoustID is disabled")?;
        provider.search(&path.to_string_lossy()).await
    }

    pub async fn lyrics(&self, resource: &str, id: &str) -> anyhow::Result<String> {
        let provider = self
            .providers
            .iter()
            .find(|p| p.name() == resource)
            .context("unsupported metadata provider")?;
        provider.lyrics(id).await
    }
}

fn score(
    mut values: Vec<MetadataCandidate>,
    title: &str,
    artist: &str,
    album: &str,
) -> Vec<MetadataCandidate> {
    for value in &mut values {
        let title_score = normalized_levenshtein(&normalize(title), &normalize(&value.name));
        let artist_score = if artist.is_empty() {
            0.5
        } else {
            normalized_levenshtein(&normalize(artist), &normalize(&value.artist))
        };
        let album_score = if album.is_empty() {
            0.5
        } else {
            normalized_levenshtein(&normalize(album), &normalize(&value.album))
        };
        value.score = title_score * 0.6 + artist_score * 0.25 + album_score * 0.15;
    }
    values.retain(|v| v.score >= 0.25);
    values.sort_by(|a, b| b.score.total_cmp(&a.score));
    values
}

fn normalize(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

fn default_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36",
        ),
    );
    headers.insert(REFERER, HeaderValue::from_static("https://music.163.com/"));
    headers
}

struct NetEase {
    client: Client,
}
#[async_trait]
impl MetadataProvider for NetEase {
    fn name(&self) -> &'static str {
        "netease"
    }
    async fn search(&self, query: &str) -> anyhow::Result<Vec<MetadataCandidate>> {
        let json: Value = self
            .client
            .get("https://music.163.com/api/cloudsearch/pc")
            .query(&[
                ("s", query),
                ("type", "1"),
                ("limit", "10"),
                ("offset", "0"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(json
            .pointer("/result/songs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|song| netease_candidate(song, None, self.name()))
            .collect())
    }
    async fn lyrics(&self, id: &str) -> anyhow::Result<String> {
        let json: Value = self
            .client
            .get("https://music.163.com/api/song/lyric")
            .query(&[("id", id), ("lv", "-1")])
            .send()
            .await?
            .json()
            .await?;
        Ok(stringish(json.pointer("/lrc/lyric")))
    }
}

fn netease_candidate(song: &Value, detail: Option<&Value>, resource: &str) -> MetadataCandidate {
    let detail = detail.unwrap_or(song);
    let search_album = song
        .get("album")
        .or_else(|| song.get("al"))
        .unwrap_or(&Value::Null);
    let album = detail
        .get("album")
        .or_else(|| detail.get("al"))
        .unwrap_or(search_album);
    let artists = detail
        .get("artists")
        .or_else(|| detail.get("ar"))
        .and_then(Value::as_array)
        .or_else(|| {
            song.get("artists")
                .or_else(|| song.get("ar"))
                .and_then(Value::as_array)
        });
    let album_img = secure_url(
        [
            album.get("picUrl"),
            album.get("blurPicUrl"),
            search_album.get("picUrl"),
            search_album.get("blurPicUrl"),
        ]
        .into_iter()
        .map(stringish)
        .find(|value| !value.is_empty())
        .unwrap_or_default(),
    );
    let year = [
        detail.get("publishTime"),
        album.get("publishTime"),
        song.get("publishTime"),
        search_album.get("publishTime"),
    ]
    .into_iter()
    .map(parse_year)
    .find(|value| !value.is_empty())
    .unwrap_or_default();
    MetadataCandidate {
        id: stringish(detail.get("id").or_else(|| song.get("id"))),
        name: stringish(detail.get("name").or_else(|| song.get("name"))),
        artist: artists
            .into_iter()
            .flatten()
            .filter_map(|artist| artist.get("name").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("; "),
        artist_id: artists
            .and_then(|values| values.first())
            .map(|artist| stringish(artist.get("id")))
            .unwrap_or_default(),
        album: stringish(album.get("name")),
        album_id: stringish(album.get("id")),
        album_img,
        year,
        tracknumber: stringish(
            detail
                .get("no")
                .or_else(|| detail.get("trackNo"))
                .or_else(|| song.get("no")),
        ),
        discnumber: stringish(
            detail
                .get("disc")
                .or_else(|| detail.get("cd"))
                .or_else(|| song.get("disc")),
        ),
        duration: millis(
            detail
                .get("duration")
                .or_else(|| detail.get("dt"))
                .or_else(|| song.get("duration"))
                .or_else(|| song.get("dt")),
        ),
        resource: resource.into(),
        score: 0.0,
    }
}

struct QMusic {
    client: Client,
}
#[async_trait]
impl MetadataProvider for QMusic {
    fn name(&self) -> &'static str {
        "qmusic"
    }
    async fn search(&self, query: &str) -> anyhow::Result<Vec<MetadataCandidate>> {
        let json: Value = self
            .client
            .get("https://c.y.qq.com/soso/fcgi-bin/client_search_cp")
            .header(REFERER, "https://y.qq.com/")
            .query(&[("w", query), ("p", "1"), ("n", "10"), ("format", "json")])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(json
            .pointer("/data/song/list")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|song| {
                let album_mid = stringish(song.get("albummid"));
                MetadataCandidate {
                    id: stringish(song.get("songmid")),
                    name: stringish(song.get("songname")),
                    artist: song
                        .get("singer")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|a| a.get("name").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("; "),
                    artist_id: song
                        .pointer("/singer/0/mid")
                        .map(|v| stringish(Some(v)))
                        .unwrap_or_default(),
                    album: stringish(song.get("albumname")),
                    album_id: album_mid.clone(),
                    album_img: if album_mid.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "https://y.gtimg.cn/music/photo_new/T002R300x300M000{album_mid}.jpg"
                        )
                    },
                    year: parse_year(song.get("pubtime")),
                    tracknumber: stringish(
                        song.get("index_album")
                            .or_else(|| song.get("songnum"))
                            .or_else(|| song.get("cdIdx")),
                    ),
                    discnumber: stringish(
                        song.get("index_cd")
                            .or_else(|| song.get("belongCD"))
                            .or_else(|| song.get("disc")),
                    ),
                    duration: song.get("interval").and_then(Value::as_f64),
                    resource: self.name().into(),
                    score: 0.0,
                }
            })
            .collect())
    }
    async fn lyrics(&self, id: &str) -> anyhow::Result<String> {
        let text = self
            .client
            .get("https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg")
            .header(REFERER, "https://y.qq.com/")
            .query(&[("songmid", id), ("format", "json"), ("nobase64", "1")])
            .send()
            .await?
            .text()
            .await?;
        let json: Value = serde_json::from_str(&text)?;
        Ok(stringish(json.get("lyric")))
    }
}

struct Migu {
    client: Client,
}
#[async_trait]
impl MetadataProvider for Migu {
    fn name(&self) -> &'static str {
        "migu"
    }
    async fn search(&self, query: &str) -> anyhow::Result<Vec<MetadataCandidate>> {
        let json: Value = self
            .client
            .get("https://c.musicapp.migu.cn/v1.0/content/search_all.do")
            .header(REFERER, "https://y.migu.cn/")
            .query(&[
                ("text", query),
                ("pageNo", "1"),
                ("pageSize", "10"),
                ("isCopyright", "1"),
                ("sort", "1"),
                (
                    "searchSwitch",
                    r#"{"song":1,"album":0,"singer":0,"tagSong":1,"mvSong":0,"bestShow":1}"#,
                ),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(json
            .pointer("/songResultData/result")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|song| {
                let lyric_url = stringish(song.get("lyricUrl").or_else(|| song.get("mrcurl")));
                let album = song
                    .get("albums")
                    .and_then(Value::as_array)
                    .and_then(|values| values.first());
                let singers = song.get("singers").and_then(Value::as_array);
                let album_img = song
                    .get("imgItems")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .find(|image| stringish(image.get("imgSizeType")) == "02")
                    .or_else(|| {
                        song.get("imgItems")
                            .and_then(Value::as_array)
                            .and_then(|values| values.first())
                    })
                    .map(|image| secure_url(stringish(image.get("img"))))
                    .unwrap_or_default();
                MetadataCandidate {
                    id: if lyric_url.is_empty() {
                        stringish(song.get("copyrightId").or_else(|| song.get("contentId")))
                    } else {
                        lyric_url
                    },
                    name: stringish(song.get("name").or_else(|| song.get("songName"))),
                    artist: singers
                        .into_iter()
                        .flatten()
                        .filter_map(|artist| artist.get("name").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("; "),
                    artist_id: singers
                        .and_then(|values| values.first())
                        .map(|artist| stringish(artist.get("id")))
                        .unwrap_or_default(),
                    album: album
                        .map(|value| stringish(value.get("name")))
                        .unwrap_or_default(),
                    album_id: album
                        .map(|value| stringish(value.get("id")))
                        .unwrap_or_default(),
                    album_img,
                    year: parse_year(
                        song.get("publishDate")
                            .or_else(|| song.get("publishTime"))
                            .or_else(|| song.get("releaseDate")),
                    ),
                    tracknumber: stringish(song.get("trackNumber").or_else(|| song.get("track"))),
                    discnumber: stringish(song.get("discNumber").or_else(|| song.get("disc"))),
                    duration: seconds(song.get("duration")),
                    resource: self.name().into(),
                    score: 0.0,
                }
            })
            .collect())
    }
    async fn lyrics(&self, id: &str) -> anyhow::Result<String> {
        if id.starts_with("http://") || id.starts_with("https://") {
            return Ok(self
                .client
                .get(id)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?);
        }
        let json: Value = self
            .client
            .get("https://music.migu.cn/v3/api/music/audioPlayer/getLyric")
            .query(&[("copyrightId", id)])
            .send()
            .await?
            .json()
            .await?;
        Ok(stringish(json.get("lyric")))
    }
}

struct Kuwo {
    client: Client,
}
#[async_trait]
impl MetadataProvider for Kuwo {
    fn name(&self) -> &'static str {
        "kuwo"
    }
    async fn search(&self, query: &str) -> anyhow::Result<Vec<MetadataCandidate>> {
        let json: Value = self
            .client
            .get("https://search.kuwo.cn/r.s")
            .header(REFERER, "https://www.kuwo.cn/")
            .query(&[
                ("client", "kt"),
                ("all", query),
                ("pn", "0"),
                ("rn", "10"),
                ("uid", "794762570"),
                ("ver", "kwplayer_ar_9.2.2.1"),
                ("vipver", "1"),
                ("show_copyright_off", "1"),
                ("newver", "1"),
                ("ft", "music"),
                ("cluster", "0"),
                ("strategy", "2012"),
                ("encoding", "utf8"),
                ("rformat", "json"),
                ("mobi", "1"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(json
            .get("abslist")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|song| {
                let cover_path = stringish(
                    song.get("web_albumpic_short")
                        .or_else(|| song.get("ALBUMPIC")),
                );
                MetadataCandidate {
                    id: stringish(
                        song.get("DC_TARGETID")
                            .or_else(|| song.get("MUSICRID"))
                            .or_else(|| song.get("rid")),
                    )
                    .trim_start_matches("MUSIC_")
                    .to_owned(),
                    name: clean_text(&stringish(
                        song.get("SONGNAME").or_else(|| song.get("NAME")),
                    )),
                    artist: clean_text(&stringish(song.get("ARTIST"))),
                    artist_id: stringish(song.get("ARTISTID")),
                    album: clean_text(&stringish(song.get("ALBUM"))),
                    album_id: stringish(song.get("ALBUMID")),
                    album_img: if cover_path.is_empty() {
                        secure_url(stringish(song.get("hts_MVPIC")))
                    } else {
                        format!(
                            "https://img1.kuwo.cn/star/albumcover/300/{}",
                            cover_path.trim_start_matches("120/")
                        )
                    },
                    year: parse_year(
                        song.get("releaseDate")
                            .or_else(|| song.get("RELEASEDATE"))
                            .or_else(|| song.get("NEW")),
                    ),
                    tracknumber: stringish(song.get("TRACK").or_else(|| song.get("trackNumber"))),
                    discnumber: stringish(song.get("DISC").or_else(|| song.get("discNumber"))),
                    duration: seconds(song.get("DURATION"))
                        .or_else(|| parse_duration(song.get("songTimeMinutes"))),
                    resource: self.name().into(),
                    score: 0.0,
                }
            })
            .collect())
    }
    async fn lyrics(&self, id: &str) -> anyhow::Result<String> {
        let json: Value = self
            .client
            .get("https://kuwo.cn/newh5/singles/songinfoandlrc")
            .query(&[("musicId", id)])
            .send()
            .await?
            .json()
            .await?;
        Ok(json
            .pointer("/data/lrclist")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|line| {
                format!(
                    "[{}]{}",
                    stringish(line.get("time")),
                    stringish(line.get("lineLyric"))
                )
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

struct Kugou {
    client: Client,
}
#[async_trait]
impl MetadataProvider for Kugou {
    fn name(&self) -> &'static str {
        "kugou"
    }
    async fn search(&self, query: &str) -> anyhow::Result<Vec<MetadataCandidate>> {
        let json: Value = self
            .client
            .get("https://songsearch.kugou.com/song_search_v2")
            .header(REFERER, "https://www.kugou.com/")
            .query(&[
                ("keyword", query),
                ("page", "1"),
                ("pagesize", "10"),
                ("platform", "WebFilter"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(json
            .pointer("/data/lists")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|song| {
                let cover = stringish(song.get("Image")).replace("{size}", "300");
                MetadataCandidate {
                    id: stringish(song.get("FileHash")),
                    name: clean_text(&stringish(song.get("SongName"))),
                    artist: clean_text(&stringish(song.get("SingerName"))).replace('、', "; "),
                    artist_id: stringish(song.get("SingerId")),
                    album: clean_text(&stringish(song.get("AlbumName"))),
                    album_id: stringish(song.get("AlbumID")),
                    album_img: secure_url(if cover.is_empty() {
                        stringish(song.pointer("/trans_param/union_cover")).replace("{size}", "300")
                    } else {
                        cover
                    }),
                    year: parse_year(song.get("PublishDate").or_else(|| song.get("PublishTime"))),
                    tracknumber: stringish(song.get("Track").or_else(|| song.get("TrackNumber"))),
                    discnumber: stringish(song.get("Disc").or_else(|| song.get("DiscNumber"))),
                    duration: seconds(song.get("Duration")),
                    resource: self.name().into(),
                    score: 0.0,
                }
            })
            .collect())
    }
    async fn lyrics(&self, id: &str) -> anyhow::Result<String> {
        Ok(self
            .client
            .get("https://m.kugou.com/app/i/krc.php")
            .query(&[("cmd", "100"), ("timelength", "999999"), ("hash", id)])
            .send()
            .await?
            .text()
            .await?)
    }
}

struct AcoustId {
    client: Client,
    api_key: Option<String>,
    fpcalc: std::path::PathBuf,
}
#[async_trait]
impl MetadataProvider for AcoustId {
    fn name(&self) -> &'static str {
        "acoustid"
    }
    async fn search(&self, path: &str) -> anyhow::Result<Vec<MetadataCandidate>> {
        let configured_key = self
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty() && !key.starts_with("paste-your-"));
        // music-tag-web ships this public AcoustID client identifier. Keep user-provided
        // identifiers preferred, but remain compatible with the reference application.
        let key = configured_key.unwrap_or("cSpUJKpD");
        let output = Command::new(&self.fpcalc)
            .args(["-json", path])
            .output()
            .await?;
        if !output.status.success() {
            anyhow::bail!("fpcalc failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        let fingerprint: Value = serde_json::from_slice(&output.stdout)?;
        let duration = fingerprint
            .get("duration")
            .and_then(Value::as_f64)
            .unwrap_or_default();
        let duration_param = (duration.max(0.0) as u64).to_string();
        let fp = fingerprint
            .get("fingerprint")
            .and_then(Value::as_str)
            .context("fpcalc returned no fingerprint")?;
        let response = self
            .client
            .get("https://api.acoustid.org/v2/lookup")
            .query(&[
                ("client", key),
                ("meta", "recordings releasegroups compress"),
                ("duration", duration_param.as_str()),
                ("fingerprint", fp),
                ("format", "json"),
            ])
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        let json: Value = serde_json::from_str(&body).context("AcoustID 返回了无法解析的数据")?;
        if !status.is_success() || json.get("status").and_then(Value::as_str) == Some("error") {
            let message = stringish(json.pointer("/error/message"));
            anyhow::bail!(
                "AcoustID 指纹识别失败：{}",
                if message.is_empty() {
                    format!("HTTP {status}")
                } else {
                    message
                }
            );
        }
        let mut values = Vec::new();
        for result in json
            .get("results")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for rec in result
                .get("recordings")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let group = rec
                    .get("releasegroups")
                    .and_then(Value::as_array)
                    .and_then(|v| v.first());
                values.push(MetadataCandidate {
                    id: stringish(rec.get("id")),
                    name: stringish(rec.get("title")),
                    artist: rec
                        .get("artists")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|a| a.get("name").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("; "),
                    artist_id: rec
                        .pointer("/artists/0/id")
                        .map(|v| stringish(Some(v)))
                        .unwrap_or_default(),
                    album: group.map(|v| stringish(v.get("title"))).unwrap_or_default(),
                    album_id: group.map(|v| stringish(v.get("id"))).unwrap_or_default(),
                    album_img: String::new(),
                    year: String::new(),
                    tracknumber: String::new(),
                    discnumber: String::new(),
                    duration: Some(duration),
                    resource: self.name().into(),
                    score: result
                        .get("score")
                        .and_then(Value::as_f64)
                        .unwrap_or_default(),
                });
            }
        }
        Ok(values)
    }
    async fn lyrics(&self, _id: &str) -> anyhow::Result<String> {
        anyhow::bail!("AcoustID does not provide lyrics")
    }
}

fn stringish(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(v)) => v.clone(),
        Some(Value::Number(v)) => v.to_string(),
        _ => String::new(),
    }
}
fn clean_text(value: &str) -> String {
    value
        .replace("<em>", "")
        .replace("</em>", "")
        .replace("&amp;", "&")
        .trim()
        .to_owned()
}
fn secure_url(value: String) -> String {
    if let Some(rest) = value.strip_prefix("http://") {
        format!("https://{rest}")
    } else {
        value
    }
}
fn millis(value: Option<&Value>) -> Option<f64> {
    number(value).map(|value| value / 1000.0)
}
fn seconds(value: Option<&Value>) -> Option<f64> {
    number(value)
}
fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(value) => value.as_f64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    }
}
fn parse_duration(value: Option<&Value>) -> Option<f64> {
    let text = value.and_then(Value::as_str)?;
    let (minutes, seconds) = text.split_once(':')?;
    Some(minutes.parse::<f64>().ok()? * 60.0 + seconds.parse::<f64>().ok()?)
}
fn parse_year(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    match value {
        Value::Number(number) => number.as_i64().map(year_from_number).unwrap_or_default(),
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() {
                return String::new();
            }
            if let Ok(number) = text.parse::<i64>() {
                let year = year_from_number(number);
                if !year.is_empty() {
                    return year;
                }
            }
            text.as_bytes()
                .windows(4)
                .find(|window| window.iter().all(u8::is_ascii_digit))
                .and_then(|window| std::str::from_utf8(window).ok())
                .and_then(|year| year.parse::<i32>().ok())
                .filter(|year| (1000..=2999).contains(year))
                .map(|year| year.to_string())
                .unwrap_or_default()
        }
        _ => String::new(),
    }
}
fn year_from_number(value: i64) -> String {
    if (1000..=2999).contains(&value) {
        return value.to_string();
    }
    if value <= 0 {
        return String::new();
    }
    let date = if value >= 10_000_000_000 {
        chrono::DateTime::from_timestamp_millis(value)
    } else {
        chrono::DateTime::from_timestamp(value, 0)
    };
    date.map(|value| value.format("%Y").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_close_title_and_artist_first() {
        let candidates = vec![
            MetadataCandidate {
                name: "Completely Different".into(),
                artist: "Else".into(),
                ..Default::default()
            },
            MetadataCandidate {
                name: "夜曲".into(),
                artist: "周杰伦".into(),
                album: "十一月的萧邦".into(),
                ..Default::default()
            },
        ];
        let ranked = score(candidates, "夜曲", "周杰伦", "十一月的萧邦");
        assert_eq!(ranked[0].name, "夜曲");
    }

    #[test]
    fn netease_detail_enriches_cover_year_and_track_numbers() {
        let search = serde_json::json!({
            "id": 2725685941_i64,
            "name": "夜曲",
            "duration": 233956,
            "artists": [{"id": 98459986, "name": "Xai小爱"}],
            "album": {
                "id": 278269102,
                "name": "夜曲",
                "publishTime": 1751990400000_i64
            }
        });
        let detail = serde_json::json!({
            "id": 2725685941_i64,
            "name": "夜曲",
            "no": 1,
            "disc": "01",
            "duration": 233956,
            "artists": [{"id": 98459986, "name": "Xai小爱"}],
            "album": {
                "id": 278269102,
                "name": "夜曲",
                "picUrl": "https://p2.music.126.net/cover.jpg",
                "publishTime": 1751990400000_i64
            }
        });
        let candidate = netease_candidate(&search, Some(&detail), "netease");
        assert_eq!(candidate.album_img, "https://p2.music.126.net/cover.jpg");
        assert_eq!(candidate.year, "2025");
        assert_eq!(candidate.tracknumber, "1");
        assert_eq!(candidate.discnumber, "01");
    }

    #[test]
    fn normalizes_provider_year_formats() {
        assert_eq!(parse_year(Some(&serde_json::json!(1743091200_i64))), "2025");
        assert_eq!(
            parse_year(Some(&serde_json::json!(1751990400000_i64))),
            "2025"
        );
        assert_eq!(parse_year(Some(&serde_json::json!("2005-11-01"))), "2005");
        assert_eq!(parse_year(Some(&serde_json::json!("2024"))), "2024");
        assert_eq!(parse_year(Some(&serde_json::json!(0))), "");
    }

    #[test]
    fn normalizes_remote_cover_urls_and_markup() {
        assert_eq!(
            secure_url("http://imge.kugou.com/cover.jpg".into()),
            "https://imge.kugou.com/cover.jpg"
        );
        assert_eq!(clean_text("<em>夜曲</em>"), "夜曲");
    }
}

use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use lofty::{file::FileType, probe::Probe};
use md5::{Digest, Md5};
use reqwest::{Client, RequestBuilder, Response, Url, header};
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{process::Command, sync::Mutex};
use uuid::Uuid;
use wasmi::{
    Caller, Config as WasmiConfig, Engine as WasmiEngine, Extern, ExternType, Func, Linker,
    Module as WasmiModule, Ref as WasmiRef, Store, Val,
};

use crate::artist_credit;

pub const DOWNLOAD_FILENAME_FORMAT_KEY: &str = "download_filename_format";
pub const DEFAULT_DOWNLOAD_FILENAME_FORMAT: &str = "artist-title";
pub const DOWNLOAD_FILENAME_FORMATS: &[&str] = &["artist-title", "title-artist"];

#[derive(Debug, Clone)]
pub struct RemoteConnection {
    pub source: String,
    pub gateway_url: String,
    pub cookie: String,
    pub subsonic_url: String,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSearchRequest {
    pub source_id: String,
    pub query: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteImportRequest {
    pub source_id: String,
    pub song: RemoteSong,
    pub quality: String,
    pub root_id: String,
    #[serde(default)]
    pub directory: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteQuality {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSong {
    pub source: String,
    pub id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: String,
    pub suffix: String,
    pub bit_rate: Option<i64>,
    pub qualities: Vec<RemoteQuality>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteImportPayload {
    pub download_url: String,
    pub root_id: String,
    pub directory: String,
    pub filename: String,
    pub source: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NeteaseLoginStart {
    pub key: String,
    pub qr_image: String,
    pub cookie: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NeteaseLoginStatus {
    pub code: i64,
    pub message: String,
    pub cookie: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeteaseAudioMatchRequest {
    pub duration: u8,
    pub audio_fp: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NeteaseAudioMatchResult {
    pub id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: String,
    pub start_time_ms: i64,
}

const NETEASE_AUDIO_MATCH_RUNTIME_MAX_BYTES: usize = 512 * 1024;
const NETEASE_AUDIO_MATCH_SAMPLE_BYTES: usize = 24_000 * size_of::<f32>();
pub const NETEASE_AUDIO_MATCH_MEDIA_MAX_BYTES: usize = 8 * 1024 * 1024;
const NETEASE_AUDIO_MATCH_DECODE_TIMEOUT: Duration = Duration::from_secs(15);
const NETEASE_AUDIO_MATCH_WASM_TIMEOUT: Duration = Duration::from_secs(15);
const NETEASE_AUDIO_MATCH_RUNTIME_CACHE_ENTRIES: usize = 8;
const NETEASE_AUDIO_MATCH_WASM_SHA256: &str =
    "8064415bb66e45410e88877f07cd007a3ae62ebf463aac13c159a8f13e4c8fe8";
const NETEASE_AUDIO_MATCH_WASM_FUEL: u64 = 200_000_000;

// ABI of the pinned Emscripten build above. GenerateFP is only a thin JS wrapper around these
// table functions: ExtractQueryFP(std::string), std::vector<char> destruction, and libc malloc.
const NETEASE_EXTRACT_TABLE_INDEX: u64 = 15;
const NETEASE_EXTRACT_CONTEXT: i32 = 16;
const NETEASE_VECTOR_DESTRUCTOR_TABLE_INDEX: u64 = 2;
const NETEASE_AUDIO_MATCH_RUNTIME_FILES: [(&str, &str); 2] = [
    (
        "afp.wasm.js",
        "4926a6d69527a1afbc7f7d120b9c21947e677d80c484c8b65572fa7d4ce3b99f",
    ),
    (
        "afp.js",
        "3776f3122d8a516d716ec00f55c24b919f03a2fb4bb009191007327170d33763",
    ),
];

pub async fn search(connection: &RemoteConnection, query: &str) -> anyhow::Result<Vec<RemoteSong>> {
    let query = query.trim();
    anyhow::ensure!(!query.is_empty(), "请输入搜索关键词");
    anyhow::ensure!(query.len() <= 512, "搜索关键词过长");
    let client = client()?;
    match connection.source.as_str() {
        "netease" => search_netease(&client, connection, query).await,
        "qq" => search_qq(&client, connection, query).await,
        "qq2" => search_qq2(&client, connection, query).await,
        "subsonic" => search_subsonic(&client, connection, query).await,
        _ => anyhow::bail!("不支持的下载来源"),
    }
}

pub async fn prepare_import(
    connection: &RemoteConnection,
    request: &RemoteImportRequest,
    filename_format: &str,
) -> anyhow::Result<RemoteImportPayload> {
    anyhow::ensure!(request.song.source == connection.source, "下载来源不匹配");
    anyhow::ensure!(
        !request.song.id.trim().is_empty() && request.song.id.len() <= 512,
        "歌曲 ID 无效"
    );
    anyhow::ensure!(
        !request.song.title.trim().is_empty() && request.song.title.len() <= 512,
        "歌曲标题无效"
    );
    anyhow::ensure!(
        request.song.artists.len() <= 64
            && request.song.artists.iter().map(String::len).sum::<usize>() <= 2048,
        "艺术家信息过长"
    );
    anyhow::ensure!(request.song.suffix.len() <= 32, "文件扩展名过长");
    anyhow::ensure!(request.quality.len() <= 32, "码率参数过长");
    anyhow::ensure!(request.directory.len() <= 1024, "下载目录过长");
    anyhow::ensure!(
        request.directory.split(['/', '\\']).count() <= 16,
        "下载目录层级过深"
    );
    let client = client()?;
    let download_url = match connection.source.as_str() {
        "netease" => {
            resolve_netease(&client, connection, &request.song.id, &request.quality).await?
        }
        "qq" => resolve_qq(&client, connection, &request.song.id, &request.quality).await?,
        "qq2" => resolve_qq2(&client, connection, &request.song.id, &request.quality).await?,
        "subsonic" => subsonic_download_url(connection, &request.song.id, &request.quality)?,
        _ => anyhow::bail!("不支持的下载来源"),
    };
    validate_http_url(&download_url)?;
    let filename = import_filename(&request.song, &request.quality, filename_format)?;
    Ok(RemoteImportPayload {
        download_url,
        root_id: request.root_id.clone(),
        directory: request.directory.trim().to_owned(),
        filename,
        source: request.song.source.clone(),
        title: request.song.title.trim().to_owned(),
    })
}

fn import_filename(
    song: &RemoteSong,
    quality: &str,
    filename_format: &str,
) -> anyhow::Result<String> {
    anyhow::ensure!(
        DOWNLOAD_FILENAME_FORMATS.contains(&filename_format),
        "下载文件名格式无效"
    );
    let extension = extension_for(song, quality);
    let joined_artists = song.artists.join("; ");
    let artists = if joined_artists.trim().is_empty() {
        String::new()
    } else {
        artist_credit::parse_artist_names(&joined_artists).join(", ")
    };
    let title = song.title.trim();
    let stem = if artists.is_empty() {
        title.to_owned()
    } else if filename_format == "title-artist" {
        format!("{title} - {artists}")
    } else {
        format!("{artists} - {title}")
    };
    Ok(format!("{}.{}", safe_component(&stem), extension))
}

pub async fn preview_stream(
    connection: &RemoteConnection,
    song_id: &str,
    range: Option<&str>,
) -> anyhow::Result<reqwest::Response> {
    anyhow::ensure!(!song_id.trim().is_empty(), "试听歌曲 ID 不能为空");
    let client = client()?;
    let download_url = match connection.source.as_str() {
        "netease" => resolve_netease(&client, connection, song_id, "128").await?,
        "qq" => resolve_qq(&client, connection, song_id, "128").await?,
        "qq2" => resolve_qq2(&client, connection, song_id, "128").await?,
        "subsonic" => subsonic_download_url(connection, song_id, "128")?,
        _ => anyhow::bail!("不支持的下载来源"),
    };
    validate_http_url(&download_url)?;
    let mut request = client.get(download_url);
    if let Some(range) = range.filter(|value| !value.trim().is_empty()) {
        request = request.header(header::RANGE, range);
    }
    send_checked(request).await
}

pub async fn netease_login_start(
    connection: &RemoteConnection,
) -> anyhow::Result<NeteaseLoginStart> {
    anyhow::ensure!(connection.source == "netease", "这个来源不是网易云后端");
    let client = client()?;
    let timestamp = timestamp_millis();
    let mut key_url = gateway_url(connection, "/login/qr/key")?;
    key_url
        .query_pairs_mut()
        .append_pair("timestamp", &timestamp)
        .append_pair("ua", "pc");
    let (key_value, first_cookie) = send_gateway_with_cookie(&client, connection, key_url).await?;
    let key = stringish(key_value.pointer("/data/unikey"));
    anyhow::ensure!(!key.is_empty(), "网易云后端没有返回二维码 key");
    let mut next = connection.clone();
    next.cookie = first_cookie;
    let mut qr_url = gateway_url(&next, "/login/qr/create")?;
    qr_url
        .query_pairs_mut()
        .append_pair("key", &key)
        .append_pair("qrimg", "true")
        .append_pair("timestamp", &timestamp)
        .append_pair("ua", "pc");
    let (qr_value, cookie) = send_gateway_with_cookie(&client, &next, qr_url).await?;
    let qr_image = stringish(qr_value.pointer("/data/qrimg"));
    anyhow::ensure!(!qr_image.is_empty(), "网易云后端没有返回二维码图片");
    Ok(NeteaseLoginStart {
        key,
        qr_image,
        cookie,
    })
}

pub async fn netease_login_check(
    connection: &RemoteConnection,
    key: &str,
) -> anyhow::Result<NeteaseLoginStatus> {
    anyhow::ensure!(connection.source == "netease", "这个来源不是网易云后端");
    anyhow::ensure!(!key.trim().is_empty(), "二维码 key 不能为空");
    let client = client()?;
    let mut url = gateway_url(connection, "/login/qr/check")?;
    url.query_pairs_mut()
        .append_pair("key", key)
        .append_pair("timestamp", &timestamp_millis())
        .append_pair("ua", "pc");
    let (value, header_cookie) = send_gateway_with_cookie(&client, connection, url).await?;
    let body_cookie = stringish(
        value
            .get("cookie")
            .or_else(|| value.pointer("/data/cookie")),
    );
    let cookie = merge_cookie(&header_cookie, &body_cookie);
    Ok(NeteaseLoginStatus {
        code: value
            .get("code")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        message: stringish(value.get("message").or_else(|| value.get("msg"))),
        cookie,
    })
}

pub async fn netease_account_name(connection: &RemoteConnection) -> anyhow::Result<String> {
    anyhow::ensure!(connection.source == "netease", "这个来源不是网易云后端");
    anyhow::ensure!(
        !connection.cookie.trim().is_empty(),
        "网易云 Cookie 不能为空"
    );
    let client = client()?;
    let mut url = gateway_url(connection, "/login/status")?;
    url.query_pairs_mut()
        .append_pair("timestamp", &timestamp_millis());
    let value = send_gateway(&client, connection, url).await?;
    let account_name = stringish(
        value
            .pointer("/data/profile/nickname")
            .or_else(|| value.pointer("/profile/nickname")),
    );
    anyhow::ensure!(!account_name.is_empty(), "网易云没有返回登录用户名");
    Ok(account_name)
}

pub async fn netease_audio_match(
    connection: &RemoteConnection,
    request: &NeteaseAudioMatchRequest,
) -> anyhow::Result<Vec<NeteaseAudioMatchResult>> {
    anyhow::ensure!(connection.source == "netease", "这个来源不是网易云后端");
    anyhow::ensure!(request.duration == 3, "听歌识曲只接受3秒音频指纹");
    anyhow::ensure!(
        !request.audio_fp.is_empty() && request.audio_fp.len() <= 64 * 1024,
        "音频指纹无效"
    );
    anyhow::ensure!(
        request
            .audio_fp
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'+' | b'/' | b'=')),
        "音频指纹格式无效"
    );
    let client = client()?;
    let mut url = gateway_url(connection, "/audio/match")?;
    url.query_pairs_mut()
        .append_pair("duration", &request.duration.to_string())
        .append_pair("audioFP", &request.audio_fp);
    let mut upstream = client.post(url);
    if !connection.cookie.trim().is_empty() {
        upstream = upstream.header(header::COOKIE, connection.cookie.trim());
    }
    let value = send_checked(upstream)
        .await?
        .json::<Value>()
        .await
        .map_err(|error| anyhow::anyhow!(error.without_url()))?;
    anyhow::ensure!(
        value.get("code").and_then(Value::as_i64).unwrap_or(200) == 200,
        "网易云听歌识曲接口返回失败"
    );
    Ok(normalize_netease_audio_matches(&value))
}

pub async fn netease_audio_match_runtime(connection: &RemoteConnection) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(connection.source == "netease", "这个来源不是网易云后端");
    let client = client()?;
    let mut runtime = Vec::new();
    for (file, expected_sha256) in NETEASE_AUDIO_MATCH_RUNTIME_FILES {
        let bytes =
            fetch_netease_audio_match_runtime_file(&client, connection, file, expected_sha256)
                .await?;
        runtime.extend_from_slice(&bytes);
        runtime.push(b'\n');
    }
    Ok(runtime)
}

async fn fetch_netease_audio_match_runtime_file(
    client: &Client,
    connection: &RemoteConnection,
    file: &str,
    expected_sha256: &str,
) -> anyhow::Result<Vec<u8>> {
    let url = gateway_url(connection, &format!("/audio_match_demo/{file}"))?;
    let response = send_checked(client.get(url)).await?;
    if let Some(length) = response.content_length() {
        anyhow::ensure!(
            length <= NETEASE_AUDIO_MATCH_RUNTIME_MAX_BYTES as u64,
            "网易云听歌识曲运行时文件过大"
        );
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| anyhow::anyhow!(error.without_url()))?;
    anyhow::ensure!(
        bytes.len() <= NETEASE_AUDIO_MATCH_RUNTIME_MAX_BYTES,
        "网易云听歌识曲运行时文件过大"
    );
    let bytes = normalize_netease_audio_match_runtime_file(file, bytes.to_vec())?;
    let actual_sha256 = hex::encode(digest(&SHA256, &bytes));
    anyhow::ensure!(
        actual_sha256 == expected_sha256,
        "网易云听歌识曲运行时 {file} 校验失败，请更新网易云下载源后端"
    );
    Ok(bytes)
}

pub async fn netease_audio_fingerprint_from_media(
    ffmpeg: &Path,
    connection: &RemoteConnection,
    media_sample: &[u8],
) -> anyhow::Result<String> {
    anyhow::ensure!(
        !media_sample.is_empty() && media_sample.len() <= NETEASE_AUDIO_MATCH_MEDIA_MAX_BYTES,
        "听歌识曲媒体样本无效"
    );
    let directory = tempfile::tempdir().context("无法创建听歌识曲解码目录")?;
    let media_path = directory.path().join("radio-sample.media");
    tokio::fs::write(&media_path, media_sample)
        .await
        .context("无法保存听歌识曲媒体样本")?;
    let pcm_f32le = decode_netease_audio_sample(ffmpeg, &media_path).await?;
    let wasm = cached_netease_audio_match_wasm(connection).await?;
    tokio::time::timeout(
        NETEASE_AUDIO_MATCH_WASM_TIMEOUT,
        tokio::task::spawn_blocking(move || run_netease_audio_fingerprint(&wasm, &pcm_f32le)),
    )
    .await
    .context("听歌识曲指纹生成超时")?
    .context("听歌识曲指纹任务异常")?
}

async fn decode_netease_audio_sample(ffmpeg: &Path, media_path: &Path) -> anyhow::Result<Vec<u8>> {
    let mut command = Command::new(ffmpeg);
    command
        .kill_on_drop(true)
        .args(netease_audio_decode_arguments(media_path));
    let output = tokio::time::timeout(NETEASE_AUDIO_MATCH_DECODE_TIMEOUT, command.output())
        .await
        .context("听歌识曲媒体解码超时")?
        .context("无法启动听歌识曲解码器")?;
    anyhow::ensure!(output.status.success(), "听歌识曲媒体解码失败");
    anyhow::ensure!(
        output.stdout.len() >= NETEASE_AUDIO_MATCH_SAMPLE_BYTES,
        "听歌识曲媒体样本不足3秒"
    );
    Ok(output.stdout[..NETEASE_AUDIO_MATCH_SAMPLE_BYTES].to_vec())
}

fn netease_audio_decode_arguments(media_path: &Path) -> Vec<String> {
    vec![
        "-nostdin".into(),
        "-v".into(),
        "error".into(),
        "-protocol_whitelist".into(),
        "file,pipe".into(),
        "-probesize".into(),
        NETEASE_AUDIO_MATCH_MEDIA_MAX_BYTES.to_string(),
        "-analyzeduration".into(),
        "5000000".into(),
        "-i".into(),
        media_path.to_string_lossy().into_owned(),
        "-t".into(),
        "3".into(),
        "-map".into(),
        "0:a:0".into(),
        "-vn".into(),
        "-ac".into(),
        "1".into(),
        "-ar".into(),
        "8000".into(),
        "-c:a".into(),
        "pcm_f32le".into(),
        "-f".into(),
        "f32le".into(),
        "pipe:1".into(),
    ]
}

async fn cached_netease_audio_match_wasm(
    connection: &RemoteConnection,
) -> anyhow::Result<Arc<[u8]>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<[u8]>>>> = OnceLock::new();
    let key = connection.gateway_url.trim_end_matches('/').to_owned();
    if let Some(wasm) = CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .await
        .get(&key)
        .cloned()
    {
        return Ok(wasm);
    }
    anyhow::ensure!(connection.source == "netease", "这个来源不是网易云后端");
    let client = client()?;
    let (file, expected_sha256) = NETEASE_AUDIO_MATCH_RUNTIME_FILES[0];
    let runtime_file =
        fetch_netease_audio_match_runtime_file(&client, connection, file, expected_sha256).await?;
    let wasm = Arc::<[u8]>::from(extract_netease_audio_match_wasm(&runtime_file)?);
    let mut cache = CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .await;
    if cache.len() >= NETEASE_AUDIO_MATCH_RUNTIME_CACHE_ENTRIES && !cache.contains_key(&key) {
        cache.clear();
    }
    cache.insert(key, wasm.clone());
    Ok(wasm)
}

fn extract_netease_audio_match_wasm(runtime_file: &[u8]) -> anyhow::Result<Vec<u8>> {
    extract_netease_audio_match_wasm_with_hash(runtime_file, NETEASE_AUDIO_MATCH_WASM_SHA256)
}

fn extract_netease_audio_match_wasm_with_hash(
    runtime_file: &[u8],
    expected_sha256: &str,
) -> anyhow::Result<Vec<u8>> {
    const MARKER: &str = "const WASM_BINARY = \"";
    let source =
        std::str::from_utf8(runtime_file).context("网易云听歌识曲 WASM 包装文件不是有效文本")?;
    let encoded = source
        .split_once(MARKER)
        .and_then(|(_, suffix)| suffix.split_once('"').map(|(value, _)| value))
        .context("网易云听歌识曲 WASM 包装文件格式无效")?;
    let wasm = STANDARD
        .decode(encoded)
        .context("网易云听歌识曲 WASM 数据无法解码")?;
    anyhow::ensure!(
        wasm.len() <= NETEASE_AUDIO_MATCH_RUNTIME_MAX_BYTES && wasm.starts_with(b"\0asm"),
        "网易云听歌识曲 WASM 数据无效"
    );
    anyhow::ensure!(
        hex::encode(digest(&SHA256, &wasm)) == expected_sha256,
        "网易云听歌识曲 WASM 校验失败"
    );
    Ok(wasm)
}

fn run_netease_audio_fingerprint(wasm: &[u8], pcm_f32le: &[u8]) -> anyhow::Result<String> {
    anyhow::ensure!(
        pcm_f32le.len() == NETEASE_AUDIO_MATCH_SAMPLE_BYTES,
        "听歌识曲 PCM 样本长度无效"
    );
    let mut config = WasmiConfig::default();
    config.consume_fuel(true);
    let engine = WasmiEngine::new(&config);
    let module = WasmiModule::new(&engine, wasm).context("听歌识曲 WASM 无法加载")?;
    let mut store = Store::new(&engine, ());
    store.set_fuel(NETEASE_AUDIO_MATCH_WASM_FUEL)?;
    let mut linker = Linker::new(&engine);

    for import in module.imports() {
        anyhow::ensure!(
            import.module() == "a",
            "听歌识曲 WASM 包含未知导入 {}.{}",
            import.module(),
            import.name()
        );
        let ExternType::Func(func_type) = import.ty() else {
            anyhow::bail!("听歌识曲 WASM 包含非函数导入 {}", import.name());
        };
        let result_types = func_type.results().to_vec();
        let import_name = import.name().to_owned();
        let func = Func::new(
            &mut store,
            func_type.clone(),
            move |mut caller, inputs, results| {
                for (result, ty) in results.iter_mut().zip(&result_types) {
                    *result = Val::default(*ty);
                }
                match import_name.as_str() {
                    // emscripten_memcpy_js
                    "r" => {
                        let destination = inputs[0].i32().unwrap_or_default() as usize;
                        let source = inputs[1].i32().unwrap_or_default() as usize;
                        let length = inputs[2].i32().unwrap_or_default() as usize;
                        let memory = netease_wasm_memory(&mut caller)?;
                        let mut bytes = vec![0_u8; length];
                        memory
                            .read(&caller, source, &mut bytes)
                            .map_err(|error| wasmi::Error::new(error.to_string()))?;
                        memory
                            .write(&mut caller, destination, &bytes)
                            .map_err(|error| wasmi::Error::new(error.to_string()))?;
                    }
                    // fd_write: the fingerprint module only writes diagnostics, so discard them.
                    "i" => {
                        let vectors = inputs[1].i32().unwrap_or_default() as usize;
                        let count = inputs[2].i32().unwrap_or_default() as usize;
                        let written = inputs[3].i32().unwrap_or_default() as usize;
                        let memory = netease_wasm_memory(&mut caller)?;
                        let mut total = 0_u32;
                        for index in 0..count {
                            let mut vector = [0_u8; 8];
                            memory
                                .read(&caller, vectors + index * 8, &mut vector)
                                .map_err(|error| wasmi::Error::new(error.to_string()))?;
                            total = total.saturating_add(u32::from_le_bytes(
                                vector[4..8].try_into().unwrap(),
                            ));
                        }
                        memory
                            .write(&mut caller, written, &total.to_le_bytes())
                            .map_err(|error| wasmi::Error::new(error.to_string()))?;
                    }
                    // Emscripten assertion, exception, abort and OOM handlers.
                    "d" | "f" | "h" | "s" => {
                        return Err(wasmi::Error::new(format!(
                            "fingerprint WASM aborted in import {import_name}"
                        )));
                    }
                    // The remaining imports register Embind types for JavaScript. The native Rust
                    // caller uses the pinned raw ABI and does not need those registrations.
                    _ => {}
                }
                Ok(())
            },
        );
        linker.define(import.module(), import.name(), func)?;
    }

    let instance = linker.instantiate_and_start(&mut store, &module)?;
    instance
        .get_typed_func::<(), ()>(&store, "C")?
        .call(&mut store, ())?;
    let memory = instance
        .get_memory(&store, "B")
        .context("听歌识曲 WASM 没有导出内存")?;
    let table = instance
        .get_table(&store, "D")
        .context("听歌识曲 WASM 没有导出函数表")?;
    let malloc = instance.get_typed_func::<i32, i32>(&store, "E")?;
    let free = instance.get_typed_func::<i32, ()>(&store, "F")?;
    let extract =
        netease_wasm_table_func(&table, &store, NETEASE_EXTRACT_TABLE_INDEX)?
            .typed::<(i32, i32), i32>(&store)?;
    let destroy_vector =
        netease_wasm_table_func(&table, &store, NETEASE_VECTOR_DESTRUCTOR_TABLE_INDEX)?
            .typed::<i32, ()>(&store)?;

    // Embind's std::string wire representation is a byte length followed by the bytes.
    let input_length = pcm_f32le.len() + 5;
    let input = malloc.call(&mut store, i32::try_from(input_length)?)?;
    anyhow::ensure!(input > 0, "听歌识曲 WASM 无法分配输入内存");
    memory.write(
        &mut store,
        input as usize,
        &u32::try_from(pcm_f32le.len())?.to_le_bytes(),
    )?;
    memory.write(&mut store, input as usize + 4, pcm_f32le)?;
    memory.write(&mut store, input as usize + 4 + pcm_f32le.len(), &[0])?;

    let vector = extract.call(&mut store, (NETEASE_EXTRACT_CONTEXT, input));
    let free_result = free.call(&mut store, input);
    let vector = vector.context("听歌识曲 WASM 生成指纹失败")?;
    free_result?;

    // The pinned wasm32 libc++ ABI stores std::vector<char> as begin/end/capacity pointers.
    let fingerprint_result = (|| -> anyhow::Result<Vec<u8>> {
        let mut layout = [0_u8; 12];
        memory.read(&store, vector as usize, &mut layout)?;
        let begin = u32::from_le_bytes(layout[0..4].try_into()?) as usize;
        let end = u32::from_le_bytes(layout[4..8].try_into()?) as usize;
        let capacity = u32::from_le_bytes(layout[8..12].try_into()?) as usize;
        anyhow::ensure!(
            begin <= end && end <= capacity && capacity <= memory.data_size(&store),
            "听歌识曲 WASM 返回了无效向量"
        );
        let length = end - begin;
        anyhow::ensure!(
            length > 0 && length <= 64 * 1024,
            "听歌识曲 WASM 返回长度无效"
        );
        let mut fingerprint = vec![0_u8; length];
        memory.read(&store, begin, &mut fingerprint)?;
        Ok(fingerprint)
    })();
    let destroy_result = destroy_vector.call(&mut store, vector);
    let fingerprint = fingerprint_result?;
    destroy_result?;
    Ok(STANDARD.encode(fingerprint))
}

fn netease_wasm_memory(caller: &mut Caller<'_, ()>) -> Result<wasmi::Memory, wasmi::Error> {
    caller
        .get_export("B")
        .and_then(Extern::into_memory)
        .ok_or_else(|| wasmi::Error::new("fingerprint WASM memory export is missing"))
}

fn netease_wasm_table_func(
    table: &wasmi::Table,
    store: &Store<()>,
    index: u64,
) -> anyhow::Result<Func> {
    match table.get(store, index) {
        Some(Val::FuncRef(WasmiRef::Val(func))) => Ok(func),
        _ => anyhow::bail!("听歌识曲 WASM 函数表缺少索引 {index}"),
    }
}

fn normalize_netease_audio_match_runtime_file(
    file: &str,
    bytes: Vec<u8>,
) -> anyhow::Result<Vec<u8>> {
    if file != "afp.js" {
        return Ok(bytes);
    }

    let source = String::from_utf8(bytes).context("网易云听歌识曲运行时 afp.js 不是有效文本")?;
    Ok(source
        .replace("\r\n", "\n")
        .replace("const logger = require('../../util/logger.js')\n", "")
        .replace("logger.info(", "console.info(")
        .into_bytes())
}

fn normalize_netease_audio_matches(value: &Value) -> Vec<NeteaseAudioMatchResult> {
    value
        .pointer("/data/result")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let song = item.get("song")?;
            let id = stringish(song.get("id"));
            let title = stringish(song.get("name"));
            if id.is_empty() || title.is_empty() {
                return None;
            }
            let artists = names(song.get("artists").or_else(|| song.get("ar")));
            let album = stringish(
                song.pointer("/album/name")
                    .or_else(|| song.pointer("/al/name")),
            );
            Some(NeteaseAudioMatchResult {
                id,
                title,
                artists,
                album,
                start_time_ms: item
                    .get("startTime")
                    .and_then(Value::as_i64)
                    .unwrap_or_default(),
            })
        })
        .take(20)
        .collect()
}

fn client() -> anyhow::Result<Client> {
    Ok(Client::builder()
        .connect_timeout(Duration::from_secs(12))
        .timeout(Duration::from_secs(35))
        .user_agent("mNest/remote-download")
        .build()?)
}

async fn search_netease(
    client: &Client,
    connection: &RemoteConnection,
    query: &str,
) -> anyhow::Result<Vec<RemoteSong>> {
    let mut url = gateway_url(connection, "/cloudsearch")?;
    url.query_pairs_mut().append_pair("keywords", query);
    let value = send_gateway(client, connection, url).await?;
    Ok(value
        .pointer("/result/songs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|song| RemoteSong {
            source: "netease".into(),
            id: stringish(song.get("id")),
            title: stringish(song.get("name")),
            artists: names(song.get("ar")),
            album: stringish(song.pointer("/al/name")),
            suffix: "mp3".into(),
            bit_rate: None,
            qualities: qualities(&[("max", "最大"), ("320", "320k"), ("128", "128k")]),
        })
        .filter(|song| !song.id.is_empty())
        .collect())
}

async fn search_qq(
    client: &Client,
    connection: &RemoteConnection,
    query: &str,
) -> anyhow::Result<Vec<RemoteSong>> {
    let mut url = gateway_url(connection, "/search")?;
    url.query_pairs_mut().append_pair("key", query);
    let value = send_gateway(client, connection, url).await?;
    anyhow::ensure!(
        value.get("result").and_then(Value::as_i64).unwrap_or(100) == 100,
        "QQ 音乐搜索失败"
    );
    Ok(value
        .pointer("/data/list")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|song| remote_qq_song("qq", song))
        .filter(|song| !song.id.is_empty())
        .collect())
}

async fn search_qq2(
    client: &Client,
    connection: &RemoteConnection,
    query: &str,
) -> anyhow::Result<Vec<RemoteSong>> {
    let mut url = gateway_url(connection, "/getSearchByKey")?;
    url.query_pairs_mut().append_pair("key", query);
    let value = send_gateway(client, connection, url).await?;
    Ok(value
        .pointer("/response/data/song/list")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|song| remote_qq_song("qq2", song))
        .filter(|song| !song.id.is_empty())
        .collect())
}

fn remote_qq_song(source: &str, song: &Value) -> RemoteSong {
    let mut available = Vec::new();
    for (field, id, label) in [
        ("sizeflac", "flac", "FLAC"),
        ("sizeape", "ape", "APE"),
        ("size320", "320", "320k"),
        ("size128", "128", "128k"),
    ] {
        if truthy_size(song.get(field)) {
            available.push(RemoteQuality {
                id: id.into(),
                label: label.into(),
            });
        }
    }
    if available.is_empty() {
        available = qualities(&[("320", "320k"), ("128", "128k")]);
    }
    RemoteSong {
        source: source.into(),
        id: stringish(song.get("songmid").or_else(|| song.get("id"))),
        title: stringish(song.get("songname").or_else(|| song.get("name"))),
        artists: names(song.get("singer")),
        album: stringish(
            song.get("albumname")
                .or_else(|| song.pointer("/album/name")),
        ),
        suffix: stringish(song.get("filetype")),
        bit_rate: song.get("bitRate").and_then(Value::as_i64),
        qualities: available,
    }
}

async fn search_subsonic(
    client: &Client,
    connection: &RemoteConnection,
    query: &str,
) -> anyhow::Result<Vec<RemoteSong>> {
    let mut url = subsonic_url(connection, "search3")?;
    url.query_pairs_mut()
        .append_pair("query", query)
        .append_pair("songCount", "80")
        .append_pair("albumCount", "0")
        .append_pair("artistCount", "0");
    let value = send_checked(client.get(url))
        .await?
        .json::<Value>()
        .await
        .map_err(|error| anyhow::anyhow!(error.without_url()))?;
    let envelope = value
        .get("subsonic-response")
        .context("Subsonic 返回格式无效")?;
    ensure_subsonic_ok(envelope)?;
    Ok(envelope
        .pointer("/searchResult3/song")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|song| {
            let structured = names(song.get("artists"));
            let artists = if structured.is_empty() {
                vec![stringish(song.get("artist"))]
                    .into_iter()
                    .filter(|name| !name.is_empty())
                    .collect()
            } else {
                structured
            };
            RemoteSong {
                source: "subsonic".into(),
                id: stringish(song.get("id")),
                title: stringish(song.get("title")),
                artists,
                album: stringish(song.get("album")),
                suffix: stringish(song.get("suffix")),
                bit_rate: song.get("bitRate").and_then(Value::as_i64),
                qualities: qualities(&[("original", "原格式"), ("320", "320k"), ("128", "128k")]),
            }
        })
        .filter(|song| !song.id.is_empty())
        .collect())
}

async fn resolve_netease(
    client: &Client,
    connection: &RemoteConnection,
    id: &str,
    quality: &str,
) -> anyhow::Result<String> {
    let mut url = gateway_url(connection, "/song/url")?;
    let bitrate = netease_bitrate(quality);
    url.query_pairs_mut()
        .append_pair("id", id)
        .append_pair("br", bitrate);
    let value = send_gateway(client, connection, url).await?;
    let url = stringish(value.pointer("/data/0/url"));
    anyhow::ensure!(!url.is_empty(), "网易云没有返回可下载地址");
    Ok(url)
}

fn netease_bitrate(quality: &str) -> &'static str {
    match quality {
        "max" => "999000",
        "128" => "128000",
        _ => "320000",
    }
}

async fn resolve_qq(
    client: &Client,
    connection: &RemoteConnection,
    id: &str,
    quality: &str,
) -> anyhow::Result<String> {
    let mut url = gateway_url(connection, "/song/url")?;
    url.query_pairs_mut()
        .append_pair("id", id)
        .append_pair("type", quality);
    let value = send_gateway(client, connection, url).await?;
    anyhow::ensure!(
        value.get("result").and_then(Value::as_i64) == Some(100),
        "QQ 音乐获取下载地址失败"
    );
    let url = stringish(value.get("data"));
    anyhow::ensure!(!url.is_empty(), "QQ 音乐没有返回可下载地址");
    Ok(url)
}

async fn resolve_qq2(
    client: &Client,
    connection: &RemoteConnection,
    id: &str,
    quality: &str,
) -> anyhow::Result<String> {
    let mut url = gateway_url(connection, "/getMusicPlay")?;
    url.query_pairs_mut()
        .append_pair("songmid", id)
        .append_pair("quality", quality)
        .append_pair("justPlayUrl", "play");
    let value = send_gateway(client, connection, url).await?;
    let item = value
        .pointer(&format!("/data/playUrl/{id}"))
        .context("QQ2 返回格式无效")?;
    anyhow::ensure!(
        item.get("error").and_then(Value::as_bool) != Some(true),
        "QQ2 获取下载地址失败"
    );
    let url = stringish(item.get("url"));
    anyhow::ensure!(!url.is_empty(), "QQ2 没有返回可下载地址");
    Ok(url)
}

fn subsonic_download_url(
    connection: &RemoteConnection,
    id: &str,
    quality: &str,
) -> anyhow::Result<String> {
    let action = if quality == "original" {
        "download"
    } else {
        "stream"
    };
    let mut url = subsonic_url(connection, action)?;
    url.query_pairs_mut().append_pair("id", id);
    if quality != "original" {
        url.query_pairs_mut()
            .append_pair("format", "mp3")
            .append_pair("maxBitRate", quality);
    }
    Ok(url.to_string())
}

async fn send_gateway(
    client: &Client,
    connection: &RemoteConnection,
    url: Url,
) -> anyhow::Result<Value> {
    let mut request = client.get(url);
    if !connection.cookie.trim().is_empty() {
        request = request.header(header::COOKIE, connection.cookie.trim());
    }
    send_checked(request)
        .await?
        .json::<Value>()
        .await
        .map_err(|error| anyhow::anyhow!(error.without_url()))
}

async fn send_gateway_with_cookie(
    client: &Client,
    connection: &RemoteConnection,
    url: Url,
) -> anyhow::Result<(Value, String)> {
    let mut request = client.get(url);
    if !connection.cookie.trim().is_empty() {
        request = request.header(header::COOKIE, connection.cookie.trim());
    }
    let response = send_checked(request).await?;
    let cookie = cookie_from_headers(&connection.cookie, response.headers());
    let value = response
        .json::<Value>()
        .await
        .map_err(|error| anyhow::anyhow!(error.without_url()))?;
    Ok((value, cookie))
}

async fn send_checked(request: RequestBuilder) -> anyhow::Result<Response> {
    let response = request
        .send()
        .await
        .map_err(|error| anyhow::anyhow!(error.without_url()))?;
    response
        .error_for_status()
        .map_err(|error| anyhow::anyhow!(error.without_url()))
}

fn gateway_url(connection: &RemoteConnection, endpoint: &str) -> anyhow::Result<Url> {
    joined_url(&connection.gateway_url, endpoint)
}

fn subsonic_url(connection: &RemoteConnection, action: &str) -> anyhow::Result<Url> {
    anyhow::ensure!(
        !connection.username.trim().is_empty() && !connection.password.is_empty(),
        "Subsonic 用户名和密码不能为空"
    );
    let endpoint = format!("/rest/{action}.view");
    let mut url = joined_url(connection.subsonic_url.trim_end_matches("/rest"), &endpoint)?;
    let salt = Uuid::new_v4().simple().to_string();
    let token = hex::encode(Md5::digest(
        format!("{}{}", connection.password, salt).as_bytes(),
    ));
    url.query_pairs_mut()
        .append_pair("u", connection.username.trim())
        .append_pair("t", &token)
        .append_pair("s", &salt)
        .append_pair("v", "1.16.1")
        .append_pair("c", "mNest-import")
        .append_pair("f", "json");
    Ok(url)
}

fn joined_url(base: &str, endpoint: &str) -> anyhow::Result<Url> {
    let mut url = Url::parse(base.trim()).context("服务器地址无效")?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https"),
        "只支持 HTTP 或 HTTPS 服务器"
    );
    let prefix = url.path().trim_end_matches('/');
    url.set_path(&format!("{prefix}{endpoint}"));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn validate_http_url(value: &str) -> anyhow::Result<()> {
    let url = Url::parse(value).context("下载地址无效")?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https"),
        "下载地址必须使用 HTTP 或 HTTPS"
    );
    Ok(())
}

fn ensure_subsonic_ok(envelope: &Value) -> anyhow::Result<()> {
    if envelope.get("status").and_then(Value::as_str) == Some("ok") {
        Ok(())
    } else {
        anyhow::bail!("{}", stringish(envelope.pointer("/error/message")).trim())
    }
}

fn extension_for(song: &RemoteSong, quality: &str) -> String {
    if matches!(quality, "320" | "128") {
        return "mp3".into();
    }
    let candidate = if matches!(quality, "original" | "max") {
        &song.suffix
    } else {
        quality
    };
    let sanitized = candidate
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    if sanitized.is_empty() || sanitized.len() > 8 {
        "audio".into()
    } else {
        sanitized
    }
}

pub(crate) fn downloaded_audio_extension(
    path: &Path,
    expected_extension: &str,
    content_type: Option<&str>,
) -> anyhow::Result<String> {
    let expected_extension = expected_extension.trim().to_ascii_lowercase();
    let file_type = Probe::open(path)?.guess_file_type()?.file_type();
    let detected = match file_type {
        Some(FileType::Aac) => Some("aac"),
        Some(FileType::Aiff) if matches!(expected_extension.as_str(), "aif" | "aiff") => {
            Some(expected_extension.as_str())
        }
        Some(FileType::Aiff) => Some("aiff"),
        Some(FileType::Ape) => Some("ape"),
        Some(FileType::Flac) => Some("flac"),
        Some(FileType::Mpeg) if matches!(expected_extension.as_str(), "mp1" | "mp2" | "mp3") => {
            Some(expected_extension.as_str())
        }
        Some(FileType::Mpeg) => Some("mp3"),
        Some(FileType::Mp4) if matches!(expected_extension.as_str(), "m4a" | "mp4") => {
            Some(expected_extension.as_str())
        }
        Some(FileType::Mp4) => Some("m4a"),
        Some(FileType::Mpc) => Some("mpc"),
        Some(FileType::Opus) => Some("opus"),
        Some(FileType::Vorbis) => Some("ogg"),
        Some(FileType::Speex) => Some("spx"),
        Some(FileType::Wav) => Some("wav"),
        Some(FileType::WavPack) => Some("wv"),
        Some(FileType::Custom(_)) | None => None,
        _ => None,
    };
    if let Some(extension) = detected {
        return Ok(extension.to_owned());
    }

    let content_type = content_type
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if content_type.starts_with("text/")
        || content_type.contains("json")
        || content_type.contains("html")
        || content_type.contains("xml")
    {
        anyhow::bail!("下载源返回了 {content_type}，不是音频文件");
    }

    if matches!(
        expected_extension.as_str(),
        "wma" | "wmv" | "tta" | "dsf" | "dff"
    ) {
        return Ok(expected_extension);
    }

    anyhow::bail!(
        "无法识别下载文件的实际音频格式（响应类型：{}）",
        if content_type.is_empty() {
            "unknown"
        } else {
            &content_type
        }
    )
}

pub fn safe_component(value: &str) -> String {
    let mut value = value
        .trim()
        .chars()
        .map(|character| {
            if matches!(
                character,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'
            ) {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    while value.len() > 160 {
        value.pop();
    }
    if value.is_empty() || matches!(value.as_str(), "." | "..") {
        "download".into()
    } else {
        value
    }
}

fn qualities(values: &[(&str, &str)]) -> Vec<RemoteQuality> {
    values
        .iter()
        .map(|(id, label)| RemoteQuality {
            id: (*id).into(),
            label: (*label).into(),
        })
        .collect()
}

fn names(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let name = stringish(item.get("name").or_else(|| item.get("title")));
            (!name.is_empty()).then_some(name)
        })
        .collect()
}

fn truthy_size(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Number(number)) => number.as_i64().unwrap_or_default() > 0,
        Some(Value::String(value)) => value.parse::<i64>().unwrap_or_default() > 0,
        _ => false,
    }
}

fn stringish(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Number(value)) => value.to_string(),
        Some(Value::Bool(value)) => value.to_string(),
        _ => String::new(),
    }
}

fn timestamp_millis() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

fn cookie_from_headers(existing: &str, headers: &reqwest::header::HeaderMap) -> String {
    let mut cookie = existing.to_owned();
    for value in headers.get_all(header::SET_COOKIE) {
        if let Ok(value) = value.to_str() {
            cookie = merge_cookie(&cookie, value.split(';').next().unwrap_or_default());
        }
    }
    cookie
}

fn merge_cookie(left: &str, right: &str) -> String {
    let mut values = BTreeMap::new();
    for cookie in [left, right] {
        for pair in cookie
            .split(';')
            .map(str::trim)
            .filter(|pair| pair.contains('='))
        {
            if let Some((name, value)) = pair.split_once('=') {
                values.insert(name.trim().to_owned(), value.trim().to_owned());
            }
        }
    }
    values
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_safe_import_names() {
        let song = RemoteSong {
            source: "qq".into(),
            id: "1".into(),
            title: "A/B?".into(),
            artists: vec!["Singer".into()],
            album: String::new(),
            suffix: "flac".into(),
            bit_rate: None,
            qualities: vec![],
        };
        assert_eq!(extension_for(&song, "original"), "flac");
        assert_eq!(extension_for(&song, "max"), "flac");
        assert_eq!(safe_component("Singer - A/B?"), "Singer - A_B_");
        assert_eq!(
            import_filename(&song, "original", "artist-title").unwrap(),
            "Singer - A_B_.flac"
        );
        assert_eq!(
            import_filename(&song, "original", "title-artist").unwrap(),
            "A_B_ - Singer.flac"
        );
    }

    #[test]
    fn detects_the_downloaded_audio_format_from_file_contents() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("download.part");

        std::fs::write(&path, b"fLaC\0\0\0\0").unwrap();
        assert_eq!(
            downloaded_audio_extension(&path, "flac", Some("application/octet-stream")).unwrap(),
            "flac"
        );

        std::fs::write(&path, [0xff, 0xfb, 0x90, 0x64, 0, 0, 0, 0]).unwrap();
        assert_eq!(
            downloaded_audio_extension(&path, "flac", Some("audio/mpeg")).unwrap(),
            "mp3"
        );
        assert_eq!(
            downloaded_audio_extension(&path, "mp2", Some("audio/mpeg")).unwrap(),
            "mp2"
        );
    }

    #[test]
    fn rejects_successful_http_responses_that_are_not_audio() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("download.part");
        std::fs::write(&path, br#"{"error":"not logged in"}"#).unwrap();
        let error = downloaded_audio_extension(&path, "flac", Some("application/json"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("不是音频文件"));
    }

    #[test]
    fn formats_multiple_artists_with_comma_space() {
        let song = RemoteSong {
            source: "subsonic".into(),
            id: "1".into(),
            title: "Song".into(),
            artists: vec!["Artist A, Artist B".into(), "Artist C".into()],
            album: String::new(),
            suffix: "mp3".into(),
            bit_rate: None,
            qualities: vec![],
        };
        assert_eq!(
            import_filename(&song, "original", "artist-title").unwrap(),
            "Artist A, Artist B, Artist C - Song.mp3"
        );
        assert_eq!(
            import_filename(&song, "original", "title-artist").unwrap(),
            "Song - Artist A, Artist B, Artist C.mp3"
        );
    }

    #[test]
    fn omits_the_artist_separator_when_no_artist_is_available() {
        let song = RemoteSong {
            source: "qq".into(),
            id: "1".into(),
            title: "Song".into(),
            artists: vec![],
            album: String::new(),
            suffix: "mp3".into(),
            bit_rate: None,
            qualities: vec![],
        };
        assert_eq!(
            import_filename(&song, "original", "artist-title").unwrap(),
            "Song.mp3"
        );
    }

    #[test]
    fn builds_reference_gateway_urls() {
        let connection = RemoteConnection {
            source: "qq".into(),
            gateway_url: "https://music.example/api/".into(),
            cookie: String::new(),
            subsonic_url: String::new(),
            username: String::new(),
            password: String::new(),
        };
        assert_eq!(
            gateway_url(&connection, "/search").unwrap().as_str(),
            "https://music.example/api/search"
        );
    }

    #[test]
    fn merges_server_side_cookies() {
        assert_eq!(
            merge_cookie("MUSIC_U=old; os=pc", "MUSIC_U=new; csrf=1"),
            "MUSIC_U=new; csrf=1; os=pc"
        );
    }

    #[test]
    fn maps_netease_max_quality_to_999k() {
        assert_eq!(netease_bitrate("max"), "999000");
        assert_eq!(netease_bitrate("320"), "320000");
        assert_eq!(netease_bitrate("128"), "128000");
    }

    #[test]
    fn normalizes_netease_audio_match_results() {
        let value = serde_json::json!({
            "code": 200,
            "data": {
                "result": [{
                    "startTime": 1250,
                    "song": {
                        "id": 42,
                        "name": "夜航",
                        "artists": [{"name": "甲"}, {"name": "乙"}],
                        "album": {"name": "远方"}
                    }
                }]
            }
        });

        assert_eq!(
            normalize_netease_audio_matches(&value),
            vec![NeteaseAudioMatchResult {
                id: "42".into(),
                title: "夜航".into(),
                artists: vec!["甲".into(), "乙".into()],
                album: "远方".into(),
                start_time_ms: 1250,
            }]
        );
    }

    #[test]
    fn generates_fingerprint_with_wasm_without_a_javascript_runtime() {
        let wasm = wat::parse_str(
            r#"
(module
  (memory (export "B") 2)
  (table (export "D") 16 funcref)
  (func (export "C"))
  (func (export "E") (param i32) (result i32) i32.const 4096)
  (func (export "F") (param i32))
  (func $destroy (param i32))
  (func $extract (param i32 i32) (result i32)
    i32.const 1024 i32.const 1040 i32.store
    i32.const 1028 i32.const 1043 i32.store
    i32.const 1032 i32.const 1043 i32.store
    i32.const 1024)
  (elem (i32.const 2) $destroy)
  (elem (i32.const 15) $extract)
  (data (i32.const 1040) "abc"))
"#,
        )
        .unwrap();
        let pcm = vec![0_u8; NETEASE_AUDIO_MATCH_SAMPLE_BYTES];

        let fingerprint = run_netease_audio_fingerprint(&wasm, &pcm).unwrap();

        assert_eq!(fingerprint, "YWJj");
    }

    #[test]
    fn extracts_wasm_from_the_gateway_javascript_wrapper() {
        let wasm = b"\0asm\x01\0\0\0";
        let expected_sha256 = hex::encode(digest(&SHA256, wasm));
        let wrapper = format!(
            "'use strict'\nconst WASM_BINARY = \"{}\";\n",
            STANDARD.encode(wasm)
        );
        let extracted =
            extract_netease_audio_match_wasm_with_hash(wrapper.as_bytes(), &expected_sha256)
                .unwrap();

        assert_eq!(extracted, wasm);
    }

    #[test]
    fn media_decode_disables_network_protocols() {
        let arguments = netease_audio_decode_arguments(Path::new("radio.media"));
        assert!(
            arguments
                .windows(2)
                .any(|values| values == ["-protocol_whitelist", "file,pipe"])
        );
        assert!(!arguments.iter().any(|value| value.contains("http")));
        assert!(arguments.windows(2).any(|values| values == ["-t", "3"]));
        assert!(arguments.windows(2).any(|values| values == ["-ar", "8000"]));
    }

    #[tokio::test]
    async fn decodes_a_float_wave_media_sample_when_ffmpeg_is_available() {
        if Command::new("ffmpeg")
            .arg("-version")
            .output()
            .await
            .is_err()
        {
            return;
        }
        let sample_rate = 8_000_u32;
        let sample_count = sample_rate as usize * 4;
        let data_bytes = sample_count * size_of::<f32>();
        let mut wave = Vec::with_capacity(44 + data_bytes);
        wave.extend_from_slice(b"RIFF");
        wave.extend_from_slice(&(36_u32 + data_bytes as u32).to_le_bytes());
        wave.extend_from_slice(b"WAVEfmt ");
        wave.extend_from_slice(&16_u32.to_le_bytes());
        wave.extend_from_slice(&3_u16.to_le_bytes());
        wave.extend_from_slice(&1_u16.to_le_bytes());
        wave.extend_from_slice(&sample_rate.to_le_bytes());
        wave.extend_from_slice(&(sample_rate * 4).to_le_bytes());
        wave.extend_from_slice(&4_u16.to_le_bytes());
        wave.extend_from_slice(&32_u16.to_le_bytes());
        wave.extend_from_slice(b"data");
        wave.extend_from_slice(&(data_bytes as u32).to_le_bytes());
        wave.resize(44 + data_bytes, 0);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sample.wav");
        tokio::fs::write(&path, wave).await.unwrap();

        let decoded = decode_netease_audio_sample(Path::new("ffmpeg"), &path)
            .await
            .unwrap();

        assert_eq!(decoded.len(), NETEASE_AUDIO_MATCH_SAMPLE_BYTES);
    }

    #[test]
    fn normalizes_the_audio_match_runtime_for_browsers() {
        let source = b"'use strict'\r\nconst logger = require('../../util/logger.js')\r\nlogger.info('start')\r\n";

        assert_eq!(
            normalize_netease_audio_match_runtime_file("afp.js", source.to_vec()).unwrap(),
            b"'use strict'\nconsole.info('start')\n"
        );
        assert_eq!(
            normalize_netease_audio_match_runtime_file("afp.wasm.js", source.to_vec()).unwrap(),
            source
        );
    }

    #[tokio::test]
    async fn sends_netease_audio_match_to_the_configured_gateway() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let size = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.starts_with("POST /audio/match?"));
            assert!(request.contains("duration=3"));
            assert!(request.contains("audioFP=QUJDRA%3D%3D"));
            let body = r#"{"code":200,"data":{"result":[{"startTime":0,"song":{"id":7,"name":"Matched","artists":[],"album":{"name":"Album"}}}]}}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let connection = RemoteConnection {
            source: "netease".into(),
            gateway_url: format!("http://{address}"),
            cookie: String::new(),
            subsonic_url: String::new(),
            username: String::new(),
            password: String::new(),
        };

        let matches = netease_audio_match(
            &connection,
            &NeteaseAudioMatchRequest {
                duration: 3,
                audio_fp: "QUJDRA==".into(),
            },
        )
        .await
        .unwrap();

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "7");
        server.await.unwrap();
    }
}

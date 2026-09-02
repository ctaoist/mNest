//! Audio decoding and transcoding backed directly by FFmpeg's libav libraries.

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::PathBuf,
    pin::Pin,
    ptr,
    sync::Once,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use axum::body::Bytes;
use ffmpeg_sys_next as ffi;
use futures::Stream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const LIBAV_VERSION: &str = "5.1.7";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioFormat {
    Mp3,
    Opus,
    Aac,
    Flac,
    OggVorbis,
    PcmF32Le,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RadioInput {
    Hls,
    Rtsp,
    Mmsh,
    Mmst,
}

#[derive(Clone, Debug)]
pub enum MediaInput {
    File(PathBuf),
    Radio { url: String, protocol: RadioInput },
}

#[derive(Clone, Debug)]
pub struct TranscodeRequest {
    pub input: MediaInput,
    pub format: AudioFormat,
    pub bitrate_kbps: Option<u32>,
    pub offset: Option<Duration>,
    pub sample_rate: Option<i32>,
    pub channels: Option<i32>,
    pub max_samples: Option<i64>,
    pub realtime: bool,
}

impl TranscodeRequest {
    pub fn file(path: PathBuf, format: AudioFormat) -> Self {
        Self {
            input: MediaInput::File(path),
            format,
            bitrate_kbps: None,
            offset: None,
            sample_rate: None,
            channels: None,
            max_samples: None,
            realtime: false,
        }
    }

    pub fn radio(url: String, protocol: RadioInput) -> Self {
        Self {
            input: MediaInput::Radio { url, protocol },
            format: AudioFormat::Mp3,
            bitrate_kbps: Some(128),
            offset: None,
            sample_rate: None,
            channels: None,
            max_samples: None,
            realtime: protocol == RadioInput::Hls,
        }
    }
}

pub trait MediaEngine: Send + Sync {
    fn transcode(&self, request: TranscodeRequest) -> io::Result<MediaStream>;

    fn version(&self) -> String;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LibavMediaEngine;

impl MediaEngine for LibavMediaEngine {
    fn transcode(&self, request: TranscodeRequest) -> io::Result<MediaStream> {
        initialize_libav();
        validate_request(&request)?;
        let (sender, receiver) = mpsc::channel(8);
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        std::thread::Builder::new()
            .name("mnest-libav".to_owned())
            .spawn(move || {
                if let Err(error) = run_transcode(request, sender.clone(), worker_cancellation) {
                    let _ = sender.blocking_send(Err(io::Error::other(error.to_string())));
                }
            })?;
        Ok(MediaStream {
            receiver,
            cancellation,
        })
    }

    fn version(&self) -> String {
        initialize_libav();
        unsafe {
            let version = ffi::av_version_info();
            if version.is_null() {
                return LIBAV_VERSION.to_owned();
            }
            CStr::from_ptr(version).to_string_lossy().into_owned()
        }
    }
}

fn initialize_libav() {
    static INITIALIZE: Once = Once::new();
    INITIALIZE.call_once(|| unsafe {
        ffi::av_log_set_level(ffi::AV_LOG_QUIET);
        ffi::avformat_network_init();
    });
}

pub struct MediaStream {
    receiver: mpsc::Receiver<io::Result<Bytes>>,
    cancellation: CancellationToken,
}

impl MediaStream {
    #[cfg(test)]
    pub fn channel(capacity: usize) -> (mpsc::Sender<io::Result<Bytes>>, Self) {
        let (sender, receiver) = mpsc::channel(capacity);
        (
            sender,
            Self {
                receiver,
                cancellation: CancellationToken::new(),
            },
        )
    }
}

impl Stream for MediaStream {
    type Item = io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx)
    }
}

impl Drop for MediaStream {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

fn validate_request(request: &TranscodeRequest) -> io::Result<()> {
    if request
        .bitrate_kbps
        .is_some_and(|rate| !(16..=320).contains(&rate))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "audio bitrate must be between 16 and 320 kbps",
        ));
    }
    if request
        .sample_rate
        .is_some_and(|rate| !(1..=MAX_SAMPLE_RATE).contains(&rate))
        || request
            .channels
            .is_some_and(|channels| !(1..=MAX_CHANNELS).contains(&channels))
        || request.max_samples.is_some_and(|samples| samples <= 0)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid audio output constraints",
        ));
    }
    Ok(())
}

const MAX_SAMPLE_RATE: c_int = 768_000;
const MAX_CHANNELS: c_int = 64;
const AV_NOPTS_VALUE: i64 = i64::MIN;
const REALTIME_INITIAL_BURST: Duration = Duration::from_millis(500);

struct OutputSink {
    sender: mpsc::Sender<io::Result<Bytes>>,
    cancellation: CancellationToken,
}

unsafe fn write_output_inner(opaque: *mut c_void, buffer: *const u8, size: c_int) -> c_int {
    if opaque.is_null() || buffer.is_null() || size <= 0 {
        return -libc::EINVAL;
    }
    let sink = unsafe { &*(opaque.cast::<OutputSink>()) };
    if sink.cancellation.is_cancelled() {
        return -libc::ECANCELED;
    }
    let bytes =
        Bytes::copy_from_slice(unsafe { std::slice::from_raw_parts(buffer, size as usize) });
    match sink.sender.blocking_send(Ok(bytes)) {
        Ok(()) => size,
        Err(_) => -libc::EPIPE,
    }
}

#[cfg(mnest_legacy_avio_write)]
unsafe extern "C" fn write_output(opaque: *mut c_void, buffer: *mut u8, size: c_int) -> c_int {
    unsafe { write_output_inner(opaque, buffer, size) }
}

#[cfg(not(mnest_legacy_avio_write))]
unsafe extern "C" fn write_output(opaque: *mut c_void, buffer: *const u8, size: c_int) -> c_int {
    unsafe { write_output_inner(opaque, buffer, size) }
}

struct InterruptState {
    cancellation: CancellationToken,
}

struct InputSource {
    file: File,
}

unsafe extern "C" fn read_input(opaque: *mut c_void, buffer: *mut u8, size: c_int) -> c_int {
    if opaque.is_null() || buffer.is_null() || size <= 0 {
        return -libc::EINVAL;
    }
    let source = unsafe { &mut *opaque.cast::<InputSource>() };
    let buffer = unsafe { std::slice::from_raw_parts_mut(buffer, size as usize) };
    match source.file.read(buffer) {
        Ok(0) => AVERROR_EOF_CODE,
        Ok(read) => c_int::try_from(read).unwrap_or(c_int::MAX),
        Err(error) => negative_io_error(&error),
    }
}

unsafe extern "C" fn seek_input(opaque: *mut c_void, offset: i64, whence: c_int) -> i64 {
    if opaque.is_null() {
        return -i64::from(libc::EINVAL);
    }
    let source = unsafe { &mut *opaque.cast::<InputSource>() };
    if whence & ffi::AVSEEK_SIZE != 0 {
        return source
            .file
            .metadata()
            .ok()
            .and_then(|metadata| i64::try_from(metadata.len()).ok())
            .unwrap_or_else(|| -i64::from(libc::EIO));
    }
    let seek_from = match whence & !ffi::AVSEEK_FORCE {
        libc::SEEK_SET if offset >= 0 => SeekFrom::Start(offset as u64),
        libc::SEEK_CUR => SeekFrom::Current(offset),
        libc::SEEK_END => SeekFrom::End(offset),
        _ => return -i64::from(libc::EINVAL),
    };
    source
        .file
        .seek(seek_from)
        .ok()
        .and_then(|position| i64::try_from(position).ok())
        .unwrap_or_else(|| -i64::from(libc::EIO))
}

fn negative_io_error(error: &io::Error) -> c_int {
    -error.raw_os_error().unwrap_or(libc::EIO).saturating_abs()
}

unsafe extern "C" fn interrupt_input(opaque: *mut c_void) -> c_int {
    if opaque.is_null() {
        return 0;
    }
    i32::from(
        unsafe { &*(opaque.cast::<InterruptState>()) }
            .cancellation
            .is_cancelled(),
    )
}

fn run_transcode(
    request: TranscodeRequest,
    sender: mpsc::Sender<io::Result<Bytes>>,
    cancellation: CancellationToken,
) -> anyhow::Result<()> {
    unsafe { transcode_inner(request, sender, cancellation) }
}

unsafe fn transcode_inner(
    request: TranscodeRequest,
    sender: mpsc::Sender<io::Result<Bytes>>,
    cancellation: CancellationToken,
) -> anyhow::Result<()> {
    let mut input = unsafe { InputContext::open(&request.input, cancellation.clone())? };
    if let Some(offset) = request.offset.filter(|offset| !offset.is_zero()) {
        unsafe { input.seek(offset)? };
    }

    let mut output =
        unsafe { OutputContext::create(&request, input.decoder, sender, cancellation.clone())? };
    let mut pipeline = unsafe { AudioPipeline::new(&mut input, &mut output, &request)? };
    unsafe { pipeline.run(&cancellation)? };
    Ok(())
}

const AVERROR_EOF_CODE: c_int = -541_478_725;

struct InputContext {
    format: *mut ffi::AVFormatContext,
    decoder: *mut ffi::AVCodecContext,
    io: *mut ffi::AVIOContext,
    source: *mut InputSource,
    stream_index: c_int,
    stream_time_base: ffi::AVRational,
    trim_start_timestamp: Option<i64>,
    _interrupt: Box<InterruptState>,
}

impl InputContext {
    unsafe fn open(input: &MediaInput, cancellation: CancellationToken) -> anyhow::Result<Self> {
        let (location, forced_format, options) = match input {
            MediaInput::File(path) => (
                path_to_c_string(path)?,
                None,
                vec![("protocol_whitelist", String::new())],
            ),
            MediaInput::Radio { url, protocol } => {
                let mut options = vec![("rw_timeout", "30000000".to_owned())];
                let forced_format = match protocol {
                    RadioInput::Hls => {
                        options.extend([
                            ("reconnect", "1".to_owned()),
                            ("reconnect_streamed", "1".to_owned()),
                            ("reconnect_delay_max", "5".to_owned()),
                            ("user_agent", "mNest/internet-radio".to_owned()),
                            ("live_start_index", "-1".to_owned()),
                        ]);
                        Some("hls")
                    }
                    RadioInput::Rtsp => {
                        options.push(("rtsp_transport", "tcp".to_owned()));
                        None
                    }
                    RadioInput::Mmsh => {
                        options.push(("user_agent", "mNest/internet-radio".to_owned()));
                        None
                    }
                    RadioInput::Mmst => None,
                };
                (c_string(url)?, forced_format, options)
            }
        };
        let mut interrupt = Box::new(InterruptState { cancellation });
        let format = unsafe { ffi::avformat_alloc_context() };
        anyhow::ensure!(!format.is_null(), "allocate input format context");
        unsafe {
            (*format).interrupt_callback = ffi::AVIOInterruptCB {
                callback: Some(interrupt_input),
                opaque: ptr::from_mut(interrupt.as_mut()).cast(),
            };
        }
        let mut context = Self {
            format,
            decoder: ptr::null_mut(),
            io: ptr::null_mut(),
            source: ptr::null_mut(),
            stream_index: -1,
            stream_time_base: ffi::AVRational { num: 0, den: 1 },
            trim_start_timestamp: None,
            _interrupt: interrupt,
        };
        if let MediaInput::File(path) = input {
            unsafe { context.attach_file(path)? };
        }
        let input_format = forced_format
            .map(c_string)
            .transpose()?
            .map(|name| unsafe { ffi::av_find_input_format(name.as_ptr()) })
            .unwrap_or(ptr::null());
        if forced_format.is_some() {
            anyhow::ensure!(
                !input_format.is_null(),
                "requested input format is unavailable"
            );
        }
        let mut dictionary = ptr::null_mut();
        for (key, value) in options {
            unsafe { dictionary_set(&mut dictionary, key, &value)? };
        }
        let open_result = unsafe {
            ffi::avformat_open_input(
                &mut context.format,
                location.as_ptr(),
                input_format,
                &mut dictionary,
            )
        };
        unsafe { ffi::av_dict_free(&mut dictionary) };
        if open_result < 0 {
            anyhow::bail!("open input: {}", ffmpeg_error(open_result));
        }
        let format = context.format;
        if matches!(input, MediaInput::File(_)) {
            unsafe { reject_local_reference_format(format)? };
        }
        unsafe {
            check(
                ffi::avformat_find_stream_info(format, ptr::null_mut()),
                "probe input",
            )?
        };
        let mut decoder = ptr::null();
        let stream_index = unsafe {
            ffi::av_find_best_stream(
                format,
                ffi::AVMediaType::AVMEDIA_TYPE_AUDIO,
                -1,
                -1,
                &mut decoder,
                0,
            )
        };
        check(stream_index, "find audio stream")?;
        anyhow::ensure!(!decoder.is_null(), "audio decoder is unavailable");
        let decoder_context = unsafe { ffi::avcodec_alloc_context3(decoder) };
        anyhow::ensure!(!decoder_context.is_null(), "allocate audio decoder");
        context.decoder = decoder_context;
        let stream = unsafe { *(*format).streams.add(stream_index as usize) };
        unsafe {
            check(
                ffi::avcodec_parameters_to_context(decoder_context, (*stream).codecpar),
                "copy decoder parameters",
            )?;
            (*decoder_context).pkt_timebase = (*stream).time_base;
            check(
                ffi::avcodec_open2(decoder_context, decoder, ptr::null_mut()),
                "open audio decoder",
            )?;
        }
        validate_audio_properties(unsafe { (*decoder_context).sample_rate }, unsafe {
            (*decoder_context).ch_layout.nb_channels
        })?;
        context.stream_index = stream_index;
        context.stream_time_base = unsafe { (*stream).time_base };
        Ok(context)
    }

    unsafe fn attach_file(&mut self, path: &std::path::Path) -> anyhow::Result<()> {
        let file = File::open(path)?;
        anyhow::ensure!(
            file.metadata()?.is_file(),
            "media input is not a regular file"
        );
        let buffer_size = 32 * 1024;
        let buffer = unsafe { ffi::av_malloc(buffer_size) }.cast::<u8>();
        anyhow::ensure!(!buffer.is_null(), "allocate input buffer");
        let source = Box::into_raw(Box::new(InputSource { file }));
        let io = unsafe {
            ffi::avio_alloc_context(
                buffer,
                buffer_size as c_int,
                0,
                source.cast(),
                Some(read_input),
                None,
                Some(seek_input),
            )
        };
        if io.is_null() {
            unsafe {
                ffi::av_free(buffer.cast());
                drop(Box::from_raw(source));
            }
            anyhow::bail!("allocate input IO context");
        }
        self.io = io;
        self.source = source;
        unsafe {
            (*self.format).pb = io;
            (*self.format).flags |= ffi::AVFMT_FLAG_CUSTOM_IO;
        }
        Ok(())
    }

    unsafe fn seek(&mut self, offset: Duration) -> anyhow::Result<()> {
        let offset_micros = i64::try_from(offset.as_micros()).unwrap_or(i64::MAX);
        let stream = unsafe { *(*self.format).streams.add(self.stream_index as usize) };
        let stream_start = unsafe {
            if (*stream).start_time != AV_NOPTS_VALUE {
                (*stream).start_time
            } else if (*self.format).start_time != AV_NOPTS_VALUE {
                ffi::av_rescale_q(
                    (*self.format).start_time,
                    ffi::AVRational {
                        num: 1,
                        den: ffi::AV_TIME_BASE,
                    },
                    self.stream_time_base,
                )
            } else {
                0
            }
        };
        let offset_timestamp = unsafe {
            ffi::av_rescale_q(
                offset_micros,
                ffi::AVRational {
                    num: 1,
                    den: ffi::AV_TIME_BASE,
                },
                self.stream_time_base,
            )
        };
        let target = stream_start.saturating_add(offset_timestamp);
        unsafe {
            check(
                ffi::avformat_seek_file(
                    self.format,
                    self.stream_index,
                    i64::MIN,
                    target,
                    target,
                    ffi::AVSEEK_FLAG_BACKWARD,
                ),
                "seek input",
            )?;
            ffi::avcodec_flush_buffers(self.decoder);
        }
        self.trim_start_timestamp = Some(target);
        Ok(())
    }
}

unsafe fn reject_local_reference_format(format: *const ffi::AVFormatContext) -> anyhow::Result<()> {
    let input_format = unsafe { (*format).iformat };
    anyhow::ensure!(!input_format.is_null(), "input format is unavailable");
    let name = unsafe { (*input_format).name };
    anyhow::ensure!(!name.is_null(), "input format name is unavailable");
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    let references_external_resources = name.split(',').any(|name| {
        matches!(
            name,
            "concat" | "concatf" | "dash" | "hls" | "smoothstreaming"
        )
    });
    anyhow::ensure!(
        !references_external_resources,
        "local reference playlists are not supported"
    );
    Ok(())
}

impl Drop for InputContext {
    fn drop(&mut self) {
        unsafe {
            if !self.decoder.is_null() {
                ffi::avcodec_free_context(&mut self.decoder);
            }
            if !self.format.is_null() {
                ffi::avformat_close_input(&mut self.format);
            }
            if !self.io.is_null() {
                ffi::avio_context_free(&mut self.io);
            }
            if !self.source.is_null() {
                drop(Box::from_raw(self.source));
                self.source = ptr::null_mut();
            }
        }
    }
}

struct OutputContext {
    format: *mut ffi::AVFormatContext,
    encoder: *mut ffi::AVCodecContext,
    stream: *mut ffi::AVStream,
    io: *mut ffi::AVIOContext,
    sink: *mut OutputSink,
    header_written: bool,
}

impl OutputContext {
    unsafe fn create(
        request: &TranscodeRequest,
        decoder: *mut ffi::AVCodecContext,
        sender: mpsc::Sender<io::Result<Bytes>>,
        cancellation: CancellationToken,
    ) -> anyhow::Result<Self> {
        let (muxer, encoder_name) = output_names(request.format);
        let muxer = c_string(muxer)?;
        let encoder_name = c_string(encoder_name)?;
        let mut format = ptr::null_mut();
        unsafe {
            check(
                ffi::avformat_alloc_output_context2(
                    &mut format,
                    ptr::null(),
                    muxer.as_ptr(),
                    ptr::null(),
                ),
                "allocate output format",
            )?;
        }
        anyhow::ensure!(!format.is_null(), "allocate output format context");
        let mut output = Self {
            format,
            encoder: ptr::null_mut(),
            stream: ptr::null_mut(),
            io: ptr::null_mut(),
            sink: ptr::null_mut(),
            header_written: false,
        };

        let encoder = unsafe { ffi::avcodec_find_encoder_by_name(encoder_name.as_ptr()) };
        anyhow::ensure!(
            !encoder.is_null(),
            "audio encoder {} is unavailable",
            encoder_name.to_string_lossy()
        );
        let encoder_context = unsafe { ffi::avcodec_alloc_context3(encoder) };
        anyhow::ensure!(!encoder_context.is_null(), "allocate audio encoder");
        output.encoder = encoder_context;

        let preferred_rate = request
            .sample_rate
            .unwrap_or(unsafe { (*decoder).sample_rate });
        let sample_rate = unsafe { supported_sample_rate(encoder, preferred_rate.max(1)) };
        let preferred_channels = request
            .channels
            .unwrap_or(unsafe { (*decoder).ch_layout.nb_channels })
            .max(1);
        unsafe {
            (*encoder_context).sample_rate = sample_rate;
            (*encoder_context).sample_fmt = supported_sample_format(encoder);
            (*encoder_context).time_base = ffi::AVRational {
                num: 1,
                den: sample_rate,
            };
            (*encoder_context).bit_rate = request
                .bitrate_kbps
                .map(|rate| i64::from(rate) * 1000)
                .unwrap_or(0);
            select_channel_layout(
                encoder,
                preferred_channels,
                &mut (*encoder_context).ch_layout,
            )?;
            if (*(*format).oformat).flags & ffi::AVFMT_GLOBALHEADER != 0 {
                (*encoder_context).flags |= ffi::AV_CODEC_FLAG_GLOBAL_HEADER as c_int;
            }
            check(
                ffi::avcodec_open2(encoder_context, encoder, ptr::null_mut()),
                "open audio encoder",
            )?;
        }

        let stream = unsafe { ffi::avformat_new_stream(format, ptr::null()) };
        anyhow::ensure!(!stream.is_null(), "allocate output audio stream");
        output.stream = stream;
        unsafe {
            (*stream).time_base = (*encoder_context).time_base;
            check(
                ffi::avcodec_parameters_from_context((*stream).codecpar, encoder_context),
                "copy encoder parameters",
            )?;
        }

        let avio_buffer_size = 32 * 1024;
        let avio_buffer = unsafe { ffi::av_malloc(avio_buffer_size) }.cast::<u8>();
        anyhow::ensure!(!avio_buffer.is_null(), "allocate output buffer");
        let sink = Box::into_raw(Box::new(OutputSink {
            sender,
            cancellation,
        }));
        output.sink = sink;
        let io = unsafe {
            ffi::avio_alloc_context(
                avio_buffer,
                avio_buffer_size as c_int,
                1,
                sink.cast(),
                None,
                Some(write_output),
                None,
            )
        };
        if io.is_null() {
            unsafe { ffi::av_free(avio_buffer.cast()) };
            anyhow::bail!("allocate output IO context");
        }
        output.io = io;
        unsafe {
            (*io).seekable = 0;
            (*format).pb = io;
            (*format).flags |= ffi::AVFMT_FLAG_CUSTOM_IO;
        }
        let mut options = ptr::null_mut();
        if matches!(request.input, MediaInput::Radio { .. }) && request.format == AudioFormat::Mp3 {
            unsafe {
                dictionary_set(&mut options, "id3v2_version", "0")?;
                dictionary_set(&mut options, "write_xing", "0")?;
            }
        }
        let header_result = unsafe { ffi::avformat_write_header(format, &mut options) };
        unsafe { ffi::av_dict_free(&mut options) };
        check(header_result, "write output header")?;
        output.header_written = true;
        Ok(output)
    }
}

impl Drop for OutputContext {
    fn drop(&mut self) {
        unsafe {
            if !self.format.is_null() {
                (*self.format).pb = ptr::null_mut();
                ffi::avformat_free_context(self.format);
                self.format = ptr::null_mut();
            }
            if !self.encoder.is_null() {
                ffi::avcodec_free_context(&mut self.encoder);
            }
            if !self.io.is_null() {
                ffi::avio_context_free(&mut self.io);
            }
            if !self.sink.is_null() {
                drop(Box::from_raw(self.sink));
                self.sink = ptr::null_mut();
            }
        }
    }
}

struct AudioPipeline<'a> {
    input: &'a mut InputContext,
    output: &'a mut OutputContext,
    swr: *mut ffi::SwrContext,
    fifo: *mut ffi::AVAudioFifo,
    input_packet: *mut ffi::AVPacket,
    output_packet: *mut ffi::AVPacket,
    decoded_frame: *mut ffi::AVFrame,
    converted_frame: *mut ffi::AVFrame,
    encoder_frame: *mut ffi::AVFrame,
    input_sample_rate: c_int,
    input_sample_format: ffi::AVSampleFormat,
    input_channels: c_int,
    max_samples: Option<i64>,
    accepted_samples: i64,
    encoded_samples: i64,
    realtime_origin: Option<Instant>,
    cancellation: CancellationToken,
}

impl<'a> AudioPipeline<'a> {
    unsafe fn new(
        input: &'a mut InputContext,
        output: &'a mut OutputContext,
        request: &TranscodeRequest,
    ) -> anyhow::Result<Self> {
        let cancellation = input._interrupt.cancellation.clone();
        let input_sample_rate = unsafe { (*input.decoder).sample_rate };
        let input_sample_format = unsafe { (*input.decoder).sample_fmt };
        let input_channels = unsafe { (*input.decoder).ch_layout.nb_channels };
        let mut pipeline = Self {
            input,
            output,
            swr: ptr::null_mut(),
            fifo: ptr::null_mut(),
            input_packet: ptr::null_mut(),
            output_packet: ptr::null_mut(),
            decoded_frame: ptr::null_mut(),
            converted_frame: ptr::null_mut(),
            encoder_frame: ptr::null_mut(),
            input_sample_rate,
            input_sample_format,
            input_channels,
            max_samples: request.max_samples,
            accepted_samples: 0,
            encoded_samples: 0,
            realtime_origin: request.realtime.then(Instant::now),
            cancellation,
        };
        unsafe {
            check(
                ffi::swr_alloc_set_opts2(
                    &mut pipeline.swr,
                    ptr::from_ref(&(*pipeline.output.encoder).ch_layout).cast_mut(),
                    (*pipeline.output.encoder).sample_fmt,
                    (*pipeline.output.encoder).sample_rate,
                    ptr::from_ref(&(*pipeline.input.decoder).ch_layout).cast_mut(),
                    (*pipeline.input.decoder).sample_fmt,
                    (*pipeline.input.decoder).sample_rate,
                    0,
                    ptr::null_mut(),
                ),
                "configure audio resampler",
            )?;
            check(ffi::swr_init(pipeline.swr), "initialize audio resampler")?;
        }
        pipeline.fifo = unsafe {
            ffi::av_audio_fifo_alloc(
                (*pipeline.output.encoder).sample_fmt,
                (*pipeline.output.encoder).ch_layout.nb_channels,
                output_frame_size(pipeline.output.encoder),
            )
        };
        anyhow::ensure!(!pipeline.fifo.is_null(), "allocate audio FIFO");
        pipeline.input_packet = unsafe { ffi::av_packet_alloc() };
        pipeline.output_packet = unsafe { ffi::av_packet_alloc() };
        pipeline.decoded_frame = unsafe { ffi::av_frame_alloc() };
        pipeline.converted_frame = unsafe { ffi::av_frame_alloc() };
        pipeline.encoder_frame = unsafe { ffi::av_frame_alloc() };
        if pipeline.input_packet.is_null()
            || pipeline.output_packet.is_null()
            || pipeline.decoded_frame.is_null()
            || pipeline.converted_frame.is_null()
            || pipeline.encoder_frame.is_null()
        {
            anyhow::bail!("allocate audio pipeline buffers");
        }
        Ok(pipeline)
    }

    unsafe fn run(&mut self, cancellation: &CancellationToken) -> anyhow::Result<()> {
        let mut reached_limit = false;
        while !reached_limit && !cancellation.is_cancelled() {
            let result = unsafe { ffi::av_read_frame(self.input.format, self.input_packet) };
            if result == AVERROR_EOF_CODE {
                break;
            }
            check(result, "read input packet")?;
            if unsafe { (*self.input_packet).stream_index } == self.input.stream_index {
                unsafe {
                    check(
                        ffi::avcodec_send_packet(self.input.decoder, self.input_packet),
                        "send packet to decoder",
                    )?;
                    reached_limit = self.drain_decoder()?;
                }
            }
            unsafe { ffi::av_packet_unref(self.input_packet) };
        }
        unsafe { ffi::av_packet_unref(self.input_packet) };
        if cancellation.is_cancelled() {
            anyhow::bail!("media operation cancelled");
        }
        if !reached_limit {
            unsafe {
                check(
                    ffi::avcodec_send_packet(self.input.decoder, ptr::null()),
                    "flush decoder",
                )?;
                self.drain_decoder()?;
                self.flush_resampler()?;
            }
        }
        unsafe {
            self.encode_fifo(true)?;
            check(
                ffi::avcodec_send_frame(self.output.encoder, ptr::null()),
                "flush encoder",
            )?;
            self.drain_encoder()?;
            check(
                ffi::av_write_trailer(self.output.format),
                "write output trailer",
            )?;
            ffi::avio_flush(self.output.io);
            check((*self.output.io).error, "flush output")?;
        }
        self.output.header_written = false;
        Ok(())
    }

    unsafe fn drain_decoder(&mut self) -> anyhow::Result<bool> {
        loop {
            let result =
                unsafe { ffi::avcodec_receive_frame(self.input.decoder, self.decoded_frame) };
            if result == -libc::EAGAIN || result == AVERROR_EOF_CODE {
                return Ok(false);
            }
            check(result, "receive decoded audio")?;
            let reached_limit = unsafe { self.convert_decoded_frame()? };
            unsafe { ffi::av_frame_unref(self.decoded_frame) };
            if reached_limit {
                return Ok(true);
            }
        }
    }

    unsafe fn convert_decoded_frame(&mut self) -> anyhow::Result<bool> {
        let input_samples = unsafe { (*self.decoded_frame).nb_samples };
        anyhow::ensure!(
            input_samples >= 0,
            "decoder returned a negative sample count"
        );
        let frame_rate = unsafe { (*self.decoded_frame).sample_rate };
        let frame_channels = unsafe { (*self.decoded_frame).ch_layout.nb_channels };
        validate_audio_properties(frame_rate, frame_channels)?;
        anyhow::ensure!(
            frame_rate == self.input_sample_rate
                && frame_channels == self.input_channels
                && unsafe { (*self.decoded_frame).format } == self.input_sample_format as c_int,
            "decoded audio parameters changed during transcoding"
        );
        let input_rate = self.input_sample_rate;
        let skipped = unsafe { self.leading_samples_to_skip(input_samples, input_rate)? };
        let input_samples = input_samples - skipped;
        if input_samples == 0 {
            return Ok(false);
        }
        let output_rate = unsafe { (*self.output.encoder).sample_rate };
        let delayed = unsafe { ffi::swr_get_delay(self.swr, i64::from(input_rate)) };
        let capacity = unsafe {
            ffi::av_rescale_rnd(
                delayed + i64::from(input_samples),
                i64::from(output_rate),
                i64::from(input_rate),
                ffi::AVRounding::AV_ROUND_UP,
            )
        } as c_int;
        unsafe {
            prepare_audio_frame(self.converted_frame, self.output.encoder, capacity.max(1))?;
        }
        let adjusted_input = if skipped == 0 {
            None
        } else {
            Some(unsafe {
                offset_audio_data(
                    self.decoded_frame,
                    self.input_sample_format,
                    self.input_channels,
                    skipped,
                )?
            })
        };
        let input_data = if let Some(adjusted_input) = adjusted_input.as_ref() {
            adjusted_input.as_ptr()
        } else {
            unsafe { (*self.decoded_frame).extended_data.cast::<*const u8>() }
        };
        let converted = unsafe {
            ffi::swr_convert(
                self.swr,
                (*self.converted_frame).extended_data,
                capacity,
                input_data.cast_mut(),
                input_samples,
            )
        };
        check(converted, "resample decoded audio")?;
        unsafe { self.write_converted(converted) }
    }

    unsafe fn leading_samples_to_skip(
        &mut self,
        input_samples: c_int,
        input_rate: c_int,
    ) -> anyhow::Result<c_int> {
        let Some(target) = self.input.trim_start_timestamp else {
            return Ok(0);
        };
        let frame_timestamp = unsafe {
            let best_effort = (*self.decoded_frame).best_effort_timestamp;
            if best_effort != AV_NOPTS_VALUE {
                best_effort
            } else {
                (*self.decoded_frame).pts
            }
        };
        anyhow::ensure!(
            frame_timestamp != AV_NOPTS_VALUE,
            "decoded audio timestamp unavailable after seek"
        );
        if frame_timestamp >= target {
            self.input.trim_start_timestamp = None;
            return Ok(0);
        }
        let skipped = unsafe {
            ffi::av_rescale_q_rnd(
                target - frame_timestamp,
                self.input.stream_time_base,
                ffi::AVRational {
                    num: 1,
                    den: input_rate,
                },
                ffi::AVRounding::AV_ROUND_UP,
            )
        }
        .clamp(0, i64::from(input_samples)) as c_int;
        if skipped < input_samples {
            self.input.trim_start_timestamp = None;
        }
        Ok(skipped)
    }

    unsafe fn flush_resampler(&mut self) -> anyhow::Result<()> {
        loop {
            let capacity = unsafe { ffi::swr_get_out_samples(self.swr, 0) };
            if capacity <= 0 {
                return Ok(());
            }
            unsafe {
                prepare_audio_frame(self.converted_frame, self.output.encoder, capacity)?;
            }
            let converted = unsafe {
                ffi::swr_convert(
                    self.swr,
                    (*self.converted_frame).extended_data,
                    capacity,
                    ptr::null_mut(),
                    0,
                )
            };
            check(converted, "flush audio resampler")?;
            if converted == 0 {
                return Ok(());
            }
            if unsafe { self.write_converted(converted)? } {
                return Ok(());
            }
        }
    }

    unsafe fn write_converted(&mut self, converted: c_int) -> anyhow::Result<bool> {
        let remaining = self
            .max_samples
            .map(|maximum| maximum.saturating_sub(self.accepted_samples))
            .unwrap_or(i64::MAX);
        let accepted = i64::from(converted).min(remaining).max(0) as c_int;
        if accepted > 0 {
            let written = unsafe {
                ffi::av_audio_fifo_write(
                    self.fifo,
                    (*self.converted_frame).extended_data.cast(),
                    accepted,
                )
            };
            check(written, "write resampled audio")?;
            anyhow::ensure!(written == accepted, "short write to audio FIFO");
            self.accepted_samples += i64::from(accepted);
            unsafe { self.encode_fifo(false)? };
        }
        unsafe { ffi::av_frame_unref(self.converted_frame) };
        Ok(self
            .max_samples
            .is_some_and(|maximum| self.accepted_samples >= maximum))
    }

    unsafe fn encode_fifo(&mut self, final_frame: bool) -> anyhow::Result<()> {
        let frame_size = output_frame_size(self.output.encoder);
        loop {
            let available = unsafe { ffi::av_audio_fifo_size(self.fifo) };
            if available < frame_size && !(final_frame && available > 0) {
                return Ok(());
            }
            let samples = available.min(frame_size);
            unsafe { prepare_audio_frame(self.encoder_frame, self.output.encoder, samples)? };
            let read = unsafe {
                ffi::av_audio_fifo_read(
                    self.fifo,
                    (*self.encoder_frame).extended_data.cast(),
                    samples,
                )
            };
            check(read, "read audio FIFO")?;
            anyhow::ensure!(read == samples, "short read from audio FIFO");
            unsafe { (*self.encoder_frame).pts = self.encoded_samples };
            self.pace_realtime(self.encoded_samples, unsafe {
                (*self.output.encoder).sample_rate
            });
            self.encoded_samples += i64::from(samples);
            unsafe {
                check(
                    ffi::avcodec_send_frame(self.output.encoder, self.encoder_frame),
                    "send audio to encoder",
                )?;
                ffi::av_frame_unref(self.encoder_frame);
                self.drain_encoder()?;
            }
        }
    }

    unsafe fn drain_encoder(&mut self) -> anyhow::Result<()> {
        loop {
            let result =
                unsafe { ffi::avcodec_receive_packet(self.output.encoder, self.output_packet) };
            if result == -libc::EAGAIN || result == AVERROR_EOF_CODE {
                return Ok(());
            }
            check(result, "receive encoded audio")?;
            unsafe {
                ffi::av_packet_rescale_ts(
                    self.output_packet,
                    (*self.output.encoder).time_base,
                    (*self.output.stream).time_base,
                );
                (*self.output_packet).stream_index = (*self.output.stream).index;
                check(
                    ffi::av_interleaved_write_frame(self.output.format, self.output_packet),
                    "write encoded audio",
                )?;
                ffi::av_packet_unref(self.output_packet);
            }
        }
    }

    fn pace_realtime(&self, samples: i64, sample_rate: c_int) {
        let Some(origin) = self.realtime_origin else {
            return;
        };
        let target = origin + realtime_presentation_delay(samples, sample_rate);
        while let Some(remaining) = target.checked_duration_since(Instant::now()) {
            if remaining.is_zero() || self.cancellation.is_cancelled() {
                break;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(25)));
        }
    }
}

fn realtime_presentation_delay(samples: i64, sample_rate: c_int) -> Duration {
    Duration::from_secs_f64(samples.max(0) as f64 / f64::from(sample_rate.max(1)))
        .saturating_sub(REALTIME_INITIAL_BURST)
}

fn validate_audio_properties(sample_rate: c_int, channels: c_int) -> anyhow::Result<()> {
    anyhow::ensure!(
        (1..=MAX_SAMPLE_RATE).contains(&sample_rate),
        "unsupported decoded audio sample rate: {sample_rate}"
    );
    anyhow::ensure!(
        (1..=MAX_CHANNELS).contains(&channels),
        "unsupported decoded audio channel count: {channels}"
    );
    Ok(())
}

unsafe fn offset_audio_data(
    frame: *const ffi::AVFrame,
    format: ffi::AVSampleFormat,
    channels: c_int,
    skipped_samples: c_int,
) -> anyhow::Result<Vec<*const u8>> {
    let bytes_per_sample = unsafe { ffi::av_get_bytes_per_sample(format) };
    anyhow::ensure!(
        bytes_per_sample > 0,
        "unsupported decoded audio sample format"
    );
    let planar = unsafe { ffi::av_sample_fmt_is_planar(format) } != 0;
    let plane_count = if planar { channels } else { 1 };
    let sample_stride = if planar {
        bytes_per_sample
    } else {
        bytes_per_sample
            .checked_mul(channels)
            .ok_or_else(|| anyhow::anyhow!("decoded audio stride overflow"))?
    };
    let byte_offset = usize::try_from(
        skipped_samples
            .checked_mul(sample_stride)
            .ok_or_else(|| anyhow::anyhow!("decoded audio offset overflow"))?,
    )?;
    let extended_data = unsafe { (*frame).extended_data };
    anyhow::ensure!(
        !extended_data.is_null(),
        "decoded audio data is unavailable"
    );
    let mut data = Vec::with_capacity(plane_count as usize);
    for plane in 0..plane_count as usize {
        let pointer = unsafe { *extended_data.add(plane) };
        anyhow::ensure!(!pointer.is_null(), "decoded audio plane is unavailable");
        data.push(unsafe { pointer.add(byte_offset) }.cast_const());
    }
    Ok(data)
}

impl Drop for AudioPipeline<'_> {
    fn drop(&mut self) {
        unsafe {
            if !self.input_packet.is_null() {
                ffi::av_packet_free(&mut self.input_packet);
            }
            if !self.output_packet.is_null() {
                ffi::av_packet_free(&mut self.output_packet);
            }
            if !self.decoded_frame.is_null() {
                ffi::av_frame_free(&mut self.decoded_frame);
            }
            if !self.converted_frame.is_null() {
                ffi::av_frame_free(&mut self.converted_frame);
            }
            if !self.encoder_frame.is_null() {
                ffi::av_frame_free(&mut self.encoder_frame);
            }
            if !self.fifo.is_null() {
                ffi::av_audio_fifo_free(self.fifo);
                self.fifo = ptr::null_mut();
            }
            if !self.swr.is_null() {
                ffi::swr_free(&mut self.swr);
            }
        }
    }
}

unsafe fn prepare_audio_frame(
    frame: *mut ffi::AVFrame,
    encoder: *mut ffi::AVCodecContext,
    samples: c_int,
) -> anyhow::Result<()> {
    unsafe {
        ffi::av_frame_unref(frame);
        (*frame).nb_samples = samples;
        (*frame).format = (*encoder).sample_fmt as c_int;
        (*frame).sample_rate = (*encoder).sample_rate;
        check(
            ffi::av_channel_layout_copy(&mut (*frame).ch_layout, &(*encoder).ch_layout),
            "copy audio channel layout",
        )?;
        check(ffi::av_frame_get_buffer(frame, 0), "allocate audio frame")?;
    }
    Ok(())
}

fn output_frame_size(encoder: *mut ffi::AVCodecContext) -> c_int {
    match unsafe { (*encoder).frame_size } {
        1.. => unsafe { (*encoder).frame_size },
        _ => 1024,
    }
}

unsafe fn supported_sample_rate(encoder: *const ffi::AVCodec, preferred: c_int) -> c_int {
    let rates = unsafe { (*encoder).supported_samplerates };
    if rates.is_null() {
        return preferred;
    }
    let mut best = unsafe { *rates };
    let mut index = 0;
    loop {
        let candidate = unsafe { *rates.add(index) };
        if candidate == 0 {
            break;
        }
        if (candidate - preferred).abs() < (best - preferred).abs() {
            best = candidate;
        }
        index += 1;
    }
    best
}

unsafe fn supported_sample_format(encoder: *const ffi::AVCodec) -> ffi::AVSampleFormat {
    let formats = unsafe { (*encoder).sample_fmts };
    if formats.is_null() {
        ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP
    } else {
        unsafe { *formats }
    }
}

unsafe fn select_channel_layout(
    encoder: *const ffi::AVCodec,
    preferred_channels: c_int,
    target: *mut ffi::AVChannelLayout,
) -> anyhow::Result<()> {
    let layouts = unsafe { (*encoder).ch_layouts };
    if layouts.is_null() {
        unsafe { ffi::av_channel_layout_default(target, preferred_channels) };
        return Ok(());
    }
    let mut selected = layouts;
    let mut index = 0;
    loop {
        let candidate = unsafe { layouts.add(index) };
        if unsafe { (*candidate).nb_channels } == 0 {
            break;
        }
        if unsafe { (*candidate).nb_channels } == preferred_channels {
            selected = candidate;
            break;
        }
        index += 1;
    }
    unsafe {
        check(
            ffi::av_channel_layout_copy(target, selected),
            "select encoder channel layout",
        )?;
    }
    Ok(())
}

fn output_names(format: AudioFormat) -> (&'static str, &'static str) {
    match format {
        AudioFormat::Mp3 => ("mp3", "libmp3lame"),
        AudioFormat::Opus => ("opus", "libopus"),
        AudioFormat::Aac => ("adts", "aac"),
        AudioFormat::Flac => ("flac", "flac"),
        AudioFormat::OggVorbis => ("ogg", "libvorbis"),
        AudioFormat::PcmF32Le => ("f32le", "pcm_f32le"),
    }
}

fn c_string(value: &str) -> anyhow::Result<CString> {
    CString::new(value).map_err(|_| anyhow::anyhow!("value contains NUL"))
}

#[cfg(unix)]
fn path_to_c_string(path: &std::path::Path) -> anyhow::Result<CString> {
    use std::os::unix::ffi::OsStrExt;

    CString::new(path.as_os_str().as_bytes()).map_err(|_| anyhow::anyhow!("input contains NUL"))
}

#[cfg(not(unix))]
fn path_to_c_string(path: &std::path::Path) -> anyhow::Result<CString> {
    c_string(&path.to_string_lossy())
}

unsafe fn dictionary_set(
    dictionary: *mut *mut ffi::AVDictionary,
    key: &str,
    value: &str,
) -> anyhow::Result<()> {
    let key = c_string(key)?;
    let value = c_string(value)?;
    unsafe {
        check(
            ffi::av_dict_set(dictionary, key.as_ptr(), value.as_ptr(), 0),
            "set media option",
        )?;
    }
    Ok(())
}

fn ffmpeg_error(code: c_int) -> String {
    let mut buffer = [0 as c_char; 256];
    unsafe {
        ffi::av_strerror(code, buffer.as_mut_ptr(), buffer.len());
        CStr::from_ptr(buffer.as_ptr())
            .to_string_lossy()
            .into_owned()
    }
}

fn check(code: c_int, operation: &str) -> anyhow::Result<c_int> {
    if code < 0 {
        anyhow::bail!("{operation}: {}", ffmpeg_error(code));
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    #[test]
    fn realtime_pacing_allows_a_bounded_initial_burst() {
        assert_eq!(realtime_presentation_delay(0, 44_100), Duration::ZERO);
        assert_eq!(realtime_presentation_delay(22_050, 44_100), Duration::ZERO);
        assert_eq!(
            realtime_presentation_delay(44_100, 44_100),
            Duration::from_millis(500)
        );
    }

    use futures::StreamExt;
    use tokio::io::AsyncReadExt;

    use super::*;

    fn float_wave(seconds: usize) -> Vec<u8> {
        let sample_rate = 48_000_u32;
        let sample_count = sample_rate as usize * seconds;
        float_wave_from_samples(&vec![0.0; sample_count], sample_rate)
    }

    fn float_wave_from_samples(samples: &[f32], sample_rate: u32) -> Vec<u8> {
        let data_bytes = std::mem::size_of_val(samples);
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
        for sample in samples {
            wave.extend_from_slice(&sample.to_le_bytes());
        }
        wave
    }

    async fn collect(mut stream: MediaStream) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk?);
        }
        Ok(bytes)
    }

    #[tokio::test]
    async fn transcodes_all_supported_streaming_formats_without_a_process() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("source.wav");
        std::fs::write(&input, float_wave(1)).unwrap();
        let engine = LibavMediaEngine;

        for (format, signature) in [
            (AudioFormat::Mp3, &b"ID3"[..]),
            (AudioFormat::Opus, &b"OggS"[..]),
            (AudioFormat::Aac, &[0xff][..]),
            (AudioFormat::Flac, &b"fLaC"[..]),
            (AudioFormat::OggVorbis, &b"OggS"[..]),
        ] {
            let mut request = TranscodeRequest::file(input.clone(), format);
            request.bitrate_kbps = Some(64);
            let output = tokio::time::timeout(
                Duration::from_secs(5),
                collect(engine.transcode(request).unwrap()),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(
                output.starts_with(signature)
                    || (format == AudioFormat::Mp3 && output.first().copied() == Some(0xff)),
                "unexpected {format:?} signature"
            );

            let encoded = directory.path().join(format!("encoded-{format:?}"));
            std::fs::write(&encoded, &output).unwrap();
            let mut decode = TranscodeRequest::file(encoded, AudioFormat::PcmF32Le);
            decode.sample_rate = Some(8_000);
            decode.channels = Some(1);
            decode.max_samples = Some(4_000);
            let decoded = collect(engine.transcode(decode).unwrap()).await.unwrap();
            assert_eq!(
                decoded.len(),
                4_000 * size_of::<f32>(),
                "could not decode generated {format:?} output"
            );
        }
    }

    #[tokio::test]
    async fn bounds_pcm_output_by_sample_count() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("source.wav");
        std::fs::write(&input, float_wave(2)).unwrap();
        let mut request = TranscodeRequest::file(input, AudioFormat::PcmF32Le);
        request.sample_rate = Some(8_000);
        request.channels = Some(1);
        request.max_samples = Some(8_000);

        let output = collect(LibavMediaEngine.transcode(request).unwrap())
            .await
            .unwrap();

        assert_eq!(output.len(), 8_000 * size_of::<f32>());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn transcodes_a_non_utf8_local_filename_without_lossy_conversion() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let directory = tempfile::tempdir().unwrap();
        let input = directory
            .path()
            .join(OsString::from_vec(b"source-\x80.wav".to_vec()));
        std::fs::write(&input, float_wave(1)).unwrap();
        let mut request = TranscodeRequest::file(input, AudioFormat::PcmF32Le);
        request.sample_rate = Some(8_000);
        request.channels = Some(1);
        request.max_samples = Some(8_000);

        let output = collect(LibavMediaEngine.transcode(request).unwrap())
            .await
            .unwrap();

        assert_eq!(output.len(), 8_000 * size_of::<f32>());
    }

    #[tokio::test]
    async fn trims_seek_output_to_the_requested_sample_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.wav");
        let samples = (0..48_000 * 3)
            .map(|index| if index < 48_000 { -0.5 } else { 0.5 })
            .collect::<Vec<_>>();
        std::fs::write(&source, float_wave_from_samples(&samples, 48_000)).unwrap();

        let encoded = collect(
            LibavMediaEngine
                .transcode(TranscodeRequest::file(source, AudioFormat::Flac))
                .unwrap(),
        )
        .await
        .unwrap();
        let encoded_path = directory.path().join("source.flac");
        std::fs::write(&encoded_path, encoded).unwrap();

        let mut request = TranscodeRequest::file(encoded_path, AudioFormat::PcmF32Le);
        request.offset = Some(Duration::from_secs(1));
        request.sample_rate = Some(48_000);
        request.channels = Some(1);
        request.max_samples = Some(128);
        let output = collect(LibavMediaEngine.transcode(request).unwrap())
            .await
            .unwrap();
        let first_samples = output
            .chunks_exact(size_of::<f32>())
            .take(32)
            .map(|sample| f32::from_le_bytes(sample.try_into().unwrap()))
            .collect::<Vec<_>>();

        assert_eq!(output.len(), 128 * size_of::<f32>());
        assert!(
            first_samples.iter().all(|sample| *sample > 0.45),
            "seek included audio preceding the requested offset: {first_samples:?}"
        );
    }

    #[tokio::test]
    async fn rejects_damaged_media_as_a_stream_error() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("damaged.mp3");
        std::fs::write(&input, b"not an audio file").unwrap();

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            collect(
                LibavMediaEngine
                    .transcode(TranscodeRequest::file(input, AudioFormat::Mp3))
                    .unwrap(),
            ),
        )
        .await
        .unwrap();

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn local_playlists_cannot_fetch_network_resources() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("remote.m3u8");
        std::fs::write(
            &input,
            format!(
                "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nhttp://{address}/segment.wav\n#EXT-X-ENDLIST\n"
            ),
        )
        .unwrap();

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            collect(
                LibavMediaEngine
                    .transcode(TranscodeRequest::file(input, AudioFormat::PcmF32Le))
                    .unwrap(),
            ),
        )
        .await
        .unwrap();

        assert!(result.is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(250), listener.accept())
                .await
                .is_err(),
            "local media input attempted a network connection"
        );
    }

    #[tokio::test]
    async fn local_concat_playlists_cannot_read_referenced_files() {
        let directory = tempfile::tempdir().unwrap();
        let referenced = directory.path().join("referenced.wav");
        std::fs::write(&referenced, float_wave(1)).unwrap();
        let input = directory.path().join("references.ffconcat");
        std::fs::write(
            &input,
            format!("ffconcat version 1.0\nfile '{}'\n", referenced.display()),
        )
        .unwrap();

        let error = collect(
            LibavMediaEngine
                .transcode(TranscodeRequest::file(input, AudioFormat::PcmF32Le))
                .unwrap(),
        )
        .await
        .unwrap_err();

        let error = error.to_string();
        assert!(
            error.contains("open input") && !error.contains("reference playlists"),
            "local concat input should fail before reading referenced files"
        );
    }

    #[tokio::test]
    async fn dropping_a_network_stream_interrupts_libav_io() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let stream = LibavMediaEngine
            .transcode(TranscodeRequest::radio(
                format!("http://{address}/live.m3u8"),
                RadioInput::Hls,
            ))
            .unwrap();
        let (mut connection, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut request = [0_u8; 1024];
        let received = tokio::time::timeout(Duration::from_secs(2), connection.read(&mut request))
            .await
            .unwrap()
            .unwrap();
        assert!(request[..received].starts_with(b"GET "));

        drop(stream);

        let closed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if connection.read(&mut request).await? == 0 {
                    return Ok::<_, io::Error>(());
                }
            }
        })
        .await;
        assert!(matches!(closed, Ok(Ok(()))));
    }

    #[test]
    fn rejects_unsafe_output_constraints_before_starting_a_worker() {
        let updates: [fn(&mut TranscodeRequest); 4] = [
            |request: &mut TranscodeRequest| request.sample_rate = Some(MAX_SAMPLE_RATE + 1),
            |request: &mut TranscodeRequest| request.channels = Some(MAX_CHANNELS + 1),
            |request: &mut TranscodeRequest| request.max_samples = Some(0),
            |request: &mut TranscodeRequest| request.bitrate_kbps = Some(321),
        ];
        for update in updates {
            let mut request =
                TranscodeRequest::file(PathBuf::from("unused"), AudioFormat::PcmF32Le);
            update(&mut request);
            assert_eq!(
                LibavMediaEngine.transcode(request).err().unwrap().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }
}

//! Blocking demux/decode lives on one worker; the output callback only consumes PCM.
//!
//! A caller supplying a blocking transfer reader must cancel/wake that transfer before
//! shutdown (or before abandoning the engine). `Read + Seek` itself has no interrupt API.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rodio::cpal::traits::{DeviceTrait, HostTrait};
use rodio::{ChannelCount, DeviceSinkBuilder, MixerDeviceSink, SampleRate, Source};
use rtrb::{Consumer, Producer, RingBuffer};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{
    CodecParameters, Decoder, DecoderOptions, CODEC_TYPE_AAC, CODEC_TYPE_MP3, CODEC_TYPE_NULL,
};
use symphonia::core::errors::Error as DecodeError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::{Limit, MetadataOptions};
use symphonia::core::probe::Hint;
use symphonia::core::units::{Time, TimeBase};
use tokio::sync::{broadcast, mpsc, oneshot};

const COMMAND_CAPACITY: usize = 8;
const MAX_CHANNELS: usize = 32;
const MAX_PCM_BYTES: usize = 64 * 1024 * 1024;
const MAX_PACKET_SAMPLES: usize = 2 * 1024 * 1024;
const WORKER_TICK: Duration = Duration::from_millis(2);
const POSITION_INTERVAL: Duration = Duration::from_millis(250);

pub trait AudioRead: Read + Seek + Send + Sync {}
impl<T: Read + Seek + Send + Sync> AudioRead for T {}
pub type AudioInput = Box<dyn AudioRead>;

#[derive(Clone, Debug)]
pub struct AudioOptions {
    pub device: String,
    pub pcm_buffer_ms: u64,
}

#[derive(Clone, Debug)]
pub enum AudioEvent {
    Loaded { duration_ms: u64 },
    Position { position_ms: u64, duration_ms: u64 },
    End,
    Error { message: String, retryable: bool },
}

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("audio output: {0}")]
    Device(String),
    #[error("audio I/O: {message}")]
    Io {
        kind: io::ErrorKind,
        message: String,
    },
    #[error("unsupported audio: {0}")]
    Unsupported(String),
    #[error("audio decoding: {0}")]
    Decode(String),
    #[error("audio seek: {0}")]
    Seek(String),
    #[error("invalid audio option: {0}")]
    InvalidOption(String),
    #[error("no audio source is loaded")]
    NoSource,
    #[error("audio command was superseded")]
    Superseded,
    #[error("audio worker has stopped")]
    Closed,
    #[error("could not start audio worker: {0}")]
    Spawn(String),
}

impl AudioError {
    pub fn user_message(&self) -> String {
        self.to_string()
    }

    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Device(_) => true,
            Self::Io { kind, .. } => matches!(
                kind,
                io::ErrorKind::TimedOut
                    | io::ErrorKind::WouldBlock
                    | io::ErrorKind::Interrupted
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::NotConnected
                    | io::ErrorKind::BrokenPipe
            ),
            _ => false,
        }
    }
}

impl From<io::Error> for AudioError {
    fn from(error: io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

type Reply = oneshot::Sender<Result<(), AudioError>>;

enum Command {
    Load {
        input: AudioInput,
        byte_len: Option<u64>,
        start_ms: u64,
        paused: bool,
        reply: Reply,
    },
    Seek {
        position_ms: u64,
        reply: Reply,
    },
    Shutdown {
        reply: Reply,
    },
}

struct Shared {
    cancelled: AtomicBool,
    paused: AtomicBool,
    looping: AtomicBool,
    volume: AtomicU32,
    epoch: AtomicU64,
    target_ms: AtomicU64,
    audible_epoch: AtomicU64,
    audible_ms: AtomicU64,
    duration_ms: AtomicU64,
}

impl Shared {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            looping: AtomicBool::new(false),
            volume: AtomicU32::new(1.0_f32.to_bits()),
            epoch: AtomicU64::new(1),
            target_ms: AtomicU64::new(0),
            audible_epoch: AtomicU64::new(0),
            audible_ms: AtomicU64::new(0),
            duration_ms: AtomicU64::new(0),
        }
    }

    fn position_ms(&self) -> u64 {
        loop {
            let epoch = self.epoch.load(Ordering::Acquire);
            let audible_epoch = self.audible_epoch.load(Ordering::Acquire);
            let position = if audible_epoch == epoch {
                self.audible_ms.load(Ordering::Relaxed)
            } else {
                self.target_ms.load(Ordering::Relaxed)
            };
            if self.epoch.load(Ordering::Acquire) == epoch {
                return position;
            }
        }
    }

    fn reposition(&self, position_ms: u64) -> u64 {
        self.target_ms.store(position_ms, Ordering::Relaxed);
        self.epoch.fetch_add(1, Ordering::AcqRel).wrapping_add(1)
    }
}

/// The handle contains no device or decoder; both stay on the dedicated worker.
/// Creating it is lazy: only `load` opens an audio device.
pub struct AudioEngine {
    commands: mpsc::Sender<Command>,
    events: broadcast::Sender<AudioEvent>,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for AudioEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioEngine")
            .field("position_ms", &self.position_ms())
            .field("duration_ms", &self.duration_ms())
            .field("paused", &self.is_paused())
            .finish_non_exhaustive()
    }
}

impl AudioEngine {
    pub async fn spawn(options: AudioOptions) -> Result<Self, AudioError> {
        if options.pcm_buffer_ms == 0 {
            return Err(AudioError::InvalidOption(
                "pcm_buffer_ms must be positive".into(),
            ));
        }
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let (events, _) = broadcast::channel(32);
        let shared = Arc::new(Shared::new());
        let worker_shared = shared.clone();
        let worker_events = events.clone();
        let worker = thread::Builder::new()
            .name("yoruplay-decoder".into())
            .spawn(move || worker_main(options, receiver, worker_shared, worker_events))
            .map_err(|error| AudioError::Spawn(error.to_string()))?;
        Ok(Self {
            commands,
            events,
            shared,
            worker: Some(worker),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AudioEvent> {
        self.events.subscribe()
    }

    pub async fn load(
        &self,
        input: AudioInput,
        byte_len: Option<u64>,
        start_ms: u64,
        paused: bool,
    ) -> Result<(), AudioError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::Load {
            input,
            byte_len,
            start_ms,
            paused,
            reply,
        })
        .await?;
        response.await.map_err(|_| AudioError::Closed)?
    }

    pub async fn pause(&self, paused: bool) -> Result<(), AudioError> {
        self.ensure_open()?;
        // Do not wait for an in-flight blocking read. At most that packet finishes;
        // the worker checks this gate before issuing the next packet demand.
        self.shared.paused.store(paused, Ordering::Release);
        self.wake();
        Ok(())
    }

    pub async fn seek_ms(&self, position_ms: u64) -> Result<(), AudioError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::Seek { position_ms, reply }).await?;
        response.await.map_err(|_| AudioError::Closed)?
    }

    pub async fn set_volume(&self, volume: f64) -> Result<(), AudioError> {
        self.ensure_open()?;
        if !volume.is_finite() {
            return Err(AudioError::InvalidOption("volume must be finite".into()));
        }
        self.shared
            .volume
            .store((volume.clamp(0.0, 1.0) as f32).to_bits(), Ordering::Release);
        Ok(())
    }

    pub async fn set_loop_file(&self, looping: bool) -> Result<(), AudioError> {
        self.ensure_open()?;
        self.shared.looping.store(looping, Ordering::Release);
        self.wake();
        Ok(())
    }

    pub fn position_ms(&self) -> u64 {
        self.shared.position_ms()
    }

    pub fn duration_ms(&self) -> u64 {
        self.shared.duration_ms.load(Ordering::Acquire)
    }

    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Acquire)
    }

    pub async fn shutdown(mut self) -> Result<(), AudioError> {
        // Also closes the ring gate immediately, even if the decoder is in Read.
        self.shared.cancelled.store(true, Ordering::Release);
        let (reply, response) = oneshot::channel();
        let sent = self
            .commands
            .send(Command::Shutdown { reply })
            .await
            .is_ok();
        self.wake();
        if sent {
            let _ = response.await;
        }
        if let Some(worker) = self.worker.take() {
            tokio::task::spawn_blocking(move || worker.join())
                .await
                .map_err(|error| AudioError::Spawn(error.to_string()))?
                .map_err(|_| AudioError::Closed)?;
        }
        Ok(())
    }

    async fn send(&self, command: Command) -> Result<(), AudioError> {
        self.ensure_open()?;
        self.commands
            .send(command)
            .await
            .map_err(|_| AudioError::Closed)?;
        self.wake();
        Ok(())
    }

    fn ensure_open(&self) -> Result<(), AudioError> {
        if self.shared.cancelled.load(Ordering::Acquire) || self.commands.is_closed() {
            Err(AudioError::Closed)
        } else {
            Ok(())
        }
    }

    fn wake(&self) {
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.shared.cancelled.store(true, Ordering::Release);
        let (reply, _) = oneshot::channel();
        let _ = self.commands.try_send(Command::Shutdown { reply });
        self.wake();
    }
}

// Every sample has a load/seek epoch. The consumer, not the producer, discards
// invalidated PCM, preserving rtrb's single-consumer ownership even during seek.
#[derive(Clone, Copy, Debug)]
struct PcmSample {
    epoch: u64,
    value: f32,
}

struct OutputState {
    alive: AtomicBool,
    eof_epoch: AtomicU64,
    drained_epoch: AtomicU64,
}

impl OutputState {
    fn new() -> Self {
        Self {
            alive: AtomicBool::new(true),
            eof_epoch: AtomicU64::new(0),
            drained_epoch: AtomicU64::new(0),
        }
    }
}

struct RingSource {
    consumer: Consumer<PcmSample>,
    shared: Arc<Shared>,
    output: Arc<OutputState>,
    channels: ChannelCount,
    sample_rate: SampleRate,
    epoch: u64,
    base_ms: u64,
    played_frames: u64,
    frame: [f32; MAX_CHANNELS],
    channel: usize,
    media_frame: bool,
}

impl RingSource {
    fn new(
        consumer: Consumer<PcmSample>,
        shared: Arc<Shared>,
        output: Arc<OutputState>,
        channels: ChannelCount,
        sample_rate: SampleRate,
    ) -> Self {
        Self {
            consumer,
            shared,
            output,
            channels,
            sample_rate,
            epoch: 0,
            base_ms: 0,
            played_frames: 0,
            frame: [0.0; MAX_CHANNELS],
            channel: 0,
            media_frame: false,
        }
    }

    fn prepare_frame(&mut self) {
        let epoch = self.shared.epoch.load(Ordering::Acquire);
        if self.epoch != epoch {
            self.epoch = epoch;
            self.base_ms = self.shared.target_ms.load(Ordering::Relaxed);
            self.played_frames = 0;
        }
        // Bound seek-discard work per callback invocation as well as total storage.
        for _ in 0..64 {
            match self.consumer.peek() {
                Ok(sample) if sample.epoch != epoch => {
                    let _ = self.consumer.pop();
                }
                _ => break,
            }
        }
        self.media_frame = false;
        self.frame[..self.channels.get() as usize].fill(0.0);
        if self.shared.paused.load(Ordering::Acquire) {
            return;
        }
        // Acquire EOF before checking slots: EOF publishes all preceding PCM.
        let eof = self.output.eof_epoch.load(Ordering::Acquire) == epoch;
        let channels = self.channels.get() as usize;
        if self.consumer.slots() >= channels
            && self
                .consumer
                .peek()
                .is_ok_and(|sample| sample.epoch == epoch)
        {
            for sample in &mut self.frame[..channels] {
                // A producer publishes only complete frames and never overwrites.
                *sample = self.consumer.pop().expect("complete PCM frame").value;
            }
            self.media_frame = true;
        } else if eof && self.consumer.is_empty() {
            // Keep this Source attached until the worker has drained the OS
            // buffer, or seeks/loops. Only teardown terminates the iterator.
            self.output.drained_epoch.store(epoch, Ordering::Release);
        }
    }
}

impl Iterator for RingSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if !self.output.alive.load(Ordering::Acquire)
            || self.shared.cancelled.load(Ordering::Acquire)
        {
            return None;
        }
        if self.channel == 0 {
            self.prepare_frame();
        }
        let value = if self.shared.paused.load(Ordering::Acquire) {
            0.0
        } else {
            self.frame[self.channel]
        };
        self.channel += 1;
        if self.channel == self.channels.get() as usize {
            self.channel = 0;
            if self.media_frame {
                self.played_frames += 1;
                if self.shared.epoch.load(Ordering::Acquire) == self.epoch {
                    let elapsed = (u128::from(self.played_frames) * 1000
                        / u128::from(self.sample_rate.get()))
                        as u64;
                    self.shared
                        .audible_ms
                        .store(self.base_ms.saturating_add(elapsed), Ordering::Relaxed);
                    self.shared
                        .audible_epoch
                        .store(self.epoch, Ordering::Release);
                }
            }
        }
        Some(value * f32::from_bits(self.shared.volume.load(Ordering::Relaxed)))
    }
}

impl Source for RingSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> ChannelCount {
        self.channels
    }
    fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }
    // In particular, never let a duration adapter stop the ring on an underrun.
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

#[derive(Default)]
struct ReadEvidence {
    position: u64,
    eof_position: Option<u64>,
    failure: Option<(io::ErrorKind, String)>,
}

struct InputSource {
    input: AudioInput,
    byte_len: Option<u64>,
    shared: Arc<Shared>,
    evidence: Arc<Mutex<ReadEvidence>>,
}

impl Read for InputSource {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.shared.cancelled.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "audio cancelled",
            ));
        }
        let result = self.input.read(buffer);
        let mut evidence = self
            .evidence
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match &result {
            Ok(0) if !buffer.is_empty() => {
                evidence.eof_position = Some(evidence.position);
                if self
                    .byte_len
                    .is_some_and(|length| evidence.position < length)
                {
                    let error = io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "audio ended before its declared byte length",
                    );
                    evidence.failure = Some((error.kind(), error.to_string()));
                    return Err(error);
                }
            }
            Ok(bytes) => {
                evidence.position = evidence.position.saturating_add(*bytes as u64);
            }
            Err(error) => {
                evidence.failure = Some((error.kind(), error.to_string()));
            }
        }
        result
    }
}

impl Seek for InputSource {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if self.shared.cancelled.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "audio cancelled",
            ));
        }
        let result = self.input.seek(position);
        let mut evidence = self
            .evidence
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Format probing may legitimately test an unsupported seek and recover.
        // Only an explicit seek operation propagates that seek failure.
        if let Ok(position) = &result {
            evidence.position = *position;
            evidence.eof_position = None;
        }
        result
    }
}

impl MediaSource for InputSource {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        self.byte_len
    }
}

struct TrackDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    params: CodecParameters,
    track_id: u32,
    evidence: Arc<Mutex<ReadEvidence>>,
    buffer: Option<SampleBuffer<f32>>,
    offset: usize,
    channels: ChannelCount,
    sample_rate: SampleRate,
    seek_target_ts: Option<u64>,
    last_packet_end: u64,
    decoded_frames: u64,
    duration_ms: u64,
    frame_count_estimated: bool,
    at_end: bool,
}

impl TrackDecoder {
    fn open(
        input: AudioInput,
        byte_len: Option<u64>,
        shared: Arc<Shared>,
    ) -> Result<Self, AudioError> {
        let evidence = Arc::new(Mutex::new(ReadEvidence::default()));
        let source = InputSource {
            input,
            byte_len,
            shared,
            evidence: evidence.clone(),
        };
        let stream = MediaSourceStream::new(
            Box::new(source),
            MediaSourceStreamOptions {
                buffer_len: 64 * 1024,
            },
        );
        let options = FormatOptions {
            enable_gapless: true,
            ..FormatOptions::default()
        };
        let metadata = MetadataOptions {
            limit_metadata_bytes: Limit::Maximum(128 * 1024),
            limit_visual_bytes: Limit::Maximum(512 * 1024),
        };
        let probed = symphonia::default::get_probe()
            .format(&Hint::new(), stream, &options, &metadata)
            .map_err(map_decode_error)?;
        let format = probed.format;
        let mut selected = None;
        let mut unsupported = None;
        for track in format.tracks() {
            if track.codec_params.codec == CODEC_TYPE_NULL {
                continue;
            }
            match symphonia::default::get_codecs()
                .make(&track.codec_params, &DecoderOptions::default())
            {
                Ok(decoder) => {
                    selected = Some((track.id, track.codec_params.clone(), decoder));
                    break;
                }
                Err(error) => {
                    unsupported = Some(error);
                }
            }
        }
        let (track_id, params, decoder) = selected.ok_or_else(|| {
            unsupported
                .map(map_decode_error)
                .unwrap_or_else(|| AudioError::Unsupported("no playable audio track".into()))
        })?;
        let time_base = params.time_base.or_else(|| {
            params
                .sample_rate
                .filter(|rate| *rate > 0)
                .map(|rate| TimeBase::new(1, rate))
        });
        let duration_ms = time_base
            .zip(params.n_frames)
            .map(|(base, frames)| time_ms(base.calc_time(frames)))
            .unwrap_or(0);
        // Native ADTS always estimates n_frames from sampled bitrates. MPEG
        // audio without a LAME delay/padding header may do the same; those
        // counts are display estimates, not authoritative end-of-stream proof.
        let frame_count_estimated = (params.codec == CODEC_TYPE_AAC && params.extra_data.is_none())
            || (params.codec == CODEC_TYPE_MP3 && params.delay.is_none());
        let sample_rate = SampleRate::new(params.sample_rate.unwrap_or(1))
            .ok_or_else(|| AudioError::Decode("zero sample rate".into()))?;
        let channel_count = params.channels.map_or(1, |channels| channels.count());
        let channels = checked_channels(channel_count)?;
        let mut result = Self {
            format,
            decoder,
            params,
            track_id,
            evidence,
            buffer: None,
            offset: 0,
            channels,
            sample_rate,
            seek_target_ts: None,
            last_packet_end: 0,
            decoded_frames: 0,
            duration_ms,
            frame_count_estimated,
            at_end: false,
        };
        // Some headers omit the signal specification. Decode exactly one packet,
        // retaining it as the first bounded PCM packet rather than losing samples.
        if !result.decode_packet()? {
            return Err(AudioError::Decode(
                "audio contains no decoded frames".into(),
            ));
        }
        Ok(result)
    }

    fn decode_packet(&mut self) -> Result<bool, AudioError> {
        if self.at_end {
            return Ok(false);
        }
        loop {
            let mut packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(error) => {
                    if let Some(error) = self.recorded_failure() {
                        return Err(error);
                    }
                    if matches!(&error, DecodeError::IoError(error) if error.kind() == io::ErrorKind::UnexpectedEof)
                    {
                        return self.validate_eof(&error).map(|()| false);
                    }
                    return Err(map_decode_error(error));
                }
            };
            // Do not accumulate metadata revisions during long streams.
            let mut metadata = self.format.metadata();
            while !metadata.is_latest() {
                metadata.pop();
            }
            if packet.track_id() != self.track_id {
                continue;
            }
            if self.frame_count_estimated && self.params.codec == CODEC_TYPE_MP3 {
                // MpaReader also applies its estimated n_frames as gapless
                // end trimming. Restore the packet rather than silently cutting
                // valid VBR samples at a bitrate-derived duration estimate.
                packet.dur = packet.block_dur();
                packet.ts = packet.ts.saturating_sub(u64::from(packet.trim_start));
                packet.trim_start = 0;
                packet.trim_end = 0;
            }
            let packet_ts = packet.ts();
            let packet_end = packet_ts.saturating_add(packet.dur());
            let decoded = self.decoder.decode(&packet).map_err(map_decode_error)?;
            let spec = *decoded.spec();
            let channels = checked_channels(spec.channels.count())?;
            let sample_rate = SampleRate::new(spec.rate)
                .ok_or_else(|| AudioError::Decode("zero decoded sample rate".into()))?;
            let samples = decoded
                .capacity()
                .checked_mul(channels.get() as usize)
                .ok_or_else(|| AudioError::Decode("decoded packet is too large".into()))?;
            if samples > MAX_PACKET_SAMPLES {
                return Err(AudioError::Decode(
                    "decoded packet exceeds the bounded PCM packet limit".into(),
                ));
            }
            if self.buffer.is_some()
                && (self.channels != channels || self.sample_rate != sample_rate)
            {
                return Err(AudioError::Unsupported(
                    "sample format changes inside one audio track".into(),
                ));
            }
            self.channels = channels;
            self.sample_rate = sample_rate;
            if self
                .buffer
                .as_ref()
                .is_none_or(|buffer| buffer.capacity() < samples)
            {
                self.buffer = Some(SampleBuffer::new(decoded.capacity() as u64, spec));
            }
            let buffer = self.buffer.as_mut().expect("PCM packet buffer");
            buffer.copy_interleaved_ref(decoded);
            let frames = (buffer.len() / channels.get() as usize) as u64;
            let base = self.params.time_base.unwrap_or(TimeBase::new(1, spec.rate));
            let decoded_ticks = (u128::from(frames) * u128::from(base.denom))
                .div_ceil(u128::from(spec.rate) * u128::from(base.numer))
                .min(u128::from(u64::MAX)) as u64;
            // A RIFF demuxer can return a short final payload while retaining
            // its declared packet duration. Only decoded samples prove its end.
            self.last_packet_end = packet_ts.saturating_add(packet.dur().min(decoded_ticks));
            self.decoded_frames = self.decoded_frames.saturating_add(frames);
            self.offset = 0;
            if let Some(target) = self.seek_target_ts {
                let base = self.params.time_base.unwrap_or(TimeBase::new(1, spec.rate));
                let skip = u128::from(target.saturating_sub(packet_ts))
                    * u128::from(base.numer)
                    * u128::from(spec.rate)
                    / u128::from(base.denom);
                let frames = skip.min((buffer.len() / channels.get() as usize) as u128) as usize;
                self.offset = frames * channels.get() as usize;
                if packet_end > target {
                    self.seek_target_ts = None;
                }
            }
            if self.offset < buffer.len() {
                return Ok(true);
            }
        }
    }

    fn recorded_failure(&self) -> Option<AudioError> {
        self.evidence
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .failure
            .as_ref()
            .map(|(kind, message)| AudioError::Io {
                kind: *kind,
                message: message.clone(),
            })
    }

    fn validate_eof(&self, error: &DecodeError) -> Result<(), AudioError> {
        let evidence = self
            .evidence
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let expected_end = self
            .params
            .n_frames
            .filter(|_| !self.frame_count_estimated)
            .map(|frames| self.params.start_ts.saturating_add(frames));
        if expected_end.is_some_and(|expected| self.last_packet_end < expected) {
            return Err(AudioError::Decode(format!(
                "truncated audio: last packet ends at {}, expected {}",
                self.last_packet_end,
                expected_end.unwrap()
            )));
        }
        let reached_declared_end = expected_end.is_some() && self.decoded_frames > 0;
        let physical_end = evidence.eof_position.is_some();
        // Symphonia uses UnexpectedEof for its ordinary packet-iterator end.
        // Only accept that sentinel with independent endpoint evidence. Reader
        // failures, premature known-length EOF and declared missing frames fail.
        let ordinary_sentinel =
            matches!(error, DecodeError::IoError(error) if error.to_string() == "end of stream");
        if self.decoded_frames > 0 && (reached_declared_end || (physical_end && ordinary_sentinel))
        {
            Ok(())
        } else {
            Err(AudioError::Decode(format!("unverified audio EOF: {error}")))
        }
    }

    fn seek(&mut self, position_ms: u64) -> Result<u64, AudioError> {
        let target = if self.duration_ms == 0 || self.frame_count_estimated {
            position_ms
        } else {
            position_ms.min(self.duration_ms)
        };
        if !self.frame_count_estimated && self.duration_ms > 0 && target == self.duration_ms {
            self.offset = self.buffer.as_ref().map_or(0, SampleBuffer::len);
            self.at_end = true;
            return Ok(target);
        }
        {
            let mut evidence = self
                .evidence
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            evidence.failure = None;
            evidence.eof_position = None;
        }
        let sought = self
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: Time::new(target / 1000, (target % 1000) as f64 / 1000.0),
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|error| {
                self.recorded_failure().unwrap_or_else(|| match error {
                    DecodeError::IoError(error) => error.into(),
                    error => AudioError::Seek(error.to_string()),
                })
            })?;
        self.decoder.reset();
        self.at_end = false;
        self.offset = self.buffer.as_ref().map_or(0, SampleBuffer::len);
        let delay = if self.params.codec == CODEC_TYPE_MP3 {
            self.params.delay.unwrap_or(0)
        } else {
            0
        };
        self.seek_target_ts = Some(sought.required_ts.saturating_sub(u64::from(delay)));
        self.last_packet_end = sought.actual_ts.saturating_sub(u64::from(delay));
        self.decoded_frames = 0;
        Ok(target)
    }
}

fn checked_channels(channels: usize) -> Result<ChannelCount, AudioError> {
    if channels == 0 || channels > MAX_CHANNELS {
        return Err(AudioError::Unsupported(format!(
            "channel count {channels} is outside 1..={MAX_CHANNELS}"
        )));
    }
    Ok(ChannelCount::new(channels as u16).expect("validated channel count"))
}

fn time_ms(time: Time) -> u64 {
    time.seconds
        .saturating_mul(1000)
        .saturating_add((time.frac * 1000.0) as u64)
}

fn map_decode_error(error: DecodeError) -> AudioError {
    match error {
        DecodeError::IoError(error) => error.into(),
        DecodeError::Unsupported(message) => AudioError::Unsupported(message.into()),
        error => AudioError::Decode(error.to_string()),
    }
}

struct Session {
    track: TrackDecoder,
    producer: Producer<PcmSample>,
    output: Arc<OutputState>,
    _device: MixerDeviceSink,
    errors: std_mpsc::Receiver<rodio::cpal::StreamError>,
    epoch: u64,
    produced_frames: u64,
    base_ms: u64,
    decoding_ended: bool,
    end_deadline: Option<Instant>,
    drain_delay: Duration,
}

impl Session {
    fn open(
        options: &AudioOptions,
        input: AudioInput,
        byte_len: Option<u64>,
        start_ms: u64,
        shared: Arc<Shared>,
    ) -> Result<Self, AudioError> {
        let mut track = TrackDecoder::open(input, byte_len, shared.clone())?;
        let base_ms = if start_ms == 0 {
            0
        } else {
            track.seek(start_ms)?
        };
        let samples = u128::from(track.sample_rate.get())
            * u128::from(track.channels.get())
            * u128::from(options.pcm_buffer_ms);
        let samples = samples.div_ceil(1000).max(u128::from(track.channels.get()));
        let channels = track.channels.get() as usize;
        let capacity = usize::try_from(samples)
            .ok()
            .and_then(|samples| samples.checked_add(channels - 1))
            .map(|samples| samples / channels * channels)
            .filter(|samples| *samples <= MAX_PCM_BYTES / std::mem::size_of::<PcmSample>())
            .ok_or_else(|| AudioError::InvalidOption("PCM ring would exceed 64 MiB".into()))?;
        let (producer, consumer) = RingBuffer::new(capacity);
        let output = Arc::new(OutputState::new());
        let (error_sender, errors) = std_mpsc::sync_channel(4);
        let (device, _) = open_output(&options.device, error_sender)?;
        let config = device.config();
        let drain_delay = match config.buffer_size() {
            rodio::cpal::BufferSize::Fixed(frames) => Duration::from_secs_f64(
                f64::from(*frames) * 3.0 / f64::from(config.sample_rate().get()),
            ),
            rodio::cpal::BufferSize::Default => Duration::from_millis(200),
        };
        let epoch = shared.reposition(base_ms);
        shared
            .duration_ms
            .store(track.duration_ms, Ordering::Release);
        let source = RingSource::new(
            consumer,
            shared,
            output.clone(),
            track.channels,
            track.sample_rate,
        );
        device.mixer().add(source);
        Ok(Self {
            track,
            producer,
            output,
            _device: device,
            errors,
            epoch,
            produced_frames: 0,
            base_ms,
            decoding_ended: false,
            end_deadline: None,
            drain_delay,
        })
    }

    fn seek(&mut self, position_ms: u64, shared: &Arc<Shared>) -> Result<(), AudioError> {
        // Invalidate before the potentially blocking seek so old audio cannot
        // continue playing while the new byte range is being acquired.
        self.epoch = shared.reposition(position_ms);
        self.output.eof_epoch.store(0, Ordering::Release);
        self.output.drained_epoch.store(0, Ordering::Release);
        let target = self.track.seek(position_ms)?;
        self.epoch = shared.reposition(target);
        self.base_ms = target;
        self.produced_frames = 0;
        self.decoding_ended = false;
        self.end_deadline = None;
        Ok(())
    }

    fn tick(&mut self, shared: &Arc<Shared>) -> Result<bool, AudioError> {
        if let Ok(error) = self.errors.try_recv() {
            return Err(AudioError::Device(error.to_string()));
        }
        if self.producer.is_abandoned() {
            return Err(AudioError::Device(
                "audio output stopped consuming PCM".into(),
            ));
        }
        if self.output.drained_epoch.load(Ordering::Acquire) == self.epoch {
            if shared.looping.load(Ordering::Acquire) {
                self.seek(0, shared)?;
            } else {
                // PCM consumed by the callback can still be queued in the OS.
                // Keep the device alive across its configured buffering before
                // notifying the service, which may immediately close this engine.
                let deadline = self
                    .end_deadline
                    .get_or_insert_with(|| Instant::now() + self.drain_delay);
                return Ok(Instant::now() >= *deadline);
            }
        }
        if shared.paused.load(Ordering::Acquire) || self.decoding_ended {
            return Ok(false);
        }
        let channels = self.track.channels.get() as usize;
        let free_samples = self.producer.slots() / channels * channels;
        if free_samples == 0 {
            return Ok(false);
        }
        if self
            .track
            .buffer
            .as_ref()
            .is_none_or(|buffer| self.track.offset >= buffer.len())
            && !self.track.decode_packet()?
        {
            self.decoding_ended = true;
            if self.track.duration_ms == 0 || self.track.frame_count_estimated {
                self.track.duration_ms = self
                    .track
                    .params
                    .time_base
                    .map(|base| {
                        time_ms(
                            base.calc_time(
                                self.track
                                    .last_packet_end
                                    .saturating_sub(self.track.params.start_ts),
                            ),
                        )
                    })
                    .unwrap_or_else(|| {
                        self.base_ms.saturating_add(
                            (u128::from(self.produced_frames) * 1000
                                / u128::from(self.track.sample_rate.get()))
                                as u64,
                        )
                    });
                shared
                    .duration_ms
                    .store(self.track.duration_ms, Ordering::Release);
            }
            self.output.eof_epoch.store(self.epoch, Ordering::Release);
            return Ok(false);
        }
        let samples = self
            .track
            .buffer
            .as_ref()
            .expect("decoded PCM packet")
            .samples();
        let count = free_samples.min(samples.len() - self.track.offset);
        let count = count / channels * channels;
        for value in &samples[self.track.offset..self.track.offset + count] {
            self.producer
                .push(PcmSample {
                    epoch: self.epoch,
                    value: *value,
                })
                .expect("reserved PCM ring capacity");
        }
        self.track.offset += count;
        self.produced_frames = self
            .produced_frames
            .saturating_add((count / channels) as u64);
        Ok(false)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.output.alive.store(false, Ordering::Release);
        // The device is owned here so it closes before the worker acknowledges
        // replacement/shutdown; its Source never waits for a decoder/read lock.
    }
}

fn worker_main(
    options: AudioOptions,
    mut commands: mpsc::Receiver<Command>,
    shared: Arc<Shared>,
    events: broadcast::Sender<AudioEvent>,
) {
    let mut session: Option<Session> = None;
    let mut pending = None;
    let mut last_position = Instant::now();
    loop {
        if shared.cancelled.load(Ordering::Acquire) {
            break;
        }
        let command = pending.take().or_else(|| commands.try_recv().ok());
        let command = if command.is_none() && session.is_none() {
            commands.blocking_recv()
        } else {
            command
        };
        if let Some(mut command) = command {
            // Coalesce adjacent queued loads/seeks without growing an auxiliary
            // queue or retaining an unbounded number of readers.
            while let Ok(next) = commands.try_recv() {
                match (&command, &next) {
                    (Command::Load { .. }, Command::Load { .. })
                    | (Command::Seek { .. }, Command::Seek { .. }) => {
                        reject_superseded(command);
                        command = next;
                    }
                    _ => {
                        pending = Some(next);
                        break;
                    }
                }
            }
            match command {
                Command::Load {
                    input,
                    byte_len,
                    start_ms,
                    paused,
                    reply,
                } => {
                    drop(session.take());
                    shared.paused.store(paused, Ordering::Release);
                    shared.duration_ms.store(0, Ordering::Release);
                    shared.reposition(start_ms);
                    if reply.is_closed() {
                        continue;
                    }
                    match Session::open(&options, input, byte_len, start_ms, shared.clone()) {
                        Ok(loaded) => {
                            let duration_ms = loaded.track.duration_ms;
                            session = Some(loaded);
                            let _ = events.send(AudioEvent::Loaded { duration_ms });
                            let _ = reply.send(Ok(()));
                        }
                        Err(error) => {
                            let _ = reply.send(Err(error));
                        }
                    }
                }
                Command::Seek { position_ms, reply } => {
                    if reply.is_closed() {
                        continue;
                    }
                    let previous_position = shared.position_ms();
                    let result = session
                        .as_mut()
                        .ok_or(AudioError::NoSource)
                        .and_then(|session| session.seek(position_ms, &shared));
                    if let Err(error) = &result {
                        drop(session.take());
                        shared.reposition(previous_position);
                        let _ = events.send(AudioEvent::Error {
                            message: error.user_message(),
                            retryable: error.is_retryable(),
                        });
                    }
                    let _ = reply.send(result);
                }
                Command::Shutdown { reply } => {
                    drop(session.take());
                    let _ = reply.send(Ok(()));
                    break;
                }
            }
        } else if commands.is_closed() {
            break;
        }
        if shared.cancelled.load(Ordering::Acquire) {
            break;
        }
        if let Some(current) = session.as_mut() {
            match current.tick(&shared) {
                Ok(true) => {
                    let _ = events.send(AudioEvent::Position {
                        position_ms: shared.position_ms(),
                        duration_ms: shared.duration_ms.load(Ordering::Acquire),
                    });
                    drop(session.take());
                    let _ = events.send(AudioEvent::End);
                }
                Ok(false) => {}
                Err(error) => {
                    drop(session.take());
                    if shared.cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    let _ = events.send(AudioEvent::Error {
                        message: error.user_message(),
                        retryable: error.is_retryable(),
                    });
                }
            }
        }
        if session.is_some() && last_position.elapsed() >= POSITION_INTERVAL {
            let _ = events.send(AudioEvent::Position {
                position_ms: shared.position_ms(),
                duration_ms: shared.duration_ms.load(Ordering::Acquire),
            });
            last_position = Instant::now();
        }
        // Fill available PCM without sleeping between packets. Park only once
        // full/paused/ended, and wake immediately for control commands.
        let should_park = session.as_ref().is_some_and(|session| {
            shared.paused.load(Ordering::Acquire)
                || session.decoding_ended
                || session.producer.slots() < session.track.channels.get() as usize
        });
        if should_park {
            thread::park_timeout(WORKER_TICK);
        }
    }
    drop(session);
}

fn reject_superseded(command: Command) {
    match command {
        Command::Load { reply, .. } | Command::Seek { reply, .. } | Command::Shutdown { reply } => {
            let _ = reply.send(Err(AudioError::Superseded));
        }
    }
}

fn device_identity(device: &rodio::cpal::Device) -> (String, String) {
    let id = device.id().map(|id| id.to_string()).unwrap_or_default();
    let name = device
        .description()
        .map(|description| description.name().to_owned())
        .unwrap_or_default();
    (id, name)
}

fn open_output(
    requested: &str,
    errors: std_mpsc::SyncSender<rodio::cpal::StreamError>,
) -> Result<(MixerDeviceSink, String), AudioError> {
    let host = rodio::cpal::default_host();
    let requested = requested.trim();
    let mut candidates = Vec::new();
    if !requested.is_empty() {
        if let Ok(id) = requested.parse::<rodio::cpal::DeviceId>() {
            if let Some(device) = host.device_by_id(&id) {
                candidates.push(device);
            }
        }
        if candidates.is_empty() {
            let devices = host
                .output_devices()
                .map_err(|error| AudioError::Device(error.to_string()))?;
            for device in devices {
                let (id, name) = device_identity(&device);
                if id == requested
                    || name == requested
                    || id.split_once(':').is_some_and(|(_, raw)| raw == requested)
                {
                    candidates.push(device);
                }
            }
        }
        if candidates.is_empty() {
            return Err(AudioError::Device(format!(
                "configured output device {requested:?} was not found"
            )));
        }
    } else {
        // Do not invoke rodio's open_default_sink: its last-resort fallback
        // enumerates raw hardware and can unexpectedly take an exclusive ALSA
        // device. Only shared logical routes and the configured system default
        // are automatic choices; explicit hardware selection remains possible.
        if let Ok(devices) = host.output_devices() {
            let mut shared_routes = Vec::new();
            for device in devices {
                let (id, name) = device_identity(&device);
                let raw = id.split_once(':').map_or(id.as_str(), |(_, raw)| raw);
                let priority = match raw {
                    "pipewire" => Some(0),
                    "pulse" => Some(1),
                    _ if name.eq_ignore_ascii_case("pipewire") => Some(0),
                    _ if name.eq_ignore_ascii_case("pulse") => Some(1),
                    _ => None,
                };
                if let Some(priority) = priority {
                    shared_routes.push((priority, device));
                }
            }
            shared_routes.sort_by_key(|(priority, _)| *priority);
            candidates.extend(shared_routes.into_iter().map(|(_, device)| device));
        }
        if let Some(device) = host.default_output_device() {
            let identity = device_identity(&device).0;
            if !candidates
                .iter()
                .any(|candidate| device_identity(candidate).0 == identity)
            {
                candidates.push(device);
            }
        }
    }
    let mut failures = Vec::new();
    for device in candidates {
        let (id, name) = device_identity(&device);
        let label = if id.is_empty() { name } else { id };
        let sender = errors.clone();
        let opened = DeviceSinkBuilder::from_device(device)
            .map(|builder| {
                builder.with_error_callback(move |error| {
                    let _ = sender.try_send(error);
                })
            })
            .and_then(|builder| builder.open_sink_or_fallback());
        match opened {
            Ok(mut output) => {
                output.log_on_drop(false);
                return Ok((output, label));
            }
            Err(error) => {
                failures.push(format!("{label}: {error}"));
            }
        }
    }
    Err(AudioError::Device(if failures.is_empty() {
        "no output device is available".into()
    } else {
        failures.join("; ")
    }))
}

/// Explicit, local-only diagnostics. This opens and closes the selected real
/// output route; callers on an async runtime should use `spawn_blocking`.
pub fn doctor(device: &str) -> Result<serde_json::Value, AudioError> {
    // Exercise the linked PCM/container decoder without network, media files or
    // audible test tones. The output probe below still opens the real device.
    const WAV: &[u8] = b"RIFF\x28\x00\x00\x00WAVEfmt \x10\x00\x00\x00\x01\x00\x01\x00\x40\x1f\x00\x00\x80\x3e\x00\x00\x02\x00\x10\x00data\x04\x00\x00\x00\x00\x00\x00\x00";
    let mut decoder = TrackDecoder::open(
        Box::new(io::Cursor::new(WAV)),
        Some(WAV.len() as u64),
        Arc::new(Shared::new()),
    )?;
    while decoder.decode_packet()? {}
    let (sender, errors) = std_mpsc::sync_channel(4);
    let (output, selected) = open_output(device, sender)?;
    if let Ok(error) = errors.try_recv() {
        return Err(AudioError::Device(error.to_string()));
    }
    let config = output.config();
    Ok(serde_json::json!({
        "backend": "rodio/cpal",
        "decoder": "symphonia",
        "decoder_probe": "wav/pcm verified",
        "device": selected,
        "sample_rate": config.sample_rate().get(),
        "channels": config.channel_count().get(),
        "sample_format": format!("{:?}", config.sample_format()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn ring(
        channels: u16,
        capacity: usize,
    ) -> (
        Producer<PcmSample>,
        RingSource,
        Arc<Shared>,
        Arc<OutputState>,
    ) {
        let (producer, consumer) = RingBuffer::new(capacity);
        let shared = Arc::new(Shared::new());
        let output = Arc::new(OutputState::new());
        let source = RingSource::new(
            consumer,
            shared.clone(),
            output.clone(),
            ChannelCount::new(channels).unwrap(),
            SampleRate::new(1000).unwrap(),
        );
        (producer, source, shared, output)
    }

    fn push(producer: &mut Producer<PcmSample>, epoch: u64, samples: &[f32]) {
        for value in samples {
            producer
                .push(PcmSample {
                    epoch,
                    value: *value,
                })
                .unwrap();
        }
    }

    #[test]
    fn underrun_is_silence_without_advancing_media_clock() {
        let (mut producer, mut source, shared, _) = ring(2, 8);
        for _ in 0..20 {
            assert_eq!(source.next(), Some(0.0));
        }
        assert_eq!(shared.position_ms(), 0);
        push(
            &mut producer,
            shared.epoch.load(Ordering::Acquire),
            &[0.25, -0.25],
        );
        assert_eq!(source.next(), Some(0.25));
        assert_eq!(shared.position_ms(), 0);
        assert_eq!(source.next(), Some(-0.25));
        assert_eq!(shared.position_ms(), 1);
        for _ in 0..20 {
            assert_eq!(source.next(), Some(0.0));
        }
        assert_eq!(shared.position_ms(), 1);
    }

    #[test]
    fn partial_frame_waits_without_channel_misalignment() {
        let (mut producer, mut source, shared, _) = ring(2, 8);
        let epoch = shared.epoch.load(Ordering::Acquire);
        push(&mut producer, epoch, &[0.25]);
        assert_eq!(source.next(), Some(0.0));
        assert_eq!(source.next(), Some(0.0));
        push(&mut producer, epoch, &[-0.25]);
        assert_eq!(source.next(), Some(0.25));
        assert_eq!(source.next(), Some(-0.25));
        assert_eq!(shared.position_ms(), 1);
    }

    #[test]
    fn paused_seek_discards_old_pcm_without_consuming_new_pcm() {
        let (mut producer, mut source, shared, _) = ring(2, 8);
        push(
            &mut producer,
            shared.epoch.load(Ordering::Acquire),
            &[0.1, 0.1, 0.2, 0.2],
        );
        shared.paused.store(true, Ordering::Release);
        let epoch = shared.reposition(3000);
        push(&mut producer, epoch, &[0.3, -0.3]);
        assert_eq!(source.next(), Some(0.0));
        assert_eq!(source.next(), Some(0.0));
        assert_eq!(shared.position_ms(), 3000);
        shared.paused.store(false, Ordering::Release);
        assert_eq!(source.next(), Some(0.3));
        assert_eq!(source.next(), Some(-0.3));
        assert_eq!(shared.position_ms(), 3001);
    }

    #[test]
    fn eof_drains_pcm_then_keeps_device_alive_until_worker_teardown() {
        let (mut producer, mut source, shared, output) = ring(1, 8);
        let epoch = shared.epoch.load(Ordering::Acquire);
        push(&mut producer, epoch, &[0.25, 0.5]);
        output.eof_epoch.store(epoch, Ordering::Release);
        assert_eq!(source.next(), Some(0.25));
        assert_eq!(source.next(), Some(0.5));
        assert_eq!(output.drained_epoch.load(Ordering::Acquire), 0);
        assert_eq!(source.next(), Some(0.0));
        assert_eq!(output.drained_epoch.load(Ordering::Acquire), epoch);
        assert_eq!(shared.position_ms(), 2);
        output.alive.store(false, Ordering::Release);
        assert_eq!(source.next(), None);
    }

    #[test]
    fn stale_eof_cannot_end_a_new_seek_generation() {
        let (_, mut source, shared, output) = ring(1, 8);
        let old = shared.epoch.load(Ordering::Acquire);
        let current = shared.reposition(1000);
        output.eof_epoch.store(old, Ordering::Release);
        assert_eq!(source.next(), Some(0.0));
        assert_ne!(output.drained_epoch.load(Ordering::Acquire), current);
        assert_eq!(shared.position_ms(), 1000);
    }

    fn wav(actual_frames: u32, declared_frames: u32) -> Vec<u8> {
        let data_bytes = declared_frames * 2;
        let mut data = Vec::with_capacity(44 + actual_frames as usize * 2);
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        data.extend_from_slice(b"WAVEfmt ");
        data.extend_from_slice(&16_u32.to_le_bytes());
        data.extend_from_slice(&1_u16.to_le_bytes());
        data.extend_from_slice(&1_u16.to_le_bytes());
        data.extend_from_slice(&8000_u32.to_le_bytes());
        data.extend_from_slice(&16000_u32.to_le_bytes());
        data.extend_from_slice(&2_u16.to_le_bytes());
        data.extend_from_slice(&16_u16.to_le_bytes());
        data.extend_from_slice(b"data");
        data.extend_from_slice(&data_bytes.to_le_bytes());
        for _ in 0..actual_frames {
            data.extend_from_slice(&1000_i16.to_le_bytes());
        }
        data
    }

    #[test]
    fn ordinary_wav_eof_and_seek_to_duration_are_supported() {
        let bytes = wav(800, 800);
        let mut track = TrackDecoder::open(
            Box::new(Cursor::new(bytes.clone())),
            Some(bytes.len() as u64),
            Arc::new(Shared::new()),
        )
        .unwrap();
        assert_eq!(track.duration_ms, 100);
        while track.decode_packet().unwrap() {}
        assert_eq!(track.seek(1000).unwrap(), 100);
        assert!(!track.decode_packet().unwrap());
        assert_eq!(track.seek(0).unwrap(), 0);
        assert!(track.decode_packet().unwrap());
    }

    #[test]
    fn truncated_wav_is_not_successful_eof() {
        let bytes = wav(400, 800);
        let opened = TrackDecoder::open(
            Box::new(Cursor::new(bytes.clone())),
            Some(bytes.len() as u64),
            Arc::new(Shared::new()),
        );
        if let Ok(mut track) = opened {
            loop {
                match track.decode_packet() {
                    Ok(true) => {}
                    Ok(false) => panic!("truncated WAV was accepted as complete"),
                    Err(_) => break,
                }
            }
        }
    }

    #[test]
    fn declared_byte_length_and_read_cancellation_are_enforced() {
        let shared = Arc::new(Shared::new());
        let mut input = InputSource {
            input: Box::new(Cursor::new(vec![1_u8, 2])),
            byte_len: Some(3),
            shared: shared.clone(),
            evidence: Arc::new(Mutex::new(ReadEvidence::default())),
        };
        let mut buffer = [0_u8; 8];
        assert_eq!(input.read(&mut buffer).unwrap(), 2);
        assert_eq!(
            input.read(&mut buffer).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        shared.cancelled.store(true, Ordering::Release);
        assert_eq!(
            input.read(&mut buffer).unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
    }

    #[tokio::test]
    async fn failed_seek_broadcasts_terminal_error_before_returning_command_error() {
        // No load is needed: NoSource follows the same terminal seek-error
        // branch as a demuxer seek failure, without requiring an audio device.
        let engine = AudioEngine::spawn(AudioOptions {
            device: String::new(),
            pcm_buffer_ms: 100,
        })
        .await
        .unwrap();
        let mut events = engine.subscribe();
        let error = engine.seek_ms(100).await.unwrap_err();
        assert!(matches!(&error, AudioError::NoSource));
        match events
            .try_recv()
            .expect("seek error must be published before its reply")
        {
            AudioEvent::Error { message, retryable } => {
                assert_eq!(message, error.user_message());
                assert!(!retryable);
            }
            event => panic!("expected terminal seek error, got {event:?}"),
        }
        engine.shutdown().await.unwrap();
    }
}

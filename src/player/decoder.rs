//! Bounded AAC decoding, including YouTube's combined video/audio MP4 fallback.
//!
//! Rodio's MP4 adapter filters video packets only during initialization and
//! derives audio duration from the default (possibly video) track. Its 64 KiB
//! backtracking cache also cannot follow some interleaved MP4 packet layouts.
//! Keep the stream non-seekable, select AAC explicitly, and use a fixed cache.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rodio::{ChannelCount, SampleRate, Source};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_AAC, Decoder, DecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, Track};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use super::backend::AudioStream;

const PACKET_CACHE_BYTES: usize = 2 * 1024 * 1024;
const MAX_BAD_PACKETS: usize = 128;

struct StreamingReader(AudioStream);

impl Read for StreamingReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.0.read(bytes)
    }
}

impl Seek for StreamingReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        self.0.seek(to)
    }
}

impl MediaSource for StreamingReader {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

pub struct AacDecoder {
    format: Box<dyn FormatReader>,
    codec: Box<dyn Decoder>,
    track_id: u32,
    total: Option<Duration>,
    channels: ChannelCount,
    sample_rate: SampleRate,
    samples: Vec<f32>,
    next_sample: usize,
    ended: bool,
}

fn audio_track(tracks: &[Track]) -> Option<&Track> {
    tracks
        .iter()
        .find(|track| track.codec_params.codec == CODEC_TYPE_AAC)
}

fn audio_duration(track: &Track) -> Option<Duration> {
    track
        .codec_params
        .time_base
        .zip(track.codec_params.n_frames)
        .map(|(base, frames)| Duration::from(base.calc_time(frames)))
        .filter(|duration| !duration.is_zero())
}

impl AacDecoder {
    pub fn open(stream: AudioStream) -> Result<Self> {
        let source = MediaSourceStream::new(
            Box::new(StreamingReader(stream)),
            MediaSourceStreamOptions {
                buffer_len: PACKET_CACHE_BYTES,
            },
        );
        let mut hint = Hint::new();
        hint.with_extension("m4a");
        let format = symphonia::default::get_probe()
            .format(
                &hint,
                source,
                &FormatOptions {
                    enable_gapless: true,
                    ..Default::default()
                },
                &MetadataOptions::default(),
            )?
            .format;
        let track =
            audio_track(format.tracks()).context("MP4 contains no supported AAC audio track")?;
        let codec = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())?;
        let mut decoder = Self {
            track_id: track.id,
            total: audio_duration(track),
            channels: ChannelCount::new(2).unwrap(),
            sample_rate: SampleRate::new(44100).unwrap(),
            format,
            codec,
            samples: Vec::new(),
            next_sample: 0,
            ended: false,
        };
        if !decoder.fill_packet()? {
            bail!("AAC stream produced no audio");
        }
        Ok(decoder)
    }

    fn fill_packet(&mut self) -> Result<bool> {
        let mut bad_packets = 0;
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(Error::IoError(error)) if error.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(false);
                }
                Err(error) => return Err(error.into()),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let audio = match self.codec.decode(&packet) {
                Ok(audio) => audio,
                Err(Error::DecodeError(_)) if bad_packets < MAX_BAD_PACKETS => {
                    bad_packets += 1;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            if audio.frames() == 0 {
                continue;
            }
            self.channels = ChannelCount::new(audio.spec().channels.count().try_into()?)
                .context("AAC stream has no channels")?;
            self.sample_rate =
                SampleRate::new(audio.spec().rate).context("AAC stream has no sample rate")?;
            let mut buffer = SampleBuffer::<f32>::new(audio.capacity() as u64, *audio.spec());
            buffer.copy_interleaved_ref(audio);
            self.samples.clear();
            self.samples.extend_from_slice(buffer.samples());
            self.next_sample = 0;
            return Ok(true);
        }
    }
}

impl Iterator for AacDecoder {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.ended {
            return None;
        }
        if self.next_sample >= self.samples.len() {
            match self.fill_packet() {
                Ok(true) => {}
                result => {
                    if let Err(error) = result {
                        crate::diagnostics::warn(
                            "decoder",
                            &format!("AAC decoding stopped: {error:#}"),
                        );
                    }
                    self.ended = true;
                    return None;
                }
            }
        }
        let sample = self.samples[self.next_sample];
        self.next_sample += 1;
        Some(sample)
    }
}

impl Source for AacDecoder {
    fn current_span_len(&self) -> Option<usize> {
        Some(self.samples.len().saturating_sub(self.next_sample))
    }
    fn channels(&self) -> ChannelCount {
        self.channels
    }
    fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }
    fn total_duration(&self) -> Option<Duration> {
        self.total
    }

    fn try_seek(&mut self, to: Duration) -> Result<(), rodio::source::SeekError> {
        let to = self.total.map_or(to, |total| to.min(total));
        let active_channel = self.next_sample % self.channels.get() as usize;
        let seeked = self
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: to.into(),
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|error| rodio::source::SeekError::Other(Arc::new(error)))?;
        self.codec.reset();
        self.samples.clear();
        self.next_sample = 0;
        self.ended = false;
        if let Some(base) = self.codec.codec_params().time_base {
            let delta =
                Duration::from(base.calc_time(seeked.required_ts.saturating_sub(seeked.actual_ts)));
            let frames = (delta.as_secs_f64() * self.sample_rate.get() as f64).ceil() as usize;
            for _ in 0..frames * self.channels.get() as usize + active_channel {
                if self.next().is_none() {
                    break;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia::core::codecs::{CODEC_TYPE_NULL, CodecParameters};
    use symphonia::core::units::TimeBase;

    #[test]
    fn mixed_mp4_uses_audio_timing_and_ignores_the_default_video_track() {
        let mut video = CodecParameters::new();
        video.codec = CODEC_TYPE_NULL;
        video.time_base = Some(TimeBase::new(1, 90000));
        video.n_frames = Some(9000000);
        let mut audio = CodecParameters::new();
        audio.codec = CODEC_TYPE_AAC;
        audio.time_base = Some(TimeBase::new(1, 44100));
        audio.n_frames = Some(8290800);
        let tracks = [Track::new(0, video), Track::new(1, audio)];
        let selected = audio_track(&tracks).unwrap();
        assert_eq!(selected.id, 1);
        assert_eq!(audio_duration(selected), Some(Duration::from_secs(188)));
    }

    #[test]
    fn a_video_only_container_cannot_start_audio_playback() {
        let tracks = [Track::new(0, CodecParameters::new())];
        assert!(audio_track(&tracks).is_none());
    }
}

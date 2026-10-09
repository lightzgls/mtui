//! Distinct PCM packets make an incorrect resume audible in the assertions.

use super::*;
use std::borrow::Cow;
use std::io::Cursor;
use symphonia::core::audio::{AudioBuffer, AudioBufferRef, Channels, Signal, SignalSpec};
use symphonia::core::codecs::{CodecDescriptor, CodecParameters, FinalizeResult};
use symphonia::core::errors::Result as DecodeResult;
use symphonia::core::formats::{Cue, Packet, SeekedTo};
use symphonia::core::meta::{Metadata, MetadataLog};
use symphonia::core::units::TimeBase;

struct Packets {
    next: u64,
    tracks: [Track; 1],
    metadata: MetadataLog,
}

impl FormatReader for Packets {
    fn try_new(_: MediaSourceStream, _: &FormatOptions) -> DecodeResult<Self> {
        unreachable!("the test constructs its packet reader directly")
    }
    fn cues(&self) -> &[Cue] {
        &[]
    }
    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }
    fn tracks(&self) -> &[Track] {
        &self.tracks
    }
    fn next_packet(&mut self) -> DecodeResult<Packet> {
        if self.next >= 4 {
            return Err(Error::IoError(io::ErrorKind::UnexpectedEof.into()));
        }
        let packet = Packet::new_from_slice(0, self.next * 4, 4, &[]);
        self.next += 1;
        Ok(packet)
    }
    fn seek(&mut self, _: SeekMode, to: SeekTo) -> DecodeResult<SeekedTo> {
        let SeekTo::Time { time, .. } = to else {
            unreachable!()
        };
        let required_ts = (time.seconds as f64 * 4.0 + time.frac * 4.0) as u64;
        self.next = required_ts / 4;
        Ok(SeekedTo {
            track_id: 0,
            required_ts,
            actual_ts: self.next * 4,
        })
    }
    fn into_inner(self: Box<Self>) -> MediaSourceStream {
        MediaSourceStream::new(Box::new(Cursor::new(Vec::<u8>::new())), Default::default())
    }
}

struct Pcm {
    params: CodecParameters,
    audio: AudioBuffer<f32>,
}

impl Decoder for Pcm {
    fn try_new(params: &CodecParameters, _: &DecoderOptions) -> DecodeResult<Self> {
        Ok(Self {
            params: params.clone(),
            audio: AudioBuffer::new(4, SignalSpec::new(4, Channels::FRONT_LEFT)),
        })
    }
    fn supported_codecs() -> &'static [CodecDescriptor] {
        &[]
    }
    fn reset(&mut self) {
        self.audio.clear();
    }
    fn codec_params(&self) -> &CodecParameters {
        &self.params
    }
    fn decode(&mut self, packet: &Packet) -> DecodeResult<AudioBufferRef<'_>> {
        self.audio.clear();
        self.audio.render_reserved(None);
        for (index, sample) in self.audio.chan_mut(0).iter_mut().enumerate() {
            *sample = (packet.ts + index as u64) as f32;
        }
        Ok(self.last_decoded())
    }
    fn finalize(&mut self) -> FinalizeResult {
        FinalizeResult::default()
    }
    fn last_decoded(&self) -> AudioBufferRef<'_> {
        AudioBufferRef::F32(Cow::Borrowed(&self.audio))
    }
}

fn decoder() -> AacDecoder {
    let mut params = CodecParameters::new();
    params.codec = CODEC_TYPE_AAC;
    params.time_base = Some(TimeBase::new(1, 4));
    params.n_frames = Some(16);
    let codec = Pcm::try_new(&params, &DecoderOptions::default()).unwrap();
    let mut decoder = AacDecoder {
        format: Box::new(Packets {
            next: 0,
            tracks: [Track::new(0, params)],
            metadata: MetadataLog::default(),
        }),
        codec: Box::new(codec),
        track_id: 0,
        total: Some(Duration::from_secs(4)),
        channels: ChannelCount::new(1).unwrap(),
        sample_rate: SampleRate::new(4).unwrap(),
        samples: Vec::new(),
        next_sample: 0,
        ended: false,
    };
    assert!(decoder.fill_packet().unwrap());
    decoder
}

#[test]
fn output_resume_skips_every_packet_before_the_saved_position() {
    let resumed: Vec<_> = decoder().skip_duration(Duration::from_secs(3)).collect();
    assert_eq!(resumed, [12.0, 13.0, 14.0, 15.0]);
}

#[test]
fn packet_boundaries_are_not_advertised_as_the_end_of_audio() {
    let mut source = decoder();
    for expected in 0..16 {
        assert!(
            source.current_span_len().unwrap() > 0,
            "before sample {expected}"
        );
        assert_eq!(source.next(), Some(expected as f32));
    }
    assert_eq!(source.current_span_len(), Some(0));
    assert_eq!(source.next(), None);
}

#[test]
fn a_seek_to_a_packet_boundary_preserves_the_remaining_audio() {
    let mut source = decoder();
    source.try_seek(Duration::from_secs(2)).unwrap();
    assert!(source.current_span_len().unwrap() > 0);
    let resumed: Vec<_> = source.skip_duration(Duration::from_secs(1)).collect();
    assert_eq!(resumed, [12.0, 13.0, 14.0, 15.0]);
}

#[test]
#[ignore = "decodes a local AAC fixture (MTUI_AUDIO_FIXTURE) over loopback; no audible output"]
fn real_aac_resume_matches_the_original_audio_at_eight_seconds() {
    let bytes =
        std::fs::read(std::env::var_os("MTUI_AUDIO_FIXTURE").expect("set MTUI_AUDIO_FIXTURE"))
            .unwrap();
    let mut reply = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bytes.len()
    )
    .into_bytes();
    reply.extend_from_slice(&bytes);
    let (url, server) = crate::source::test_http::serve(vec![
        crate::source::test_http::immediate(&reply),
        crate::source::test_http::immediate(&reply),
    ]);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let open = || {
        AacDecoder::open(
            runtime
                .block_on(super::super::backend::open(&url))
                .unwrap()
                .0,
        )
        .unwrap()
    };
    let original = open();
    let samples_per_second =
        original.sample_rate().get() as usize * original.channels().get() as usize;
    let baseline: Vec<_> = original.take(10 * samples_per_second).collect();
    let resumed: Vec<_> = open()
        .skip_duration(Duration::from_secs(8))
        .take(2048)
        .collect();
    assert_eq!(resumed.len(), 2048);
    assert_eq!(
        resumed,
        baseline[8 * samples_per_second..8 * samples_per_second + 2048]
    );
    assert_eq!(server.join().unwrap().len(), 2);
}

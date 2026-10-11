//! Device routing and automatic following of the operating-system default.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) struct Output {
    pub sink: rodio::MixerDeviceSink,
    pub name: String,
    pub id: Option<String>,
    failed: Arc<AtomicBool>,
}

impl Output {
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }
}

pub(super) fn default_id() -> Option<String> {
    rodio::cpal::default_host()
        .default_output_device()?
        .id()
        .ok()
        .map(|id| id.to_string())
}

/// Record the actual endpoint, rather than treating "default" as a fixed device.
pub(super) fn open(id: Option<&str>) -> Result<Output> {
    let host = rodio::cpal::default_host();
    let device = match id {
        Some(id) => {
            let id = id
                .parse::<rodio::cpal::DeviceId>()
                .with_context(|| format!("invalid audio output id `{id}`"))?;
            host.device_by_id(&id)
                .with_context(|| format!("audio output `{id}` is not connected"))?
        }
        None => host
            .default_output_device()
            .context("no system default audio output is connected")?,
    };
    let actual_id = device.id().ok().map(|id| id.to_string());
    let name = if id.is_none() {
        "System default".to_string()
    } else {
        device
            .description()
            .map(|description| description.name().to_string())
            .unwrap_or_else(|_| "Selected output".to_string())
    };
    let failed = Arc::new(AtomicBool::new(false));
    let failure = failed.clone();
    let mut sink = rodio::DeviceSinkBuilder::from_device(device)
        .context("could not read the audio output configuration")?
        .with_error_callback(move |error| {
            if !failure.swap(true, Ordering::Relaxed) {
                crate::diagnostics::warn(
                    "player",
                    &format!("audio output disconnected or failed: {error}"),
                );
            }
        })
        .open_sink_or_fallback()
        .context("could not open the audio output")?;
    sink.log_on_drop(false);
    Ok(Output {
        sink,
        name,
        id: actual_id,
        failed,
    })
}

/// Commit only after the endpoint and decoder are ready; failed attempts leave
/// the old player intact so a default change can be retried.
pub(super) fn switch(
    runtime: &tokio::runtime::Runtime,
    output: &mut Output,
    player: &mut rodio::Player,
    track: &mut Option<Track>,
    state: PlayState,
    volume: f32,
    id: Option<&str>,
) -> Result<String> {
    let next = open(id)?;
    let next_player = rodio::Player::connect_new(next.sink.mixer());
    next_player.set_volume(volume);

    if matches!(state, PlayState::Playing | PlayState::Paused) {
        let url = track
            .as_ref()
            .context("the playing track has no stream to move")?
            .url
            .clone();
        let (decoder, total, link) = open_stream(runtime, &url)?;
        let from = track
            .as_ref()
            .map(|cur| cur.resume_at(player.get_pos()))
            .unwrap_or_default();
        stage_source(&next_player, decoder, from);
        if let Some(cur) = track.as_mut() {
            cur.link.decline();
            cur.link = link;
            if total.is_some() {
                cur.total = total;
            }
            cur.offset = from;
            cur.position = from;
            cur.seeking_to = None;
        }
        player.stop();
        if state == PlayState::Playing {
            next_player.play();
        }
    } else {
        player.stop();
    }
    *player = next_player;
    let name = next.name.clone();
    *output = next;
    Ok(name)
}

/// Prepare the replacement silently, including when the song was already paused.
fn stage_source<S: Source + Send + 'static>(player: &rodio::Player, source: S, from: Duration) {
    player.pause();
    player.append(source.skip_duration(from));
}

const CHECK: Duration = Duration::from_secs(1);
const RETRY: Duration = Duration::from_secs(3);

pub(super) struct DefaultFollow {
    enabled: bool,
    next_check: Instant,
}

impl DefaultFollow {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            next_check: Instant::now(),
        }
    }

    pub fn select(&mut self, system_default: bool) {
        self.enabled = system_default;
        self.next_check = Instant::now();
    }

    /// Look up defaults only on the audio thread and retry transient unplugging
    /// failures without changing a deliberately selected output.
    pub fn poll<T>(
        &mut self,
        now: Instant,
        active: Option<&str>,
        failed: bool,
        default: impl FnOnce() -> Option<String>,
        install: impl FnOnce() -> Result<T>,
    ) -> Option<Result<T>> {
        if !self.enabled || now < self.next_check {
            return None;
        }
        self.next_check = now + CHECK;
        let default = default()?;
        if !failed && active == Some(default.as_str()) {
            return None;
        }
        let result = install();
        if result.is_err() {
            self.next_check = now.max(Instant::now()) + RETRY;
        }
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unplug_replug_and_same_endpoint_failures_follow_the_default() {
        let start = Instant::now() + Duration::from_secs(1);
        let mut follow = DefaultFollow::new(true);
        let mut installed = "headphones".to_string();
        let unchanged = follow.poll(
            start,
            Some(&installed),
            false,
            || Some("headphones".into()),
            || -> Result<()> { panic!("an unchanged default must not restart the song") },
        );
        assert!(unchanged.is_none());
        let before_check = follow.poll(
            start + CHECK / 2,
            Some(&installed),
            true,
            || panic!("the endpoint must not be polled on every audio tick"),
            || Ok(()),
        );
        assert!(before_check.is_none());

        let speakers = follow
            .poll(
                start + CHECK,
                Some(&installed),
                true,
                || Some("speakers".into()),
                || Ok("speakers".to_string()),
            )
            .unwrap()
            .unwrap();
        installed = speakers;
        assert_eq!(installed, "speakers");
        let missing = follow.poll(
            start + CHECK * 2,
            Some(&installed),
            true,
            || None,
            || -> Result<()> { panic!("wait until Windows advertises an output") },
        );
        assert!(missing.is_none());
        let failed = follow.poll(
            start + CHECK * 3,
            Some(&installed),
            true,
            || Some("headphones".into()),
            || -> Result<String> { anyhow::bail!("endpoint is still initializing") },
        );
        assert!(failed.unwrap().is_err());
        assert_eq!(
            installed, "speakers",
            "a failed replacement must not be committed"
        );
        assert!(
            follow
                .poll(
                    start + CHECK * 4,
                    Some(&installed),
                    true,
                    || panic!("failed switches must back off"),
                    || Ok(())
                )
                .is_none()
        );
        installed = follow
            .poll(
                start + CHECK * 3 + RETRY,
                Some(&installed),
                true,
                || Some("headphones".into()),
                || Ok("headphones".to_string()),
            )
            .unwrap()
            .unwrap();
        assert_eq!(installed, "headphones");
        assert!(
            follow
                .poll(
                    start + CHECK * 4 + RETRY,
                    Some(&installed),
                    true,
                    || Some("headphones".into()),
                    || Ok(())
                )
                .unwrap()
                .is_ok(),
            "a failed stream must reopen even when the endpoint ID stays the same"
        );
    }

    #[test]
    fn explicit_output_selection_stays_pinned_until_default_is_selected() {
        let mut follow = DefaultFollow::new(false);
        let now = Instant::now() + CHECK;
        assert!(
            follow
                .poll(
                    now,
                    Some("headphones"),
                    true,
                    || panic!("explicit outputs must not follow system changes"),
                    || Ok(())
                )
                .is_none()
        );
        follow.select(true);
        assert!(
            follow
                .poll(
                    now,
                    Some("headphones"),
                    false,
                    || Some("speakers".into()),
                    || Ok(())
                )
                .unwrap()
                .is_ok()
        );
        follow.select(false);
        assert!(
            follow
                .poll(
                    now + CHECK,
                    Some("speakers"),
                    true,
                    || panic!("selecting a specific device stops following"),
                    || Ok(())
                )
                .is_none()
        );
    }

    #[test]
    fn replacement_source_keeps_pause_volume_and_song_position() {
        use std::num::NonZero;
        let channels = NonZero::new(1).unwrap();
        let rate = NonZero::new(1000).unwrap();
        let (mixer, mut samples) = rodio::mixer::mixer(channels, rate);
        let player = rodio::Player::connect_new(&mixer);
        player.set_volume(0.5);
        let source = rodio::buffer::SamplesBuffer::new(
            channels,
            rate,
            // Each second has a distinct value, avoiding the mixer's sample
            // prefetch around pause/resume while still proving the seek time.
            (0..10_000)
                .map(|i| (i / 1000) as f32 / 10.0)
                .collect::<Vec<_>>(),
        );
        stage_source(&player, source, Duration::from_secs(3));
        assert!(player.is_paused());
        assert_eq!(player.volume(), 0.5);
        assert!(
            samples.by_ref().take(20).all(|sample| sample == 0.0),
            "a paused migration must emit no audio"
        );
        player.play();
        let resumed = samples
            .by_ref()
            .take(20)
            .find(|sample| *sample > 0.0)
            .unwrap();
        assert!(
            (resumed - 0.15).abs() < 0.00001,
            "resume from the old song position at its existing volume: {resumed}"
        );
    }
}

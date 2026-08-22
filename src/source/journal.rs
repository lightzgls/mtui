//! What this program has actually played, and what that says about the user.
//!
//! The landing page used to be built from liked songs alone, for the reason
//! [`crate::source::home::personal`] gives: YouTube builds its own shelves from
//! watch history, and no API this program can reach exposes one. But there is a
//! history it can reach -- the plays that happened *here*. Nothing was recording
//! them, so the shelves had nothing to rank with and reduced to "your likes, in
//! the order you liked them", which is the same page every launch.
//!
//! This is that recording. An append-only journal of plays, and a scoring pass
//! over it that turns a pile of events into an opinion: what is in rotation,
//! what has gone cold, what gets skipped every time it comes up.
//!
//! Local and private. It is written to the config directory beside `tokens.json`
//! and is never uploaded -- reporting a play *to YouTube* is a separate thing
//! that [`crate::source::stats`] does, over a cookie, only when the user has
//! saved one. Deleting `plays.jsonl` resets the shelves to their cold-start
//! behaviour and costs nothing else.
//!
//! JSON Lines rather than one document: a play is appended with a single
//! `write` of one line, so a session that is killed mid-write loses at most the
//! last record instead of corrupting the file. A line that will not parse is
//! skipped rather than fatal, for the same reason.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Duration;

use super::sapisid::unix_now;
use super::{Track, UNKNOWN_ARTIST};

const JOURNAL_FILE: &str = "plays.jsonl";

/// How much of a track has to be heard before it is a play rather than a
/// glance. Thirty seconds is the scrobbling convention and it is a good one: it
/// is longer than anyone spends deciding they picked the wrong song, and
/// shorter than the shortest track anybody listens to on purpose.
const MIN_PLAY: u64 = 30;

/// ...or this much of it, for tracks too short for [`MIN_PLAY`] to be fair. A
/// 90-second interlude heard to the end is a play.
const MIN_PLAY_FRACTION: f64 = 0.5;

/// Below this, the user heard the opening and moved on. Recorded as evidence
/// *against* the track, which is the signal likes alone can never carry: a
/// like says what someone chose once, a skip says what they keep rejecting.
const SKIP_FRACTION: f64 = 0.2;

/// Days over which a play loses half its weight. Two weeks makes a shelf that
/// tracks what someone is on now without forgetting last month entirely.
const HALF_LIFE_DAYS: f64 = 14.0;

/// Hours a track stays too fresh for "Listen again". Suggesting something back
/// to someone who played it twenty minutes ago is the failure mode that makes a
/// recommendation shelf look broken.
const RECENT_HOURS: u64 = 6;

/// Days without a play after which something liked becomes forgotten. This is
/// what "Forgotten favorites" should have meant all along -- the old shelf was
/// the like list reversed, which on a small library is the same songs as
/// "Listen again" in the other order.
#[cfg(test)]
const FORGOTTEN_DAYS: f64 = 60.0;

/// Weight on how often something is played, against [`LIKED_WEIGHT`] on whether
/// it was liked. Plays outrank likes deliberately: liking is a thing people do
/// once and forget, playing is a thing they keep doing.
const PLAY_WEIGHT: f64 = 1.0;
const LIKED_WEIGHT: f64 = 0.6;

/// Subtracted in proportion to how often this track gets skipped.
const SKIP_WEIGHT: f64 = 1.2;

/// Seconds below which a play is not recorded at all -- not as a play, and not
/// as a skip either. At this length it is far more likely to be someone arrowing
/// through a list than a judgement about the song.
const NOISE: u64 = 5;

/// Records kept. A play is ~150 bytes, so this is well under a megabyte, and at
/// a generous 60 plays a day it is two years of listening.
const MAX_RECORDS: usize = 5_000;

/// Records tolerated on disk before the file is rewritten to [`MAX_RECORDS`].
/// The gap is slack: compacting on every append would rewrite the whole journal
/// once per song.
const COMPACT_AT: usize = 7_500;

/// Strong candidates considered when choosing the seed for community playlist
/// discovery. Bounded so refreshes vary the recommendation without eventually
/// reaching tracks the score has already said are weak fits.
#[cfg(test)]
const COMMUNITY_SEEDS: usize = 8;

/// One finished play.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Play {
    pub id: String,
    pub title: String,
    pub artist: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// When it started, in unix seconds.
    pub at: u64,
    /// Seconds actually heard. Not the track length -- the point of the whole
    /// file is the difference between the two.
    pub listened: u64,
    /// Track length, when it was known. `None` for a livestream, which is why
    /// [`Self::is_skip`] cannot judge one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<u64>,
}

impl Play {
    /// Builds a record from a track and how far it got.
    pub fn new(track: &Track, listened: Duration) -> Self {
        Self {
            id: track.id.clone(),
            title: track.title.clone(),
            artist: track.uploader.clone(),
            album: track.album.clone(),
            at: unix_now(),
            listened: listened.as_secs(),
            duration: track.duration.map(|d| d.as_secs()),
        }
    }

    /// Whether this counts as having listened to the thing.
    pub fn counts(&self) -> bool {
        if self.listened >= MIN_PLAY {
            return true;
        }
        // A livestream has no length to be a fraction of, so the absolute
        // threshold above is the only test it gets.
        self.duration
            .is_some_and(|total| self.listened as f64 >= total as f64 * MIN_PLAY_FRACTION)
    }

    /// Whether the user bailed out early. Never true for a track whose length
    /// we never knew: "gave up" is a fraction, and there is nothing to divide.
    pub fn is_skip(&self) -> bool {
        self.duration
            .is_some_and(|total| (self.listened as f64) < total as f64 * SKIP_FRACTION)
    }
}

/// One track, with everything the journal knows about it folded together.
#[derive(Debug, Clone)]
pub struct Ranked {
    pub track: Track,
    pub score: f64,
    pub plays: u32,
    pub skips: u32,
    /// Unix seconds of the most recent play, or 0 for something only liked.
    pub last: u64,
    pub liked: bool,
    /// Completed plays after each event's own age has been discounted.
    /// Kept separate from the raw count, which is still useful for skip rates.
    weighted_plays: f64,
}

impl Ranked {
    /// Days since this was last played. Infinite for a track that never was,
    /// which is what keeps a like the user never listened to out of
    /// [`Taste::forgotten`] rather than at the top of it.
    #[cfg(test)]
    fn idle_days(&self, now: u64) -> f64 {
        if self.last == 0 {
            return f64::INFINITY;
        }
        now.saturating_sub(self.last) as f64 / 86_400.0
    }
}

/// The user's listening, scored. Built once per landing page from the journal
/// and the like list together, so every shelf ranks against the same numbers.
pub struct Taste {
    /// Best first.
    ranked: Vec<Ranked>,
    /// Played within [`RECENT_HOURS`].
    recent: HashSet<String>,
}

impl Taste {
    /// Whether MTUI has heard enough of any track to call it listening history.
    #[cfg(test)]
    pub fn has_plays(&self) -> bool {
        self.ranked.iter().any(|ranked| ranked.plays > 0)
    }

    /// Strongest tracks actually heard, including recent ones.
    #[cfg(test)]
    pub fn played(&self, depth: usize) -> Vec<&Ranked> {
        self.ranked
            .iter()
            .filter(|ranked| ranked.plays > 0)
            .take(depth)
            .collect()
    }

    /// Least recently heard tracks, for a new history where nothing has crossed
    /// the strict forgotten threshold yet.
    #[cfg(test)]
    pub fn least_recent(&self, depth: usize) -> Vec<&Ranked> {
        let mut played: Vec<&Ranked> = self
            .ranked
            .iter()
            .filter(|ranked| ranked.plays > 0)
            .collect();
        played.sort_by_key(|ranked| ranked.last);
        played.truncate(depth);
        played
    }

    /// Whether there is enough here to build shelves from.
    ///
    /// False on a first run, and the caller falls back to the like list. This
    /// is the honest answer rather than a shelf of one song: a journal fills up
    /// over a few sessions, and until it has, likes are still the better guess.
    pub fn is_informed(&self) -> bool {
        self.ranked.iter().filter(|r| r.plays > 0).count() >= 5
    }

    /// The "Listen again" shelf: best-scoring tracks actually played before,
    /// minus anything played in the last few hours.
    #[cfg(test)]
    pub fn listen_again(&self, depth: usize, rotation: usize) -> Vec<&Ranked> {
        let eligible: Vec<&Ranked> = self
            .ranked
            .iter()
            .filter(|r| r.plays > 0 && !self.recent.contains(&r.track.id))
            .collect();
        if eligible.len() <= depth {
            return eligible;
        }

        // Keep half the shelf recognisably theirs, and rotate the other half
        // through the broader history. A wholly random shelf feels wrong; a
        // wholly ranked one is the same sixteen tracks forever.
        let stable = (depth / 2).max(1).min(eligible.len());
        let rotating = depth.saturating_sub(stable);
        let tail = &eligible[stable..];
        let offset = rotation.saturating_mul(rotating) % tail.len();

        eligible[..stable]
            .iter()
            .copied()
            .chain((0..rotating).map(|index| tail[(offset + index) % tail.len()]))
            .collect()
    }

    /// Loved once, untouched for [`FORGOTTEN_DAYS`]. Ordered by score so this
    /// is the best of what has gone cold rather than merely the oldest.
    #[cfg(test)]
    pub fn forgotten(&self, depth: usize, now: u64) -> Vec<&Ranked> {
        self.ranked
            .iter()
            .filter(|r| r.plays > 0 && r.idle_days(now) >= FORGOTTEN_DAYS)
            .take(depth)
            .collect()
    }

    /// One strong, actually-played track to describe the user's current vibe.
    ///
    /// Unlike radio seeds, this deliberately includes recent listening: the
    /// point is to find a longer playlist around what the user wants now, not to
    /// suggest the same song back to them. Refresh rotates within the strongest
    /// bounded candidates and never chooses a track rejected more often than it
    /// was completed.
    #[cfg(test)]
    pub fn community_seed(&self, rotation: usize) -> Option<&Ranked> {
        let candidates: Vec<&Ranked> = self
            .ranked
            .iter()
            .filter(|ranked| ranked.plays > 0 && ranked.skips <= ranked.plays)
            .take(COMMUNITY_SEEDS)
            .collect();
        (!candidates.is_empty()).then(|| candidates[rotation % candidates.len()])
    }

    /// Seeds for the radio shelves: high-scoring tracks by *different* artists.
    ///
    /// The distinct-artist rule is the whole fix for a page that reads the same
    /// every time. One seed produces one station, and a station is one mood;
    /// four seeds from four artists produce a shelf that looks like a person's
    /// taste rather than like a single song's neighbourhood.
    ///
    /// `rotation` steps the window through the candidates, so asking again
    /// gives different seeds from the same ranking.
    pub fn seeds(&self, count: usize, rotation: usize, excluded: &HashSet<String>) -> Vec<&Ranked> {
        let mut artists = HashSet::new();
        let candidates: Vec<&Ranked> = self
            .ranked
            .iter()
            // A track the user skips is a bad thing to build a station on even
            // when its play count is high -- that combination is exactly what
            // an album track sitting in front of a favourite looks like.
            .filter(|r| {
                r.plays > 0
                    && r.skips <= r.plays
                    && !self.recent.contains(&r.track.id)
                    && !excluded.contains(&r.track.id)
            })
            .filter(|r| {
                let artist = r.track.uploader.to_lowercase();
                artist != UNKNOWN_ARTIST && artists.insert(artist)
            })
            .collect();

        if candidates.is_empty() {
            return Vec::new();
        }
        // Wraps rather than running off the end, so a rotation past the last
        // candidate comes back round instead of returning nothing.
        (0..count.min(candidates.len()))
            .map(|i| candidates[(rotation.saturating_mul(count) + i) % candidates.len()])
            .collect()
    }

    /// Ids to keep off the page, because they were just played.
    #[cfg(test)]
    pub fn recent(&self) -> &HashSet<String> {
        &self.recent
    }
}

/// The play journal, held in memory and appended to on disk.
pub struct Journal {
    /// `None` when the config directory could not be located. The journal then
    /// works for the session and is not persisted -- a home directory we cannot
    /// find is not a reason to stop playing music.
    path: Option<PathBuf>,
    plays: Vec<Play>,
}

impl Journal {
    /// Reads the journal, or starts an empty one.
    ///
    /// Deliberately infallible. This is called at launch, and every way it can
    /// fail -- no config directory, an unreadable file, a corrupt line -- has
    /// the same sensible answer: carry on with what could be read. The cost of
    /// being wrong is a less personal landing page, which is where the program
    /// started.
    pub fn load() -> Self {
        let Ok(path) = crate::config::dir().map(|dir| dir.join(JOURNAL_FILE)) else {
            return Self {
                path: None,
                plays: Vec::new(),
            };
        };

        let mut plays: Vec<Play> = match fs::File::open(&path) {
            Ok(file) => BufReader::new(file)
                .lines()
                .map_while(Result::ok)
                .filter_map(|line| serde_json::from_str(&line).ok())
                .collect(),
            Err(_) => Vec::new(),
        };

        // Oldest first is the order they were written in, so this keeps the
        // newest and is a no-op on a journal that has not overflowed.
        let overflow = plays.len().saturating_sub(MAX_RECORDS);
        if plays.len() > COMPACT_AT {
            plays.drain(..overflow);
            let mut journal = Self {
                path: Some(path),
                plays,
            };
            journal.compact();
            return journal;
        }

        Self {
            path: Some(path),
            plays,
        }
    }

    /// Appends a play, if it was long enough to mean anything.
    ///
    /// Returns whether it was kept, which is what the caller reports. A track
    /// the user skipped past in four seconds is dropped entirely rather than
    /// recorded as a skip: at that length it is far more likely to be someone
    /// arrowing through a list than a judgement about the song.
    pub fn record(&mut self, play: Play) -> bool {
        if play.listened < NOISE {
            return false;
        }

        if let Some(path) = &self.path {
            append(path, &play);
        }
        self.plays.push(play);
        if self.plays.len() > COMPACT_AT {
            let overflow = self.plays.len() - MAX_RECORDS;
            self.plays.drain(..overflow);
            self.compact();
        }
        true
    }

    /// Rewrites the file from what is held in memory.
    ///
    /// Through a temporary path and a rename, on the same argument
    /// [`crate::source::bootstrap`] makes: a compaction interrupted halfway
    /// through would otherwise truncate the user's entire listening history,
    /// which is the one thing here that cannot be re-fetched.
    fn compact(&mut self) {
        let Some(path) = &self.path else {
            return;
        };
        let temp = path.with_extension("part");

        let mut body = String::new();
        for play in &self.plays {
            if let Ok(line) = serde_json::to_string(play) {
                body.push_str(&line);
                body.push('\n');
            }
        }
        if fs::write(&temp, body).is_ok() && fs::rename(&temp, path).is_err() {
            let _ = fs::remove_file(&temp);
        }
    }

    /// Scores everything known, from the journal and the like list together.
    ///
    /// `likes` is expected most-recently-liked first, which is the order the
    /// Data API returns them in -- so a like's position in the slice is its
    /// recency, and a recent like counts for more than an old one. That is the
    /// only thing about a like this program can date.
    pub fn taste(&self, likes: &[Track], now: u64) -> Taste {
        let mut stats: HashMap<String, Ranked> = HashMap::new();

        for play in &self.plays {
            let entry = stats.entry(play.id.clone()).or_insert_with(|| Ranked {
                track: Track {
                    id: play.id.clone(),
                    title: play.title.clone(),
                    uploader: play.artist.clone(),
                    duration: play.duration.map(Duration::from_secs),
                    album: play.album.clone(),
                    artist_ref: None,
                    playlist_item_id: None,
                },
                score: 0.0,
                plays: 0,
                skips: 0,
                last: 0,
                liked: false,
                weighted_plays: 0.0,
            });

            if play.counts() {
                entry.plays += 1;
                entry.weighted_plays += recency(play.at, now);
                // Only a real play advances recency. A skip should not keep a
                // track feeling fresh, or a song someone keeps rejecting would
                // hold its place at the top of "Listen again" by being rejected.
                entry.last = entry.last.max(play.at);
            } else if play.is_skip() {
                entry.skips += 1;
            }
        }

        // Likes fold in afterwards so a liked track the journal has never seen
        // still gets an entry -- that is the whole cold-start path.
        for (index, track) in likes.iter().enumerate() {
            let entry = stats.entry(track.id.clone()).or_insert_with(|| Ranked {
                track: track.clone(),
                score: 0.0,
                plays: 0,
                skips: 0,
                last: 0,
                liked: false,
                weighted_plays: 0.0,
            });
            entry.liked = true;
            // Carried in the score below via `liked`, weighted by how recent
            // the like is: first in the list is worth the full bonus, last is
            // worth almost none.
            entry.score = 1.0 - (index as f64 / likes.len().max(1) as f64);
        }

        let mut ranked: Vec<Ranked> = stats.into_values().collect();
        for entry in &mut ranked {
            entry.score = score(entry);
        }
        // Total order: `f64` is only partially ordered, and a NaN slipping in
        // would make this panic rather than merely sort oddly.
        ranked.sort_by(|a, b| b.score.total_cmp(&a.score));

        let cutoff = now.saturating_sub(RECENT_HOURS * 3_600);
        let recent = self
            .plays
            .iter()
            .filter(|play| play.counts() && play.at >= cutoff)
            .map(|play| play.id.clone())
            .collect();

        Taste { ranked, recent }
    }
}

/// Appends one line to the journal file.
///
/// Errors are swallowed on purpose: a journal that cannot be written is a worse
/// landing page next launch, not a failed play, and there is nothing the user
/// could usefully be told mid-song.
fn append(path: &std::path::Path, play: &Play) {
    let Ok(mut line) = serde_json::to_string(play) else {
        return;
    };
    line.push('\n');

    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Records one play with no [`Journal`] in hand.
///
/// For the single caller that has none: the UI thread on the way out. The
/// journal lives on the library worker, and that thread is deliberately not
/// joined at exit -- so a play handed to it as the process tears down may never
/// be written, and the last track of a session is exactly the one worth
/// keeping. This writes it directly instead.
///
/// Safe alongside the worker's own copy because it does what the ordinary path
/// does: append one line. It deliberately does *not* compact, which is the only
/// operation that rewrites the file and the only one two writers could tear.
///
/// No upstream report goes with it. That is two round trips, and holding a quit
/// open for them would trade something the user asked for against something
/// they did not.
pub fn record_final(track: &Track, listened: Duration) {
    let play = Play::new(track, listened);
    if play.listened < NOISE {
        return;
    }
    let Ok(path) = crate::config::dir().map(|dir| dir.join(JOURNAL_FILE)) else {
        return;
    };
    append(&path, &play);
}

/// What a track is worth on the landing page.
///
/// Three terms, and the shape of each is deliberate:
///
/// - Plays are logarithmic, so the twentieth play of a song counts for less
///   than the second. Otherwise one track on repeat owns every shelf.
/// - Recency decays by [`HALF_LIFE_DAYS`], applied to the play term only. A
///   like keeps its value; playing something is what expires.
/// - Skips subtract in proportion to the *rate*, not the count, so a track
///   played fifty times and skipped twice is barely touched while one skipped
///   two times out of three is buried.
fn score(entry: &Ranked) -> f64 {
    let plays = PLAY_WEIGHT * (1.0 + entry.weighted_plays).ln();
    // `entry.score` holds the like recency written in `taste`, 0.0 when unliked.
    let liked = if entry.liked {
        LIKED_WEIGHT * entry.score
    } else {
        0.0
    };

    let attempts = entry.plays + entry.skips;
    let skips = if attempts == 0 {
        0.0
    } else {
        SKIP_WEIGHT * (entry.skips as f64 / attempts as f64)
    };

    plays + liked - skips
}

fn recency(at: u64, now: u64) -> f64 {
    let age_days = now.saturating_sub(at) as f64 / 86_400.0;
    0.5f64.powf(age_days / HALF_LIFE_DAYS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;

    fn track(id: &str, artist: &str) -> Track {
        Track {
            id: id.to_string(),
            title: format!("song {id}"),
            uploader: artist.to_string(),
            duration: Some(Duration::from_secs(200)),
            album: None,
            artist_ref: None,
            playlist_item_id: None,
        }
    }

    fn play(id: &str, artist: &str, listened: u64, at: u64) -> Play {
        Play {
            id: id.to_string(),
            title: format!("song {id}"),
            artist: artist.to_string(),
            album: None,
            at,
            listened,
            duration: Some(200),
        }
    }

    /// A journal with no file behind it, so the tests never touch the disk.
    fn journal(plays: Vec<Play>) -> Journal {
        Journal { path: None, plays }
    }

    #[test]
    fn a_play_is_thirty_seconds_or_half_the_track() {
        assert!(play("a", "x", 30, 0).counts());
        assert!(!play("a", "x", 29, 0).counts());

        // Short enough that the fraction is the fairer test.
        let mut short = play("a", "x", 25, 0);
        short.duration = Some(40);
        assert!(short.counts(), "25s of a 40s track is listening to it");
    }

    #[test]
    fn a_livestream_is_judged_only_on_absolute_time() {
        // No length to be a fraction of, so it can never be a skip and needs
        // the full thirty seconds to be a play.
        let mut live = play("a", "x", 60, 0);
        live.duration = None;
        assert!(live.counts());
        assert!(!live.is_skip());

        live.listened = 10;
        assert!(!live.counts());
        assert!(!live.is_skip());
    }

    #[test]
    fn a_skip_is_recorded_against_the_track() {
        let now = 100 * DAY;
        // Same play count, but one of them gets abandoned two times in three.
        let journal = journal(vec![
            play("keeper", "A", 180, now - DAY),
            play("keeper", "A", 180, now - 2 * DAY),
            play("rejected", "B", 180, now - DAY),
            play("rejected", "B", 5, now - DAY),
            play("rejected", "B", 5, now - DAY),
        ]);

        let taste = journal.taste(&[], now);
        let best = taste.listen_again(10, 0);
        assert_eq!(best[0].track.id, "keeper");
        assert_eq!(best[0].skips, 0);
        // Recorded, and it cost the track its place.
        assert_eq!(
            taste
                .ranked
                .iter()
                .find(|r| r.track.id == "rejected")
                .unwrap()
                .skips,
            2
        );
    }

    #[test]
    fn recent_plays_are_held_back_from_listen_again() {
        let now = 100 * DAY;
        let journal = journal(vec![
            play("fresh", "A", 180, now - 600),
            play("older", "B", 180, now - 5 * DAY),
        ]);

        let taste = journal.taste(&[], now);
        let ids: Vec<&str> = taste
            .listen_again(10, 0)
            .iter()
            .map(|r| r.track.id.as_str())
            .collect();

        // Played ten minutes ago: suggesting it back is what makes a shelf
        // look broken, even though it scores highest.
        assert!(
            !ids.contains(&"fresh"),
            "just-played track was suggested back"
        );
        assert_eq!(ids, vec!["older"]);
        assert!(taste.recent().contains("fresh"));
    }

    #[test]
    fn a_recent_skip_does_not_hide_a_track_that_was_really_played() {
        let now = 100 * DAY;
        let taste = journal(vec![
            play("track", "A", 180, now - 10 * DAY),
            play("track", "A", 5, now - 600),
        ])
        .taste(&[], now);

        assert!(!taste.recent().contains("track"));
        assert_eq!(taste.listen_again(1, 0)[0].track.id, "track");
    }

    #[test]
    fn refreshing_keeps_a_core_and_rotates_the_rest_of_listen_again() {
        let now = 100 * DAY;
        let plays: Vec<Play> = (0..20)
            .map(|index| {
                play(
                    &format!("track-{index}"),
                    &format!("artist-{index}"),
                    180,
                    now - (index as u64 + 1) * DAY,
                )
            })
            .collect();
        let taste = journal(plays).taste(&[], now);
        let first: Vec<&str> = taste
            .listen_again(12, 0)
            .into_iter()
            .map(|ranked| ranked.track.id.as_str())
            .collect();
        let refreshed: Vec<&str> = taste
            .listen_again(12, 1)
            .into_iter()
            .map(|ranked| ranked.track.id.as_str())
            .collect();

        assert_eq!(&first[..6], &refreshed[..6], "the familiar core moved");
        assert_ne!(
            &first[6..],
            &refreshed[6..],
            "the rotating half did not move"
        );
    }

    #[test]
    fn recency_decays_against_raw_play_count() {
        let now = 200 * DAY;
        let mut plays = Vec::new();
        // Played into the ground, but months ago.
        for i in 0..8 {
            plays.push(play("stale", "A", 180, now - (120 + i) * DAY));
        }
        // Played half as often, this week.
        for i in 0..4 {
            plays.push(play("current", "B", 180, now - (1 + i) * DAY));
        }

        let taste = journal(plays).taste(&[], now);
        assert_eq!(
            taste.listen_again(1, 0)[0].track.id,
            "current",
            "an old obsession outranked what is in rotation now"
        );
    }

    #[test]
    fn seeds_come_from_different_artists() {
        let now = 100 * DAY;
        let mut plays = Vec::new();
        // One artist dominating the play counts entirely.
        for i in 0..10 {
            plays.push(play(&format!("a{i}"), "Dominant", 180, now - DAY));
        }
        plays.push(play("b", "Second", 180, now - DAY));
        plays.push(play("c", "Third", 180, now - DAY));

        let taste = journal(plays).taste(&[], now);
        let seeds = taste.seeds(3, 0, &HashSet::new());

        let artists: HashSet<&str> = seeds.iter().map(|r| r.track.uploader.as_str()).collect();
        assert_eq!(artists.len(), 3, "one artist took more than one seed");
    }

    #[test]
    fn rotation_moves_the_seeds_along() {
        let now = 100 * DAY;
        let plays: Vec<Play> = (0..4)
            .map(|i| {
                play(
                    &format!("t{i}"),
                    &format!("artist{i}"),
                    180,
                    now - (i + 1) * DAY,
                )
            })
            .collect();

        let taste = journal(plays).taste(&[], now);
        let first = taste.seeds(2, 0, &HashSet::new())[0].track.id.clone();
        let second = taste.seeds(2, 1, &HashSet::new())[0].track.id.clone();
        assert_ne!(first, second, "refreshing gave back the same seed");

        // Past the end it wraps rather than coming back empty.
        assert_eq!(taste.seeds(2, 9, &HashSet::new()).len(), 2);
    }

    #[test]
    fn likes_that_were_never_played_cannot_seed_a_station() {
        let now = 100 * DAY;
        let likes = vec![track("unknown-like", "Unknown"), track("known", "Known")];
        let taste = journal(vec![play("known", "Known", 180, now - DAY)]).taste(&likes, now);
        let seeds = taste.seeds(4, 0, &HashSet::new());

        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].track.id, "known");
    }

    #[test]
    fn seeds_exclude_tracks_already_shown_above() {
        let now = 100 * DAY;
        let plays = vec![
            play("shown", "A", 180, now - DAY),
            play("available", "B", 180, now - 2 * DAY),
        ];
        let taste = journal(plays).taste(&[], now);
        let excluded = HashSet::from(["shown".to_string()]);

        assert_eq!(taste.seeds(1, 0, &excluded)[0].track.id, "available");
    }

    #[test]
    fn community_playlists_can_follow_what_was_just_played() {
        let now = 100 * DAY;
        let taste = journal(vec![
            play("fresh", "A", 180, now - 60),
            play("older", "B", 180, now - 4 * DAY),
        ])
        .taste(&[], now);

        assert_eq!(taste.community_seed(0).unwrap().track.id, "fresh");
        assert!(
            !taste
                .seeds(2, 0, &HashSet::new())
                .iter()
                .any(|seed| seed.track.id == "fresh")
        );
    }

    #[test]
    fn community_playlist_seeds_rotate_and_reject_skipped_tracks() {
        let now = 100 * DAY;
        let mut plays = vec![
            play("first", "A", 180, now - DAY),
            play("second", "B", 180, now - 2 * DAY),
            play("rejected", "C", 180, now - DAY),
            play("rejected", "C", 5, now - DAY),
            play("rejected", "C", 5, now - DAY),
        ];
        // A like without a completed play must not become the vibe seed.
        let likes = [track("liked", "D")];
        let taste = journal(std::mem::take(&mut plays)).taste(&likes, now);

        assert_eq!(taste.community_seed(0).unwrap().track.id, "first");
        assert_eq!(taste.community_seed(1).unwrap().track.id, "second");
        assert_eq!(taste.community_seed(2).unwrap().track.id, "first");
    }

    #[test]
    fn forgotten_means_loved_once_and_untouched() {
        let now = 200 * DAY;
        let journal = journal(vec![
            play("old", "A", 180, now - 90 * DAY),
            play("old", "A", 180, now - 91 * DAY),
            play("current", "B", 180, now - DAY),
        ]);

        let forgotten = journal.taste(&[], now);
        let ids: Vec<&str> = forgotten
            .forgotten(10, now)
            .iter()
            .map(|r| r.track.id.as_str())
            .collect();
        assert_eq!(ids, vec!["old"]);
    }

    #[test]
    fn a_like_never_played_is_not_forgotten() {
        // It was never had, so it cannot have been lost -- and the old shelf,
        // which was just the like list reversed, would have led with it.
        let now = 200 * DAY;
        let taste = journal(Vec::new()).taste(&[track("liked", "A")], now);
        assert!(taste.forgotten(10, now).is_empty());
    }

    #[test]
    fn likes_carry_the_page_until_the_journal_has_something_to_say() {
        let now = 100 * DAY;
        let likes: Vec<Track> = (0..6).map(|i| track(&format!("l{i}"), "A")).collect();

        // Cold start: nothing played, so the caller is told to fall back.
        let cold = journal(Vec::new()).taste(&likes, now);
        assert!(!cold.is_informed());
        assert!(
            cold.listen_again(10, 0).is_empty(),
            "nothing has been played"
        );

        // Enough plays and it takes over.
        let plays: Vec<Play> = (0..5)
            .map(|i| play(&format!("p{i}"), "B", 180, now - (i + 1) * DAY))
            .collect();
        assert!(journal(plays).taste(&likes, now).is_informed());
    }

    #[test]
    fn a_recent_like_outranks_an_old_one() {
        // The Data API returns likes newest first, which is the only thing that
        // dates them, so position in the slice has to mean something.
        let now = 100 * DAY;
        let likes = vec![track("new", "A"), track("mid", "B"), track("old", "C")];
        let taste = journal(Vec::new()).taste(&likes, now);
        assert_eq!(taste.ranked[0].track.id, "new");
        assert_eq!(taste.ranked[2].track.id, "old");
    }

    #[test]
    fn a_glance_is_not_recorded_at_all() {
        // Arrowing through a list is not a judgement about the songs.
        let mut journal = journal(Vec::new());
        assert!(!journal.record(play("a", "A", 3, 0)));
        assert!(journal.plays.is_empty());

        assert!(journal.record(play("a", "A", 60, 0)));
        assert_eq!(journal.plays.len(), 1);
    }

    #[test]
    fn the_journal_stays_bounded() {
        let mut journal = journal(Vec::new());
        for i in 0..COMPACT_AT + 100 {
            journal.record(play("a", "A", 60, i as u64));
        }
        assert!(journal.plays.len() <= COMPACT_AT);
        // The newest survived; the oldest were dropped.
        assert_eq!(journal.plays.last().unwrap().at, (COMPACT_AT + 99) as u64);
    }
}

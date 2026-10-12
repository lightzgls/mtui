//! Community-reviewed Japanese romaji from AMLL TTML DB's public API.
//!
//! Only requested when the listener chooses Romaji. Original and romanized
//! lines are paired inside the same TTML file. Its times are used only when
//! the original panel confirms both the words and the recording's cues;
//! otherwise the provider's romaji remains readable as unsynced text.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use roxmltree::{Document, Node, ParsingOptions};
use serde_json::Value;
use unicode_normalization::UnicodeNormalization;

use super::http::Http;
use super::lrclib::Query;
use super::watch::{Lyrics, MAX_TIMED_LINES, TimedLine};

const BASE_URL: &str = "https://api.amll.dev/v1/lyrics";
const MAX_XML_BYTES: usize = 256 * 1024;
const MAX_TEXT_CHARS: usize = 8_000;
const METADATA_NS: &str = "http://www.w3.org/ns/ttml#metadata";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const ITUNES_NS: &str = "http://music.apple.com/lyric-ttml-internal";
const STYLE_NS: &str = "http://www.w3.org/ns/ttml#styling";

pub fn fetch(http: &Http, query: &Query, original: Option<&Lyrics>) -> Result<Lyrics> {
    fetch_from(http, BASE_URL, query, original)
}

fn fetch_from(http: &Http, base: &str, query: &Query, original: Option<&Lyrics>) -> Result<Lyrics> {
    // Some records store dakuten as combining characters; the API's fuzzy
    // search does not normalize those spellings even though they look alike.
    let spellings = [query.title.clone(), query.title.nfkc().collect(), query.title.nfkd().collect()];
    let mut candidates = Vec::new();
    for (index, title) in spellings.iter().enumerate() {
        if spellings[..index].contains(title) { continue; }
        let search = get(http, &format!("{base}/search"), &[
            ("musicName", title.as_str()), ("artistName", query.artist.as_str()), ("pageSize", "8"),
        ])?;
        let items = search["data"]["items"].as_array().context("AMLL returned an invalid search response")?;
        candidates = items.iter().take(8).filter(|item| matches_track(item, query)).take(2).cloned().collect();
        if !candidates.is_empty() { break; }
    }
    // Exact aliases and artist, rather than whichever fuzzy result has romaji.
    // A second candidate can be a newer contribution with the missing readings.
    let mut unsynced = None;
    for candidate in candidates {
        let Some(id) = candidate["id"].as_u64() else { continue };
        let id = id.to_string();
        let record = match get(http, &format!("{base}/get"), &[("id", &id)]) {
            Ok(record) => record,
            Err(_) if unsynced.is_some() => break,
            Err(error) => return Err(error),
        };
        let record = &record["data"];
        if !matches_track(record, query) { continue; }
        let Some(xml) = record["lyrics"].as_str() else { continue };
        let Some(paired) = parse(xml) else { continue };
        // TTML's body duration is the end of its lyrics, not necessarily the
        // audio length. It cannot establish that this is the same recording.
        if query.duration.is_some_and(|duration| paired.last_start.is_some_and(|start| start > duration)) {
            continue;
        }
        let lyrics = paired.into_lyrics(original);
        if !lyrics.timed.is_empty() { return Ok(lyrics); }
        if unsynced.is_none() { unsynced = Some(lyrics); }
    }
    if let Some(lyrics) = unsynced { return Ok(lyrics); }
    bail!("no matching Japanese romaji is available from AMLL")
}

fn get(http: &Http, url: &str, params: &[(&str, &str)]) -> Result<Value> {
    let query = form_urlencoded::Serializer::new(String::new()).extend_pairs(params.iter().copied()).finish();
    let request = http.client().get(format!("{url}?{query}"))
        .header(reqwest::header::USER_AGENT, concat!("mtui/", env!("CARGO_PKG_VERSION"), " (", env!("CARGO_PKG_REPOSITORY"), ")"));
    let (status, body) = http.send(request).context("could not reach the romaji provider")?;
    if status == 429 { bail!("romaji provider is busy; try again later"); }
    if status != 200 { bail!("romaji provider returned HTTP {status}"); }
    let json: Value = serde_json::from_slice(&body).context("romaji provider returned invalid JSON")?;
    if json["status"].as_u64() != Some(200) { bail!("romaji provider could not complete the request"); }
    Ok(json)
}

fn matches_track(item: &Value, query: &Query) -> bool {
    let has_name = |field: &str, expected: &str| {
        let expected = canonical(expected);
        !expected.is_empty() && item[field].as_array().is_some_and(|names|
            names.iter().filter_map(Value::as_str).any(|name| canonical(name) == expected))
    };
    has_name("musicNames", &query.title) && has_name("artistNames", &query.artist)
}

fn canonical(text: &str) -> String {
    text.nfkc().flat_map(char::to_lowercase).filter(|c| c.is_alphanumeric()).collect()
}

struct Paired {
    original: Vec<String>,
    romanized: Vec<String>,
    starts: Option<Vec<Duration>>,
    last_start: Option<Duration>,
}

impl Paired {
    fn into_lyrics(self, original: Option<&Lyrics>) -> Lyrics {
        let synced = original.is_some_and(|original| self.agrees_with(original));
        let text = self.romanized.join("\n");
        let timed = if synced {
            self.romanized.into_iter().zip(self.starts.unwrap_or_default())
                .map(|(text, start)| TimedLine { text, start }).collect()
        } else { Vec::new() };
        Lyrics {
            text, timed,
            source: Some(if synced { "Source: AMLL TTML DB • Romaji" }
                else { "Source: AMLL TTML DB • Romaji (unsynced)" }.into()),
        }
    }

    fn agrees_with(&self, lyrics: &Lyrics) -> bool {
        let Some(starts) = &self.starts else { return false };
        let original: Vec<_> = lyrics.timed.iter().filter(|line| !line.text.trim().is_empty()).collect();
        original.len() >= 2 && original.len() == self.original.len()
            && original.iter().zip(self.original.iter().zip(starts)).all(|(held, (text, start))|
                !canonical(text).is_empty() && canonical(&held.text) == canonical(text)
                    && held.start.abs_diff(*start) <= Duration::from_secs(1))
    }
}

fn parse(xml: &str) -> Option<Paired> {
    if xml.len() > MAX_XML_BYTES { return None; }
    let document = Document::parse_with_options(xml, ParsingOptions { nodes_limit: 65_536, ..Default::default() }).ok()?;
    let root = document.root_element();
    if root.tag_name().name() != "tt" { return None; }
    let body = root.children().find(|node| node.has_tag_name("body"))?;
    let sidecar: HashMap<_, _> = root.descendants().filter(|node| node.has_tag_name("transliteration"))
        .filter(|node| node.attribute((XML_NS, "lang")).is_some_and(|lang| lang.eq_ignore_ascii_case("ja-Latn")))
        .flat_map(|node| node.children().filter(|child| child.has_tag_name("text")))
        .take(MAX_TIMED_LINES).filter_map(|node| Some((node.attribute("for")?, plain_text(node)))).collect();
    let japanese = root.attribute((XML_NS, "lang")).is_some_and(|lang| lang == "ja" || lang.starts_with("ja-"))
        || !sidecar.is_empty()
        || body.descendants().filter(|node| node.has_tag_name("p")).any(|node| has_kana(&foreground_text(node)))
        || body.descendants().any(|node| role(node) == Some("x-roman")
            && node.attribute((XML_NS, "lang")).is_some_and(|lang| lang.eq_ignore_ascii_case("ja-Latn")));
    if !japanese { return None; }

    let mut paired = Paired { original: Vec::new(), romanized: Vec::new(), starts: Some(Vec::new()), last_start: None };
    let mut original_characters = 0;
    let mut roman_characters = 0;
    let mut roman_lines = 0;
    for line in body.descendants().filter(|node| node.has_tag_name("p")) {
        if paired.original.len() >= MAX_TIMED_LINES { return None; }
        let original = foreground_text(line);
        if original.is_empty() { continue; }
        let roman = line.children().find(|node| role(*node) == Some("x-roman")
            && node.attribute((XML_NS, "lang")).is_none_or(|lang| lang.eq_ignore_ascii_case("ja-Latn")))
            .map(plain_text).filter(|text| !text.is_empty())
            .or_else(|| line.attribute((ITUNES_NS, "key")).and_then(|key| sidecar.get(key).cloned()));
        let roman = match roman {
            Some(text) if !has_cjk(&text) && text.chars().any(|c| c.is_ascii_alphabetic()) => { roman_lines += 1; text }
            // English passages keep their spelling. Missing Japanese readings
            // invalidate a candidate instead of silently showing mixed scripts.
            None if !has_cjk(&original) => original.clone(),
            _ => return None,
        };
        original_characters += original.chars().count();
        roman_characters += roman.chars().count();
        if original_characters > MAX_TEXT_CHARS || roman_characters > MAX_TEXT_CHARS { return None; }
        let start = line.attribute("begin").and_then(time);
        let end = line.attribute("end").and_then(time);
        let valid_containers = line.ancestors().filter(Node::is_element).all(|node|
            node.attribute("timeContainer") != Some("seq") && (node == line
                || node.attribute("begin").is_none_or(|begin| time(begin) == Some(Duration::ZERO))));
        if let Some(start) = start {
            paired.last_start = Some(paired.last_start.map_or(start, |previous| previous.max(start)));
        }
        if let Some(starts) = &mut paired.starts {
            if let Some(start) = start.filter(|start| valid_containers && end.is_some_and(|end| end >= *start)
                && starts.last().is_none_or(|previous| previous <= start)) {
                starts.push(start);
            } else { paired.starts = None; }
        }
        paired.original.push(original);
        paired.romanized.push(roman);
    }
    (roman_lines > 0 && !paired.romanized.is_empty()).then_some(paired)
}

fn role<'a>(node: Node<'a, '_>) -> Option<&'a str> { node.attribute((METADATA_NS, "role")) }

fn plain_text(node: Node<'_, '_>) -> String {
    let text: String = node.descendants().filter(Node::is_text).filter_map(|node| node.text()).collect();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn foreground_text(line: Node<'_, '_>) -> String {
    let text: String = line.descendants().filter(Node::is_text).filter(|node| !node.ancestors().take_while(|parent| *parent != line).any(|parent|
        matches!(role(parent), Some("x-roman" | "x-translation" | "x-bg"))
            || matches!(parent.attribute((STYLE_NS, "ruby")), Some("text" | "textContainer"))))
        .filter_map(|node| node.text()).collect();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn has_kana(text: &str) -> bool {
    text.chars().any(|c| matches!(c, '\u{3041}'..='\u{3096}' | '\u{30a1}'..='\u{30fa}' | '\u{ff66}'..='\u{ff9f}'))
}

fn has_cjk(text: &str) -> bool {
    has_kana(text) || text.chars().any(|c| matches!(c, '\u{3400}'..='\u{9fff}' | '\u{f900}'..='\u{faff}' | '\u{20000}'..='\u{3134f}'))
}

fn time(value: &str) -> Option<Duration> {
    let value = value.trim().trim_end_matches('s');
    let mut total = 0.0;
    let parts: Vec<_> = value.split(':').collect();
    if parts.len() > 3 { return None; }
    for (index, part) in parts.iter().enumerate() {
        if !part.chars().all(|c| c.is_ascii_digit() || c == '.') { return None; }
        let number: f64 = part.parse().ok()?;
        if index > 0 && number >= 60.0 { return None; }
        if index + 1 < parts.len() && number.fract() != 0.0 { return None; }
        total = total * 60.0 + number;
    }
    if !total.is_finite() || !(0.0..=3600.0).contains(&total) { return None; }
    Duration::try_from_secs_f64(total).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Invented phrases, never a real song's lyrics.
    const XML: &str = r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:ttm="http://www.w3.org/ns/ttml#metadata"><body dur="00:09.000"><div>
      <p begin="00:01.250" end="00:03.000"><span>ね</span><span>こ</span><span ttm:role="x-translation">cat</span><span ttm:role="x-roman">neko</span><span ttm:role="x-bg"><span>はい</span><span ttm:role="x-roman">hai</span></span></p>
      <p begin="00:04.500" end="00:06.000"><span>そら</span><span ttm:role="x-roman">sora</span></p>
      <p begin="00:07.000" end="00:09.000">Hello &amp; goodbye</p>
    </div></body></tt>"#;

    fn original() -> Lyrics {
        Lyrics { text: "ねこ\nそら\nHello & goodbye".into(), source: None, timed: vec![
            TimedLine { text: "ねこ".into(), start: Duration::from_millis(1250) },
            TimedLine { text: "そら".into(), start: Duration::from_millis(4500) },
            TimedLine { text: "Hello & goodbye".into(), start: Duration::from_secs(7) },
        ] }
    }

    #[test]
    fn paired_romaji_keeps_verified_cues_and_english_without_auxiliary_text() {
        let lyrics = parse(XML).unwrap().into_lyrics(Some(&original()));
        assert_eq!(lyrics.text, "neko\nsora\nHello & goodbye");
        assert_eq!(lyrics.timed[0].start, Duration::from_millis(1250));
        assert_eq!(lyrics.singing(Duration::from_secs(5)), Some(1));
        assert!(!lyrics.source.unwrap().contains("unsynced"));
    }

    #[test]
    fn same_line_count_wrong_words_or_shifted_recordings_remain_unsynced() {
        let mut held = original();
        held.timed[1].text = "うみ".into();
        assert!(parse(XML).unwrap().into_lyrics(Some(&held)).timed.is_empty());
        held = original();
        for line in &mut held.timed { line.start += Duration::from_secs(3); }
        assert!(parse(XML).unwrap().into_lyrics(Some(&held)).timed.is_empty());
        assert!(parse(XML).unwrap().into_lyrics(None).source.unwrap().contains("unsynced"));
    }

    #[test]
    fn missing_readings_and_other_languages_are_not_passed_off_as_japanese_romaji() {
        assert!(parse(&XML.replace("<span ttm:role=\"x-roman\">sora</span>", "")).is_none());
        assert!(parse(r#"<tt xmlns:ttm="http://www.w3.org/ns/ttml#metadata"><body><div><p>天空<span ttm:role="x-roman">tian kong</span></p></div></body></tt>"#).is_none());
        assert!(parse(&XML.replace("<span ttm:role=\"x-roman\">sora", "<span ttm:role=\"x-roman\" xml:lang=\"zh-Latn\">sora")).is_none());
        assert!(parse("<html>not TTML</html>").is_none());
        assert!(parse(&"x".repeat(MAX_XML_BYTES + 1)).is_none());
    }

    #[test]
    fn sidecar_readings_and_ruby_annotations_preserve_the_foreground_words() {
        let xml = r#"<tt xmlns:itunes="http://music.apple.com/lyric-ttml-internal" xmlns:tts="http://www.w3.org/ns/ttml#styling"><head><iTunesMetadata><transliterations><transliteration xml:lang="ja-Latn"><text for="L1">neko</text></transliteration></transliterations></iTunesMetadata></head><body><div><p itunes:key="L1" begin="1s" end="2s"><span tts:ruby="container"><span tts:ruby="base">猫</span><span tts:ruby="text">ねこ</span></span></p></div></body></tt>"#;
        let paired = parse(xml).unwrap();
        assert_eq!(paired.original, ["猫"]);
        assert_eq!(paired.romanized, ["neko"]);
    }

    #[test]
    fn malformed_or_relative_cues_leave_the_romaji_readable_without_sync() {
        for xml in [XML.replace("00:04.500", "invalid"), XML.replace("00:06.000", "00:02.000"), XML.replace("<div>", "<div begin=\"5s\">"), XML.replace("<div>", "<div timeContainer=\"seq\">")] {
            assert!(parse(&xml).unwrap().into_lyrics(Some(&original())).timed.is_empty());
        }
        for invalid in ["NaN", "-1", "1:99", "1.5:00", "1:2:3:4", "1e3", "9999999999"] { assert!(time(invalid).is_none()); }
        assert_eq!(time("00:01:02.125"), Some(Duration::from_millis(62_125)));
    }

    #[test]
    fn metadata_matching_normalizes_unicode_but_keeps_recording_qualifiers() {
        let query = Query::new("Example - Topic", "たぶん", None, None).unwrap();
        let record = serde_json::json!({"musicNames":["たふ\u{3099}ん"], "artistNames":["EXAMPLE"]});
        assert!(matches_track(&record, &query));
        assert!(!matches_track(&record, &Query::new("Other", "たぶん", None, None).unwrap()));
        assert!(!matches_track(&record, &Query::new("Example", "たぶん (Live)", None, None).unwrap()));
    }

    #[test]
    fn public_api_response_is_matched_before_fetching_and_parsing_its_ttml() {
        use super::super::test_http::{immediate, serve};
        let record = serde_json::json!({"id":42, "musicNames":["Example"], "artistNames":["Singer"], "lyrics":XML});
        let search = serde_json::json!({"status":200,"data":{"items":[record.clone()]}}).to_string();
        let detail = serde_json::json!({"status":200,"data":record}).to_string();
        let response = |body: &str| format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let (base, server) = serve(vec![immediate(response(&search).as_bytes()), immediate(response(&detail).as_bytes())]);
        let query = Query::new("Singer", "Example", None, Some(Duration::from_secs(10))).unwrap();
        let lyrics = fetch_from(&Http::new().unwrap(), &base, &query, Some(&original())).unwrap();
        assert_eq!(lyrics.text, "neko\nsora\nHello & goodbye");
        assert_eq!(lyrics.timed.len(), 3);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("musicName=Example&artistName=Singer&pageSize=8"));
        assert!(requests[1].contains("/get?id=42"));
    }

    #[test]
    fn decomposed_japanese_titles_are_retried_and_rate_limits_are_not_retried() {
        use super::super::test_http::{immediate, serve};
        let record = serde_json::json!({"id":42, "musicNames":["たふ\u{3099}ん"], "artistNames":["Singer"], "lyrics":XML});
        let empty = serde_json::json!({"status":200,"data":{"items":[]}}).to_string();
        let search = serde_json::json!({"status":200,"data":{"items":[record.clone()]}}).to_string();
        let detail = serde_json::json!({"status":200,"data":record}).to_string();
        let response = |body: &str| format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let (base, server) = serve(vec![immediate(response(&empty).as_bytes()), immediate(response(&search).as_bytes()), immediate(response(&detail).as_bytes())]);
        let query = Query::new("Singer", "たぶん", None, None).unwrap();
        assert!(fetch_from(&Http::new().unwrap(), &base, &query, None).is_ok());
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].contains("musicName=%E3%81%9F%E3%81%B5%E3%82%99%E3%82%93"));

        let (base, server) = serve(vec![immediate(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")]);
        assert!(fetch_from(&Http::new().unwrap(), &base, &query, None).unwrap_err().to_string().contains("busy"));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    #[ignore = "read-only public API probe; run explicitly with network access"]
    fn live_japanese_romaji_fetch() {
        let title = std::env::var("MTUI_ROMAJI_PROBE_TITLE").unwrap_or_else(|_| "New me".into());
        let query = Query::new("YOASOBI", &title, None, None).unwrap();
        let lyrics = fetch(&Http::new().unwrap(), &query, None).unwrap();
        assert!(!lyrics.text.is_empty());
        assert!(!has_cjk(&lyrics.text));
        assert!(lyrics.timed.is_empty(), "a provider timeline alone does not verify the playing recording");
    }
}

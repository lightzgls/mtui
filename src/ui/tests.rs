use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::artist::ArtistSection;
use super::artist::artist_header_art_width;
use super::artist::artist_header_height;
use super::artist::artist_layouts;
use super::artist::artist_shelf_cursor;
use super::artist::artist_song_columns;
use super::artist::artist_song_line;
use super::artist::render_artist_header;
use super::artwork::ascii_cell;
use super::artwork::ascii_style;
use super::artwork::fit;
use super::artwork::fit_ascii;
use super::artwork::fit_cells;
use super::artwork::plan_image;
use super::artwork::render_cover;
use super::artwork::usable;
use super::cards::CARD_TEXT_ROWS;
use super::cards::Crop;
use super::cards::GALLERY_CARD;
use super::cards::ShelfCursor;
use super::cards::TEXT_CARD;
use super::cards::Tiles;
use super::cards::artist_prefix_width;
use super::cards::card_size;
use super::cards::detail_spans;
use super::cards::heading_line;
use super::cards::render_shelf;
use super::cards::section_shape;
use super::cards::shelf_height;
use super::cards::shelf_layouts;
use super::comments::render_comments;
use super::home::Hero;
use super::home::HomeCursor;
use super::home::plan_home;
use super::home::render_feed;
use super::home::render_hero;
use super::lyrics::lyric_line;
use super::lyrics::render_lyrics;
use super::mouse::MouseMap;
use super::mouse::MouseTarget;
use super::overlays::SIGN_IN_MAX_WIDTH;
use super::overlays::ellipsis;
use super::overlays::render_sign_in;
use super::palette::highlight;
use super::palette::luma;
use super::player::PlayerIdentity;
use super::player::has_player_identity;
use super::player::identity_byline;
use super::player::player_layout;
use super::player::progress_bar_width;
use super::player::progress_line;
use super::player::register_identity_targets;
use super::player::register_player_identity_targets;
use super::player::render_lyrics_controls;
use super::player::render_panel_body;
use super::player::render_player_identity;
use super::player::render_related;
use super::player::render_tabs;
use super::player::render_track_info;
use super::player::track_info_rows;
use super::player::volume_line;
use super::results::ALBUM_WIDTH;
use super::results::ARTIST_WIDTH;
use super::results::DURATION_WIDTH;
use super::results::MIN_TITLE_WIDTH;
use super::results::columns;
use super::results::header_line;
use super::status::HINTS_AWAY;
use super::status::HINTS_BROWSE;
use super::status::HINTS_EDITING;
use super::status::HINTS_PLAYING;
use super::status::HINTS_WIDTH;
use super::status::MIN_WIDTH_WITH_STATUS_VOLUME;
use super::status::MINI_PLAYER_FLOOR;
use super::status::MiniTrack;
use super::status::PlayerStrip;
use super::status::STATUS_VOLUME_WIDTH;
use super::status::TRANSPORT_WIDTH;
use super::status::VOLUME_BAR_WIDTH;
use super::status::mini_player_line;
use super::status::register_footer_artist;
use super::status::register_transport_targets;
use super::status::register_volume_targets;
use super::status::render_player_strip;
use super::status::status_control_areas;
use super::text::MARQUEE_HOLD;
use super::text::bounded_wrap;
use super::text::cell;
use super::text::centred;
use super::text::centred_offset;
use super::text::display_width;
use super::text::marquee_at;
use super::text::truncate;
use super::text::wrap;
use super::*;
use crate::app::CardShape;
use crate::app::CoverSize;
use crate::app::ImageBound;
use crate::app::Mode;
use crate::app::MouseAction;
use crate::app::NowPlaying;
use crate::app::POINTER_SCALE;
use crate::app::Panel;
use crate::app::RepeatMode;
use crate::app::SignIn;
use crate::app::Tab;
use crate::app::View;
use crate::art::ArtCache;
use crate::graphics::Graphics;
use crate::player::PlayState;
use crate::player::Snapshot;
use crate::source::Track;
use crate::source::artist::ArtistPage;
use crate::source::artist::ArtistSong;
use crate::source::cover::Cover;
use crate::source::home::Card;
use crate::source::home::Shelf;
use crate::source::watch::{Comment, Comments, Lyrics, TimedLine};
use crate::source::{ArtistRef, BrowseEndpoint};
use ratatui::buffer::CellDiffOption;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::Block;
use std::time::Duration;
use std::time::Instant;
#[test]
fn artist_and_album_links_match_their_clipped_byline_regions() {
    let mut now = playing();
    now.artist = "Artist".into();
    now.artist_ref = Some(ArtistRef {
        name: "Artist".into(),
        endpoint: BrowseEndpoint::new("UCartist"),
    });
    now.album = Some("Album".into());
    now.album_route = Some(("Album".into(), BrowseEndpoint::new("MPREalbum")));
    let mut mouse = MouseMap::default();
    register_identity_targets(&mut mouse, &now, Rect::new(4, 8, 30, 1));
    assert_eq!(mouse.action_at(5, 8), Some(MouseAction::OpenPlayingArtist));
    assert_eq!(mouse.action_at(14, 8), Some(MouseAction::OpenPlayingAlbum));
    assert_eq!(mouse.action_at(12, 8), None);
    mouse.targets.clear();
    register_identity_targets(&mut mouse, &now, Rect::new(4, 8, 5, 1));
    assert_eq!(mouse.action_at(8, 8), Some(MouseAction::OpenPlayingArtist));
    assert_eq!(mouse.action_at(9, 8), None);
    assert!(
        identity_byline(&now, 30).spans[0]
            .style
            .add_modifier
            .contains(Modifier::UNDERLINED)
    );
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 8)).unwrap();
    let snap = Snapshot {
        state: PlayState::Playing,
        ..Default::default()
    };
    mouse.targets.clear();
    terminal
        .draw(|frame| {
            register_footer_artist(frame, &now, &snap, Rect::new(0, 4, 100, 3), &mut mouse)
        })
        .unwrap();
    let x = 1 + display_width(&now.title) as u16 + 3;
    assert_eq!(mouse.action_at(x, 5), Some(MouseAction::OpenPlayingArtist));
    mouse.targets.clear();
    terminal
        .draw(|frame| {
            register_footer_artist(frame, &now, &snap, Rect::new(0, 4, 20, 3), &mut mouse)
        })
        .unwrap();
    assert_eq!(mouse.action_at(x, 5), None);
}

#[test]
fn queue_artist_targets_require_a_visible_linked_column() {
    let mut track = Track {
        id: "song".into(),
        title: "Song".into(),
        uploader: "Artist".into(),
        duration: Some(Duration::from_secs(180)),
        album: None,
        artist_ref: Some(ArtistRef {
            name: "Artist".into(),
            endpoint: BrowseEndpoint::new("UCartist"),
        }),
    };
    let (x, width) = queue::artist_region(&track, 50).unwrap();
    let row = queue::line(&track, false, false, 50, Color::Cyan);
    let text: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
    assert_eq!(
        text.chars().skip(x).take(width).collect::<String>(),
        "Artist"
    );
    assert!(queue::artist_region(&track, 20).is_none());
    track.artist_ref = None;
    assert!(queue::artist_region(&track, 50).is_none());
}

#[test]
fn inline_artist_names_require_full_visible_names_and_grapheme_boundaries() {
    let route = |name: &str| ArtistRef {
        name: name.into(),
        endpoint: BrowseEndpoint::new("UCartist"),
    };
    for (text, name, room, expected) in [
        ("Artist • Album", "Artist", 20, Some(6)),
        ("Artist & Guest", "Artist", 10, Some(6)),
        ("Artist", "Artist", 6, Some(6)),
        ("Artist", "Artist", 5, None),
        ("Artistical", "Artist", 7, None),
        ("Someone else", "Artist", 30, None),
        ("銀河小魚 • Album", "銀河小魚", 20, Some(8)),
        ("銀河小魚", "銀河小魚", 7, None),
        ("Cafe\u{301} & Guest", "Cafe\u{301}", 20, Some(4)),
        ("Cafe\u{301} & Guest", "Cafe", 20, None),
        ("Artist", " ", 20, None),
    ] {
        assert_eq!(
            artist_prefix_width(text, &route(name), room),
            expected,
            "{text}, {name}, {room}"
        );
    }
}

/// A 16:9 cover, as every YouTube thumbnail is once shrunk.
fn wide() -> Cover {
    Cover::solid(160, 90)
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn artist_song(duration: Option<Duration>) -> ArtistSong {
    ArtistSong {
        track: Track {
            id: "letithappen".to_string(),
            title: "Let It Happen".to_string(),
            uploader: "Tame Impala".to_string(),
            duration,
            album: Some("Currents".to_string()),
            artist_ref: None,
        },
        plays: Some("120M plays".to_string()),
    }
}

fn artist_page() -> ArtistPage {
    ArtistPage {
        artist: ArtistRef {
            name: "Tame Impala".to_string(),
            endpoint: BrowseEndpoint::new("UCGz-artist"),
        },
        audience: Some("30M monthly listeners".to_string()),
        description: Some("Psychedelic music from Kevin Parker.".to_string()),
        art: None,
        top_songs: vec![
            artist_song(None),
            artist_song(Some(Duration::from_secs(468))),
        ],
        songs_endpoint: None,
        songs_truncated: false,
        shelves: vec![Shelf {
            title: "Albums".to_string(),
            cards: vec![album("Currents", "Album • Tame Impala")],
        }],
    }
}

#[test]
fn artist_song_rows_are_bounded_at_every_width() {
    for duration in [Duration::from_secs(468), Duration::from_secs(100 * 60 * 60)] {
        let song = artist_song(Some(duration));
        for width in 0..120 {
            let columns = artist_song_columns(width);
            assert_eq!(
                artist_song_line(&song, true, columns, Color::Cyan).width(),
                width,
                "artist row escaped a {width}-column pane"
            );
        }
    }
}

#[test]
fn artist_previews_do_not_label_missing_durations_as_live() {
    let columns = artist_song_columns(90);
    let text = line_text(&artist_song_line(
        &artist_song(None),
        false,
        columns,
        Color::Cyan,
    ));
    assert!(!text.contains("LIVE"), "{text:?}");
    assert!(text.contains("120M plays"), "{text:?}");
}

#[test]
fn artist_layout_keeps_top_songs_before_catalogue_shelves() {
    let layouts = artist_layouts(
        &artist_page(),
        Rect::new(0, 0, 100, 20),
        0,
        CardShape::Tile,
        false,
    );
    assert_eq!(layouts[0].kind, ArtistSection::Songs);
    assert_eq!(layouts[1].kind, ArtistSection::Shelf(0));
    assert!(layouts[0].area.y < layouts[1].area.y);
}

#[test]
fn compact_artist_headers_keep_identity_and_drop_description_first() {
    let page = artist_page();
    let mut terminal = Terminal::new(TestBackend::new(40, 3)).unwrap();
    terminal
        .draw(|frame| {
            render_artist_header(
                frame,
                &page,
                None,
                Rect::new(0, 0, 40, 3),
                Color::Cyan,
                None,
            )
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text = (0..3)
        .map(|y| (0..40).map(|x| buffer[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Tame Impala"), "{text}");
    assert!(text.contains("30M monthly listeners"), "{text}");
    assert!(!text.contains("Psychedelic"), "{text}");
}

#[test]
fn artist_header_growth_never_takes_space_back_from_content() {
    let mut previous = 0;
    for available in 0..32 {
        let content = available - artist_header_height(available);
        assert!(
            content >= previous,
            "content shrank from {previous} to {content} at height {available}"
        );
        previous = content;
    }
}

#[test]
fn artist_header_art_is_reserved_only_where_it_can_be_drawn() {
    assert_eq!(artist_header_art_width(Rect::new(0, 0, 60, 6), true), 12);
    assert_eq!(artist_header_art_width(Rect::new(0, 0, 60, 6), false), 0);
    assert_eq!(artist_header_art_width(Rect::new(0, 0, 40, 6), true), 0);
    assert_eq!(artist_header_art_width(Rect::new(0, 0, 60, 2), true), 0);
}

#[test]
fn unfocused_artist_shelves_do_not_clamp_the_active_card() {
    let mut selected = 7;
    let mut offset = 5;

    let cursor = artist_shelf_cursor(&mut selected, &mut offset, false, 2, 2);
    assert_eq!(selected, 7);
    assert_eq!(cursor.selected, 1);
    assert_eq!(cursor.offset, 0);

    let cursor = artist_shelf_cursor(&mut selected, &mut offset, true, 3, 10);
    assert_eq!(selected, 7);
    assert_eq!(cursor.selected, 7);
    assert_eq!(cursor.offset, 5);
}

#[test]
fn fit_fills_the_width_of_a_tall_pane() {
    // 32 usable columns, 20 rows: 16:9 runs out of width first.
    assert_eq!(fit(&wide(), 32, 20), (32, 18));
}

#[test]
fn fit_gives_up_width_in_a_short_pane() {
    // 4 rows is 8 pixel rows, so the width has to come down to match.
    assert_eq!(fit(&wide(), 32, 4), (14, 8));
}

#[test]
fn fit_keeps_a_square_cover_square() {
    assert_eq!(fit(&Cover::solid(300, 300), 32, 40), (32, 32));
}

#[test]
fn fit_yields_nothing_for_a_degenerate_pane() {
    // A pane too small to hold anything must draw nothing rather than
    // dividing by a zero dimension.
    assert_eq!(fit(&wide(), 0, 10), (0, 0));
    assert_eq!(fit(&wide(), 10, 0), (0, 0));
}

/// Draws for real and reads the cells back, which is the only way to know
/// the two halves of a `▀` land on the pixels they are supposed to.
#[test]
fn cover_pane_paints_pixel_rows_as_half_blocks() {
    // One column, red over blue: the upper pixel must become a cell's
    // foreground and the lower one its background.
    let cover = Cover::from_rgb(1, 2, vec![255, 0, 0, 0, 0, 255]);
    let mut terminal = Terminal::new(TestBackend::new(12, 8)).unwrap();
    terminal
        .draw(|frame| render_cover(frame, &cover, Rect::new(0, 0, 12, 8), CoverSize::Side))
        .unwrap();
    let buf = terminal.backend().buffer();

    // The image is 6 pixels wide inside a 10-column interior, so it starts
    // two columns in, one row below the top border.
    let top = &buf[(3, 1)];
    assert_eq!(top.symbol(), "▀");
    assert_eq!(top.fg, Color::Rgb(255, 0, 0));
    assert_eq!(top.bg, Color::Rgb(255, 0, 0));

    let bottom = &buf[(3, 6)];
    assert_eq!(bottom.fg, Color::Rgb(0, 0, 255));
    assert_eq!(bottom.bg, Color::Rgb(0, 0, 255));
}

#[test]
fn colored_ascii_uses_brightness_for_glyphs_and_keeps_colour() {
    let black_and_white =
        Cover::from_rgb(1, 4, vec![0, 0, 0, 0, 0, 0, 255, 255, 255, 255, 255, 255]);
    assert_eq!(
        ascii_cell(&black_and_white, 0, 0, 1, 4),
        (' ', Color::Rgb(0, 0, 0))
    );
    assert_eq!(
        ascii_cell(&black_and_white, 0, 1, 1, 4),
        ('@', Color::Rgb(255, 255, 255))
    );

    let red = Cover::from_rgb(1, 2, vec![255, 0, 0, 255, 0, 0]);
    let (glyph, colour) = ascii_cell(&red, 0, 0, 1, 2);
    assert_ne!(glyph, ' ');
    assert_eq!(colour, Color::Rgb(255, 0, 0));
    assert_eq!(ascii_style(colour).bg, Some(Color::Rgb(0, 0, 0)));
}

#[test]
fn colored_ascii_uses_complete_character_rows_without_stretching() {
    let square = Cover::solid(300, 300);
    assert_eq!(fit_ascii(&square, 31, 40), (30, 30));
    assert_eq!(fit_ascii(&square, 1, 1), (0, 0));
    for width in 1..40 {
        assert_eq!(fit_ascii(&square, width, 40).1 % 2, 0);
    }
}

#[test]
fn cover_pane_is_only_as_tall_as_the_picture() {
    // 16:9 across 32 usable columns needs 9 text rows, so the block closes
    // at row 10 rather than running to the bottom of a 20-row pane.
    let cover = Cover::solid(160, 90);
    let mut terminal = Terminal::new(TestBackend::new(34, 20)).unwrap();
    terminal
        .draw(|frame| render_cover(frame, &cover, Rect::new(0, 0, 34, 20), CoverSize::Side))
        .unwrap();
    let buf = terminal.backend().buffer();

    assert_eq!(buf[(0, 0)].symbol(), "┌");
    assert_eq!(buf[(0, 10)].symbol(), "└");
    assert_eq!(buf[(0, 11)].symbol(), " ", "nothing painted past the block");
}

#[test]
fn fit_cells_keeps_the_aspect_ratio_within_half_a_cell() {
    let cover = Cover::solid(160, 90);
    let cell = (10, 20);

    // Width-bound: 42 columns is 420 px, which wants 236 px of height --
    // just under 12 rows, so 12 rows it is rather than a squashed 11.
    assert_eq!(fit_cells(&cover, (42, 30), cell), (42, 12));
    let (w, h) = (42 * 10, 12 * 20);
    let error = (w as f32 / h as f32) / (160.0 / 90.0);
    assert!((0.95..1.05).contains(&error), "aspect off by {error}");
}

#[test]
fn fit_cells_gives_up_columns_when_rows_run_out() {
    let cover = Cover::solid(160, 90);
    assert_eq!(fit_cells(&cover, (42, 8), (10, 20)), (28, 8));
}

#[test]
fn fit_cells_makes_a_square_cover_square() {
    // A cell twice as tall as wide means a square image needs half as many
    // rows as columns.
    assert_eq!(
        fit_cells(&Cover::solid(300, 300), (42, 30), (10, 20)),
        (42, 21)
    );
}

#[test]
fn fit_cells_yields_nothing_for_a_degenerate_pane() {
    let cover = Cover::solid(160, 90);
    assert_eq!(fit_cells(&cover, (0, 10), (10, 20)), (0, 0));
    assert_eq!(fit_cells(&cover, (10, 0), (10, 20)), (0, 0));
}

/// The sixel pane must reserve its cells rather than paint them: the
/// pixels arrive by escape sequence, and a redraw over them would shred
/// the image.
#[test]
fn sixel_pane_reserves_its_cells_and_draws_no_blocks() {
    let cover = Cover::solid(160, 90);
    let graphics = Graphics {
        sixel: true,
        kitty: false,
        cell: (10, 20),
    };
    let mut terminal = Terminal::new(TestBackend::new(34, 20)).unwrap();
    let mut plan = None;
    let mut reserved = 0;
    terminal
        .draw(|frame| {
            plan = plan_image(
                frame,
                &cover,
                Rect::new(0, 0, 34, 20),
                graphics,
                CoverSize::Side,
            );
            // Read back from the frame, not the backend: a skipped cell is
            // by definition one the backend never hears about.
            if let Some(p) = plan {
                let buffer = frame.buffer_mut();
                for y in p.row..p.row + p.height / 20 {
                    for x in p.col..p.col + p.width / 10 {
                        if buffer[(x, y)].diff_option == CellDiffOption::Skip {
                            reserved += 1;
                        }
                    }
                }
            }
        })
        .unwrap();
    let plan = plan.expect("a plan for a pane this size");

    // 32 usable columns at 10 px, and the row count that keeps 16:9.
    assert_eq!((plan.width, plan.height), (320, 180));
    assert_eq!(plan.bound, ImageBound::Columns(32));
    assert_eq!((plan.col, plan.row), (1, 1));
    assert_eq!(reserved, 32 * 9, "every cell under the image is reserved");

    // Nothing but the frame reaches the terminal: no half-blocks, and the
    // block still shrinks to the picture.
    let buf = terminal.backend().buffer();
    assert_eq!(buf[(plan.col, plan.row)].symbol(), " ");
    assert_eq!(buf[(0, 0)].symbol(), "┌");
    assert_eq!(buf[(0, plan.height / 20 + 1)].symbol(), "└");
}

/// A full-window cover is the largest picture the terminal can hold, and
/// the only remaining lever on resolution.
#[test]
fn full_size_cover_fills_the_window_and_centres() {
    // A 110x28 main area on a 10x20 cell: 1080 px of usable width against
    // 520 of usable height, so a square cover is bound by the height.
    let cover = Cover::solid(720, 720);
    let graphics = Graphics {
        sixel: true,
        kitty: false,
        cell: (10, 20),
    };
    let area = Rect::new(0, 0, 110, 28);
    let mut terminal = Terminal::new(TestBackend::new(110, 28)).unwrap();
    let mut plan = None;
    terminal
        .draw(|frame| plan = plan_image(frame, &cover, area, graphics, CoverSize::Full))
        .unwrap();

    let plan = plan.expect("a plan for a window this size");
    // No frame at full size, so all 28 rows are picture: 560 px, and the
    // width follows to keep it square.
    assert_eq!((plan.width, plan.height), (560, 560));
    assert_eq!(plan.bound, ImageBound::Rows(28));
    assert_eq!((plan.col, plan.row), ((110 - 56) / 2, 0), "centred");
}

#[test]
fn full_size_drops_the_frame_that_the_side_pane_keeps() {
    let area = Rect::new(0, 0, 34, 20);
    assert_eq!(usable(area, CoverSize::Full), (34, 20));
    assert_eq!(usable(area, CoverSize::Side), (32, 18));
}

/// Whatever is dropped, the columns must still exactly fill the row --
/// otherwise the selected row's highlight stops short or wraps.
fn assert_fills(width: usize) {
    let (title, artist, album) = columns(width);
    assert_eq!(
        title + artist + album + DURATION_WIDTH + 2,
        width,
        "columns do not fill a {width}-wide row"
    );
}

#[test]
fn a_wide_row_carries_every_column() {
    let (title, artist, album) = columns(120);
    assert_eq!((artist, album), (ARTIST_WIDTH, ALBUM_WIDTH));
    assert!(title >= MIN_TITLE_WIDTH);
    assert_fills(120);
}

#[test]
fn album_goes_first_when_the_row_narrows() {
    // Room for the artist but not both.
    let width = DURATION_WIDTH + 2 + MIN_TITLE_WIDTH + ARTIST_WIDTH;
    let (_, artist, album) = columns(width);
    assert_eq!((artist, album), (ARTIST_WIDTH, 0));
    assert_fills(width);
}

#[test]
fn a_narrow_row_keeps_only_the_title() {
    let (title, artist, album) = columns(40);
    assert_eq!((artist, album), (0, 0));
    assert_eq!(title, 40 - DURATION_WIDTH - 2);
    assert_fills(40);
}

#[test]
fn columns_never_overflow_a_tiny_row() {
    // A pane can be squeezed below the fixed columns entirely, leaving
    // nothing for the title. Saturating arithmetic has to keep that from
    // wrapping into an enormous width.
    for width in 0..=(DURATION_WIDTH + 2) {
        assert_eq!(columns(width), (0, 0, 0), "at width {width}");
    }
}

/// Prints the results grid at several widths, for eyeballing a layout
/// change. It is what caught wide CJK glyphs breaking every column.
///
/// It rebuilds the row by hand rather than calling the renderer, which
/// needs a live `App`; if the row layout changes, this has to change with
/// it or the preview quietly stops resembling the real thing.
///
/// `cargo test preview_layout -- --ignored --nocapture`
#[test]
#[ignore = "prints a layout preview rather than asserting"]
fn preview_layout() {
    let rows = [
        (
            "Harder, Better, Faster, Stronger",
            "Daft Punk",
            "Discovery",
            "3:47",
        ),
        (
            "Get Lucky (feat. Pharrell Williams and Nile Rodgers)",
            "Daft Punk, Pharrell Williams & Nile Rodgers",
            "Random Access Memories",
            "6:10",
        ),
        ("Da Funk", "Daft Punk", "", "5:35"),
        ("日本語のタイトル", "アーティスト", "アルバム", "4:02"),
    ];
    for width in [96usize, 72, 44] {
        let (t, ar, al) = columns(width);
        println!("\n--- {width} cols (title {t}, artist {ar}, album {al}) ---");
        println!(
            "|{}|",
            header_line(t, ar, al)
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        );
        for (title, artist, album, duration) in rows {
            let mut line = format!(" {}", cell(title, t));
            if ar > 0 {
                line += &cell(artist, ar);
            }
            if al > 0 {
                line += &cell(album, al);
            }
            line += &format!("{duration:>DURATION_WIDTH$} ");
            println!("|{line}|");
        }
    }
}

/// `cargo test preview_sign_in -- --ignored --nocapture`
#[test]
#[ignore = "prints a layout preview rather than asserting"]
fn preview_sign_in() {
    let phases = [
        (
            "failed",
            24,
            SignIn::Failed {
                reason: "the saved sign-in is no longer valid".to_string(),
            },
        ),
        (
            "importing",
            24,
            SignIn::Music {
                started: Instant::now(),
                recovering: false,
            },
        ),
    ];
    for (name, height, phase) in phases {
        println!("\n--- {name} ---");
        for row in drawn(72, height, &phase) {
            println!("{}", row.trim_end());
        }
    }
}

use crate::source::home::Target;

pub(super) fn song(title: &str, subtitle: &str) -> Card {
    Card {
        title: title.to_string(),
        subtitle: subtitle.to_string(),
        // Distinct per title, so a shelf of these is a shelf of cards with
        // their own artwork rather than one card drawn five times.
        art: Some(format!("https://example.invalid/{title}")),
        duration: None,
        artist_ref: None,
        target: Target::Play {
            video_id: title.to_string(),
        },
    }
}

fn album(title: &str, subtitle: &str) -> Card {
    Card {
        title: title.to_string(),
        subtitle: subtitle.to_string(),
        art: Some(format!("https://example.invalid/{title}")),
        duration: None,
        artist_ref: None,
        target: Target::Open {
            endpoint: BrowseEndpoint::new(title),
        },
    }
}

/// An empty art cache and somewhere to note what a draw asked for.
///
/// Every card in these tests has an artwork URL and none of it has arrived,
/// which is deliberately the hardest case for the layout: the tiles are
/// drawn as placeholders at exactly the size the real pictures will take,
/// so anything that fits here fits once they land.
fn no_art() -> (ArtCache, Vec<(String, Option<String>)>) {
    (ArtCache::default(), Vec::new())
}

/// A cache holding one flat-coloured sleeve per card of `shelves`, for the
/// tests and previews that need pictures rather than placeholders.
fn stub_art(shelves: &[Shelf]) -> ArtCache {
    let mut cache = ArtCache::default();
    for (n, card) in shelves.iter().flat_map(|shelf| &shelf.cards).enumerate() {
        // A different hue per card, so a preview shows the accents varying
        // the way real sleeves make them vary.
        let (r, g, b) = (
            (n as u8).wrapping_mul(97).saturating_add(40),
            (n as u8).wrapping_mul(53).saturating_add(20),
            (n as u8).wrapping_mul(29).saturating_add(90),
        );
        let rgb = [r, g, b].repeat(32 * 32);
        cache.want(card.art_key());
        cache.store(
            card.art_key().to_string(),
            Some(Cover::from_rgb(32, 32, rgb)),
        );
    }
    cache
}

/// Prints the landing page in each card shape, for eyeballing a layout
/// change.
///
/// `cargo test preview_home -- --ignored --nocapture`
#[test]
#[ignore = "prints a layout preview rather than asserting"]
fn preview_home() {
    let mut shelves = vec![
        quick_picks(),
        Shelf {
            title: "Albums for you".to_string(),
            cards: vec![
                album("Currents", "Album • Tame Impala"),
                album("Luv(sic) Hexalogy", "Album • Nujabes"),
                album("kyoto rain", "Album • Seycara Orchestral"),
                album("Pink Tape", "Album • Lil Uzi Vert"),
            ],
        },
        Shelf {
            title: "New releases".to_string(),
            cards: vec![
                song("GAMBLER'S FALLACY", "Single • AZALI & stickii B"),
                song("i have a secret", "Single • ThxSoMch"),
                song("Người Im Lặng Gặp Người Hay Nói", "Song • HIEUTHUHAI"),
            ],
        },
    ];
    crate::app::home::arrange(&mut shelves);
    let art = stub_art(&shelves);

    let now = playing();
    let cover = art.get(shelves[0].cards[0].art_key()).unwrap();
    for (width, height) in [(100u16, 36u16), (100, 22), (48, 18)] {
        println!(
            "
--- Home at {width}x{height} ---"
        );
        let (buf, _) = drawn_screen(width, height, View::Home, &now, &shelves, &art, cover);
        print_preview(&format!("home-{width}x{height}"), &buf);
    }
}

/// The now-playing strip, drawn on its own.
///
/// `cargo test preview_hero -- --ignored --nocapture`
#[test]
#[ignore = "prints a layout preview rather than asserting"]
fn preview_hero() {
    let art = Cover::from_rgb(32, 32, [180, 40, 70].repeat(32 * 32));
    let hero = Hero {
        title: "Người Im Lặng Gặp Người Hay Nói",
        artist: "HIEUTHUHAI",
        art: Some(&art),
        snap: Snapshot {
            position: Duration::from_secs(97),
            ..Default::default()
        },
    };

    for width in [70u16, 40] {
        println!(
            "
--- hero at {width} ---"
        );
        let mut terminal = Terminal::new(TestBackend::new(width, HERO_HEIGHT)).unwrap();
        terminal
            .draw(|frame| render_hero(frame, &hero, Rect::new(0, 0, width, HERO_HEIGHT), None))
            .unwrap();
        let buf = terminal.backend().buffer();
        for y in 0..HERO_HEIGHT {
            let row: String = (0..width).map(|x| buf[(x, y)].symbol()).collect();
            println!("{}", row.trim_end());
        }
    }
}

/// The strip exists to name what is playing without the user going to look
/// for it, so the name and the artist are the parts that cannot be dropped.
#[test]
fn the_hero_names_what_is_playing() {
    let art = Cover::from_rgb(4, 4, [180, 40, 70].repeat(16));
    let hero = Hero {
        title: "Feather",
        artist: "Nujabes",
        art: Some(&art),
        snap: Snapshot {
            position: Duration::from_secs(60),
            ..Default::default()
        },
    };

    let mut terminal = Terminal::new(TestBackend::new(60, HERO_HEIGHT)).unwrap();
    terminal
        .draw(|frame| render_hero(frame, &hero, Rect::new(0, 0, 60, HERO_HEIGHT), None))
        .unwrap();
    let buf = terminal.backend().buffer();
    let text: String = (0..HERO_HEIGHT)
        .map(|y| (0..60).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join(
            "
",
        );

    assert!(!text.contains("NOW PLAYING"), "{text}");
    assert!(text.contains("Feather"), "{text}");
    assert!(text.contains("Nujabes"), "{text}");
    assert!(
        !text.contains("1:00 / 3:00"),
        "progress belongs in the persistent strip: {text}"
    );
}

/// A paused player says so. The strip is the only place on the landing page
/// that can, and "now playing" over silence is simply wrong.
#[test]
fn the_hero_says_when_it_is_paused() {
    let hero = Hero {
        title: "Feather",
        artist: "Nujabes",
        art: None,
        snap: Snapshot {
            state: PlayState::Paused,
            ..Default::default()
        },
    };

    let mut terminal = Terminal::new(TestBackend::new(60, HERO_HEIGHT)).unwrap();
    terminal
        .draw(|frame| render_hero(frame, &hero, Rect::new(0, 0, 60, HERO_HEIGHT), None))
        .unwrap();
    let buf = terminal.backend().buffer();
    let text: String = (0..HERO_HEIGHT)
        .map(|y| (0..60).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join(
            "
",
        );

    assert!(text.contains("PAUSED"), "{text}");
    assert!(!text.contains("NOW PLAYING"), "{text}");
}

/// A track as the queue holds them.
fn queued(title: &str, artist: &str, secs: u64) -> Track {
    Track {
        id: title.to_string(),
        title: title.to_string(),
        uploader: artist.to_string(),
        duration: Some(Duration::from_secs(secs)),
        album: None,
        artist_ref: None,
    }
}

/// The player page as it looks a second into a track, queue and all.
pub(super) fn playing() -> NowPlaying {
    let mut now = NowPlaying::new(&Track {
        id: "letithappen".to_string(),
        title: "Let It Happen".to_string(),
        uploader: "Tame Impala".to_string(),
        duration: Some(Duration::from_secs(468)),
        album: Some("Currents".to_string()),
        artist_ref: None,
    });
    now.queue_title = "Let It Happen Mix".to_string();
    now.queue = vec![
        queued("Let It Happen", "Tame Impala", 468),
        queued("The Moment", "Tame Impala", 256),
        queued("Clean Slate", "Passion Mango", 180),
        queued("Chest Pain (I Love)", "Malcolm Todd", 201),
        queued("Razzmatazz", "I DONT KNOW HOW BUT THEY FOUND ME", 259),
        queued("Show Me How", "Men I Trust", 216),
        queued("Dust Bunny", "Crumb", 185),
    ];
    now.playing = Some(0);
    now
}

/// Lyrics as YouTube returns them for a track it holds timings for: the
/// lines split as they are sung, each with the moment it starts.
///
/// The words are invented rather than a real song's. Nothing here depends
/// on what they say -- only on how many lines there are, how long each one
/// wraps to, and how far apart their starts sit -- and a fixture that is
/// somebody's copyrighted lyric is one the tests cannot be shown in full.
/// Each line names its own index, so a test that finds the wrong line
/// highlighted says which one it found.
fn timed_lyrics() -> Lyrics {
    let timed: Vec<TimedLine> = (0..8)
        .map(|n| TimedLine {
            text: format!("sung line {n}, a lyric long enough to wrap in a narrow panel"),
            start: Duration::from_secs(n * 5),
        })
        .collect();

    Lyrics {
        text: timed
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        source: Some("Source: Musixmatch".to_string()),
        timed,
    }
}

/// One panel drawn on its own, as one string per row.
fn drawn_panel(now: &NowPlaying, width: u16, height: u16) -> Vec<String> {
    drawn_panel_at(now, Duration::ZERO, width, height)
}

/// [`drawn_panel`], with the track a given way through -- which only the
/// lyrics panel reads, and is the whole of what it follows.
fn drawn_panel_at(now: &NowPlaying, position: Duration, width: u16, height: u16) -> Vec<String> {
    let snap = Snapshot {
        position,
        ..Default::default()
    };
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut mouse = MouseMap::default();
    terminal
        .draw(|frame| {
            let area = Rect::new(0, 0, width, height);
            let [tabs, content] =
                Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(area);
            render_tabs(frame, now.tab, tabs, Color::Cyan, &mut mouse);
            match now.tab {
                Tab::UpNext => queue::render(frame, now, content, Color::Cyan),
                Tab::Lyrics => render_lyrics(frame, now, &snap, content).0,
                Tab::Comments => render_comments(frame, now, content),
                Tab::Related => render_related(frame, now, content, Color::Cyan),
            };
        })
        .unwrap();

    let buf = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

#[test]
fn compact_tabs_keep_the_new_order_and_underline_the_open_one() {
    let mut now = playing();
    now.tab = Tab::Related;
    // PANEL_MIN_WIDTH has a 32-column interior. Every title must survive in
    // that exact width rather than relying on a roomier preview panel.
    let rows = drawn_panel(&now, PANEL_MIN_WIDTH - 2, 6);

    let tabs = &rows[0];
    let positions = ["Queue", "Lyrics", "Related", "Comments"].map(|label| {
        tabs.find(label)
            .unwrap_or_else(|| panic!("{label:?} is missing from {tabs:?}"))
    });
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "{tabs:?}"
    );
    // The rule sits under the open tab and nothing else, which is the only
    // thing on the row that says which panel is showing.
    let rule = &rows[1];
    assert_eq!(rule.trim(), "─".repeat(display_width("Related")));
    assert_eq!(
        rule.find('─'),
        tabs.find("Related"),
        "the rule sits under the letters, not the padding: {rule:?}"
    );
}

#[test]
fn tiny_panels_keep_all_four_tab_destinations() {
    let rows = drawn_panel(&playing(), 12, 4);
    assert_eq!(rows[0], " 1Q 2L 3R 4C");

    let rows = drawn_panel(&playing(), 8, 4);
    assert_eq!(rows[0], " 1 2 3 4");

    let rows = drawn_panel(&playing(), 4, 4);
    assert_eq!(rows[0], "1234");
}

#[test]
fn the_queue_marks_what_is_playing() {
    let rows = drawn_panel(&playing(), 44, 12);

    assert!(rows[2].contains("Let It Happen Mix"));
    // The marker is on the playing track and on no other row.
    let marked: Vec<&String> = rows
        .iter()
        .filter(|row| row.contains(crate::navigation::current().play()))
        .collect();
    assert_eq!(marked.len(), 1, "exactly one row plays: {rows:#?}");
    assert!(marked[0].contains("Let It Happen"));
    assert!(
        marked[0].contains("7:48"),
        "and states its length: {:?}",
        marked[0]
    );
}

#[test]
fn the_queue_heading_does_not_scroll_with_the_rows() {
    // Scrolled to the bottom of a queue that does not fit. What the queue
    // is a queue of stays true wherever the cursor is, so it stays put.
    let mut now = playing();
    now.cursor[Tab::UpNext.index()] = now.queue.len() - 1;
    let rows = drawn_panel(&now, 44, 9);

    assert!(rows[2].contains("Let It Happen Mix"), "{rows:#?}");
    assert!(
        rows.iter().any(|row| row.contains("Dust Bunny")),
        "the last row scrolled to should be visible: {rows:#?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("The Moment")),
        "and the rows above it should have scrolled off: {rows:#?}"
    );
}

#[test]
fn the_queue_heading_does_not_repeat_the_playback_controls() {
    let mut now = playing();
    now.repeat = RepeatMode::All;

    let rows = drawn_panel(&now, 44, 9);

    assert!(rows[2].contains("Let It Happen Mix"), "{rows:#?}");
    assert!(!rows.iter().any(|row| row.contains("Repeat")), "{rows:#?}");
}

#[test]
fn an_unfetched_panel_says_it_is_loading_and_a_failed_one_says_why() {
    let mut now = playing();
    now.tab = Tab::Lyrics;
    assert!(drawn_panel(&now, 44, 6)[2].contains("loading"));

    now.lyrics = Panel::Empty("no lyrics are published for this track".to_string());
    let rows = drawn_panel(&now, 44, 6);
    assert!(
        rows[2].contains("no lyrics"),
        "the reason belongs in the panel, not the status bar: {rows:#?}"
    );
}

#[test]
fn lyrics_scroll_by_line_and_stop_a_screenful_short_of_the_end() {
    let mut now = playing();
    now.tab = Tab::Lyrics;
    now.lyrics = Panel::Ready(Lyrics {
        text: (1..=20).map(|n| format!("line {n}\n")).collect(),
        source: Some("Source: Musixmatch".to_string()),
        timed: Vec::new(),
    });

    let top = drawn_panel(&now, 44, 8);
    assert!(top[2].contains("line 1"), "{top:#?}");

    // Six lines of content in an eight-row pane, two of them tabs: the last
    // line has to stay on screen rather than scrolling past the top.
    now.cursor[Tab::Lyrics.index()] = usize::MAX;
    let bottom = drawn_panel(&now, 44, 8);
    assert!(
        bottom.iter().any(|row| row.contains("line 20")),
        "the end of the lyrics should be reachable: {bottom:#?}"
    );
}

/// The row carrying the mark, and where in the panel it sits. `None` when
/// no line is marked, which is a distinct outcome from the wrong one being
/// marked and is asserted on in its own right below.
fn marked(rows: &[String]) -> Option<(usize, &String)> {
    rows.iter().enumerate().find(|(_, row)| row.contains('▌'))
}

#[test]
fn the_line_being_sung_is_the_marked_one() {
    let mut now = playing();
    now.tab = Tab::Lyrics;
    now.lyrics = Panel::Ready(timed_lyrics());

    // Wide enough that every lyric is one row, so what the test is about is
    // which line is marked rather than how a line wrapped.
    let rows = drawn_panel_at(&now, Duration::from_secs(22), 70, 8);
    let (_, row) = marked(&rows).unwrap_or_else(|| panic!("a line should be marked: {rows:#?}"));
    assert!(
        row.contains("sung line 4"),
        "the fifth line starts at 20s and the sixth at 25s: {row:?}"
    );
    assert_eq!(
        rows.iter().filter(|row| row.contains('▌')).count(),
        1,
        "one lyric is being sung, so one mark: {rows:#?}"
    );
}

#[test]
fn a_wrapped_lyric_is_marked_once_at_its_first_row() {
    let mut now = playing();
    now.tab = Tab::Lyrics;
    now.lyrics = Panel::Ready(timed_lyrics());

    // Narrow enough that every lyric wraps onto two rows. One mark, on the
    // row the lyric starts at: a bar down each of its rows would read as
    // several lines being sung rather than one occupying several rows.
    let rows = drawn_panel_at(&now, Duration::from_secs(22), 44, 10);
    let (at, row) = marked(&rows).unwrap_or_else(|| panic!("a line should be marked: {rows:#?}"));

    assert_eq!(
        rows.iter().filter(|row| row.contains('▌')).count(),
        1,
        "one mark however many rows the lyric wraps onto: {rows:#?}"
    );
    assert!(row.contains("sung line 4"), "{rows:#?}");
    assert!(
        !row.contains("narrow panel"),
        "the mark belongs on the row the lyric starts at: {rows:#?}"
    );
    // The rest of the lyric is still drawn, just unmarked.
    assert!(
        rows[at + 1].contains("narrow panel"),
        "the wrapped remainder should follow it: {rows:#?}"
    );
}

#[test]
fn nothing_is_marked_before_the_first_line_is_sung() {
    let mut now = playing();
    now.tab = Tab::Lyrics;
    now.lyrics = Panel::Ready(timed_lyrics());

    // The fixture's first line starts at zero, so this is the case of a
    // track whose words have not begun: an intro should mark nothing.
    let mut late = timed_lyrics();
    late.timed[0].start = Duration::from_secs(9);
    now.lyrics = Panel::Ready(late);

    let rows = drawn_panel_at(&now, Duration::from_secs(3), 70, 8);
    assert!(
        marked(&rows).is_none(),
        "no line has started yet: {rows:#?}"
    );
}

#[test]
fn the_panel_keeps_the_line_being_sung_near_the_middle() {
    let mut now = playing();
    now.tab = Tab::Lyrics;
    now.lyrics = Panel::Ready(timed_lyrics());

    // Far enough in that the panel has to have scrolled: the sixth line is
    // past the bottom of a six-row viewport showing the top of the lyrics.
    let rows = drawn_panel_at(&now, Duration::from_secs(27), 70, 8);
    let (at, row) = marked(&rows).unwrap_or_else(|| panic!("a line should be marked: {rows:#?}"));
    assert!(row.contains("sung line 5"), "{row:?}");
    // Two rows of tabs above a six-row viewport: centred means the middle
    // of the panel, not the top edge, so what has just been sung is still
    // readable above it.
    assert!(
        (4..=6).contains(&at),
        "the marked line should sit mid-panel, not at an edge: row {at} of {rows:#?}"
    );

    // And it moves down the lyrics rather than staying put as time passes.
    let later = drawn_panel_at(&now, Duration::from_secs(35), 70, 8);
    let (_, row) = marked(&later).unwrap_or_else(|| panic!("a line should be marked: {later:#?}"));
    assert!(row.contains("sung line 7"), "{row:?}");
}

#[test]
fn a_user_who_has_scrolled_keeps_the_view_they_scrolled_to() {
    let mut now = playing();
    now.tab = Tab::Lyrics;
    now.lyrics = Panel::Ready(timed_lyrics());
    // What `scroll_page` does: the panel stops following and the cursor
    // decides what is on screen, however far through the track it is.
    now.follow_lyrics = false;
    now.cursor[Tab::Lyrics.index()] = 0;

    let rows = drawn_panel_at(&now, Duration::from_secs(35), 70, 8);
    assert!(
        rows[2].contains("sung line 0"),
        "the panel should still be at the top: {rows:#?}"
    );
    assert!(
        marked(&rows).is_none(),
        "the line being sung is off screen, and the panel stays where it \
             was put rather than chasing it: {rows:#?}"
    );

    // Only the scrolling is handed over, not the highlight: a sung line
    // that happens to be on screen is still marked.
    let early = drawn_panel_at(&now, Duration::from_secs(8), 70, 8);
    let (_, row) = marked(&early).unwrap_or_else(|| panic!("a line should be marked: {early:#?}"));
    assert!(row.contains("sung line 1"), "{row:?}");
}

#[test]
fn a_track_with_no_timings_draws_as_it_always_did() {
    let mut now = playing();
    now.tab = Tab::Lyrics;
    now.lyrics = Panel::Ready(Lyrics {
        text: (1..=20).map(|n| format!("line {n}\n")).collect(),
        source: Some("Source: Musixmatch".to_string()),
        timed: Vec::new(),
    });

    // However far through the track, an untimed panel has nothing to mark
    // and nothing to follow: it scrolls by cursor as it did before.
    let rows = drawn_panel_at(&now, Duration::from_secs(120), 44, 8);
    assert!(marked(&rows).is_none(), "{rows:#?}");
    assert!(rows[2].contains("line 1"), "{rows:#?}");
}

#[test]
fn only_the_lyric_being_sung_is_emphasised() {
    // Bold *and* a marker for the current line: on a terminal that renders
    // bold as bright and nothing else, colour alone would not say which
    // line of twenty is the one.
    let current = lyric_line("now", false, Some(3), Some(3), true);
    assert_eq!(current.spans[0].content, "▌ ");
    assert_eq!(current.spans[1].style.fg, Some(Color::White));
    assert!(current.spans[1].style.add_modifier.contains(Modifier::BOLD));

    // Every other lyric reads the same, whether it has been sung or not:
    // one of them is "here" and the rest are the song around it, and a
    // third shade would only be something to work out.
    //
    // DarkGray rather than Gray, and that is the whole of the contrast:
    // Gray is ANSI 7 to White's ANSI 15, which on many themes is no
    // difference at all and leaves the sung line marked by boldness alone.
    let sung = lyric_line("before", false, Some(2), Some(3), true);
    let ahead = lyric_line("after", false, Some(4), Some(3), true);
    assert_eq!(sung.spans[0].content, "  ");
    assert_eq!(sung.spans[1].style.fg, Some(Color::DarkGray));
    assert_eq!(ahead.spans[1].style.fg, Some(Color::DarkGray));
    assert_eq!(sung.spans[1].style, ahead.spans[1].style);
    assert!(
        !sung.spans[1].style.add_modifier.contains(Modifier::BOLD),
        "and no bold either, or the dimming buys nothing"
    );

    // A track with no timings has no line being sung, so there is nothing
    // for dimming to contrast with: the words stay at ordinary body grey
    // rather than the whole panel going dark and gaining nothing for it.
    let plain = lyric_line("plain", false, None, None, false);
    assert_eq!(plain.spans[0].content, "  ");
    assert_eq!(plain.spans[1].style.fg, Some(Color::Gray));
    // Same for a timed track still in its intro.
    assert_eq!(
        lyric_line("intro", false, Some(0), None, true).spans[1]
            .style
            .fg,
        Some(Color::Gray)
    );

    // The continuation of a wrapped lyric: still emphasised, since it is
    // the same line being sung, but the mark stays on the row it started.
    let wrapped = lyric_line("...and on", false, Some(3), Some(3), false);
    assert_eq!(wrapped.spans[0].content, "  ");
    assert_eq!(wrapped.spans[1].style, current.spans[1].style);

    // The credit is about the panel rather than the song, and is never the
    // line being sung whatever index it happens to sit at.
    let credit = lyric_line("Source: Musixmatch", true, None, Some(0), true);
    assert_eq!(credit.spans[0].content, "  ");
    assert_eq!(credit.spans[1].style.fg, Some(Color::DarkGray));
}

#[test]
fn a_comment_shows_who_wrote_it_when_and_what_it_got() {
    let mut now = playing();
    now.tab = Tab::Comments;
    now.comments = Panel::Ready(Comments {
        total: "8,407 Comments".to_string(),
        items: vec![Comment {
            author: "@SpeartonFromOrder".to_string(),
            published: "4 months ago".to_string(),
            text: "Don't search for best part. Just let it happen".to_string(),
            likes: "10K".to_string(),
            replies: "67".to_string(),
        }],
    });

    let rows = drawn_panel(&now, 44, 10);
    assert!(rows[2].contains("8,407 Comments"), "{rows:#?}");
    assert!(rows[4].contains("@SpeartonFromOrder"));
    assert!(rows[4].contains("4 months ago"));
    // Wrapped to the panel rather than truncated: a comment cut off at the
    // right margin is one nobody can read the point of.
    assert!(rows[5].contains("Don't search for best part."), "{rows:#?}");
    assert!(rows[6].contains("happen"), "{rows:#?}");
    assert!(rows.iter().any(|row| row.contains("10K likes")));
    assert!(rows.iter().any(|row| row.contains("67 replies")));
}

#[test]
fn related_lists_each_shelf_under_its_own_heading() {
    let mut now = playing();
    now.tab = Tab::Related;
    now.related = Panel::Ready(vec![
        Shelf {
            title: "You might also like".to_string(),
            cards: vec![
                song("Eventually", "Tame Impala • Currents"),
                song("Yes I'm Changing", "Tame Impala • Currents"),
            ],
        },
        Shelf {
            title: "Recommended playlists".to_string(),
            cards: vec![album("one step, another step", "Playlist")],
        },
    ]);

    let rows = drawn_panel(&now, 44, 10);
    assert!(rows[2].contains("You might also like"), "{rows:#?}");
    assert!(rows[3].contains("Eventually"));
    assert!(rows[4].contains("Tame Impala"));
    assert!(rows.iter().any(|row| row.contains("Recommended playlists")));
    assert!(
        rows.iter()
            .any(|row| row.contains("one step, another step"))
    );
}

#[test]
fn the_progress_bar_fills_in_proportion() {
    let snap = |secs: u64| Snapshot {
        position: Duration::from_secs(secs),
        ..Default::default()
    };
    let filled = |line: Line| {
        line.spans
            .iter()
            .map(|span| span.content.matches('━').count())
            .sum::<usize>()
    };

    let total = Some(Duration::from_secs(100));
    // 40 columns less the leading-space clock leaves 28.
    assert_eq!(filled(progress_line(&snap(0), total, 40, Color::Cyan)), 0);
    assert_eq!(
        filled(progress_line(&snap(100), total, 40, Color::Cyan)),
        28
    );
    assert_eq!(filled(progress_line(&snap(50), total, 40, Color::Cyan)), 14);

    // Past the end, which happens for a moment on every track: the
    // container's length and what the decoder yields never quite agree.
    assert_eq!(
        filled(progress_line(&snap(105), total, 40, Color::Cyan)),
        28
    );
}

#[test]
fn clicking_the_progress_bar_maps_its_whole_width() {
    let area = Rect::new(5, 7, 11, 1);
    let mouse = MouseMap {
        key_hints_hidden: false,
        targets: vec![MouseTarget::Progress(area)],
    };

    assert_eq!(mouse.action_at(5, 7), Some(MouseAction::SeekTo(0)));
    assert_eq!(
        mouse.action_at(10, 7),
        Some(MouseAction::SeekTo(POINTER_SCALE / 2))
    );
    assert_eq!(
        mouse.action_at(15, 7),
        Some(MouseAction::SeekTo(POINTER_SCALE))
    );
    assert_eq!(mouse.action_at(16, 7), None, "the clock is not a scrubber");
}

#[test]
fn context_click_selects_the_item_under_the_pointer_without_opening_it() {
    let mouse = MouseMap {
        key_hints_hidden: false,
        targets: vec![
            MouseTarget::Area(
                Rect::new(2, 1, 8, 3),
                MouseAction::OpenHomeCard { shelf: 3, card: 4 },
            ),
            MouseTarget::TrackRows {
                area: Rect::new(2, 5, 20, 3),
                first: 6,
            },
            MouseTarget::Area(Rect::new(25, 6, 20, 1), MouseAction::OpenPageRow(10)),
        ],
    };

    assert_eq!(
        mouse.context_action_at(3, 2),
        Some(MouseAction::SelectHomeCard { shelf: 3, card: 4 })
    );
    assert_eq!(
        mouse.context_action_at(3, 7),
        Some(MouseAction::SelectTrack(8))
    );
    assert_eq!(
        mouse.context_action_at(26, 6),
        Some(MouseAction::SelectPageRow(10))
    );
    assert_eq!(mouse.context_action_at(0, 0), None);
}

#[test]
fn the_progress_hit_target_ends_before_the_clock() {
    let snap = Snapshot {
        position: Duration::from_secs(65),
        ..Default::default()
    };
    let total = Some(Duration::from_secs(180));
    let width = progress_bar_width(&snap, total, 40).unwrap();
    let line = progress_line(&snap, total, 40, Color::Cyan);

    assert_eq!(width, 28);
    assert_eq!(line.width(), 40);
    assert_eq!(line.spans[2].content, " 1:05 / 3:00");
}

#[test]
fn a_track_of_unknown_length_gets_a_clock_and_no_bar() {
    let snap = Snapshot {
        position: Duration::from_secs(65),
        ..Default::default()
    };
    // A livestream has no end to draw a bar towards, and one drawn anyway
    // would have to invent where it is.
    let line = progress_line(&snap, None, 40, Color::Cyan);
    assert_eq!(line.spans.len(), 1);
    assert_eq!(line.spans[0].content.trim(), "1:05");
}

#[test]
fn mini_player_combines_known_progress_clock_and_track() {
    // Fit all metadata so the assertion is independent of the marquee clock.
    let width = 64;
    let snap = Snapshot {
        state: PlayState::Playing,
        title: "Let It Happen".to_string(),
        position: Duration::from_secs(65),
        ..Default::default()
    };
    let line = mini_player_line(
        &snap,
        Some(MiniTrack {
            title: "Let It Happen",
            byline: "Tame Impala",
            duration: Some(Duration::from_secs(180)),
        }),
        "idle",
        true,
        width,
        Color::Magenta,
    );
    let text = line_text(&line);

    assert!(text.starts_with(" ‹‹  II  ›› "), "{text:?}");
    assert_eq!(display_width(" ‹‹  II  ›› "), usize::from(TRANSPORT_WIDTH));
    assert!(text.contains("1:05/3:00"), "{text:?}");
    assert!(text.contains('━') && text.contains('─'), "{text:?}");
    assert!(text.contains("Let It Happen"), "{text:?}");
    assert!(text.contains("Tame Impala"), "{text:?}");
    assert!(display_width(&text) <= width, "{text:?}");
}

#[test]
fn transport_buttons_have_distinct_mouse_targets() {
    let mut mouse = MouseMap::default();
    register_transport_targets(&mut mouse, Rect::new(10, 3, 30, 1));

    assert_eq!(mouse.action_at(11, 3), Some(MouseAction::PreviousTrack));
    assert_eq!(mouse.action_at(14, 3), Some(MouseAction::TogglePlayback));
    assert_eq!(mouse.action_at(18, 3), Some(MouseAction::NextTrack));
    assert_eq!(mouse.action_at(22, 3), None);
}

#[test]
fn dragging_across_transport_and_navigation_does_not_activate_them() {
    let mut mouse = MouseMap::default();
    register_transport_targets(&mut mouse, Rect::new(0, 1, 30, 1));
    register_volume_targets(&mut mouse, Rect::new(40, 1, STATUS_VOLUME_WIDTH, 1), 0.8);
    mouse.targets.push(MouseTarget::Area(
        Rect::new(0, 0, 7, 1),
        MouseAction::GoHome,
    ));
    mouse
        .targets
        .push(MouseTarget::Progress(Rect::new(0, 2, 20, 1)));
    for (x, y) in [(1, 1), (4, 1), (7, 1), (1, 0), (41, 1)] {
        assert_eq!(mouse.drag_action_at(x, y), None);
    }
    assert!(matches!(
        mouse.drag_action_at(10, 2),
        Some(MouseAction::SeekTo(_))
    ));
    assert!(matches!(
        mouse.drag_action_at(50, 1),
        Some(MouseAction::SetVolume(_))
    ));
}

#[test]
fn status_controls_drop_volume_before_track_identity() {
    let [wide_player, wide_volume, wide_hints] =
        status_control_areas(Rect::new(0, 0, 100, 1), true, HINTS_WIDTH);
    assert_eq!(wide_volume.width, STATUS_VOLUME_WIDTH);
    assert!(wide_player.width >= MINI_PLAYER_FLOOR);
    assert_eq!(
        wide_player.width + wide_volume.width + wide_hints.width,
        100
    );

    let [narrow_player, narrow_volume, narrow_hints] = status_control_areas(
        Rect::new(0, 0, MIN_WIDTH_WITH_STATUS_VOLUME - 1, 1),
        true,
        HINTS_WIDTH,
    );
    assert_eq!(narrow_volume.width, 0);
    assert!(narrow_player.width >= MINI_PLAYER_FLOOR);
    assert_eq!(
        narrow_player.width + narrow_hints.width,
        MIN_WIDTH_WITH_STATUS_VOLUME - 1
    );

    let [idle_player, idle_volume, idle_hints] =
        status_control_areas(Rect::new(0, 0, 100, 1), false, HINTS_WIDTH);
    assert_eq!(idle_volume.width, 0);
    assert_eq!(idle_player.width + idle_hints.width, 100);
}

#[test]
fn mini_player_never_invents_progress_for_an_unknown_duration() {
    let snap = Snapshot {
        state: PlayState::Playing,
        title: "Live set".to_string(),
        position: Duration::from_secs(65),
        ..Default::default()
    };
    let line = mini_player_line(
        &snap,
        Some(MiniTrack {
            title: "Live set",
            byline: "Night Radio",
            duration: None,
        }),
        "idle",
        true,
        32,
        Color::Cyan,
    );
    let text = line_text(&line);

    assert!(text.contains("1:05"), "{text:?}");
    assert!(!text.contains('/'), "no invented total: {text:?}");
    assert!(
        !text.contains('━') && !text.contains('─'),
        "no invented bar: {text:?}"
    );
}

#[test]
fn mini_player_stays_bounded_when_narrow_and_omits_a_duplicate_clock() {
    let snap = Snapshot {
        state: PlayState::Playing,
        title: "日本語の長いタイトル".to_string(),
        position: Duration::from_secs(65),
        ..Default::default()
    };
    let track = Some(MiniTrack {
        title: "日本語の長いタイトル",
        byline: "アーティスト",
        duration: Some(Duration::from_secs(180)),
    });

    let narrow = line_text(&mini_player_line(
        &snap,
        track,
        "idle",
        true,
        7,
        Color::Cyan,
    ));
    assert!(display_width(&narrow) <= 7, "{narrow:?}");

    let without_progress = line_text(&mini_player_line(
        &snap,
        track,
        "idle",
        false,
        32,
        Color::Cyan,
    ));
    assert!(!without_progress.contains("1:05"), "{without_progress:?}");
    assert!(!without_progress.contains('/'), "{without_progress:?}");
    assert!(!without_progress.contains('━'), "{without_progress:?}");
}

#[test]
fn mini_player_distinguishes_pause_but_errors_still_win() {
    let paused = Snapshot {
        state: PlayState::Paused,
        title: "Feather".to_string(),
        ..Default::default()
    };
    let paused_text = line_text(&mini_player_line(
        &paused,
        None,
        "idle",
        true,
        20,
        Color::Cyan,
    ));
    assert!(
        paused_text.starts_with(&format!(
            " ‹‹  {}   ›› ",
            crate::navigation::current().play()
        )),
        "{paused_text:?}"
    );
    assert!(paused_text.contains("Feather"), "{paused_text:?}");

    let failed = Snapshot {
        state: PlayState::Playing,
        title: "Feather".to_string(),
        error: Some("decoder stopped unexpectedly".to_string()),
        ..Default::default()
    };
    let line = mini_player_line(&failed, None, "idle", true, 14, Color::Cyan);
    let error_text = line_text(&line);
    assert!(display_width(&error_text) <= 14, "{error_text:?}");
    assert!(!error_text.contains("Feather"), "{error_text:?}");
    assert_eq!(line.spans[0].style.fg, Some(Color::Red));
}

#[test]
fn playback_uses_the_requested_controls_and_track_label() {
    let snap = Snapshot {
        state: PlayState::Playing,
        title: "ill miss you".into(),
        ..Default::default()
    };
    let text = line_text(&mini_player_line(
        &snap,
        Some(MiniTrack {
            title: "ill miss you",
            byline: "AZALI",
            duration: None,
        }),
        "idle",
        false,
        60,
        Color::Cyan,
    ));
    assert_eq!(text, " ‹‹  II  ›› ill miss you · AZALI");
}

#[test]
fn failed_track_keeps_retry_and_skip_visible_in_the_player_strip() {
    let failed = Snapshot {
        state: PlayState::Idle,
        error: Some("network interrupted playback".to_string()),
        ..Default::default()
    };
    let track = MiniTrack {
        title: "Feather",
        byline: "Nujabes",
        duration: Some(Duration::from_secs(175)),
    };
    let text = line_text(&mini_player_line(
        &failed,
        Some(track),
        "idle",
        false,
        64,
        Color::Cyan,
    ));

    assert!(text.contains("network interrupted playback"), "{text:?}");
    assert!(text.contains("r retry"), "{text:?}");
    assert!(text.contains("n skip"), "{text:?}");
}

#[test]
fn the_cursor_stays_near_the_middle_of_a_long_list() {
    // Nothing to scroll: everything fits.
    assert_eq!(centred_offset(3, 10, 5), 0);
    // Near the top, the list stays pinned there rather than centring a
    // cursor that has nothing above it.
    assert_eq!(centred_offset(1, 10, 40), 0);
    assert_eq!(centred_offset(20, 10, 40), 15);
    // And at the bottom it stops rather than scrolling past the last row.
    assert_eq!(centred_offset(39, 10, 40), 30);
    assert_eq!(centred_offset(5, 0, 40), 0);
}

#[test]
fn the_track_details_name_the_song_the_artist_and_the_album() {
    let now = playing();
    assert_eq!(now.byline(), "Tame Impala • Currents");

    let mut terminal = Terminal::new(TestBackend::new(40, INFO_HEIGHT)).unwrap();
    terminal
        .draw(|frame| {
            let snap = Snapshot {
                position: Duration::from_secs(1),
                volume: 0.8,
                ..Default::default()
            };
            render_track_info(
                frame,
                &now,
                snap,
                Rect::new(0, 0, 40, INFO_HEIGHT),
                Color::Cyan,
            );
        })
        .unwrap();

    let buf = terminal.backend().buffer();
    let row = |y: u16| {
        (0..40)
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    };
    assert_eq!(row(0), "");
    assert_eq!(row(1), "");
    assert_eq!(row(2), "Let It Happen");
    assert_eq!(row(3), "Tame Impala • Currents");
}

#[test]
fn volume_feedback_does_not_repeat_a_visible_volume_control() {
    for (width, status, volume, visible) in [
        (120, "volume 6%", 0.06, false),
        (120, "muted", 0.0, false),
        (
            120,
            "muted -- press m or click mut to restore volume",
            0.0,
            false,
        ),
        (48, "volume 6%", 0.06, true),
        (48, "muted", 0.0, true),
        (120, "could not change volume: unavailable", 0.06, true),
        (120, "Could not save the volume setting.", 0.06, true),
        (120, "precision volume off", 0.06, true),
        (120, "Added a song to your queue", 0.06, true),
    ] {
        for show_identity in [true, false] {
            let snap = Snapshot {
                state: PlayState::Playing,
                volume,
                ..Default::default()
            };
            let mut terminal = Terminal::new(TestBackend::new(width, 4)).unwrap();
            let mut mouse = MouseMap::default();
            terminal
                .draw(|frame| {
                    render_player_strip(
                        frame,
                        PlayerStrip {
                            snap: &snap,
                            track: Some(MiniTrack {
                                title: "Song",
                                byline: "Artist",
                                duration: None,
                            }),
                            status,
                            hint: "? help",
                            accent: Color::Cyan,
                            show_identity,
                            keys: Some(View::Playing),
                        },
                        frame.area(),
                        &mut mouse,
                    )
                })
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert_eq!(
                text.contains(status),
                visible,
                "width {width}, {status}: {text}"
            );
            if status == "volume 6%" {
                assert_eq!(
                    text.matches("6%").count(),
                    1,
                    "exactly one volume readout: {text}"
                );
            }
        }
    }
}

#[test]
fn the_volume_bar_fills_against_a_hundred_percent() {
    let bar = |volume: f32| volume_line(volume, 40).spans[1].content.chars().count();

    assert_eq!(bar(0.0), 0);
    assert_eq!(bar(0.5), VOLUME_BAR_WIDTH / 2);
    assert_eq!(bar(1.0), VOLUME_BAR_WIDTH);
    assert_eq!(volume_line(1.0, 40).spans[1].style.fg, Some(Color::Gray));
}

#[test]
fn clicking_the_volume_bar_maps_zero_to_one_hundred_percent() {
    let area = Rect::new(20, 4, 11, 1);
    let mouse = MouseMap {
        key_hints_hidden: false,
        targets: vec![MouseTarget::Volume(area)],
    };

    assert_eq!(mouse.action_at(20, 4), Some(MouseAction::SetVolume(0)));
    assert_eq!(
        mouse.action_at(25, 4),
        Some(MouseAction::SetVolume(POINTER_SCALE / 2))
    );
    assert_eq!(
        mouse.action_at(30, 4),
        Some(MouseAction::SetVolume(POINTER_SCALE))
    );
    assert_eq!(mouse.action_at(31, 4), None);
}

#[test]
fn volume_label_is_a_separate_mute_control() {
    let area = Rect::new(20, 4, STATUS_VOLUME_WIDTH, 1);
    let mut mouse = MouseMap::default();
    register_volume_targets(&mut mouse, area, 0.8);

    assert_eq!(
        mouse.action_at(20, 4),
        Some(MouseAction::AdjustVolume(false))
    );
    assert_eq!(mouse.action_at(24, 4), Some(MouseAction::ToggleMute));
    assert_eq!(mouse.action_at(27, 4), Some(MouseAction::ToggleMute));
    assert_eq!(mouse.action_at(28, 4), Some(MouseAction::SetVolume(0)));
    assert_eq!(
        mouse.action_at(40, 4),
        None,
        "the percentage is only a label"
    );
    assert!(
        mouse.over_volume(40, 4),
        "the wheel also works over the percentage"
    );
    assert_eq!(
        mouse.action_at(area.right() - 1, 4),
        Some(MouseAction::AdjustVolume(true))
    );
    assert!(line_text(&volume_line(0.0, area.width as usize)).starts_with("mut "));
    assert!(line_text(&volume_line(0.8, area.width as usize)).starts_with("vol "));
    mouse.targets.push(MouseTarget::Area(
        Rect::new(0, 0, 80, 12),
        MouseAction::IgnoreOverlay,
    ));
    assert!(
        !mouse.over_volume(40, 4),
        "dialogs block the volume wheel behind them"
    );
}

#[test]
fn a_volume_with_no_room_for_a_bar_is_still_a_number() {
    // The panel is the width of the label and no more. A bar of two cells
    // would be decoration; the percentage is what is actually read.
    let line = volume_line(0.75, 12);
    assert_eq!(line.spans.len(), 1);
    assert_eq!(line.spans[0].content, "vol 75%");
}

/// Prints the whole player page, for eyeballing it against what YouTube
/// Music draws.
///
/// `cargo test preview_player -- --ignored --nocapture`
#[test]
#[ignore = "prints a layout preview rather than asserting"]
fn preview_player() {
    let mut now = preview_now();
    let cover = Cover::from_rgb(32, 32, [180, 90, 60].repeat(32 * 32));
    let art = ArtCache::default();
    for (width, height) in [(48u16, 18u16), (100, 36), (160, 42)] {
        for tab in Tab::ALL {
            now.tab = tab;
            println!("\n--- {} at {width}x{height} ---", tab.label());
            let (buf, _) = drawn_screen(width, height, View::Playing, &now, &[], &art, &cover);
            print_preview(
                &format!("player-{}-{width}x{height}", tab.label().replace(' ', "-")),
                &buf,
            );
        }
        now.tab = Tab::Lyrics;
        now.lyrics_expanded = true;
        let (buf, _) = drawn_screen(width, height, View::Playing, &now, &[], &art, &cover);
        print_preview(&format!("player-Lyrics-expanded-{width}x{height}"), &buf);
        now.lyrics_expanded = false;
    }
}

fn preview_now() -> NowPlaying {
    let mut now = playing();
    now.lyrics = Panel::Ready(timed_lyrics());
    now.comments = Panel::Ready(Comments {
        total: "8,407 Comments".to_string(),
        items: vec![
            Comment {
                author: "@SpeartonFromOrder".to_string(),
                published: "4 months ago".to_string(),
                text: "Don't search for best part. Just let it happen".to_string(),
                likes: "10K".to_string(),
                replies: "67".to_string(),
            },
            Comment {
                author: "@jujuca.lol912".to_string(),
                published: "5 months ago".to_string(),
                text: "I won't let depression win".to_string(),
                likes: "10K".to_string(),
                replies: "226".to_string(),
            },
        ],
    });
    now.related = Panel::Ready(vec![Shelf {
        title: "You might also like".to_string(),
        cards: vec![
            song("Eventually", "Tame Impala • Currents"),
            song("Yes I'm Changing", "Tame Impala • Currents"),
            song("Breathe Deeper", "Tame Impala • The Slow Rush"),
        ],
    }]);

    now
}

/// Uses production shell geometry and page components without starting
/// audio, signing in, or asking YouTube Music for fixture data.
fn drawn_screen(
    width: u16,
    height: u16,
    view: View,
    now: &NowPlaying,
    shelves: &[Shelf],
    art: &ArtCache,
    cover: &Cover,
) -> (ratatui::buffer::Buffer, MouseMap) {
    let snap = Snapshot {
        state: PlayState::Playing,
        position: Duration::from_secs(60),
        volume: 0.8,
        ..Default::default()
    };
    let accent = Color::Rgb(cover.accent.0, cover.accent.1, cover.accent.2);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut mouse = MouseMap::default();
    terminal
        .draw(|frame| {
            frame.render_widget(
                Block::default().style(Style::default().bg(shell::BACKGROUND).fg(Color::White)),
                frame.area(),
            );
            let [header, main, footer] = shell::regions(frame.area());
            shell::render_header(
                frame,
                header,
                shell::Header {
                    mode: Mode::Browse,
                    view,
                    query: "",
                    accent,
                    query_cursor: 0,
                    can_go_back: view != View::Home,
                    has_player: true,
                    accepts_input: true,
                    symbols: crate::navigation::Symbols::from_mask(0x15),
                },
                &mut mouse,
            );
            if view == View::Home {
                let inner = home::content_area(main);
                let shape = plan_home(inner).max_shape;
                let mut wanted = Vec::new();
                render_feed(
                    frame,
                    shelves,
                    &shelf_layouts(shelves, inner, 0, shape),
                    HomeCursor { shelf: 0, card: 0 },
                    &vec![0; shelves.len()],
                    Tiles {
                        shape,
                        art,
                        wanted: &mut wanted,
                        images: None,
                    },
                );
            } else {
                let layout = player_layout(main, now, Some(cover), None);
                render_player_identity(
                    frame,
                    PlayerIdentity {
                        now,
                        snap: snap.clone(),
                        cover: Some(cover),
                        accent,
                    },
                    &layout,
                    None,
                );
                register_player_identity_targets(&mut mouse, now, &layout);
                let [tabs, content] = Layout::vertical([Constraint::Length(2), Constraint::Min(0)])
                    .areas(shell::panel_inset(layout.panel));
                render_tabs(frame, now.tab, tabs, accent, &mut mouse);
                let content = if now.tab == Tab::Lyrics && content.height >= 4 {
                    let [controls, body] =
                        Layout::vertical([Constraint::Length(2), Constraint::Min(0)])
                            .areas(content);
                    render_lyrics_controls(frame, now, controls, accent, &mut mouse);
                    body
                } else {
                    content
                };
                render_panel_body(frame, now, &snap, content, accent);
                if let Some(art) = layout.art {
                    render_cover(frame, cover, art, CoverSize::Full);
                }
            }
            frame.render_widget(
                Block::default().style(Style::default().bg(shell::SURFACE)),
                footer,
            );
            let footer = shell::footer_content(footer);
            render_player_strip(
                frame,
                PlayerStrip {
                    snap: &snap,
                    track: Some(MiniTrack {
                        title: &now.title,
                        byline: &now.artist,
                        duration: now.duration,
                    }),
                    status: "",
                    hint: if view == View::Home {
                        HINTS_AWAY
                    } else {
                        HINTS_PLAYING
                    },
                    accent,
                    show_identity: view != View::Playing
                        || !has_player_identity(&player_layout(main, now, Some(cover), None)),
                    keys: Some(view),
                },
                footer,
                &mut mouse,
            );
            actions::bar(frame, now, footer, &mut mouse, Some(view));
            palette::apply(frame, Some(cover), None);
        })
        .unwrap();
    (terminal.backend().buffer().clone(), mouse)
}

pub(super) fn print_preview(name: &str, buf: &ratatui::buffer::Buffer) {
    for y in 0..buf.area.height {
        let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        println!("{}", row.trim_end());
    }
    // Optional visual QA export contains actual rendered cells and colors.
    if let Some(dir) = std::env::var_os("MTUI_PREVIEW_DIR") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cells: Vec<_> = buf
            .content
            .iter()
            .map(|cell| {
                serde_json::json!({
                    "text": cell.symbol(), "fg": format!("{:?}", cell.fg),
                    "bg": format!("{:?}", cell.bg), "bold": cell.modifier.contains(Modifier::BOLD),
                })
            })
            .collect();
        let snapshot = serde_json::json!({ "width": buf.area.width, "height": buf.area.height, "cells": cells });
        std::fs::write(
            dir.join(format!("{name}.json")),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();
    }
}

#[test]
fn player_caption_stays_below_the_fitted_cover_and_owns_its_links() {
    let mut now = preview_now();
    now.tab = Tab::Lyrics;
    now.artist_ref = Some(ArtistRef {
        name: now.artist.clone(),
        endpoint: BrowseEndpoint::new("UCartist"),
    });
    now.album = Some("Currents".into());
    now.album_route = Some(("Currents".into(), BrowseEndpoint::new("MPREalbum")));
    for (width, height) in [(64, 20), (100, 36), (160, 42)] {
        for (cols, rows) in [(32, 32), (160, 90)] {
            let cover = Cover::solid(cols, rows);
            let main = shell::regions(Rect::new(0, 0, width, height))[1];
            let layout = player_layout(main, &now, Some(&cover), None);
            let art = layout.art.unwrap();
            let info = layout.info.unwrap();
            assert_eq!((info.x, info.width), (art.x, art.width));
            assert_eq!(
                info.y,
                art.bottom(),
                "caption must stay attached to the fitted picture"
            );
            assert!(info.bottom() <= main.bottom());
            let (buf, mouse) = drawn_screen(
                width,
                height,
                View::Playing,
                &now,
                &[],
                &ArtCache::default(),
                &cover,
            );
            let title_row = track_info_rows(info)[1];
            let title: String = (title_row.x..title_row.right())
                .map(|x| buf[(x, title_row.y)].symbol())
                .collect();
            assert!(title.contains(&now.title), "{title}");
            let byline = track_info_rows(info)[2];
            assert_eq!(
                mouse.action_at(byline.x, byline.y),
                Some(MouseAction::OpenPlayingArtist)
            );
            let album_x = byline.x + display_width(&now.artist) as u16 + 3;
            if album_x < byline.right() {
                assert_eq!(
                    mouse.action_at(album_x, byline.y),
                    Some(MouseAction::OpenPlayingAlbum)
                );
            }
            let footer: String = (height - STATUS_HEIGHT..height)
                .flat_map(|y| (0..width).map(move |x| (x, y)))
                .map(|pos| buf[pos].symbol())
                .collect();
            assert!(
                !footer.contains(&now.title) && !footer.contains(&now.artist),
                "duplicate footer identity: {footer}"
            );
        }
    }
}

#[test]
fn footer_controls_keep_clear_side_and_bottom_margins() {
    let mut now = preview_now();
    now.tab = Tab::Lyrics;
    let cover = Cover::solid(32, 32);
    for view in [View::Home, View::Playing] {
        for (width, height) in [(48, 18), (80, 24), (100, 36), (160, 42)] {
            let (buf, mouse) =
                drawn_screen(width, height, view, &now, &[], &ArtCache::default(), &cover);
            let outer = shell::regions(buf.area)[2];
            let content = shell::footer_content(outer);
            #[cfg(windows)]
            {
                let (r, g, b) = canvas_color(Some(&cover));
                assert_eq!(buf[(outer.x, outer.bottom() - 1)].bg, Color::Rgb(r, g, b));
                assert_eq!(
                    buf[(outer.right() - 1, outer.bottom() - 1)].bg,
                    Color::Rgb(r, g, b),
                    "the surface must fill the resized footer width"
                );
            }
            for y in outer.y..outer.bottom() {
                for x in outer.x..outer.right() {
                    if x < content.x || x >= content.right() || y >= content.bottom() {
                        assert_eq!(
                            buf[(x, y)].symbol(),
                            " ",
                            "footer text at the window edge {x},{y}"
                        );
                        assert_eq!(
                            mouse.action_at(x, y),
                            None,
                            "empty margins must not activate controls"
                        );
                    }
                }
            }
            assert_eq!(
                mouse.action_at(content.right() - 1, content.bottom() - 1),
                Some(MouseAction::OpenPlayerActions)
            );
        }
    }
}

#[test]
fn home_and_player_center_the_pause_glyph_inside_its_mouse_button() {
    let mut now = preview_now();
    now.tab = Tab::Lyrics;
    let cover = Cover::solid(32, 32);
    for view in [View::Home, View::Playing] {
        for (width, height) in [(48, 18), (80, 24), (100, 36), (161, 42)] {
            let (buf, mouse) =
                drawn_screen(width, height, view, &now, &[], &ArtCache::default(), &cover);
            let y = height - STATUS_HEIGHT + 1;
            let columns: Vec<_> = (0..width)
                .filter(|&col| mouse.action_at(col, y) == Some(MouseAction::TogglePlayback))
                .collect();
            let x = columns[0];
            let end = columns.last().unwrap() + 1;
            let button: String = (x..end).map(|col| buf[(col, y)].symbol()).collect();
            assert_eq!(
                button.trim(),
                if columns.len() > 4 { "II Space" } else { "II" }
            );
            assert_eq!(
                columns.len(),
                usize::from(end - x),
                "the button must be contiguous"
            );
            assert_eq!(mouse.action_at(x - 1, y), Some(MouseAction::PreviousTrack));
            assert_eq!(mouse.action_at(end, y), Some(MouseAction::NextTrack));
            if view == View::Home && width >= 100 {
                let row: String = (0..x - 4).map(|col| buf[(col, y)].symbol()).collect();
                assert!(
                    row.contains(&now.title),
                    "Home lost its playing song: {row}"
                );
            }
        }
    }
}

#[test]
fn complete_player_screens_retain_identity_tabs_and_controls_after_resizing() {
    let mut now = preview_now();
    let cover = Cover::from_rgb(4, 4, [180, 90, 60].repeat(16));
    let art = ArtCache::default();
    for (width, height) in [(48, 18), (64, 20), (100, 36), (160, 42)] {
        for tab in Tab::ALL {
            now.tab = tab;
            let (buf, mouse) = drawn_screen(width, height, View::Playing, &now, &[], &art, &cover);
            let text: String = buf.content.iter().map(|cell| cell.symbol()).collect();
            for label in [
                "Let It Happen",
                "Tame Impala",
                "Lyrics",
                "Related",
                "Comments",
                "1:00 / 7:48",
            ] {
                assert!(
                    text.contains(label),
                    "missing {label:?} at {width}x{height}: {text}"
                );
            }
            let footer_y = height - STATUS_HEIGHT;
            let footer = shell::footer_content(Rect::new(0, footer_y, width, STATUS_HEIGHT));
            assert_eq!(
                mouse.action_at(footer.x, footer_y),
                Some(MouseAction::SeekTo(0))
            );
            assert_eq!(
                mouse.action_at(0, footer_y),
                None,
                "the outer margin must not seek"
            );
            let starts: Vec<_> = [
                MouseAction::PreviousTrack,
                MouseAction::TogglePlayback,
                MouseAction::NextTrack,
            ]
            .into_iter()
            .map(|action| {
                (0..width)
                    .find(|&x| mouse.action_at(x, footer_y + 1) == Some(action))
                    .unwrap()
            })
            .collect();
            assert!(
                starts.windows(2).all(|pair| pair[0] < pair[1]),
                "transport controls must remain distinct"
            );
            assert_eq!(
                mouse.action_at(width - 1, footer_y),
                None,
                "the clock must not seek"
            );
            assert!(
                !text.contains('╭') && !text.contains('┌'),
                "page frames returned: {text}"
            );
        }
    }
}

#[test]
fn terminal_shrinking_below_page_minimum_keeps_rendering_safe() {
    let now = playing();
    let cover = Cover::from_rgb(4, 4, [180, 90, 60].repeat(16));
    for (width, height) in [(1, 1), (4, 3), (12, 6), (24, 10), (100, 6)] {
        let (buf, _) = drawn_screen(
            width,
            height,
            View::Playing,
            &now,
            &[],
            &ArtCache::default(),
            &cover,
        );
        assert_eq!(buf.content.len(), width as usize * height as usize);
    }
}

#[test]
fn lyrics_reading_controls_remain_clickable_after_resizing() {
    let mut now = preview_now();
    now.tab = Tab::Lyrics;
    let cover = Cover::from_rgb(4, 4, [180, 90, 60].repeat(16));
    for expanded in [false, true] {
        now.lyrics_expanded = expanded;
        for (width, height) in [(1, 1), (12, 6), (48, 18), (100, 36), (160, 42)] {
            let (buf, mouse) = drawn_screen(
                width,
                height,
                View::Playing,
                &now,
                &[],
                &ArtCache::default(),
                &cover,
            );
            for target in &mouse.targets {
                if let MouseTarget::Area(
                    rect,
                    action @ (MouseAction::ToggleLyricsExpanded | MouseAction::ToggleLyricsFollow),
                ) = target
                {
                    assert!(rect.right() <= width && rect.bottom() <= height);
                    assert_eq!(mouse.action_at(rect.x, rect.y), Some(*action));
                }
            }
            if width >= 48 {
                let text: String = buf.content.iter().map(|cell| cell.symbol()).collect();
                assert!(text.contains(if expanded { "Restore e" } else { "Expand e" }));
                assert!(text.contains("Following"));
                for action in [
                    MouseAction::ToggleLyricsExpanded,
                    MouseAction::ToggleLyricsFollow,
                ] {
                    assert!(mouse.targets.iter().any(
                        |target| matches!(target, MouseTarget::Area(_, found) if *found == action)
                    ));
                }
            }
        }
    }
}

#[test]
fn narrow_lyrics_keep_every_character_in_languages_without_spaces() {
    let text = "天地玄黃宇宙洪荒日月盈昃辰宿列張";
    let mut now = playing();
    now.tab = Tab::Lyrics;
    now.lyrics = Panel::Ready(Lyrics {
        text: text.into(),
        source: None,
        timed: Vec::new(),
    });
    for expanded in [false, true] {
        now.lyrics_expanded = expanded;
        let drawn: String = drawn_panel(&now, 12, 24)
            .into_iter()
            .skip(2)
            .flat_map(|row| {
                row.chars()
                    .filter(|ch| text.contains(*ch))
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(drawn, text);
    }
}

/// A shelf of `shape` drawn on its own, as one string per row.
fn drawn_shelf(width: u16, shelf: &Shelf, cursor: ShelfCursor, shape: CardShape) -> Vec<String> {
    let (card_width, card_height) = card_size(shape);
    let height = card_height + 1;
    let across = (width / card_width).max(1);
    let (art, mut wanted) = no_art();
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            render_shelf(
                frame,
                shelf,
                Rect::new(0, 0, width, height),
                cursor,
                (across, width / across),
                &mut Tiles {
                    shape,
                    art: &art,
                    wanted: &mut wanted,
                    images: None,
                },
            );
        })
        .unwrap();
    let buf = terminal.backend().buffer();
    (0..height)
        .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
        .collect()
}

/// The subtitle is not one grey string. Whoever made the record is the one
/// thing on that line anybody scans a shelf for, so it is drawn a step
/// brighter than the year beside it and the bullets a step dimmer than
/// either -- which is the difference between a card and a paragraph.
#[test]
fn the_subtitle_is_styled_by_what_each_part_of_it_is() {
    let spans = detail_spans("Tame Impala • 2015", 40);
    let colours: Vec<(&str, Option<Color>)> = spans
        .iter()
        .map(|span| (span.content.as_ref(), span.style.fg))
        .collect();

    assert_eq!(
        colours,
        vec![
            ("Tame Impala", Some(Color::Gray)),
            (" • ", Some(Color::from_u32(0x0044_4444))),
            ("2015", Some(Color::DarkGray)),
        ]
    );
}

/// Narrow cards are the common case on the shapes that carry a picture, so
/// the line has to give out at a field boundary rather than halfway through
/// a separator hanging off the end of the card.
#[test]
fn a_subtitle_too_long_for_its_card_stops_at_a_field() {
    let spans = detail_spans("Tame Impala • 2015", 13);
    let written: String = spans.iter().map(|span| span.content.as_ref()).collect();

    assert_eq!(written, "Tame Impala", "no dangling separator");
    assert!(display_width(&written) <= 13);

    // And nothing at all rather than a bare bullet when there is no room.
    assert!(detail_spans("Tame Impala • 2015", 0).is_empty());
}

/// A subtitle YouTube wrote as one field is still one field: this styles
/// what it sent rather than inventing structure that is not there.
#[test]
fn a_subtitle_with_no_bullets_is_left_whole() {
    let spans = detail_spans("Mild High Club", 40);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].content.as_ref(), "Mild High Club");
    assert_eq!(spans[0].style.fg, Some(Color::Gray));
}

/// The row under the cursor is the one row the user is looking straight at,
/// so it is the one that must never be unreadable. `lift` guarantees the
/// accent's brightest *channel*, not its luminance -- a deep red sleeve
/// yields (170, 0, 0), which passes that bar and is far too dark to write
/// black on.
#[test]
fn a_highlight_picks_ink_that_can_be_read_against_its_fill() {
    let ink = |r, g, b| highlight(Color::Rgb(r, g, b)).fg;

    // Bright fills take black, as the highlight always did.
    assert_eq!(
        ink(255, 220, 90),
        Some(Color::Black),
        "a pale yellow sleeve"
    );
    assert_eq!(
        ink(170, 200, 170),
        Some(Color::Black),
        "a soft green sleeve"
    );

    // Dark fills flip, which is the case that used to be illegible.
    assert_eq!(ink(170, 0, 0), Some(Color::White), "a deep red sleeve");
    assert_eq!(ink(40, 40, 180), Some(Color::White), "a navy sleeve");

    // Every fill is legible one way or the other -- the property that
    // actually matters, stated as itself rather than as a list of cases.
    for r in (0..=255u8).step_by(15) {
        for g in (0..=255u8).step_by(15) {
            for b in (0..=255u8).step_by(15) {
                let contrast = match highlight(Color::Rgb(r, g, b)).fg {
                    Some(Color::Black) => luma(r, g, b),
                    Some(Color::White) => 255 - luma(r, g, b),
                    other => panic!("unexpected ink {other:?}"),
                };
                assert!(
                    contrast >= 115,
                    "({r},{g},{b}) leaves only {contrast} of contrast"
                );
            }
        }
    }
}

/// A terminal colour is not an RGB triple and has no luminance to measure,
/// so it keeps the black ink the highlight has always used.
#[test]
fn a_named_fill_keeps_the_ink_it_always_had() {
    assert_eq!(highlight(Color::Cyan).fg, Some(Color::Black));
    assert_eq!(highlight(Color::Cyan).bg, Some(Color::Cyan));
}

/// The gallery card exists to be flush: its sleeve should reach both
/// borders rather than sit in margins, which is the whole reason its width
/// is tied to its height rather than chosen for how many fit across.
#[test]
fn a_gallery_sleeve_fills_the_card_it_is_drawn_in() {
    let (width, height) = GALLERY_CARD;
    let picture_width = width - 2; // one focus rail and one gutter
    let picture_rows = height - CARD_TEXT_ROWS;

    assert_eq!(
        picture_rows * 2,
        picture_width,
        "a square sleeve of {picture_rows} rows is {} columns wide, \
             which must match the {picture_width} columns beside the focus rail",
        picture_rows * 2
    );
}

fn quick_picks() -> Shelf {
    Shelf {
        title: "Quick picks".to_string(),
        cards: vec![
            song("Is It True", "Tame Impala"),
            song("Nice Boys", "TEMPOREX"),
            song("Homage", "Mild High Club"),
            song("Feather", "Nujabes"),
            song("Shake", "Yeek"),
        ],
    }
}

fn cursor(selected: usize, offset: usize) -> ShelfCursor {
    ShelfCursor {
        focused: true,
        selected,
        offset,
    }
}

#[test]
fn a_shelf_draws_its_heading_and_as_many_cards_as_fit() {
    // 80 columns holds two 32-wide cards with sixteen left over, which the
    // pair stretch into rather than leave as a margin.
    let rows = drawn_shelf(80, &quick_picks(), cursor(0, 0), CardShape::Text);
    assert!(rows[0].contains("Quick picks"), "{rows:#?}");

    let text = rows.join("\n");
    assert!(text.contains("Is It True"));
    assert!(text.contains("Nice Boys"));
    // The third card is off the end of the row, not squeezed onto it.
    assert!(!text.contains("Homage"), "{rows:#?}");
}

/// The counter is the only thing on screen that says a shelf runs past the
/// edge of the window, so a shelf scrolled off its first card must still
/// say where in it the cursor is.
#[test]
fn a_focused_shelf_says_how_far_along_it_the_cursor_is() {
    let rows = drawn_shelf(80, &quick_picks(), cursor(4, 2), CardShape::Text);
    assert!(rows[0].contains("5 of 5"), "{:?}", rows[0]);

    // Unfocused, there is no cursor in it to report.
    let unfocused = ShelfCursor {
        focused: false,
        selected: 0,
        offset: 0,
    };
    assert!(!drawn_shelf(80, &quick_picks(), unfocused, CardShape::Text)[0].contains("of 5"));
}

/// Enter does two different things depending on the card, and this is the
/// only thing on the page that says which.
#[test]
fn a_card_says_whether_it_plays_or_opens() {
    let shelf = Shelf {
        title: "Albums for you".to_string(),
        cards: vec![song("Creep", "Radiohead"), album("Currents", "Tame Impala")],
    };
    let text = drawn_shelf(80, &shelf, cursor(0, 0), CardShape::Text).join("\n");
    assert!(text.contains("play"), "{text}");
    assert!(text.contains("open"), "{text}");
}

/// When YouTube did say what a card is, the badge says it instead -- which
/// is more than the glyph could, and is the line the eye lands on after the
/// title.
#[test]
fn a_card_wears_the_type_youtube_gave_it() {
    let shelf = Shelf {
        title: "Albums for you".to_string(),
        cards: vec![
            song("Creep", "Song • Radiohead"),
            album("Currents", "Album • Tame Impala"),
        ],
    };
    let text = drawn_shelf(80, &shelf, cursor(0, 0), CardShape::Text).join("\n");
    assert!(text.contains("song"), "{text}");
    assert!(text.contains("album"), "{text}");
    // The marker is drawn as the badge, so it must not also be repeated in
    // the words beside it.
    assert!(!text.contains("Song •"), "{text}");
    assert!(text.contains("Radiohead"), "{text}");
}

/// A length is drawn when the feed gave one and simply absent when it did
/// not -- an album has no single duration, and a made-up one is worse than
/// none.
#[test]
fn a_card_shows_a_length_only_when_it_has_one() {
    let mut timed = song("Creep", "Song • Radiohead");
    timed.duration = Some(Duration::from_secs(238));
    let shelf = Shelf {
        title: "Quick picks".to_string(),
        cards: vec![timed, album("Currents", "Album • Tame Impala")],
    };
    let text = drawn_shelf(80, &shelf, cursor(0, 0), CardShape::Text).join("\n");
    assert!(text.contains("3:58"), "{text}");
    assert_eq!(
        text.matches(':').count(),
        1,
        "only the song is timed: {text}"
    );
}

/// The selection is what the whole page is navigated by, so it has to be
/// visible -- and on exactly one card.
///
/// Checked as "one card differs from the rest" rather than against a named
/// colour, because the selected card is now drawn in its own sleeve's
/// accent: there is no one colour a highlight is, which is the point of it.
#[test]
fn exactly_one_card_is_highlighted() {
    let shelf = quick_picks();
    let shelves = vec![shelf.clone()];
    let art = stub_art(&shelves);
    let mut wanted = Vec::new();
    let height = card_size(CardShape::Text).1 + 1;

    let mut terminal = Terminal::new(TestBackend::new(80, height)).unwrap();
    terminal
        .draw(|frame| {
            render_shelf(
                frame,
                &shelf,
                Rect::new(0, 0, 80, height),
                cursor(1, 0),
                (3, 26),
                &mut Tiles {
                    shape: CardShape::Text,
                    art: &art,
                    wanted: &mut wanted,
                    images: None,
                },
            );
        })
        .unwrap();
    let buf = terminal.backend().buffer();

    // Top-left corner of each of the three drawn cards, on the row under
    // the heading.
    let corners: Vec<Color> = (0..3).map(|card| buf[(card * 26, 1)].fg).collect();
    assert_eq!(corners[0], Color::DarkGray, "{corners:?}");
    assert_eq!(corners[2], Color::DarkGray, "{corners:?}");
    assert_ne!(
        corners[1],
        Color::DarkGray,
        "the selected card has to stand out: {corners:?}"
    );
}

/// The card under the cursor takes its frame from its own sleeve, so that a
/// shelf of records is a shelf of colours rather than a shelf of cyan.
#[test]
fn a_selected_card_is_framed_in_its_own_artwork() {
    let shelf = quick_picks();
    let shelves = vec![shelf.clone()];
    let art = stub_art(&shelves);
    let expected = art
        .get(shelf.cards[1].art_key())
        .expect("the stub cache holds every card")
        .accent;

    let mut wanted = Vec::new();
    let height = card_size(CardShape::Text).1 + 1;
    let mut terminal = Terminal::new(TestBackend::new(80, height)).unwrap();
    terminal
        .draw(|frame| {
            render_shelf(
                frame,
                &shelf,
                Rect::new(0, 0, 80, height),
                cursor(1, 0),
                (3, 26),
                &mut Tiles {
                    shape: CardShape::Text,
                    art: &art,
                    wanted: &mut wanted,
                    images: None,
                },
            );
        })
        .unwrap();

    let buf = terminal.backend().buffer();
    assert_eq!(
        buf[(26, 1)].fg,
        Color::Rgb(expected.0, expected.1, expected.2)
    );
}

/// The lead shelf always gets the biggest complete card the window fits.
#[test]
fn the_window_picks_the_biggest_cards_that_fit() {
    let at = |height| plan_home(Rect::new(0, 0, 100, height)).max_shape;

    assert_eq!(at(80), CardShape::Gallery, "room for a page of galleries");
    // Two shelves of gallery cards need 41 rows and two of posters 33, so
    // this is the window that has to step down one rather than two -- the
    // case that would regress if the new shape were merely the old one made
    // taller.
    assert_eq!(at(36), CardShape::Gallery);
    assert_eq!(at(20), CardShape::Gallery);
    assert_eq!(at(16), CardShape::Poster);
    assert_eq!(at(8), CardShape::Tile);
    // Narrower than one shelf of anything. The pane refuses to draw the
    // feed at all well before this, so all that matters is that it settles
    // rather than looping or panicking.
    assert_eq!(at(1), CardShape::Text);
}

#[test]
fn sections_receive_distinct_layouts() {
    let shelves = [
        quick_picks(),
        Shelf {
            title: "From your listening".to_string(),
            cards: quick_picks().cards,
        },
        Shelf {
            title: "Albums for you".to_string(),
            cards: vec![
                album("Currents", "Tame Impala"),
                album("Discovery", "Daft Punk"),
                album("Modal Soul", "Nujabes"),
            ],
        },
    ];

    assert_eq!(
        section_shape(&shelves[0], 0, CardShape::Gallery),
        CardShape::Gallery
    );
    assert_eq!(
        section_shape(&shelves[1], 1, CardShape::Gallery),
        CardShape::Tile
    );
    assert_eq!(
        section_shape(&shelves[2], 2, CardShape::Gallery),
        CardShape::Poster
    );
    assert_eq!(
        section_shape(&shelves[0], 0, CardShape::Tile),
        CardShape::Tile,
        "a short terminal must clamp every section"
    );
}

#[test]
fn home_leads_with_large_covers_and_keeps_a_complete_song_grid_below() {
    let shelves = [
        quick_picks(),
        Shelf {
            title: "From your listening".to_string(),
            cards: quick_picks().cards,
        },
        Shelf {
            title: "Albums for you".to_string(),
            cards: vec![album("Currents", "Tame Impala")],
        },
    ];

    // Defend cover size and complete song cards instead of crowding the fold.
    let layouts = shelf_layouts(&shelves, Rect::new(0, 0, 100, 34), 0, CardShape::Gallery);

    assert_eq!(layouts.len(), 2);
    assert_eq!(layouts[0].shape, CardShape::Gallery);
    assert_eq!(layouts[1].shape, CardShape::Tile);
    assert_eq!(layouts[1].rows, 2);
    assert!(layouts[1].area.bottom() <= 34);
}

/// Short windows keep a complete browsing shelf.
#[test]
fn a_short_home_window_keeps_room_for_artwork() {
    let tight = plan_home(Rect::new(0, 0, 100, shelf_height(CardShape::Tile) - 1));
    assert_eq!(tight.max_shape, CardShape::Tile);
}

/// Only the cards actually on screen have their pictures fetched. This is
/// what keeps a twelve-shelf feed from being three hundred requests.
#[test]
fn only_the_visible_cards_ask_for_artwork() {
    let shelves = vec![quick_picks(), quick_picks(), quick_picks()];
    let (art, mut wanted) = no_art();
    // Two cards across and one shelf tall, so eleven of the fifteen cards
    // are off screen.
    let (width, height) = (2 * TEXT_CARD.0, TEXT_CARD.1 + 1);

    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            render_feed(
                frame,
                &shelves,
                &shelf_layouts(&shelves, Rect::new(0, 0, width, height), 0, CardShape::Text),
                HomeCursor { shelf: 0, card: 0 },
                &[0, 0, 0],
                Tiles {
                    shape: CardShape::Text,
                    art: &art,
                    wanted: &mut wanted,
                    images: None,
                },
            );
        })
        .unwrap();

    assert_eq!(wanted.len(), 2, "asked for {wanted:?}");
    let keys: Vec<&str> = wanted.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys, ["Is It True", "Nice Boys"]);
}

/// A picture already in hand remains recent without being downloaded again.
#[test]
fn artwork_already_held_is_kept_recent_without_a_new_request() {
    let mut shelves = vec![quick_picks()];
    let mut art = stub_art(&shelves);
    for card in &mut shelves[0].cards {
        card.art = None;
    }
    let mut wanted = Vec::new();
    let (width, height) = (2 * TEXT_CARD.0, TEXT_CARD.1 + 1);

    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            render_feed(
                frame,
                &shelves,
                &shelf_layouts(&shelves, Rect::new(0, 0, width, height), 0, CardShape::Text),
                HomeCursor { shelf: 0, card: 0 },
                &[0],
                Tiles {
                    shape: CardShape::Text,
                    art: &art,
                    wanted: &mut wanted,
                    images: None,
                },
            );
        })
        .unwrap();

    assert_eq!(wanted.len(), 2, "visible art was not kept recent");
    assert!(wanted.iter().all(|(_, url)| url.is_none()));
    assert!(
        wanted.into_iter().all(|(key, _)| !art.want(&key)),
        "cached artwork was requested again"
    );
}

/// A tile fills its box corner to corner whatever shape the source is, so
/// that a shelf of 16:9 video thumbnails and square sleeves is still a
/// shelf of matching squares.
#[test]
fn a_tile_is_cropped_to_fill_rather_than_letterboxed() {
    // Sixteen by nine into a square tile: every row survives, the sides go.
    let wide = Cover::solid(160, 90);
    let crop = Crop::fill(&wide, 20, 20);
    assert_eq!(crop.height, 90, "no rows are dropped");
    assert_eq!(crop.width, 90, "the sides are");
    assert_eq!(crop.x, 35, "and what is kept is the middle");

    // A square into a square is left alone.
    let square = Cover::solid(64, 64);
    assert_eq!(
        Crop::fill(&square, 20, 20),
        Crop {
            x: 0,
            y: 0,
            width: 64,
            height: 64
        }
    );

    // A tall source into a wide tile loses its top and bottom instead.
    let tall = Cover::solid(50, 200);
    let crop = Crop::fill(&tall, 20, 10);
    assert_eq!((crop.width, crop.height), (50, 25));
    assert_eq!(crop.y, 87);
}

/// Degenerate sizes reach this from a card squeezed to nothing, and it has
/// to return a usable rect rather than dividing by zero.
#[test]
fn cropping_survives_an_empty_tile() {
    let crop = Crop::fill(&Cover::solid(64, 64), 0, 0);
    assert!(crop.width >= 1 && crop.height >= 1);
}

/// Titles are what the page is read by, and a card is narrow. Overflowing
/// one would run its title into the card beside it.
#[test]
fn a_long_title_is_truncated_inside_its_card() {
    let shelf = Shelf {
        title: "New releases".to_string(),
        cards: vec![song(
            "Get Lucky (feat. Pharrell Williams and Nile Rodgers)",
            "Daft Punk, Pharrell Williams & Nile Rodgers • 6:10",
        )],
    };
    for width in [40, 60, 80, 120] {
        for row in drawn_shelf(width, &shelf, cursor(0, 0), CardShape::Text) {
            assert_eq!(
                display_width(&row),
                width as usize,
                "a {width}-wide shelf drew {row:?}"
            );
        }
    }
}

/// A shelf heading is one row and truncates rather than wrapping, so a long
/// name must not push the counter off the end of it.
#[test]
fn a_heading_fits_its_row_whatever_the_shelf_is_called() {
    let shelf = Shelf {
        title: "Recommended because you listened to something with a very long name".to_string(),
        cards: vec![song("a", "b"), song("c", "d")],
    };
    for width in 4..120 {
        assert!(
            heading_line(&shelf, cursor(1, 0), width).width() <= width,
            "heading overflows a {width}-wide shelf"
        );
    }
}

/// Everything the sign-in panel drew, as one string per row.
///
/// Read back from a real draw rather than asserted on the `Line`s going in:
/// the panel is laid out with nested areas and a block inside a block, and
/// only the finished buffer says whether any of it landed where it was
/// meant to -- or fit at all.
fn drawn(width: u16, height: u16, phase: &SignIn) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render_sign_in(frame, phase)).unwrap();
    let buf = terminal.backend().buffer();
    (0..height)
        .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
        .collect()
}

#[test]
fn the_panel_keeps_one_title_across_every_phase() {
    // Every phase is one flow, not a new window. A title that changed
    // under the user would read as the panel having been replaced.
    for phase in [
        SignIn::Failed {
            reason: "sign-in was declined".to_string(),
        },
        SignIn::Music {
            started: Instant::now(),
            recovering: false,
        },
    ] {
        let rows = drawn(70, 24, &phase).join("\n");
        assert!(rows.contains("YouTube Music session"), "{rows}");
    }
}

/// The thread behind a failed sign-in is gone, so nothing on screen will
/// change again on its own -- the panel has to offer the way out.
#[test]
fn a_failed_sign_in_states_the_reason_and_offers_the_retry() {
    let phase = SignIn::Failed {
        reason: "sign-in was declined".to_string(),
    };
    let rows = drawn(70, 24, &phase).join("\n");
    assert!(rows.contains("sign-in was declined"), "{rows}");
    assert!(rows.contains("M retry sign-in"), "{rows}");
}

#[test]
fn recovery_does_not_claim_a_sign_in_window_is_already_open() {
    let phase = SignIn::Music {
        started: Instant::now(),
        recovering: true,
    };
    let rows = drawn(70, 24, &phase).join("\n");
    assert!(rows.contains("Renewing your saved"), "{rows}");
    assert!(rows.contains("opens only if needed"), "{rows}");
    assert!(!rows.contains("Complete sign-in"), "{rows}");
}

/// Every phase is sized to its own content and keeps its exit visible.
#[test]
fn no_phase_clips_the_key_that_dismisses_it() {
    for phase in [
        SignIn::Failed {
            reason: "sign-in was declined".to_string(),
        },
        SignIn::Music {
            started: Instant::now(),
            recovering: false,
        },
    ] {
        let rows = drawn(70, 24, &phase).join("\n");
        assert!(rows.contains("Esc"), "{rows}");
    }
}

/// A reason long enough to stretch the panel past the other phases' width
/// is wrapped instead, so the flow keeps one shape throughout.
#[test]
fn a_long_failure_reason_wraps_rather_than_widening_the_panel() {
    let phase = SignIn::Failed {
        reason: "the saved session is no longer valid. Sign in to YouTube Music \
                     in the MTUI window, then press M to try again"
            .to_string(),
    };
    let rows = drawn(120, 24, &phase);
    let panel = rows
        .iter()
        .find(|row| row.contains("YouTube Music session"))
        .expect("the panel is drawn");
    assert!(
        display_width(panel.trim()) <= SIGN_IN_MAX_WIDTH as usize,
        "{:?} is {} columns",
        panel.trim(),
        display_width(panel.trim())
    );
    assert!(
        rows.iter().filter(|r| r.contains('│')).count() > 4,
        "wrapped onto several rows"
    );
}

#[test]
fn the_ellipsis_cycles_so_a_long_wait_does_not_look_frozen() {
    assert_eq!(ellipsis(Duration::ZERO), ".");
    assert_eq!(ellipsis(Duration::from_millis(500)), "..");
    assert_eq!(ellipsis(Duration::from_millis(900)), "...");
    assert_eq!(ellipsis(Duration::from_millis(1300)), ".");
}

#[test]
fn wrapping_breaks_at_spaces_but_never_inside_a_word() {
    assert_eq!(wrap("one two three", 8), ["one two", "three"]);
    // A word over the width overhangs rather than splitting: the long words
    // in these messages are URLs, and half a URL is useless.
    assert_eq!(
        wrap("see https://myaccount.google.com/permissions", 10),
        ["see", "https://myaccount.google.com/permissions"]
    );
    assert_eq!(wrap("", 10), [""]);
}

#[test]
fn artist_prose_hard_wraps_languages_without_spaces() {
    let lines = bounded_wrap("日本語の説明", 4);
    assert_eq!(lines, ["日本", "語の", "説明"]);
    assert!(lines.iter().all(|line| display_width(line) <= 4));

    assert_eq!(bounded_wrap("か\u{3099}き", 2), ["か\u{3099}", "き"]);
    assert_eq!(bounded_wrap("👩‍💻X", 2), ["👩‍💻", "X"]);
}

#[test]
fn every_hint_line_fits_the_column_reserved_for_it() {
    // The hints column truncates rather than wrapping, so a hint that grew
    // past it would be silently cut mid-word.
    for hint in [HINTS_EDITING, HINTS_BROWSE, HINTS_AWAY, HINTS_PLAYING] {
        assert!(
            display_width(hint) <= HINTS_WIDTH as usize,
            "{hint:?} is {} columns, over the {HINTS_WIDTH} reserved",
            display_width(hint)
        );
    }
}

#[test]
fn browse_hints_route_to_the_shared_command_surfaces() {
    assert_eq!(HINTS_BROWSE, ". song actions  ? help");
    assert_eq!(HINTS_AWAY, ". song actions  ? help");
    assert_eq!(HINTS_PLAYING, "Tab panels  . song actions  ? help");
}

#[test]
fn the_editing_hint_offers_no_key_the_search_box_would_swallow() {
    // Every printable key is text while typing, so a bare letter here would
    // name a key that inserts that letter. Only Enter, Esc and a Ctrl-chord
    // survive the search box.
    let advertised_keys = HINTS_EDITING
        .split("  ")
        .map(|hint| hint.split_whitespace().next().unwrap_or(""))
        .collect::<Vec<_>>();
    assert_eq!(advertised_keys, ["Enter", "Esc"]);
}

#[test]
fn modals_are_centred_and_never_escape_the_window() {
    let window = Rect::new(0, 0, 80, 24);
    let area = centred(window, 40, 10);
    assert_eq!((area.x, area.y, area.width, area.height), (20, 7, 40, 10));

    // A modal larger than the window is clamped, not drawn off-screen.
    let huge = centred(Rect::new(0, 0, 30, 6), 56, 12);
    assert_eq!((huge.x, huge.y, huge.width, huge.height), (0, 0, 30, 6));
    let narrow_menu = centred(Rect::new(4, 3, 30, 6), 52, 24);
    assert_eq!(
        (
            narrow_menu.x,
            narrow_menu.y,
            narrow_menu.width,
            narrow_menu.height,
        ),
        (4, 3, 30, 6)
    );
}

#[test]
fn the_header_lines_up_with_the_rows() {
    // Swept, because the header and the rows are built by separate code
    // paths off the same widths -- exactly the arrangement that drifts.
    for width in (DURATION_WIDTH + 2)..200 {
        let (title, artist, album) = columns(width);
        assert_eq!(
            header_line(title, artist, album).width(),
            width,
            "header does not fill a {width}-wide row"
        );
    }
}

#[test]
fn the_header_drops_the_same_columns_the_rows_do() {
    // Narrow enough that only the title survives: the labels for the
    // dropped columns must not be drawn anyway.
    let (title, artist, album) = columns(40);
    let header = header_line(title, artist, album);
    let text = header
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<String>();
    assert!(text.contains("TITLE"));
    assert!(text.contains("DURATION"));
    assert!(!text.contains("ARTIST"), "artist label survived its column");
    assert!(!text.contains("ALBUM"), "album label survived its column");
}

#[test]
fn columns_fill_every_row_they_can() {
    // Swept rather than sampled: the widths where a column drops out are
    // exactly where an off-by-one would leave the highlight short.
    for width in (DURATION_WIDTH + 2)..200 {
        assert_fills(width);
    }
}

#[test]
fn a_cell_is_exactly_its_width_and_keeps_a_gap() {
    assert_eq!(display_width(&cell("Daft Punk", 12)), 12);
    assert_eq!(display_width(&cell("", 12)), 12);
    // Too long: truncated with room left for the separating space.
    let packed = cell("Random Access Memories", 12);
    assert_eq!(display_width(&packed), 12);
    assert!(packed.ends_with(' '), "columns must not run together");
}

/// Wide glyphs are the case that silently broke every column: one
/// character, two terminal cells.
#[test]
fn a_cell_measures_wide_glyphs_in_columns_not_characters() {
    for width in 1..24 {
        for text in [
            "日本語のタイトル",
            "아티스트",
            "ASCII title",
            "mixed 日本語 text",
            "",
        ] {
            assert_eq!(
                display_width(&cell(text, width)),
                width,
                "{text:?} at width {width}"
            );
        }
    }
}

#[test]
fn truncate_measures_in_columns() {
    // Four CJK glyphs are eight columns, so a limit of eight fits exactly
    // and must not be cut.
    assert_eq!(truncate("日本語だ", 8), "日本語だ");
    // Seven columns cannot hold it: three glyphs plus an ellipsis is
    // seven, which is the most that fits.
    assert_eq!(display_width(&truncate("日本語だ", 7)), 7);
}

#[test]
fn truncate_leaves_short_strings_alone() {
    assert_eq!(truncate("abc", 10), "abc");
    assert_eq!(truncate("abc", 3), "abc");
}

#[test]
fn truncate_adds_ellipsis_when_cutting() {
    assert_eq!(truncate("abcdef", 4), "abc…");
    assert_eq!(truncate("abc", 0), "");
}

#[test]
fn truncate_respects_char_boundaries() {
    // Multi-byte characters must not be sliced mid-codepoint. Each of
    // these glyphs is two terminal columns, so a four-column budget holds
    // one of them plus the ellipsis -- not three, which is what counting
    // characters would have given.
    let s = "日本語のタイトル";
    assert_eq!(truncate(s, 4), "日…");
    assert_eq!(truncate("café", 10), "café");
}

#[test]
fn truncate_never_exceeds_its_budget() {
    // The invariant the columns depend on, swept over mixed-width text.
    for text in ["日本語のタイトル", "mixed 日本語 text", "ASCII", "아티스트"] {
        for max in 0..20 {
            assert!(
                display_width(&truncate(text, max)) <= max,
                "{text:?} overflowed a {max}-column budget"
            );
        }
    }
}

#[test]
fn marquee_pauses_scrolls_and_returns() {
    let title = "abcdefgh";
    assert_eq!(marquee_at(title, 4, 0), "abcd");
    assert_eq!(marquee_at(title, 4, MARQUEE_HOLD - 1), "abcd");
    assert_eq!(marquee_at(title, 4, MARQUEE_HOLD), "bcde");
    assert_eq!(marquee_at(title, 4, MARQUEE_HOLD + 3), "efgh");
    assert_eq!(
        marquee_at(title, 4, MARQUEE_HOLD + 4 + MARQUEE_HOLD),
        "defg"
    );
}

#[test]
fn marquee_never_splits_a_wide_character() {
    for tick in 0..30 {
        let shown = marquee_at("A日本語Z", 4, tick);
        assert!(display_width(&shown) <= 4, "{shown:?} overflowed");
        assert!(std::str::from_utf8(shown.as_bytes()).is_ok());
    }
}

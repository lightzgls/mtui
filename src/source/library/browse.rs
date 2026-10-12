//! Account Library pages. Only collection contents are parsed, never menu suggestions.

use super::super::account::Session;
use super::super::home::{Card, Target};
use super::super::{BrowseEndpoint, Track, account, home, http::Http};
use anyhow::{Result, bail};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Category {
    #[default]
    All,
    Playlists,
    Songs,
    Albums,
    Artists,
    Subscriptions,
}

impl Category {
    pub const ALL: [Self; 6] = [
        Self::All,
        Self::Playlists,
        Self::Songs,
        Self::Albums,
        Self::Artists,
        Self::Subscriptions,
    ];

    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|category| *category == self)
            .unwrap_or(0)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Playlists => "Playlists",
            Self::Songs => "Songs",
            Self::Albums => "Albums",
            Self::Artists => "Artists",
            Self::Subscriptions => "Subscriptions",
        }
    }

    pub fn endpoint(self) -> BrowseEndpoint {
        BrowseEndpoint::new(match self {
            Self::All => "FEmusic_library_landing",
            Self::Playlists => "FEmusic_liked_playlists",
            Self::Songs => "FEmusic_liked_videos",
            Self::Albums => "FEmusic_liked_albums",
            Self::Artists => "FEmusic_library_corpus_track_artists",
            Self::Subscriptions => "FEmusic_library_corpus_artists",
        })
    }

    pub fn from_endpoint(endpoint: &BrowseEndpoint) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|category| category.endpoint().browse_id == endpoint.browse_id)
    }
}

#[derive(Debug, Clone)]
pub struct Item {
    pub card: Card,
    pub track: Option<Track>,
    pub available: bool,
}

#[derive(Debug, Clone)]
pub struct Sort {
    pub label: String,
    pub endpoint: BrowseEndpoint,
    pub selected: bool,
    pub reload: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Page {
    pub items: Vec<Item>,
    pub continuation: Option<String>,
    pub routes: Vec<(Category, BrowseEndpoint)>,
    pub sorts: Vec<Sort>,
    pub account: Option<String>,
}

pub fn fetch(
    http: &Http,
    session: &Session,
    category: Category,
    endpoint: &BrowseEndpoint,
    token: Option<&str>,
    read_account: bool,
) -> Result<Page> {
    let body = if let Some(token) = token {
        json!({"continuation": token})
    } else {
        let mut body = json!({"browseId": endpoint.browse_id});
        if let Some(params) = &endpoint.params {
            body["params"] = json!(params);
        }
        body
    };
    let json = super::post(http, session, "browse", body)?;
    let mut page = parse(&json, category)?;
    if read_account {
        // Account identity is supplementary: a menu outage must not discard a collection.
        if let Ok(menu) = super::post(http, session, "account/account_menu", json!({})) {
            page.account = account_name(&menu);
        }
    }
    session.cookies()?;
    Ok(page)
}

fn account_name(json: &Value) -> Option<String> {
    let mut headers = Vec::new();
    home::collect(json, "activeAccountHeaderRenderer", &mut headers);
    headers
        .into_iter()
        .find_map(|header| text(&header["accountName"]))
}

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value["simpleText"].as_str().map(str::to_owned))
        .or_else(|| home::runs_text(&value["runs"]))
        .filter(|text| !text.trim().is_empty())
}

fn route(value: &Value) -> Option<BrowseEndpoint> {
    let endpoint = value.get("browseEndpoint")?;
    Some(BrowseEndpoint {
        browse_id: endpoint["browseId"].as_str()?.to_owned(),
        params: endpoint["params"].as_str().map(str::to_owned),
    })
}

fn command_route(value: &Value) -> Option<BrowseEndpoint> {
    route(value).or_else(|| {
        value
            .pointer("/commandExecutorCommand/commands")?
            .as_array()?
            .iter()
            .find_map(command_route)
    })
}

pub(crate) fn parse(json: &Value, category: Category) -> Result<Page> {
    super::validate_response(json)?;
    if account::logged_in(json) == Some(false) {
        bail!("Sign in to YouTube Music to open your Library.");
    }
    let mut page = Page::default();
    let mut recognized = false;
    if let Some(contents) = json.get("continuationContents") {
        contents_into(contents, category, &mut page, &mut recognized);
    } else if let Some(actions) = json
        .get("onResponseReceivedActions")
        .or_else(|| json.get("onResponseReceivedCommands"))
    {
        for action in actions.as_array().into_iter().flatten() {
            if let Some(items) = action
                .pointer("/appendContinuationItemsAction/continuationItems")
                .or_else(|| action.pointer("/reloadContinuationItemsCommand/continuationItems"))
                .or_else(|| action.pointer("/reloadContinuationItemsAction/continuationItems"))
            {
                recognized = true;
                contents_into(items, category, &mut page, &mut recognized);
            }
        }
    } else if let Some(contents) = json.get("contents") {
        contents_into(contents, category, &mut page, &mut recognized);
    }
    let mut messages = Vec::new();
    home::collect(json, "messageRenderer", &mut messages);
    let message = messages
        .into_iter()
        .find_map(|message| text(&message["text"]));
    if !recognized {
        bail!(
            "{}",
            message.unwrap_or_else(|| {
                "YouTube Music returned an unrecognized Library page. Press r to retry.".into()
            })
        );
    }
    // Stable IDs deduplicate repeated continuation entries without guessing membership.
    let mut seen = std::collections::HashSet::new();
    page.items
        .retain(|item| seen.insert(item.card.art_key().to_owned()));
    controls(json, category, &mut page);
    Ok(page)
}

fn contents_into(value: &Value, category: Category, page: &mut Page, recognized: &mut bool) {
    if let Some(items) = value.as_array() {
        for item in items {
            contents_into(item, category, page, recognized);
        }
        return;
    }
    if value.get("musicTwoRowItemRenderer").is_some()
        || value.get("musicResponsiveListItemRenderer").is_some()
    {
        if let Some(item) = parse_item(value, category) {
            page.items.push(item);
        }
        return;
    }
    if let Some(renderer) = value.get("continuationItemRenderer") {
        let mut tokens = Vec::new();
        home::collect(renderer, "continuationCommand", &mut tokens);
        page.continuation = tokens
            .into_iter()
            .find_map(|token| token["token"].as_str().map(str::to_owned));
        return;
    }
    for renderer in [
        "singleColumnBrowseResultsRenderer",
        "twoColumnBrowseResultsRenderer",
    ] {
        if let Some(tabs) = value
            .pointer(&format!("/{renderer}/tabs"))
            .and_then(Value::as_array)
        {
            let tab = tabs
                .iter()
                .find(|tab| {
                    tab.pointer("/tabRenderer/selected")
                        .and_then(Value::as_bool)
                        == Some(true)
                })
                .or_else(|| tabs.first());
            if let Some(content) = tab.and_then(|tab| tab.pointer("/tabRenderer/content")) {
                contents_into(content, category, page, recognized);
            }
            return;
        }
    }
    for key in [
        "gridRenderer",
        "musicGridRenderer",
        "musicShelfRenderer",
        "musicPlaylistShelfRenderer",
        "musicCarouselShelfRenderer",
        "sectionListRenderer",
        "itemSectionRenderer",
        "gridContinuation",
        "musicShelfContinuation",
        "musicPlaylistShelfContinuation",
        "sectionListContinuation",
    ] {
        if let Some(container) = value.get(key) {
            if !matches!(key, "sectionListRenderer" | "itemSectionRenderer") {
                *recognized = true;
            }
            if let Some(items) = container.get("items").or_else(|| container.get("contents")) {
                contents_into(items, category, page, recognized);
            }
            if page.continuation.is_none() {
                page.continuation = container
                    .get("continuations")
                    .and_then(Value::as_array)
                    .and_then(|tokens| {
                        tokens.iter().find_map(|token| {
                            token
                                .pointer("/nextContinuationData/continuation")
                                .or_else(|| token.pointer("/reloadContinuationData/continuation"))?
                                .as_str()
                                .map(str::to_owned)
                        })
                    });
            }
        }
    }
}

fn parse_item(value: &Value, category: Category) -> Option<Item> {
    let mut card = home::parse_card(value).or_else(|| {
        // Some library album/playlist cards place their route on the title run.
        let renderer = value.get("musicTwoRowItemRenderer")?;
        let endpoint = renderer
            .pointer("/title/runs")?
            .as_array()?
            .iter()
            .find_map(|run| run.get("navigationEndpoint"))?;
        let mut corrected = renderer.clone();
        corrected["navigationEndpoint"] = endpoint.clone();
        home::parse_card(&json!({"musicTwoRowItemRenderer": corrected}))
    })?;
    if matches!(card.kind(), Some("Podcast" | "Episode")) {
        return None;
    }
    let row = &value["musicResponsiveListItemRenderer"];
    let available = row["musicItemRendererDisplayPolicy"].as_str()
        != Some("MUSIC_ITEM_RENDERER_DISPLAY_POLICY_GREY_OUT");
    let track = if matches!(card.target, Target::Play { .. }) {
        home::parse_row(row).or_else(|| card.track())
    } else {
        None
    };
    let kind = match &card.target {
        Target::Play { .. } => "Song",
        Target::Artist { .. } => "Artist",
        Target::Open { endpoint } if endpoint.browse_id.starts_with("MPRE") => "Album",
        Target::Open { endpoint }
            if endpoint.browse_id.starts_with("VL")
                && endpoint.browse_id != "VLSE"
                && endpoint.browse_id != "VLRDPN" =>
        {
            "Playlist"
        }
        _ => return None, // Podcasts, uploads and profile collections are outside this Library.
    };
    if category != Category::All
        && match category {
            Category::Songs => kind != "Song",
            Category::Albums => kind != "Album",
            Category::Playlists => kind != "Playlist",
            Category::Artists | Category::Subscriptions => kind != "Artist",
            Category::All => false,
        }
    {
        return None;
    }
    // Library artists navigate to their saved tracks; subscriptions retain public artist pages.
    if let Target::Artist { artist } = &card.target
        && artist.endpoint.browse_id.starts_with("MPLA")
    {
        card.target = Target::Open {
            endpoint: artist.endpoint.clone(),
        };
    }
    if card.kind().is_none() {
        card.subtitle = if card.subtitle.is_empty() {
            kind.into()
        } else {
            format!("{kind} • {}", card.subtitle)
        };
    }
    Some(Item {
        card,
        track,
        available,
    })
}

fn controls(json: &Value, category: Category, page: &mut Page) {
    let mut chips = Vec::new();
    home::collect(json, "chipCloudChipRenderer", &mut chips);
    for chip in chips {
        if let Some(endpoint) = chip.get("navigationEndpoint").and_then(command_route)
            && let Some(category) = Category::from_endpoint(&endpoint)
        {
            page.routes.push((category, endpoint));
        }
    }
    // Use the provider's sort commands, including their full filter parameters.
    let mut entries = Vec::new();
    let mut buttons = Vec::new();
    home::collect(json, "musicSortFilterButtonRenderer", &mut buttons);
    let current_label = buttons.iter().find_map(|button| text(&button["title"]));
    home::collect(json, "dropdownItemRenderer", &mut entries);
    for button in buttons {
        home::collect(
            &button["menu"],
            "musicMultiSelectMenuItemRenderer",
            &mut entries,
        );
    }
    for entry in entries {
        let label = text(&entry["label"]).or_else(|| text(&entry["title"]));
        let command = ["onSelectCommand", "navigationEndpoint", "selectedCommand"]
            .into_iter()
            .find_map(|key| entry.get(key));
        let mut reloads = Vec::new();
        if let Some(command) = command {
            home::collect(command, "browseSectionListReloadEndpoint", &mut reloads);
        }
        let reload = reloads.into_iter().find_map(|endpoint| {
            endpoint
                .pointer("/continuation/reloadContinuationData/continuation")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
        let endpoint = command
            .and_then(command_route)
            .or_else(|| reload.as_ref().map(|_| category.endpoint()));
        if let (Some(label), Some(endpoint)) = (label, endpoint)
            && Category::from_endpoint(&endpoint).is_some()
            && !page.sorts.iter().any(|sort| sort.label == label)
        {
            let selected = entry["isSelected"]
                .as_bool()
                .or_else(|| entry["selected"].as_bool())
                .unwrap_or_else(|| current_label.as_deref() == Some(label.as_str()));
            page.sorts.push(Sort {
                label,
                endpoint,
                selected,
                reload,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str, title: &str) -> Value {
        json!({"musicTwoRowItemRenderer":{"title":{"runs":[{"text":title,"navigationEndpoint":{"browseEndpoint":{"browseId":id}}}]},"subtitle":{"runs":[{"text":"You • 12 songs"}]}}})
    }

    #[test]
    fn collections_keep_routes_counts_and_ignore_menu_recommendations() {
        let json = json!({"contents":{"singleColumnBrowseResultsRenderer":{"tabs":[{"tabRenderer":{"content":{"sectionListRenderer":{"contents":[
            {"itemSectionRenderer":{"contents":[{"gridRenderer":{"items":[card("VLmine", "Mine"), card("MPREalbum", "Album"), card("MPSPpodcast", "Podcast")]}}]}}
        ]}}}}]}}, "menu":{"gridRenderer":{"items":[card("VLsuggested", "Not saved")]}}});
        let page = parse(&json, Category::All).unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].card.art_key(), "VLmine");
        assert!(page.items[0].card.subtitle.contains("12 songs"));
        assert_eq!(parse(&json, Category::Albums).unwrap().items.len(), 1);
    }

    #[test]
    fn continuations_support_grid_shelves_and_wrapped_commands() {
        let token = json!({"continuationItemRenderer":{"continuationEndpoint":{"commandExecutorCommand":{"commands":[{"continuationCommand":{"token":"next"}}]}}}});
        let json = json!({"onResponseReceivedActions":[{"appendContinuationItemsAction":{"continuationItems":[card("VLone","One"), token]}}]});
        let page = parse(&json, Category::Playlists).unwrap();
        assert_eq!(page.continuation.as_deref(), Some("next"));
        let json = json!({"continuationContents":{"gridContinuation":{"items":[card("VLtwo","Two")],"continuations":[{"nextContinuationData":{"continuation":"third"}}]}}});
        assert_eq!(
            parse(&json, Category::Playlists)
                .unwrap()
                .continuation
                .as_deref(),
            Some("third")
        );
    }

    #[test]
    fn library_artists_open_saved_tracks_and_subscriptions_open_public_pages() {
        for id in ["MPLAUCsaved", "UCfollowed"] {
            let mut item = card(id, "Artist");
            item["musicTwoRowItemRenderer"]["navigationEndpoint"] =
                json!({"browseEndpoint":{"browseId":id,"params":"keep-me"}});
            let item = parse_item(&item, Category::Artists).unwrap();
            assert!(item.card.subtitle.starts_with("Artist"));
            match item.card.target {
                Target::Open { endpoint } => {
                    assert!(id.starts_with("MPLA"));
                    assert_eq!(endpoint.params.as_deref(), Some("keep-me"));
                }
                Target::Artist { artist } => assert_eq!(artist.endpoint.browse_id, "UCfollowed"),
                _ => panic!("artist cannot become a song"),
            }
        }
    }

    #[test]
    fn empty_and_failed_library_responses_are_distinct() {
        assert!(
            parse(
                &json!({"contents":{"gridRenderer":{"items":[]}}}),
                Category::Songs
            )
            .unwrap()
            .items
            .is_empty()
        );
        assert!(
            parse(
                &json!({"contents":{"messageRenderer":{"text":{"simpleText":"Sign in"}}}}),
                Category::Songs
            )
            .unwrap_err()
            .to_string()
            .contains("Sign in")
        );
        assert!(
            parse(
                &json!({"error":{"code":401,"message":"Unauthorized"}}),
                Category::Songs
            )
            .is_err()
        );
    }

    #[test]
    fn server_sort_and_filter_parameters_are_preserved() {
        let json = json!({"contents":{"gridRenderer":{"items":[]}},"header":{
            "chipCloudChipRenderer":{"navigationEndpoint":{"commandExecutorCommand":{"commands":[{"browseEndpoint":{"browseId":"FEmusic_liked_albums","params":"albums-filter"}}]}}},
            "dropdownItemRenderer":{"label":{"simpleText":"A–Z"},"isSelected":true,"onSelectCommand":{"browseEndpoint":{"browseId":"FEmusic_liked_albums","params":"sort-and-filter"}}}
        }});
        let page = parse(&json, Category::Albums).unwrap();
        assert_eq!(page.routes[0].1.params.as_deref(), Some("albums-filter"));
        assert_eq!(
            page.sorts[0].endpoint.params.as_deref(),
            Some("sort-and-filter")
        );
        assert!(page.sorts[0].selected);
    }

    #[test]
    fn sort_reload_reads_skip_checkbox_and_persistence_commands() {
        let json = json!({"contents":{"gridRenderer":{"items":[]}},"header":{"musicSortFilterButtonRenderer":{
            "title":{"runs":[{"text":"Recently saved"}]},"menu":{"musicMultiSelectMenuRenderer":{"options":[{"musicMultiSelectMenuItemRenderer":{
                "title":{"runs":[{"text":"Recently saved"}]},"selectedCommand":{"commandExecutorCommand":{"commands":[
                    {"musicCheckboxFormItemMutatedCommand":{"newCheckedState":true}},
                    {"browseSectionListReloadEndpoint":{"continuation":{"reloadContinuationData":{"continuation":"sort-read"}}}}
                ]}}
            }}]}}
        }}});
        let page = parse(&json, Category::All).unwrap();
        assert_eq!(page.sorts.len(), 1);
        assert_eq!(page.sorts[0].reload.as_deref(), Some("sort-read"));
        assert!(page.sorts[0].selected);
        let reload = json!({"onResponseReceivedCommands":[{"reloadContinuationItemsCommand":{"continuationItems":[{"gridRenderer":{"items":[card("VLsorted","Sorted")]}}]}}]});
        assert_eq!(parse(&reload, Category::All).unwrap().items.len(), 1);
    }

    #[test]
    #[ignore = "read-only signed-in YouTube Music Library probe"]
    fn live_library_collections() {
        let http = Http::new().unwrap();
        let session = Session::current().unwrap();
        for category in Category::ALL {
            let page = fetch(
                &http,
                &session,
                category,
                &category.endpoint(),
                None,
                category == Category::All,
            )
            .unwrap();
            println!(
                "{}: {} items, continuation={}, sorts={}, account={}",
                category.label(),
                page.items.len(),
                page.continuation.is_some(),
                page.sorts.len(),
                page.account.is_some()
            );
            if let Some(token) = &page.continuation {
                let more = fetch(
                    &http,
                    &session,
                    category,
                    &category.endpoint(),
                    Some(token),
                    false,
                )
                .unwrap();
                println!("  next page: {} items", more.items.len());
            }
            if let Some(sort) = page
                .sorts
                .iter()
                .find(|sort| !sort.selected && sort.reload.is_some())
            {
                let sorted = fetch(
                    &http,
                    &session,
                    category,
                    &sort.endpoint,
                    sort.reload.as_deref(),
                    false,
                )
                .unwrap();
                assert!(
                    sorted
                        .sorts
                        .iter()
                        .any(|result| result.selected && result.label == sort.label)
                );
                println!("  sort {}: {} items", sort.label, sorted.items.len());
            }
            if category == Category::Artists
                && let Some(item) = page.items.first()
                && let Target::Open { endpoint } = &item.card.target
            {
                let saved =
                    super::super::super::collection::fetch(&http, endpoint, &item.card.title)
                        .unwrap();
                assert!(!saved.tracks.is_empty());
                println!("  saved artist page: {} songs", saved.tracks.len());
            }
        }
    }
}

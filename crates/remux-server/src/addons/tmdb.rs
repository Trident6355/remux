use anyhow::Result;
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use std::{pin::Pin, sync::Arc, time::Duration};
use tracing::{debug, warn};
use uuid::Uuid;

use super::{
    AddonCapabilities, AddonKind, AddonMetadata, AddonPreset, AddonPresetRegistration,
    CatalogAddon, CatalogInfo, MediaKind, MetaAddon, ResourceType, SearchAddon,
    TreeAddon,
};
use crate::{
    AppContext, api, common, db, sdks,
    sdks::{CachedEndpoint, ClientError},
    services::MediaResolveService,
};

pub struct TmdbPreset;

impl AddonPreset for TmdbPreset {
    fn id(&self) -> &'static str {
        "tmdb"
    }

    fn metadata(&self) -> AddonMetadata {
        AddonMetadata {
            id: "tmdb".to_string(),
            display_name: "TMDB".to_string(),
            description:
                "The Movie Database — high-resolution images, fallback metadata, \
                 and people search."
                    .to_string(),
            icon: None,
            supported_resources: vec![
                AddonMetadata::simple_resource(ResourceType::Meta),
                AddonMetadata::simple_resource(ResourceType::Search),
                AddonMetadata::simple_resource(ResourceType::Catalog),
            ],
            supported_types: vec![
                MediaKind::Movie,
                MediaKind::Series,
                MediaKind::Episode,
                MediaKind::Person,
            ],
            supported_resources_user: vec![ResourceType::Search],
            supported_types_user: vec![
                MediaKind::Movie,
                MediaKind::Series,
                MediaKind::Episode,
                MediaKind::Person,
            ],
            options: vec![],
        }
    }

    fn from_cfg(
        &self,
        _addon_id: Uuid,
        _cfg: &serde_json::Value,
        _config: &crate::Config,
    ) -> Result<AddonCapabilities> {
        let addon = Arc::new(TmdbAddon);
        Ok(AddonCapabilities {
            kind: Some(addon.clone()),
            meta: Some(addon.clone()),
            search: Some(addon.clone()),
            tree: Some(addon.clone()),
            catalog: Some(addon.clone()),
            ..Default::default()
        })
    }
}

inventory::submit! {
    AddonPresetRegistration(|| Box::new(TmdbPreset))
}

pub struct TmdbAddon;

fn tmdb_image(path: Option<&str>, kind: db::ImageKind) -> Option<String> {
    let size = match kind {
        db::ImageKind::Backdrop => "w1280",
        db::ImageKind::Logo => "w500",
        db::ImageKind::Primary | db::ImageKind::Thumb => "w780",
    };
    path.filter(|p| !p.is_empty())
        .map(|p| format!("https://image.tmdb.org/t/p/{}{}", size, p))
}

#[async_trait]
impl AddonKind for TmdbAddon {
    fn id(&self) -> &'static str {
        "tmdb"
    }
}

#[async_trait]
impl MetaAddon for TmdbAddon {
    async fn supports(&self, media: &db::Media) -> bool {
        matches!(
            media.kind,
            db::MediaKind::Movie
                | db::MediaKind::Series
                | db::MediaKind::Season
                | db::MediaKind::Episode
                | db::MediaKind::Person
        )
    }

    async fn meta_fetch(
        &self,
        media: &db::Media,
        ctx: &AppContext,
        config: &crate::api::ServerConfiguration,
    ) -> Result<Option<db::Media>> {
        let patch = match fetch_tmdb_meta(media, ctx, config).await {
            Err(e) if is_404(&e) => {
                fetch_tmdb_meta_by_other_ids(media, ctx, config).await
            }
            other => other,
        }?;
        // TMDB is asked in the metadata language and answers in it, as
        // Jellyfin's TMDB providers report (`ResultLanguage`); an episode
        // TMDB has no name for in that language reports the language it
        // came in instead (see `tmdb_episode_meta`).
        Ok(patch.map(|mut p| {
            if p.result_language
                .is_none()
            {
                p.result_language = config
                    .preferred_metadata_language
                    .clone();
            }
            p
        }))
    }

    async fn rate_limit_cooldown(&self) -> Duration {
        common::tmdb_rate_limit()
            .remaining_cooldown()
            .await
    }

    async fn images_fetch(
        &self,
        media: &db::Media,
        ctx: &AppContext,
        options: super::ImageFetchOptions,
    ) -> Result<Vec<crate::api::RemoteImageInfo>> {
        tmdb_remote_images(ctx, media, options).await
    }
}

#[async_trait]
impl SearchAddon for TmdbAddon {
    async fn search_supports(&self, kind: &db::MediaKind) -> bool {
        matches!(
            kind,
            db::MediaKind::Person | db::MediaKind::Movie | db::MediaKind::Series
        )
    }

    async fn search(
        &self,
        kind: &db::MediaKind,
        query: &str,
        limit: usize,
        ctx: &AppContext,
    ) -> Result<Option<Vec<db::Media>>> {
        match kind {
            db::MediaKind::Person => {
                Ok(Some(search_tmdb_person(query, limit, ctx).await?))
            }
            db::MediaKind::Movie => {
                Ok(Some(search_tmdb_movie(query, limit, ctx).await?))
            }
            db::MediaKind::Series => {
                Ok(Some(search_tmdb_series(query, limit, ctx).await?))
            }
            _ => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// CatalogAddon
// ---------------------------------------------------------------------------

struct CatalogDef {
    id: &'static str,
    name: &'static str,
    kind: db::MediaKind,
    collection_kind: db::CollectionMediaKind,
}

const TMDB_CATALOGS: &[CatalogDef] = &[
    CatalogDef {
        id: "popular_movies",
        name: "Popular Movies",
        kind: db::MediaKind::Movie,
        collection_kind: db::CollectionMediaKind::Movie,
    },
    CatalogDef {
        id: "popular_tv",
        name: "Popular TV Shows",
        kind: db::MediaKind::Series,
        collection_kind: db::CollectionMediaKind::Series,
    },
    CatalogDef {
        id: "top_rated_movies",
        name: "Top Rated Movies",
        kind: db::MediaKind::Movie,
        collection_kind: db::CollectionMediaKind::Movie,
    },
    CatalogDef {
        id: "top_rated_tv",
        name: "Top Rated TV Shows",
        kind: db::MediaKind::Series,
        collection_kind: db::CollectionMediaKind::Series,
    },
    CatalogDef {
        id: "trending_movies_week",
        name: "Trending Movies This Week",
        kind: db::MediaKind::Movie,
        collection_kind: db::CollectionMediaKind::Movie,
    },
    CatalogDef {
        id: "trending_tv_week",
        name: "Trending TV This Week",
        kind: db::MediaKind::Series,
        collection_kind: db::CollectionMediaKind::Series,
    },
];

#[async_trait]
impl CatalogAddon for TmdbAddon {
    async fn catalog_list(&self, _ctx: &AppContext) -> Result<Vec<CatalogInfo>> {
        Ok(TMDB_CATALOGS
            .iter()
            .map(|c| CatalogInfo {
                media_kind: Some(
                    c.kind
                        .clone(),
                ),
                collection_media_kind: Some(
                    c.collection_kind
                        .clone(),
                ),
                default_enabled: false,
                default_max_items: Some(100),
                ..CatalogInfo::new(c.id, c.name)
            })
            .collect())
    }

    async fn catalog_stream(
        &self,
        ctx: &AppContext,
        local_id: &str,
    ) -> Result<Option<Pin<Box<dyn Stream<Item = db::Media> + Send>>>> {
        let config = crate::db::Settings::get_config(&ctx.db).await?;
        let client = tmdb_client(
            config.get_tmdb_key(),
            &ctx.config
                .tmdb_base_url,
        )?;

        let stream: Pin<Box<dyn Stream<Item = db::Media> + Send>> = match local_id {
            "popular_movies" => Box::pin(discover_movie_stream(
                client,
                sdks::tmdb::DiscoverQuery {
                    sort_by: Some("popularity.desc".into()),
                    ..Default::default()
                },
            )),
            "popular_tv" => Box::pin(discover_tv_stream(
                client,
                sdks::tmdb::DiscoverQuery {
                    sort_by: Some("popularity.desc".into()),
                    ..Default::default()
                },
            )),
            "top_rated_movies" => Box::pin(discover_movie_stream(
                client,
                sdks::tmdb::DiscoverQuery {
                    sort_by: Some("vote_average.desc".into()),
                    vote_count_gte: Some(300),
                    ..Default::default()
                },
            )),
            "top_rated_tv" => Box::pin(discover_tv_stream(
                client,
                sdks::tmdb::DiscoverQuery {
                    sort_by: Some("vote_average.desc".into()),
                    vote_count_gte: Some(300),
                    ..Default::default()
                },
            )),
            "trending_movies_week" => Box::pin(trending_movie_stream(
                client,
                sdks::tmdb::TrendingWindow::Week,
            )),
            "trending_tv_week" => {
                Box::pin(trending_tv_stream(client, sdks::tmdb::TrendingWindow::Week))
            }
            _ => return Ok(None),
        };

        Ok(Some(stream))
    }
}

fn movie_result_to_stub(m: sdks::tmdb::MovieSearchResult) -> db::Media {
    let id =
        common::stable_media_uuid(&db::MediaKind::Movie, &format!("tmdb:{}", m.id));
    let mut media = db::Media {
        id,
        title: m.title,
        kind: db::MediaKind::Movie,
        released_at: m
            .release_date
            .and_then(|d| d.and_hms_opt(0, 0, 0)),
        external_ids: db::ExternalIds {
            tmdb: Some(m.id),
            ..Default::default()
        },
        ..Default::default()
    };
    if let Some(url) = tmdb_image(
        m.poster_path
            .as_deref(),
        db::ImageKind::Primary,
    ) {
        media.set_image(db::ImageKind::Primary, url);
    }
    media
}

fn series_result_to_stub(s: sdks::tmdb::SeriesSearchResult) -> db::Media {
    let id =
        common::stable_media_uuid(&db::MediaKind::Series, &format!("tmdb:{}", s.id));
    let mut media = db::Media {
        id,
        title: s.name,
        kind: db::MediaKind::Series,
        released_at: s
            .first_air_date
            .and_then(|d| d.and_hms_opt(0, 0, 0)),
        external_ids: db::ExternalIds {
            tmdb: Some(s.id),
            ..Default::default()
        },
        ..Default::default()
    };
    if let Some(url) = tmdb_image(
        s.poster_path
            .as_deref(),
        db::ImageKind::Primary,
    ) {
        media.set_image(db::ImageKind::Primary, url);
    }
    media
}

fn discover_movie_stream(
    client: sdks::RestClient<sdks::BearerAuth>,
    query: sdks::tmdb::DiscoverQuery,
) -> impl Stream<Item = db::Media> + Send {
    futures::stream::unfold(
        (client, query, Some(1u32)),
        |(client, query, maybe_page)| async move {
            let page = maybe_page?;
            let page_query = sdks::tmdb::DiscoverQuery {
                page: Some(page),
                ..query.clone()
            };
            let resp = client
                .execute(sdks::tmdb::DiscoverMovieEndpoint { query: page_query })
                .await
                .ok()?;
            if resp
                .results
                .is_empty()
            {
                return None;
            }
            let items: Vec<db::Media> = resp
                .results
                .into_iter()
                .map(movie_result_to_stub)
                .collect();
            let next = (page < resp.total_pages).then_some(page + 1);
            Some((futures::stream::iter(items), (client, query, next)))
        },
    )
    .flatten()
}

fn discover_tv_stream(
    client: sdks::RestClient<sdks::BearerAuth>,
    query: sdks::tmdb::DiscoverQuery,
) -> impl Stream<Item = db::Media> + Send {
    futures::stream::unfold(
        (client, query, Some(1u32)),
        |(client, query, maybe_page)| async move {
            let page = maybe_page?;
            let page_query = sdks::tmdb::DiscoverQuery {
                page: Some(page),
                ..query.clone()
            };
            let resp = client
                .execute(sdks::tmdb::DiscoverTvEndpoint { query: page_query })
                .await
                .ok()?;
            if resp
                .results
                .is_empty()
            {
                return None;
            }
            let items: Vec<db::Media> = resp
                .results
                .into_iter()
                .map(series_result_to_stub)
                .collect();
            let next = (page < resp.total_pages).then_some(page + 1);
            Some((futures::stream::iter(items), (client, query, next)))
        },
    )
    .flatten()
}

fn trending_movie_stream(
    client: sdks::RestClient<sdks::BearerAuth>,
    window: sdks::tmdb::TrendingWindow,
) -> impl Stream<Item = db::Media> + Send {
    futures::stream::unfold(
        (client, Some(1u32)),
        move |(client, maybe_page)| async move {
            let page = maybe_page?;
            let resp = client
                .execute(sdks::tmdb::TrendingMovieEndpoint {
                    window,
                    page: Some(page),
                })
                .await
                .ok()?;
            if resp
                .results
                .is_empty()
            {
                return None;
            }
            let items: Vec<db::Media> = resp
                .results
                .into_iter()
                .map(movie_result_to_stub)
                .collect();
            let next = (page < resp.total_pages).then_some(page + 1);
            Some((futures::stream::iter(items), (client, next)))
        },
    )
    .flatten()
}

fn trending_tv_stream(
    client: sdks::RestClient<sdks::BearerAuth>,
    window: sdks::tmdb::TrendingWindow,
) -> impl Stream<Item = db::Media> + Send {
    futures::stream::unfold(
        (client, Some(1u32)),
        move |(client, maybe_page)| async move {
            let page = maybe_page?;
            let resp = client
                .execute(sdks::tmdb::TrendingTvEndpoint {
                    window,
                    page: Some(page),
                })
                .await
                .ok()?;
            if resp
                .results
                .is_empty()
            {
                return None;
            }
            let items: Vec<db::Media> = resp
                .results
                .into_iter()
                .map(series_result_to_stub)
                .collect();
            let next = (page < resp.total_pages).then_some(page + 1);
            Some((futures::stream::iter(items), (client, next)))
        },
    )
    .flatten()
}

// ---------------------------------------------------------------------------
// TMDB SDK type → db::Media conversions
// ---------------------------------------------------------------------------

fn tmdb_external_ids(
    tmdb_id: i64,
    external: Option<&sdks::tmdb::ExternalIds>,
) -> db::ExternalIds {
    db::ExternalIds {
        tmdb: Some(tmdb_id),
        imdb: external
            .and_then(|ids| {
                ids.imdb_id
                    .as_ref()
            })
            .and_then(|id| db::NonEmptyString::try_new(id.clone()).ok()),
        tvdb: external.and_then(|ids| ids.tvdb_id),
        ..Default::default()
    }
}

impl From<&sdks::tmdb::Season> for db::Media {
    fn from(s: &sdks::tmdb::Season) -> Self {
        let air_date = s
            .air_date
            .and_then(|d| d.and_hms_opt(0, 0, 0));
        let mut media = db::Media {
            kind: db::MediaKind::Season,
            title: crate::addons::season_title(s.season_number),
            description: s
                .overview
                .clone()
                .filter(|o| !o.is_empty()),
            idx: Some(s.season_number),
            external_ids: tmdb_external_ids(
                s.id,
                s.external_ids
                    .as_ref(),
            ),
            released_at: air_date,
            digital_released_at: air_date,
            ..Default::default()
        };
        if let Some(url) = tmdb_image(
            s.poster_path
                .as_deref(),
            db::ImageKind::Primary,
        ) {
            media.set_image(db::ImageKind::Primary, url);
        }
        media
    }
}

impl From<&sdks::tmdb::Episode> for db::Media {
    fn from(ep: &sdks::tmdb::Episode) -> Self {
        let external_ratings = db::ExternalRatings {
            tmdb: ep
                .vote_average
                .map(|score| db::Rating {
                    score,
                    vote_count: ep
                        .vote_count
                        .map(|v| v as u32),
                }),
            ..Default::default()
        };
        let mut media = db::Media {
            kind: db::MediaKind::Episode,
            title: ep
                .name
                .clone(),
            description: ep
                .overview
                .clone()
                .filter(|o| !o.is_empty()),
            idx: Some(ep.episode_number),
            parent_idx: Some(ep.season_number),
            external_ids: tmdb_external_ids(
                ep.id,
                ep.external_ids
                    .as_ref(),
            ),
            released_at: ep
                .air_date
                .and_then(|d| d.and_hms_opt(0, 0, 0)),
            runtime: ep
                .runtime
                .map(|r| r * 60),
            rating_audience: external_ratings.audience_rating(),
            external_ratings: Some(external_ratings),
            refreshed_at: Some(chrono::Utc::now().naive_utc()),
            ..Default::default()
        };
        if let Some(url) = tmdb_image(
            ep.still_path
                .as_deref(),
            db::ImageKind::Primary,
        ) {
            media.set_image(db::ImageKind::Primary, url);
        }
        media
    }
}

// ---------------------------------------------------------------------------
// TMDB tree (seasons + episodes)
// ---------------------------------------------------------------------------

#[async_trait]
impl TreeAddon for TmdbAddon {
    fn supports(&self, root: &db::Media) -> bool {
        matches!(root.kind, db::MediaKind::Series | db::MediaKind::Season)
    }

    async fn get_children(
        &self,
        root: &db::Media,
        ctx: &AppContext,
    ) -> Result<Option<Vec<db::Media>>> {
        match root.kind {
            db::MediaKind::Series => tmdb_series_seasons(root, ctx).await,
            db::MediaKind::Season => tmdb_season_episodes(root, ctx).await,
            _ => Ok(None),
        }
    }

    async fn rate_limit_cooldown(&self) -> Duration {
        common::tmdb_rate_limit()
            .remaining_cooldown()
            .await
    }
}

fn tmdb_client(
    api_key: &str,
    base_url: &str,
) -> Result<sdks::RestClient<sdks::BearerAuth>> {
    Ok(sdks::RestClient::new(base_url)?
        .with_auth(sdks::BearerAuth {
            token: api_key.to_string(),
        })
        .with_retry(sdks::ExponentialBackoff::builder().build_with_max_retries(3))
        // TMDB never sends a `Retry-After` header on 429s (they dropped
        // fixed rate limiting in 2019), so without this every 429 falls
        // back to the SDK's generic 60s default, which is far longer than
        // TMDB's actual throttle window. Keep this in sync with
        // `common::tmdb_client_from_config`'s value — both share one
        // cooldown, and a 429 seen on either client installs its own
        // fallback as the block duration.
        .with_default_retry_after(std::time::Duration::from_secs(2))
        // This is the client TmdbAddon's own meta_fetch/tree/catalog use —
        // the highest-traffic TMDB path during a refresh. Without sharing
        // the same cooldown as `common::tmdb_client`, a 429 seen here would
        // never throttle calls made through that other factory, and vice
        // versa: two independent clients, two independent (and inconsistent)
        // views of whether TMDB is currently rate-limiting us.
        .with_shared_rate_limit(common::tmdb_rate_limit()))
}

async fn tmdb_client_from_ctx(
    ctx: &AppContext,
) -> Result<sdks::RestClient<sdks::BearerAuth>> {
    // Cache the client after first build to avoid querying the DB for the
    // API key on every call — critical when many concurrent refresh_meta
    // or get_tree calls all need a TMDB client.
    use std::sync::OnceLock;
    static CLIENT: OnceLock<sdks::RestClient<sdks::BearerAuth>> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let config = crate::db::Settings::get_config(&ctx.db).await?;
    let client = tmdb_client(
        config.get_tmdb_key(),
        &ctx.config
            .tmdb_base_url,
    )?;
    let _ = CLIENT.set(client.clone());
    Ok(client)
}

async fn tmdb_series_seasons(
    series: &db::Media,
    ctx: &AppContext,
) -> Result<Option<Vec<db::Media>>> {
    let Some(tmdb_id) = series
        .external_ids
        .tmdb
    else {
        return Ok(None);
    };
    let client = tmdb_client_from_ctx(ctx).await?;
    let preferred_language = crate::db::Settings::get_config(&ctx.db)
        .await
        .ok()
        .and_then(|c| c.preferred_metadata_language);
    let tv = client
        .execute(
            sdks::tmdb::SeriesEndpoint::new(tmdb_id, preferred_language)
                .with_cache(Duration::from_secs(360)),
        )
        .await?;

    let series_key = series.series_canonical_key();

    let seasons: Vec<db::Media> = tv
        .seasons
        .iter()
        .map(|s| {
            let mut stub = db::Media::from(s);
            stub.id = db::Media::season_id(&series_key, s.season_number);
            stub.parent_id = Some(series.id);
            stub.grandparent_id = Some(series.id);
            stub
        })
        .collect::<Vec<_>>();

    if seasons.is_empty() {
        Ok(None)
    } else {
        Ok(Some(seasons))
    }
}

async fn tmdb_season_episodes(
    season: &db::Media,
    ctx: &AppContext,
) -> Result<Option<Vec<db::Media>>> {
    let (Some(gp), Some(season_number)) = (
        season
            .grandparent
            .as_deref(),
        season.idx,
    ) else {
        return Ok(None);
    };
    let Some(series_tmdb_id) = gp
        .external_ids
        .tmdb
    else {
        return Ok(None);
    };
    let client = tmdb_client_from_ctx(ctx).await?;
    let config = crate::db::Settings::get_config(&ctx.db)
        .await
        .ok();
    let translations_budget = config
        .as_ref()
        .map(|c| c.addon_fetch_timeout())
        .unwrap_or(Duration::from_secs(5))
        * 3
        / 5;
    let preferred_language = config.and_then(|c| c.preferred_metadata_language);
    let season_details = client
        .execute(
            sdks::tmdb::SeasonEndpoint {
                series_id: series_tmdb_id,
                season_number: season_number as i64,
                language: preferred_language.clone(),
                append_to_response: None,
            }
            .with_cache(Duration::from_secs(360)),
        )
        .await?;

    let anchor = gp.series_canonical_key();
    // An untranslated episode costs one more request (its translations);
    // run those side by side, or a season with many of them (90 untranslated
    // specials) would not finish within the tree fetch timeout.
    let preferred_language = preferred_language.as_deref();
    let original_language = gp
        .original_language
        .as_deref();
    let mut lookups = Vec::new();
    for ep in season_details
        .episodes
        .as_deref()
        .unwrap_or_default()
    {
        lookups.push(tmdb_episode_meta(
            &client,
            series_tmdb_id,
            ep,
            preferred_language,
            original_language,
        ));
    }
    // The whole tree fetch has a timeout; translations get part of it. When
    // they don't finish in time (TMDB slow, a huge untranslated season), the
    // season comes back anyway: an episode that may be untranslated is left
    // unrefreshed, so its own refresh decides its name, with its own timeout
    // and whatever translations already arrived in the cache.
    let lookups = futures::stream::iter(lookups)
        .buffered(EPISODE_TRANSLATIONS_CONCURRENCY)
        .collect::<Vec<_>>();
    let mut episodes: Vec<db::Media> =
        match tokio::time::timeout(translations_budget, lookups).await {
            Ok(results) => results
                .into_iter()
                .collect::<Result<_>>()?,
            Err(_) => {
                debug!(
                    series = series_tmdb_id,
                    season = season_number,
                    "episode translations took too long, left to the episode refresh"
                );
                season_details
                    .episodes
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .map(|ep| {
                        let mut stub = db::Media::from(ep);
                        stub.result_language = preferred_language.map(str::to_string);
                        if tmdb_episode_may_be_untranslated(ep, preferred_language) {
                            stub.refreshed_at = None;
                        }
                        stub
                    })
                    .collect()
            }
        };
    for stub in &mut episodes {
        let (Some(season_number), Some(episode_number)) = (stub.parent_idx, stub.idx)
        else {
            continue;
        };
        stub.id = db::Media::episode_id(&anchor, season_number, episode_number);
        stub.parent_id = Some(db::Media::season_id(&anchor, season_number));
        stub.grandparent_id = season.parent_id;
    }

    if episodes.is_empty() {
        Ok(None)
    } else {
        Ok(Some(episodes))
    }
}

/// How long an episode's TMDB translations are kept: they only decide which
/// language an episode's name is given in, and change rarely.
const EPISODE_TRANSLATIONS_TTL: Duration = Duration::from_secs(7 * 86400);

/// How many episode translations of one season are asked for at once (TMDB
/// allows 20 connections per IP), so that a season with 100+ untranslated
/// specials still fits the tree fetch timeout.
const EPISODE_TRANSLATIONS_CONCURRENCY: usize = 16;

/// Whether TMDB's answer for `ep` in `language` may be its stand-in for a
/// missing translation. TMDB then fills the name with a made-up one in the
/// asked language ("Episode 26", "Folge 26") and leaves the overview empty,
/// without saying so; only the episode's translations tell. Without a
/// language TMDB answers in its default (English) as before.
fn tmdb_episode_may_be_untranslated(
    ep: &sdks::tmdb::Episode,
    language: Option<&str>,
) -> bool {
    language.is_some_and(|l| !l.is_empty())
        && ep
            .overview
            .as_deref()
            .is_none_or(|o| {
                o.trim()
                    .is_empty()
            })
}

/// The episode as a metadata result, in `language` where TMDB has it.
///
/// For an episode whose localized answer has no overview, the episode's
/// translations decide: when none in `language` has a name, the name and
/// overview come from the English translation, or else from the one in the
/// series' original language, and the result reports that language
/// (`result_language`), so its name is never taken for one in `language`.
/// When no translation has a name either, TMDB's answer stays as it is.
async fn tmdb_episode_meta(
    client: &sdks::RestClient<sdks::BearerAuth>,
    series_tmdb_id: i64,
    ep: &sdks::tmdb::Episode,
    language: Option<&str>,
    original_language: Option<&str>,
) -> Result<db::Media> {
    let mut media = db::Media::from(ep);
    media.result_language = language
        .filter(|l| !l.is_empty())
        .map(str::to_string);
    let Some(language) =
        language.filter(|_| tmdb_episode_may_be_untranslated(ep, language))
    else {
        return Ok(media);
    };
    let translations = match client
        .execute_arc(
            sdks::tmdb::EpisodeTranslationsEndpoint {
                series_id: series_tmdb_id,
                season_number: ep.season_number,
                episode_number: ep.episode_number,
            }
            .with_cache(EPISODE_TRANSLATIONS_TTL),
        )
        .await
    {
        Ok(t) => t,
        Err(e) => {
            let e = anyhow::Error::from(e);
            if is_404(&e) {
                return Ok(media);
            }
            return Err(e);
        }
    };
    if let Some(found) = episode_translation(&translations, language, original_language)
    {
        media.title = found.name;
        media.description = found.overview;
        media.result_language = Some(found.language);
    }
    Ok(media)
}

/// An untranslated episode's name and overview from its translations.
struct EpisodeTranslation {
    /// The language the name is in, which the result reports.
    language: String,
    name: String,
    overview: Option<String>,
}

/// The name comes from the first translation with a name in `language` (its
/// own region first), else in English (US first), else in the series'
/// original language. The overview, field by field, from the first of those
/// that has one, so an episode with a translated name but no translated
/// overview keeps the English overview TMDB answered with before it was
/// asked in the metadata language.
fn episode_translation(
    translations: &sdks::tmdb::EpisodeTranslations,
    language: &str,
    original_language: Option<&str>,
) -> Option<EpisodeTranslation> {
    let mut parts = language.split('-');
    let subtag = parts
        .next()
        .unwrap_or_default();
    let region = parts.next();
    let original = original_language.filter(|o| !o.is_empty());
    let pick =
        |lang: &str,
         region: Option<&str>,
         field: fn(&sdks::tmdb::EpisodeTranslationData) -> Option<&str>| {
            let mut with_field = translations
                .translations
                .iter()
                .filter(|t| {
                    t.iso_639_1
                        .eq_ignore_ascii_case(lang)
                })
                .filter_map(|t| {
                    field(&t.data)
                        .filter(|v| {
                            !v.trim()
                                .is_empty()
                        })
                        .map(|v| (t, v))
                });
            let first = with_field
                .clone()
                .next();
            region
                .and_then(|r| {
                    with_field.find(|(t, _)| {
                        t.iso_3166_1
                            .eq_ignore_ascii_case(r)
                    })
                })
                .or(first)
                .map(|(_, v)| v.to_string())
        };
    let in_order = |field: fn(&sdks::tmdb::EpisodeTranslationData) -> Option<&str>| {
        pick(subtag, region, field)
            .map(|v| (language.to_string(), v))
            .or_else(|| pick("en", Some("US"), field).map(|v| ("en".to_string(), v)))
            .or_else(|| {
                original.and_then(|o| pick(o, None, field).map(|v| (o.to_string(), v)))
            })
    };
    let (language, name) = in_order(|d| {
        d.name
            .as_deref()
    })?;
    let overview = in_order(|d| {
        d.overview
            .as_deref()
    })
    .map(|(_, v)| v);
    Some(EpisodeTranslation {
        language,
        name,
        overview,
    })
}

// ---------------------------------------------------------------------------
// TMDB meta fetch
// ---------------------------------------------------------------------------

fn select_rating<'a, T, FCountry, FRating>(
    ratings: &'a [T],
    metadata_country: &str,
    country: FCountry,
    rating: FRating,
) -> Option<(String, String)>
where
    FCountry: Fn(&'a T) -> &'a str,
    FRating: Fn(&'a T) -> Option<&'a str>,
{
    let valid = |item: &'a T| {
        rating(item)
            .map(str::trim)
            .filter(|rating| !rating.is_empty())
            .map(|rating| (country(item).to_string(), rating.to_string()))
    };
    ratings
        .iter()
        .find(|item| {
            country(item).eq_ignore_ascii_case(metadata_country)
                && valid(item).is_some()
        })
        .and_then(valid)
        .or_else(|| {
            ratings
                .iter()
                .find(|item| {
                    country(item).eq_ignore_ascii_case("US") && valid(item).is_some()
                })
                .and_then(valid)
        })
        .or_else(|| {
            ratings
                .iter()
                .find_map(valid)
        })
}

fn tmdb_rating_label(country: &str, rating: &str) -> String {
    if country.eq_ignore_ascii_case("US") {
        rating.to_string()
    } else if country.eq_ignore_ascii_case("DE")
        && !rating
            .to_uppercase()
            .starts_with("FSK")
    {
        format!("FSK-{rating}")
    } else {
        rating.to_string()
    }
}

fn rating_age(label: &str, country: &str) -> Option<i32> {
    crate::localization::ratings::resolve_rating_age(Some(label), Some(country))
        .or_else(|| crate::localization::ratings::resolve_rating_age(Some(label), None))
}

fn build_person_relations(
    left_media_id: uuid::Uuid,
    credits: &sdks::tmdb::Credits,
) -> Vec<(db::MediaRelation, db::Media)> {
    let mut relations = Vec::new();
    for (_i, member) in credits
        .cast
        .iter()
        .enumerate()
    {
        let name = &member.name;
        let person_id = common::stable_media_uuid(
            &db::MediaKind::Person,
            &member
                .id
                .to_string(),
        );
        let mut person = db::Media {
            id: person_id,
            title: name.clone(),
            kind: db::MediaKind::Person,
            external_ids: db::ExternalIds {
                tmdb: Some(member.id),
                ..Default::default()
            },
            ..Default::default()
        };
        if let Some(url) = tmdb_image(
            member
                .profile_path
                .as_deref(),
            db::ImageKind::Primary,
        ) {
            person.set_image(db::ImageKind::Primary, url);
        }
        relations.push((
            db::MediaRelation {
                left_media_id,
                right_media_id: person_id,
                weight: Some(member.order as i64),
                role: Some(db::RelationRole::Actor),
                character: member
                    .character
                    .clone(),
                ..Default::default()
            },
            person,
        ));
    }
    for (i, member) in credits
        .crew
        .iter()
        .enumerate()
    {
        let role = match member
            .job
            .as_str()
        {
            "Director" => Some(db::RelationRole::Director),
            "Writer" | "Screenplay" | "Author" => Some(db::RelationRole::Writer),
            "Producer" | "Executive Producer" | "Co-Producer" => {
                Some(db::RelationRole::Producer)
            }
            _ => None,
        };
        if let Some(role) = role {
            let name = &member.name;
            let person_id = common::stable_media_uuid(
                &db::MediaKind::Person,
                &member
                    .id
                    .to_string(),
            );
            let mut person = db::Media {
                id: person_id,
                title: name.clone(),
                kind: db::MediaKind::Person,
                external_ids: db::ExternalIds {
                    tmdb: Some(member.id),
                    ..Default::default()
                },
                ..Default::default()
            };
            if let Some(url) = tmdb_image(
                member
                    .profile_path
                    .as_deref(),
                db::ImageKind::Primary,
            ) {
                person.set_image(db::ImageKind::Primary, url);
            }
            relations.push((
                db::MediaRelation {
                    left_media_id,
                    right_media_id: person_id,
                    weight: Some(i as i64),
                    role: Some(role),
                    ..Default::default()
                },
                person,
            ));
        }
    }
    relations
}

fn build_genre_relations(
    left_media_id: uuid::Uuid,
    genres: &[sdks::tmdb::Genre],
) -> Vec<(db::MediaRelation, db::Media)> {
    genres
        .iter()
        .map(|genre| {
            let name = &genre.name;
            let genre_id =
                common::stable_media_uuid(&db::MediaKind::Genre, &name.to_lowercase());
            (
                db::MediaRelation {
                    left_media_id,
                    right_media_id: genre_id,
                    ..Default::default()
                },
                db::Media {
                    id: genre_id,
                    title: name.clone(),
                    kind: db::MediaKind::Genre,
                    ..Default::default()
                },
            )
        })
        .collect()
}

fn build_studio_relations(
    left_media_id: uuid::Uuid,
    companies: &[sdks::tmdb::ProductionCompany],
) -> Vec<(db::MediaRelation, db::Media)> {
    companies
        .iter()
        .map(|company| {
            let name = &company.name;
            let studio_id =
                common::stable_media_uuid(&db::MediaKind::Studio, &name.to_lowercase());
            (
                db::MediaRelation {
                    left_media_id,
                    right_media_id: studio_id,
                    ..Default::default()
                },
                db::Media {
                    id: studio_id,
                    title: name.clone(),
                    kind: db::MediaKind::Studio,
                    ..Default::default()
                },
            )
        })
        .collect()
}

fn build_location_relations(
    left_media_id: uuid::Uuid,
    countries: &[sdks::tmdb::ProductionCountry],
) -> Vec<(db::MediaRelation, db::Media)> {
    countries
        .iter()
        .map(|country| {
            let name = &country.name;
            let country_id = common::stable_media_uuid(
                &db::MediaKind::Country,
                &name.to_lowercase(),
            );
            (
                db::MediaRelation {
                    left_media_id,
                    right_media_id: country_id,
                    ..Default::default()
                },
                db::Media {
                    id: country_id,
                    title: name.clone(),
                    kind: db::MediaKind::Country,
                    ..Default::default()
                },
            )
        })
        .collect()
}

/// The stored tmdb id 404s. That usually means TMDB merged the entry into
/// another one or deleted it. `/find` by IMDb or TVDB id still points at the
/// entry that is left, so look the id up again and fetch that one. The patch
/// carries the new tmdb id. Returns `None` if nothing else resolves, so
/// lower-priority meta addons can still fill the item.
async fn fetch_tmdb_meta_by_other_ids(
    media: &db::Media,
    ctx: &AppContext,
    config: &crate::api::ServerConfiguration,
) -> Result<Option<db::Media>> {
    let is_tv = match media.kind {
        db::MediaKind::Movie => false,
        db::MediaKind::Series => true,
        _ => return Ok(None),
    };
    let client = tmdb_client(
        config.get_tmdb_key(),
        &ctx.config
            .tmdb_base_url,
    )?;
    let ids = &media.external_ids;
    let keys = [
        ids.imdb
            .clone()
            .map(|imdb| (imdb.into(), "imdb_id")),
        ids.tvdb
            .map(|tvdb| (tvdb.to_string(), "tvdb_id")),
    ];
    for (external_id, external_source) in keys
        .into_iter()
        .flatten()
    {
        let found = MediaResolveService::find_tmdb_id_by(
            external_id,
            external_source,
            is_tv,
            &client,
        )
        .await
        .ok()
        .flatten();
        let Some(tmdb_id) = found.filter(|id| Some(*id) != ids.tmdb) else {
            continue;
        };
        debug!(
            old = ?ids.tmdb,
            new = tmdb_id,
            title = %media.title,
            "stored tmdb id is gone, re-resolved by {external_source}"
        );
        let mut moved = media.clone();
        moved
            .external_ids
            .tmdb = Some(tmdb_id);
        return match fetch_tmdb_meta(&moved, ctx, config).await {
            Err(e) if is_404(&e) => Ok(None),
            other => other,
        };
    }
    Ok(None)
}

fn is_404(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<ClientError>(),
        Some(ClientError::Http { status: 404, .. })
    )
}

/// Extract unique provider names from a watch/providers response for the given
/// country (falls back to "US", then to the first available country). Returns
/// tags of the form `"provider:Name"` covering flatrate, rent, and buy entries.
fn watch_provider_tags(
    resp: Option<&sdks::tmdb::WatchProvidersResponse>,
    country: &str,
) -> Vec<String> {
    let Some(resp) = resp else { return vec![] };

    let pick = resp
        .results
        .get(&country.to_uppercase())
        .or_else(|| {
            resp.results
                .get("US")
        })
        .or_else(|| {
            resp.results
                .values()
                .next()
        });

    let Some(entry) = pick else { return vec![] };

    let mut names: Vec<String> = entry
        .flatrate
        .iter()
        .chain(
            entry
                .rent
                .iter(),
        )
        .chain(
            entry
                .buy
                .iter(),
        )
        .map(|p| {
            p.provider_name
                .clone()
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    names
        .into_iter()
        .map(|n| format!("provider:{}", n))
        .collect()
}

/// Image languages to request alongside the configured metadata language so
/// `best_logo`/`best_thumb` (which look for English-tagged title-card art)
/// can find a match even when the server's preferred language isn't English.
/// Without `include_image_language`, TMDB restricts `images.backdrops`/
/// `logos` to the request's `language` plus untagged entries, so an "en"
/// entry never comes back unless the configured language already is "en".
/// Videos are requested in the same languages, for the same reason.
fn thumb_and_logo_languages(preferred_language: Option<&str>) -> String {
    let mut langs = vec!["en", "null"];
    if let Some(primary) = preferred_language.and_then(|l| {
        l.split('-')
            .next()
    }) && !primary.is_empty()
        && !langs.contains(&primary)
    {
        langs.insert(0, primary);
    }
    langs.join(",")
}

/// At most this many TMDB trailers per item; the rest are mostly the same
/// trailer in other cuts or languages.
const MAX_TMDB_TRAILERS: usize = 5;

/// The YouTube trailers of a movie or series, best first. `None` when TMDB
/// sent no videos, so the trailers other addons found are kept.
fn tmdb_trailers(
    videos: Option<&sdks::tmdb::Videos>,
    preferred_language: Option<&str>,
) -> Option<Vec<db::Trailer>> {
    let language = preferred_language.and_then(|l| {
        l.split('-')
            .next()
    });
    Some(
        videos?
            .youtube_trailers(language)
            .into_iter()
            .take(MAX_TMDB_TRAILERS)
            .map(|v| db::Trailer {
                source: db::TrailerSource::Youtube(
                    v.key
                        .clone(),
                ),
                name: v
                    .name
                    .clone(),
            })
            .collect(),
    )
}

/// The original language of an episode's series, from the preloaded parents
/// or the stored series.
async fn stored_series_original_language(
    media: &db::Media,
    ctx: &AppContext,
) -> Option<String> {
    if let Some(series) = media
        .grandparent
        .as_deref()
        .or(media
            .parent
            .as_deref())
        .filter(|g| g.kind == db::MediaKind::Series)
    {
        return series
            .original_language
            .clone();
    }
    let series_id = media
        .grandparent_id
        .or(media.parent_id)?;
    db::Media::get_by_id(&ctx.db, &series_id)
        .await
        .ok()
        .flatten()
        .filter(|m| m.kind == db::MediaKind::Series)
        .and_then(|m| m.original_language)
}

async fn fetch_tmdb_meta(
    media: &db::Media,
    ctx: &AppContext,
    config: &crate::api::ServerConfiguration,
) -> Result<Option<db::Media>> {
    let metadata_country = config
        .metadata_country_code
        .as_deref()
        .map(db::normalize_country_alpha2)
        .unwrap_or_else(|| "US".to_string());

    let ids = &media.external_ids;

    let client = tmdb_client(
        config.get_tmdb_key(),
        &ctx.config
            .tmdb_base_url,
    )?;

    match media.kind {
        db::MediaKind::Movie => {
            // `resolve_external_ids` (called at the top of
            // `refresh_meta`, before any addon runs) already resolves a
            // tmdb id from whatever else is known, if one is resolvable at
            // all — nothing left to discover here.
            let tmdb_movie_id: Option<i64> = ids.tmdb;

            if let Some(tmdb_id) = tmdb_movie_id {
                let movie_details = client
                    .execute(
                        sdks::tmdb::MovieEndpoint::new(
                            tmdb_id,
                            config
                                .preferred_metadata_language
                                .clone(),
                        )
                        .with_image_languages(thumb_and_logo_languages(
                            config
                                .preferred_metadata_language
                                .as_deref(),
                        ))
                        .with_videos(thumb_and_logo_languages(
                            config
                                .preferred_metadata_language
                                .as_deref(),
                        ))
                        .with_cache(Duration::from_secs(360)),
                    )
                    .await?;
                let external_ids = db::ExternalIds {
                    tmdb: Some(movie_details.id),
                    imdb: movie_details
                        .imdb_id
                        .as_deref()
                        .and_then(|s| db::NonEmptyString::try_new(s.to_string()).ok())
                        .or_else(|| {
                            ids.imdb
                                .clone()
                        }),
                    tvdb: ids.tvdb,
                    ..Default::default()
                };
                let logo = movie_details
                    .images
                    .as_ref()
                    .and_then(|i| i.best_logo())
                    .and_then(|p| tmdb_image(Some(p), db::ImageKind::Logo));
                let thumb = movie_details
                    .images
                    .as_ref()
                    .and_then(|i| i.best_thumb())
                    .and_then(|p| tmdb_image(Some(p), db::ImageKind::Thumb));
                let rating = movie_details
                    .release_dates
                    .as_ref()
                    .and_then(|release_dates| {
                        let releases = release_dates
                            .results
                            .iter()
                            .flat_map(|country| {
                                country
                                    .release_dates
                                    .iter()
                                    .map(|release| {
                                        (
                                            country
                                                .iso_3166_1
                                                .as_str(),
                                            release
                                                .certification
                                                .as_deref(),
                                        )
                                    })
                            })
                            .collect::<Vec<_>>();
                        select_rating(
                            &releases,
                            &metadata_country,
                            |(country, _)| country,
                            |(_, certification)| *certification,
                        )
                    });
                let (certification, certification_age) = rating
                    .map(|(country, rating)| {
                        let label = tmdb_rating_label(&country, &rating);
                        let age = rating_age(&label, &country);
                        (Some(label), age)
                    })
                    .unwrap_or((None, None));
                let digital_released_at = movie_details
                    .release_dates
                    .as_ref()
                    .and_then(|rd| {
                        rd.results
                            .iter()
                            .flat_map(|country| {
                                country
                                    .release_dates
                                    .iter()
                            })
                            .filter(|e| e.release_type >= 4)
                            .filter_map(|e| e.release_date)
                            .min()
                    })
                    .map(|dt| dt.naive_utc());
                let external_ratings = db::ExternalRatings {
                    tmdb: movie_details
                        .vote_average
                        .map(|score| db::Rating {
                            score,
                            vote_count: movie_details
                                .vote_count
                                .map(|v| v as u32),
                        }),
                    ..Default::default()
                };
                let mut patch = db::Media {
                    title: movie_details.title,
                    description: movie_details.overview,
                    released_at: movie_details
                        .release_date
                        .and_then(|d| d.and_hms_opt(0, 0, 0)),
                    digital_released_at,
                    runtime: movie_details
                        .runtime
                        .map(|r| r * 60),
                    rating_audience: external_ratings.audience_rating(),
                    external_ratings: Some(external_ratings),
                    certification,
                    certification_age,
                    external_ids: external_ids,
                    original_language: Some(
                        movie_details
                            .original_language
                            .clone(),
                    ),
                    trailers: tmdb_trailers(
                        movie_details
                            .videos
                            .as_ref(),
                        config
                            .preferred_metadata_language
                            .as_deref(),
                    ),
                    ..Default::default()
                };
                if let Some(url) = tmdb_image(
                    movie_details
                        .poster_path
                        .as_deref(),
                    db::ImageKind::Primary,
                ) {
                    patch.set_image(db::ImageKind::Primary, url);
                }
                if let Some(url) = tmdb_image(
                    movie_details
                        .backdrop_path
                        .as_deref(),
                    db::ImageKind::Backdrop,
                ) {
                    patch.set_image(db::ImageKind::Backdrop, url);
                }
                if let Some(url) = logo {
                    patch.set_image(db::ImageKind::Logo, url);
                }
                if let Some(url) = thumb {
                    patch.set_image(db::ImageKind::Thumb, url);
                }
                let mut relations = vec![];
                if let Some(genres) = &movie_details.genres {
                    relations.extend(build_genre_relations(media.id, genres));
                }
                if let Some(credits) = &movie_details.credits {
                    relations.extend(build_person_relations(media.id, credits));
                }
                if let Some(companies) = &movie_details.production_companies {
                    relations.extend(build_studio_relations(media.id, companies));
                }
                if let Some(countries) = &movie_details.production_countries {
                    relations.extend(build_location_relations(media.id, countries));
                }
                if !relations.is_empty() {
                    patch.relations = Some(relations);
                }
                let providers = client
                    .execute(
                        sdks::tmdb::MovieWatchProvidersEndpoint { movie_id: tmdb_id }
                            .with_cache(Duration::from_secs(86400)),
                    )
                    .await
                    .ok();
                patch.tags = watch_provider_tags(providers.as_ref(), &metadata_country);
                return Ok(Some(patch));
            }
        }
        db::MediaKind::Series => {
            // `resolve_external_ids` (called at the top of
            // `refresh_meta`, before any addon runs) already resolves a
            // tmdb id from whatever else is known, if one is resolvable at
            // all — nothing left to discover here.
            let tmdb_series_id: Option<i64> = ids.tmdb;

            if let Some(tmdb_id) = tmdb_series_id {
                let tv_details = client
                    .execute(
                        sdks::tmdb::SeriesEndpoint::new(
                            tmdb_id,
                            config
                                .preferred_metadata_language
                                .clone(),
                        )
                        .with_image_languages(thumb_and_logo_languages(
                            config
                                .preferred_metadata_language
                                .as_deref(),
                        ))
                        .with_videos(thumb_and_logo_languages(
                            config
                                .preferred_metadata_language
                                .as_deref(),
                        ))
                        .with_cache(Duration::from_secs(360)),
                    )
                    .await?;
                let tmdb_ext = tv_details
                    .external_ids
                    .as_ref();
                let external_ids = db::ExternalIds {
                    tmdb: Some(tv_details.id),
                    imdb: tmdb_ext
                        .and_then(|e| {
                            e.imdb_id
                                .as_deref()
                        })
                        .and_then(|s| db::NonEmptyString::try_new(s.to_string()).ok()),
                    tvdb: tmdb_ext.and_then(|e| e.tvdb_id),
                    ..Default::default()
                };
                let country = tv_details
                    .origin_country
                    .into_iter()
                    .next();
                let logo = tv_details
                    .images
                    .as_ref()
                    .and_then(|i| i.best_logo())
                    .and_then(|p| tmdb_image(Some(p), db::ImageKind::Logo));
                let thumb = tv_details
                    .images
                    .as_ref()
                    .and_then(|i| i.best_thumb())
                    .and_then(|p| tmdb_image(Some(p), db::ImageKind::Thumb));
                let rating = tv_details
                    .content_ratings
                    .as_ref()
                    .and_then(|content_ratings| {
                        select_rating(
                            &content_ratings.results,
                            &metadata_country,
                            |rating| {
                                rating
                                    .iso_3166_1
                                    .as_str()
                            },
                            |rating| {
                                rating
                                    .rating
                                    .as_deref()
                            },
                        )
                    });
                let (certification, certification_age) = rating
                    .map(|(country, rating)| {
                        let label = tmdb_rating_label(&country, &rating);
                        let age = rating_age(&label, &country);
                        (Some(label), age)
                    })
                    .unwrap_or((None, None));
                let external_ratings = db::ExternalRatings {
                    tmdb: tv_details
                        .vote_average
                        .map(|score| db::Rating {
                            score,
                            vote_count: Some(tv_details.vote_count as u32),
                        }),
                    ..Default::default()
                };
                let tmdb_status = tv_details
                    .status
                    .as_ref();
                let status = match tmdb_status {
                    Some(sdks::tmdb::Status::Ended | sdks::tmdb::Status::Canceled) => {
                        Some(db::MediaStatus::Ended)
                    }
                    Some(
                        sdks::tmdb::Status::ReturningSeries | sdks::tmdb::Status::Pilot,
                    ) => Some(db::MediaStatus::Continuing),
                    Some(
                        sdks::tmdb::Status::InProduction
                        | sdks::tmdb::Status::Planned
                        | sdks::tmdb::Status::PostProduction,
                    ) => Some(db::MediaStatus::Unreleased),
                    _ => None,
                };
                let end_date = if matches!(
                    tmdb_status,
                    Some(sdks::tmdb::Status::Ended | sdks::tmdb::Status::Canceled)
                ) {
                    tv_details
                        .last_air_date
                        .as_deref()
                        .and_then(|s| {
                            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
                        })
                        .and_then(|d| d.and_hms_opt(0, 0, 0))
                } else {
                    None
                };
                let mut patch = db::Media {
                    title: tv_details.name,
                    description: tv_details.overview,
                    released_at: tv_details
                        .first_air_date
                        .and_then(|d| d.and_hms_opt(0, 0, 0)),
                    rating_audience: external_ratings.audience_rating(),
                    external_ratings: Some(external_ratings),
                    certification,
                    certification_age,
                    country,
                    external_ids: external_ids,
                    original_language: Some(
                        tv_details
                            .original_language
                            .clone(),
                    ),
                    status,
                    end_date,
                    trailers: tmdb_trailers(
                        tv_details
                            .videos
                            .as_ref(),
                        config
                            .preferred_metadata_language
                            .as_deref(),
                    ),
                    ..Default::default()
                };
                if let Some(url) = tmdb_image(
                    tv_details
                        .poster_path
                        .as_deref(),
                    db::ImageKind::Primary,
                ) {
                    patch.set_image(db::ImageKind::Primary, url);
                }
                if let Some(url) = tmdb_image(
                    tv_details
                        .backdrop_path
                        .as_deref(),
                    db::ImageKind::Backdrop,
                ) {
                    patch.set_image(db::ImageKind::Backdrop, url);
                }
                if let Some(url) = logo {
                    patch.set_image(db::ImageKind::Logo, url);
                }
                if let Some(url) = thumb {
                    patch.set_image(db::ImageKind::Thumb, url);
                }
                let mut relations = vec![];
                if let Some(genres) = &tv_details.genres {
                    relations.extend(build_genre_relations(media.id, genres));
                }
                if let Some(credits) = &tv_details.credits {
                    relations.extend(build_person_relations(media.id, credits));
                }
                if let Some(companies) = &tv_details.production_companies {
                    relations.extend(build_studio_relations(media.id, companies));
                }
                if let Some(countries) = &tv_details.production_countries {
                    relations.extend(build_location_relations(media.id, countries));
                }
                if let Some(creators) = &tv_details.created_by {
                    for (i, creator) in creators
                        .iter()
                        .enumerate()
                    {
                        let name = &creator.name;
                        let tmdb_id = creator.id as i64;
                        let person_id = common::stable_media_uuid(
                            &db::MediaKind::Person,
                            &tmdb_id.to_string(),
                        );
                        let mut creator_media = db::Media {
                            id: person_id,
                            title: name.clone(),
                            kind: db::MediaKind::Person,
                            external_ids: db::ExternalIds {
                                tmdb: Some(tmdb_id),
                                ..Default::default()
                            },
                            ..Default::default()
                        };
                        if let Some(url) = tmdb_image(
                            creator
                                .profile_path
                                .as_deref(),
                            db::ImageKind::Primary,
                        ) {
                            creator_media.set_image(db::ImageKind::Primary, url);
                        }
                        relations.push((
                            db::MediaRelation {
                                left_media_id: media.id,
                                right_media_id: person_id,
                                weight: Some(i as i64),
                                role: Some(db::RelationRole::Creator),
                                ..Default::default()
                            },
                            creator_media,
                        ));
                    }
                }
                if !relations.is_empty() {
                    patch.relations = Some(relations);
                }
                let providers = client
                    .execute(
                        sdks::tmdb::TvWatchProvidersEndpoint { series_id: tmdb_id }
                            .with_cache(Duration::from_secs(86400)),
                    )
                    .await
                    .ok();
                patch.tags = watch_provider_tags(providers.as_ref(), &metadata_country);
                return Ok(Some(patch));
            }
        }
        db::MediaKind::Episode => {
            let series_tmdb_id =
                MediaResolveService::stored_series_tmdb_id(media, ctx).await?;
            let season_number = media.parent_idx;
            let episode_number = media.idx;
            if let (Some(tmdb_id), Some(s_n), Some(e_n)) =
                (series_tmdb_id, season_number, episode_number)
            {
                // Use the SeasonEndpoint (already cached from get_tree) instead of
                // a separate per-episode EpisodeEndpoint call.
                // Borrow the cached season rather than taking it by value: this runs
                // once per episode, and copying every episode's overview/credits/
                // guest stars just to read one of them is quadratic per season.
                let language = config
                    .preferred_metadata_language
                    .as_deref();
                // A season TMDB doesn't have (the episode list came from another
                // source that splits TMDB's season) is not an error here.
                let season = tmdb_cached_season(
                    &client,
                    tmdb_id,
                    s_n,
                    language.map(str::to_string),
                )
                .await?;
                let by_number = season
                    .as_deref()
                    .and_then(|s| {
                        s.episodes
                            .as_deref()
                    })
                    .unwrap_or_default()
                    .iter()
                    .find(|e| e.episode_number == e_n)
                    .filter(|ep| is_same_tmdb_episode(ep, media));
                // The episode list came from a source that numbers episodes
                // differently (Cinemeta/IMDb splits seasons TMDB keeps as one,
                // or parts TMDB merges): TMDB's episode at this number is
                // another episode, or there is none. Find TMDB's own episode
                // by the TVDB id that source gave us; the numbering stays the
                // list's.
                let (ep, renumbered) = match by_number {
                    Some(ep) => (std::borrow::Cow::Borrowed(ep), false),
                    None => match tmdb_episode_by_tvdb(
                        &client,
                        tmdb_id,
                        media,
                        language.map(str::to_string),
                    )
                    .await?
                    {
                        Some(ep) => (std::borrow::Cow::Owned(ep), true),
                        None => return Ok(None),
                    },
                };
                let original_language =
                    if tmdb_episode_may_be_untranslated(&ep, language) {
                        stored_series_original_language(media, ctx).await
                    } else {
                        None
                    };
                let mut patch = tmdb_episode_meta(
                    &client,
                    tmdb_id,
                    &ep,
                    language,
                    original_language.as_deref(),
                )
                .await?;
                if renumbered {
                    patch.idx = None;
                    patch.parent_idx = None;
                }
                return Ok(Some(patch));
            }
        }
        db::MediaKind::Season => {
            return fetch_tmdb_season_meta(media, ctx, &client, config).await;
        }
        db::MediaKind::Person => {
            let tmdb_id = if let Some(id) = media
                .external_ids
                .tmdb
            {
                id
            } else {
                // No stored TMDB ID — search by name to resolve it.
                let resp = client
                    .execute(sdks::tmdb::PersonSearchEndpoint {
                        query: media
                            .title
                            .clone(),
                    })
                    .await?;
                let Some(hit) = resp
                    .results
                    .into_iter()
                    .next()
                else {
                    return Ok(None);
                };
                hit.id
            };
            let details = client
                .execute(
                    sdks::tmdb::PersonDetailsEndpoint { person_id: tmdb_id }
                        .with_cache(Duration::from_secs(86400)),
                )
                .await?;
            let released_at = details
                .birthday
                .as_deref()
                .and_then(|b| {
                    chrono::NaiveDate::parse_from_str(b, "%Y-%m-%d")
                        .ok()
                        .and_then(|d| d.and_hms_opt(0, 0, 0))
                });
            let mut patch = db::Media {
                description: details
                    .biography
                    .filter(|b| !b.is_empty()),
                released_at,
                country: details
                    .place_of_birth
                    .filter(|p| !p.is_empty()),
                external_ids: db::ExternalIds {
                    tmdb: Some(tmdb_id),
                    imdb: details
                        .imdb_id
                        .and_then(|s| db::NonEmptyString::try_new(s).ok()),
                    ..Default::default()
                },
                ..Default::default()
            };
            if let Some(url) = tmdb_image(
                details
                    .profile_path
                    .as_deref(),
                db::ImageKind::Primary,
            ) {
                patch.set_image(db::ImageKind::Primary, url);
            }
            return Ok(Some(patch));
        }
        _ => {}
    }

    Ok(None)
}

/// A TMDB season from the shared cache; `None` when TMDB has no such season.
async fn tmdb_cached_season(
    client: &sdks::RestClient<sdks::BearerAuth>,
    series_id: i64,
    season_number: i64,
    language: Option<String>,
) -> Result<Option<Arc<sdks::tmdb::Season>>> {
    match client
        .execute_arc(
            sdks::tmdb::SeasonEndpoint {
                series_id,
                season_number,
                language,
                append_to_response: None,
            }
            .with_cache(Duration::from_secs(360)),
        )
        .await
    {
        Ok(season) => Ok(Some(season)),
        Err(e) => {
            let e = anyhow::Error::from(e);
            if is_404(&e) { Ok(None) } else { Err(e) }
        }
    }
}

/// Whether TMDB's episode at our number is the episode we have: it carries
/// our TMDB episode id, or their air dates agree within a day (time zones),
/// or one side has no date to compare.
fn is_same_tmdb_episode(ep: &sdks::tmdb::Episode, media: &db::Media) -> bool {
    if media
        .external_ids
        .tmdb
        == Some(ep.id)
    {
        return true;
    }
    match (
        ep.air_date,
        media
            .released_at
            .map(|d| d.date()),
    ) {
        (Some(a), Some(b)) => {
            (a - b)
                .num_days()
                .abs()
                <= 1
        }
        _ => true,
    }
}

/// TMDB's episode with the TVDB episode id the episode list gave us, read
/// from TMDB's season (cached, in the metadata language). `None` without a
/// TVDB id or when TMDB doesn't know it for this series.
async fn tmdb_episode_by_tvdb(
    client: &sdks::RestClient<sdks::BearerAuth>,
    series_tmdb_id: i64,
    media: &db::Media,
    language: Option<String>,
) -> Result<Option<sdks::tmdb::Episode>> {
    let Some(tvdb) = media
        .external_ids
        .tvdb
    else {
        return Ok(None);
    };
    let found = client
        .execute_arc(
            sdks::tmdb::FindByIdEndpoint {
                external_id: tvdb.to_string(),
                external_source: "tvdb_id".to_string(),
            }
            .with_cache(Duration::from_secs(86400)),
        )
        .await?;
    let Some(hit) = found
        .tv_episode_results
        .iter()
        .find(|e| {
            e.show_id
                .is_none_or(|id| id == series_tmdb_id)
        })
    else {
        return Ok(None);
    };
    let season =
        tmdb_cached_season(client, series_tmdb_id, hit.season_number, language).await?;
    Ok(season
        .as_deref()
        .and_then(|s| {
            s.episodes
                .as_deref()
        })
        .unwrap_or_default()
        .iter()
        .find(|e| e.episode_number == hit.episode_number)
        .cloned())
}

async fn fetch_tmdb_season_meta(
    media: &db::Media,
    ctx: &AppContext,
    client: &sdks::RestClient<sdks::BearerAuth>,
    config: &crate::api::ServerConfiguration,
) -> Result<Option<db::Media>> {
    let series_tmdb_id = MediaResolveService::stored_series_tmdb_id(media, ctx).await?;

    let (Some(tmdb_id), Some(season_idx)) = (series_tmdb_id, media.idx) else {
        return Ok(None);
    };
    let tv_details = client
        .execute(
            sdks::tmdb::SeriesEndpoint::new(
                tmdb_id,
                config
                    .preferred_metadata_language
                    .clone(),
            )
            .with_cache(Duration::from_secs(360)),
        )
        .await?;
    let season_data = tv_details
        .seasons
        .iter()
        .find(|s| s.season_number == season_idx);
    let Some(season) = season_data else {
        return Ok(None);
    };
    let mut patch = db::Media::default();
    if let Some(url) = tmdb_image(
        season
            .poster_path
            .as_deref(),
        db::ImageKind::Primary,
    ) {
        patch.set_image(db::ImageKind::Primary, url);
    }
    patch.external_ids = tmdb_external_ids(
        season.id,
        season
            .external_ids
            .as_ref(),
    );
    match client
        .execute(
            sdks::tmdb::SeasonExternalIdsEndpoint {
                series_id: tmdb_id,
                season_number: season_idx,
            }
            .with_cache(Duration::from_secs(360)),
        )
        .await
    {
        Ok(ids) => patch
            .external_ids
            .merge(&tmdb_external_ids(season.id, Some(&ids)), true),
        Err(error) => {
            warn!(%error, series_id = tmdb_id, season = season_idx, "TMDB season external IDs unavailable")
        }
    }
    Ok(Some(patch))
}

// ---------------------------------------------------------------------------
// TMDB person search
// ---------------------------------------------------------------------------

async fn search_tmdb_person(
    query: &str,
    limit: usize,
    ctx: &AppContext,
) -> Result<Vec<db::Media>> {
    let config = crate::db::Settings::get_config(&ctx.db).await?;
    let client = tmdb_client(
        config.get_tmdb_key(),
        &ctx.config
            .tmdb_base_url,
    )?;

    let resp = match client
        .execute(sdks::tmdb::PersonSearchEndpoint {
            query: query.to_string(),
        })
        .await
    {
        Ok(r) => r,
        // TMDB occasionally returns a non-JSON body (HTML rate-limit/CDN page) with
        // status 200 for person searches. Treat as zero results rather than surfacing
        // a spurious WARN on every general search that includes the Person type.
        Err(ClientError::Json { ref source, .. }) => {
            debug!(error = %source, query, "tmdb person search returned non-JSON body");
            return Ok(vec![]);
        }
        Err(e) => return Err(e.into()),
    };

    let media = resp
        .results
        .into_iter()
        .take(limit)
        .map(|p| {
            let id = common::stable_media_uuid(
                &db::MediaKind::Person,
                &p.id
                    .to_string(),
            );
            let profile_url = p
                .profile_path
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| format!("{}{}", "https://image.tmdb.org/t/p/original", s));
            let mut media = db::Media {
                id,
                title: p.name,
                kind: db::MediaKind::Person,
                external_ids: db::ExternalIds {
                    tmdb: Some(p.id),
                    ..Default::default()
                },
                ..Default::default()
            };
            if let Some(url) = profile_url {
                media.set_image(db::ImageKind::Primary, url);
            }
            media
        })
        .collect();

    Ok(media)
}

async fn search_tmdb_movie(
    query: &str,
    limit: usize,
    ctx: &AppContext,
) -> Result<Vec<db::Media>> {
    let config = crate::db::Settings::get_config(&ctx.db).await?;
    let client = tmdb_client(
        config.get_tmdb_key(),
        &ctx.config
            .tmdb_base_url,
    )?;
    let resp = client
        .execute(sdks::tmdb::SearchMovieEndpoint {
            query: query.to_string(),
            year: None,
        })
        .await?;
    Ok(resp
        .results
        .into_iter()
        .take(limit)
        .map(movie_result_to_stub)
        .collect())
}

async fn search_tmdb_series(
    query: &str,
    limit: usize,
    ctx: &AppContext,
) -> Result<Vec<db::Media>> {
    let config = crate::db::Settings::get_config(&ctx.db).await?;
    let client = tmdb_client(
        config.get_tmdb_key(),
        &ctx.config
            .tmdb_base_url,
    )?;
    let resp = client
        .execute(sdks::tmdb::SearchTvEndpoint {
            query: query.to_string(),
        })
        .await?;
    Ok(resp
        .results
        .into_iter()
        .take(limit)
        .map(series_result_to_stub)
        .collect())
}

// ---------------------------------------------------------------------------
// Remote images
// ---------------------------------------------------------------------------

fn tmdb_remote_image_languages(
    image_type: Option<&api::ImageType>,
    include_all_languages: bool,
) -> Option<&'static str> {
    (!include_all_languages && matches!(image_type, Some(api::ImageType::Backdrop)))
        .then_some("null")
}

fn tmdb_remote_metadata_language(
    preferred_language: Option<&str>,
    include_all_languages: bool,
) -> Option<String> {
    (!include_all_languages)
        .then(|| preferred_language.map(str::to_string))
        .flatten()
}

fn map_remote_image(
    type_label: &str,
    entry: &sdks::tmdb::ImageEntry,
) -> api::RemoteImageInfo {
    let url = format!("https://image.tmdb.org/t/p/original{}", entry.file_path);
    let thumb = format!("https://image.tmdb.org/t/p/w300{}", entry.file_path);
    api::RemoteImageInfo {
        provider_name: Some("TheMovieDb".to_string()),
        url: Some(url),
        thumbnail_url: Some(thumb),
        type_: Some(type_label.to_string()),
        width: entry.width,
        height: entry.height,
    }
}

fn extend_from_tmdb_images(
    out: &mut Vec<api::RemoteImageInfo>,
    images: &sdks::tmdb::Images,
    language_neutral_backdrops: bool,
) {
    out.extend(
        images
            .backdrops
            .iter()
            .filter(|entry| {
                !language_neutral_backdrops
                    || entry
                        .iso_639_1
                        .is_none()
            })
            .map(|entry| map_remote_image("Backdrop", entry)),
    );
    out.extend(
        images
            .posters
            .iter()
            .map(|entry| map_remote_image("Primary", entry)),
    );
    out.extend(
        images
            .logos
            .iter()
            .map(|entry| map_remote_image("Logo", entry)),
    );
    out.extend(
        images
            .stills
            .iter()
            .filter(|entry| {
                !language_neutral_backdrops
                    || entry
                        .iso_639_1
                        .is_none()
            })
            .map(|entry| map_remote_image("Backdrop", entry)),
    );
    out.extend(
        images
            .stills
            .iter()
            .map(|entry| map_remote_image("Screenshot", entry)),
    );
    out.extend(
        images
            .stills
            .iter()
            .map(|entry| map_remote_image("Thumb", entry)),
    );
}

async fn tmdb_remote_images(
    ctx: &AppContext,
    media: &db::Media,
    options: super::ImageFetchOptions,
) -> Result<Vec<api::RemoteImageInfo>> {
    let config = crate::db::Settings::get_config(&ctx.db).await?;
    let api_key = config.get_tmdb_key();
    if api_key.is_empty() {
        return Ok(vec![]);
    }
    let client = tmdb_client(
        api_key,
        &ctx.config
            .tmdb_base_url,
    )?;
    let image_languages = tmdb_remote_image_languages(
        options
            .image_type
            .as_ref(),
        options.include_all_languages,
    );
    let language_neutral_backdrops = image_languages.is_some();
    let preferred_language = tmdb_remote_metadata_language(
        config
            .preferred_metadata_language
            .as_deref(),
        options.include_all_languages,
    );

    let mut out = Vec::new();

    match media.kind {
        db::MediaKind::Movie => {
            let tmdb_id = if let Some(id) = media
                .external_ids
                .tmdb
            {
                Some(id)
            } else if let Some((external_id, external_source)) =
                MediaResolveService::tmdb_search_key(&media.external_ids, None).await
            {
                MediaResolveService::find_tmdb_id_by(
                    external_id,
                    external_source,
                    false,
                    &client,
                )
                .await?
            } else {
                None
            };
            if let Some(tmdb_id) = tmdb_id {
                let mut endpoint =
                    sdks::tmdb::MovieEndpoint::new(tmdb_id, preferred_language.clone());
                if let Some(languages) = image_languages {
                    endpoint = endpoint.with_image_languages(languages);
                }
                let movie = client
                    .execute(endpoint.with_cache(Duration::from_secs(360)))
                    .await?;
                if let Some(images) = &movie.images {
                    extend_from_tmdb_images(
                        &mut out,
                        images,
                        language_neutral_backdrops,
                    );
                }
                if out
                    .iter()
                    .all(|i| {
                        i.type_
                            .as_deref()
                            != Some("Primary")
                    })
                {
                    if let Some(p) = &movie.poster_path {
                        out.push(api::RemoteImageInfo {
                            provider_name: Some("TheMovieDb".to_string()),
                            url: Some(format!(
                                "https://image.tmdb.org/t/p/original{p}"
                            )),
                            thumbnail_url: Some(format!(
                                "https://image.tmdb.org/t/p/w300{p}"
                            )),
                            type_: Some("Primary".to_string()),
                            width: None,
                            height: None,
                        });
                    }
                }
                if out
                    .iter()
                    .all(|i| {
                        i.type_
                            .as_deref()
                            != Some("Backdrop")
                    })
                {
                    if let Some(b) = &movie.backdrop_path {
                        out.push(api::RemoteImageInfo {
                            provider_name: Some("TheMovieDb".to_string()),
                            url: Some(format!(
                                "https://image.tmdb.org/t/p/original{b}"
                            )),
                            thumbnail_url: Some(format!(
                                "https://image.tmdb.org/t/p/w300{b}"
                            )),
                            type_: Some("Backdrop".to_string()),
                            width: None,
                            height: None,
                        });
                    }
                }
            }
        }
        db::MediaKind::Series => {
            let tmdb_id = if let Some(id) = media
                .external_ids
                .tmdb
            {
                Some(id)
            } else if let Some((external_id, external_source)) =
                MediaResolveService::tmdb_search_key(&media.external_ids, None).await
            {
                MediaResolveService::find_tmdb_id_by(
                    external_id,
                    external_source,
                    true,
                    &client,
                )
                .await?
            } else {
                None
            };
            if let Some(tmdb_id) = tmdb_id {
                let mut endpoint = sdks::tmdb::SeriesEndpoint::new(
                    tmdb_id,
                    preferred_language.clone(),
                );
                if let Some(languages) = image_languages {
                    endpoint = endpoint.with_image_languages(languages);
                }
                let tv = client
                    .execute(endpoint.with_cache(Duration::from_secs(360)))
                    .await?;
                if let Some(images) = &tv.images {
                    extend_from_tmdb_images(
                        &mut out,
                        images,
                        language_neutral_backdrops,
                    );
                }
            }
        }
        db::MediaKind::Episode => {
            let series_tmdb_id =
                MediaResolveService::stored_series_tmdb_id(media, ctx).await?;
            if let (Some(tmdb_id), Some(s_n), Some(e_n)) =
                (series_tmdb_id, media.parent_idx, media.idx)
            {
                let mut endpoint = sdks::tmdb::EpisodeEndpoint::new(
                    tmdb_id,
                    s_n,
                    e_n,
                    preferred_language,
                );
                if let Some(languages) = image_languages {
                    endpoint = endpoint.with_image_languages(languages);
                }
                let ep = client
                    .execute(endpoint.with_cache(Duration::from_secs(360)))
                    .await?;
                if let Some(images) = ep
                    .as_ref()
                    .and_then(|e| {
                        e.images
                            .as_ref()
                    })
                {
                    extend_from_tmdb_images(
                        &mut out,
                        images,
                        language_neutral_backdrops,
                    );
                }
                if out
                    .iter()
                    .all(|i| {
                        i.type_
                            .as_deref()
                            != Some("Thumb")
                    })
                {
                    if let Some(p) = ep
                        .as_ref()
                        .and_then(|e| {
                            e.still_path
                                .as_ref()
                        })
                    {
                        let url = format!("https://image.tmdb.org/t/p/original{p}");
                        let thumb = format!("https://image.tmdb.org/t/p/w300{p}");
                        out.push(api::RemoteImageInfo {
                            provider_name: Some("TheMovieDb".to_string()),
                            url: Some(url.clone()),
                            thumbnail_url: Some(thumb.clone()),
                            type_: Some("Backdrop".to_string()),
                            width: None,
                            height: None,
                        });
                        out.push(api::RemoteImageInfo {
                            provider_name: Some("TheMovieDb".to_string()),
                            url: Some(url.clone()),
                            thumbnail_url: Some(thumb.clone()),
                            type_: Some("Screenshot".to_string()),
                            width: None,
                            height: None,
                        });
                        out.push(api::RemoteImageInfo {
                            provider_name: Some("TheMovieDb".to_string()),
                            url: Some(url),
                            thumbnail_url: Some(thumb),
                            type_: Some("Thumb".to_string()),
                            width: None,
                            height: None,
                        });
                    }
                }
            }
        }
        _ => {}
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use remux_sdks::Endpoint;

    #[test]
    fn tmdb_trailers_are_youtube_trailers_and_absent_without_videos() {
        assert!(tmdb_trailers(None, Some("en-US")).is_none());
        let videos: sdks::tmdb::Videos = serde_json::from_value(serde_json::json!({"results": [
            {"key": "abc", "site": "YouTube", "type": "Trailer", "name": "Official Trailer"}
        ]}))
        .unwrap();
        assert_eq!(
            tmdb_trailers(Some(&videos), Some("en-US")),
            Some(vec![db::Trailer {
                source: db::TrailerSource::Youtube("abc".to_string()),
                name: Some("Official Trailer".to_string()),
            }])
        );
    }

    #[test]
    fn detail_requests_ask_for_videos_in_the_image_languages() {
        let endpoint = sdks::tmdb::MovieEndpoint::new(1, Some("de-DE".to_string()))
            .with_videos(thumb_and_logo_languages(Some("de-DE")));
        let query = serde_json::to_value(endpoint.query_params()).unwrap();
        assert_eq!(query["include_video_language"], "de,en,null");
        assert!(
            query["append_to_response"]
                .as_str()
                .unwrap()
                .ends_with(",videos")
        );
    }

    #[test]
    fn season_and_episode_external_ids_are_mapped() {
        let ids = sdks::tmdb::ExternalIds {
            imdb_id: Some("tt1234567".into()),
            tvdb_id: Some(456),
        };
        let season = sdks::tmdb::Season {
            id: 123,
            external_ids: Some(ids.clone()),
            ..Default::default()
        };
        let episode = sdks::tmdb::Episode {
            id: 789,
            external_ids: Some(ids),
            ..Default::default()
        };
        for (patch, tmdb) in [
            (db::Media::from(&season), 123),
            (db::Media::from(&episode), 789),
        ] {
            assert_eq!(
                patch
                    .external_ids
                    .tmdb,
                Some(tmdb)
            );
            assert_eq!(
                patch
                    .external_ids
                    .tvdb,
                Some(456)
            );
            assert_eq!(
                patch
                    .external_ids
                    .imdb
                    .as_deref()
                    .map(String::as_str),
                Some("tt1234567")
            );
        }
    }

    #[test]
    fn external_id_refresh_preserves_missing_values_and_respects_force() {
        let existing = db::ExternalIds {
            tmdb: Some(123),
            tvdb: Some(456),
            ..Default::default()
        };
        for force in [false, true] {
            let mut media = db::Media {
                external_ids: existing.clone(),
                ..Default::default()
            };
            let empty = sdks::tmdb::ExternalIds {
                imdb_id: Some(String::new()),
                tvdb_id: None,
            };
            super::super::apply_meta(
                &mut media,
                db::Media {
                    external_ids: tmdb_external_ids(123, Some(&empty)),
                    ..Default::default()
                },
                force,
            );
            assert_eq!(
                media
                    .external_ids
                    .tvdb,
                Some(456)
            );
            assert!(
                media
                    .external_ids
                    .imdb
                    .is_none()
            );
            super::super::apply_meta(
                &mut media,
                db::Media {
                    external_ids: tmdb_external_ids(
                        123,
                        Some(&sdks::tmdb::ExternalIds {
                            imdb_id: Some("tt1234567".into()),
                            tvdb_id: Some(999),
                        }),
                    ),
                    ..Default::default()
                },
                force,
            );
            assert_eq!(
                media
                    .external_ids
                    .tvdb,
                Some(if force { 999 } else { 456 })
            );
            assert_eq!(
                media
                    .external_ids
                    .imdb
                    .as_deref()
                    .map(String::as_str),
                Some("tt1234567")
            );
        }
    }

    fn image_entry(path: &str, language: Option<&str>) -> sdks::tmdb::ImageEntry {
        sdks::tmdb::ImageEntry {
            file_path: path.to_string(),
            iso_639_1: language.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn remote_backdrops_only_include_language_neutral_images() {
        let images = sdks::tmdb::Images {
            backdrops: vec![
                image_entry("/neutral.jpg", None),
                image_entry("/localized.jpg", Some("en")),
            ],
            posters: vec![
                image_entry("/localized-poster.jpg", Some("en")),
                image_entry("/neutral-poster.jpg", None),
            ],
            logos: vec![
                image_entry("/localized-logo.svg", Some("en")),
                image_entry("/neutral-logo.svg", None),
            ],
            ..Default::default()
        };
        let mut remote_images = Vec::new();

        extend_from_tmdb_images(&mut remote_images, &images, true);

        let backdrop_urls = remote_images
            .iter()
            .filter(|image| {
                image
                    .type_
                    .as_deref()
                    == Some("Backdrop")
            })
            .filter_map(|image| {
                image
                    .url
                    .as_deref()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            backdrop_urls,
            ["https://image.tmdb.org/t/p/original/neutral.jpg"]
        );
        let primary_count = remote_images
            .iter()
            .filter(|image| {
                image
                    .type_
                    .as_deref()
                    == Some("Primary")
            })
            .count();
        assert_eq!(primary_count, 2);

        let mut all_languages = Vec::new();
        extend_from_tmdb_images(&mut all_languages, &images, false);
        assert_eq!(
            all_languages
                .iter()
                .filter(|image| image
                    .type_
                    .as_deref()
                    == Some("Backdrop"))
                .count(),
            2
        );
    }

    #[test]
    fn remote_image_queries_respect_include_all_languages() {
        assert_eq!(
            tmdb_remote_image_languages(Some(&api::ImageType::Backdrop), false),
            Some("null")
        );
        assert_eq!(
            tmdb_remote_image_languages(Some(&api::ImageType::Primary), false),
            None
        );
        assert_eq!(
            tmdb_remote_image_languages(Some(&api::ImageType::Backdrop), true),
            None
        );
        assert_eq!(tmdb_remote_image_languages(None, false), None);
        assert_eq!(
            tmdb_remote_metadata_language(Some("nl-NL"), false),
            Some("nl-NL".to_string())
        );
        assert_eq!(tmdb_remote_metadata_language(Some("nl-NL"), true), None);

        let backdrop_query =
            sdks::tmdb::MovieEndpoint::new(1, Some("nl-NL".to_string()))
                .with_image_languages("null")
                .query();
        let primary_query =
            sdks::tmdb::MovieEndpoint::new(1, Some("nl-NL".to_string())).query();

        assert!(
            backdrop_query.contains(&("include_image_language".into(), "null".into()))
        );
        assert!(
            primary_query
                .iter()
                .all(|(key, _)| key != "include_image_language")
        );
    }

    /// TMDB restricts `images.backdrops`/`logos` to the request's `language`
    /// plus untagged entries, so an "en"-tagged title card never comes back
    /// unless "en" is explicitly requested — regardless of the server's
    /// configured metadata language.
    #[test]
    fn thumb_and_logo_languages_always_include_english_and_null() {
        assert_eq!(thumb_and_logo_languages(None), "en,null");
        assert_eq!(thumb_and_logo_languages(Some("en")), "en,null");
        assert_eq!(thumb_and_logo_languages(Some("nl")), "nl,en,null");
        assert_eq!(thumb_and_logo_languages(Some("nl-NL")), "nl,en,null");
    }

    /// A server whose TMDB calls go to `mock`.
    async fn ctx_with_tmdb(
        mock: &httpmock::MockServer,
    ) -> crate::integration_test::TestGuard {
        crate::integration_test::new_test_server_with_config(crate::Config {
            database_url: Some("sqlite::memory:".into()),
            torrent_http_port: None,
            disable_dht: true,
            tmdb_base_url: mock.base_url(),
            // As in `new_test_server`: no startup catalog import or library
            // refresh running against the mock while a test is.
            disable_startup_tasks: true,
            ..Default::default()
        })
        .await
        .unwrap()
        .1
    }

    fn tmdb_not_found(mock: &httpmock::MockServer, path: &str) {
        mock.mock(|when, then| {
            when.path(path);
            then.status(404)
                .json_body(serde_json::json!({
                    "success": false,
                    "status_code": 34,
                    "status_message": "The resource you requested could not be found."
                }));
        });
    }

    /// TMDB answers 404 for an entry it merged into another one, but `/find`
    /// by IMDb id already points at the surviving entry.
    #[tokio::test]
    async fn meta_fetch_follows_imdb_id_when_stored_series_tmdb_id_is_gone() {
        let tmdb = httpmock::MockServer::start();
        tmdb_not_found(&tmdb, "/tv/1001");
        tmdb.mock(|when, then| {
            when.path("/find/tt7770010")
                .query_param("external_source", "imdb_id");
            then.status(200)
                .json_body(serde_json::json!({
                    "tv_results": [{ "id": 2002, "name": "Show" }],
                    "movie_results": []
                }));
        });
        tmdb.mock(|when, then| {
            when.path("/tv/2002");
            then.status(200)
                .json_body(serde_json::json!({
                    "id": 2002,
                    "name": "Show",
                    "overview": "From the surviving entry.",
                    "external_ids": { "imdb_id": "tt7770010", "tvdb_id": 3003 }
                }));
        });
        let guard = ctx_with_tmdb(&tmdb).await;
        let media = db::Media {
            kind: db::MediaKind::Series,
            title: "Show".to_string(),
            external_ids: db::ExternalIds {
                tmdb: Some(1001),
                imdb: db::NonEmptyString::try_new("tt7770010").ok(),
                ..Default::default()
            },
            ..Default::default()
        };

        let patch = TmdbAddon
            .meta_fetch(&media, &guard.0, &Default::default())
            .await
            .unwrap()
            .expect("re-resolved patch");

        assert_eq!(
            patch
                .external_ids
                .tmdb,
            Some(2002)
        );
        assert_eq!(
            patch
                .description
                .as_deref(),
            Some("From the surviving entry.")
        );

        // A full refresh stores the new id in place of the dead one.
        let mut stored = media.clone();
        crate::addons::apply_meta(&mut stored, patch, true);
        assert_eq!(
            stored
                .external_ids
                .tmdb,
            Some(2002)
        );
    }

    /// Without an IMDb match the TVDB id is tried next; a movie goes through
    /// the same path as a series.
    #[tokio::test]
    async fn meta_fetch_falls_back_to_tvdb_id_when_stored_movie_tmdb_id_is_gone() {
        let tmdb = httpmock::MockServer::start();
        tmdb_not_found(&tmdb, "/movie/1001");
        tmdb.mock(|when, then| {
            when.path("/find/tt7770011");
            then.status(200)
                .json_body(
                    serde_json::json!({ "tv_results": [], "movie_results": [] }),
                );
        });
        tmdb.mock(|when, then| {
            when.path("/find/3004")
                .query_param("external_source", "tvdb_id");
            then.status(200)
                .json_body(serde_json::json!({
                    "tv_results": [],
                    "movie_results": [{
                        "id": 2004,
                        "title": "Film",
                        "adult": false,
                        "original_language": "en"
                    }]
                }));
        });
        tmdb.mock(|when, then| {
            when.path("/movie/2004");
            then.status(200)
                .json_body(serde_json::json!({
                    "id": 2004,
                    "title": "Film",
                    "adult": false,
                    "original_language": "en"
                }));
        });
        let guard = ctx_with_tmdb(&tmdb).await;
        let media = db::Media {
            kind: db::MediaKind::Movie,
            title: "Film".to_string(),
            external_ids: db::ExternalIds {
                tmdb: Some(1001),
                imdb: db::NonEmptyString::try_new("tt7770011").ok(),
                tvdb: Some(3004),
                ..Default::default()
            },
            ..Default::default()
        };

        let patch = TmdbAddon
            .meta_fetch(&media, &guard.0, &Default::default())
            .await
            .unwrap()
            .expect("re-resolved patch");

        assert_eq!(
            patch
                .external_ids
                .tmdb,
            Some(2004)
        );
    }

    /// Nothing else resolves: keep returning no patch so lower-priority meta
    /// addons can still fill the item.
    #[tokio::test]
    async fn meta_fetch_returns_none_when_gone_tmdb_id_cannot_be_re_resolved() {
        let tmdb = httpmock::MockServer::start();
        tmdb_not_found(&tmdb, "/tv/1001");
        tmdb.mock(|when, then| {
            when.path("/find/tt7770012");
            then.status(200)
                .json_body(
                    serde_json::json!({ "tv_results": [], "movie_results": [] }),
                );
        });
        let guard = ctx_with_tmdb(&tmdb).await;
        let media = db::Media {
            kind: db::MediaKind::Series,
            external_ids: db::ExternalIds {
                tmdb: Some(1001),
                imdb: db::NonEmptyString::try_new("tt7770012").ok(),
                ..Default::default()
            },
            ..Default::default()
        };

        assert!(
            TmdbAddon
                .meta_fetch(&media, &guard.0, &Default::default())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn season_endpoint_sends_its_language() {
        let ep = sdks::tmdb::SeasonEndpoint {
            series_id: 1,
            season_number: 2,
            language: Some("de".into()),
            append_to_response: None,
        };
        assert_eq!(ep.path(), "tv/1/season/2");
        assert_eq!(ep.query(), vec![("language".to_string(), "de".to_string())]);
        let ep = sdks::tmdb::SeasonEndpoint {
            language: None,
            ..ep
        };
        assert!(
            ep.query()
                .is_empty()
        );
    }

    fn tmdb_episode(number: i64, name: &str, overview: &str) -> sdks::tmdb::Episode {
        sdks::tmdb::Episode {
            id: 1000 + number,
            name: name.into(),
            overview: Some(overview.into()),
            season_number: 1,
            episode_number: number,
            ..Default::default()
        }
    }

    fn translations(entries: &[(&str, &str, &str, &str)]) -> serde_json::Value {
        serde_json::json!({
            "id": 1,
            "translations": entries
                .iter()
                .map(|(lang, region, name, overview)| serde_json::json!({
                    "iso_639_1": lang, "iso_3166_1": region,
                    "data": { "name": name, "overview": overview }
                }))
                .collect::<Vec<_>>()
        })
    }

    /// TMDB answers a localized request for an episode nobody translated with
    /// a made-up name ("Folge 2") and an empty overview. Only for such an
    /// episode (empty overview) its translations decide: without a name in
    /// the metadata language, the episode keeps its English name (or else
    /// the one in the original language) and reports that language, so it
    /// is never taken for a German title. With a name in the metadata
    /// language the name stays and an empty overview is filled from the
    /// English (else original-language) translation; with no name anywhere,
    /// TMDB's answer stays.
    #[tokio::test]
    async fn untranslated_tmdb_episodes_keep_their_english_name() {
        let tmdb = httpmock::MockServer::start();
        let base = "/tv/900101/season/1/episode";
        let mut mocks = Vec::new();
        for (episode, body) in [
            (
                2,
                translations(&[
                    ("de", "DE", "", ""),
                    ("en", "GB", "Promise (GB)", ""),
                    ("en", "US", "Each One's Promise", "Subaru wakes up."),
                    ("ja", "JP", "それぞれの誓い", ""),
                ]),
            ),
            (
                3,
                translations(&[
                    ("de", "AT", "Dritte (AT)", ""),
                    ("de", "DE", "Dritte", ""),
                    ("en", "US", "Third", "Third overview."),
                ]),
            ),
            (
                4,
                translations(&[
                    ("de", "DE", "", ""),
                    ("en", "US", "", ""),
                    ("ja", "JP", "第四話", "あらすじ"),
                ]),
            ),
            (
                5,
                translations(&[("de", "DE", "", ""), ("en", "US", "", "")]),
            ),
        ] {
            mocks.push(tmdb.mock(|when, then| {
                when.path(format!("{base}/{episode}/translations"));
                then.status(200)
                    .json_body(body);
            }));
        }
        let untouched = tmdb.mock(|when, then| {
            when.path(format!("{base}/1/translations"));
            then.status(200)
                .json_body(translations(&[]));
        });
        let client = tmdb_client("test", &tmdb.base_url()).unwrap();
        let mut got = Vec::new();
        for ep in [
            tmdb_episode(1, "Das Ende vom Anfang", "Subaru kommt an."),
            tmdb_episode(2, "Folge 2", ""),
            tmdb_episode(3, "Folge 3", ""),
            tmdb_episode(4, "Folge 4", ""),
            tmdb_episode(5, "Folge 5", ""),
        ] {
            let m = tmdb_episode_meta(&client, 900101, &ep, Some("de-DE"), Some("ja"))
                .await
                .unwrap();
            got.push((m.title, m.description, m.result_language));
        }
        let s = |v: &str| Some(v.to_string());
        assert_eq!(
            got,
            vec![
                (
                    "Das Ende vom Anfang".into(),
                    s("Subaru kommt an."),
                    s("de-DE")
                ),
                ("Each One's Promise".into(), s("Subaru wakes up."), s("en")),
                ("Dritte".into(), s("Third overview."), s("de-DE")),
                ("第四話".into(), s("あらすじ"), s("ja")),
                ("Folge 5".into(), None, s("de-DE")),
            ]
        );
        untouched.assert_hits(0);
        for m in &mocks {
            m.assert_hits(1);
        }
    }

    /// Without a metadata language TMDB answers in its default language as
    /// before, and no translations are asked for.
    #[tokio::test]
    async fn tmdb_episodes_without_a_metadata_language_are_unchanged() {
        let tmdb = httpmock::MockServer::start();
        let translations_mock = tmdb.mock(|when, then| {
            when.path_contains("/translations");
            then.status(200)
                .json_body(translations(&[]));
        });
        let client = tmdb_client("test", &tmdb.base_url()).unwrap();
        let ep = tmdb_episode(2, "Episode 2", "");
        let m = tmdb_episode_meta(&client, 900102, &ep, None, Some("ja"))
            .await
            .unwrap();
        let mut expected = db::Media::from(&ep);
        expected.refreshed_at = m.refreshed_at;
        assert_eq!(
            (m.title, m.description, m.result_language),
            (expected.title, expected.description, None)
        );
        translations_mock.assert_hits(0);
    }

    /// The per-episode refresh goes the same way, and reports the language
    /// of its result: the metadata language, or the one the name came in.
    #[tokio::test]
    async fn tmdb_episode_refresh_reports_the_language_of_its_name() {
        let tmdb = httpmock::MockServer::start();
        tmdb.mock(|when, then| {
            when.path("/tv/900103/season/0")
                .query_param("language", "de");
            then.status(200)
                .json_body(serde_json::json!({
                    "id": 9001030, "season_number": 0, "name": "Specials",
                    "episodes": [
                        { "id": 1, "name": "Folge 1", "overview": "", "season_number": 0, "episode_number": 1 },
                        { "id": 2, "name": "Hinter den Kulissen", "overview": "Ein Blick.",
                          "season_number": 0, "episode_number": 2 }
                    ]
                }));
        });
        tmdb.mock(|when, then| {
            when.path("/tv/900103/season/0/episode/1/translations");
            then.status(200)
                .json_body(translations(&[
                    ("de", "DE", "", ""),
                    ("en", "US", "Inside the Show", "A look."),
                ]));
        });
        let guard = ctx_with_tmdb(&tmdb).await;
        let series = Arc::new(db::Media {
            kind: db::MediaKind::Series,
            original_language: Some("en".into()),
            external_ids: db::ExternalIds {
                tmdb: Some(900103),
                ..Default::default()
            },
            ..Default::default()
        });
        let config = crate::api::ServerConfiguration {
            preferred_metadata_language: Some("de".into()),
            ..Default::default()
        };
        let mut got = Vec::new();
        for idx in [1, 2] {
            let episode = db::Media {
                kind: db::MediaKind::Episode,
                parent_idx: Some(0),
                idx: Some(idx),
                grandparent: Some(series.clone()),
                ..Default::default()
            };
            let patch = TmdbAddon
                .meta_fetch(&episode, &guard.0, &config)
                .await
                .unwrap()
                .expect("patch");
            got.push((patch.title, patch.result_language));
        }
        assert_eq!(
            got,
            vec![
                ("Inside the Show".to_string(), Some("en".to_string())),
                ("Hinter den Kulissen".to_string(), Some("de".to_string())),
            ]
        );
    }

    type EpisodeRow = (
        Option<i64>,
        Option<i64>,
        String,
        Option<String>,
        Option<i64>,
    );

    /// Cinemeta (ranked above TMDB) gives Re:Zero IMDb's seasons, which is
    /// also how stream addons number episodes; TMDB keeps them in one season
    /// (IMDb S2E1 = TMDB S1E2 here). Returns the episodes (season, number,
    /// title, overview, TMDB id) and the series as the last row.
    async fn rezero_with_cinemeta(
        cinemeta_language: Option<&str>,
        preferred_language: Option<&str>,
    ) -> Vec<EpisodeRow> {
        let cinemeta = httpmock::MockServer::start();
        // Own path: the HTTP response cache is process-wide and keyed by URL,
        // and a parallel test run reuses mock servers' ports.
        let cinemeta_prefix = format!("/{}", Uuid::new_v4().simple());
        cinemeta.mock(|when, then| {
            when.path(format!("{cinemeta_prefix}/manifest.json"));
            then.status(200)
                .json_body(serde_json::json!({
                    "id": "cinemeta-test", "name": "Cinemeta", "version": "1.0.0",
                    "resources": ["meta"], "types": ["series"], "idPrefixes": ["tt"], "catalogs": []
                }));
        });
        cinemeta.mock(|when, then| {
            when.path(format!("{cinemeta_prefix}/meta/series/tt5607616.json"));
            then.status(200)
                .json_body(serde_json::json!({ "meta": {
                    "id": "tt5607616", "type": "series", "name": "Re:ZERO",
                    "description": "English overview.",
                    "videos": [
                        { "id": "tt5607616:0:1", "name": "Inside Re:ZERO", "season": 0, "episode": 1,
                          "overview": "A look behind.", "released": "2016-12-01T00:00:00.000Z", "tvdb_id": 5637300 },
                        { "id": "tt5607616:1:1", "name": "The End of the Beginning", "season": 1, "episode": 1,
                          "overview": "Subaru arrives.", "released": "2016-04-04T00:00:00.000Z", "tvdb_id": 5637311 },
                        { "id": "tt5607616:2:1", "name": "Each One's Promise", "season": 2, "episode": 1,
                          "overview": "Subaru wakes up.", "released": "2020-07-08T00:00:00.000Z", "tvdb_id": 7546792 }
                    ]
                }}));
        });
        let tmdb = httpmock::MockServer::start();
        tmdb.mock(|when, then| {
            when.path("/tv/65942");
            then.status(200)
                .json_body(serde_json::json!({
                    "id": 65942, "name": "Re:ZERO DE", "overview": "Deutsche Beschreibung.",
                    "original_language": "ja",
                    "seasons": [{ "id": 77001, "season_number": 1, "name": "Staffel 1", "episode_count": 2 }]
                }));
        });
        tmdb.mock(|when, then| {
            when.path("/tv/65942/season/0");
            then.status(200)
                .json_body(serde_json::json!({
                    "id": 77000, "season_number": 0, "name": "Specials",
                    "episodes": [
                        { "id": 1176800, "name": "Folge 1", "season_number": 0, "episode_number": 1,
                          "overview": "", "air_date": "2016-12-01" }
                    ]
                }));
        });
        tmdb.mock(|when, then| {
            when.path("/tv/65942/season/0/episode/1/translations");
            then.status(200)
                .json_body(translations(&[
                    ("de", "DE", "", ""),
                    ("en", "US", "Inside Re:ZERO (TMDB)", "Behind the scenes."),
                ]));
        });
        tmdb.mock(|when, then| {
            when.path("/tv/65942/season/1");
            then.status(200)
                .json_body(serde_json::json!({
                    "id": 77001, "season_number": 1, "name": "Staffel 1",
                    "episodes": [
                        { "id": 1176875, "name": "Das Ende vom Anfang", "season_number": 1, "episode_number": 1,
                          "overview": "Subaru kommt an.", "air_date": "2016-04-04" },
                        { "id": 4319552, "name": "Verschiedene Versprechen", "season_number": 1, "episode_number": 2,
                          "overview": "Subaru wacht auf.", "air_date": "2020-07-08" }
                    ]
                }));
        });
        tmdb_not_found(&tmdb, "/tv/65942/season/2");
        tmdb.mock(|when, then| {
            when.path("/find/7546792")
                .query_param("external_source", "tvdb_id");
            then.status(200)
                .json_body(serde_json::json!({
                    "tv_episode_results": [{ "id": 4319552, "name": "Each One's Promise", "show_id": 65942,
                                             "season_number": 1, "episode_number": 2 }]
                }));
        });

        let guard = ctx_with_tmdb(&tmdb).await;
        let ctx = &guard.0;
        let mut config = db::Settings::get_config_or_default(&ctx.db).await;
        config.preferred_metadata_language = preferred_language.map(str::to_string);
        // A busy test run must not turn a mock's slow answer into a fallback.
        config.addon_fetch_timeout_secs = Some(60);
        db::Settings::set_config(&ctx.db, &config)
            .await
            .unwrap();
        let now = chrono::Utc::now().naive_utc();
        let mut addon_cfg = serde_json::json!({
            "manifest_url": format!("{}{cinemeta_prefix}/manifest.json", cinemeta.base_url())
        });
        if let Some(language) = cinemeta_language {
            addon_cfg["language"] = language.into();
        }
        crate::addons::Addon {
            id: Uuid::new_v4(),
            name: "Cinemeta".into(),
            preset: serde_json::from_value(
                serde_json::json!({ "kind": "stremio", "config": addon_cfg }),
            )
            .unwrap(),
            resources: vec![remux_sdks::stremio::ResourceType::Meta],
            types: vec![],
            enabled: true,
            priority: -1,
            created_at: now,
            updated_at: now,
            system: false,
            is_default: true,
            http_redirect_stream: false,
            subtitle_extraction: false,
            probe_on_scan: false,
            service_filter: vec![],
        }
        .insert(&ctx.db)
        .await
        .unwrap();
        ctx.addons
            .reload(&ctx.db, &ctx.config)
            .await
            .unwrap();

        let mut series = db::Media {
            id: Uuid::new_v4(),
            kind: db::MediaKind::Series,
            title: "Re:ZERO".into(),
            external_ids: db::ExternalIds {
                tmdb: Some(65942),
                imdb: db::NonEmptyString::try_new("tt5607616").ok(),
                ..Default::default()
            },
            ..Default::default()
        };
        series
            .save(&ctx.db)
            .await
            .unwrap();
        let ids = ctx
            .addons
            .process_meta_batch(vec![series.clone()], ctx, false, None)
            .await
            .unwrap();
        let series_id = ids[&series.id];

        let mut rows: Vec<EpisodeRow> = sqlx::query_as(
            "SELECT parent_idx, idx, title, description, CAST(json_extract(external_ids, '$.tmdb') AS INTEGER) \
             FROM media WHERE kind = 'episode' AND grandparent_id = ? ORDER BY parent_idx, idx",
        )
        .bind(series_id)
        .fetch_all(&ctx.db)
        .await
        .unwrap();
        let stored = db::Media::get_by_id(&ctx.db, &series_id)
            .await
            .unwrap()
            .unwrap();
        rows.push((None, None, stored.title, stored.description, None));
        rows
    }

    fn row(
        season: Option<i64>,
        number: Option<i64>,
        title: &str,
        overview: &str,
        tmdb: Option<i64>,
    ) -> EpisodeRow {
        (season, number, title.into(), Some(overview.into()), tmdb)
    }

    /// With German as metadata language and the Cinemeta addon marked as
    /// English, overviews are only a fallback (Jellyfin's rule) and TMDB's
    /// German overview replaces them; TMDB finds its own episode by TVDB id
    /// where it numbers it differently. Titles and numbering stay Cinemeta's.
    /// Where TMDB has no German text (the special), Cinemeta's stays.
    #[tokio::test]
    async fn english_addon_overviews_give_way_to_tmdb_in_the_metadata_language() {
        assert_eq!(
            rezero_with_cinemeta(Some("en"), Some("de")).await,
            vec![
                row(
                    Some(0),
                    Some(1),
                    "Inside Re:ZERO",
                    "A look behind.",
                    Some(1176800)
                ),
                row(
                    Some(1),
                    Some(1),
                    "The End of the Beginning",
                    "Subaru kommt an.",
                    Some(1176875)
                ),
                row(
                    Some(2),
                    Some(1),
                    "Each One's Promise",
                    "Subaru wacht auf.",
                    Some(4319552)
                ),
                row(None, None, "Re:ZERO", "Deutsche Beschreibung.", None),
            ]
        );
    }

    /// Without a language on the addon nothing changes from before.
    #[tokio::test]
    async fn addon_without_a_language_keeps_todays_behavior() {
        assert_eq!(
            rezero_with_cinemeta(None, Some("de")).await,
            vec![
                row(Some(0), Some(1), "Inside Re:ZERO", "A look behind.", None),
                row(
                    Some(1),
                    Some(1),
                    "The End of the Beginning",
                    "Subaru arrives.",
                    None
                ),
                row(
                    Some(2),
                    Some(1),
                    "Each One's Promise",
                    "Subaru wakes up.",
                    None
                ),
                row(None, None, "Re:ZERO", "English overview.", None),
            ]
        );
    }

    /// No metadata language configured: every result matches, nothing is a
    /// fallback, the addon priority decides as before.
    #[tokio::test]
    async fn no_metadata_language_keeps_the_addon_priority() {
        assert_eq!(
            rezero_with_cinemeta(Some("en"), None).await,
            rezero_with_cinemeta(None, Some("de")).await
        );
    }

    /// TMDB numbers an episode the addon's list has elsewhere: its episode is
    /// found by TVDB id and the numbering stays the list's.
    #[tokio::test]
    async fn tmdb_episode_meta_follows_the_tvdb_id_when_numbers_differ() {
        let tmdb = httpmock::MockServer::start();
        tmdb.mock(|when, then| {
            when.path("/tv/65943/season/1");
            then.status(200)
                .json_body(serde_json::json!({
                    "id": 77001, "season_number": 1, "name": "Staffel 1",
                    "episodes": [
                        { "id": 4319552, "name": "Verschiedene Versprechen", "season_number": 1, "episode_number": 26,
                          "overview": "Subaru wacht auf.", "air_date": "2020-07-08" }
                    ]
                }));
        });
        tmdb_not_found(&tmdb, "/tv/65943/season/2");
        tmdb.mock(|when, then| {
            when.path("/find/7546799");
            then.status(200)
                .json_body(serde_json::json!({
                    "tv_episode_results": [{ "id": 4319552, "name": "x", "show_id": 65943,
                                             "season_number": 1, "episode_number": 26 }]
                }));
        });
        let guard = ctx_with_tmdb(&tmdb).await;
        let series = Arc::new(db::Media {
            kind: db::MediaKind::Series,
            external_ids: db::ExternalIds {
                tmdb: Some(65943),
                ..Default::default()
            },
            ..Default::default()
        });
        let episode = db::Media {
            kind: db::MediaKind::Episode,
            parent_idx: Some(2),
            idx: Some(1),
            released_at: chrono::NaiveDate::from_ymd_opt(2020, 7, 8)
                .and_then(|d| d.and_hms_opt(0, 0, 0)),
            external_ids: db::ExternalIds {
                tvdb: Some(7546799),
                ..Default::default()
            },
            grandparent: Some(series),
            ..Default::default()
        };
        let patch = TmdbAddon
            .meta_fetch(&episode, &guard.0, &Default::default())
            .await
            .unwrap()
            .expect("patch");
        assert_eq!(patch.title, "Verschiedene Versprechen");
        assert_eq!(
            patch
                .external_ids
                .tmdb,
            Some(4319552)
        );
        assert_eq!((patch.parent_idx, patch.idx), (None, None));
    }

    /// TMDB's episode at our number with our TMDB id is ours, even when TMDB
    /// has since moved its air date.
    #[test]
    fn an_episode_with_our_tmdb_id_is_ours_whatever_its_air_date() {
        let ep = sdks::tmdb::Episode {
            id: 42,
            air_date: chrono::NaiveDate::from_ymd_opt(2024, 1, 10),
            ..Default::default()
        };
        let mut media = db::Media {
            released_at: chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
                .and_then(|d| d.and_hms_opt(0, 0, 0)),
            ..Default::default()
        };
        assert!(!is_same_tmdb_episode(&ep, &media));
        media
            .external_ids
            .tmdb = Some(42);
        assert!(is_same_tmdb_episode(&ep, &media));
        media.released_at = None;
        media
            .external_ids
            .tmdb = None;
        assert!(is_same_tmdb_episode(&ep, &media));
    }
}

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{Context, Entity, Task};
use music::{Genre, GenreDetail, GenreSection, MusicApi};
use serde::Deserialize;
use tokio::task::AbortHandle;

use crate::{Io, Session, SessionEvent, Shelf, join, trim_heap};

/// How many bare genres get a picture fetched for them, which covers the first screens of tiles.
const DRESSED: usize = 48;
/// How many of those pictures are asked for at once.
const AT_ONCE: usize = 4;
/// How long the pictures found for a provider's genres are reused before they are looked up again.
const KEEP_COVERS: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The pictures found for a provider's genres by genre id, with `None` for a genre known to have
/// none.
type Covers = HashMap<String, Option<String>>;

/// The genres the search page browses. Nothing is fetched until a screen calls `load`, and a
/// sign-in fetches them again only after that.
pub struct Genres {
    genres: Rc<Vec<Genre>>,
    /// Whether a screen has asked for the genres, so a sign-in knows to fetch them.
    wanted: bool,
    loading: bool,
    error: Option<String>,
    session: Entity<Session>,
    io: Io,
    cache: storage::Cache,
    task: Option<Task<()>>,
    /// Fetches the pictures of genres the provider listed without one.
    dressing: Option<Task<()>>,
    generation: u64,
}

/// The pictures a provider's genres were last dressed with, as `cache.sqlite` keeps them.
#[derive(Deserialize)]
struct Stored {
    saved: u64,
    covers: Covers,
}

impl Genres {
    pub fn new(session: Entity<Session>, io: Io, cx: &mut Context<Self>) -> Self {
        cx.subscribe(&session, |this, _, event, cx| match event {
            SessionEvent::SignedIn(_) | SessionEvent::Reconnected(_) if this.wanted => {
                this.reload(cx);
            }
            SessionEvent::SignedIn(_) | SessionEvent::Reconnected(_) => {}
            SessionEvent::SignedOut(slug) => {
                this.dressing = None;
                this.drop_account(slug, cx);
            }
            SessionEvent::WentOffline
            | SessionEvent::LocalChanged
            | SessionEvent::RekordboxChanged => {}
        })
        .detach();

        Self {
            genres: Rc::new(Vec::new()),
            wanted: false,
            loading: false,
            error: None,
            session,
            io,
            cache: storage::Cache::standard(),
            task: None,
            dressing: None,
            generation: 0,
        }
    }

    pub fn genres(&self) -> Rc<Vec<Genre>> {
        self.genres.clone()
    }

    pub fn adopt(&mut self, id: &str, cover: Option<String>, cx: &mut Context<Self>) {
        let Some(cover) = cover else {
            return;
        };
        let Some(genre) = Rc::make_mut(&mut self.genres)
            .iter_mut()
            .find(|genre| genre.id == id && genre.cover.is_none())
        else {
            return;
        };

        genre.cover = Some(cover);
        cx.notify();
    }

    pub fn forget(&mut self, id: &str, cx: &mut Context<Self>) {
        let kept = self.genres.len();
        Rc::make_mut(&mut self.genres).retain(|genre| genre.id != id);
        if self.genres.len() != kept {
            cx.notify();
        }
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Fetches the genres unless they are loaded or loading. Before a sign-in it only marks them
    /// wanted, and the sign-in fetches them.
    pub fn load(&mut self, cx: &mut Context<Self>) {
        self.wanted = true;
        if self.loading || !self.genres.is_empty() {
            return;
        }
        self.reload(cx);
    }

    fn drop_account(&mut self, slug: &str, cx: &mut Context<Self>) {
        let before = self.genres.len();
        Rc::make_mut(&mut self.genres)
            .retain(|genre| self.session.read(cx).slug_for(&genre.id) != Some(slug));
        if self.genres.len() != before {
            cx.notify();
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let clients = streaming_clients(self.session.read(cx));
        if clients.is_empty() {
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        if self.genres.is_empty() {
            self.loading = true;
            self.error = None;
            cx.notify();
        }

        let io = self.io.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let mut merged = Vec::new();
            let mut failed = None;
            for client in clients {
                match join(io.spawn(async move { client.genres().await })).await {
                    Ok(genres) => merged.extend(genres),
                    Err(error) => failed = Some(error),
                }
            }

            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                this.task = None;
                {
                    let session = this.session.read(cx);
                    merged.retain(|genre| {
                        session
                            .slug_for(&genre.id)
                            .is_some_and(|slug| session.account_authenticated(slug))
                    });
                }
                match failed {
                    Some(error) if merged.is_empty() => {
                        this.error = Some(crate::settled::<()>(Err(error), cx).unwrap_err());
                    }
                    _ => {
                        this.error = None;
                        this.genres = Rc::new(merged);
                        this.dress(cx);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Puts pictures on the first genres each provider listed without one. Pictures stored for a
    /// provider within `KEEP_COVERS` go on at once, and the rest are fetched a few at a time and
    /// stored for the next launch. A genre whose picture cannot be found keeps the placeholder.
    fn dress(&mut self, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        let mut listed: HashMap<&'static str, (Arc<dyn MusicApi>, Vec<String>)> = HashMap::new();
        for genre in self
            .genres
            .iter()
            .filter(|genre| genre.cover.is_none())
            .take(DRESSED)
        {
            let Some(slug) = session.slug_for(&genre.id) else {
                continue;
            };
            let Some(client) = session.client_of(Shelf::Account(slug)) else {
                continue;
            };
            listed
                .entry(slug)
                .or_insert_with(|| (client, Vec::new()))
                .1
                .push(genre.id.clone());
        }
        if listed.is_empty() {
            return;
        }

        let cache = self.cache.clone();
        let io = self.io.clone();
        self.dressing = Some(cx.spawn(async move |this, cx| {
            let mut known: HashMap<&'static str, Covers> = HashMap::new();
            let mut bare: Vec<(&'static str, Arc<dyn MusicApi>, Vec<String>)> = Vec::new();
            for (slug, (client, ids)) in listed {
                let stored = {
                    let cache = cache.clone();
                    io.spawn_blocking(move || remembered(&cache, slug))
                        .await
                        .unwrap_or_default()
                };
                let mut kept = Covers::new();
                let mut missing = Vec::new();
                for id in ids {
                    match stored.get(&id) {
                        Some(cover) => {
                            kept.insert(id, cover.clone());
                        }
                        None => missing.push(id),
                    }
                }
                let adopted = kept.clone();
                let applied = this.update(cx, |this, cx| {
                    for (id, cover) in &adopted {
                        this.adopt(id, cover.clone(), cx);
                    }
                });
                if applied.is_err() {
                    return;
                }
                known.insert(slug, kept);
                if !missing.is_empty() {
                    bare.push((slug, client, missing));
                }
            }

            for (slug, client, ids) in bare {
                for batch in ids.chunks(AT_ONCE) {
                    let asked = batch.iter().map(|id| {
                        let client = client.clone();
                        let id = id.clone();
                        join(io.spawn(async move { client.genre_cover(&id).await }))
                    });
                    let found = futures::future::join_all(asked).await;
                    let applied = this.update(cx, |this, cx| {
                        let known = known.entry(slug).or_default();
                        for (id, cover) in batch.iter().zip(found) {
                            match cover {
                                Ok(cover) => {
                                    this.adopt(id, cover.clone(), cx);
                                    known.insert(id.clone(), cover);
                                }
                                Err(error) => log::debug!("genres: no picture for {id}: {error:#}"),
                            }
                        }
                    });
                    if applied.is_err() {
                        return;
                    }
                }
            }

            // A provider may answer with whole pages to find one picture each, and what parsing
            // them freed would otherwise stay resident.
            io.spawn_blocking(move || {
                for (slug, covers) in known {
                    remember(&cache, slug, &covers);
                }
                trim_heap();
            })
            .await
            .ok();
        }));
    }
}

pub struct GenreDetails {
    id: Option<String>,
    detail: Option<Arc<GenreDetail>>,
    sections: Rc<Vec<GenreSection>>,
    loading: bool,
    error: Option<String>,
    session: Entity<Session>,
    genres: Entity<Genres>,
    io: Io,
    task: Option<Task<()>>,
    request: Option<AbortHandle>,
}

impl GenreDetails {
    pub fn new(
        session: Entity<Session>,
        genres: Entity<Genres>,
        io: Io,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.subscribe(&session, |this, _, event, cx| match event {
            SessionEvent::SignedIn(slug) | SessionEvent::Reconnected(slug) => {
                let slug = *slug;
                if let Some(id) = this
                    .id
                    .clone()
                    .filter(|id| this.session.read(cx).slug_for(id) == Some(slug))
                {
                    this.clear();
                    this.open(&id, cx);
                }
            }
            SessionEvent::SignedOut(slug) => {
                let slug = *slug;
                if this
                    .id
                    .as_deref()
                    .is_some_and(|id| this.session.read(cx).slug_for(id) == Some(slug))
                {
                    this.clear();
                    cx.notify();
                }
            }
            SessionEvent::WentOffline
            | SessionEvent::LocalChanged
            | SessionEvent::RekordboxChanged => {}
        })
        .detach();

        Self {
            id: None,
            detail: None,
            sections: Rc::new(Vec::new()),
            loading: false,
            error: None,
            session,
            genres,
            io,
            task: None,
            request: None,
        }
    }

    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    pub fn name(&self) -> Option<&str> {
        self.detail.as_ref().map(|detail| detail.name.as_str())
    }

    pub fn sections(&self) -> Rc<Vec<GenreSection>> {
        self.sections.clone()
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn open(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.id.as_deref() == Some(id) && (self.loading || self.detail.is_some()) {
            return;
        }

        self.clear();
        self.id = Some(id.to_owned());
        let Some(catalog) = self.session.read(cx).catalog(id) else {
            cx.notify();
            return;
        };
        if let Some(detail) = catalog.peek_genre(id) {
            self.adopt(id, detail, cx);
            cx.notify();
            return;
        }
        self.load(id.to_owned(), false, cx);
    }

    /// Drops the page and fetches it again from the provider, past the catalog cache, so it
    /// shows its loading state until the new sections land.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.id.clone() else {
            return;
        };
        self.clear();
        self.id = Some(id.clone());
        self.load(id, true, cx);
    }

    /// Fetches the genre's sections. A `fresh` load drops the cached genre first, so the
    /// provider is asked again.
    fn load(&mut self, id: String, fresh: bool, cx: &mut Context<Self>) {
        let Some(catalog) = self.session.read(cx).catalog(&id) else {
            cx.notify();
            return;
        };

        self.loading = true;
        cx.notify();

        let request = self.io.spawn({
            let id = id.clone();
            async move {
                if fresh {
                    catalog.invalidate_genre(&id).await;
                }
                catalog.genre(&id).await
            }
        });
        self.request = Some(request.abort_handle());
        self.task = Some(cx.spawn(async move |this, cx| {
            let loaded = join(request).await;

            this.update(cx, |this, cx| {
                if this.id.as_deref() != Some(id.as_str()) {
                    return;
                }

                this.loading = false;
                this.request = None;
                match crate::settled(loaded, cx) {
                    Ok(detail) => this.adopt(&id, detail, cx),
                    Err(reason) => this.error = Some(reason),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn adopt(&mut self, id: &str, detail: Arc<GenreDetail>, cx: &mut Context<Self>) {
        let cover = detail.cover();
        self.genres
            .update(cx, |genres, cx| match detail.sections.is_empty() {
                true => genres.forget(id, cx),
                false => genres.adopt(id, cover, cx),
            });
        self.sections = Rc::new(detail.sections.clone());
        self.detail = Some(detail);
    }

    fn clear(&mut self) {
        self.task = None;
        if let Some(request) = self.request.take() {
            request.abort();
        }
        self.id = None;
        self.detail = None;
        self.sections = Rc::new(Vec::new());
        self.loading = false;
        self.error = None;
    }
}

/// The genre pictures stored for the provider `slug`, or none when they are missing, unreadable
/// or older than `KEEP_COVERS`. It blocks on sqlite.
fn remembered(cache: &storage::Cache, slug: &str) -> Covers {
    let stored = cache.read(&covers_key(slug), |text| {
        serde_json::from_str::<Stored>(text).ok()
    });
    match stored {
        Ok(Some(Some(stored))) if now().saturating_sub(stored.saved) < KEEP_COVERS.as_secs() => {
            stored.covers
        }
        Ok(_) => Covers::new(),
        Err(error) => {
            log::warn!("genres: {error:#}");
            Covers::new()
        }
    }
}

/// Stores the genre pictures found for the provider `slug`, replacing what was kept before. It
/// blocks on sqlite.
fn remember(cache: &storage::Cache, slug: &str, covers: &Covers) {
    let saved = now();
    let value = serde_json::json!({ "saved": saved, "covers": covers }).to_string();
    if let Err(error) = cache.write(&covers_key(slug), &value, saved as i64) {
        log::warn!("genres: {error:#}");
    }
}

fn covers_key(slug: &str) -> String {
    format!("genre-covers/{slug}")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn streaming_clients(session: &Session) -> Vec<Arc<dyn music::MusicApi>> {
    let local = session.local_slug();
    session
        .active_slugs()
        .into_iter()
        .filter(|slug| *slug != local)
        .filter_map(|slug| session.client_of(Shelf::Account(slug)))
        .collect()
}

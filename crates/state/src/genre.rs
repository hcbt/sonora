use std::rc::Rc;
use std::sync::Arc;

use gpui::{Context, Entity, Task};
use music::{Genre, GenreDetail, GenreItem, GenreSection};
use tokio::task::AbortHandle;

use crate::{Io, Session, SessionEvent, Shelf, join};

pub struct Genres {
    genres: Rc<Vec<Genre>>,
    loading: bool,
    error: Option<String>,
    session: Entity<Session>,
    io: Io,
    task: Option<Task<()>>,
    generation: u64,
}

impl Genres {
    pub fn new(session: Entity<Session>, io: Io, cx: &mut Context<Self>) -> Self {
        cx.subscribe(&session, |this, _, event, cx| match event {
            SessionEvent::SignedIn(_) | SessionEvent::Reconnected(_) => this.reload(cx),
            SessionEvent::SignedOut(slug) => this.drop_account(slug, cx),
            SessionEvent::LocalChanged => {}
        })
        .detach();

        Self {
            genres: Rc::new(Vec::new()),
            loading: false,
            error: None,
            session,
            io,
            task: None,
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

    pub fn load(&mut self, cx: &mut Context<Self>) {
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
                let session = this.session.read(cx);
                merged.retain(|genre| {
                    session
                        .slug_for(&genre.id)
                        .is_some_and(|slug| session.account_authenticated(slug))
                });
                match failed {
                    Some(error) if merged.is_empty() => {
                        this.error = Some(crate::settled::<()>(Err(error), cx).unwrap_err());
                    }
                    _ => {
                        this.error = None;
                        this.genres = Rc::new(merged);
                    }
                }
                cx.notify();
            })
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
            SessionEvent::LocalChanged => {}
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

        self.loading = true;
        cx.notify();

        let id = id.to_owned();
        let request = self.io.spawn({
            let id = id.clone();
            async move { catalog.genre(&id).await }
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
        let cover = pictured(&detail);
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

fn pictured(detail: &GenreDetail) -> Option<String> {
    let items = || detail.sections.iter().flat_map(|section| &section.items);
    let album = items().find_map(|item| match item {
        GenreItem::Album(album) => album.cover.clone(),
        _ => None,
    });

    album.or_else(|| {
        items().find_map(|item| match item {
            GenreItem::Playlist(playlist) => playlist.cover.clone(),
            _ => None,
        })
    })
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

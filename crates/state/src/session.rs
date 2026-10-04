use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Error;
use gpui::{Context, Entity, EventEmitter, Task};
use i18n::t;
use music::{
    Capabilities, MusicApi, MusicProvider, PlaybackFactory, PromptSink, ProviderSession, Shape,
    SignIn, SignInFailure, SignInProblem, SignInPrompt, UserProfile,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::Shelf;
use crate::catalog::CatalogSource;
use crate::owned::{Owned, OwnedApi};
use crate::settings::AppSettings;
use crate::{Io, Network, join};

const HEARTBEAT: Duration = Duration::from_secs(30);
/// How often the sign-in window is asked whether the user is through.
const WINDOW_POLL: Duration = Duration::from_millis(300);
const BACKOFF: [Duration; 5] = [
    Duration::ZERO,
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub problem: Option<SignInProblem>,
    pub summary: String,
    pub detail: Option<String>,
}

impl Failure {
    fn new(error: &Error) -> Self {
        let reason = format!("{error:#}");
        let problem = error
            .downcast_ref::<SignInFailure>()
            .map(|failure| failure.0)
            .or_else(|| music::trouble::offline(&reason).then_some(SignInProblem::Network));
        let detail = error
            .chain()
            .skip(1)
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        Self {
            problem,
            summary: error.to_string(),
            detail: (!detail.is_empty()).then(|| detail.join(": ")),
        }
    }

    /// Whether the sign-in failed because there was no network, rather than because the account
    /// was refused. A provider that was only unreachable is still the user's.
    pub fn offline(&self) -> bool {
        self.problem == Some(SignInProblem::Network)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionState {
    SignedOut,
    Restoring,
    Authorizing(Option<SignInPrompt>),
    SignedIn(UserProfile),
    /// A stored account could not be reached, and nothing else is live. It is still the user's,
    /// so nothing is signed out: the app stays on whatever the libraries kept and on the local
    /// files, and tries the accounts again once the network is back.
    Offline(Failure),
    Failed(Failure),
}

/// A live account changed. The slug is which one, so another account's library is left alone.
pub enum SessionEvent {
    SignedIn(&'static str),
    SignedOut(&'static str),
    Reconnected(&'static str),
    LocalChanged,
}

#[derive(Clone, Copy)]
enum SecretInput {
    Browser,
    Manual,
}

pub struct ProviderInfo {
    pub slug: &'static str,
    pub name: &'static str,
    pub options: Vec<SignIn>,
    pub web_sign_in: bool,
    pub protected: bool,
    pub stored: bool,
    /// Whether what is stored is an anonymous session rather than an account.
    pub guest: bool,
    /// Whether this provider has a live session right now. Several can be live at once.
    pub active: bool,
    pub pending: bool,
    pub error: Option<Failure>,
}

/// One signed-in streaming account. Local music is not one of these: it already lives beside
/// every account, the way a second library does.
struct Live {
    index: usize,
    profile: UserProfile,
    client: Arc<dyn MusicApi>,
    catalog: Arc<CatalogSource>,
    playback: Arc<dyn PlaybackFactory>,
    shape: Shape,
    authenticated: bool,
    capabilities: Capabilities,
    offline: bool,
    reconnecting: bool,
    attempt: usize,
    reconnect: Option<Task<()>>,
}

pub struct Session {
    state: SessionState,
    providers: Vec<Arc<dyn MusicProvider>>,
    /// Streaming accounts that are signed in right now, in provider order.
    lives: Vec<Live>,
    /// Ids each live account has handed out, so a track routes to its own client.
    owned: Owned,
    awaiting: Option<usize>,
    /// The sign-in prompt, even while other accounts stay live and the app stays open.
    prompt: Option<SignInPrompt>,
    failures: Vec<(usize, Failure)>,
    /// Restores still in flight. A second account connecting does not wait on the first.
    inflight: usize,
    settings: Entity<AppSettings>,
    io: Io,
    task: Option<Task<()>>,
    tasks: Vec<Task<()>>,
    prompt_task: Option<Task<()>>,
    input: Option<UnboundedSender<String>>,
    /// The browser window a `SignInPrompt::Secret` opened, while it is up.
    window: Option<webview::Page>,
    window_task: Option<Task<()>>,
    heartbeat: Option<Task<()>>,
    local_provider: Arc<dyn MusicProvider>,
    local_folders: Vec<PathBuf>,
    local_client: Option<Arc<dyn MusicApi>>,
    local_catalog: Option<Arc<CatalogSource>>,
    local_playback: Option<Arc<dyn PlaybackFactory>>,
    local_capabilities: Capabilities,
    local_task: Option<Task<()>>,
    /// Whether a local scan is under way, so the UI can show its progress.
    scanning: bool,
}

impl EventEmitter<SessionEvent> for Session {}

impl Session {
    pub fn new(
        providers: Vec<Arc<dyn MusicProvider>>,
        local_provider: Arc<dyn MusicProvider>,
        settings: Entity<AppSettings>,
        io: Io,
        cx: &mut Context<Self>,
    ) -> Self {
        let local_folders = settings.read(cx).local_folders().to_vec();
        let local_playback = local_provider.playback_factory();
        let mut session = Self {
            state: SessionState::SignedOut,
            providers,
            lives: Vec::new(),
            owned: Owned::new(),
            awaiting: None,
            prompt: None,
            failures: Vec::new(),
            inflight: 0,
            settings,
            io,
            task: None,
            tasks: Vec::new(),
            prompt_task: None,
            input: None,
            window: None,
            window_task: None,
            heartbeat: None,
            local_provider,
            local_folders,
            local_client: None,
            local_catalog: None,
            local_playback,
            local_capabilities: Capabilities::NONE,
            local_task: None,
            scanning: false,
        };
        session.restore_local(cx);
        session
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// The sign-in prompt, if one is up. Other accounts stay live while it is, so the app does
    /// not leave for the login page to collect it.
    pub fn prompt(&self) -> Option<SignInPrompt> {
        self.prompt.clone()
    }

    pub fn owned(&self) -> Owned {
        self.owned.clone()
    }

    /// Records that a streaming id belongs to `slug` before any of that account's calls have
    /// returned it. A `spotify:` link is the case: the id is known before the account answers.
    pub fn own(&self, slug: &'static str, id: &str) {
        self.owned.claim(slug, id);
    }

    pub fn client_for(&self, id: &str) -> Option<Arc<dyn MusicApi>> {
        match music::is_local_id(id) {
            true => self.local_client.clone(),
            false => self
                .slug_for(id)
                .and_then(|slug| self.live(slug))
                .map(|live| live.client.clone()),
        }
    }

    pub fn playback_for(&self, id: &str) -> Option<Arc<dyn PlaybackFactory>> {
        match music::is_local_id(id) {
            true => self.local_playback.clone(),
            false => self
                .slug_for(id)
                .and_then(|slug| self.live(slug))
                .map(|live| live.playback.clone()),
        }
    }

    pub fn playback_of(&self, slug: &str) -> Option<Arc<dyn PlaybackFactory>> {
        self.live(slug).map(|live| live.playback.clone())
    }

    pub fn local_client(&self) -> Option<Arc<dyn MusicApi>> {
        self.local_client.clone()
    }

    /// The client serving a shelf, if that shelf has a provider right now.
    pub fn client_of(&self, shelf: Shelf) -> Option<Arc<dyn MusicApi>> {
        match shelf {
            Shelf::Account(slug) => self.live(slug).map(|live| live.client.clone()),
            Shelf::Local => self.local_client.clone(),
        }
    }

    /// What a shelf's library is made of. The local shelf is always a catalog.
    pub fn shape_of(&self, shelf: Shelf) -> Shape {
        match shelf {
            Shelf::Account(slug) => self
                .live(slug)
                .map(|live| live.shape)
                .unwrap_or(Shape::Saved),
            Shelf::Local => Shape::Catalog,
        }
    }

    pub(crate) fn catalog(&self, id: &str) -> Option<Arc<CatalogSource>> {
        match music::is_local_id(id) {
            true => self.local_catalog.clone(),
            false => self
                .slug_for(id)
                .and_then(|slug| self.live(slug))
                .map(|live| live.catalog.clone()),
        }
    }

    pub fn local_playback(&self) -> Option<Arc<dyn PlaybackFactory>> {
        self.local_playback.clone()
    }

    /// The local provider itself, so a file can be read as a track without waiting for
    /// a scan: an explorer open may point outside every configured folder.
    pub fn local_provider(&self) -> Arc<dyn MusicProvider> {
        self.local_provider.clone()
    }

    pub fn local_paths(&self) -> Vec<String> {
        self.local_folders
            .iter()
            .map(|path| path.display().to_string())
            .collect()
    }

    pub fn providers(&self) -> impl Iterator<Item = ProviderInfo> + '_ {
        self.providers
            .iter()
            .enumerate()
            .map(|(index, provider)| ProviderInfo {
                slug: provider.slug(),
                name: provider.name(),
                options: provider.sign_in_options(),
                // Asked in this order because answering `supported` starts a child process once
                // on Linux, and only a provider that signs in with cookies is worth it.
                web_sign_in: provider.web_sign_in().is_some() && webview::supported(),
                protected: provider.protected(),
                stored: provider.stored(),
                guest: provider.stored_guest(),
                active: self
                    .lives
                    .iter()
                    .any(|live| live.index == index && !live.offline),
                pending: self.awaiting == Some(index),
                error: self
                    .failures
                    .iter()
                    .find(|(failed, _)| *failed == index)
                    .map(|(_, failure)| failure.clone()),
            })
    }

    pub fn connected(&self) -> impl Iterator<Item = ProviderInfo> + '_ {
        self.providers().filter(|info| info.stored)
    }

    /// Accounts that have a library to show: signed in, not a guest session.
    pub fn libraries(&self) -> Vec<(&'static str, &'static str)> {
        self.lives
            .iter()
            .filter(|live| live.authenticated && !live.offline)
            .map(|live| {
                let provider = &self.providers[live.index];
                (provider.slug(), provider.name())
            })
            .collect()
    }

    /// Stored accounts that are not guest sessions, in provider order. A launch can open one
    /// of their libraries before restore has finished.
    pub fn stored_libraries(&self) -> Vec<&'static str> {
        self.providers
            .iter()
            .filter(|provider| provider.stored() && !provider.stored_guest())
            .map(|provider| provider.slug())
            .collect()
    }

    /// Whether any live account's tracks need the Widevine module.
    pub fn wants_drm(&self) -> bool {
        self.lives.iter().any(|live| {
            let provider = &self.providers[live.index];
            provider.protected() && provider.stored() && live.authenticated
        })
    }

    /// Signs `slug` out and drops its live session. Other accounts stay.
    pub fn forget(&mut self, slug: &str, cx: &mut Context<Self>) {
        let Some(index) = self.index_of(slug) else {
            return;
        };
        self.providers[index].sign_out();
        self.failures.retain(|(failed, _)| *failed != index);
        self.drop_live(index, cx);
    }

    /// Connects a stored account without disconnecting the others. Already live is a no-op.
    pub fn switch(&mut self, slug: &str, cx: &mut Context<Self>) {
        let Some(index) = self.index_of(slug) else {
            return;
        };
        if self
            .lives
            .iter()
            .any(|live| live.index == index && !live.offline)
        {
            return;
        }
        self.restore_one(index, cx);
    }

    pub fn name_of(&self, slug: &str) -> Option<&'static str> {
        self.providers
            .iter()
            .find(|provider| provider.slug() == slug)
            .map(|provider| provider.name())
    }

    /// The host to try when checking whether the network is back. Any live account's own, so
    /// the check never touches a service the app is not already using.
    pub fn reach(&self) -> Option<String> {
        let index = self
            .lives
            .iter()
            .find(|live| !live.offline)
            .map(|live| live.index)
            .or_else(|| self.providers.iter().position(|provider| provider.stored()))?;
        self.providers.get(index)?.reach()
    }
    pub fn account_authenticated(&self, slug: &str) -> bool {
        self.live(slug)
            .is_some_and(|live| live.authenticated && !live.offline)
    }

    /// The history scope for a live account, `{slug}:{profile id}`.
    pub fn account_scope(&self, slug: &str) -> Option<String> {
        self.live(slug)
            .map(|live| format!("{slug}:{}", live.profile.id))
    }

    pub fn local_slug(&self) -> &'static str {
        self.local_provider.slug()
    }

    pub fn active_slugs(&self) -> Vec<&'static str> {
        let mut slugs: Vec<&'static str> = self
            .lives
            .iter()
            .filter(|live| !live.offline)
            .map(|live| self.providers[live.index].slug())
            .collect();
        if self.local_client.is_some() {
            slugs.push(self.local_slug());
        }
        slugs
    }

    pub fn slug_for(&self, id: &str) -> Option<&'static str> {
        match music::is_local_id(id) {
            true => Some(self.local_slug()),
            false => self.owned.slug(id),
        }
    }

    pub fn shelf_for(&self, id: &str) -> Option<Shelf> {
        match music::is_local_id(id) {
            true => Some(Shelf::Local),
            false => self.owned.slug(id).map(Shelf::Account),
        }
    }

    /// The provider an id belongs to, whichever library it sits in.
    pub(crate) fn provider_for(&self, id: &str) -> Option<&dyn MusicProvider> {
        match music::is_local_id(id) {
            true => Some(self.local_provider.as_ref()),
            false => self.slug_for(id).and_then(|slug| {
                self.providers
                    .iter()
                    .find(|provider| provider.slug() == slug)
                    .map(|provider| provider.as_ref())
            }),
        }
    }

    /// Whether any streaming account is signed in for real, rather than as a guest.
    pub fn authenticated(&self) -> bool {
        self.lives
            .iter()
            .any(|live| live.authenticated && !live.offline)
    }

    /// Whether the only live streaming session is an anonymous guest, so home can fall back
    /// to the local collection the way it did when guest was the whole session.
    pub fn guest(&self) -> bool {
        self.lives.iter().any(|live| !live.offline) && !self.authenticated()
    }

    /// What the account an id belongs to can do. Local files keep favorites but seed no station.
    pub fn capabilities_for(&self, id: &str) -> Capabilities {
        match music::is_local_id(id) {
            true => self.local_capabilities,
            false => self
                .slug_for(id)
                .and_then(|slug| self.live(slug))
                .map(|live| live.capabilities)
                .unwrap_or(Capabilities::NONE),
        }
    }

    /// The same, for whichever shelf a thing belongs to.
    pub fn capabilities_of(&self, shelf: Shelf) -> Capabilities {
        match shelf {
            Shelf::Account(slug) => self
                .live(slug)
                .map(|live| live.capabilities)
                .unwrap_or(Capabilities::NONE),
            Shelf::Local => self.local_capabilities,
        }
    }

    pub fn is_pending(&self) -> bool {
        self.awaiting.is_some() || self.inflight > 0
    }

    /// Restores every stored account. None of them replaces another.
    pub fn restore(&mut self, cx: &mut Context<Self>) {
        let stored: Vec<usize> = self
            .providers
            .iter()
            .enumerate()
            .filter(|(_, provider)| provider.stored())
            .map(|(index, _)| index)
            .collect();
        if stored.is_empty() {
            self.publish();
            cx.notify();
            return;
        }
        for index in stored {
            self.restore_one(index, cx);
        }
    }

    pub fn sign_in(&mut self, slug: &str, method: SignIn, cx: &mut Context<Self>) {
        self.start_sign_in(slug, method, SecretInput::Browser, cx);
    }

    pub fn sign_in_with_cookies(&mut self, slug: &str, cx: &mut Context<Self>) {
        self.start_sign_in(slug, SignIn::Secret, SecretInput::Manual, cx);
    }

    fn start_sign_in(
        &mut self,
        slug: &str,
        method: SignIn,
        secret_input: SecretInput,
        cx: &mut Context<Self>,
    ) {
        if self.awaiting.is_some() {
            return;
        }
        let Some(index) = self.index_of(slug) else {
            return;
        };
        self.failures.retain(|(failed, _)| *failed != index);
        self.awaiting = Some(index);
        self.prompt = None;
        self.publish();
        cx.notify();

        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        self.input = Some(input_tx);
        let (prompt_tx, mut prompt_rx) = tokio::sync::mpsc::unbounded_channel::<SignInPrompt>();
        self.prompt_task = Some(cx.spawn(async move |this, cx| {
            while let Some(prompt) = prompt_rx.recv().await {
                this.update(cx, |this, cx| {
                    if this.awaiting.is_some() {
                        let secret = matches!(prompt, SignInPrompt::Secret);
                        this.prompt = Some(prompt);
                        this.publish();
                        cx.notify();
                        if secret && matches!(secret_input, SecretInput::Browser) {
                            this.open_window(cx);
                        }
                    }
                })
                .ok();
            }
        }));
        let prompt: PromptSink = Arc::new(move |prompt| {
            prompt_tx.send(prompt).ok();
        });

        let provider = self.providers[index].clone();
        let io = self.io.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let authorized =
                join(io.spawn(async move { provider.sign_in(method, prompt, input_rx).await }))
                    .await;

            this.update(cx, |this, cx| {
                this.prompt_task = None;
                this.input = None;
                this.window = None;
                match authorized {
                    Ok(session) => this.signed_in(session, index, cx),
                    Err(error) => this.failed_index(index, &error, true, cx),
                }
            })
            .ok();
        }));
    }

    pub fn cancel_sign_in(&mut self, cx: &mut Context<Self>) {
        if self.awaiting.is_none() {
            return;
        }
        if let Some(index) = self.awaiting {
            let provider = self.providers[index].clone();
            self.io.spawn(async move { provider.abandon() });
        }
        self.task = None;
        self.prompt_task = None;
        self.input = None;
        self.window = None;
        self.window_task = None;
        self.awaiting = None;
        self.prompt = None;
        self.publish();
        cx.notify();
    }

    /// Opens the browser window for the secret prompt now showing. The window answers the prompt
    /// itself once the user is through; closing it cancels the sign-in.
    fn open_window(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.prompt, Some(SignInPrompt::Secret)) || self.window.is_some() {
            return;
        }
        let Some(index) = self.awaiting else {
            return;
        };
        let provider = &self.providers[index];
        let Some(sign_in) = provider.web_sign_in() else {
            return;
        };
        let target = webview::Target {
            url: sign_in.url.to_string(),
            landing: sign_in.landing.to_string(),
            domain: sign_in.domain.to_string(),
            proof: sign_in.proof.iter().map(ToString::to_string).collect(),
            title: t!("login-window-title", provider = provider.name()).to_string(),
            agent: sign_in.agent.map(str::to_owned),
            script: None,
        };
        match webview::Page::open(target) {
            Ok(login) => self.window = Some(login),
            Err(error) => {
                log::warn!("session: cannot open the sign-in window: {error:#}");
                self.task = None;
                self.input = None;
                return self.failed_index(index, &error, true, cx);
            }
        }
        self.window_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(WINDOW_POLL).await;
                let open = this.update(cx, |this, cx| {
                    let Some(login) = this.window.as_mut() else {
                        return false;
                    };
                    match login.poll() {
                        webview::Poll::Pending => true,
                        webview::Poll::Closed => {
                            this.window = None;
                            this.cancel_sign_in(cx);
                            false
                        }
                        webview::Poll::Cookies(header) => {
                            this.window = None;
                            this.submit_input(header, cx);
                            false
                        }
                    }
                });
                if !open.unwrap_or(false) {
                    break;
                }
            }
        }));
    }

    pub fn submit_input(&mut self, text: String, cx: &mut Context<Self>) {
        if let Some(input) = &self.input {
            self.window = None;
            input.send(text).ok();
            if matches!(
                self.prompt,
                Some(SignInPrompt::Secret | SignInPrompt::Accounts(_))
            ) {
                self.prompt = None;
                self.publish();
                cx.notify();
            }
        }
    }

    /// Signs every stored streaming account out. Local music is left as it is.
    pub fn sign_out(&mut self, cx: &mut Context<Self>) {
        let slugs: Vec<&'static str> = self
            .providers
            .iter()
            .filter(|provider| provider.stored())
            .map(|provider| provider.slug())
            .collect();
        for slug in slugs {
            self.forget(slug, cx);
        }
    }

    /// Tries every stored account that is not live again, once the network is back.
    pub fn restore_if_offline(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.state, SessionState::Offline(_)) && self.authenticated() {
            let missing: Vec<usize> = self
                .providers
                .iter()
                .enumerate()
                .filter(|(index, provider)| {
                    provider.stored()
                        && !self
                            .lives
                            .iter()
                            .any(|live| live.index == *index && !live.offline)
                })
                .map(|(index, _)| index)
                .collect();
            for index in missing {
                self.restore_one(index, cx);
            }
            return;
        }
        if matches!(self.state, SessionState::Offline(_)) {
            self.restore(cx);
        }
    }

    fn index_of(&self, slug: &str) -> Option<usize> {
        self.providers
            .iter()
            .position(|provider| provider.slug() == slug)
    }

    fn live(&self, slug: &str) -> Option<&Live> {
        let index = self.index_of(slug)?;
        self.lives.iter().find(|live| live.index == index)
    }

    fn restore_one(&mut self, index: usize, cx: &mut Context<Self>) {
        if self
            .lives
            .iter()
            .any(|live| live.index == index && !live.offline)
        {
            return;
        }
        self.inflight += 1;
        self.publish();
        cx.notify();

        let provider = self.providers[index].clone();
        let io = self.io.clone();
        self.tasks.push(cx.spawn(async move |this, cx| {
            let restored = join(io.spawn(async move { provider.restore().await })).await;
            this.update(cx, |this, cx| {
                this.inflight = this.inflight.saturating_sub(1);
                match restored {
                    Ok(Some(session)) => this.signed_in(session, index, cx),
                    Ok(None) => {
                        this.publish();
                        cx.notify();
                    }
                    Err(error) => this.failed_index(index, &error, false, cx),
                }
            })
            .ok();
        }));
    }

    fn drop_live(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(pos) = self.lives.iter().position(|live| live.index == index) else {
            self.publish();
            cx.notify();
            return;
        };
        let slug = self.providers[index].slug();
        self.lives.remove(pos);
        self.publish();
        cx.notify();
        // Subscribers still see the ids this account owned, then they go.
        cx.emit(SessionEvent::SignedOut(slug));
        self.owned.forget_slug(slug);
        if self.lives.is_empty() {
            self.heartbeat = None;
        }
    }

    fn signed_in(&mut self, session: ProviderSession, index: usize, cx: &mut Context<Self>) {
        let slug = self.providers[index].slug();
        let replaced = self.lives.iter().any(|live| live.index == index);
        if replaced {
            self.lives.retain(|live| live.index != index);
            self.owned.forget_slug(slug);
            cx.emit(SessionEvent::SignedOut(slug));
        }
        let client = OwnedApi::wrap(slug, session.api, self.owned.clone());
        self.lives.push(Live {
            index,
            profile: session.profile,
            catalog: Arc::new(CatalogSource::new(client.clone())),
            client,
            playback: session.playback,
            shape: session.shape,
            authenticated: session.authenticated,
            capabilities: session.capabilities,
            offline: false,
            reconnecting: false,
            attempt: 0,
            reconnect: None,
        });
        self.lives.sort_by_key(|live| live.index);
        self.awaiting = None;
        self.prompt = None;
        self.failures.retain(|(failed, _)| *failed != index);
        self.publish();
        self.ensure_heartbeat(cx);
        cx.notify();
        cx.emit(SessionEvent::SignedIn(slug));
    }

    fn ensure_heartbeat(&mut self, cx: &mut Context<Self>) {
        if self.heartbeat.is_some() || self.lives.is_empty() {
            return;
        }
        self.heartbeat = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(HEARTBEAT).await;
                let keep = this.update(cx, |this, cx| this.reconnect_if_stale(cx));
                if keep.is_err() || !keep.unwrap_or(false) {
                    return;
                }
            }
        }));
    }

    pub fn reconnect_if_stale(&mut self, cx: &mut Context<Self>) -> bool {
        if self.lives.is_empty() {
            return false;
        }
        let stale: Vec<usize> = self
            .lives
            .iter()
            .enumerate()
            .filter(|(_, live)| !live.offline && !live.reconnecting && !live.client.alive())
            .map(|(pos, _)| pos)
            .collect();
        for pos in stale {
            self.reconnect_at(pos, cx);
        }
        true
    }

    fn reconnect_at(&mut self, pos: usize, cx: &mut Context<Self>) {
        let live = &mut self.lives[pos];
        if live.reconnecting {
            return;
        }
        let index = live.index;
        let wait = BACKOFF[live.attempt.min(BACKOFF.len() - 1)];
        live.attempt += 1;
        live.reconnecting = true;
        log::warn!(
            "session: the {} session went stale, reconnecting in {}s",
            self.providers[index].name(),
            wait.as_secs()
        );
        let provider = self.providers[index].clone();
        let io = self.io.clone();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let restored = join(io.spawn(async move { provider.restore().await })).await;
            this.update(cx, |this, cx| {
                if let Some(live) = this.lives.iter_mut().find(|live| live.index == index) {
                    live.reconnecting = false;
                }
                match restored {
                    Ok(Some(session)) => this.reconnected(session, index, cx),
                    Ok(None) => log::warn!("session: nothing stored to reconnect with"),
                    Err(error) => log::warn!("session: cannot reconnect: {error:#}"),
                }
            })
            .ok();
        });
        if let Some(live) = self.lives.iter_mut().find(|live| live.index == index) {
            live.reconnect = Some(task);
        }
    }

    fn reconnected(&mut self, session: ProviderSession, index: usize, cx: &mut Context<Self>) {
        let Some(live) = self.lives.iter_mut().find(|live| live.index == index) else {
            return;
        };
        let slug = self.providers[index].slug();
        let client = OwnedApi::wrap(slug, session.api, self.owned.clone());
        live.attempt = 0;
        live.catalog = Arc::new(CatalogSource::new(client.clone()));
        live.client = client;
        live.playback = session.playback;
        live.shape = session.shape;
        live.authenticated = session.authenticated;
        live.capabilities = session.capabilities;
        live.profile = session.profile;
        live.offline = false;
        log::debug!("session: reconnected {slug}");
        self.publish();
        cx.notify();
        cx.emit(SessionEvent::Reconnected(slug));
    }

    fn failed_index(
        &mut self,
        index: usize,
        error: &Error,
        signing_in: bool,
        cx: &mut Context<Self>,
    ) {
        let failure = Failure::new(error);
        Network::failed(&format!("{error:#}"), cx);
        self.failures.retain(|(failed, _)| *failed != index);
        self.failures.push((index, failure.clone()));
        if self.awaiting == Some(index) {
            self.awaiting = None;
            self.prompt = None;
            self.input = None;
            self.window = None;
        }
        let slug = self.providers[index].slug();
        if !signing_in && failure.offline() && self.providers[index].stored() {
            log::warn!("session: {slug} could not be reached, carrying on offline");
            self.publish();
            cx.notify();
            return;
        }
        let was_live = self.lives.iter().any(|live| live.index == index);
        if was_live {
            self.drop_live(index, cx);
            return;
        }
        self.publish();
        cx.notify();
        if !signing_in {
            cx.emit(SessionEvent::SignedOut(slug));
        }
    }

    fn any_connected(&self) -> bool {
        self.lives.iter().any(|live| !live.offline)
    }

    fn publish(&mut self) {
        self.state = if self.awaiting.is_some() && !self.any_connected() {
            SessionState::Authorizing(self.prompt.clone())
        } else if let Some(live) = self.lives.iter().find(|live| !live.offline) {
            SessionState::SignedIn(live.profile.clone())
        } else if self.inflight > 0 {
            SessionState::Restoring
        } else if let Some((_, failure)) = self
            .failures
            .iter()
            .find(|(index, failure)| failure.offline() && self.providers[*index].stored())
        {
            SessionState::Offline(failure.clone())
        } else if let Some((_, failure)) = self.failures.last() {
            SessionState::Failed(failure.clone())
        } else {
            SessionState::SignedOut
        };
    }

    fn restore_local(&mut self, cx: &mut Context<Self>) {
        if self.local_folders.is_empty() {
            return;
        }
        self.rescan_local(false, cx);
    }

    /// Adds a folder to the local library, then rescans every configured folder together so
    /// artists and albums that span more than one root merge into one, seamlessly.
    pub fn add_local_folder(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.add_local_folders(vec![path], cx);
    }

    /// Same as [`Session::add_local_folder`], for a batch picked in one native dialog.
    pub fn add_local_folders(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let mut folders = self.local_folders.clone();
        for path in paths {
            if folders.iter().any(|existing| overlaps(existing, &path)) {
                log::warn!(
                    "session: {} overlaps an already-added local folder",
                    path.display()
                );
                continue;
            }
            folders.push(path);
        }
        if folders.len() == self.local_folders.len() {
            return;
        }
        self.set_local_folders(folders, cx);
    }

    pub fn remove_local_folder(&mut self, path: &Path, cx: &mut Context<Self>) {
        let mut folders = self.local_folders.clone();
        let before = folders.len();
        folders.retain(|existing| existing != path);
        if folders.len() == before {
            return;
        }
        self.set_local_folders(folders, cx);
    }

    /// Rescans every configured local folder without changing the list, e.g. after files
    /// changed on disk or a tag was edited. A `thorough` rescan is the one the user asked for:
    /// it forgets what the last scan recorded, so every folder is listed and every file stat'd
    /// again, which is the only way an edit made behind Sonora's back is noticed.
    pub fn rescan_local(&mut self, thorough: bool, cx: &mut Context<Self>) {
        if thorough {
            self.local_provider.forget_scan();
        }
        self.set_local_folders(self.local_folders.clone(), cx);
    }

    /// Points the local library at `folders` and scans them. A scan already under way is
    /// cancelled first: its folders may be the ones just removed, and two scans would only
    /// fight over the same disk.
    fn set_local_folders(&mut self, folders: Vec<PathBuf>, cx: &mut Context<Self>) {
        music::progress::cancel();
        if folders.is_empty() {
            self.scanning = false;
            self.local_provider.sign_out();
            self.local_folders = Vec::new();
            self.settings.update(cx, |settings, cx| {
                settings.set_local_folders(Vec::new(), cx)
            });
            self.local_client = None;
            self.local_catalog = None;
            self.local_capabilities = Capabilities::NONE;
            self.local_task = None;
            cx.notify();
            cx.emit(SessionEvent::LocalChanged);
            return;
        }

        let provider = self.local_provider.clone();
        let chosen = folders.clone();
        let io = self.io.clone();
        self.scanning = true;
        cx.notify();
        self.local_task = Some(cx.spawn(async move |this, cx| {
            let prompt: PromptSink = Arc::new(|_| {});
            let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            let signed_in = join(
                io.spawn(async move { provider.sign_in(SignIn::Path(folders), prompt, rx).await }),
            )
            .await;

            this.update(cx, |this, cx| {
                this.scanning = false;
                match signed_in {
                    Ok(session) => {
                        this.local_folders = chosen.clone();
                        this.settings
                            .update(cx, |settings, cx| settings.set_local_folders(chosen, cx));
                        this.local_signed_in(session, cx);
                    }
                    Err(error) => {
                        log::warn!("session: cannot update local music folders: {error:#}");
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
    }

    /// Whether a local scan is under way right now.
    pub fn scanning(&self) -> bool {
        self.scanning
    }

    fn local_signed_in(&mut self, session: ProviderSession, cx: &mut Context<Self>) {
        self.local_catalog = Some(Arc::new(CatalogSource::new(session.api.clone())));
        self.local_client = Some(session.api);
        if self.local_playback.is_none() {
            self.local_playback = Some(session.playback);
        }
        self.local_capabilities = session.capabilities;
        cx.notify();
        cx.emit(SessionEvent::LocalChanged);
    }
}

/// Whether `a` and `b` are the same directory, or one contains the other — either way, scanning
/// both would double-count the tracks they share.
fn overlaps(a: &Path, b: &Path) -> bool {
    let a = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let b = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    a == b || a.starts_with(&b) || b.starts_with(&a)
}

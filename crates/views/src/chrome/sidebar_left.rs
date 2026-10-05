use std::rc::Rc;

use ui::{
    ActiveTheme as _, Button, Card, Deck, DraggedPin, Edge, MenuItem, Panel, Picker, Pin, PinKind,
    Pinnable as _, Popup, SNUG, Scroller, Shield, Side, Spot, Tabs, Text, Vacancy, drop_gap,
    drop_marker,
};

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, DragMoveEvent, ElementId, Entity, Hsla, MouseButton, MouseDownEvent,
    Pixels, Point, Render, ScrollHandle, SharedString, svg,
};
use gpui::{Window, div, px};
use router::{
    Destination, LibraryTab, NavEntry, Navigation, NavigationEvent, SettingsTab, navigate,
};
use state::{AppSettings, Library, Origin, PinSort, Pins, Playback, Session, Shelf, Sonora};

use crate::shared::menus::{ItemMenu, item_menu};

/// The one drag list the pinned section keeps, so a pin dropped anywhere in it lands in order.
const PINS: &str = "sidebar-pins";

const NAV: [(Option<NavEntry>, &str, Destination); 7] = [
    (Some(NavEntry::Home), "icons/house.svg", Destination::Home),
    (
        Some(NavEntry::Search),
        "icons/search.svg",
        Destination::Search,
    ),
    (
        Some(NavEntry::Library),
        "icons/library-big.svg",
        Destination::Home,
    ),
    (
        Some(NavEntry::Local),
        "icons/file-music.svg",
        Destination::Local(LibraryTab::Songs),
    ),
    (
        Some(NavEntry::Rekordbox),
        "icons/list-music.svg",
        Destination::Rekordbox(LibraryTab::Songs),
    ),
    (
        Some(NavEntry::History),
        "icons/rotate-ccw-clock.svg",
        Destination::History,
    ),
    (
        None,
        "icons/settings.svg",
        Destination::Settings(SettingsTab::General),
    ),
];

const LIBRARY_TABS: [(&str, LibraryTab); 4] = [
    ("nav-songs", LibraryTab::Songs),
    ("nav-albums", LibraryTab::Albums),
    ("nav-artists", LibraryTab::Artists),
    ("nav-playlists", LibraryTab::Playlists),
];

const MIN_WIDTH: Pixels = px(160.);
const MAX_WIDTH: Pixels = px(400.);
const HINT_HEIGHT: Pixels = px(42.);
const VACANCY_HEIGHT: Pixels = px(88.);
/// How far the pin mark on a library row falls back from the accent.
const PIN_MARK: f32 = 0.7;
/// The space between two pinned entries, the same as the `gap_1` between the rows above them.
const ROW_GAP: Pixels = px(4.);

/// Local Music expands into tabs. Each signed-in account does the same, beside it.
#[derive(Clone, Copy, PartialEq)]
enum Group {
    Local,
    Rekordbox,
}

impl Group {
    fn of(destination: &Destination) -> Option<Self> {
        match destination {
            Destination::Local(_) => Some(Self::Local),
            Destination::Rekordbox(_) => Some(Self::Rekordbox),
            _ => None,
        }
    }
}

pub(crate) struct SidebarLeft {
    settings: Entity<AppSettings>,
    session: Entity<Session>,
    trail: Entity<Navigation>,
    at: Destination,
    width: Pixels,
    open: bool,
    cramped: bool,
    forced: Option<bool>,
    accounts_open: Vec<String>,
    local_open: bool,
    rekordbox_open: bool,
    pinned_open: bool,
    /// Shelf keys whose playlist lists are open. A key is an account slug, `local`, or `rekordbox`.
    playlists_open: Vec<String>,
    /// The playlist a track is hovering, so the row can show it will take the drop.
    dropping_on: Option<String>,
    dropping: bool,
    drop_gap: Option<usize>,
    playback: Entity<Playback>,
    pins: Entity<Pins>,
    library: Entity<Library>,
    track_menu: ItemMenu,
    context_menu: Option<(Pin, Point<Pixels>)>,
    scrollbar: Entity<ui::Scrollbar>,
    popovers: ui::Popovers,
}

impl SidebarLeft {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let settings = Sonora::global(cx).settings.clone();
        let session = Sonora::global(cx).session.clone();
        let playback = Sonora::global(cx).playback.clone();
        let pins = Sonora::global(cx).pins.clone();
        let library = Sonora::global(cx).library.clone();
        cx.observe(&library, |_, _, cx| cx.notify()).detach();
        cx.observe(&pins, |_, _, cx| cx.notify()).detach();
        cx.observe(&playback, |_, _, cx| cx.notify()).detach();
        let me = cx.entity_id();
        let playlist_scrollbar = cx.new(|_| ui::Scrollbar::inset().watching(me));
        let scrollbar = cx.new(|_| ui::Scrollbar::new(ScrollHandle::new()).watching(me));
        let width = px(settings.read(cx).sidebar_width()).clamp(MIN_WIDTH, MAX_WIDTH);
        let open = settings.read(cx).sidebar_open();
        let pinned_open = settings.read(cx).sidebar_pinned_open();
        let trail = router::trail(cx);

        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        cx.observe(&settings, |_, _, cx| cx.notify()).detach();
        cx.observe(&trail, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&trail, |this, _, _: &NavigationEvent, cx| {
            this.dismiss(cx);
            cx.notify();
        })
        .detach();

        let at = trail.read(cx).current();
        let mut accounts_open = Vec::new();
        if let Destination::Library { account, .. } = &at {
            accounts_open.push(account.to_string());
        }
        let local_open = matches!(at, Destination::Local(_));
        let rekordbox_open = matches!(at, Destination::Rekordbox(_));

        let mut sidebar = Self {
            settings,
            session,
            trail,
            at: at.clone(),
            width,
            open,
            forced: None,
            cramped: false,
            accounts_open,
            local_open,
            rekordbox_open,
            pinned_open,
            playlists_open: Vec::new(),
            dropping_on: None,
            dropping: false,
            drop_gap: None,
            playback,
            pins,
            library,
            track_menu: ItemMenu::new(playlist_scrollbar, cx),
            context_menu: None,
            scrollbar,
            popovers: ui::Popovers::default(),
        };
        sidebar.reveal(&at, cx);
        sidebar
    }

    fn follow(&mut self, current: &Destination, cx: &App) {
        if self.at == *current {
            return;
        }
        self.at = current.clone();
        self.reveal(current, cx);
    }

    /// Opens the library, and its playlist list, that `current` belongs to.
    fn reveal(&mut self, current: &Destination, cx: &App) {
        if let Destination::Library { account, tab } = current {
            self.open_account(account);
            if *tab == LibraryTab::Playlists {
                self.open_playlists(account);
            }
        }
        self.local_open |= matches!(current, Destination::Local(_));
        self.rekordbox_open |= matches!(current, Destination::Rekordbox(_));
        if matches!(current, Destination::Local(LibraryTab::Playlists)) {
            self.open_playlists("local");
        }
        if matches!(current, Destination::Rekordbox(LibraryTab::Playlists)) {
            self.open_playlists("rekordbox");
        }
        if let Destination::Playlist(id) = current
            && let Some(shelf) = self.session.read(cx).shelf_for(id)
        {
            match shelf {
                Shelf::Account(slug) => self.open_account(slug),
                Shelf::Local => self.local_open = true,
                Shelf::Rekordbox => self.rekordbox_open = true,
            }
            self.open_playlists(shelf_key(shelf));
        }
    }

    fn open_account(&mut self, slug: &str) {
        if self.accounts_open.iter().any(|open| open == slug) {
            return;
        }
        self.accounts_open.push(slug.to_owned());
    }

    fn account_open(&self, slug: &str) -> bool {
        self.accounts_open.iter().any(|open| open == slug)
    }

    fn flip_account(&mut self, slug: &str) {
        match self.accounts_open.iter().position(|open| open == slug) {
            Some(index) => {
                self.accounts_open.remove(index);
            }
            None => self.accounts_open.push(slug.to_owned()),
        }
    }

    fn playlists_open(&self, key: &str) -> bool {
        self.playlists_open.iter().any(|open| open == key)
    }

    fn open_playlists(&mut self, key: &str) {
        if !self.playlists_open(key) {
            self.playlists_open.push(key.to_owned());
        }
    }

    fn flip_playlists(&mut self, key: &str) {
        match self.playlists_open.iter().position(|open| open == key) {
            Some(index) => {
                self.playlists_open.remove(index);
            }
            None => self.playlists_open.push(key.to_owned()),
        }
    }

    fn dismiss_menu(&mut self, cx: &mut Context<Self>) {
        self.track_menu.reset(cx);
        self.context_menu = None;
        cx.notify();
    }

    fn menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (pin, position) = self.context_menu.clone()?;
        let menu = item_menu(&pin, &self.track_menu, self.playback.clone(), cx);

        Some(
            Popup::new(position, menu)
                .on_close(cx.listener(|this, _, _, cx| this.dismiss_menu(cx))),
        )
    }

    pub fn is_open(&self) -> bool {
        self.forced.unwrap_or(self.open && !self.cramped)
    }

    pub fn overlays(&self) -> bool {
        self.cramped && self.is_open()
    }

    pub fn overlay_width(&self) -> Pixels {
        match self.overlays() {
            true => self.width,
            false => Pixels::ZERO,
        }
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        if !self.overlays() {
            return;
        }
        self.forced = Some(false);
        cx.notify();
    }

    pub fn occupied_width(&self) -> Pixels {
        match self.is_open() && !self.overlays() {
            true => self.width,
            false => Pixels::ZERO,
        }
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        match self.cramped {
            true => self.forced = Some(!self.is_open()),
            false => {
                self.open = !self.open;
                self.persist(cx);
            }
        }
        cx.notify();
    }

    fn ceiling(&self, window: &Window, cx: &Context<Self>) -> Pixels {
        let reserved = match self.overlays() {
            true => Pixels::ZERO,
            false => SNUG + super::Chrome::reserved_right(cx),
        };

        super::cap(MIN_WIDTH, MAX_WIDTH, reserved, window)
    }

    /// Flips into or out of the cramped state from the room the window leaves
    /// once `right` pixels are held back for the right sidebar. This runs inside
    /// a render, where a notify schedules nothing, so a flip asks for a full
    /// window refresh instead. That effect lands once the draw is over.
    pub fn adapt(&mut self, right: Pixels, window: &Window, cx: &mut App) {
        self.width = ui::snapped(self.width, window);

        let space_left = window.viewport_size().width - self.width - right;
        let cramped = space_left < SNUG;
        if cramped != self.cramped {
            self.cramped = cramped;
            self.forced = None;
            cx.refresh_windows();
        }
    }

    /// The navigation entries, each followed by its tabs while its group is open, and by its
    /// playlists while that list is open.
    fn navigation(&self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let show_libraries = self.settings.read(cx).nav_shown(NavEntry::Library.id());
        let accounts = self.library_rows(cx);
        let mut rows = Vec::new();
        for (index, (entry, _, _)) in NAV.iter().enumerate() {
            if entry.is_some_and(|entry| !self.settings.read(cx).nav_shown(entry.id())) {
                continue;
            }
            if entry == &Some(NavEntry::Library) {
                if show_libraries {
                    for (slug, name) in &accounts {
                        rows.push(self.account_nav(slug, name, cx));
                        if self.account_open(slug) {
                            let shelf = Shelf::Account(slug);
                            rows.push(self.library_tabs(shelf, cx));
                            self.push_playlists(&mut rows, shelf, window, cx);
                        }
                    }
                }
                continue;
            }
            rows.push(self.nav(index, cx));
            if let Some(group) = Group::of(&NAV[index].2).filter(|group| self.opened(*group)) {
                let shelf = match group {
                    Group::Local => Shelf::Local,
                    Group::Rekordbox => Shelf::Rekordbox,
                };
                rows.push(self.library_tabs(shelf, cx));
                self.push_playlists(&mut rows, shelf, window, cx);
            }
        }
        rows
    }

    fn push_playlists(
        &self,
        rows: &mut Vec<AnyElement>,
        shelf: Shelf,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if !self.playlists_open(shelf_key(shelf))
            || self.library.read(cx).state(shelf).playlists().is_empty()
        {
            return;
        }
        rows.push(self.playlist_branch(shelf, window, cx));
    }

    /// Signed-in libraries, plus a stored one whose snapshot is already on screen.
    fn library_rows(&self, cx: &App) -> Vec<(&'static str, &'static str)> {
        let session = self.session.read(cx);
        let library = self.library.read(cx);
        session
            .stored_libraries()
            .into_iter()
            .filter(|slug| {
                session.account_authenticated(slug) || library.stocked(Shelf::Account(slug))
            })
            .filter_map(|slug| session.name_of(slug).map(|name| (slug, name)))
            .collect()
    }
    /// The pinned section: its header, and its entries once it is expanded. The entries are a
    /// `Deck`, so only the ones on screen are ever built.
    fn pins(&self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if !self.settings.read(cx).nav_shown(NavEntry::Pins.id()) {
            return Vec::new();
        }

        let mut rows = vec![self.pins_header(cx)];
        if !self.pinned_open {
            return rows;
        }

        let pins = self.pins.read(cx);
        let pinned = pins.entries(cx);
        let rest = match self.settings.read(cx).sidebar_full_library() {
            true => pins.library(cx),
            false => Rc::new(Vec::new()),
        };
        if pinned.is_empty() && rest.is_empty() {
            rows.push(match self.dropping {
                true => hint(cx),
                false => vacancy(),
            });
            return rows;
        }

        let held = pinned.len();
        let count = held + rest.len();
        let row = ui::snapped(cx.theme().metrics.list_row, window);
        rows.push(
            Deck::new("sidebar-pins-deck")
                .rows((0..count).map(|_| row))
                .gap(ROW_GAP)
                .draw(cx.processor(move |this, index: usize, _, cx| {
                    let pin = match index < held {
                        true => pinned.get(index),
                        false => rest.get(index - held),
                    };
                    match pin {
                        Some(pin) => this.pin_row(index, pin.clone(), held, cx),
                        None => div().into_any_element(),
                    }
                }))
                .into_any_element(),
        );
        rows
    }

    fn opened(&self, group: Group) -> bool {
        match group {
            Group::Local => self.local_open,
            Group::Rekordbox => self.rekordbox_open,
        }
    }

    fn flip(&mut self, group: Group) {
        let open = match group {
            Group::Local => &mut self.local_open,
            Group::Rekordbox => &mut self.rekordbox_open,
        };
        *open = !*open;
    }

    fn pins_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.theme();
        let open = self.pinned_open;

        div()
            .id("sidebar-pins-header")
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .w_full()
            .min_w_0()
            .h(theme.metrics.control_small)
            .pl_2()
            // Narrower on the right than on the left, so the sort glyph falls on the same
            // line as the chevrons of the rows above.
            .pr_1()
            .mt(theme.metrics.pad)
            .mb(px(2.))
            .child(
                div()
                    .id("sidebar-pins-toggle")
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap_1()
                    .cursor_pointer()
                    .child(
                        svg()
                            .path(icons::path(chevron(open)))
                            .flex_none()
                            .size(theme.text(Text::Small))
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        ui::eyebrow(i18n::lookup("nav-pinned", None), cx)
                            .min_w_0()
                            .truncate(),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.pinned_open = !this.pinned_open;
                        let open = this.pinned_open;
                        this.settings.update(cx, |settings, cx| {
                            settings.set_sidebar_pinned_open(open, cx)
                        });
                        cx.notify();
                    })),
            )
            .when(open, |header| header.child(self.sort_picker(cx)))
            // Dragging over the header aims at the top of the list, which is otherwise
            // out of reach above the first entry.
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<DraggedPin>, _, cx| {
                    if !event.bounds.contains(&event.event.position) || this.drop_gap == Some(0) {
                        return;
                    }
                    this.drop_gap = Some(0);
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    fn sort_picker(&self, cx: &App) -> AnyElement {
        let chosen = self.pins.read(cx).sort(cx);
        let reversed = self.pins.read(cx).reversed(cx);
        let full = self.settings.read(cx).sidebar_full_library();
        let arrow = match reversed {
            true => "icons/chevron-down.svg",
            false => "icons/chevron-up.svg",
        };
        let pins = self.pins.clone();

        Picker::icon(
            "sidebar-pins-sort",
            &self.popovers,
            "icons/arrow-up-down.svg",
        )
        .tooltip("tool-sort")
        .sticky()
        .width(Picker::NARROW)
        .tint(match self.pins.read(cx).sorted(cx) {
            true => cx.theme().primary,
            false => cx.theme().muted_foreground,
        })
        .items(PinSort::ALL.into_iter().map(move |sort| {
            let pins = pins.clone();

            MenuItem::new(sort.id(), i18n::lookup(sort.key(), None))
                .selected(Some(sort) == chosen)
                .when(Some(sort) == chosen, |item| item.icon(arrow))
                .on_click(move |_, _, cx| {
                    pins.update(cx, |pins, cx| pins.choose(sort, cx));
                })
        }))
        .item(MenuItem::separator("sidebar-pins-scope"))
        .item(
            MenuItem::new(
                "sidebar-full-library",
                i18n::lookup("nav-show-full-library", None),
            )
            .checked(full)
            .on_click({
                let settings = self.settings.clone();
                move |_, _, cx| {
                    settings.update(cx, |settings, cx| {
                        settings.set_sidebar_full_library(!full, cx)
                    });
                }
            }),
        )
        .into_any_element()
    }

    /// One entry. The first `count` of them are the pins, which carry the drop slots. The rest
    /// is the library underneath, which can be dragged up into the pins but holds no slot.
    fn pin_row(&self, index: usize, pin: Pin, count: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.theme();
        let accent = theme.sidebar_accent;
        let destination = Destination::from(&pin);
        let active = destination == self.trail.read(cx).current();
        let opened = pin.clone();
        let held = index < count;
        // Only worth marking when the library sits alongside, since otherwise every row is a pin.
        let marked = held && self.settings.read(cx).sidebar_full_library();
        let edge = match self.drop_gap {
            Some(gap) if held && gap == index => Some(Edge::Above),
            Some(gap) if gap == count && index + 1 == count => Some(Edge::Below),
            _ => None,
        };

        let origin = Origin::from(&pin);
        let playing = self.playback.read(cx).playing_from(&origin, cx) == Some(true);

        let card = Card::new(("pinned", index), pin.label())
            .cover(pin.cover.clone())
            .fallback(pin.kind.icon())
            .when(pin.kind.round(), Card::circle)
            .play(
                playing,
                cx.listener(move |this, _, _, cx| {
                    this.playback
                        .update(cx, |playback, cx| playback.toggle_origin(&origin, cx));
                }),
            )
            .tint(match active {
                true => theme.foreground,
                false => theme.muted_foreground,
            })
            .meta(caption(pin.kind.key(), marked, cx))
            .when(active, |card| card.bg(accent))
            .hover(move |style| style.bg(accent))
            .press(move |_, _, cx| navigate(destination.clone(), cx))
            .menu(cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                this.track_menu.reset(cx);
                this.context_menu = Some((opened.clone(), event.position));
                cx.notify();
            }))
            .when_else(
                held,
                |card| card.pin_from(pin.clone(), Spot::new(PINS, index)),
                |card| card.pin(pin.clone()),
            )
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<DraggedPin>, _, cx| {
                    let Some(gap) = drop_gap(event.bounds, event.event.position, index) else {
                        return;
                    };
                    let gap = Some(match held {
                        true => gap,
                        false => count,
                    });
                    if this.drop_gap != gap {
                        this.drop_gap = gap;
                        cx.notify();
                    }
                }),
            );

        div()
            .id(("pinned-slot", index))
            .relative()
            .flex_none()
            .w_full()
            .min_w_0()
            .child(card)
            .when_some(edge, |this, edge| this.child(drop_marker(edge, cx)))
            .into_any_element()
    }

    /// One navigation entry. An expandable one flips its group open and shut, any other one
    /// navigates and is lit while its section is current.
    fn nav(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.theme();
        let accent = theme.sidebar_accent;
        let (entry, icon, destination) = NAV[index].clone();
        let key = entry.map_or("nav-settings", NavEntry::key);
        let current = self.trail.read(cx).current();
        let group = Group::of(&destination);
        let active = match group {
            Some(group) => Group::of(&current) == Some(group),
            None => destination.same_section(&current),
        };
        let tint = match active {
            true => theme.foreground,
            false => theme.muted_foreground,
        };
        let row = nav_row(index, key, tint, accent).icon(icon);

        match group {
            Some(group) => row
                .trailing(chevron(self.opened(group)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.flip(group);
                    cx.notify();
                })),
            None => row
                .when(active, |button| button.bg(accent))
                .on_click(move |_, _, cx| navigate(destination.clone(), cx)),
        }
        .into_any_element()
    }

    fn account_nav(
        &self,
        slug: &'static str,
        name: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = *cx.theme();
        let accent = theme.sidebar_accent;
        let current = self.trail.read(cx).current();
        let active = matches!(
            &current,
            Destination::Library { account, .. } if account.as_ref() == slug
        );
        let tint = match active {
            true => theme.foreground,
            false => theme.muted_foreground,
        };
        let open = self.account_open(slug);
        named_row(
            SharedString::from(format!("library-{slug}")),
            name.into(),
            tint,
            accent,
        )
        .icon(crate::shared::provider_logo(slug))
        .trailing(chevron(open))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.flip_account(slug);
            cx.notify();
        }))
        .into_any_element()
    }

    /// Songs, albums, artists, and a Playlists row that opens the lists themselves.
    fn library_tabs(&self, shelf: Shelf, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.theme();
        let accent = theme.sidebar_accent;
        let current = self.trail.read(cx).current();
        let key = shelf_key(shelf);
        let mut items = Vec::new();
        for (slot, (name, tab)) in LIBRARY_TABS.into_iter().enumerate() {
            let destination = shelf_destination(shelf, tab);
            if tab == LibraryTab::Playlists {
                items.push(self.playlists_header(shelf, destination, cx));
                continue;
            }
            let chosen = destination == current;
            let tint = match chosen {
                true => theme.foreground,
                false => theme.muted_foreground,
            };
            items.push(
                named_row(
                    SharedString::from(format!("library-tab-{key}-{slot}")),
                    i18n::lookup(name, None),
                    tint,
                    accent,
                )
                .flex_1()
                .when(chosen, |button| button.bg(accent))
                .on_click(move |_, _, cx| navigate(destination.clone(), cx))
                .into_any_element(),
            );
        }
        Tabs::new().items(items).into_any_element()
    }

    /// The Playlists row. It opens and closes the lists under it, the way a library opens its tabs.
    fn playlists_header(
        &self,
        shelf: Shelf,
        page: Destination,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = shelf_key(shelf);
        let open = self.playlists_open(key);
        let theme = *cx.theme();
        let accent = theme.sidebar_accent;
        let current = self.trail.read(cx).current();
        let chosen = current == page
            || matches!(&current, Destination::Playlist(id) if self
                .library
                .read(cx)
                .state(shelf)
                .playlists()
                .iter()
                .any(|playlist| playlist.id == id.as_ref()));
        let tint = match chosen {
            true => theme.foreground,
            false => theme.muted_foreground,
        };
        let button = nav_row(
            SharedString::from(format!("library-playlists-{key}")),
            "nav-playlists",
            tint,
            accent,
        )
        .flex_1()
        .trailing(chevron(open))
        .when(chosen, |button| button.bg(accent))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.flip_playlists(key);
            cx.notify();
        }));
        div()
            .id(SharedString::from(format!("library-playlists-drop-{key}")))
            .flex()
            .flex_1()
            .min_w_0()
            .child(button)
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<DraggedPin>, _, cx| {
                    if event.drag(cx).pin.kind != PinKind::Song
                        || !event.bounds.contains(&event.event.position)
                    {
                        return;
                    }
                    if !this.playlists_open(key) {
                        this.open_playlists(key);
                        cx.notify();
                    }
                }),
            )
            .into_any_element()
    }

    fn playlist_branch(&self, shelf: Shelf, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let listed = Rc::new(self.library.read(cx).state(shelf).playlists().to_vec());
        let row = ui::snapped(cx.theme().metrics.control, window);
        let count = listed.len();
        div()
            .ml(px(32.))
            .w_full()
            .min_w_0()
            .child(
                Deck::new(SharedString::from(format!(
                    "sidebar-playlists-{}",
                    shelf_key(shelf)
                )))
                .rows((0..count).map(|_| row))
                .gap(ROW_GAP)
                .draw(
                    cx.processor(move |this, index, _, cx| match listed.get(index) {
                        Some(playlist) => this.playlist_row(shelf, playlist, cx),
                        None => div().into_any_element(),
                    }),
                ),
            )
            .into_any_element()
    }

    fn playlist_row(
        &self,
        shelf: Shelf,
        playlist: &music::Playlist,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = *cx.theme();
        let accent = theme.sidebar_accent;
        let id = playlist.id.clone();
        let destination = Destination::Playlist(id.clone().into());
        let active = self.trail.read(cx).current() == destination;
        let hot = self.dropping_on.as_deref() == Some(playlist.id.as_str());
        let editable = playlist.owned || playlist.collaborative;
        let tint = match active {
            true => theme.foreground,
            false => theme.muted_foreground,
        };
        let name = playlist.name.clone();
        let height = theme.metrics.control;
        let hover_id = id.clone();
        let drop_id = id;
        div()
            .id(SharedString::from(format!("sidebar-playlist-{drop_id}")))
            .flex()
            .items_center()
            .gap_2()
            .h(height)
            .w_full()
            .min_w_0()
            .px_2()
            .rounded(theme.radius)
            .text_size(theme.text(Text::Small))
            .text_color(tint)
            .cursor_pointer()
            .when(active || hot, |row| row.bg(accent))
            .hover(move |style| style.bg(accent))
            .on_click({
                let opened = destination;
                move |_, _, cx| navigate(opened.clone(), cx)
            })
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<DraggedPin>, _, cx| {
                    this.hover_playlist(shelf, &hover_id, editable, event, cx);
                }),
            )
            .on_drop(cx.listener(move |this, dragged: &DraggedPin, _, cx| {
                this.drop_on_playlist(shelf, &drop_id, editable, dragged, cx);
            }))
            .child(
                svg()
                    .path(icons::path("icons/list.svg"))
                    .flex_none()
                    .size(theme.text(Text::Small))
                    .text_color(tint),
            )
            .child(div().min_w_0().truncate().child(name))
            .into_any_element()
    }

    fn hover_playlist(
        &mut self,
        shelf: Shelf,
        id: &str,
        editable: bool,
        event: &DragMoveEvent<DraggedPin>,
        cx: &mut Context<Self>,
    ) {
        let dragged = event.drag(cx);
        let same = self.session.read(cx).shelf_for(&dragged.pin.id) == Some(shelf);
        let hover = editable
            && same
            && dragged.pin.kind == PinKind::Song
            && event.bounds.contains(&event.event.position);
        match hover {
            true if self.dropping_on.as_deref() != Some(id) => {
                self.dropping_on = Some(id.to_owned());
                cx.notify();
            }
            false if self.dropping_on.as_deref() == Some(id) => {
                self.dropping_on = None;
                cx.notify();
            }
            _ => {}
        }
    }

    fn drop_on_playlist(
        &mut self,
        shelf: Shelf,
        id: &str,
        editable: bool,
        dragged: &DraggedPin,
        cx: &mut Context<Self>,
    ) {
        if !editable || dragged.pin.kind != PinKind::Song {
            return;
        }
        if self.session.read(cx).shelf_for(&dragged.pin.id) != Some(shelf) {
            return;
        }
        let track = dragged.pin.id.clone();
        let playlist = id.to_owned();
        self.library.update(cx, |library, cx| {
            library.add_to_playlist(playlist, track, cx);
        });
    }

    fn persist(&self, cx: &mut Context<Self>) {
        let width = self.width / px(1.);
        let open = self.open;
        self.settings
            .update(cx, |settings, cx| settings.set_sidebar(width, open, cx));
    }
}

impl Render for SidebarLeft {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        let sidebar_bg = theme.sidebar;
        let sidebar_border = theme.sidebar_border;

        let current = self.trail.read(cx).current();
        self.follow(&current, cx);
        self.adapt(super::Chrome::reserved_right(cx), window, cx);

        if !cx.has_active_drag() {
            self.dropping = false;
            self.drop_gap = None;
            self.dropping_on = None;
        }

        let mut rows = self.navigation(window, cx);
        rows.extend(self.pins(window, cx));

        let overlaid = self.overlays();
        let panel = Panel::new("sidebar-left", Side::Left, self.width)
            .limits(MIN_WIDTH, MAX_WIDTH)
            .reach(self.ceiling(window, cx))
            .clears_scrollbar()
            .on_resize(cx.listener(|this, width: &Pixels, _, cx| {
                this.width = *width;
                this.persist(cx);
                cx.notify();
            }))
            .on_drag_move(cx.listener(|this, _: &DragMoveEvent<DraggedPin>, _, cx| {
                if this.dropping {
                    return;
                }
                this.dropping = true;
                // A drag has nowhere to land while the section is closed, so open it.
                if !this.pinned_open {
                    this.pinned_open = true;
                    this.settings.update(cx, |settings, cx| {
                        settings.set_sidebar_pinned_open(true, cx)
                    });
                }
                cx.notify();
            }))
            .on_drop(cx.listener(|this, dragged: &DraggedPin, _, cx| {
                if this.dropping_on.is_some() {
                    return;
                }
                let gap = this.drop_gap.take();
                this.dropping = false;
                let pin = dragged.pin.clone();
                this.pins.update(cx, |pins, cx| pins.place(pin, gap, cx));
                cx.notify();
            }))
            .when(!self.is_open(), |this| this.hidden())
            .when(!theme.transparent, |this| this.bg(sidebar_bg))
            .border_color(sidebar_border)
            .when(overlaid, |this| {
                this.occlude().absolute().left_0().top_0().bottom_0()
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        Scroller::new("sidebar-left-rows", &self.scrollbar)
                            .size_full()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .w_full()
                                    .p_3()
                                    // Leave the last row somewhere to go, clear of the
                                    // button that floats over the bottom.
                                    .pb(ui::perch_room(cx))
                                    .children(rows),
                            ),
                    )
                    .children(ui::return_top("sidebar-return-top", &self.scrollbar, cx)),
            )
            .children(self.menu(cx));

        match overlaid {
            false => panel.into_any_element(),
            true => div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .child(
                    Shield::new("sidebar-shield")
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseDownEvent, _, cx| this.dismiss(cx)),
                        ),
                )
                .child(panel)
                .into_any_element(),
        }
    }
}

/// The kind of an entry, behind a dimmed pin when it is one of the pinned ones.
fn caption(kind: &'static str, marked: bool, cx: &App) -> impl IntoElement {
    let theme = *cx.theme();

    div()
        .flex()
        .items_center()
        .gap(theme.metrics.pad / 4.)
        .min_w_0()
        .when(marked, |row| {
            row.child(
                svg()
                    .path(icons::path("icons/pin.svg"))
                    .flex_none()
                    .size(theme.text(Text::Tiny))
                    .text_color(theme.primary.opacity(PIN_MARK)),
            )
        })
        .child(div().min_w_0().truncate().child(i18n::lookup(kind, None)))
}

fn hint(cx: &App) -> AnyElement {
    let theme = *cx.theme();

    div()
        .flex()
        .flex_none()
        .min_w_0()
        .items_center()
        .justify_center()
        // A narrow sidebar wraps the line, so the box grows rather than the text spilling out.
        .min_h(HINT_HEIGHT)
        .mx_2()
        .px_2()
        .py_1()
        .rounded(theme.radius)
        .border_1()
        .border_dashed()
        .border_color(theme.sidebar_border)
        .text_size(theme.text(Text::Small))
        .text_color(theme.muted_foreground)
        .text_center()
        .child(div().min_w_0().child(i18n::lookup("nav-pin-hint", None)))
        .into_any_element()
}

fn vacancy() -> AnyElement {
    Vacancy::new(i18n::lookup("nav-nothing-pinned", None))
        .icon("icons/pin.svg")
        .compact()
        .flex_none()
        .h(VACANCY_HEIGHT)
        .px_2()
        .into_any_element()
}

#[cfg(test)]
fn expanded(current: &Destination) -> (Option<&str>, bool) {
    match current {
        Destination::Library { account, .. } => (Some(account.as_ref()), false),
        Destination::Local(_) => (None, true),
        _ => (None, false),
    }
}

fn shelf_key(shelf: Shelf) -> &'static str {
    match shelf {
        Shelf::Account(slug) => slug,
        Shelf::Local => "local",
        Shelf::Rekordbox => "rekordbox",
    }
}

fn shelf_destination(shelf: Shelf, tab: LibraryTab) -> Destination {
    match shelf {
        Shelf::Account(slug) => Destination::Library {
            account: slug.into(),
            tab,
        },
        Shelf::Local => Destination::Local(tab),
        Shelf::Rekordbox => Destination::Rekordbox(tab),
    }
}

fn chevron(open: bool) -> &'static str {
    match open {
        true => "icons/chevron-down.svg",
        false => "icons/chevron-right.svg",
    }
}

fn nav_row(id: impl Into<ElementId>, key: &'static str, tint: Hsla, accent: Hsla) -> Button {
    named_row(id, i18n::lookup(key, None), tint, accent)
}

fn named_row(id: impl Into<ElementId>, label: SharedString, tint: Hsla, accent: Hsla) -> Button {
    Button::new(id)
        .ghost()
        .label(label)
        .tint(tint)
        .gap_2p5()
        .justify_start()
        .hover(move |style| style.bg(accent))
        .active(move |style| style.bg(accent))
}

#[cfg(test)]
mod tests {
    use router::{Destination, LibraryTab, SettingsTab};

    use super::expanded;

    #[test]
    fn a_section_expands_only_where_it_leads() {
        assert_eq!(
            expanded(&Destination::Library {
                account: "spotify".into(),
                tab: LibraryTab::Albums,
            }),
            (Some("spotify"), false)
        );
        assert_eq!(
            expanded(&Destination::Local(LibraryTab::Albums)),
            (None, true)
        );
        assert_eq!(
            expanded(&Destination::Settings(SettingsTab::General)),
            (None, false)
        );
    }

    #[test]
    fn content_belongs_to_neither_section() {
        let away = [
            Destination::Home,
            Destination::Search,
            Destination::Album("id".into()),
            Destination::Playlist("id".into()),
            Destination::Artist("id".into()),
            Destination::Song("id".into()),
        ];

        for destination in away {
            assert_eq!(expanded(&destination), (None, false), "{destination:?}");
        }
    }
}

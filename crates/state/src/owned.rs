use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use async_trait::async_trait;
use music::{
    Album, AlbumCatalogue, AlbumDetail, Artist, ArtistCatalogue, ArtistProfile, Feed, Genre,
    GenreDetail, GenreItem, GenreSection, HomeFeed, MediaKind, MusicApi, Pages, PinOutcome,
    PinTarget, PinTargetKind, Playlist, PlaylistDetail, Report, SavedArtist, Track, TrackRhythm,
    TrackTags, UserDetail, UserProfile,
};

/// Which streaming account handed out an id. Local ids are never entered: their prefix already
/// says so. Shared so a library lookup and a playback call agree without a focused account.
#[derive(Clone)]
pub struct Owned(Arc<Mutex<HashMap<String, &'static str>>>);

impl Owned {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }

    /// Records that `slug` owns `id`. An empty or local id is left alone, and a second claim
    /// from the same account is a no-op. A different account replacing the owner is the last
    /// word, which only matters if two providers ever minted the same id.
    pub fn claim(&self, slug: &'static str, id: &str) {
        if id.is_empty() || music::is_local_id(id) {
            return;
        }
        let Ok(mut owned) = self.0.lock() else {
            return;
        };
        if owned.get(id).copied() == Some(slug) {
            return;
        }
        owned.insert(id.to_owned(), slug);
    }

    pub fn slug(&self, id: &str) -> Option<&'static str> {
        self.0.lock().ok()?.get(id).copied()
    }

    /// Drops every id `slug` owned, so a signed-out account cannot keep routing.
    pub fn forget_slug(&self, slug: &str) {
        let Ok(mut owned) = self.0.lock() else {
            return;
        };
        owned.retain(|_, owner| *owner != slug);
    }
}

impl Default for Owned {
    fn default() -> Self {
        Self::new()
    }
}

/// A `MusicApi` that records every id it hands back against `slug`, then forwards the call.
/// Providers keep speaking raw ids. The rest of the app asks [`Owned`] which account an id
/// belongs to, so two libraries can be open without prefixing stored snapshots.
pub struct OwnedApi {
    slug: &'static str,
    inner: Arc<dyn MusicApi>,
    owned: Owned,
}

impl OwnedApi {
    pub fn wrap(slug: &'static str, inner: Arc<dyn MusicApi>, owned: Owned) -> Arc<dyn MusicApi> {
        Arc::new(Self { slug, inner, owned })
    }

    fn claim(&self, id: &str) {
        self.owned.claim(self.slug, id);
    }

    fn claim_track(&self, track: &Track) {
        if let Some(id) = track.id.as_deref() {
            self.claim(id);
        }
        if let Some(id) = track.album_id.as_deref() {
            self.claim(id);
        }
        for artist in &track.artist_refs {
            if let Some(id) = artist.id.as_deref() {
                self.claim(id);
            }
        }
    }

    fn claim_tracks(&self, tracks: &[Track]) {
        for track in tracks {
            self.claim_track(track);
        }
    }

    fn claim_album(&self, album: &Album) {
        self.claim(&album.id);
        for artist in &album.artist_refs {
            if let Some(id) = artist.id.as_deref() {
                self.claim(id);
            }
        }
    }

    fn claim_albums(&self, albums: &[Album]) {
        for album in albums {
            self.claim_album(album);
        }
    }

    fn claim_playlist(&self, playlist: &Playlist) {
        self.claim(&playlist.id);
        if !playlist.owner_id.is_empty() {
            self.claim(&playlist.owner_id);
        }
    }

    fn claim_playlists(&self, playlists: &[Playlist]) {
        for playlist in playlists {
            self.claim_playlist(playlist);
        }
    }

    fn claim_artist(&self, artist: &SavedArtist) {
        self.claim(&artist.id);
    }

    fn claim_artists(&self, artists: &[SavedArtist]) {
        for artist in artists {
            self.claim_artist(artist);
        }
    }

    fn claim_item(&self, item: &GenreItem) {
        match item {
            GenreItem::Playlist(playlist) => self.claim_playlist(playlist),
            GenreItem::Album(album) => self.claim_album(album),
            GenreItem::Genre(genre) => self.claim(&genre.id),
            GenreItem::Track(track) => self.claim_track(track),
            GenreItem::Artist(artist) => self.claim_artist(artist),
        }
    }

    fn claim_sections(&self, sections: &[GenreSection]) {
        for section in sections {
            for item in &section.items {
                self.claim_item(item);
            }
        }
    }

    fn claim_feed(&self, feed: &HomeFeed) {
        for item in &feed.listen_again {
            self.claim_item(item);
        }
        if let Some(picks) = &feed.quick_picks {
            self.claim_tracks(picks);
        }
        self.claim_sections(&feed.sections);
    }

    fn watch_pages<T: Send + 'static>(
        &self,
        mut pages: Pages<T>,
        claim: impl Fn(&Self, &T) + Send + 'static,
    ) -> Pages<T> {
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let watcher = Self {
            slug: self.slug,
            inner: self.inner.clone(),
            owned: self.owned.clone(),
        };
        tokio::spawn(async move {
            while let Some(page) = pages.recv().await {
                if let Ok(page) = &page {
                    for item in &page.items {
                        claim(&watcher, item);
                    }
                }
                if sender.send(page).await.is_err() {
                    break;
                }
            }
        });
        receiver
    }

    fn watch_feed(&self, mut feed: Feed) -> Feed {
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let watcher = Self {
            slug: self.slug,
            inner: self.inner.clone(),
            owned: self.owned.clone(),
        };
        tokio::spawn(async move {
            while let Some(lot) = feed.recv().await {
                if let Ok(lot) = &lot {
                    watcher.claim_feed(lot);
                }
                if sender.send(lot).await.is_err() {
                    break;
                }
            }
        });
        receiver
    }
}

#[async_trait]
impl MusicApi for OwnedApi {
    fn alive(&self) -> bool {
        self.inner.alive()
    }

    fn share_url(&self, kind: MediaKind, id: &str) -> Option<String> {
        self.inner.share_url(kind, id)
    }

    async fn profile(&self) -> Result<UserProfile> {
        self.inner.profile().await
    }

    async fn user(&self, user_id: &str) -> Result<UserDetail> {
        let user = self.inner.user(user_id).await?;
        self.claim(&user.id);
        self.claim_playlists(&user.playlists);
        Ok(user)
    }

    async fn artist(&self, artist_id: &str) -> Result<Artist> {
        let artist = self.inner.artist(artist_id).await?;
        self.claim_tracks(&artist.top_tracks);
        self.claim_albums(&artist.albums);
        Ok(artist)
    }

    async fn artist_catalogue(&self, artist_id: &str, known: &[Track]) -> Result<ArtistCatalogue> {
        let catalogue = self.inner.artist_catalogue(artist_id, known).await?;
        self.claim_albums(&catalogue.albums);
        self.claim_tracks(&catalogue.top_tracks);
        self.claim_albums(&catalogue.appears_on);
        Ok(catalogue)
    }

    async fn artist_profile(&self, artist_id: &str) -> Result<ArtistProfile> {
        self.inner.artist_profile(artist_id).await
    }

    async fn artist_images(&self, ids: Vec<String>) -> Result<HashMap<String, String>> {
        for id in &ids {
            self.claim(id);
        }
        self.inner.artist_images(ids).await
    }

    async fn saved_tracks(&self) -> Result<Vec<Track>> {
        let tracks = self.inner.saved_tracks().await?;
        self.claim_tracks(&tracks);
        Ok(tracks)
    }

    async fn saved_tracks_paged(&self) -> Result<Pages<Track>> {
        let pages = self.inner.saved_tracks_paged().await?;
        Ok(self.watch_pages(pages, |watcher, track| watcher.claim_track(track)))
    }

    async fn all_tracks(&self) -> Result<Vec<Track>> {
        let tracks = self.inner.all_tracks().await?;
        self.claim_tracks(&tracks);
        Ok(tracks)
    }

    async fn all_tracks_paged(&self) -> Result<Pages<Track>> {
        let pages = self.inner.all_tracks_paged().await?;
        Ok(self.watch_pages(pages, |watcher, track| watcher.claim_track(track)))
    }

    async fn set_track_saved(&self, track_id: &str, saved: bool) -> Result<()> {
        self.inner.set_track_saved(track_id, saved).await
    }

    async fn track_tags(&self, track_id: &str) -> Result<TrackTags> {
        self.inner.track_tags(track_id).await
    }

    async fn set_track_tags(&self, track_id: &str, tags: TrackTags) -> Result<()> {
        self.inner.set_track_tags(track_id, tags).await
    }

    async fn track(&self, track_id: &str) -> Result<Track> {
        let track = self.inner.track(track_id).await?;
        self.claim_track(&track);
        Ok(track)
    }

    async fn delete_track_file(&self, track_id: &str) -> Result<()> {
        self.inner.delete_track_file(track_id).await
    }

    async fn track_playcount(&self, track_id: &str) -> Result<Option<u64>> {
        self.inner.track_playcount(track_id).await
    }

    async fn track_rhythm(&self, track_id: &str) -> Result<TrackRhythm> {
        self.inner.track_rhythm(track_id).await
    }

    async fn report(&self, track_id: &str, report: Report, position: Duration) -> Result<()> {
        self.inner.report(track_id, report, position).await
    }

    async fn played(&self, track_id: &str, at: SystemTime) -> Result<()> {
        self.inner.played(track_id, at).await
    }

    async fn report_play(&self, track_id: &str) -> Result<()> {
        self.inner.report_play(track_id).await
    }

    async fn recently_played(&self) -> Result<Vec<Track>> {
        let tracks = self.inner.recently_played().await?;
        self.claim_tracks(&tracks);
        Ok(tracks)
    }

    async fn playlists(&self) -> Result<Vec<Playlist>> {
        let playlists = self.inner.playlists().await?;
        self.claim_playlists(&playlists);
        Ok(playlists)
    }

    async fn set_pinned(&self, uri: &str, pinned: bool) -> Result<PinOutcome> {
        self.inner.set_pinned(uri, pinned).await
    }

    async fn pin_targets(&self) -> Result<Option<Vec<PinTarget>>> {
        self.inner.pin_targets().await
    }

    fn pin_uri(&self, kind: PinTargetKind, id: &str) -> Option<String> {
        self.inner.pin_uri(kind, id)
    }

    async fn create_playlist(&self, name: &str) -> Result<String> {
        let id = self.inner.create_playlist(name).await?;
        self.claim(&id);
        Ok(id)
    }

    async fn rename_playlist(&self, playlist_id: &str, name: &str) -> Result<()> {
        self.inner.rename_playlist(playlist_id, name).await
    }

    async fn delete_playlist(&self, playlist_id: &str) -> Result<()> {
        self.inner.delete_playlist(playlist_id).await
    }

    async fn remove_playlist_from_library(&self, playlist_id: &str) -> Result<()> {
        self.inner.remove_playlist_from_library(playlist_id).await
    }

    async fn add_playlist_to_library(&self, playlist_id: &str) -> Result<()> {
        self.inner.add_playlist_to_library(playlist_id).await
    }

    async fn set_in_library(&self, kind: MediaKind, id: &str, present: bool) -> Result<()> {
        self.inner.set_in_library(kind, id, present).await
    }

    async fn set_playlist_public(&self, playlist_id: &str, public: bool) -> Result<()> {
        self.inner.set_playlist_public(playlist_id, public).await
    }

    async fn add_track_to_playlist(&self, playlist_id: &str, track_id: &str) -> Result<()> {
        self.inner
            .add_track_to_playlist(playlist_id, track_id)
            .await
    }

    async fn remove_track_from_playlist(&self, playlist_id: &str, track_id: &str) -> Result<()> {
        self.inner
            .remove_track_from_playlist(playlist_id, track_id)
            .await
    }

    async fn saved_albums(&self) -> Result<Vec<Album>> {
        let albums = self.inner.saved_albums().await?;
        self.claim_albums(&albums);
        Ok(albums)
    }

    async fn all_albums(&self) -> Result<Vec<Album>> {
        let albums = self.inner.all_albums().await?;
        self.claim_albums(&albums);
        Ok(albums)
    }

    async fn saved_albums_paged(&self) -> Result<Pages<Album>> {
        let pages = self.inner.saved_albums_paged().await?;
        Ok(self.watch_pages(pages, |watcher, album| watcher.claim_album(album)))
    }

    async fn all_albums_paged(&self) -> Result<Pages<Album>> {
        let pages = self.inner.all_albums_paged().await?;
        Ok(self.watch_pages(pages, |watcher, album| watcher.claim_album(album)))
    }

    async fn set_album_saved(&self, album_id: &str, saved: bool) -> Result<()> {
        self.inner.set_album_saved(album_id, saved).await
    }

    async fn saved_artists(&self) -> Result<Vec<SavedArtist>> {
        let artists = self.inner.saved_artists().await?;
        self.claim_artists(&artists);
        Ok(artists)
    }

    async fn all_artists(&self) -> Result<Vec<SavedArtist>> {
        let artists = self.inner.all_artists().await?;
        self.claim_artists(&artists);
        Ok(artists)
    }

    async fn saved_artists_paged(&self) -> Result<Pages<SavedArtist>> {
        let pages = self.inner.saved_artists_paged().await?;
        Ok(self.watch_pages(pages, |watcher, artist| watcher.claim_artist(artist)))
    }

    async fn all_artists_paged(&self) -> Result<Pages<SavedArtist>> {
        let pages = self.inner.all_artists_paged().await?;
        Ok(self.watch_pages(pages, |watcher, artist| watcher.claim_artist(artist)))
    }

    async fn set_artist_saved(&self, artist_id: &str, saved: bool) -> Result<()> {
        self.inner.set_artist_saved(artist_id, saved).await
    }

    async fn album(&self, album_id: &str) -> Result<AlbumDetail> {
        let detail = self.inner.album(album_id).await?;
        self.claim_album(&detail.album);
        self.claim_tracks(&detail.tracks);
        Ok(detail)
    }

    async fn album_tracks(&self, album_id: &str) -> Result<Vec<Track>> {
        let tracks = self.inner.album_tracks(album_id).await?;
        self.claim_tracks(&tracks);
        Ok(tracks)
    }

    async fn album_catalogue(
        &self,
        album_id: &str,
        artist_id: Option<&str>,
    ) -> Result<AlbumCatalogue> {
        let catalogue = self.inner.album_catalogue(album_id, artist_id).await?;
        self.claim_albums(&catalogue.also_like);
        self.claim_artists(&catalogue.similar);
        Ok(catalogue)
    }

    async fn playlist(&self, playlist_id: &str) -> Result<PlaylistDetail> {
        let detail = self.inner.playlist(playlist_id).await?;
        self.claim_playlist(&detail.playlist);
        self.claim_tracks(&detail.tracks);
        Ok(detail)
    }

    async fn playlist_continuation(
        &self,
        continuation: &str,
    ) -> Result<(Vec<Track>, Option<String>)> {
        let (tracks, next) = self.inner.playlist_continuation(continuation).await?;
        self.claim_tracks(&tracks);
        Ok((tracks, next))
    }

    async fn playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>> {
        let tracks = self.inner.playlist_tracks(playlist_id).await?;
        self.claim_tracks(&tracks);
        Ok(tracks)
    }

    async fn playlist_covers(&self, playlist_id: &str, wanted: usize) -> Result<Vec<String>> {
        self.inner.playlist_covers(playlist_id, wanted).await
    }

    async fn track_radio(
        &self,
        track_id: &str,
        from: Option<&str>,
    ) -> Result<(Vec<Track>, Option<String>)> {
        let (tracks, next) = self.inner.track_radio(track_id, from).await?;
        self.claim_tracks(&tracks);
        Ok((tracks, next))
    }

    async fn search(&self, query: &str) -> Result<Vec<Track>> {
        let tracks = self.inner.search(query).await?;
        self.claim_tracks(&tracks);
        Ok(tracks)
    }

    async fn search_albums(&self, query: &str) -> Result<Vec<Album>> {
        let albums = self.inner.search_albums(query).await?;
        self.claim_albums(&albums);
        Ok(albums)
    }

    async fn search_playlists(&self, query: &str) -> Result<Vec<Playlist>> {
        let playlists = self.inner.search_playlists(query).await?;
        self.claim_playlists(&playlists);
        Ok(playlists)
    }

    async fn home(&self) -> Result<HomeFeed> {
        let feed = self.inner.home().await?;
        self.claim_feed(&feed);
        Ok(feed)
    }

    async fn home_paged(&self) -> Result<Feed> {
        let feed = self.inner.home_paged().await?;
        Ok(self.watch_feed(feed))
    }

    async fn name_home_playlists(&self, sections: Vec<GenreSection>) -> Vec<GenreSection> {
        let sections = self.inner.name_home_playlists(sections).await;
        self.claim_sections(&sections);
        sections
    }

    async fn genres(&self) -> Result<Vec<Genre>> {
        let genres = self.inner.genres().await?;
        for genre in &genres {
            self.claim(&genre.id);
        }
        Ok(genres)
    }

    async fn genre(&self, genre_id: &str) -> Result<GenreDetail> {
        let detail = self.inner.genre(genre_id).await?;
        self.claim_sections(&detail.sections);
        Ok(detail)
    }
}

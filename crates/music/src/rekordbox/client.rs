//! The rekordbox collection as a [`MusicApi`]. Stars stay in Sonora. Playlist create, rename,
//! delete and track edits are written back to `master.db`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use storage::Database;

use crate::{
    Album, AlbumCatalogue, AlbumDetail, Artist, ArtistProfile, GenreItem, GenreSection, HomeFeed,
    MediaKind, MusicApi, Playlist, PlaylistDetail, SUGGESTIONS, SavedArtist, Track, UserProfile,
    distinct_covers,
};

use super::cipher::Seal;
use super::ids;
use super::read::Catalog;
use super::stars::{Kind, Stars};
use super::write::{self, Edit};

pub struct Client {
    catalog: Mutex<Catalog>,
    database: PathBuf,
    seal: Mutex<Seal>,
    writing: Mutex<()>,
    stars: Stars,
}

impl Client {
    pub fn new(catalog: Catalog, database: PathBuf, seal: Seal, stars: Database) -> Self {
        Self {
            catalog: Mutex::new(catalog),
            database,
            seal: Mutex::new(seal),
            writing: Mutex::new(()),
            stars: Stars::new(stars),
        }
    }

    fn catalog(&self) -> std::sync::MutexGuard<'_, Catalog> {
        self.catalog.lock().unwrap_or_else(|err| err.into_inner())
    }

    fn track(&self, id: &str) -> Option<Track> {
        self.catalog()
            .tracks
            .iter()
            .find(|track| track.id.as_deref() == Some(id))
            .cloned()
    }

    fn playlist(&self, id: &str) -> Result<super::read::PlaylistEntry> {
        self.catalog()
            .playlists
            .iter()
            .find(|playlist| playlist.id == id)
            .cloned()
            .ok_or_else(|| anyhow!("cannot find rekordbox playlist {id}"))
    }

    fn starred_ids(&self, kind: Kind) -> Vec<String> {
        self.stars
            .starred(kind)
            .unwrap_or_default()
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    fn edit(&self, edit: Edit) -> Result<String> {
        let _gate = self.writing.lock().unwrap_or_else(|err| err.into_inner());
        let seal = self
            .seal
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        let saved = write::commit(&self.database, &seal, edit)?;
        *self.seal.lock().unwrap_or_else(|err| err.into_inner()) = saved.seal;
        let id = saved.playlist_id;
        *self.catalog.lock().unwrap_or_else(|err| err.into_inner()) = saved.catalog;
        Ok(id)
    }
}

fn playlist_model(entry: &super::read::PlaylistEntry) -> Playlist {
    Playlist {
        id: entry.id.clone(),
        name: entry.name.clone(),
        owner: String::new(),
        owner_id: String::new(),
        owned: true,
        collaborative: false,
        blend: false,
        public: false,
        cover: entry
            .cover
            .clone()
            .or_else(|| entry.tracks.iter().find_map(|track| track.cover.clone())),
        track_count: entry.tracks.len() as u32,
        modified_at: None,
    }
}

#[async_trait]
impl MusicApi for Client {
    fn share_url(&self, kind: MediaKind, id: &str) -> Option<String> {
        match kind {
            MediaKind::Track => {
                let path = ids::path_from_track_id(id)?;
                Some(format!("file://{}", path.display()))
            }
            MediaKind::Album | MediaKind::Artist | MediaKind::Playlist => None,
        }
    }

    async fn profile(&self) -> Result<UserProfile> {
        Ok(UserProfile {
            id: "rekordbox".to_owned(),
            display_name: "Rekordbox".to_owned(),
            avatar: None,
        })
    }

    async fn artist(&self, artist_id: &str) -> Result<Artist> {
        let catalog = self.catalog();
        let artist = catalog
            .artists
            .iter()
            .find(|artist| artist.id == artist_id)
            .cloned()
            .ok_or_else(|| anyhow!("cannot find rekordbox artist {artist_id}"))?;
        Ok(Artist {
            name: artist.name,
            cover_large: artist.cover,
            biography: None,
            monthly_listeners: None,
            top_tracks: catalog
                .tracks
                .iter()
                .filter(|track| {
                    track
                        .artist_refs
                        .iter()
                        .any(|artist_ref| artist_ref.id.as_deref() == Some(artist_id))
                })
                .cloned()
                .collect(),
            albums: catalog
                .albums
                .iter()
                .filter(|album| {
                    album
                        .artist_refs
                        .iter()
                        .any(|artist_ref| artist_ref.id.as_deref() == Some(artist_id))
                })
                .cloned()
                .collect(),
        })
    }

    async fn artist_profile(&self, artist_id: &str) -> Result<ArtistProfile> {
        let artist = self.artist(artist_id).await?;
        Ok(ArtistProfile {
            name: artist.name,
            cover_large: artist.cover_large,
            biography: None,
        })
    }

    async fn artist_images(&self, ids: Vec<String>) -> Result<HashMap<String, String>> {
        let catalog = self.catalog();
        Ok(ids
            .into_iter()
            .filter_map(|id| {
                catalog
                    .artists
                    .iter()
                    .find(|artist| artist.id == id)
                    .and_then(|artist| artist.cover.clone())
                    .map(|cover| (id, cover))
            })
            .collect())
    }

    async fn saved_tracks(&self) -> Result<Vec<Track>> {
        let ids = self.starred_ids(Kind::Track);
        Ok(ids.iter().filter_map(|id| self.track(id)).collect())
    }

    async fn set_track_saved(&self, track_id: &str, saved: bool) -> Result<()> {
        self.stars.set(Kind::Track, track_id, saved)
    }

    async fn track(&self, track_id: &str) -> Result<Track> {
        self.track(track_id)
            .ok_or_else(|| anyhow!("cannot find rekordbox track {track_id}"))
    }

    async fn track_playcount(&self, track_id: &str) -> Result<Option<u64>> {
        Ok(self.track(track_id).and_then(|track| track.playcount))
    }

    async fn playlists(&self) -> Result<Vec<Playlist>> {
        Ok(self
            .catalog()
            .playlists
            .iter()
            .map(playlist_model)
            .collect())
    }

    async fn create_playlist(&self, name: &str) -> Result<String> {
        self.edit(Edit::Create(name.to_owned()))
    }

    async fn rename_playlist(&self, playlist_id: &str, name: &str) -> Result<()> {
        self.edit(Edit::Rename {
            playlist: playlist_id.to_owned(),
            name: name.to_owned(),
        })
        .map(|_| ())
    }

    async fn delete_playlist(&self, playlist_id: &str) -> Result<()> {
        self.edit(Edit::Delete(playlist_id.to_owned())).map(|_| ())
    }

    async fn remove_playlist_from_library(&self, playlist_id: &str) -> Result<()> {
        self.delete_playlist(playlist_id).await
    }

    async fn add_playlist_to_library(&self, _playlist_id: &str) -> Result<()> {
        anyhow::bail!("a rekordbox playlist is already in the library")
    }

    async fn set_playlist_public(&self, _playlist_id: &str, _public: bool) -> Result<()> {
        anyhow::bail!("rekordbox playlists have no public setting")
    }

    async fn add_track_to_playlist(&self, playlist_id: &str, track_id: &str) -> Result<()> {
        self.edit(Edit::Add {
            playlist: playlist_id.to_owned(),
            track: track_id.to_owned(),
        })
        .map(|_| ())
    }

    async fn remove_track_from_playlist(&self, playlist_id: &str, track_id: &str) -> Result<()> {
        self.edit(Edit::Remove {
            playlist: playlist_id.to_owned(),
            track: track_id.to_owned(),
        })
        .map(|_| ())
    }

    async fn saved_albums(&self) -> Result<Vec<Album>> {
        let ids = self.starred_ids(Kind::Album);
        Ok(ids
            .iter()
            .filter_map(|id| {
                self.catalog()
                    .albums
                    .iter()
                    .find(|album| album.id == *id)
                    .cloned()
            })
            .collect())
    }

    async fn set_album_saved(&self, album_id: &str, saved: bool) -> Result<()> {
        self.stars.set(Kind::Album, album_id, saved)
    }

    async fn saved_artists(&self) -> Result<Vec<SavedArtist>> {
        let ids = self.starred_ids(Kind::Artist);
        Ok(ids
            .iter()
            .filter_map(|id| {
                self.catalog()
                    .artists
                    .iter()
                    .find(|artist| artist.id == *id)
                    .cloned()
            })
            .collect())
    }

    async fn set_artist_saved(&self, artist_id: &str, saved: bool) -> Result<()> {
        self.stars.set(Kind::Artist, artist_id, saved)
    }

    async fn all_tracks(&self) -> Result<Vec<Track>> {
        Ok(self.catalog().tracks.clone())
    }

    async fn all_albums(&self) -> Result<Vec<Album>> {
        Ok(self.catalog().albums.clone())
    }

    async fn all_artists(&self) -> Result<Vec<SavedArtist>> {
        Ok(self.catalog().artists.clone())
    }

    async fn album(&self, album_id: &str) -> Result<AlbumDetail> {
        let album = self
            .catalog()
            .albums
            .iter()
            .find(|album| album.id == album_id)
            .cloned()
            .ok_or_else(|| anyhow!("cannot find rekordbox album {album_id}"))?;
        Ok(AlbumDetail {
            tracks: self.album_tracks(album_id).await?,
            album,
        })
    }

    async fn album_tracks(&self, album_id: &str) -> Result<Vec<Track>> {
        let mut tracks: Vec<Track> = self
            .catalog()
            .tracks
            .iter()
            .filter(|track| track.album_id.as_deref() == Some(album_id))
            .cloned()
            .collect();
        tracks.sort_by(|left, right| {
            (left.disc_number, left.track_number)
                .cmp(&(right.disc_number, right.track_number))
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(tracks)
    }

    async fn album_catalogue(
        &self,
        album_id: &str,
        _artist_id: Option<&str>,
    ) -> Result<AlbumCatalogue> {
        let Some(artists) = self
            .catalog()
            .albums
            .iter()
            .find(|album| album.id == album_id)
            .map(|album| album.artists.clone())
        else {
            return Ok(AlbumCatalogue::default());
        };
        Ok(AlbumCatalogue {
            also_like: self
                .catalog()
                .albums
                .iter()
                .filter(|album| album.id != album_id && album.artists == artists)
                .take(SUGGESTIONS)
                .cloned()
                .collect(),
            similar: Vec::new(),
        })
    }

    async fn playlist(&self, playlist_id: &str) -> Result<PlaylistDetail> {
        let entry = self.playlist(playlist_id)?;
        Ok(PlaylistDetail {
            playlist: playlist_model(&entry),
            tracks: entry.tracks.clone(),
            continuation: None,
        })
    }

    async fn playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>> {
        Ok(self.playlist(playlist_id)?.tracks)
    }

    async fn playlist_covers(&self, playlist_id: &str, wanted: usize) -> Result<Vec<String>> {
        let entry = self.playlist(playlist_id)?;
        Ok(distinct_covers(&entry.tracks, wanted))
    }

    async fn track_radio(
        &self,
        _track_id: &str,
        _from: Option<&str>,
    ) -> Result<(Vec<Track>, Option<String>)> {
        Ok((Vec::new(), None))
    }

    async fn search(&self, query: &str) -> Result<Vec<Track>> {
        let query = query.to_lowercase();
        Ok(self
            .catalog()
            .tracks
            .iter()
            .filter(|track| {
                track.name.to_lowercase().contains(&query)
                    || track.artists.to_lowercase().contains(&query)
                    || track.album.to_lowercase().contains(&query)
            })
            .cloned()
            .collect())
    }

    async fn search_albums(&self, query: &str) -> Result<Vec<Album>> {
        let query = query.to_lowercase();
        Ok(self
            .catalog()
            .albums
            .iter()
            .filter(|album| {
                album.name.to_lowercase().contains(&query)
                    || album.artists.to_lowercase().contains(&query)
            })
            .cloned()
            .collect())
    }

    async fn search_playlists(&self, query: &str) -> Result<Vec<Playlist>> {
        let query = query.to_lowercase();
        Ok(self
            .catalog()
            .playlists
            .iter()
            .filter(|playlist| playlist.name.to_lowercase().contains(&query))
            .map(playlist_model)
            .collect())
    }

    async fn home(&self) -> Result<HomeFeed> {
        let mut sections = Vec::new();
        if !self.catalog().playlists.is_empty() {
            sections.push(GenreSection {
                title: "home-playlists".to_owned(),
                items: self
                    .catalog()
                    .playlists
                    .iter()
                    .take(15)
                    .map(playlist_model)
                    .map(GenreItem::Playlist)
                    .collect(),
            });
        }
        if !self.catalog().albums.is_empty() {
            sections.push(GenreSection {
                title: "home-collection-albums".to_owned(),
                items: self
                    .catalog()
                    .albums
                    .iter()
                    .take(15)
                    .cloned()
                    .map(GenreItem::Album)
                    .collect(),
            });
        }
        Ok(HomeFeed {
            listen_again: Vec::new(),
            quick_picks: None,
            sections,
        })
    }
}

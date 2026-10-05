//! Ids for a rekordbox library. A track id carries the database id and the file path, so playback
//! can open the file without asking the database again. Album, artist and playlist ids carry only
//! the database id.

use std::path::Path;

pub const TRACK_PREFIX: &str = "rekordbox:";
pub const MISSING_PREFIX: &str = "rekordbox-missing:";
pub const ALBUM_PREFIX: &str = "rekordbox-album:";
pub const ARTIST_PREFIX: &str = "rekordbox-artist:";
pub const PLAYLIST_PREFIX: &str = "rekordbox-playlist:";

pub fn track_id(content_id: &str, path: Option<&Path>) -> String {
    match path {
        Some(path) => format!("{TRACK_PREFIX}{content_id}:{}", path.display()),
        None => format!("{MISSING_PREFIX}{content_id}"),
    }
}

pub fn album_id(id: &str) -> String {
    format!("{ALBUM_PREFIX}{id}")
}

pub fn artist_id(id: &str) -> String {
    format!("{ARTIST_PREFIX}{id}")
}

pub fn playlist_id(id: &str) -> String {
    format!("{PLAYLIST_PREFIX}{id}")
}

pub fn is_rekordbox_id(id: &str) -> bool {
    id.starts_with(TRACK_PREFIX)
        || id.starts_with(MISSING_PREFIX)
        || id.starts_with(ALBUM_PREFIX)
        || id.starts_with(ARTIST_PREFIX)
        || id.starts_with(PLAYLIST_PREFIX)
}

/// The database id of a track, from either a playable id or one whose file was missing.
pub fn content_id(id: &str) -> Option<&str> {
    if let Some(rest) = id.strip_prefix(TRACK_PREFIX) {
        return rest.split_once(':').map(|(id, _)| id).or(Some(rest));
    }
    id.strip_prefix(MISSING_PREFIX)
}

pub fn path_from_track_id(id: &str) -> Option<&Path> {
    let rest = id.strip_prefix(TRACK_PREFIX)?;
    let (_, path) = rest.split_once(':')?;
    (!path.is_empty()).then(|| Path::new(path))
}

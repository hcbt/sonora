//! Reads a rekordbox `master.db` into the models the rest of the app already shows.
//!
//! Playlists, artists and albums come from the tables rekordbox itself keeps. A playlist folder
//! is not a playlist: its name is prefixed onto the lists inside it, in the order rekordbox
//! shows them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use rusqlite::{Connection, Row, types::ValueRef};

use crate::{
    Album, ArtistRef, KeyMode, MusicalKey, ReleaseType, SavedArtist, Track, TrackRhythm,
    iso_8601_to_epoch,
};

use super::ids;

/// Cloud Library Sync and the CUE analysis list. They are rekordbox's own, not a playlist the
/// listener made.
const SPECIAL: &[&str] = &["100000", "200000"];

pub struct Catalog {
    pub tracks: Vec<Track>,
    pub albums: Vec<Album>,
    pub artists: Vec<SavedArtist>,
    pub playlists: Vec<PlaylistEntry>,
}

#[derive(Clone)]
pub struct PlaylistEntry {
    pub id: String,
    pub name: String,
    pub cover: Option<String>,
    pub tracks: Vec<Track>,
}

/// Opens `master.db` in `folder` (or `folder` itself when it is the file) and reads the
/// collection. `plain` is the unlocked SQLite image.
pub fn catalog(plain: &[u8], folder: &Path) -> Result<Catalog> {
    let file = tempfile::NamedTempFile::new().context("cannot stage the rekordbox database")?;
    std::fs::write(file.path(), plain).context("cannot stage the rekordbox database")?;
    let connection = Connection::open(file.path()).context("cannot open the rekordbox database")?;
    connection
        .query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
        .context("the rekordbox database did not unlock")?;

    let root = if folder.is_file() {
        folder.parent().unwrap_or(folder)
    } else {
        folder
    };
    read(&connection, root)
}

fn read(connection: &Connection, root: &Path) -> Result<Catalog> {
    if !table(connection, "djmdContent")? {
        bail!("this folder does not hold a rekordbox collection");
    }

    let artists = names(connection, "djmdArtist")?;
    let genres = names(connection, "djmdGenre")?;
    let keys = keys(connection)?;
    let albums = albums(connection, root)?;
    let (tracks, years) = tracks(connection, root, &artists, &albums, &genres, &keys)?;
    let by_id = tracks
        .iter()
        .filter_map(|track| {
            ids::content_id(track.id.as_deref()?).map(|id| (id.to_owned(), track.clone()))
        })
        .collect::<HashMap<_, _>>();
    let playlists = playlists(connection, root, &by_id)?;

    let album_models = album_models(&tracks, &albums, &artists, &years);
    let artist_models = artist_models(&tracks);

    Ok(Catalog {
        tracks,
        albums: album_models,
        artists: artist_models,
        playlists,
    })
}

struct AlbumRow {
    name: String,
    artist_id: String,
    image: Option<String>,
    compilation: bool,
}

fn albums(connection: &Connection, root: &Path) -> Result<HashMap<String, AlbumRow>> {
    if !table(connection, "djmdAlbum")? {
        return Ok(HashMap::new());
    }
    let columns = columns(connection, "djmdAlbum")?;
    let mut query = connection
        .prepare(&select(
            "djmdAlbum",
            &columns,
            &["ID", "Name", "AlbumArtistID", "ImagePath", "Compilation"],
        ))
        .context("cannot read rekordbox albums")?;
    let rows = query
        .query_map([], |row| {
            Ok((
                cell(row, 0),
                AlbumRow {
                    name: cell(row, 1),
                    artist_id: cell(row, 2),
                    image: artwork(root, &cell(row, 3)),
                    compilation: flag(&cell(row, 4)),
                },
            ))
        })
        .context("cannot read rekordbox albums")?;
    let mut albums = HashMap::new();
    for row in rows {
        let (id, album) = row.context("cannot read a rekordbox album")?;
        if id.is_empty() {
            continue;
        }
        albums.insert(id, album);
    }
    Ok(albums)
}

fn tracks(
    connection: &Connection,
    root: &Path,
    artists: &HashMap<String, String>,
    albums: &HashMap<String, AlbumRow>,
    genres: &HashMap<String, String>,
    keys: &HashMap<String, String>,
) -> Result<(Vec<Track>, HashMap<String, i32>)> {
    let columns = columns(connection, "djmdContent")?;
    let wanted = [
        "ID",
        "FolderPath",
        "rb_LocalFolderPath",
        "Title",
        "ArtistID",
        "AlbumID",
        "GenreID",
        "BPM",
        "Length",
        "TrackNo",
        "DiscNo",
        "ReleaseYear",
        "KeyID",
        "ImagePath",
        "DJPlayCount",
        "DateCreated",
        "FileNameL",
    ];
    let mut query = connection
        .prepare(&select("djmdContent", &columns, &wanted))
        .context("cannot read the rekordbox collection")?;
    let rows = query
        .query_map([], |row| Ok(content(row)))
        .context("cannot read the rekordbox collection")?;

    let mut tracks = Vec::new();
    let mut years: HashMap<String, i32> = HashMap::new();
    for row in rows {
        let row = row.context("cannot read a rekordbox track")?;
        if row.id.is_empty() {
            continue;
        }
        if row.year > 0 && !row.album_id.is_empty() {
            let id = ids::album_id(&row.album_id);
            years
                .entry(id)
                .and_modify(|year| *year = (*year).max(row.year as i32))
                .or_insert(row.year as i32);
        }
        tracks.push(track(row, root, artists, albums, genres, keys));
    }
    tracks.sort_by(|left, right| {
        left.album
            .cmp(&right.album)
            .then(left.disc_number.cmp(&right.disc_number))
            .then(left.track_number.cmp(&right.track_number))
            .then(left.name.cmp(&right.name))
    });
    Ok((tracks, years))
}

struct Content {
    id: String,
    path: String,
    relocated: String,
    title: String,
    artist_id: String,
    album_id: String,
    genre_id: String,
    bpm: i64,
    length: i64,
    track_no: i64,
    disc_no: i64,
    year: i64,
    key_id: String,
    image: String,
    plays: i64,
    created: String,
    file_name: String,
}

fn content(row: &Row<'_>) -> Content {
    Content {
        id: cell(row, 0),
        path: cell(row, 1),
        relocated: cell(row, 2),
        title: cell(row, 3),
        artist_id: cell(row, 4),
        album_id: cell(row, 5),
        genre_id: cell(row, 6),
        bpm: number(row, 7),
        length: number(row, 8),
        track_no: number(row, 9),
        disc_no: number(row, 10),
        year: number(row, 11),
        key_id: cell(row, 12),
        image: cell(row, 13),
        plays: number(row, 14),
        created: cell(row, 15),
        file_name: cell(row, 16),
    }
}

fn track(
    row: Content,
    root: &Path,
    artists: &HashMap<String, String>,
    albums: &HashMap<String, AlbumRow>,
    genres: &HashMap<String, String>,
    keys: &HashMap<String, String>,
) -> Track {
    let path = located(&row.path, &row.relocated);
    let album = albums.get(&row.album_id);
    let artist_name = artists.get(&row.artist_id).cloned().unwrap_or_default();
    let album_name = album
        .map(|album| album.name.clone())
        .filter(|name| !name.is_empty())
        .unwrap_or_default();
    let title = if row.title.is_empty() {
        row.file_name
            .is_empty()
            .then(|| {
                path.as_ref()
                    .and_then(|path| path.file_stem())
                    .and_then(|stem| stem.to_str())
            })
            .flatten()
            .unwrap_or("Unknown")
            .to_owned()
    } else {
        row.title
    };
    let cover = artwork(root, &row.image).or_else(|| album.and_then(|album| album.image.clone()));
    let key = keys.get(&row.key_id).cloned().filter(|key| !key.is_empty());
    let playable = path.as_ref().is_some_and(|path| path.is_file());

    Track {
        id: Some(ids::track_id(&row.id, path.as_deref())),
        name: title,
        playable,
        artists: artist_name.clone(),
        artist_refs: if !row.artist_id.is_empty() && !artist_name.is_empty() {
            vec![ArtistRef {
                name: artist_name,
                id: Some(ids::artist_id(&row.artist_id)),
            }]
        } else {
            Vec::new()
        },
        album: album_name,
        album_id: if row.album_id.is_empty() {
            None
        } else {
            Some(ids::album_id(&row.album_id))
        },
        cover,
        duration: duration(row.length),
        added_at: (!row.created.is_empty())
            .then(|| iso_8601_to_epoch(Some(row.created.as_str())))
            .flatten(),
        added_by: None,
        playcount: (row.plays > 0).then_some(row.plays as u64),
        popularity: 0,
        explicit: false,
        track_number: row.track_no.max(0) as u32,
        disc_number: row.disc_no.max(0) as u32,
        tags: genres
            .get(&row.genre_id)
            .filter(|genre| !genre.is_empty())
            .cloned()
            .into_iter()
            .collect(),
        languages: Vec::new(),
        credits: Vec::new(),
        rhythm: rhythm(row.bpm, key).packed(),
    }
}

fn album_models(
    tracks: &[Track],
    albums: &HashMap<String, AlbumRow>,
    artists: &HashMap<String, String>,
    years: &HashMap<String, i32>,
) -> Vec<Album> {
    let mut grouped: HashMap<String, Vec<&Track>> = HashMap::new();
    for track in tracks {
        let Some(id) = track.album_id.clone() else {
            continue;
        };
        grouped.entry(id).or_default().push(track);
    }

    let mut models = Vec::new();
    for (id, songs) in grouped {
        let db_id = id
            .strip_prefix(ids::ALBUM_PREFIX)
            .unwrap_or(id.as_str())
            .to_owned();
        let row = albums.get(&db_id);
        let first = songs[0];
        let artist = row
            .and_then(|row| {
                artists
                    .get(&row.artist_id)
                    .filter(|name| !name.is_empty())
                    .cloned()
            })
            .unwrap_or_else(|| first.artists.clone());
        let cover = row
            .and_then(|row| row.image.clone())
            .or_else(|| songs.iter().find_map(|track| track.cover.clone()));
        models.push(Album {
            id: id.clone(),
            name: row
                .map(|row| row.name.clone())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| first.album.clone()),
            artists: artist.clone(),
            artist_refs: first.artist_refs.clone(),
            cover: cover.clone(),
            cover_large: cover,
            release_type: if row.is_some_and(|row| row.compilation) {
                ReleaseType::Compilation
            } else {
                ReleaseType::Album
            },
            year: years.get(&id).copied().unwrap_or(0),
            track_count: songs.len() as u32,
            release_date: String::new(),
            label: String::new(),
            copyrights: Vec::new(),
            added_at: songs.iter().filter_map(|track| track.added_at).max(),
        });
    }
    models.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.artists.cmp(&right.artists))
    });
    models
}

fn artist_models(tracks: &[Track]) -> Vec<SavedArtist> {
    let mut seen = HashMap::new();
    for track in tracks {
        for artist in &track.artist_refs {
            let Some(id) = artist.id.clone() else {
                continue;
            };
            seen.entry(id).or_insert_with(|| SavedArtist {
                id: artist.id.clone().unwrap_or_default(),
                name: artist.name.clone(),
                cover: track.cover.clone(),
                added_at: track.added_at,
            });
        }
    }
    let mut models: Vec<SavedArtist> = seen.into_values().collect();
    models.sort_by(|left, right| left.name.cmp(&right.name));
    models
}

struct Node {
    id: String,
    name: String,
    parent: String,
    seq: i64,
    folder: bool,
    image: String,
}

fn playlists(
    connection: &Connection,
    root: &Path,
    tracks: &HashMap<String, Track>,
) -> Result<Vec<PlaylistEntry>> {
    if !table(connection, "djmdPlaylist")? {
        return Ok(Vec::new());
    }
    let columns = columns(connection, "djmdPlaylist")?;
    let mut query = connection
        .prepare(&select(
            "djmdPlaylist",
            &columns,
            &["ID", "Name", "ParentID", "Seq", "Attribute", "ImagePath"],
        ))
        .context("cannot read rekordbox playlists")?;
    let rows = query
        .query_map([], |row| {
            let attribute = number(row, 4);
            Ok(Node {
                id: cell(row, 0),
                name: cell(row, 1),
                parent: cell(row, 2),
                seq: number(row, 3),
                folder: attribute == 1,
                image: cell(row, 5),
            })
        })
        .context("cannot read rekordbox playlists")?;
    let nodes = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("cannot read rekordbox playlists")?;
    let songs = playlist_songs(connection)?;

    let mut children: HashMap<String, Vec<usize>> = HashMap::new();
    let ids = nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    for (index, node) in nodes.iter().enumerate() {
        if SPECIAL.contains(&node.id.as_str()) {
            continue;
        }
        let parent = if node.parent.is_empty()
            || node.parent == "0"
            || !ids.contains(node.parent.as_str())
        {
            String::new()
        } else {
            node.parent.clone()
        };
        children.entry(parent).or_default().push(index);
    }
    for kids in children.values_mut() {
        kids.sort_by_key(|index| (nodes[*index].seq, nodes[*index].name.clone()));
    }

    let mut listed = Vec::new();
    walk(&nodes, &children, &songs, tracks, root, "", &mut listed);
    Ok(listed)
}

fn walk(
    nodes: &[Node],
    children: &HashMap<String, Vec<usize>>,
    songs: &HashMap<String, Vec<String>>,
    tracks: &HashMap<String, Track>,
    root: &Path,
    prefix: &str,
    out: &mut Vec<PlaylistEntry>,
) {
    let Some(kids) = children.get(prefix) else {
        return;
    };
    for index in kids {
        let node = &nodes[*index];
        if SPECIAL.contains(&node.id.as_str()) {
            continue;
        }
        if node.folder {
            walk(nodes, children, songs, tracks, root, &node.id, out);
            continue;
        }
        let name = display_name(nodes, node);
        let songs = songs.get(&node.id).cloned().unwrap_or_default();
        let tracks = songs
            .iter()
            .filter_map(|id| tracks.get(id).cloned())
            .collect::<Vec<_>>();
        let cover = artwork(root, &node.image)
            .or_else(|| tracks.iter().find_map(|track| track.cover.clone()));
        out.push(PlaylistEntry {
            id: ids::playlist_id(&node.id),
            name,
            cover,
            tracks,
        });
    }
}

fn display_name(nodes: &[Node], node: &Node) -> String {
    let mut names = vec![node.name.clone()];
    let mut parent = node.parent.as_str();
    let mut guard = 0;
    while !parent.is_empty() && parent != "0" && guard < 32 {
        guard += 1;
        let Some(folder) = nodes.iter().find(|candidate| candidate.id == parent) else {
            break;
        };
        if SPECIAL.contains(&folder.id.as_str()) {
            break;
        }
        if !folder.name.is_empty() {
            names.push(folder.name.clone());
        }
        parent = folder.parent.as_str();
    }
    names.reverse();
    names.join(" / ")
}

fn playlist_songs(connection: &Connection) -> Result<HashMap<String, Vec<String>>> {
    if !table(connection, "djmdSongPlaylist")? {
        return Ok(HashMap::new());
    }
    let columns = columns(connection, "djmdSongPlaylist")?;
    let mut query = connection
        .prepare(&select(
            "djmdSongPlaylist",
            &columns,
            &["PlaylistID", "ContentID", "TrackNo"],
        ))
        .context("cannot read rekordbox playlist tracks")?;
    let rows = query
        .query_map([], |row| Ok((cell(row, 0), cell(row, 1), number(row, 2))))
        .context("cannot read rekordbox playlist tracks")?;
    let mut grouped: HashMap<String, Vec<(i64, String)>> = HashMap::new();
    for row in rows {
        let (playlist, content, track_no) =
            row.context("cannot read a rekordbox playlist track")?;
        if playlist.is_empty() || content.is_empty() {
            continue;
        }
        grouped
            .entry(playlist)
            .or_default()
            .push((track_no, content));
    }
    Ok(grouped
        .into_iter()
        .map(|(id, mut songs)| {
            songs.sort_by_key(|(track_no, _)| *track_no);
            (id, songs.into_iter().map(|(_, content)| content).collect())
        })
        .collect())
}

fn names(connection: &Connection, table_name: &str) -> Result<HashMap<String, String>> {
    if !table(connection, table_name)? {
        return Ok(HashMap::new());
    }
    let columns = columns(connection, table_name)?;
    let mut query = connection
        .prepare(&select(table_name, &columns, &["ID", "Name"]))
        .with_context(|| format!("cannot read rekordbox {table_name}"))?;
    let rows = query
        .query_map([], |row| Ok((cell(row, 0), cell(row, 1))))
        .with_context(|| format!("cannot read rekordbox {table_name}"))?;
    let mut names = HashMap::new();
    for row in rows {
        let (id, name) = row.with_context(|| format!("cannot read rekordbox {table_name}"))?;
        if !id.is_empty() {
            names.insert(id, name);
        }
    }
    Ok(names)
}

fn keys(connection: &Connection) -> Result<HashMap<String, String>> {
    if !table(connection, "djmdKey")? {
        return Ok(HashMap::new());
    }
    let columns = columns(connection, "djmdKey")?;
    let name = if columns.iter().any(|column| column == "ScaleName") {
        "ScaleName"
    } else {
        "Name"
    };
    let mut query = connection
        .prepare(&select("djmdKey", &columns, &["ID", name]))
        .context("cannot read rekordbox keys")?;
    let rows = query
        .query_map([], |row| Ok((cell(row, 0), cell(row, 1))))
        .context("cannot read rekordbox keys")?;
    let mut keys = HashMap::new();
    for row in rows {
        let (id, name) = row.context("cannot read a rekordbox key")?;
        if !id.is_empty() {
            keys.insert(id, name);
        }
    }
    Ok(keys)
}

fn table(connection: &Connection, name: &str) -> Result<bool> {
    let found = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?",
            [name],
            |_| Ok(()),
        )
        .optional()
        .with_context(|| format!("cannot look for {name}"))?;
    Ok(found.is_some())
}

fn columns(connection: &Connection, table_name: &str) -> Result<Vec<String>> {
    let mut query = connection
        .prepare(&format!("PRAGMA table_info({table_name})"))
        .with_context(|| format!("cannot read {table_name}"))?;
    let rows = query
        .query_map([], |row| row.get::<_, String>(1))
        .with_context(|| format!("cannot read {table_name}"))?;
    rows.collect::<rusqlite::Result<_>>()
        .with_context(|| format!("cannot read {table_name}"))
}

/// Selects `wanted` columns that exist, substituting NULL for one the file does not have, so an
/// older rekordbox database still opens.
fn select(table_name: &str, have: &[String], wanted: &[&str]) -> String {
    let fields = wanted
        .iter()
        .map(|name| match have.iter().any(|column| column == name) {
            true => (*name).to_owned(),
            false => format!("NULL AS {name}"),
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("SELECT {fields} FROM {table_name}")
}

fn cell(row: &Row<'_>, index: usize) -> String {
    match row.get_ref(index) {
        Ok(ValueRef::Text(bytes)) => String::from_utf8_lossy(bytes).trim().to_owned(),
        Ok(ValueRef::Integer(value)) => value.to_string(),
        Ok(ValueRef::Real(value)) => value.to_string(),
        _ => String::new(),
    }
}

fn number(row: &Row<'_>, index: usize) -> i64 {
    match row.get_ref(index) {
        Ok(ValueRef::Integer(value)) => value,
        Ok(ValueRef::Real(value)) => value as i64,
        Ok(ValueRef::Text(bytes)) => String::from_utf8_lossy(bytes).trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn flag(value: &str) -> bool {
    matches!(value, "1" | "true" | "TRUE")
}

/// Rekordbox stores BPM as hundredths (`12800` is 128.00). A value that is already a tempo is
/// left alone, so a database that stored it plain still reads.
fn rhythm(bpm: i64, written: Option<String>) -> TrackRhythm {
    let bpm = match bpm {
        0 => None,
        value if value >= 1_000 => Some((value / 100) as u32),
        value => Some(value as u32),
    };
    TrackRhythm {
        bpm,
        key: written.as_deref().and_then(musical_key),
        written,
    }
}

fn musical_key(name: &str) -> Option<MusicalKey> {
    let (pitch, mode) = name
        .strip_suffix('m')
        .map(|pitch| (pitch, KeyMode::Minor))
        .or_else(|| {
            name.strip_suffix("min")
                .map(|pitch| (pitch, KeyMode::Minor))
        })?;
    let pitch = pitch.trim();
    if pitch.is_empty() {
        return None;
    }
    Some(MusicalKey {
        pitch: pitch.to_owned(),
        mode,
        camelot: None,
    })
}

fn duration(length: i64) -> Duration {
    if length <= 0 {
        return Duration::ZERO;
    }
    // A length past a few hours is milliseconds, which some exports write instead of seconds.
    match length > 20_000 {
        true => Duration::from_millis(length as u64),
        false => Duration::from_secs(length as u64),
    }
}

fn located(folder_path: &str, relocated: &str) -> Option<PathBuf> {
    let primary = file_path(folder_path);
    if primary.as_ref().is_some_and(|path| path.is_file()) {
        return primary;
    }
    let moved = file_path(relocated);
    if moved.as_ref().is_some_and(|path| path.is_file()) {
        return moved;
    }
    primary.or(moved)
}

fn file_path(value: &str) -> Option<PathBuf> {
    let value = value.trim().trim_end_matches('\0');
    if value.is_empty() {
        return None;
    }
    let value = value
        .strip_prefix("file://")
        .map(percent_decode)
        .unwrap_or_else(|| value.to_owned());
    Some(PathBuf::from(value))
}

fn percent_decode(value: &str) -> String {
    percent_encoding::percent_decode_str(value)
        .decode_utf8_lossy()
        .into_owned()
}

/// Artwork paths in the database look absolute (`/PIONEER/Artwork/…`) but are rooted at the
/// `share` directory beside `master.db`. A path that really is a file is used as written.
fn artwork(root: &Path, image: &str) -> Option<String> {
    let image = image.trim().trim_end_matches('\0').replace('\\', "/");
    if image.is_empty() {
        return None;
    }
    let as_given = PathBuf::from(&image);
    if as_given.is_file() {
        return Some(located_file(&as_given));
    }
    let relative = Path::new(image.trim_start_matches('/'));
    if relative.as_os_str().is_empty() {
        return None;
    }
    [root.join("share").join(relative), root.join(relative)]
        .into_iter()
        .find(|candidate| candidate.is_file())
        .map(|candidate| located_file(&candidate))
}

fn located_file(path: &Path) -> String {
    format!("file://{}", path.display())
}

trait OptionalRow {
    fn optional(self) -> rusqlite::Result<Option<()>>;
}

impl OptionalRow for rusqlite::Result<()> {
    fn optional(self) -> rusqlite::Result<Option<()>> {
        match self {
            Ok(()) => Ok(Some(())),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn rooted_artwork_is_under_share() {
        let root =
            std::env::temp_dir().join(format!("sonora-rekordbox-art-{}", std::process::id()));
        let image = root.join("share/PIONEER/Artwork/9ed/example/artwork.jpg");
        fs::create_dir_all(image.parent().unwrap()).unwrap();
        fs::write(&image, b"jpg").unwrap();

        let found = artwork(&root, "/PIONEER/Artwork/9ed/example/artwork.jpg");
        let _ = fs::remove_dir_all(&root);

        assert_eq!(
            found.as_deref(),
            Some(format!("file://{}", image.display())).as_deref()
        );
    }
}

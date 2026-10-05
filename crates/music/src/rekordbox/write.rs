//! Writes a playlist change back to `master.db` and `masterPlaylists6.xml`.
//!
//! Rekordbox reads both. A row that exists only in the database, or a database that was edited
//! and then left encrypted with a different key, does not show up when rekordbox opens. The
//! file is replaced only after the edit is in a temporary copy, and the previous file is kept
//! beside it as `master.db.bak`.

use anyhow::{Context as _, Result, bail};
use rusqlite::OptionalExtension as _;
use rusqlite::{Connection, types::Value};
use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::cipher::{self, Seal};
use super::ids;
use super::read::{self, Catalog};

const SPECIAL: &[&str] = &["100000", "200000"];
const PLAYLIST: i64 = 0;
const FOLDER: i64 = 1;

pub enum Edit {
    Create(String),
    Rename { playlist: String, name: String },
    Delete(String),
    Add { playlist: String, track: String },
    Remove { playlist: String, track: String },
}

pub struct Saved {
    pub catalog: Catalog,
    pub playlist_id: String,
    pub seal: Seal,
}

/// Applies `edit` to the library at `database` and returns the catalog rekordbox will read.
pub fn commit(database: &Path, seal: &Seal, edit: Edit) -> Result<Saved> {
    recover(database);
    let bytes =
        std::fs::read(database).with_context(|| format!("cannot read {}", database.display()))?;
    let (plain, seal) = match seal.reveal(&bytes) {
        Some(plain) => (plain, seal.clone()),
        None => cipher::open(&bytes)?,
    };
    if plain.len() < 100 || !plain.len().is_multiple_of(seal.page_size()) {
        bail!("the rekordbox database is not a whole number of pages");
    }
    if plain[20] != u8::try_from(seal.reserve()).unwrap_or(255) {
        bail!("the rekordbox database is not in the form rekordbox writes");
    }
    if !tails_clear(&plain, seal.page_size(), seal.reserve()) {
        bail!("the rekordbox database uses its page tail, so it was not written");
    }

    let staged = tempfile::NamedTempFile::new().context("cannot stage the rekordbox database")?;
    std::fs::write(staged.path(), &plain).context("cannot stage the rekordbox database")?;
    let (stamp, millis) = stamp();
    let playlist_id = {
        let connection =
            Connection::open(staged.path()).context("cannot open the rekordbox database")?;
        connection
            .execute_batch("PRAGMA journal_mode = DELETE")
            .context("cannot open the rekordbox database")?;
        connection
            .execute_batch("BEGIN IMMEDIATE")
            .context("cannot change the rekordbox database")?;
        let playlist_id =
            apply(&connection, &edit, &stamp).context("cannot change the rekordbox database")?;
        connection
            .execute_batch("COMMIT")
            .context("cannot change the rekordbox database")?;
        playlist_id
    };
    let edited =
        std::fs::read(staged.path()).context("cannot read the changed rekordbox database")?;
    if !tails_clear(&edited, seal.page_size(), seal.reserve()) {
        bail!("the rekordbox database grew into its page tail, so it was not written");
    }
    let xml = xml_after(database, &edit, &playlist_id, millis)?;
    let locked = seal.lock(&edited)?;

    replace(database, &locked)
        .context("cannot write the rekordbox library; close rekordbox and try again")?;
    if let Some(xml) = xml
        && let Err(error) = replace(&xml_path(database), xml.as_bytes())
    {
        restore(database);
        return Err(error).context("cannot write masterPlaylists6.xml");
    }

    let catalog = read::catalog(&edited, database)?;
    Ok(Saved {
        catalog,
        playlist_id,
        seal,
    })
}

/// Puts `master.db` back if a previous edit stopped between replacing the file and finishing.
pub fn recover(database: &Path) {
    if database.is_file() {
        return;
    }
    let writing = suffixed(database, ".writing");
    if writing.is_file() {
        let _ = std::fs::rename(&writing, database);
        return;
    }
    let bak = suffixed(database, ".bak");
    if bak.is_file() {
        let _ = std::fs::rename(&bak, database);
    }
}

fn apply(connection: &Connection, edit: &Edit, stamp: &str) -> Result<String> {
    match edit {
        Edit::Create(name) => create(connection, name, stamp),
        Edit::Rename { playlist, name } => {
            let id = db_id(playlist)?;
            rename(connection, id, name, stamp)?;
            Ok(playlist.clone())
        }
        Edit::Delete(playlist) => {
            let id = db_id(playlist)?;
            delete(connection, id, stamp)?;
            Ok(playlist.clone())
        }
        Edit::Add { playlist, track } => {
            let id = db_id(playlist)?;
            add(connection, id, track, stamp)?;
            Ok(playlist.clone())
        }
        Edit::Remove { playlist, track } => {
            let id = db_id(playlist)?;
            remove(connection, id, track, stamp)?;
            Ok(playlist.clone())
        }
    }
}

fn create(connection: &Connection, name: &str, stamp: &str) -> Result<String> {
    let name = playlist_name(name)?;
    let id = unused_id(connection)?;
    let parent = root_parent(connection)?;
    let seq = next_seq(connection, &parent)?;
    let usn = bump(connection, stamp)?;
    let mut fields = vec![
        ("ID", Value::Text(id.to_string())),
        ("Seq", Value::Integer(seq)),
        ("Name", Value::Text(name)),
        ("Attribute", Value::Integer(PLAYLIST)),
        ("ParentID", Value::Text(parent)),
        ("UUID", Value::Text(uuid())),
        ("rb_local_usn", usn_value(usn)),
        ("created_at", Value::Text(stamp.to_owned())),
        ("updated_at", Value::Text(stamp.to_owned())),
    ];
    if usn.is_none() {
        fields.retain(|(name, _)| *name != "rb_local_usn");
    }
    insert(connection, "djmdPlaylist", &fields)?;
    Ok(ids::playlist_id(&id.to_string()))
}

fn rename(connection: &Connection, id: &str, name: &str, stamp: &str) -> Result<()> {
    let name = playlist_name(name)?;
    let (attribute, _, _) = playlist(connection, id)?;
    if attribute == FOLDER || SPECIAL.contains(&id) {
        bail!("that rekordbox playlist cannot be renamed");
    }
    let usn = bump(connection, stamp)?;
    update(
        connection,
        "djmdPlaylist",
        id,
        &[
            ("Name", Value::Text(name)),
            ("updated_at", Value::Text(stamp.to_owned())),
            ("rb_local_usn", usn_value(usn)),
        ],
    )
}

fn delete(connection: &Connection, id: &str, stamp: &str) -> Result<()> {
    let (attribute, seq, parent) = playlist(connection, id)?;
    if attribute == FOLDER || SPECIAL.contains(&id) {
        bail!("that rekordbox playlist cannot be deleted");
    }
    let _ = bump(connection, stamp)?;
    connection
        .execute("DELETE FROM djmdSongPlaylist WHERE PlaylistID = ?1", [id])
        .context("cannot delete the playlist tracks")?;
    let changed = connection
        .execute("DELETE FROM djmdPlaylist WHERE ID = ?1", [id])
        .context("cannot delete the playlist")?;
    if changed == 0 {
        bail!("cannot find rekordbox playlist {id}");
    }
    if column_set(connection, "djmdPlaylist")?.contains("Seq") {
        connection
            .execute(
                "UPDATE djmdPlaylist SET Seq = Seq - 1 WHERE ParentID = ?1 AND Seq > ?2",
                rusqlite::params![parent, seq],
            )
            .context("cannot close the gap left by the playlist")?;
    }
    Ok(())
}

fn add(connection: &Connection, playlist_id: &str, track_id: &str, stamp: &str) -> Result<()> {
    let (attribute, _, _) = playlist(connection, playlist_id)?;
    if attribute != PLAYLIST {
        bail!("rekordbox can only add tracks to a normal playlist");
    }
    let Some(content) = ids::content_id(track_id) else {
        bail!("only a rekordbox track can be added to a rekordbox playlist");
    };
    let found: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM djmdContent WHERE ID = ?1)",
            [content],
            |row| row.get(0),
        )
        .context("cannot look up the track")?;
    if !found {
        bail!("that track is not in the rekordbox library");
    }
    let track_no = next_track_no(connection, playlist_id)?;
    let usn = bump(connection, stamp)?;
    let song_id = uuid();
    insert(
        connection,
        "djmdSongPlaylist",
        &[
            ("ID", Value::Text(song_id)),
            ("PlaylistID", Value::Text(playlist_id.to_owned())),
            ("ContentID", Value::Text(content.to_owned())),
            ("TrackNo", Value::Integer(track_no)),
            ("UUID", Value::Text(uuid())),
            ("rb_local_usn", usn_value(usn)),
            ("created_at", Value::Text(stamp.to_owned())),
            ("updated_at", Value::Text(stamp.to_owned())),
        ],
    )?;
    touch_playlist(connection, playlist_id, stamp, usn)
}

fn remove(connection: &Connection, playlist_id: &str, track_id: &str, stamp: &str) -> Result<()> {
    let (attribute, _, _) = playlist(connection, playlist_id)?;
    if attribute != PLAYLIST {
        bail!("rekordbox fills that playlist itself");
    }
    let Some(content) = ids::content_id(track_id) else {
        bail!("that track is not in the rekordbox playlist");
    };
    let row: Option<(String, i64)> = connection
        .query_row(
            "SELECT ID, TrackNo FROM djmdSongPlaylist WHERE PlaylistID = ?1 AND ContentID = ?2 ORDER BY TrackNo LIMIT 1",
            [playlist_id, content],
            |row| Ok((row.get(0)?, integer(row, 1))),
        )
        .optional()
        .context("cannot look up the playlist track")?;
    let Some((song_id, track_no)) = row else {
        bail!("that track is not in the rekordbox playlist");
    };
    let usn = bump(connection, stamp)?;
    connection
        .execute("DELETE FROM djmdSongPlaylist WHERE ID = ?1", [song_id])
        .context("cannot remove the track")?;
    if column_set(connection, "djmdSongPlaylist")?.contains("TrackNo") {
        connection
            .execute(
                "UPDATE djmdSongPlaylist SET TrackNo = TrackNo - 1 WHERE PlaylistID = ?1 AND TrackNo > ?2",
                rusqlite::params![playlist_id, track_no],
            )
            .context("cannot close the gap left by the track")?;
    }
    touch_playlist(connection, playlist_id, stamp, usn)
}

fn touch_playlist(connection: &Connection, id: &str, stamp: &str, usn: Option<i64>) -> Result<()> {
    update(
        connection,
        "djmdPlaylist",
        id,
        &[
            ("updated_at", Value::Text(stamp.to_owned())),
            ("rb_local_usn", usn_value(usn)),
        ],
    )
}

fn playlist(connection: &Connection, id: &str) -> Result<(i64, i64, String)> {
    connection
        .query_row(
            "SELECT Attribute, Seq, ParentID FROM djmdPlaylist WHERE ID = ?1",
            [id],
            |row| Ok((integer(row, 0), integer(row, 1), text(row, 2))),
        )
        .optional()
        .context("cannot look up the playlist")?
        .ok_or_else(|| anyhow::anyhow!("cannot find rekordbox playlist {id}"))
}

fn unused_id(connection: &Connection) -> Result<i64> {
    for _ in 0..10_000 {
        let id = i64::from(fastrand::u32(100..u32::MAX));
        let used: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM djmdPlaylist WHERE ID = ?1)",
                [id.to_string()],
                |row| row.get(0),
            )
            .unwrap_or(true);
        if !used && !SPECIAL.contains(&id.to_string().as_str()) {
            return Ok(id);
        }
    }
    bail!("cannot find an unused rekordbox playlist id")
}

fn root_parent(connection: &Connection) -> Result<String> {
    let mut query = connection
        .prepare("SELECT DISTINCT ParentID FROM djmdPlaylist")
        .context("cannot read rekordbox playlists")?;
    let rows = query
        .query_map([], |row| row.get::<_, Option<String>>(0))
        .context("cannot read rekordbox playlists")?;
    let mut saw_blank = false;
    for row in rows {
        let parent = row
            .context("cannot read a rekordbox playlist")?
            .unwrap_or_default();
        if parent == "root" {
            return Ok(parent);
        }
        if parent.is_empty() || parent == "0" {
            saw_blank = true;
        }
    }
    match saw_blank {
        true => Ok(String::new()),
        false => Ok("root".to_owned()),
    }
}

fn next_seq(connection: &Connection, parent: &str) -> Result<i64> {
    let seq: Option<i64> = connection
        .query_row(
            "SELECT MAX(Seq) FROM djmdPlaylist WHERE ParentID = ?1",
            [parent],
            |row| row.get(0),
        )
        .context("cannot read the playlist order")?;
    Ok(seq.unwrap_or(0) + 1)
}

fn next_track_no(connection: &Connection, playlist_id: &str) -> Result<i64> {
    let track_no: Option<i64> = connection
        .query_row(
            "SELECT MAX(TrackNo) FROM djmdSongPlaylist WHERE PlaylistID = ?1",
            [playlist_id],
            |row| row.get(0),
        )
        .context("cannot read the playlist")?;
    Ok(track_no.unwrap_or(0) + 1)
}

fn bump(connection: &Connection, stamp: &str) -> Result<Option<i64>> {
    let tables: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'agentRegistry')",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if !tables {
        return Ok(None);
    }
    let changed = connection
        .execute(
            "UPDATE agentRegistry SET int_1 = COALESCE(int_1, 0) + 1, updated_at = ?1 WHERE registry_id = 'localUpdateCount'",
            [stamp],
        )
        .context("cannot advance the rekordbox update count")?;
    if changed == 0 {
        return Ok(None);
    }
    connection
        .query_row(
            "SELECT int_1 FROM agentRegistry WHERE registry_id = 'localUpdateCount'",
            [],
            |row| row.get(0),
        )
        .optional()
        .context("cannot read the rekordbox update count")
}

fn insert(connection: &Connection, table: &str, fields: &[(&str, Value)]) -> Result<()> {
    let have = column_set(connection, table)?;
    let fields: Vec<_> = fields
        .iter()
        .filter(|(name, _)| have.contains(*name))
        .cloned()
        .collect();
    if fields.is_empty() {
        bail!("cannot write a rekordbox {table} row");
    }
    let names = fields
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ");
    let marks = (1..=fields.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let values = fields
        .into_iter()
        .map(|(_, value)| value)
        .collect::<Vec<_>>();
    connection
        .execute(
            &format!("INSERT INTO {table} ({names}) VALUES ({marks})"),
            rusqlite::params_from_iter(values),
        )
        .with_context(|| format!("cannot write a rekordbox {table} row"))?;
    Ok(())
}

fn update(connection: &Connection, table: &str, id: &str, fields: &[(&str, Value)]) -> Result<()> {
    let have = column_set(connection, table)?;
    let fields: Vec<_> = fields
        .iter()
        .filter(|(name, _)| have.contains(*name))
        .cloned()
        .collect();
    if fields.is_empty() {
        return Ok(());
    }
    let sets = fields
        .iter()
        .enumerate()
        .map(|(index, (name, _))| format!("{name} = ?{}", index + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let mut values = fields
        .into_iter()
        .map(|(_, value)| value)
        .collect::<Vec<_>>();
    values.push(Value::Text(id.to_owned()));
    connection
        .execute(
            &format!("UPDATE {table} SET {sets} WHERE ID = ?{}", values.len()),
            rusqlite::params_from_iter(values),
        )
        .with_context(|| format!("cannot update a rekordbox {table} row"))?;
    Ok(())
}

fn column_set(connection: &Connection, table: &str) -> Result<HashSet<String>> {
    let mut query = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .with_context(|| format!("cannot read {table}"))?;
    let rows = query
        .query_map([], |row| row.get::<_, String>(1))
        .with_context(|| format!("cannot read {table}"))?;
    rows.collect::<rusqlite::Result<_>>()
        .with_context(|| format!("cannot read {table}"))
}

fn usn_value(usn: Option<i64>) -> Value {
    match usn {
        Some(usn) => Value::Integer(usn),
        None => Value::Null,
    }
}

fn playlist_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("a rekordbox playlist needs a name");
    }
    if name.chars().count() > 255 {
        bail!("a rekordbox playlist name is too long");
    }
    Ok(name.to_owned())
}

fn db_id(playlist_id: &str) -> Result<&str> {
    playlist_id
        .strip_prefix(ids::PLAYLIST_PREFIX)
        .context("that is not a rekordbox playlist")
}

fn integer(row: &rusqlite::Row<'_>, index: usize) -> i64 {
    match row.get_ref(index) {
        Ok(rusqlite::types::ValueRef::Integer(value)) => value,
        Ok(rusqlite::types::ValueRef::Text(bytes)) => {
            String::from_utf8_lossy(bytes).trim().parse().unwrap_or(0)
        }
        _ => 0,
    }
}

fn text(row: &rusqlite::Row<'_>, index: usize) -> String {
    match row.get_ref(index) {
        Ok(rusqlite::types::ValueRef::Text(bytes)) => {
            String::from_utf8_lossy(bytes).trim().to_owned()
        }
        Ok(rusqlite::types::ValueRef::Integer(value)) => value.to_string(),
        _ => String::new(),
    }
}

fn uuid() -> String {
    let mut bytes = [0u8; 16];
    fastrand::fill(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    )
}

fn stamp() -> (String, i64) {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(0))
        .unwrap_or(0);
    let zoned = jiff::Timestamp::from_millisecond(millis)
        .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
        .to_zoned(jiff::tz::TimeZone::UTC);
    let text = format!(
        "{stamp} +00:00",
        stamp = zoned.strftime("%Y-%m-%d %H:%M:%S%.3f")
    );
    (text, millis)
}

fn tails_clear(plain: &[u8], page_size: usize, reserve: usize) -> bool {
    plain.chunks(page_size).all(|page| {
        page.len() == page_size && page[page_size - reserve..].iter().all(|byte| *byte == 0)
    })
}

fn xml_path(database: &Path) -> PathBuf {
    database.with_file_name("masterPlaylists6.xml")
}

fn xml_after(
    database: &Path,
    edit: &Edit,
    playlist_id: &str,
    millis: i64,
) -> Result<Option<String>> {
    let path = xml_path(database);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            log::warn!(
                "rekordbox: no masterPlaylists6.xml beside {}",
                database.display()
            );
            return Ok(None);
        }
        Err(error) => return Err(error).context("cannot read masterPlaylists6.xml"),
    };
    let id = db_id(playlist_id)?;
    let next = match edit {
        Edit::Create(_) => xml_add(&text, id, millis)?,
        Edit::Delete(_) => xml_remove(&text, id),
        Edit::Rename { .. } | Edit::Add { .. } | Edit::Remove { .. } => {
            xml_touch(&text, id, millis)
        }
    };
    Ok(Some(next))
}

fn xml_add(text: &str, id: &str, millis: i64) -> Result<String> {
    let Some(end) = text.rfind("</PLAYLISTS>") else {
        bail!("masterPlaylists6.xml has no playlist list");
    };
    let hex = hex_id(id);
    if node_at(text, &hex).is_some() {
        bail!("playlist {id} is already in masterPlaylists6.xml");
    }
    let line = text[..end].rfind('\n').map(|index| index + 1).unwrap_or(0);
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let node = format!(
        "    <NODE Id=\"{hex}\" ParentId=\"0\" Attribute=\"0\" Timestamp=\"{millis}\" Lib_Type=\"0\" CheckType=\"0\"/>{newline}"
    );
    let mut next = text.to_owned();
    next.insert_str(line, &node);
    Ok(next)
}

fn xml_remove(text: &str, id: &str) -> String {
    let Some((start, end)) = node_at(text, &hex_id(id)) else {
        log::warn!("rekordbox: playlist {id} is not in masterPlaylists6.xml");
        return text.to_owned();
    };
    let mut next = text.to_owned();
    next.replace_range(start..end, "");
    next
}

fn xml_touch(text: &str, id: &str, millis: i64) -> String {
    let hex = hex_id(id);
    let Some((start, end)) = node_at(text, &hex) else {
        log::warn!("rekordbox: playlist {id} is not in masterPlaylists6.xml");
        return text.to_owned();
    };
    let node = &text[start..end];
    let Some(at) = node.find("Timestamp=\"") else {
        return text.to_owned();
    };
    let value = at + "Timestamp=\"".len();
    let Some(close) = node[value..].find('"') else {
        return text.to_owned();
    };
    let mut next = text.to_owned();
    next.replace_range(start + value..start + value + close, &millis.to_string());
    next
}

fn node_at(text: &str, hex: &str) -> Option<(usize, usize)> {
    let needle = format!("Id=\"{hex}\"");
    let mut from = 0;
    while let Some(found) = text[from..].find(&needle) {
        let id_at = from + found;
        let mut start = text[..id_at].rfind("<NODE")?;
        if let Some(line) = text[..start].rfind('\n')
            && text[line + 1..start].trim().is_empty()
        {
            start = line + 1;
        }
        let close = text[id_at..].find("/>")? + id_at + 2;
        let end = match text[close..].starts_with("\r\n") {
            true => close + 2,
            false if text[close..].starts_with('\n') => close + 1,
            false => close,
        };
        if text[start..id_at].contains("ParentId") {
            from = id_at + needle.len();
            continue;
        }
        return Some((start, end));
    }
    None
}

fn hex_id(id: &str) -> String {
    match id.parse::<u64>() {
        Ok(id) => format!("{id:X}"),
        Err(_) => id.to_owned(),
    }
}

fn replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let writing = suffixed(path, ".writing");
    {
        let mut file = std::fs::File::create(&writing)
            .with_context(|| format!("cannot write {}", path.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("cannot write {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    let bak = suffixed(path, ".bak");
    let _ = std::fs::remove_file(&bak);
    if path.exists() {
        std::fs::rename(path, &bak)
            .with_context(|| format!("cannot replace {}", path.display()))?;
    }
    if let Err(error) = std::fs::rename(&writing, path) {
        let _ = std::fs::rename(&bak, path);
        return Err(error).with_context(|| format!("cannot replace {}", path.display()));
    }
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn restore(path: &Path) {
    let bak = suffixed(path, ".bak");
    if bak.is_file() {
        let _ = std::fs::rename(&bak, path);
    }
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{name}{suffix}"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn playlist_edits_roundtrip_into_the_database_and_the_xml() {
        let (plain, _) = cipher::open(include_bytes!("fixture.db")).expect("unlock");
        let dir = tempfile::tempdir().expect("temp");
        let database = dir.path().join("master.db");
        std::fs::write(&database, &plain).expect("write");
        std::fs::write(
            dir.path().join("masterPlaylists6.xml"),
            "<MASTER_PLAYLIST Version=\"3.0.0\" AutomaticSync=\"0\">\n  <PLAYLISTS>\n  </PLAYLISTS>\n</MASTER_PLAYLIST>\n",
        )
        .expect("xml");
        let (_, seal) = cipher::open(&plain).expect("seal");

        let created = commit(&database, &seal, Edit::Create("Dawn".into())).expect("create");
        assert!(created.playlist_id.starts_with(ids::PLAYLIST_PREFIX));
        assert_eq!(
            created
                .catalog
                .playlists
                .iter()
                .find(|playlist| playlist.id == created.playlist_id)
                .map(|playlist| playlist.name.as_str()),
            Some("Dawn")
        );
        let xml = std::fs::read_to_string(dir.path().join("masterPlaylists6.xml")).expect("xml");
        let db_id = created
            .playlist_id
            .trim_start_matches(ids::PLAYLIST_PREFIX)
            .parse::<u64>()
            .unwrap();
        assert!(xml.contains(&format!("Id=\"{db_id:X}\"")));

        let track = ids::track_id("42", Some(Path::new("/tmp/she.flac")));
        let added = commit(
            &database,
            &created.seal,
            Edit::Add {
                playlist: created.playlist_id.clone(),
                track: track.clone(),
            },
        )
        .expect("add");
        let playlist = added
            .catalog
            .playlists
            .iter()
            .find(|playlist| playlist.id == created.playlist_id)
            .expect("playlist");
        assert_eq!(playlist.tracks.len(), 1);
        assert_eq!(playlist.tracks[0].name, "She Moves She");

        let removed = commit(
            &database,
            &added.seal,
            Edit::Remove {
                playlist: created.playlist_id.clone(),
                track,
            },
        )
        .expect("remove");
        assert_eq!(
            removed
                .catalog
                .playlists
                .iter()
                .find(|playlist| playlist.id == created.playlist_id)
                .map(|playlist| playlist.tracks.len()),
            Some(0)
        );

        let renamed = commit(
            &database,
            &removed.seal,
            Edit::Rename {
                playlist: created.playlist_id.clone(),
                name: "Morning".into(),
            },
        )
        .expect("rename");
        assert_eq!(
            renamed
                .catalog
                .playlists
                .iter()
                .find(|playlist| playlist.id == created.playlist_id)
                .map(|playlist| playlist.name.as_str()),
            Some("Morning")
        );

        let deleted = commit(
            &database,
            &renamed.seal,
            Edit::Delete(created.playlist_id.clone()),
        )
        .expect("delete");
        assert!(
            deleted
                .catalog
                .playlists
                .iter()
                .all(|playlist| playlist.id != created.playlist_id)
        );
        let xml = std::fs::read_to_string(dir.path().join("masterPlaylists6.xml")).expect("xml");
        assert!(!xml.contains(&format!("Id=\"{db_id:X}\"")));
        assert!(xml.contains("  </PLAYLISTS>"));
        let locked = std::fs::read(&database).expect("locked");
        assert!(cipher::open(&locked).is_ok());
    }
}

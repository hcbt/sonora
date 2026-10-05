//! A rekordbox collection, read from `master.db` and played from the files it points at.
//!
//! This is a library on disk, like local files, not an account. The folder is the one rekordbox
//! calls its master database directory. Playlists come from that database. Nothing here writes
//! back to it.

mod cipher;
mod client;
mod ids;
mod playback;
mod read;
mod stars;

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use storage::Database;

use crate::{MusicApi, PlaybackFactory};

pub use ids::is_rekordbox_id;

/// A library that opened: the catalog the shelves read, and the player that opens its files.
pub struct Opened {
    pub api: Arc<dyn MusicApi>,
    pub playback: Arc<dyn PlaybackFactory>,
}

/// Opens the rekordbox library at `folder`. `folder` may be the directory that holds
/// `master.db`, or the database file itself.
pub fn open(folder: &Path) -> Result<Opened> {
    let database_path = master_db(folder)?;
    let bytes = std::fs::read(&database_path)
        .with_context(|| format!("cannot read {}", database_path.display()))?;
    let plain = cipher::unlock(&bytes)?;
    let catalog = read::catalog(&plain, &database_path)?;
    let api: Arc<dyn MusicApi> = Arc::new(client::Client::new(catalog, Database::standard()));
    Ok(Opened {
        api,
        playback: playback::factory(),
    })
}

fn master_db(folder: &Path) -> Result<std::path::PathBuf> {
    if folder.is_file() {
        return Ok(folder.to_path_buf());
    }
    let direct = folder.join("master.db");
    if direct.is_file() {
        return Ok(direct);
    }
    bail!(
        "{} does not contain a rekordbox master.db",
        folder.display()
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn reads_an_encrypted_collection() {
        let plain = cipher::unlock(include_bytes!("fixture.db")).expect("unlock");
        let catalog = read::catalog(&plain, Path::new("/tmp")).expect("catalog");
        assert_eq!(catalog.tracks.len(), 1);
        assert_eq!(catalog.tracks[0].name, "She Moves She");
        assert_eq!(catalog.tracks[0].artists, "Four Tet");
        assert_eq!(catalog.tracks[0].album, "Rounds");
        assert_eq!(
            catalog.tracks[0]
                .rhythm
                .as_ref()
                .and_then(|rhythm| rhythm.bpm),
            Some(128)
        );
        assert_eq!(catalog.albums.len(), 1);
        assert_eq!(catalog.albums[0].year, 2003);
        assert_eq!(catalog.albums[0].artists, "Four Tet");
        assert_eq!(catalog.playlists.len(), 1);
        assert_eq!(catalog.playlists[0].name, "Sets / Night");
        assert_eq!(catalog.playlists[0].tracks.len(), 1);
    }

}

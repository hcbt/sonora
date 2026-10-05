//! Stars for a rekordbox library. They live in Sonora's own database, keyed by the rekordbox id,
//! and never get written back into `master.db`.

use anyhow::{Context as _, Result};
use rusqlite::params;
use storage::Database;

use super::ids::{ALBUM_PREFIX, ARTIST_PREFIX, TRACK_PREFIX};

#[derive(Clone, Copy)]
pub enum Kind {
    Track,
    Album,
    Artist,
}

impl Kind {
    fn table(self) -> &'static str {
        match self {
            Self::Track => "favorites",
            Self::Album => "favorite_albums",
            Self::Artist => "favorite_artists",
        }
    }

    fn column(self) -> &'static str {
        match self {
            Self::Track => "track_id",
            Self::Album => "album_id",
            Self::Artist => "artist_id",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Track => TRACK_PREFIX,
            Self::Album => ALBUM_PREFIX,
            Self::Artist => ARTIST_PREFIX,
        }
    }
}

pub struct Stars {
    database: Database,
}

impl Stars {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    pub fn starred(&self, kind: Kind) -> Result<Vec<(String, i64)>> {
        let connection = self
            .database
            .open()
            .context("cannot open rekordbox stars")?;
        let mut query = connection
            .prepare(&format!(
                "SELECT {}, added_at FROM {} WHERE {} LIKE ? ORDER BY added_at DESC",
                kind.column(),
                kind.table(),
                kind.column(),
            ))
            .context("cannot read rekordbox stars")?;
        let rows = query
            .query_map(params![format!("{}%", kind.prefix())], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .context("cannot read rekordbox stars")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("cannot read rekordbox stars")
    }

    pub fn set(&self, kind: Kind, id: &str, saved: bool) -> Result<()> {
        let connection = self
            .database
            .open()
            .context("cannot open rekordbox stars")?;
        match saved {
            true => {
                connection
                    .execute(
                        &format!(
                            "INSERT OR REPLACE INTO {} ({}, added_at) VALUES (?, ?)",
                            kind.table(),
                            kind.column(),
                        ),
                        params![id, now()],
                    )
                    .context("cannot star a rekordbox item")?;
            }
            false => {
                connection
                    .execute(
                        &format!("DELETE FROM {} WHERE {} = ?", kind.table(), kind.column()),
                        params![id],
                    )
                    .context("cannot unstar a rekordbox item")?;
            }
        }
        Ok(())
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}

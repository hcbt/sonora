//! Playback for a rekordbox track. The id names a file on disk, so this is the same decoder the
//! local library uses, pointed at the path rekordbox recorded.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use async_trait::async_trait;
use rodio::Source as _;

use crate::engine::{self, Fetch, Loudness};
use crate::local::{decode_path, loudness};
use crate::{PlaybackConfig, PlaybackEvents, PlaybackFactory, Player};

use super::ids;

#[derive(Clone)]
struct Loaded {
    length: Option<Duration>,
    loudness: Option<Loudness>,
}

#[derive(Default)]
pub struct Factory;

impl PlaybackFactory for Factory {
    fn start(&self, config: PlaybackConfig) -> (Box<dyn Player>, Box<dyn PlaybackEvents>) {
        engine::start(Files, config)
    }
}

pub fn factory() -> Arc<dyn PlaybackFactory> {
    Arc::new(Factory)
}

struct Files;

#[async_trait]
impl Fetch for Files {
    type Loaded = Loaded;
    type Source = rodio::Decoder<std::io::BufReader<crate::local::Audio>>;

    fn name(&self) -> &'static str {
        "rekordbox"
    }

    async fn load(&self, id: &str) -> Result<Loaded> {
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            let path = ids::path_from_track_id(&id)
                .ok_or_else(|| anyhow!("{id} is not a rekordbox track"))?
                .to_path_buf();
            let length = decode_path(&path)
                .with_context(|| format!("cannot decode {}", path.display()))?
                .total_duration();
            Ok(Loaded {
                length,
                loudness: loudness(&path),
            })
        })
        .await
        .context("cannot probe the rekordbox file")?
    }

    fn length(&self, loaded: &Loaded) -> Option<Duration> {
        loaded.length
    }

    fn loudness(&self, loaded: &Loaded) -> Option<Loudness> {
        loaded.loudness
    }

    fn open(&self, id: &str, _loaded: &Loaded, at: Duration) -> Option<Self::Source> {
        let path = ids::path_from_track_id(id)?;
        let mut decoder = match decode_path(path) {
            Ok(decoder) => decoder,
            Err(error) => {
                log::warn!("playback: cannot decode the rekordbox track {id}: {error:#}");
                return None;
            }
        };
        if !at.is_zero()
            && let Err(error) = decoder.try_seek(at)
        {
            log::warn!("playback: cannot start {id} at {}s: {error}", at.as_secs());
        }
        Some(decoder)
    }
}

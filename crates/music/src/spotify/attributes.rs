use std::collections::HashMap;

use anyhow::{Context as _, Result};
use librespot_core::Session;
use librespot_protocol::extended_metadata::{BatchedEntityRequest, EntityRequest, ExtensionQuery};
use protobuf::EnumOrUnknown;

use crate::{KeyMode, MusicalKey, Track, TrackRhythm};

/// `AUDIO_ATTRIBUTES_V2` in the desktop client. librespot's `ExtensionKind` stops short of it.
const AUDIO_ATTRIBUTES_V2: i32 = 222;
const TRACK_PREFIX: &str = "spotify:track:";
/// A tempo outside this range is an empty or broken analysis, not a song.
const MAX_BPM: f64 = 999.;

/// Tempo and key from the same extended metadata the desktop client reads for its BPM and
/// key rows. A track with no analysis answers empty rather than failing the song page.
pub async fn rhythm(session: &Session, track_id: &str) -> Result<TrackRhythm> {
    let request = BatchedEntityRequest {
        entity_request: vec![EntityRequest {
            entity_uri: format!("{TRACK_PREFIX}{track_id}"),
            query: vec![ExtensionQuery {
                extension_kind: EnumOrUnknown::from_i32(AUDIO_ATTRIBUTES_V2),
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    let response = session
        .spclient()
        .get_extended_metadata(request)
        .await
        .context("cannot read tempo and key")?;
    let Some(entity) = response
        .extended_metadata
        .into_iter()
        .flat_map(|array| array.extension_data)
        .next()
    else {
        return Ok(TrackRhythm::default());
    };
    let status = entity.header.status_code;
    if status != 0 && status != 200 {
        return Ok(TrackRhythm::default());
    }
    Ok(decode(&entity.extension_data.value))
}

/// Fills tempo and key on tracks already loaded, keyed by Spotify URI. A failure leaves them
/// empty rather than failing the list they belong to.
pub async fn fill(session: &Session, tracks: &mut HashMap<String, Track>) {
    let uris: Vec<String> = tracks.keys().cloned().collect();
    let found = match rhythms(session, &uris).await {
        Ok(found) => found,
        Err(error) => {
            log::warn!("spotify: cannot read tempo and key: {error:#}");
            return;
        }
    };
    for (uri, track) in tracks.iter_mut() {
        if let Some(rhythm) = found.get(uri) {
            track.rhythm = rhythm.clone().packed();
        }
    }
}

/// The same fill for a list keyed by track id rather than URI.
pub async fn apply(session: &Session, tracks: &mut [Track]) {
    let uris: Vec<String> = tracks
        .iter()
        .filter_map(|track| track.id.as_ref().map(|id| format!("{TRACK_PREFIX}{id}")))
        .collect();
    let found = match rhythms(session, &uris).await {
        Ok(found) => found,
        Err(error) => {
            log::warn!("spotify: cannot read tempo and key: {error:#}");
            return;
        }
    };
    for track in tracks {
        let Some(id) = track.id.as_deref() else {
            continue;
        };
        if let Some(rhythm) = found.get(&format!("{TRACK_PREFIX}{id}")) {
            track.rhythm = rhythm.clone().packed();
        }
    }
}

async fn rhythms(session: &Session, uris: &[String]) -> Result<HashMap<String, TrackRhythm>> {
    let mut found = HashMap::new();
    if uris.is_empty() {
        return Ok(found);
    }
    for chunk in uris.chunks(500) {
        let request = BatchedEntityRequest {
            entity_request: chunk
                .iter()
                .map(|uri| EntityRequest {
                    entity_uri: uri.clone(),
                    query: vec![ExtensionQuery {
                        extension_kind: EnumOrUnknown::from_i32(AUDIO_ATTRIBUTES_V2),
                        ..Default::default()
                    }],
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let response = session
            .spclient()
            .get_extended_metadata(request)
            .await
            .context("cannot read tempo and key")?;
        for entity in response
            .extended_metadata
            .into_iter()
            .flat_map(|array| array.extension_data)
        {
            let status = entity.header.status_code;
            if status != 0 && status != 200 {
                continue;
            }
            let rhythm = decode(&entity.extension_data.value);
            if rhythm.bpm.is_some() || rhythm.key.is_some() {
                found.insert(entity.entity_uri, rhythm);
            }
        }
    }
    Ok(found)
}

pub(crate) fn decode(bytes: &[u8]) -> TrackRhythm {
    let mut bpm = None;
    let mut key = None;
    let mut cursor = 0;
    while cursor < bytes.len() {
        let Some((tag, next)) = varint(bytes, cursor) else {
            break;
        };
        cursor = next;
        if tag == 0 {
            break;
        }
        match (tag >> 3, tag & 7) {
            (1, 1) => {
                let Some(value) = fixed64(bytes, &mut cursor) else {
                    break;
                };
                bpm = rounded(f64::from_le_bytes(value));
            }
            (2, 2) => {
                let Some(body) = length(bytes, &mut cursor) else {
                    break;
                };
                key = musical_key(body);
            }
            (_, wire) => {
                if !skip(bytes, &mut cursor, wire) {
                    break;
                }
            }
        }
    }
    TrackRhythm {
        bpm,
        key,
        written: None,
    }
}

fn musical_key(bytes: &[u8]) -> Option<MusicalKey> {
    let mut pitch = String::new();
    let mut mode = 0i32;
    let mut camelot = None;
    let mut cursor = 0;
    while cursor < bytes.len() {
        let Some((tag, next)) = varint(bytes, cursor) else {
            break;
        };
        cursor = next;
        if tag == 0 {
            break;
        }
        match (tag >> 3, tag & 7) {
            (1, 2) => {
                let Some(value) = text(bytes, &mut cursor) else {
                    break;
                };
                pitch = value;
            }
            (2, 0) => {
                let Some((value, next)) = varint(bytes, cursor) else {
                    break;
                };
                cursor = next;
                mode = value as i32;
            }
            (3, 2) => {
                let Some(body) = length(bytes, &mut cursor) else {
                    break;
                };
                camelot = camelot_code(body);
            }
            (_, wire) => {
                if !skip(bytes, &mut cursor, wire) {
                    break;
                }
            }
        }
    }
    let pitch = pitch.trim().to_owned();
    let camelot = camelot.filter(|code| !code.is_empty());
    if pitch.is_empty() && camelot.is_none() {
        return None;
    }
    Some(MusicalKey {
        pitch,
        mode: match mode {
            1 => KeyMode::Minor,
            2 => KeyMode::Major,
            _ => KeyMode::Unknown,
        },
        camelot,
    })
}

fn camelot_code(bytes: &[u8]) -> Option<String> {
    let mut value = None;
    let mut cursor = 0;
    while cursor < bytes.len() {
        let Some((tag, next)) = varint(bytes, cursor) else {
            break;
        };
        cursor = next;
        if tag == 0 {
            break;
        }
        match (tag >> 3, tag & 7) {
            (1, 2) => {
                let Some(read) = text(bytes, &mut cursor) else {
                    break;
                };
                value = Some(read);
            }
            (_, wire) => {
                if !skip(bytes, &mut cursor, wire) {
                    break;
                }
            }
        }
    }
    value
}

fn rounded(value: f64) -> Option<u32> {
    if !value.is_finite() || value <= 0. || value > MAX_BPM {
        return None;
    }
    Some(value.round() as u32)
}

fn varint(bytes: &[u8], mut cursor: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0;
    while cursor < bytes.len() && shift < 64 {
        let byte = bytes[cursor];
        cursor += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some((value, cursor));
        }
        shift += 7;
    }
    None
}

fn fixed64(bytes: &[u8], cursor: &mut usize) -> Option<[u8; 8]> {
    let next = cursor.checked_add(8)?;
    let value = bytes.get(*cursor..next)?.try_into().ok()?;
    *cursor = next;
    Some(value)
}

fn length<'a>(bytes: &'a [u8], cursor: &mut usize) -> Option<&'a [u8]> {
    let (len, next) = varint(bytes, *cursor)?;
    let len = usize::try_from(len).ok()?;
    let end = next.checked_add(len)?;
    let body = bytes.get(next..end)?;
    *cursor = end;
    Some(body)
}

fn text(bytes: &[u8], cursor: &mut usize) -> Option<String> {
    let body = length(bytes, cursor)?;
    String::from_utf8(body.to_vec()).ok()
}

fn skip(bytes: &[u8], cursor: &mut usize, wire: u64) -> bool {
    match wire {
        0 => varint(bytes, *cursor).is_some_and(|(_, next)| {
            *cursor = next;
            true
        }),
        1 => fixed64(bytes, cursor).is_some(),
        2 => length(bytes, cursor).is_some(),
        5 => {
            let Some(next) = cursor.checked_add(4) else {
                return false;
            };
            if next > bytes.len() {
                return false;
            }
            *cursor = next;
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint_bytes(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
        out
    }

    fn delimited(field: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![(field << 3) | 2];
        out.extend(varint_bytes(body.len() as u64));
        out.extend_from_slice(body);
        out
    }

    fn analysed(bpm: f64, pitch: &str, mode: i32, camelot: &str) -> Vec<u8> {
        let mut key = delimited(1, pitch.as_bytes());
        key.push(16);
        key.extend(varint_bytes(mode as u64));
        let mut camelot_body = delimited(1, camelot.as_bytes());
        camelot_body.extend(delimited(2, b"#ff00aa"));
        key.extend(delimited(3, &camelot_body));

        let mut out = vec![9];
        out.extend_from_slice(&bpm.to_le_bytes());
        out.extend(delimited(2, &key));
        // An unknown field must be skipped, not end the message.
        out.extend(delimited(9, b"ignored"));
        out
    }

    #[test]
    fn reads_bpm_pitch_mode_and_camelot() {
        let rhythm = decode(&analysed(127.6, "F#", 1, "4A"));
        assert_eq!(rhythm.bpm, Some(128));
        let key = rhythm.key.unwrap();
        assert_eq!(key.pitch, "F#");
        assert_eq!(key.mode, KeyMode::Minor);
        assert_eq!(key.camelot.as_deref(), Some("4A"));
    }

    #[test]
    fn hides_a_zero_tempo_and_keeps_a_camelot_only_key() {
        let mut camelot = delimited(1, b"8A");
        camelot.extend(delimited(2, b"red"));
        let bytes = delimited(2, &delimited(3, &camelot));
        let rhythm = decode(&bytes);
        assert_eq!(rhythm.bpm, None);
        let key = rhythm.key.unwrap();
        assert!(key.pitch.is_empty());
        assert_eq!(key.mode, KeyMode::Unknown);
        assert_eq!(key.camelot.as_deref(), Some("8A"));
    }
}

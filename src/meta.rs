//! Release lookup on Discogs and MusicBrainz.

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::io::Read;
use std::time::Duration;

const UA: &str = "JPVinylRec/0.1 (+https://github.com/jpvaneerde)";

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Discogs(u64),
    MusicBrainz(String),
}

#[derive(Clone, Debug)]
pub struct Track {
    /// As printed on the release: "A1", "B3", "1"...
    pub position: String,
    /// Side/medium group used to pick tracks for one recording ("A", "B", "Disc 1"...).
    pub side: String,
    /// Sequential number across the whole release (1-based), used as TRACKNUMBER.
    pub number: usize,
    pub title: String,
    /// Track artist when it differs from the album artist.
    pub artist: Option<String>,
    /// Seconds; None when the database has no length.
    pub duration: Option<f64>,
    pub mb_recording_id: Option<String>,
    pub mb_track_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Release {
    pub source: Source,
    pub title: String,
    pub artist: String,
    /// Original release year (Discogs master / MusicBrainz release group) when known.
    pub original_year: Option<String>,
    /// Year of this pressing.
    pub release_year: Option<String>,
    pub label: Option<String>,
    pub catno: Option<String>,
    pub country: Option<String>,
    pub tracks: Vec<Track>,
    pub cover_url: Option<String>,
    pub mb_release_group: Option<String>,
}

impl Release {
    pub fn sides(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for t in &self.tracks {
            if !v.contains(&t.side) {
                v.push(t.side.clone());
            }
        }
        v
    }
    pub fn tracks_on(&self, side: &str) -> Vec<&Track> {
        self.tracks.iter().filter(|t| t.side == side).collect()
    }
    pub fn year(&self) -> Option<&str> {
        self.original_year.as_deref().or(self.release_year.as_deref())
    }
    pub fn url(&self) -> String {
        match &self.source {
            Source::Discogs(id) => format!("https://www.discogs.com/release/{id}"),
            Source::MusicBrainz(id) => format!("https://musicbrainz.org/release/{id}"),
        }
    }
}

/// Accepts a Discogs release URL/ID ("https://www.discogs.com/release/123-Foo", "r123", "123")
/// or a MusicBrainz release URL/MBID.
pub fn parse_source(input: &str) -> Result<Source> {
    let s = input.trim();
    let is_uuid = |x: &str| {
        x.len() == 36 && x.chars().filter(|c| *c == '-').count() == 4 && x.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
    };
    if s.contains("musicbrainz.org") {
        if let Some(i) = s.find("/release/") {
            let id: String = s[i + 9..].chars().take(36).collect();
            if is_uuid(&id) {
                return Ok(Source::MusicBrainz(id.to_lowercase()));
            }
        }
        return Err(anyhow!("need a MusicBrainz *release* URL (…/release/<mbid>)"));
    }
    if is_uuid(s) {
        return Ok(Source::MusicBrainz(s.to_lowercase()));
    }
    if s.contains("discogs.com") {
        if s.contains("/master/") {
            return Err(anyhow!("that's a Discogs master; open the specific release (pressing) you own and paste its URL"));
        }
        if let Some(i) = s.find("/release/") {
            let id: String = s[i + 9..].chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = id.parse() {
                return Ok(Source::Discogs(n));
            }
        }
        return Err(anyhow!("could not find a release id in that Discogs URL"));
    }
    let digits = s.trim_start_matches(['r', 'R', '[']).trim_end_matches(']');
    if let Ok(n) = digits.parse::<u64>() {
        return Ok(Source::Discogs(n));
    }
    Err(anyhow!("paste a Discogs release URL/ID or a MusicBrainz release URL/MBID"))
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().user_agent(UA).timeout(Duration::from_secs(20)).build()
}

fn get_json(url: &str, discogs_token: Option<&str>) -> Result<Value> {
    let mut req = agent().get(url);
    if let Some(t) = discogs_token.filter(|t| !t.trim().is_empty()) {
        if url.contains("discogs.com") {
            req = req.set("Authorization", &format!("Discogs token={}", t.trim()));
        }
    }
    let resp = req.call().map_err(|e| match e {
        ureq::Error::Status(404, _) => anyhow!("release not found ({url})"),
        ureq::Error::Status(429, _) => anyhow!("rate limited by the server; wait a minute and retry"),
        other => anyhow!("{url}: {other}"),
    })?;
    Ok(resp.into_json()?)
}

pub fn fetch_bytes(url: &str, discogs_token: Option<&str>) -> Result<Vec<u8>> {
    let mut req = agent().get(url);
    if let Some(t) = discogs_token.filter(|t| !t.trim().is_empty()) {
        if url.contains("discogs.com") {
            req = req.set("Authorization", &format!("Discogs token={}", t.trim()));
        }
    }
    let resp = req.call().with_context(|| format!("downloading {url}"))?;
    let mut buf = Vec::new();
    resp.into_reader().take(30_000_000).read_to_end(&mut buf)?;
    Ok(buf)
}

pub fn fetch(input: &str, discogs_token: Option<&str>) -> Result<Release> {
    match parse_source(input)? {
        Source::Discogs(id) => fetch_discogs(id, discogs_token),
        Source::MusicBrainz(id) => fetch_musicbrainz(&id),
    }
}

/// "Artist (2)" -> "Artist"; joins multiple artists with their join phrases.
fn discogs_artists(v: &Value) -> Option<String> {
    let arr = v.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let mut out = String::new();
    for (i, a) in arr.iter().enumerate() {
        let anv = a["anv"].as_str().unwrap_or("");
        let name = if !anv.is_empty() { anv } else { a["name"].as_str().unwrap_or("") };
        let name = strip_discogs_suffix(name);
        out.push_str(&name);
        let join = a["join"].as_str().unwrap_or("").trim();
        if i + 1 < arr.len() {
            if join.is_empty() || join == "," {
                out.push_str(if join == "," { ", " } else { " & " });
            } else {
                out.push(' ');
                out.push_str(join);
                out.push(' ');
            }
        }
    }
    Some(out)
}

fn strip_discogs_suffix(name: &str) -> String {
    let n = name.trim();
    if n.ends_with(')') {
        if let Some(i) = n.rfind(" (") {
            if n[i + 2..n.len() - 1].chars().all(|c| c.is_ascii_digit()) {
                return n[..i].to_string();
            }
        }
    }
    n.to_string()
}

/// "A1" -> "A", "B" -> "B", "C3a" -> "C", "1" -> "", "CD1-2" -> "CD1"
fn side_of(position: &str) -> String {
    let p = position.trim();
    if let Some(i) = p.find('-') {
        return p[..i].to_string();
    }
    p.chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_uppercase()
}

fn fetch_discogs(id: u64, token: Option<&str>) -> Result<Release> {
    let v = get_json(&format!("https://api.discogs.com/releases/{id}"), token)?;
    let mut rel = parse_discogs(id, &v);
    if let Some(mid) = v["master_id"].as_u64() {
        if let Ok(m) = get_json(&format!("https://api.discogs.com/masters/{mid}"), token) {
            rel.original_year = m["year"].as_u64().filter(|y| *y > 0).map(|y| y.to_string());
        }
    }
    Ok(rel)
}

pub fn parse_discogs(id: u64, v: &Value) -> Release {
    let artist = discogs_artists(&v["artists"]).unwrap_or_else(|| "Unknown Artist".into());
    let mut tracks = Vec::new();
    for t in v["tracklist"].as_array().into_iter().flatten() {
        let kind = t["type_"].as_str().unwrap_or("track");
        if kind == "heading" {
            continue;
        }
        let mut duration = crate::split::parse_duration(t["duration"].as_str().unwrap_or(""));
        if kind == "index" && duration.is_none() {
            // medley: sum the sub tracks if all have lengths
            let subs: Vec<Option<f64>> = t["sub_tracks"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|s| crate::split::parse_duration(s["duration"].as_str().unwrap_or("")))
                .collect();
            if !subs.is_empty() && subs.iter().all(|d| d.is_some()) {
                duration = Some(subs.iter().map(|d| d.unwrap()).sum());
            }
        }
        let mut position = t["position"].as_str().unwrap_or("").to_string();
        if kind == "index" && position.is_empty() {
            if let Some(first) = t["sub_tracks"].as_array().and_then(|a| a.first()) {
                position = first["position"].as_str().unwrap_or("").to_string();
            }
        }
        let tartist = discogs_artists(&t["artists"]).filter(|a| *a != artist);
        tracks.push(Track {
            side: side_of(&position),
            position,
            number: tracks.len() + 1,
            title: t["title"].as_str().unwrap_or("").to_string(),
            artist: tartist,
            duration,
            mb_recording_id: None,
            mb_track_id: None,
        });
    }
    if tracks.iter().all(|t| t.side.is_empty()) {
        for t in &mut tracks {
            t.side = "All".into();
        }
    }
    let label = v["labels"].get(0).and_then(|l| l["name"].as_str()).map(strip_discogs_suffix);
    let catno = v["labels"].get(0).and_then(|l| l["catno"].as_str()).map(String::from);
    let release_year = v["year"].as_u64().filter(|y| *y > 0).map(|y| y.to_string());
    let original_year = None;
    let images = v["images"].as_array();
    let cover_url = images
        .and_then(|imgs| imgs.iter().find(|i| i["type"] == "primary").or_else(|| imgs.first()))
        .and_then(|i| i["uri"].as_str())
        .filter(|u| !u.is_empty())
        .map(String::from);
    Release {
        source: Source::Discogs(id),
        title: v["title"].as_str().unwrap_or("Unknown Album").to_string(),
        artist,
        original_year,
        release_year,
        label,
        catno,
        country: v["country"].as_str().map(String::from),
        tracks,
        cover_url,
        mb_release_group: None,
    }
}

fn mb_artist_credit(v: &Value) -> Option<String> {
    let arr = v.as_array()?;
    if arr.is_empty() {
        return None;
    }
    Some(
        arr.iter()
            .map(|c| format!("{}{}", c["name"].as_str().unwrap_or(""), c["joinphrase"].as_str().unwrap_or("")))
            .collect(),
    )
}

fn fetch_musicbrainz(mbid: &str) -> Result<Release> {
    let v = get_json(
        &format!(
            "https://musicbrainz.org/ws/2/release/{mbid}?inc=recordings+artist-credits+labels+release-groups&fmt=json"
        ),
        None,
    )?;
    Ok(parse_musicbrainz(mbid, &v))
}

pub fn parse_musicbrainz(mbid: &str, v: &Value) -> Release {
    let artist = mb_artist_credit(&v["artist-credit"]).unwrap_or_else(|| "Unknown Artist".into());
    let media = v["media"].as_array().cloned().unwrap_or_default();
    let multi = media.len() > 1;
    let mut tracks = Vec::new();
    for m in &media {
        let mpos = m["position"].as_u64().unwrap_or(1);
        for t in m["tracks"].as_array().into_iter().flatten() {
            let number_str = t["number"].as_str().unwrap_or("").to_string();
            let mut side = side_of(&number_str);
            if side.is_empty() {
                side = if multi { format!("Disc {mpos}") } else { "All".into() };
            }
            let len_ms = t["length"].as_f64().or_else(|| t["recording"]["length"].as_f64());
            let tartist = mb_artist_credit(&t["artist-credit"]).filter(|a| *a != artist);
            tracks.push(Track {
                position: number_str,
                side,
                number: tracks.len() + 1,
                title: t["title"].as_str().unwrap_or("").to_string(),
                artist: tartist,
                duration: len_ms.map(|ms| ms / 1000.0),
                mb_recording_id: t["recording"]["id"].as_str().map(String::from),
                mb_track_id: t["id"].as_str().map(String::from),
            });
        }
    }
    let year = |s: &str| s.get(..4).filter(|y| y.chars().all(|c| c.is_ascii_digit())).map(String::from);
    let li = v["label-info"].get(0);
    let has_front = v["cover-art-archive"]["front"].as_bool().unwrap_or(false);
    Release {
        source: Source::MusicBrainz(mbid.to_string()),
        title: v["title"].as_str().unwrap_or("Unknown Album").to_string(),
        artist,
        original_year: v["release-group"]["first-release-date"].as_str().and_then(year),
        release_year: v["date"].as_str().and_then(year),
        label: li.and_then(|l| l["label"]["name"].as_str()).map(String::from),
        catno: li.and_then(|l| l["catalog-number"].as_str()).map(String::from),
        country: v["country"].as_str().map(String::from),
        tracks,
        cover_url: has_front.then(|| format!("https://coverartarchive.org/release/{mbid}/front-1200")),
        mb_release_group: v["release-group"]["id"].as_str().map(String::from),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sources() {
        assert_eq!(parse_source("https://www.discogs.com/release/249504-Rick-Astley").unwrap(), Source::Discogs(249504));
        assert_eq!(parse_source("r249504").unwrap(), Source::Discogs(249504));
        assert_eq!(parse_source("[r249504]").unwrap(), Source::Discogs(249504));
        assert!(parse_source("https://www.discogs.com/master/96559").is_err());
        let mb = "https://musicbrainz.org/release/1e477f68-c407-4eae-ad01-518528cedc2c";
        assert_eq!(parse_source(mb).unwrap(), Source::MusicBrainz("1e477f68-c407-4eae-ad01-518528cedc2c".into()));
        assert_eq!(side_of("A1"), "A");
        assert_eq!(side_of("b12"), "B");
        assert_eq!(side_of("3"), "");
        assert_eq!(strip_discogs_suffix("Prince (2)"), "Prince");
    }

    #[test]
    fn discogs_json() {
        let v: Value = serde_json::from_str(
            r#"{
            "id": 123, "title": "Purple Rain", "year": 1984, "country": "Europe", "master_id": 4567,
            "artists": [{"name": "Prince (2)", "anv": "", "join": "And"}, {"name": "The Revolution", "anv": "", "join": ""}],
            "labels": [{"name": "Warner Bros. Records", "catno": "925 110-1"}],
            "images": [{"type": "secondary", "uri": "https://i.discogs.com/b.jpg"}, {"type": "primary", "uri": "https://i.discogs.com/a.jpg"}],
            "tracklist": [
              {"type_": "heading", "position": "", "title": "Side One", "duration": ""},
              {"type_": "track", "position": "A1", "title": "Let's Go Crazy", "duration": "4:39"},
              {"type_": "track", "position": "A2", "title": "Take Me With U", "duration": "3:54",
               "artists": [{"name": "Apollonia (4)", "anv": "", "join": ""}]},
              {"type_": "index", "position": "", "title": "Medley", "duration": "",
               "sub_tracks": [{"type_": "track", "position": "B1a", "title": "x", "duration": "1:00"},
                              {"type_": "track", "position": "B1b", "title": "y", "duration": "2:30"}]},
              {"type_": "track", "position": "B2", "title": "Purple Rain", "duration": ""}
            ]}"#,
        )
        .unwrap();
        let r = parse_discogs(123, &v);
        assert_eq!(r.artist, "Prince And The Revolution");
        assert_eq!(r.release_year.as_deref(), Some("1984"));
        assert_eq!(r.cover_url.as_deref(), Some("https://i.discogs.com/a.jpg"));
        assert_eq!(r.tracks.len(), 4);
        assert_eq!(r.sides(), vec!["A".to_string(), "B".to_string()]);
        assert_eq!(r.tracks[0].duration, Some(279.0));
        assert_eq!(r.tracks[1].artist.as_deref(), Some("Apollonia"));
        assert_eq!(r.tracks[2].position, "B1a");
        assert_eq!(r.tracks[2].duration, Some(210.0));
        assert_eq!(r.tracks[3].duration, None);
        assert_eq!(r.tracks[3].number, 4);
        assert_eq!(r.label.as_deref(), Some("Warner Bros. Records"));
    }

    #[test]
    fn musicbrainz_json() {
        let v: Value = serde_json::from_str(
            r#"{
            "id": "aaaaaaaa-1111-2222-3333-444444444444", "title": "Rumours", "date": "1977-02-04", "country": "GB",
            "artist-credit": [{"name": "Fleetwood Mac", "joinphrase": ""}],
            "label-info": [{"catalog-number": "K 56344", "label": {"name": "Warner Bros. Records"}}],
            "release-group": {"id": "rg-1", "first-release-date": "1977-02-04"},
            "cover-art-archive": {"front": true},
            "media": [{"position": 1, "format": "12\" Vinyl", "tracks": [
              {"id": "t1", "number": "A1", "position": 1, "title": "Second Hand News", "length": 163000,
               "recording": {"id": "r1", "length": 163000}},
              {"id": "t2", "number": "B1", "position": 2, "title": "The Chain", "length": null,
               "recording": {"id": "r2", "length": 270500}}
            ]}]}"#,
        )
        .unwrap();
        let r = parse_musicbrainz("aaaaaaaa-1111-2222-3333-444444444444", &v);
        assert_eq!(r.artist, "Fleetwood Mac");
        assert_eq!(r.release_year.as_deref(), Some("1977"));
        assert_eq!(r.sides(), vec!["A".to_string(), "B".to_string()]);
        assert_eq!(r.tracks[0].duration, Some(163.0));
        assert_eq!(r.tracks[1].duration, Some(270.5));
        assert_eq!(r.tracks[1].mb_recording_id.as_deref(), Some("r2"));
        assert_eq!(r.catno.as_deref(), Some("K 56344"));
        assert!(r.cover_url.unwrap().contains("coverartarchive.org"));
    }
}

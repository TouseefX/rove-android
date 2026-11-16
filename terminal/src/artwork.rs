//! Public game artwork. No account state enters this module.
use crate::search::Game;
use image::{DynamicImage, ImageReader, Limits};
use serde_json::Value;
use std::{
    collections::VecDeque,
    io::{Cursor, Read},
    sync::{mpsc, Arc},
    thread,
    time::{Duration, Instant},
};

pub struct Artwork {
    pub pictures: Vec<Arc<DynamicImage>>,
    pub icon: Option<Arc<DynamicImage>>,
}
type Loaded = Result<Arc<Artwork>, String>;
struct Entry {
    place: u64,
    result: Loaded,
    when: Instant,
}
#[derive(Default)]
pub struct Loader {
    cache: VecDeque<Entry>,
    response: Option<mpsc::Receiver<(u64, Loaded)>>,
    selected: Option<(Game, Instant)>,
    pub picture: usize,
}
impl Loader {
    pub fn select(&mut self, game: Option<&Game>) {
        if self.selected.as_ref().map(|(g, _)| g.place_id) != game.map(|g| g.place_id) {
            self.selected = game.cloned().map(|g| (g, Instant::now()));
            self.picture = 0;
        }
    }
    pub fn poll(&mut self) {
        if let Some((place, result)) = self.response.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.response = None;
            self.cache.retain(|e| e.place != place);
            self.cache.push_back(Entry {
                place,
                result,
                when: Instant::now(),
            });
            // Four games, at most three 768x432 covers and a 128x128 icon each.
            while self.cache.len() > 4 {
                self.cache.pop_front();
            }
        }
        let Some((game, since)) = &self.selected else {
            return;
        };
        if self.response.is_some() || since.elapsed() < Duration::from_millis(180) {
            return;
        }
        if let Some(entry) = self.cache.iter().find(|e| e.place == game.place_id) {
            let ttl = if entry.result.is_ok() { 300 } else { 30 };
            if entry.when.elapsed().as_secs() < ttl {
                return;
            }
        }
        let game = game.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        self.response = Some(rx);
        thread::spawn(move || {
            let result = load(&game).map(Arc::new);
            let _ = tx.send((game.place_id, result));
        });
    }
    pub fn get(&self, place: u64) -> Option<&Arc<Artwork>> {
        self.cache
            .iter()
            .find(|e| e.place == place)?
            .result
            .as_ref()
            .ok()
    }
    pub fn status(&self, place: u64) -> &str {
        match self.cache.iter().find(|e| e.place == place) {
            Some(Entry {
                result: Err(error), ..
            }) => error,
            _ => "loading artwork…",
        }
    }
    pub fn step(&mut self, delta: isize) {
        let count = self
            .selected
            .as_ref()
            .and_then(|(g, _)| self.get(g.place_id))
            .map(|art| art.pictures.len())
            .unwrap_or(0);
        if count > 0 {
            self.picture = (self.picture as isize + delta).rem_euclid(count as isize) as usize;
        }
    }
}

fn client() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(Duration::from_secs(6))
        .build()
}
fn body(response: ureq::Response, limit: usize) -> Result<Vec<u8>, String> {
    if response.status() != 200 {
        return Err("artwork unavailable".into());
    }
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "artwork download interrupted")?;
    if bytes.len() > limit {
        return Err("artwork exceeds size limit".into());
    }
    Ok(bytes)
}
fn json(request: ureq::Request) -> Result<Value, String> {
    let response = request
        .set("Accept", "application/json")
        .call()
        .map_err(|_| "artwork temporarily unavailable")?;
    serde_json::from_slice(&body(response, 256 * 1024)?)
        .map_err(|_| "artwork response was not valid JSON".into())
}
pub fn trusted_image_url(raw: &str) -> bool {
    url::Url::parse(raw).is_ok_and(|url| {
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
            && url
                .host_str()
                .is_some_and(|host| host == "rbxcdn.com" || host.ends_with(".rbxcdn.com"))
    })
}
fn completed(row: &Value) -> Option<String> {
    let url = row["imageUrl"].as_str()?;
    (row["state"] == "Completed" && trusted_image_url(url)).then(|| url.to_owned())
}
pub fn cover_urls(value: &Value, universe: u64) -> Vec<String> {
    let mut urls = Vec::new();
    for row in value["data"].as_array().into_iter().flatten() {
        if row["universeId"].as_u64() != Some(universe) {
            continue;
        }
        for thumbnail in row["thumbnails"].as_array().into_iter().flatten() {
            if let Some(url) = completed(thumbnail) {
                if !urls.contains(&url) {
                    urls.push(url);
                }
            }
            if urls.len() == 3 {
                return urls;
            }
        }
    }
    urls
}
fn decode(bytes: Vec<u8>, max_width: u32, max_height: u32) -> Result<Arc<DynamicImage>, String> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| "unrecognized artwork format")?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(2048);
    limits.max_image_height = Some(2048);
    limits.max_alloc = Some(16 * 1024 * 1024);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|_| "artwork could not be decoded")?;
    Ok(Arc::new(DynamicImage::ImageRgba8(
        image.thumbnail(max_width, max_height).to_rgba8(),
    )))
}
fn download(agent: &ureq::Agent, url: &str, size: (u32, u32)) -> Result<Arc<DynamicImage>, String> {
    if !trusted_image_url(url) {
        return Err("unrecognized artwork host".into());
    }
    let response = agent
        .get(url)
        .call()
        .map_err(|_| "artwork download failed")?;
    decode(body(response, 3 * 1024 * 1024)?, size.0, size.1)
}
fn load(game: &Game) -> Result<Artwork, String> {
    let agent = client();
    let universe = if let Some(id) = game.universe_id {
        id
    } else {
        json(agent.get(&format!(
            "https://apis.roblox.com/universes/v1/places/{}/universe",
            game.place_id
        )))?["universeId"]
            .as_u64()
            .ok_or("game artwork unavailable")?
    };
    let gallery = json(
        agent
            .get("https://thumbnails.roblox.com/v1/games/multiget/thumbnails")
            .query("universeIds", &universe.to_string())
            .query("countPerUniverse", "3")
            .query("defaults", "true")
            .query("size", "768x432")
            .query("format", "Png")
            .query("isCircular", "false"),
    );
    let pictures = gallery
        .ok()
        .into_iter()
        .flat_map(|v| cover_urls(&v, universe))
        .filter_map(|url| download(&agent, &url, (768, 432)).ok())
        .collect::<Vec<_>>();
    let icons = json(
        agent
            .get("https://thumbnails.roblox.com/v1/games/icons")
            .query("universeIds", &universe.to_string())
            .query("size", "150x150")
            .query("format", "Png")
            .query("isCircular", "false"),
    );
    let icon = icons
        .ok()
        .and_then(|value| {
            value["data"]
                .as_array()?
                .iter()
                .find(|row| row["targetId"].as_u64() == Some(universe))
                .and_then(completed)
        })
        .and_then(|url| download(&agent, &url, (128, 128)).ok());
    let pictures = if pictures.is_empty() {
        icon.iter().cloned().collect()
    } else {
        pictures
    };
    if pictures.is_empty() {
        return Err("no artwork available".into());
    }
    Ok(Artwork { pictures, icon })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn only_completed_art_for_the_selected_universe_is_used() {
        let url = "https://tr.rbxcdn.com/cover.png";
        let data = json!({"data":[
            {"universeId":2,"thumbnails":[{"state":"Completed","imageUrl":url}]},
            {"universeId":1,"thumbnails":[{"state":"Pending","imageUrl":url},
                {"state":"Completed","imageUrl":"https://evil.test/img"},
                {"state":"Completed","imageUrl":url},{"state":"Completed","imageUrl":url}]}
        ]});
        assert_eq!(cover_urls(&data, 1), [url]);
        for bad in [
            "http://tr.rbxcdn.com/a",
            "https://rbxcdn.com.evil.test/a",
            "https://rbxcdn.com@evil.test/a",
            "file:///tmp/a",
            "https://evil@tr.rbxcdn.com/a",
        ] {
            assert!(!trusted_image_url(bad));
        }
    }
    #[test]
    fn corrupt_images_fail_and_large_images_are_bounded() {
        assert!(decode(vec![0, 1, 2, 3], 512, 288).is_err());
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::new_rgba8(2300, 1)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        assert!(decode(bytes.into_inner(), 512, 288).is_err());
    }
    #[test]
    fn switching_selection_resets_the_gallery_without_showing_another_game() {
        let mut loader = Loader::default();
        let one = crate::search::direct("1").unwrap();
        let two = crate::search::direct("2").unwrap();
        loader.select(Some(&one));
        loader.picture = 2;
        loader.select(Some(&two));
        assert_eq!(loader.picture, 0);
        assert!(loader.get(two.place_id).is_none());
    }
}

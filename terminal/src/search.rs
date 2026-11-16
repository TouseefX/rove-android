use serde_json::Value;
use std::{
    collections::HashSet,
    io::Read,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, PartialEq)]
pub struct Game {
    pub place_id: u64,
    pub universe_id: Option<u64>,
    pub name: String,
    pub creator: String,
    pub players: Option<u64>,
    pub description: String,
    pub verified: bool,
    pub approval: Option<f64>,
}
pub fn clean(text: &str) -> String {
    // Game names and script output cannot inject terminal escape sequences.
    text.chars()
        .filter(|c| !c.is_control() && *c != '\u{202e}' && *c != '\u{202d}')
        .take(4096)
        .collect()
}
pub fn place_id(input: &str) -> Option<u64> {
    let input = input.trim();
    let id = if input.bytes().all(|b| b.is_ascii_digit()) {
        input.parse().ok()?
    } else {
        let url = url::Url::parse(input).ok()?;
        if url.scheme() != "https"
            || !matches!(url.host_str(), Some("www.roblox.com" | "roblox.com"))
        {
            return None;
        }
        let parts: Vec<_> = url.path_segments()?.collect();
        if parts.first()? != &"games" {
            return None;
        }
        parts.get(1)?.parse().ok()?
    };
    (id > 0 && id <= 9_007_199_254_740_991).then_some(id)
}
pub fn direct(input: &str) -> Option<Game> {
    place_id(input).map(|place_id| Game {
        place_id,
        universe_id: None,
        name: format!("place {place_id}"),
        creator: String::new(),
        players: None,
        description: String::new(),
        verified: false,
        approval: None,
    })
}
pub fn parse_results(value: &Value) -> Result<Vec<Game>, String> {
    let groups = value["searchResults"]
        .as_array()
        .ok_or("Roblox returned an unfamiliar search response")?;
    let mut seen = HashSet::new();
    let mut results = Vec::new();
    for group in groups {
        if group["contentGroupType"] != "Game" {
            continue;
        }
        for row in group["contents"].as_array().into_iter().flatten() {
            let Some(id) = row["rootPlaceId"]
                .as_u64()
                .filter(|id| *id > 0 && *id <= 9_007_199_254_740_991)
            else {
                continue;
            };
            if row["isSponsored"] == true || !seen.insert(id) {
                continue;
            }
            let Some(name) = row["name"].as_str() else {
                continue;
            };
            results.push(Game {
                place_id: id,
                universe_id: row["universeId"].as_u64().filter(|id| *id > 0),
                name: clean(name),
                creator: clean(row["creatorName"].as_str().unwrap_or("")),
                players: row["playerCount"].as_u64(),
                description: clean(row["description"].as_str().unwrap_or("")),
                verified: row["creatorHasVerifiedBadge"] == true,
                approval: row["totalUpVotes"]
                    .as_u64()
                    .zip(row["totalDownVotes"].as_u64())
                    .and_then(|(up, down)| {
                        let total = (up as f64) + (down as f64);
                        (total > 0.0).then_some(up as f64 / total * 100.0)
                    }),
            });
            if results.len() == 40 {
                return Ok(results);
            }
        }
    }
    Ok(results)
}
pub fn search(query: &str) -> Result<Vec<Game>, String> {
    let query = query.trim();
    if query.is_empty() || query.len() > 200 {
        return Err("Search needs 1–200 characters".into());
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let session = format!(
        "{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}",
        (stamp >> 64) as u32,
        (stamp >> 48) as u16,
        (stamp >> 36) & 0xfff,
        (stamp >> 24) & 0xfff,
        stamp & 0xffffffffffff
    );
    // This public search never receives the account cookie.
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .build();
    let response = agent
        .get("https://apis.roblox.com/search-api/omni-search")
        .query("searchQuery", query)
        .query("sessionId", &session)
        .query("pageType", "all")
        .set("Accept", "application/json")
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => {
                format!("Roblox search returned HTTP {code}; try a place ID or retry later")
            }
            _ => "Search could not reach Roblox; check your connection".into(),
        })?;
    let mut body = Vec::new();
    response
        .into_reader()
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut body)
        .map_err(|_| "Search response interrupted")?;
    if body.len() > 2 * 1024 * 1024 {
        return Err("Search response exceeded 2 MiB".into());
    }
    parse_results(&serde_json::from_slice(&body).map_err(|_| "Search response was not JSON")?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn accepts_only_real_public_place_links_and_safe_ids() {
        assert_eq!(place_id("189707"), Some(189707));
        assert_eq!(
            place_id("https://www.roblox.com/games/189707/name"),
            Some(189707)
        );
        for input in [
            "0",
            "9007199254740992",
            "https://roblox.com.bad/games/4",
            "https://www.roblox.com@evil.test/games/4",
            "javascript:alert(1)",
            "https://www.roblox.com/share?code=secret",
        ] {
            assert_eq!(place_id(input), None, "{input}");
        }
    }
    #[test]
    fn ignores_non_game_sponsored_and_duplicate_results() {
        let result = parse_results(&json!({"searchResults":[
            {"contentGroupType":"User","contents":[{"rootPlaceId":6,"name":"user"}]},
            {"contentGroupType":"Game","contents":[{"rootPlaceId":1,"name":"real","playerCount":9},{"rootPlaceId":1,"name":"duplicate"},{"rootPlaceId":2,"name":"ad","isSponsored":true},{"universeId":3,"name":"no place"}]}
        ]})).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].players, Some(9));
        assert!(parse_results(&json!({"errors":[]})).is_err());
    }
    #[test]
    fn strips_terminal_controls() {
        assert_eq!(clean("hi\x1b[31m\r\n"), "hi[31m");
    }
}

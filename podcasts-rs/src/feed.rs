//! RSS feed parsing (RSS 2.0 + iTunes namespace).

use std::sync::LazyLock;

use anyhow::{anyhow, Result};
use regex::Regex;
use roxmltree::{Document, Node, ParsingOptions};
use serde::Serialize;

use crate::app::{short_id, truncate};

const ITUNES: &str = "http://www.itunes.com/dtds/podcast-1.0.dtd";

static TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
static WS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
static EP_NUM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:#|ep(?:isode)?\.?\s*|folge\s*|第)\s*([0-9]{1,5})").unwrap());

#[derive(Serialize, Clone)]
pub struct Info {
    pub title: String,
    pub author: String,
    pub artwork: String,
    pub description: String,
}

#[derive(Serialize, Clone)]
pub struct Episode {
    pub id: String,
    pub title: String,
    pub date: Option<f64>,
    pub duration: Option<i64>,
    pub number: Option<i64>,
    pub url: String,
    #[serde(rename = "type")]
    pub mime: String,
    pub image: String,
    pub description: String,
}

pub struct Feed {
    pub info: Info,
    pub episodes: Vec<Episode>,
}

fn child<'a, 'i>(node: Node<'a, 'i>, ns: Option<&str>, name: &str) -> Option<Node<'a, 'i>> {
    node.children()
        .find(|c| c.is_element() && c.tag_name().name() == name && c.tag_name().namespace() == ns)
}

/// Direct text content (incl. CDATA) of an optional element, trimmed.
fn text(node: Option<Node>) -> String {
    node.map(|n| {
        n.children()
            .filter(|c| c.is_text())
            .filter_map(|c| c.text())
            .collect::<String>()
            .trim()
            .to_string()
    })
    .unwrap_or_default()
}

fn itunes_href(node: Node, name: &str) -> String {
    child(node, Some(ITUNES), name)
        .and_then(|n| n.attribute("href"))
        .unwrap_or("")
        .to_string()
}

fn strip_html(raw: &str) -> String {
    let s = TAG_RE.replace_all(raw, " ");
    let s = html_escape::decode_html_entities(&s);
    WS_RE.replace_all(&s, " ").trim().to_string()
}

fn parse_duration(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.contains(':') {
        let mut secs = 0i64;
        for part in raw.split(':') {
            secs = secs * 60 + part.trim().parse::<f64>().ok()? as i64;
        }
        Some(secs)
    } else {
        raw.parse::<f64>().ok().map(|f| f as i64)
    }
}

pub fn parse(bytes: &[u8]) -> Result<Feed> {
    let xml = String::from_utf8_lossy(bytes);
    let xml = xml.trim_start_matches('\u{feff}');
    let opts = ParsingOptions { allow_dtd: true, ..Default::default() };
    let doc = Document::parse_with_options(xml, opts)?;
    let channel = child(doc.root_element(), None, "channel").ok_or_else(|| anyhow!("Kein RSS-Channel gefunden"))?;

    let mut artwork = itunes_href(channel, "image");
    if artwork.is_empty() {
        artwork = text(child(channel, None, "image").and_then(|n| child(n, None, "url")));
    }
    let info = Info {
        title: text(child(channel, None, "title")),
        author: text(child(channel, Some(ITUNES), "author")),
        artwork,
        description: truncate(&strip_html(&text(child(channel, None, "description"))), 600),
    };

    let mut episodes = Vec::new();
    for item in channel.children().filter(|c| c.is_element() && c.has_tag_name("item")) {
        let Some(enc) = child(item, None, "enclosure") else { continue };
        let url = enc.attribute("url").unwrap_or("");
        if url.is_empty() {
            continue;
        }
        let mut guid = text(child(item, None, "guid"));
        if guid.is_empty() {
            guid = url.to_string();
        }
        let mut title = text(child(item, None, "title"));
        if title.is_empty() {
            title = "(ohne Titel)".into();
        }
        let date = chrono::DateTime::parse_from_rfc2822(&text(child(item, None, "pubDate")))
            .ok()
            .map(|d| d.timestamp() as f64);
        let num_raw = text(child(item, Some(ITUNES), "episode"));
        let mut number = None;
        if !num_raw.is_empty() && num_raw.bytes().all(|b| b.is_ascii_digit()) {
            number = num_raw.parse().ok();
        }
        if number.is_none() {
            number = EP_NUM_RE.captures(&title).and_then(|c| c[1].parse().ok());
        }
        let mut desc = text(child(item, Some(ITUNES), "summary"));
        if desc.is_empty() {
            desc = text(child(item, None, "description"));
        }
        episodes.push(Episode {
            id: short_id(&guid),
            date,
            duration: parse_duration(&text(child(item, Some(ITUNES), "duration"))),
            number,
            url: url.to_string(),
            mime: enc.attribute("type").unwrap_or("").to_string(),
            image: itunes_href(item, "image"),
            description: truncate(&strip_html(&desc), 500),
            title,
        });
    }
    Ok(Feed { info, episodes })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_feed() {
        let xml = br#"<?xml version="1.0"?>
<rss xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd"><channel>
  <title>Test</title><itunes:author>Me</itunes:author>
  <itunes:image href="https://x/cover.jpg"/>
  <description><![CDATA[<p>Hello &amp; welcome</p>]]></description>
  <item><title>#85 Something</title><guid>abc</guid>
    <pubDate>Sun, 07 Nov 2021 10:00:00 +0000</pubDate>
    <itunes:duration>01:02:03</itunes:duration>
    <enclosure url="https://x/ep.mp3" type="audio/mpeg"/></item>
  <item><title>no enclosure</title></item>
</channel></rss>"#;
        let feed = parse(xml).unwrap();
        assert_eq!(feed.info.artwork, "https://x/cover.jpg");
        assert_eq!(feed.info.description, "Hello & welcome");
        assert_eq!(feed.episodes.len(), 1);
        let ep = &feed.episodes[0];
        assert_eq!(ep.number, Some(85));
        assert_eq!(ep.duration, Some(3723));
        assert_eq!(ep.date, Some(1636279200.0));
        assert_eq!(ep.id, short_id("abc"));
    }

    #[test]
    fn duration_formats() {
        assert_eq!(parse_duration("905"), Some(905));
        assert_eq!(parse_duration("15:05"), Some(905));
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("abc"), None);
    }
}

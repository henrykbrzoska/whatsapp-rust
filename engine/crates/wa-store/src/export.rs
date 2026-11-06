//! Chat history export — streaming-friendly encoders for JSON / JSONL / CSV.
//!
//! The caller pages through `Store::page` and feeds every batch to
//! [`Exporter::chunk`]; `begin`/`end` wrap the stream.

use crate::{Store, StoredMessage};

pub const PAGE_SIZE: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Jsonl,
    Csv,
}

impl std::str::FromStr for Format {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "json" | "" => Ok(Self::Json),
            "jsonl" | "ndjson" => Ok(Self::Jsonl),
            "csv" => Ok(Self::Csv),
            o => Err(format!("unknown export format `{o}`")),
        }
    }
}

impl Format {
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Json => "application/json",
            Self::Jsonl => "application/x-ndjson",
            Self::Csv => "text/csv; charset=utf-8",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Jsonl => "jsonl",
            Self::Csv => "csv",
        }
    }
}

pub struct Exporter {
    format: Format,
    first: bool,
}

impl Exporter {
    pub fn new(format: Format) -> Self {
        Self { format, first: true }
    }

    pub fn begin(&self) -> String {
        match self.format {
            Format::Json => "[".into(),
            Format::Jsonl => String::new(),
            Format::Csv => "id,chat_jid,sender,timestamp,type,text\n".into(),
        }
    }

    pub fn chunk(&mut self, msgs: &[StoredMessage]) -> String {
        let mut out = String::new();
        for m in msgs {
            match self.format {
                Format::Json => {
                    if !self.first {
                        out.push(',');
                    }
                    out.push_str(&serde_json::to_string(m).unwrap_or_default());
                }
                Format::Jsonl => {
                    out.push_str(&serde_json::to_string(m).unwrap_or_default());
                    out.push('\n');
                }
                Format::Csv => {
                    let cells = [
                        m.id.clone(),
                        m.chat_jid.clone(),
                        m.sender.clone(),
                        m.timestamp.to_string(),
                        m.kind.clone(),
                        m.text.clone().unwrap_or_default(),
                    ];
                    out.push_str(&cells.iter().map(|c| csv_cell(c)).collect::<Vec<_>>().join(","));
                    out.push('\n');
                }
            }
            self.first = false;
        }
        out
    }

    pub fn end(&self) -> String {
        match self.format {
            Format::Json => "]".into(),
            _ => String::new(),
        }
    }
}

fn csv_cell(s: &str) -> String {
    // Also neutralise spreadsheet formula injection.
    let s = if s.starts_with(['=', '+', '-', '@']) { format!("'{s}") } else { s.to_string() };
    if s.contains([',', '"', '\n', '\r']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s }
}

/// Export a whole chat into a single string (small chats / tests).
pub fn export_all(
    store: &Store,
    chat: &str,
    format: Format,
    from: Option<i64>,
    to: Option<i64>,
) -> anyhow::Result<String> {
    let mut ex = Exporter::new(format);
    let mut out = ex.begin();
    let mut cursor: Option<(i64, String)> = None;
    loop {
        let page = store.page(chat, cursor.as_ref().map(|(t, i)| (*t, i.as_str())), from, to, PAGE_SIZE)?;
        if page.is_empty() {
            break;
        }
        out.push_str(&ex.chunk(&page));
        let last = page.last().unwrap();
        cursor = Some((last.timestamp, last.id.clone()));
    }
    out.push_str(&ex.end());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::msg;

    fn store() -> Store {
        let s = Store::open_memory(None).unwrap();
        s.insert_messages(&[
            msg("1", "c", 1, "hey"),
            msg("2", "c", 2, "a, \"quote\"\nnew line"),
            msg("3", "other", 3, "no"),
        ])
        .unwrap();
        s
    }

    #[test]
    fn json_is_valid_array() {
        let out = export_all(&store(), "c", Format::Json, None, None).unwrap();
        let v: Vec<StoredMessage> = serde_json::from_str(&out).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].id, "2");
    }

    #[test]
    fn empty_json_chat() {
        assert_eq!(export_all(&store(), "none", Format::Json, None, None).unwrap(), "[]");
    }

    #[test]
    fn jsonl_lines() {
        let out = export_all(&store(), "c", Format::Jsonl, None, None).unwrap();
        assert_eq!(out.lines().count(), 2);
        for l in out.lines() {
            serde_json::from_str::<StoredMessage>(l).unwrap();
        }
    }

    #[test]
    fn csv_escapes() {
        let out = export_all(&store(), "c", Format::Csv, None, Some(2)).unwrap();
        assert!(out.starts_with("id,chat_jid,sender,timestamp,type,text\n"));
        assert!(out.contains("\"a, \"\"quote\"\"\nnew line\""));
        assert_eq!(csv_cell("=1+1"), "'=1+1");
    }

    #[test]
    fn formats_parse() {
        assert_eq!("csv".parse::<Format>().unwrap(), Format::Csv);
        assert!("xml".parse::<Format>().is_err());
    }
}

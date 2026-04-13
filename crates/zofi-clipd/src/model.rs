use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Image,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Image => "image",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "text" => Some(Kind::Text),
            "image" => Some(Kind::Image),
            _ => None,
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    /// Stable identifier per unique item. Reused on dedup of the primary mime.
    pub uuid: String,
    pub kind: Kind,
    /// Canonical mime — what `activate()` puts on the clipboard by default
    /// and what drives the list preview before the user picks a variant.
    pub primary_mime: String,
    /// Truncated text snippet for the list row. None for images.
    pub preview: Option<String>,
    pub created_at: i64,
    pub last_used_at: i64,
    /// All mime representations the daemon captured when this item was
    /// synced. Always contains at least `primary_mime`.
    pub mimes: Vec<MimeContent>,
}

#[derive(Debug, Clone)]
pub struct MimeContent {
    pub mime: String,
    pub content: Vec<u8>,
}

impl Entry {
    pub fn primary_content(&self) -> Option<&[u8]> {
        self.mimes
            .iter()
            .find(|m| m.mime == self.primary_mime)
            .map(|m| m.content.as_slice())
    }

    pub fn content_for(&self, mime: &str) -> Option<&[u8]> {
        self.mimes
            .iter()
            .find(|m| m.mime == mime)
            .map(|m| m.content.as_slice())
    }
}

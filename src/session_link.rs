//! Stable links to sessions in this app's data directory.

use uuid::Uuid;

pub(crate) fn url(session_id: Uuid) -> String {
    format!("{}://session/{session_id}", crate::identity::URL_SCHEME)
}

/// Only a complete session link is an attachment paste. Prose, Markdown,
/// malformed links and links for the other app build remain ordinary text.
pub(crate) fn session_id(text: &str) -> Option<Uuid> {
    let prefix = format!("{}://session/", crate::identity::URL_SCHEME);
    let id = text.trim().strip_prefix(&prefix)?;
    // Require the canonical UUID spelling, with no path, query or fragment.
    let session_id = Uuid::parse_str(id).ok()?;
    (session_id.to_string() == id).then_some(session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_links_round_trip_without_accepting_other_content() {
        let id = Uuid::new_v4();
        let link = url(id);
        assert_eq!(session_id(&link), Some(id));
        assert_eq!(session_id(&format!(" \n{link}\r\n")), Some(id));
        for text in [
            format!("see {link}"),
            format!("[Session]({link})"),
            format!("{link}/"),
            format!("{link}?title=test"),
            format!("{link}#fragment"),
            format!("{link}\n{link}"),
            link.replace("://session/", "://project/"),
            link.replace(&id.to_string(), "invalid"),
            link.replacen(crate::identity::URL_SCHEME, "https", 1),
        ] {
            assert_eq!(session_id(&text), None, "{text}");
        }
    }
}

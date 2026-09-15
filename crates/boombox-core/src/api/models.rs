use serde::{Deserialize, Serialize};

/// Deliberately minimal. February 2026 stripped `country`, `email`,
/// `product`, `followers` and `explicit_content` from this object, so
/// modelling them would only invite code that can never work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrentUser {
    pub id: String,
    pub display_name: Option<String>,
    pub uri: String,
    #[serde(default)]
    pub images: Vec<Image>,
}

impl CurrentUser {
    pub fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    pub url: String,
    pub height: Option<u32>,
    pub width: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_post_february_2026_profile() {
        let raw = r#"{
            "id": "example",
            "display_name": "Alex",
            "uri": "spotify:user:example",
            "href": "https://api.spotify.com/v1/users/example",
            "images": [{"url": "https://i.scdn.co/image/abc", "height": 300, "width": 300}],
            "type": "user"
        }"#;
        let u: CurrentUser = serde_json::from_str(raw).unwrap();
        assert_eq!(u.label(), "Alex");
        assert_eq!(u.images.len(), 1);
    }

    #[test]
    fn falls_back_to_id_when_display_name_is_null() {
        let raw = r#"{"id":"example","display_name":null,"uri":"spotify:user:example"}"#;
        let u: CurrentUser = serde_json::from_str(raw).unwrap();
        assert_eq!(u.label(), "example");
        assert!(u.images.is_empty());
    }
}

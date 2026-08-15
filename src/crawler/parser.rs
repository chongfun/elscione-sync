/// A single entry from a directory listing (returned by the h5ai API or an HTML parser).
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// Full URL of the entry.
    pub url: String,
    /// Display name.
    pub name: String,
    /// True if this entry is a sub-directory.
    pub is_dir: bool,
    /// Raw last-modified string (e.g. "2023-04-13 00:12").
    pub last_modified: Option<String>,
    /// Size in bytes (`None` for directories or when unavailable).
    pub size_bytes: Option<i64>,
}

/// Extract the h5ai CSRF token from a `<meta ...>` tag in HTML.
///
/// Handles arbitrary attribute ordering, double quotes, single quotes, unquoted values,
/// whitespace differences, self-closing tags, and case-insensitivity.
pub fn extract_clckd(html: &str) -> Option<String> {
    let mut rest = html;
    while let Some(start_idx) = find_case_insensitive(rest, "<meta") {
        let tag_rest = &rest[start_idx + 5..];
        let end_idx = match tag_rest.find('>') {
            Some(i) => i,
            None => break,
        };
        let tag_body = &tag_rest[..end_idx];
        rest = &tag_rest[end_idx + 1..];

        let attrs = parse_tag_attributes(tag_body);
        let mut is_clckd = false;
        let mut content = None;

        for (k, v) in attrs {
            let k_lower = k.to_ascii_lowercase();
            if k_lower == "name" || k_lower == "property" || k_lower == "id" {
                if v.eq_ignore_ascii_case("clckd") {
                    is_clckd = true;
                }
            } else if k_lower == "content" {
                content = Some(v);
            }
        }

        if is_clckd {
            if let Some(c) = content {
                if !c.is_empty() {
                    return Some(c);
                }
            }
        }
    }
    None
}

/// Extract page `<title>` from HTML for diagnostic error messages.
pub fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start_tag = "<title";
    let start_pos = lower.find(start_tag)?;
    let after_tag = &html[start_pos + start_tag.len()..];
    let close_tag_start = after_tag.find('>')?;
    let content_start = &after_tag[close_tag_start + 1..];
    let end_pos = content_start.to_ascii_lowercase().find("</title>")?;
    let title = content_start[..end_pos].trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_owned())
    }
}

/// Parse HTML tag attributes into (name, value) pairs.
fn parse_tag_attributes(body: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let chars: Vec<char> = body.chars().collect();
    let n = chars.len();
    let mut i = 0;

    while i < n {
        while i < n && (chars[i].is_whitespace() || chars[i] == '/') {
            i += 1;
        }
        if i >= n {
            break;
        }

        let key_start = i;
        while i < n
            && !chars[i].is_whitespace()
            && chars[i] != '='
            && chars[i] != '>'
            && chars[i] != '/'
        {
            i += 1;
        }
        let key: String = chars[key_start..i].iter().collect();
        if key.is_empty() {
            i += 1;
            continue;
        }

        while i < n && chars[i].is_whitespace() {
            i += 1;
        }

        if i < n && chars[i] == '=' {
            i += 1;
            while i < n && chars[i].is_whitespace() {
                i += 1;
            }
            if i >= n {
                attrs.push((key, String::new()));
                break;
            }

            let quote = chars[i];
            if quote == '"' || quote == '\'' {
                i += 1;
                let val_start = i;
                while i < n && chars[i] != quote {
                    i += 1;
                }
                let val: String = chars[val_start..i].iter().collect();
                if i < n && chars[i] == quote {
                    i += 1;
                }
                attrs.push((key, val));
            } else {
                let val_start = i;
                while i < n && !chars[i].is_whitespace() && chars[i] != '>' && chars[i] != '/' {
                    i += 1;
                }
                let val: String = chars[val_start..i].iter().collect();
                attrs.push((key, val));
            }
        } else {
            attrs.push((key, String::new()));
        }
    }

    attrs
}

fn find_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let needle_lower = needle.to_ascii_lowercase();
    let needle_bytes = needle_lower.as_bytes();
    let haystack_bytes = haystack.as_bytes();
    let n = needle_bytes.len();

    if haystack_bytes.len() < n {
        return None;
    }

    for (byte_idx, _) in haystack.char_indices() {
        if byte_idx + n <= haystack_bytes.len() {
            if haystack_bytes[byte_idx..byte_idx + n].eq_ignore_ascii_case(needle_bytes) {
                return Some(byte_idx);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_clckd_standard() {
        let html = r#"<head><meta name="clckd" content="abc123"/></head>"#;
        assert_eq!(extract_clckd(html), Some("abc123".to_string()));
    }

    #[test]
    fn test_extract_clckd_with_multibyte_utf8_prefix() {
        let html = r#"<!DOCTYPE html><html><head><title>日本語のタイトル - 魔法少女</title><meta name="author" content="作家"><meta name="clckd" content="utf8_safe_token"></head></html>"#;
        assert_eq!(extract_clckd(html), Some("utf8_safe_token".to_string()));
    }

    #[test]
    fn test_extract_clckd_reversed_attributes() {
        let html = r#"<meta content="token456" name="clckd">"#;
        assert_eq!(extract_clckd(html), Some("token456".to_string()));
    }

    #[test]
    fn test_extract_clckd_single_quotes() {
        let html = r#"<meta name='clckd' content='single_token'>"#;
        assert_eq!(extract_clckd(html), Some("single_token".to_string()));
    }

    #[test]
    fn test_extract_clckd_extra_attributes_and_whitespace() {
        let html = r#"<meta data-foo="bar"   name = "clckd"  id="meta-tag"  content = "extra_token"  />"#;
        assert_eq!(extract_clckd(html), Some("extra_token".to_string()));
    }

    #[test]
    fn test_extract_clckd_uppercase() {
        let html = r#"<META NAME="CLCKD" CONTENT="UPPERCASE_TOKEN">"#;
        assert_eq!(extract_clckd(html), Some("UPPERCASE_TOKEN".to_string()));
    }

    #[test]
    fn test_extract_clckd_missing() {
        let html = r#"<html><head><title>Not h5ai</title><meta name="description" content="hello"></head></html>"#;
        assert_eq!(extract_clckd(html), None);
    }

    #[test]
    fn test_extract_title() {
        let html = r#"<html><head><title> Access Denied | Cloudflare </title></head><body></body></html>"#;
        assert_eq!(
            extract_title(html),
            Some("Access Denied | Cloudflare".to_string())
        );

        assert_eq!(extract_title("<html><body>No title</body></html>"), None);
    }
}

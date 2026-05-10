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

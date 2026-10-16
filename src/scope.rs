//! Restricting a search to one folder.
//!
//! Everything treats a search term that contains `\` as a match against the
//! full path, so prefixing the user's text with the quoted folder path plus a
//! trailing `\` limits results to that folder and everything below it.

/// The query Everything receives for `text` searched inside `folder`.
pub fn scoped_query(folder: &str, text: &str) -> String {
    let mut f = folder.trim_end_matches('\\').to_string();
    f.push('\\');
    format!("\"{f}\" {text}")
}

/// Short label for the scope chip: the folder's own name, or the drive.
pub fn label(folder: &str) -> String {
    let trimmed = folder.trim_end_matches('\\');
    match trimmed.rsplit('\\').next() {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => folder.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_path_prefixed_query() {
        assert_eq!(
            scoped_query("C:\\Users\\nikhi\\Downloads", "report pdf"),
            "\"C:\\Users\\nikhi\\Downloads\\\" report pdf"
        );
        assert_eq!(scoped_query("D:\\", "x"), "\"D:\\\" x");
        assert_eq!(scoped_query("D:\\Music\\", "x"), "\"D:\\Music\\\" x");
    }

    #[test]
    fn labels() {
        assert_eq!(label("C:\\Users\\nikhi\\Downloads"), "Downloads");
        assert_eq!(label("C:\\Users\\nikhi\\Downloads\\"), "Downloads");
        assert_eq!(label("D:\\"), "D:");
    }
}

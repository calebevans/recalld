//! Entity name canonicalization for improved cross-memory linking.

const HONORIFICS: &[&str] = &[
    "dr.",
    "dr",
    "mr.",
    "mr",
    "mrs.",
    "mrs",
    "ms.",
    "ms",
    "professor",
    "prof.",
    "prof",
    "captain",
    "capt.",
    "capt",
    "officer",
    "sir",
    "dame",
    "lord",
    "lady",
    "reverend",
    "rev.",
    "rev",
    "sergeant",
    "sgt.",
    "sgt",
    "lieutenant",
    "lt.",
    "lt",
    "corporal",
    "cpl.",
    "cpl",
    "private",
    "pvt.",
    "pvt",
    "general",
    "gen.",
    "gen",
    "colonel",
    "col.",
    "col",
    "major",
    "maj.",
    "maj",
    "judge",
    "justice",
    "senator",
    "representative",
    "congressman",
    "congresswoman",
    "president",
    "governor",
    "mayor",
    "coach",
    "chef",
    "nurse",
    "brother",
    "sister",
    "father",
    "mother",
    "uncle",
    "aunt",
];

/// Return canonical form(s) of an entity name.
///
/// Returns 1-2 strings: always the normalized full name, plus the
/// last name alone if the input is multi-word. Both forms are indexed
/// so "Dr. Sarah Martinez" links to memories mentioning just "Martinez".
pub fn canonicalize_entity(raw: &str) -> Vec<String> {
    let mut name = raw.trim().to_lowercase();

    // Strip possessives
    for suffix in &["'s", "\u{2019}s", "'s"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            name = stripped.trim_end().to_string();
            break;
        }
    }

    // Strip leading honorifics
    for prefix in HONORIFICS {
        if let Some(rest) = name.strip_prefix(prefix) {
            let rest = rest.trim_start();
            if !rest.is_empty() {
                name = rest.to_string();
                break;
            }
        }
    }

    // Collapse whitespace
    let name: String = name.split_whitespace().collect::<Vec<_>>().join(" ");

    if name.is_empty() {
        return vec![];
    }

    let mut results = vec![name.clone()];

    // Last-name alias for multi-word names
    let parts: Vec<&str> = name.split_whitespace().collect();
    if parts.len() >= 2 {
        let last = parts.last().unwrap().to_string();
        if last.len() > 1 && last != name {
            results.push(last);
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_case_fold() {
        assert_eq!(canonicalize_entity("Alice"), vec!["alice"]);
    }

    #[test]
    fn honorific_stripping() {
        assert_eq!(
            canonicalize_entity("Dr. Sarah Martinez"),
            vec!["sarah martinez", "martinez"]
        );
    }

    #[test]
    fn possessive_stripping() {
        assert_eq!(canonicalize_entity("Alice's"), vec!["alice"]);
    }

    #[test]
    fn multi_word_last_name() {
        assert_eq!(
            canonicalize_entity("John Smith"),
            vec!["john smith", "smith"]
        );
    }

    #[test]
    fn single_word_no_last_name() {
        assert_eq!(canonicalize_entity("Alice"), vec!["alice"]);
    }

    #[test]
    fn whitespace_collapse() {
        assert_eq!(
            canonicalize_entity("  Dr.   Sarah   Martinez  "),
            vec!["sarah martinez", "martinez"]
        );
    }

    #[test]
    fn empty_input() {
        assert!(canonicalize_entity("").is_empty());
        assert!(canonicalize_entity("   ").is_empty());
    }

    #[test]
    fn full_word_honorifics() {
        assert_eq!(
            canonicalize_entity("Professor Dumbledore"),
            vec!["dumbledore"]
        );
        assert_eq!(canonicalize_entity("Reverend Smith"), vec!["smith"]);
        assert_eq!(canonicalize_entity("General Patton"), vec!["patton"]);
        assert_eq!(canonicalize_entity("Colonel Sanders"), vec!["sanders"]);
        assert_eq!(canonicalize_entity("Sergeant York"), vec!["york"]);
    }
}

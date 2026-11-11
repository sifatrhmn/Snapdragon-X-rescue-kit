//! Typed confirmation for destructive operations.

use diskcore::Partition;

/// The prompt shown above the confirmation box.
pub const CONFIRM_PROMPT: &str =
    "Type the partition's label or drive letter to confirm. This cannot be undone.";

/// Prompt for a partition that has neither label nor letter, so only `DELETE` works.
pub const CONFIRM_PROMPT_UNNAMED: &str = "This partition has no label or drive letter. \
Type DELETE to confirm. This cannot be undone.";

/// What the user must type for `p`: its label or drive letter, or `DELETE` if it has neither.
pub fn confirm_prompt(p: &Partition) -> &'static str {
    if p.label.trim().is_empty() && p.letter.is_none() {
        CONFIRM_PROMPT_UNNAMED
    } else {
        CONFIRM_PROMPT
    }
}

/// True when `typed` confirms `p`: its label (case-insensitive, trimmed) or its drive
/// letter (`D` or `D:`). A partition with neither label nor letter needs `DELETE`.
pub fn confirmation_matches(p: &Partition, typed: &str) -> bool {
    let typed = typed.trim();
    let label = p.label.trim();
    if label.is_empty() && p.letter.is_none() {
        return typed == "DELETE";
    }
    if typed.is_empty() {
        return false;
    }
    if !label.is_empty() && typed.to_lowercase() == label.to_lowercase() {
        return true;
    }
    match p.letter {
        Some(l) => {
            let l = l.to_ascii_uppercase().to_string();
            let t = typed.to_uppercase();
            t == l || t == format!("{l}:")
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskcore::{FsKind, PartKind, Protection};

    fn part(label: &str, letter: Option<char>) -> Partition {
        Partition {
            number: 3,
            offset: 1 << 20,
            size: 1 << 30,
            kind: PartKind::Basic,
            letter,
            label: label.to_string(),
            fs: FsKind::Ntfs,
            used: None,
            protection: Protection::None,
            supported: None,
            volume_path: None,
            has_windows: false,
        }
    }

    #[test]
    fn label_matches_ignoring_case_and_padding() {
        let p = part("Data", None);
        assert!(confirmation_matches(&p, " data "));
        assert!(confirmation_matches(&p, "DATA"));
        assert!(!confirmation_matches(&p, "Dat"));
        assert!(!confirmation_matches(&p, ""));
        assert!(!confirmation_matches(&p, "DELETE"));
    }

    #[test]
    fn letter_matches_with_or_without_colon() {
        let p = part("", Some('D'));
        assert!(confirmation_matches(&p, "d:"));
        assert!(confirmation_matches(&p, " D "));
        assert!(confirmation_matches(&p, "D:"));
        assert!(!confirmation_matches(&p, "E:"));
        assert!(!confirmation_matches(&p, "DD"));
        assert!(!confirmation_matches(&p, ""));
        assert!(!confirmation_matches(&p, "DELETE"));
    }

    #[test]
    fn label_or_letter_either_confirms() {
        let p = part("Data", Some('D'));
        assert!(confirmation_matches(&p, "data"));
        assert!(confirmation_matches(&p, "d"));
        assert!(!confirmation_matches(&p, "DELETE"));
    }

    #[test]
    fn unnamed_partition_accepts_only_delete() {
        let p = part("", None);
        assert!(confirmation_matches(&p, "DELETE"));
        assert!(confirmation_matches(&p, "  DELETE "));
        assert!(!confirmation_matches(&p, "delete"));
        assert!(!confirmation_matches(&p, ""));
        assert!(!confirmation_matches(&p, "D"));
        assert!(!confirmation_matches(&p, "3"));
    }

    #[test]
    fn whitespace_only_label_counts_as_no_label() {
        let p = part("   ", None);
        assert!(confirmation_matches(&p, "DELETE"));
        assert!(!confirmation_matches(&p, ""));
        assert!(!confirmation_matches(&p, "  "));
    }

    #[test]
    fn prompt_matches_what_is_required() {
        assert_eq!(confirm_prompt(&part("Data", None)), CONFIRM_PROMPT);
        assert_eq!(confirm_prompt(&part("", Some('D'))), CONFIRM_PROMPT);
        assert_eq!(confirm_prompt(&part("", None)), CONFIRM_PROMPT_UNNAMED);
        assert_eq!(
            CONFIRM_PROMPT,
            "Type the partition's label or drive letter to confirm. This cannot be undone."
        );
    }
}

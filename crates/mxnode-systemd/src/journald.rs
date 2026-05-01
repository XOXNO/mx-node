//! Idempotent management of `/etc/systemd/journald.conf` so node logs
//! don't fill `/var/log/journal`. Mirrors the bash `prerequisites` flow
//! (caps `SystemMaxUse=2G`, `SystemMaxFileSize=200M`).
//!
//! We never overwrite operator-set values outside our managed block;
//! the block is delimited by sentinel comments and re-applying is a no-op.

const BEGIN: &str = "# >>> mxnode journald managed block >>>";
const END: &str = "# <<< mxnode journald managed block <<<";

/// Recommended values, lifted from the bash `prerequisites` recipe and
/// MultiversX validator host docs.
pub const DEFAULT_SYSTEM_MAX_USE: &str = "2G";
pub const DEFAULT_SYSTEM_MAX_FILE_SIZE: &str = "200M";

/// Compute the journald.conf body after applying mxnode's managed block.
/// Pure function: returns the new file contents. Caller decides whether
/// to write (typically via `sudo tee`) — see Task C2.
///
/// Idempotent: re-applying with the same `max_use` / `max_file_size`
/// returns a byte-identical result.
pub fn apply_managed_block(existing: &str, max_use: &str, max_file_size: &str) -> String {
    let block = format!(
        "{BEGIN}\n[Journal]\nSystemMaxUse={max_use}\nSystemMaxFileSize={max_file_size}\n{END}\n"
    );
    if let (Some(start), Some(end)) = (existing.find(BEGIN), existing.find(END)) {
        // Replace existing block. `end` points to the opening of END;
        // include the END line + trailing newline.
        let end_full = end + END.len();
        let after = &existing[end_full..];
        let after = after.strip_prefix('\n').unwrap_or(after);
        let mut out = String::new();
        out.push_str(&existing[..start]);
        out.push_str(&block);
        out.push_str(after);
        out
    } else {
        let mut out = existing.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(&block);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_block_to_empty_file() {
        let out = apply_managed_block("", "2G", "200M");
        assert!(out.contains("[Journal]"));
        assert!(out.contains("SystemMaxUse=2G"));
        assert!(out.contains("SystemMaxFileSize=200M"));
        assert!(out.starts_with('\n'));
    }

    #[test]
    fn appends_to_existing_file_without_clobbering_operator_lines() {
        let existing = "[Journal]\nForwardToSyslog=yes\n";
        let out = apply_managed_block(existing, "2G", "200M");
        assert!(out.contains("ForwardToSyslog=yes"));
        assert!(out.contains("SystemMaxUse=2G"));
    }

    #[test]
    fn replaces_existing_managed_block_idempotently() {
        let initial = apply_managed_block("", "1G", "100M");
        let updated = apply_managed_block(&initial, "2G", "200M");
        assert!(!updated.contains("SystemMaxUse=1G"));
        assert!(updated.contains("SystemMaxUse=2G"));
        // Re-applying with the same values is a no-op.
        let again = apply_managed_block(&updated, "2G", "200M");
        assert_eq!(again, updated);
    }

    #[test]
    fn round_trip_preserves_unrelated_sections() {
        let existing = "[Foo]\nBar=baz\n";
        let after_first = apply_managed_block(existing, "2G", "200M");
        let after_second = apply_managed_block(&after_first, "2G", "200M");
        assert!(after_second.contains("[Foo]"));
        assert!(after_second.contains("Bar=baz"));
        assert_eq!(after_first, after_second);
    }
}

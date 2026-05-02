//! Interactive prompts for `install` / `add-nodes`. Kept in one place so
//! both call sites have identical UX (template-expanded default, Enter
//! accepts, blank input falls through, EOF / non-TTY skips silently).
//!
//! TTY detection lives at the call site (the CLI flag `--non-interactive`
//! plus `std::io::IsTerminal` on stdin) so this module stays test-friendly:
//! every entry point takes `Read + Write` plus an `interactive: bool`
//! switch.

use std::io::{BufRead, Write};

use mxnode_core::Environment;

/// Prompt the operator for the MultiversX network on first-time install.
///
/// `interactive == false` (non-TTY, `--json`, or operator opt-out) yields
/// `Environment::Mainnet` silently — the same default the auto-init has
/// always used so CI/automation paths behave identically to before. When
/// interactive, accept either the digit shortcut (`1`/`2`/`3`), the
/// literal name (`mainnet`/`testnet`/`devnet`, case-insensitive), or a
/// blank line for the default.
pub fn prompt_for_network<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    interactive: bool,
) -> std::io::Result<Environment> {
    if !interactive {
        return Ok(Environment::Mainnet);
    }
    writeln!(writer)?;
    writeln!(writer, "Select the MultiversX network for this install:")?;
    writeln!(writer, "  1) mainnet  (default)")?;
    writeln!(writer, "  2) testnet")?;
    writeln!(writer, "  3) devnet")?;
    write!(writer, "  network [1]: ")?;
    writer.flush()?;
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        // EOF — accept the default rather than fail the bare-bones first
        // command an operator runs on a fresh box.
        return Ok(Environment::Mainnet);
    }
    let answer = line.trim().to_ascii_lowercase();
    Ok(match answer.as_str() {
        "" | "1" | "mainnet" => Environment::Mainnet,
        "2" | "testnet" => Environment::Testnet,
        "3" | "devnet" => Environment::Devnet,
        other => {
            // Unrecognised input falls back to mainnet so a misread does
            // not fail the operator's first command. The warning is
            // visible enough that they can `mxnode config set
            // network.environment <env>` immediately if needed.
            writeln!(writer, "  unrecognised choice {other:?}; using mainnet")?;
            Environment::Mainnet
        }
    })
}

/// Substitute `{env}` and `{index}` in a name template, matching the
/// expansion done by the install orchestrator. Pure helper so prompts
/// and the orchestrator agree on what "the default" is byte-for-byte.
pub fn expand_template(template: &str, env: &str, index: u16) -> String {
    template
        .replace("{env}", env)
        .replace("{index}", &index.to_string())
}

/// Resolve per-node display names.
///
/// When `interactive` is true, prompt for each node with the
/// template-expanded default and accept either an explicit value or a
/// blank line (default). When `interactive` is false, expand silently.
/// EOF mid-prompt is treated as "accept all remaining defaults" so a
/// piped-stdin invocation still completes.
pub fn resolve_node_names<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    count: u16,
    indices: &[u16],
    template: &str,
    env: &str,
    interactive: bool,
) -> std::io::Result<Vec<String>> {
    debug_assert_eq!(
        indices.len(),
        count as usize,
        "indices length must match count",
    );
    if !interactive {
        return Ok(indices
            .iter()
            .map(|i| expand_template(template, env, *i))
            .collect());
    }

    writeln!(writer)?;
    writeln!(
        writer,
        "Choose a NodeDisplayName for each node (Enter accepts the default):"
    )?;

    let mut names: Vec<String> = Vec::with_capacity(count as usize);
    for (slot, idx) in indices.iter().enumerate() {
        let default = expand_template(template, env, *idx);
        write!(writer, "  node {idx} [{default}]: ")?;
        writer.flush()?;

        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            // EOF: accept defaults for the rest. Surface a hint so the
            // operator notices the input stream closed unexpectedly.
            writeln!(
                writer,
                "  (input closed; using defaults for the remaining {} node(s))",
                count as usize - slot
            )?;
            for j in &indices[slot..] {
                names.push(expand_template(template, env, *j));
            }
            return Ok(names);
        }
        let trimmed = line.trim();
        names.push(if trimmed.is_empty() {
            default
        } else {
            trimmed.to_string()
        });
    }
    writeln!(writer)?;
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn run(
        input: &str,
        count: u16,
        indices: &[u16],
        template: &str,
        interactive: bool,
    ) -> (Vec<String>, String) {
        let mut reader = Cursor::new(input.as_bytes().to_vec());
        let mut writer: Vec<u8> = Vec::new();
        let names = resolve_node_names(
            &mut reader,
            &mut writer,
            count,
            indices,
            template,
            "mainnet",
            interactive,
        )
        .expect("ok");
        (names, String::from_utf8(writer).expect("utf-8"))
    }

    #[test]
    fn non_interactive_expands_template_silently() {
        let (names, out) = run("", 3, &[0, 1, 2], "x-{env}-{index}", false);
        assert_eq!(names, vec!["x-mainnet-0", "x-mainnet-1", "x-mainnet-2"]);
        assert!(
            out.is_empty(),
            "non-interactive must not write anything: {out:?}"
        );
    }

    #[test]
    fn interactive_accepts_blank_line_as_default() {
        let (names, out) = run("\n\n\n", 3, &[0, 1, 2], "default-{index}", true);
        assert_eq!(names, vec!["default-0", "default-1", "default-2"]);
        assert!(out.contains("node 0 [default-0]"));
        assert!(out.contains("node 1 [default-1]"));
        assert!(out.contains("node 2 [default-2]"));
    }

    #[test]
    fn interactive_uses_typed_value_when_present() {
        let (names, _) = run(
            "custom-zero\n\ncustom-two\n",
            3,
            &[0, 1, 2],
            "default-{index}",
            true,
        );
        assert_eq!(names, vec!["custom-zero", "default-1", "custom-two"]);
    }

    #[test]
    fn interactive_trims_whitespace_around_typed_value() {
        let (names, _) = run("  spaced-value  \n", 1, &[0], "default-{index}", true);
        assert_eq!(names, vec!["spaced-value"]);
    }

    #[test]
    fn eof_mid_prompt_falls_back_to_defaults() {
        // Two node prompts queued, only one line of input → second falls back to default.
        let (names, out) = run("first\n", 2, &[0, 1], "x-{index}", true);
        assert_eq!(names, vec!["first", "x-1"]);
        assert!(
            out.contains("input closed"),
            "operator must see the EOF hint: {out:?}"
        );
    }

    #[test]
    fn network_prompt_non_interactive_returns_mainnet_silently() {
        let mut reader = Cursor::new(Vec::new());
        let mut writer: Vec<u8> = Vec::new();
        let env = prompt_for_network(&mut reader, &mut writer, false).unwrap();
        assert_eq!(env, Environment::Mainnet);
        assert!(writer.is_empty());
    }

    #[test]
    fn network_prompt_blank_line_picks_default() {
        let mut reader = Cursor::new(b"\n".to_vec());
        let mut writer: Vec<u8> = Vec::new();
        let env = prompt_for_network(&mut reader, &mut writer, true).unwrap();
        assert_eq!(env, Environment::Mainnet);
    }

    #[test]
    fn network_prompt_digit_shortcuts() {
        for (input, expected) in [
            ("1\n", Environment::Mainnet),
            ("2\n", Environment::Testnet),
            ("3\n", Environment::Devnet),
        ] {
            let mut reader = Cursor::new(input.as_bytes().to_vec());
            let mut writer: Vec<u8> = Vec::new();
            let env = prompt_for_network(&mut reader, &mut writer, true).unwrap();
            assert_eq!(env, expected, "input: {input:?}");
        }
    }

    #[test]
    fn network_prompt_named_values() {
        for (input, expected) in [
            ("mainnet\n", Environment::Mainnet),
            ("Testnet\n", Environment::Testnet),
            ("DEVNET\n", Environment::Devnet),
        ] {
            let mut reader = Cursor::new(input.as_bytes().to_vec());
            let mut writer: Vec<u8> = Vec::new();
            let env = prompt_for_network(&mut reader, &mut writer, true).unwrap();
            assert_eq!(env, expected, "input: {input:?}");
        }
    }

    #[test]
    fn network_prompt_unknown_falls_back_with_warning() {
        let mut reader = Cursor::new(b"hyperliquid\n".to_vec());
        let mut writer: Vec<u8> = Vec::new();
        let env = prompt_for_network(&mut reader, &mut writer, true).unwrap();
        assert_eq!(env, Environment::Mainnet);
        let out = String::from_utf8(writer).unwrap();
        assert!(
            out.contains("unrecognised"),
            "warning missing from prompt output: {out}",
        );
    }

    #[test]
    fn network_prompt_eof_falls_back_to_mainnet() {
        let mut reader = Cursor::new(Vec::new());
        let mut writer: Vec<u8> = Vec::new();
        let env = prompt_for_network(&mut reader, &mut writer, true).unwrap();
        assert_eq!(env, Environment::Mainnet);
    }

    #[test]
    fn add_nodes_indices_can_start_above_zero() {
        // add-nodes appends to existing nodes; the index list reflects the
        // first free slot rather than 0.
        let (names, out) = run("\n\n", 2, &[7, 8], "later-{index}", true);
        assert_eq!(names, vec!["later-7", "later-8"]);
        assert!(out.contains("node 7"));
        assert!(out.contains("node 8"));
    }
}

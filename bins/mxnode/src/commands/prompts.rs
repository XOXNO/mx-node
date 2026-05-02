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

/// Operator's chosen install shape, picked at the top of the wizard.
/// Maps cleanly onto `InstallArgs.role` + `squad` + `with_proxy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallType {
    /// Free-count validator nodes, no shard pinning. Operator supplies
    /// `node-{i}.zip` per node from `paths.node_keys`.
    Validators,
    /// Free-count observer nodes, no shard pinning. mx-chain-go
    /// auto-generates a throwaway BLS key on first start.
    Observers,
    /// Four observers pinned to shards 0/1/2/metachain, with a local
    /// MultiversX proxy on the same host. Mirrors bash `observing_squad`.
    ObserversSquad,
    /// Four multikey nodes signing for an operator-supplied
    /// `allValidatorsKeys.pem` bundle. Mirrors bash `multikey_squad`.
    MultikeySquad,
}

/// Prompt the operator for the install type.
pub fn prompt_for_install_type<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    interactive: bool,
) -> std::io::Result<InstallType> {
    if !interactive {
        return Ok(InstallType::Validators);
    }
    writeln!(writer)?;
    writeln!(writer, "What kind of install?")?;
    writeln!(
        writer,
        "  1) Validators        (you supply node-{{i}}.zip per node)",
    )?;
    writeln!(
        writer,
        "  2) Observers         (free count; throwaway BLS key auto-generated)",
    )?;
    writeln!(
        writer,
        "  3) Observers squad   (4 observers pinned to shards 0/1/2/metachain + proxy)",
    )?;
    writeln!(
        writer,
        "  4) Multikey squad    (4 nodes signing allValidatorsKeys.pem)",
    )?;
    write!(writer, "  type [1]: ")?;
    writer.flush()?;
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(InstallType::Validators);
    }
    Ok(match line.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "validator" | "validators" => InstallType::Validators,
        "2" | "observer" | "observers" => InstallType::Observers,
        "3" | "squad" | "observers squad" | "observer squad" => InstallType::ObserversSquad,
        "4" | "multikey" | "multikey squad" => InstallType::MultikeySquad,
        other => {
            writeln!(writer, "  unrecognised choice {other:?}; using validators")?;
            InstallType::Validators
        }
    })
}

/// Prompt for a node count with a numeric default. Rejects 0 and any
/// non-numeric input by reverting to `default` with a stderr-style
/// hint, so a misread never blocks the wizard.
pub fn prompt_for_count<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    default: u16,
    interactive: bool,
) -> std::io::Result<u16> {
    if !interactive {
        return Ok(default);
    }
    write!(writer, "Number of nodes [{default}]: ")?;
    writer.flush()?;
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(default);
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(default);
    }
    match trimmed.parse::<u16>() {
        Ok(n) if n > 0 => Ok(n),
        _ => {
            writeln!(writer, "  invalid count {trimmed:?}; using {default}")?;
            Ok(default)
        }
    }
}

/// Prompt for `Preferences.RedundancyLevel`. `0` = primary (default),
/// `1+` = backup level. Used for both multikey and (post-relaxation)
/// validator installs. Bad input falls back to 0 with a hint.
pub fn prompt_for_redundancy<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    interactive: bool,
) -> std::io::Result<u8> {
    if !interactive {
        return Ok(0);
    }
    writeln!(writer)?;
    writeln!(writer, "RedundancyLevel for backup nodes:")?;
    writeln!(writer, "  0  primary (default)")?;
    writeln!(
        writer,
        "  1+ backup level (1 = first backup, 2 = backup-of-backup, …)"
    )?;
    write!(writer, "  redundancy [0]: ")?;
    writer.flush()?;
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(0);
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(0);
    }
    match trimmed.parse::<u8>() {
        Ok(n) => Ok(n),
        _ => {
            writeln!(writer, "  invalid redundancy {trimmed:?}; using 0")?;
            Ok(0)
        }
    }
}

/// Substitute `{env}` and `{index}` in a name template, matching the
/// expansion done by the install orchestrator. Pure helper so prompts
/// and the orchestrator agree on what "the default" is byte-for-byte.
pub fn expand_template(template: &str, env: &str, index: u16) -> String {
    template
        .replace("{env}", env)
        .replace("{index}", &index.to_string())
}

/// Resolve the display name to show for one node. Used by every
/// surface that renders a node label (`reapply-config`, `dashboard`,
/// `status`, the install/add-nodes success output) so they all agree
/// on what "the operator's chosen name" is.
///
/// Precedence:
///   1. The name persisted on the `NodeState` (stamped at install
///      time, kept in sync by `mxnode rename`). Honouring this stops
///      every read-side surface from silently re-templating an
///      operator's chosen name just because the config-side
///      `node.name_template` happens to differ.
///   2. The current `node.name_template`, with `{env}` / `{index}`
///      substituted. Only used when the persisted name is empty
///      (legacy installs imported via `migrate-bash`, or installs
///      from mxnode versions that predated the persisted-name field).
///   3. Empty string when neither source has a value — callers fall
///      back to a unit-level label like `node-{index}`.
pub fn resolve_display_name(persisted: &str, template: &str, env: &str, index: u16) -> String {
    if !persisted.is_empty() {
        return persisted.to_string();
    }
    if template.is_empty() {
        return String::new();
    }
    expand_template(template, env, index)
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
    fn resolve_display_name_prefers_persisted() {
        let out = resolve_display_name(
            "my-validator-prod",
            "mx-chain-{env}-validator-{index}",
            "mainnet",
            0,
        );
        assert_eq!(out, "my-validator-prod");
    }

    #[test]
    fn resolve_display_name_falls_back_to_template() {
        let out = resolve_display_name("", "mx-chain-{env}-validator-{index}", "mainnet", 3);
        assert_eq!(out, "mx-chain-mainnet-validator-3");
    }

    #[test]
    fn resolve_display_name_empty_when_neither_source_set() {
        assert_eq!(resolve_display_name("", "", "mainnet", 0), "");
    }

    #[test]
    fn install_type_prompt_non_interactive_returns_validators() {
        let mut reader = Cursor::new(Vec::new());
        let mut writer: Vec<u8> = Vec::new();
        let t = prompt_for_install_type(&mut reader, &mut writer, false).unwrap();
        assert_eq!(t, InstallType::Validators);
        assert!(writer.is_empty());
    }

    #[test]
    fn install_type_prompt_digit_shortcuts() {
        for (input, expected) in [
            ("1\n", InstallType::Validators),
            ("2\n", InstallType::Observers),
            ("3\n", InstallType::ObserversSquad),
            ("4\n", InstallType::MultikeySquad),
        ] {
            let mut reader = Cursor::new(input.as_bytes().to_vec());
            let mut writer: Vec<u8> = Vec::new();
            let t = prompt_for_install_type(&mut reader, &mut writer, true).unwrap();
            assert_eq!(t, expected, "input: {input:?}");
        }
    }

    #[test]
    fn install_type_prompt_blank_picks_validators() {
        let mut reader = Cursor::new(b"\n".to_vec());
        let mut writer: Vec<u8> = Vec::new();
        let t = prompt_for_install_type(&mut reader, &mut writer, true).unwrap();
        assert_eq!(t, InstallType::Validators);
    }

    #[test]
    fn install_type_prompt_named_values() {
        for (input, expected) in [
            ("validators\n", InstallType::Validators),
            ("Observers\n", InstallType::Observers),
            ("squad\n", InstallType::ObserversSquad),
            ("MULTIKEY\n", InstallType::MultikeySquad),
        ] {
            let mut reader = Cursor::new(input.as_bytes().to_vec());
            let mut writer: Vec<u8> = Vec::new();
            let t = prompt_for_install_type(&mut reader, &mut writer, true).unwrap();
            assert_eq!(t, expected, "input: {input:?}");
        }
    }

    #[test]
    fn count_prompt_blank_returns_default() {
        let mut reader = Cursor::new(b"\n".to_vec());
        let mut writer: Vec<u8> = Vec::new();
        let n = prompt_for_count(&mut reader, &mut writer, 1, true).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn count_prompt_parses_valid_input() {
        let mut reader = Cursor::new(b"7\n".to_vec());
        let mut writer: Vec<u8> = Vec::new();
        let n = prompt_for_count(&mut reader, &mut writer, 1, true).unwrap();
        assert_eq!(n, 7);
    }

    #[test]
    fn count_prompt_zero_falls_back_with_warning() {
        let mut reader = Cursor::new(b"0\n".to_vec());
        let mut writer: Vec<u8> = Vec::new();
        let n = prompt_for_count(&mut reader, &mut writer, 4, true).unwrap();
        assert_eq!(n, 4);
        let out = String::from_utf8(writer).unwrap();
        assert!(out.contains("invalid count"));
    }

    #[test]
    fn count_prompt_garbage_falls_back() {
        let mut reader = Cursor::new(b"abc\n".to_vec());
        let mut writer: Vec<u8> = Vec::new();
        let n = prompt_for_count(&mut reader, &mut writer, 2, true).unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn redundancy_prompt_blank_returns_zero() {
        let mut reader = Cursor::new(b"\n".to_vec());
        let mut writer: Vec<u8> = Vec::new();
        let n = prompt_for_redundancy(&mut reader, &mut writer, true).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn redundancy_prompt_parses_valid_input() {
        let mut reader = Cursor::new(b"3\n".to_vec());
        let mut writer: Vec<u8> = Vec::new();
        let n = prompt_for_redundancy(&mut reader, &mut writer, true).unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn redundancy_prompt_garbage_falls_back_to_zero() {
        let mut reader = Cursor::new(b"definitely-not-a-number\n".to_vec());
        let mut writer: Vec<u8> = Vec::new();
        let n = prompt_for_redundancy(&mut reader, &mut writer, true).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn redundancy_prompt_non_interactive_returns_zero() {
        let mut reader = Cursor::new(Vec::new());
        let mut writer: Vec<u8> = Vec::new();
        let n = prompt_for_redundancy(&mut reader, &mut writer, false).unwrap();
        assert_eq!(n, 0);
        assert!(writer.is_empty());
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

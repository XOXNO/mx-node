//! Resolve `LifecycleArgs` against the live `HostState` to a list of nodes the
//! caller is targeting.
//!
//! The selector forms (mutually exclusive at parse time via `clap::ArgGroup`):
//!   - `--all` → every node
//!   - `--validators-only` → role == validator
//!   - `--observers-only` → role == observer
//!   - `--shard <s>` → shard == parsed value
//!   - `--node N --node M` → explicit indices
//!   - `--select 'role=validator,shard=0|shard=1'` → expression form
//!
//! When no selector is supplied, the caller decides whether to refuse or
//! default to "all" — selectors here are pure parsing/filtering, not policy.

use mxnode_core::{NodeIndex, NodeState, Role, Shard, HostState};

use crate::cli::LifecycleArgs;

/// Outcome of selector resolution.
///
/// `Ok(Vec<NodeIndex>)` is the list of indices the caller should act on,
/// in stable ascending order. `Err(SelectorError)` produces a structured
/// failure with operator-actionable text.
#[derive(Debug, PartialEq, Eq)]
pub enum SelectorError {
    /// `--node N` was passed but no node with that index exists.
    NodeMissing { missing: Vec<u16> },
    /// Selector syntax inside `--select` was malformed.
    BadExpression(String),
    /// HostState has zero nodes — running a lifecycle command makes no sense.
    EmptyState,
}

impl std::fmt::Display for SelectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SelectorError::NodeMissing { missing } => write!(
                f,
                "no node(s) at index {missing:?}; run `mxnode status` to list available indices",
            ),
            SelectorError::BadExpression(s) => {
                write!(f, "could not parse --select expression: {s}")
            }
            SelectorError::EmptyState => {
                write!(f, "mxnode.toml has no nodes; run `mxnode install` first",)
            }
        }
    }
}

/// Resolve `LifecycleArgs` to a list of node indices. Stable ascending
/// order. A bare invocation (no `--all` / `--node` / `--shard` / role
/// flag / `--select`) is treated as "every node" — all current
/// production callers (`start` / `stop` / `restart` / upgrade) want
/// that semantic. Add a `default: DefaultSelection` parameter back
/// here when a destructive command needs the opposite behaviour.
pub fn resolve(state: &HostState, args: &LifecycleArgs) -> Result<Vec<NodeIndex>, SelectorError> {
    if state.nodes.is_empty() {
        return Err(SelectorError::EmptyState);
    }

    // `--all` always wins when set.
    if args.all {
        return Ok(sorted_indices(state.nodes.iter()));
    }

    if !args.node.is_empty() {
        let supplied: Vec<u16> = args.node.clone();
        let known: Vec<u16> = state.nodes.iter().map(|n| n.index.get()).collect();
        let missing: Vec<u16> = supplied
            .iter()
            .copied()
            .filter(|i| !known.contains(i))
            .collect();
        if !missing.is_empty() {
            return Err(SelectorError::NodeMissing { missing });
        }
        let mut out: Vec<NodeIndex> = supplied.into_iter().map(NodeIndex::new).collect();
        out.sort();
        out.dedup();
        return Ok(out);
    }

    if args.validators_only {
        return Ok(sorted_indices(
            state.nodes.iter().filter(|n| n.role == Role::Validator),
        ));
    }
    if args.observers_only {
        return Ok(sorted_indices(
            state.nodes.iter().filter(|n| n.role == Role::Observer),
        ));
    }
    if let Some(shard_str) = &args.shard {
        let parsed: Shard = shard_str
            .parse()
            .map_err(|_| SelectorError::BadExpression(format!("invalid shard {shard_str:?}")))?;
        return Ok(sorted_indices(
            state.nodes.iter().filter(|n| n.shard == parsed),
        ));
    }

    if let Some(expr) = &args.select {
        return resolve_expression(state, expr);
    }

    Ok(sorted_indices(state.nodes.iter()))
}

fn sorted_indices<'a>(iter: impl Iterator<Item = &'a NodeState>) -> Vec<NodeIndex> {
    let mut v: Vec<NodeIndex> = iter.map(|n| n.index).collect();
    v.sort();
    v.dedup();
    v
}

/// Parse a `--select` expression. Grammar:
///
/// ```text
/// expr     := clause ('|' clause)*    # OR over clauses
/// clause   := atom (',' atom)*        # AND over atoms
/// atom     := key '=' value
/// key      := role | shard | index
/// value    := for role: validator|observer|multikey
///             for shard: 0|1|2|metachain|disabled|auto
///             for index: integer (or comma-separated list)
/// ```
fn resolve_expression(state: &HostState, expr: &str) -> Result<Vec<NodeIndex>, SelectorError> {
    let mut matched: Vec<NodeIndex> = Vec::new();
    for clause in expr.split('|').map(str::trim).filter(|s| !s.is_empty()) {
        let preds = parse_clause(clause)?;
        for node in &state.nodes {
            if preds.iter().all(|p| p.matches(node)) && !matched.contains(&node.index) {
                matched.push(node.index);
            }
        }
    }
    matched.sort();
    Ok(matched)
}

#[derive(Debug)]
enum Predicate {
    Role(Role),
    Shard(Shard),
    Index(u16),
}

impl Predicate {
    fn matches(&self, node: &NodeState) -> bool {
        match self {
            Predicate::Role(r) => node.role == *r,
            Predicate::Shard(s) => node.shard == *s,
            Predicate::Index(i) => node.index.get() == *i,
        }
    }
}

fn parse_clause(clause: &str) -> Result<Vec<Predicate>, SelectorError> {
    let mut out: Vec<Predicate> = Vec::new();
    for atom in clause.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (key, value) = atom
            .split_once('=')
            .ok_or_else(|| SelectorError::BadExpression(format!("missing '=' in atom {atom:?}")))?;
        let key = key.trim();
        let value = value.trim();
        let pred =
            match key {
                "role" => Predicate::Role(value.parse().map_err(|_| {
                    SelectorError::BadExpression(format!("invalid role {value:?}"))
                })?),
                "shard" => Predicate::Shard(value.parse().map_err(|_| {
                    SelectorError::BadExpression(format!("invalid shard {value:?}"))
                })?),
                "index" => Predicate::Index(value.parse().map_err(|_| {
                    SelectorError::BadExpression(format!("invalid index {value:?}"))
                })?),
                other => {
                    return Err(SelectorError::BadExpression(format!(
                        "unknown selector key {other:?}"
                    )))
                }
            };
        out.push(pred);
    }
    if out.is_empty() {
        return Err(SelectorError::BadExpression("empty clause".to_string()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mxnode_core::{NodeState, Role, Shard, HostState};

    fn node(idx: u16, role: Role, shard: Shard) -> NodeState {
        NodeState {
            index: NodeIndex::new(idx),
            role,
            shard,
            display_name: String::new(),
            api_port: 8080 + idx,
            unit: format!("elrond-node-{idx}.service"),
            unit_override: String::new(),
            workdir: std::path::PathBuf::from("/tmp"),
            last_known_pubkey: String::new(),
            last_action: String::new(),
            last_action_at: None,
        }
    }

    fn state_with_mixed_nodes() -> HostState {
        let mut s = HostState::empty("test");
        s.nodes = vec![
            node(0, Role::Validator, Shard::Zero),
            node(1, Role::Observer, Shard::One),
            node(2, Role::Validator, Shard::Metachain),
            node(3, Role::Observer, Shard::Two),
        ];
        s
    }

    fn args_default() -> LifecycleArgs {
        LifecycleArgs {
            all: false,
            select: None,
            validators_only: false,
            observers_only: false,
            shard: None,
            node: Vec::new(),
        }
    }

    #[test]
    fn empty_state_is_an_error() {
        let s = HostState::empty("test");
        let err = resolve(&s, &args_default()).unwrap_err();
        assert_eq!(err, SelectorError::EmptyState);
    }

    #[test]
    fn no_selector_returns_all_nodes() {
        let s = state_with_mixed_nodes();
        let v = resolve(&s, &args_default()).unwrap();
        assert_eq!(v.len(), 4);
    }

    #[test]
    fn all_flag_returns_every_node() {
        let s = state_with_mixed_nodes();
        let mut a = args_default();
        a.all = true;
        let v = resolve(&s, &a).unwrap();
        assert_eq!(
            v.iter().map(|i| i.get()).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn validators_only_filters_by_role() {
        let s = state_with_mixed_nodes();
        let mut a = args_default();
        a.validators_only = true;
        let v = resolve(&s, &a).unwrap();
        assert_eq!(v.iter().map(|i| i.get()).collect::<Vec<_>>(), vec![0, 2]);
    }

    #[test]
    fn observers_only_filters_by_role() {
        let s = state_with_mixed_nodes();
        let mut a = args_default();
        a.observers_only = true;
        let v = resolve(&s, &a).unwrap();
        assert_eq!(v.iter().map(|i| i.get()).collect::<Vec<_>>(), vec![1, 3]);
    }

    #[test]
    fn shard_filters_by_shard() {
        let s = state_with_mixed_nodes();
        let mut a = args_default();
        a.shard = Some("metachain".to_string());
        let v = resolve(&s, &a).unwrap();
        assert_eq!(v.iter().map(|i| i.get()).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn explicit_node_indices_resolve() {
        let s = state_with_mixed_nodes();
        let mut a = args_default();
        a.node = vec![1, 3, 1]; // duplicate is dedup'd
        let v = resolve(&s, &a).unwrap();
        assert_eq!(v.iter().map(|i| i.get()).collect::<Vec<_>>(), vec![1, 3]);
    }

    #[test]
    fn explicit_node_missing_errors_with_full_list() {
        let s = state_with_mixed_nodes();
        let mut a = args_default();
        a.node = vec![1, 99, 100];
        let err = resolve(&s, &a).unwrap_err();
        assert_eq!(
            err,
            SelectorError::NodeMissing {
                missing: vec![99, 100]
            },
        );
    }

    #[test]
    fn select_expression_and_clause() {
        let s = state_with_mixed_nodes();
        let mut a = args_default();
        a.select = Some("role=validator,shard=metachain".to_string());
        let v = resolve(&s, &a).unwrap();
        assert_eq!(v.iter().map(|i| i.get()).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn select_expression_or_clause() {
        let s = state_with_mixed_nodes();
        let mut a = args_default();
        a.select = Some("shard=0|shard=1".to_string());
        let v = resolve(&s, &a).unwrap();
        assert_eq!(v.iter().map(|i| i.get()).collect::<Vec<_>>(), vec![0, 1]);
    }

    #[test]
    fn select_expression_unknown_key_errors() {
        let s = state_with_mixed_nodes();
        let mut a = args_default();
        a.select = Some("nope=foo".to_string());
        let err = resolve(&s, &a).unwrap_err();
        match err {
            SelectorError::BadExpression(s) => assert!(s.contains("unknown selector key")),
            other => panic!("expected BadExpression, got {other:?}"),
        }
    }

    #[test]
    fn select_expression_index_atom() {
        let s = state_with_mixed_nodes();
        let mut a = args_default();
        a.select = Some("index=2".to_string());
        let v = resolve(&s, &a).unwrap();
        assert_eq!(v.iter().map(|i| i.get()).collect::<Vec<_>>(), vec![2]);
    }
}

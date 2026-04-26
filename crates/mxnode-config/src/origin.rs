use std::collections::BTreeMap;

/// Which layer supplied the value at a given dotted-path key.
///
/// Recorded once per leaf during `load` so `mxnode config show --origin` can
/// annotate each line with its provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Default,
    System,
    User,
    Explicit,
    Flag,
}

impl Origin {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::System => "system",
            Self::User => "user",
            Self::Explicit => "explicit",
            Self::Flag => "flag",
        }
    }
}

/// Dotted-path → origin map. `BTreeMap` so iteration is stable for diffs.
pub type OriginMap = BTreeMap<String, Origin>;

/// Recursively merge `src` into `dst`, recording each leaf with `origin`.
///
/// Tables are deep-merged; scalar/array values from `src` overwrite values in
/// `dst`. The `prefix` accumulates dotted-path keys for origin recording.
pub fn merge_with_origin(
    dst: &mut toml::Value,
    src: &toml::Value,
    prefix: &str,
    origin: Origin,
    out: &mut OriginMap,
) {
    if !(dst.is_table() && src.is_table()) {
        *dst = src.clone();
        record_leaf(src, prefix, origin, out);
        return;
    }

    let dst_table = dst.as_table_mut().expect("dst is_table checked above");
    let src_table = src.as_table().expect("src is_table checked above");

    for (k, src_v) in src_table.iter() {
        let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        let descend = matches!(dst_table.get(k), Some(v) if v.is_table()) && src_v.is_table();
        if descend {
            let existing = dst_table.get_mut(k).expect("existence checked");
            merge_with_origin(existing, src_v, &path, origin, out);
        } else {
            dst_table.insert(k.clone(), src_v.clone());
            record_leaf(src_v, &path, origin, out);
        }
    }
}

fn record_leaf(value: &toml::Value, prefix: &str, origin: Origin, out: &mut OriginMap) {
    match value {
        toml::Value::Table(t) => {
            for (k, v) in t.iter() {
                let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                record_leaf(v, &path, origin, out);
            }
        }
        _ => {
            out.insert(prefix.to_string(), origin);
        }
    }
}

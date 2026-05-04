//! Apply a `Combo` to a workspace `Cargo.toml`, returning the new
//! TOML body as a string. Uses `toml_edit` so existing formatting,
//! comments, and key ordering are preserved.

use anyhow::{Context, Result};
use toml_edit::{value, DocumentMut, Item, Table};

use crate::matrix::Combo;

pub fn apply_combo(input: &str, combo: &Combo) -> Result<String> {
    let mut doc: DocumentMut = input.parse().with_context(|| "parse Cargo.toml")?;

    let release = ensure_table_path(&mut doc, &["profile", "release"])?;
    release.insert("lto", value(combo.profile.lto.clone()));
    if combo.profile.opt_level == "3" {
        // Default; do not write to keep the diff small.
        release.remove("opt-level");
    } else {
        release.insert("opt-level", value(combo.profile.opt_level.clone()));
    }
    release.insert("strip", value(combo.profile.strip.clone()));
    release.insert("codegen-units", value(combo.profile.codegen_units as i64));
    release.insert("panic", value(combo.profile.panic.clone()));

    Ok(doc.to_string())
}

fn ensure_table_path<'a>(doc: &'a mut DocumentMut, path: &[&str]) -> Result<&'a mut Table> {
    let mut current: &mut Item = doc.as_item_mut();
    for segment in path {
        let table = current
            .as_table_mut()
            .with_context(|| format!("{segment} is not a table"))?;
        table
            .entry(segment)
            .or_insert_with(|| Item::Table(Table::new()));
        current = &mut table[segment];
    }
    current
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("final segment is not a table"))
}

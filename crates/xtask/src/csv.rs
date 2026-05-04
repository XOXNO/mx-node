//! Append-only RFC-4180 CSV writer.
//!
//! Why hand-rolled and not the `csv` crate: the harness needs a tiny,
//! dependency-light writer that can append safely across xtask
//! invocations and keep header parity. Round-trip tests assert
//! string-equal output so any divergence from the spec is caught
//! immediately.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use anyhow::{anyhow, Context, Result};

/// One row of the CSV. Field order is determined by the writer's
/// header at open time — pass any subset of header columns; missing
/// fields write as empty strings.
pub struct Row {
    fields: BTreeMap<String, String>,
}

impl<K: AsRef<str>, V: AsRef<str>, const N: usize> From<[(K, V); N]> for Row {
    fn from(items: [(K, V); N]) -> Self {
        let mut fields = BTreeMap::new();
        for (k, v) in items {
            fields.insert(k.as_ref().to_string(), v.as_ref().to_string());
        }
        Self { fields }
    }
}

impl Row {
    pub fn new() -> Self {
        Self {
            fields: BTreeMap::new(),
        }
    }

    pub fn set(&mut self, key: impl AsRef<str>, value: impl AsRef<str>) -> &mut Self {
        self.fields
            .insert(key.as_ref().to_string(), value.as_ref().to_string());
        self
    }
}

impl Default for Row {
    fn default() -> Self {
        Self::new()
    }
}

/// Append-only writer. Opening a path that already exists with a
/// matching first-line header reuses the file (append mode); a fresh
/// path writes the header line first.
pub struct Writer {
    out: BufWriter<File>,
    header: Vec<String>,
}

impl Writer {
    pub fn open(path: &Path, header: &[String]) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create parent dir for {}", path.display()))?;
        }

        let header_owned: Vec<String> = header.to_vec();
        let exists = path.exists();
        if exists {
            // Verify the existing first line matches the requested header
            // exactly. If not, refuse — the caller has changed the schema
            // and we won't silently corrupt history.
            let f = File::open(path)
                .with_context(|| format!("open {} for header check", path.display()))?;
            let mut first = String::new();
            BufReader::new(f).read_line(&mut first)?;
            let on_disk = first.trim_end_matches('\n');
            let expected = header_owned.join(",");
            if on_disk != expected {
                return Err(anyhow!(
                    "csv header mismatch: on-disk={on_disk:?}, expected={expected:?}"
                ));
            }
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open {} for append", path.display()))?;
        let mut out = BufWriter::new(file);

        if !exists {
            writeln!(out, "{}", header_owned.join(","))?;
        }

        Ok(Self {
            out,
            header: header_owned,
        })
    }

    pub fn append(&mut self, row: Row) -> Result<()> {
        let mut first = true;
        for col in &self.header {
            if !first {
                self.out.write_all(b",")?;
            }
            first = false;
            let raw = row.fields.get(col).map(String::as_str).unwrap_or("");
            self.out.write_all(escape(raw).as_bytes())?;
        }
        self.out.write_all(b"\n")?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

/// Quote per RFC 4180: any field containing comma, quote, CR, or LF gets
/// wrapped in double-quotes and inner quotes are doubled.
fn escape(field: &str) -> String {
    let needs_quoting = field
        .bytes()
        .any(|b| matches!(b, b',' | b'"' | b'\n' | b'\r'));
    if !needs_quoting {
        return field.to_string();
    }
    let mut out = String::with_capacity(field.len() + 2);
    out.push('"');
    for ch in field.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

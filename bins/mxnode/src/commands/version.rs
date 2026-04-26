use crate::errors::CliError;

/// `mxnode version` — also runnable via `--version`. Emits the same shape in
/// both human and JSON modes, matching D10 (universal --json from v0.1).
pub fn run(json: bool) -> Result<(), CliError> {
    let name = env!("CARGO_PKG_NAME");
    let version = env!("CARGO_PKG_VERSION");
    let schema = mxnode_core::SCHEMA_VERSION;

    if json {
        let payload = serde_json::json!({
            "name": name,
            "version": version,
            "schema_version": schema,
        });
        println!("{}", payload);
    } else {
        println!("{name} {version} (schema_version {schema})");
    }
    Ok(())
}

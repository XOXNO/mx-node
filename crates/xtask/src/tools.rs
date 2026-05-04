//! Host tool detection. Missing tools downgrade the affected
//! measurement (CSV row gets `tool_missing=<name>`) rather than
//! aborting the whole run — it's normal to bench-size from a fresh
//! macOS box without `upx` installed yet.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy)]
pub enum Tool {
    Hyperfine,
    Upx,
    Zstd,
    Xz,
    CargoMachete,
    CargoZigbuild,
    Zig,
}

impl Tool {
    pub fn binary(self) -> &'static str {
        match self {
            Tool::Hyperfine => "hyperfine",
            Tool::Upx => "upx",
            Tool::Zstd => "zstd",
            Tool::Xz => "xz",
            Tool::CargoMachete => "cargo-machete",
            Tool::CargoZigbuild => "cargo-zigbuild",
            Tool::Zig => "zig",
        }
    }

    pub fn install_hint(self) -> &'static str {
        match self {
            Tool::Hyperfine => "brew install hyperfine    # or: cargo install hyperfine",
            Tool::Upx => "brew install upx                # Linux: apt install upx-ucl",
            Tool::Zstd => "brew install zstd",
            Tool::Xz => "brew install xz",
            Tool::CargoMachete => "cargo install cargo-machete",
            Tool::CargoZigbuild => "cargo install --locked cargo-zigbuild",
            Tool::Zig => "brew install zig                # or: https://ziglang.org/download/",
        }
    }
}

pub fn check(tool: Tool) -> bool {
    which::which(tool.binary()).is_ok()
}

pub fn missing(tools: &[Tool]) -> Vec<Tool> {
    tools.iter().copied().filter(|t| !check(*t)).collect()
}

pub fn install_hint_table(tools: &[Tool]) -> String {
    let mut by_tool: BTreeMap<&str, &str> = BTreeMap::new();
    for t in tools {
        by_tool.insert(t.binary(), t.install_hint());
    }
    let mut out = String::new();
    for (binary, hint) in by_tool {
        out.push_str(&format!("  - {binary}: {hint}\n"));
    }
    out
}

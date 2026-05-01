use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// Target MultiversX network.
///
/// Matches the bash `ENVIRONMENT` knob and is used to select the correct
/// `mx-chain-{env}-config` repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    Mainnet,
    Testnet,
    Devnet,
}

impl Environment {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Testnet => "testnet",
            Self::Devnet => "devnet",
        }
    }

    /// `mx-chain-{env}-config` is the canonical config repo name on GitHub.
    pub fn config_repo(&self) -> String {
        format!("mx-chain-{}-config", self.as_str())
    }
}

impl fmt::Display for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Environment {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "mainnet" => Ok(Self::Mainnet),
            "testnet" => Ok(Self::Testnet),
            "devnet" => Ok(Self::Devnet),
            other => Err(Error::InvalidEnvironment(other.to_string())),
        }
    }
}

/// 0-based node index, matching the bash `INDEX` variable.
///
/// `u16` is overkill for any single host but cheap, and prevents accidental
/// confusion with shard ids (also `u16`-sized).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeIndex(pub u16);

impl NodeIndex {
    pub fn new(value: u16) -> Self {
        Self(value)
    }

    pub fn get(self) -> u16 {
        self.0
    }
}

impl fmt::Display for NodeIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for NodeIndex {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.trim()
            .parse::<u16>()
            .map(Self)
            .map_err(|_| Error::InvalidNodeIndex(s.to_string()))
    }
}

/// Shard assignment for an observer or validator.
///
/// MultiversX uses `4294967295` (`u32::MAX`) as the metachain shard id; we
/// surface it as the explicit `Metachain` variant rather than the magic
/// number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shard {
    Zero,
    One,
    Two,
    Metachain,
    Disabled,
    Auto,
}

/// Numeric value the MultiversX protocol assigns to the metachain shard.
const METACHAIN_SHARD_ID: u32 = 4_294_967_295;

impl Shard {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Zero => "0",
            Self::One => "1",
            Self::Two => "2",
            Self::Metachain => "metachain",
            Self::Disabled => "disabled",
            Self::Auto => "auto",
        }
    }

    /// Wire-level shard id used inside MultiversX configs and the proxy
    /// `[[Observers]]` blocks. Returns `None` for `Auto` and `Disabled` since
    /// they are operator intents, not shard ids.
    pub fn protocol_id(&self) -> Option<u32> {
        match self {
            Self::Zero => Some(0),
            Self::One => Some(1),
            Self::Two => Some(2),
            Self::Metachain => Some(METACHAIN_SHARD_ID),
            Self::Disabled | Self::Auto => None,
        }
    }
}

impl fmt::Display for Shard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Shard {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "0" => Ok(Self::Zero),
            "1" => Ok(Self::One),
            "2" => Ok(Self::Two),
            "metachain" => Ok(Self::Metachain),
            "disabled" => Ok(Self::Disabled),
            "auto" => Ok(Self::Auto),
            other => Err(Error::InvalidShard(other.to_string())),
        }
    }
}

/// Operator role for a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Validator,
    Observer,
    Multikey,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Validator => "validator",
            Self::Observer => "observer",
            Self::Multikey => "multikey",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "validator" => Ok(Self::Validator),
            "observer" => Ok(Self::Observer),
            "multikey" => Ok(Self::Multikey),
            other => Err(Error::InvalidRole(other.to_string())),
        }
    }
}

/// What this install is overall — drives the shape of upgrade flows and
/// state defaults. Mirrors the bash `.squad_install` marker but with
/// "validators" and "mixed" added explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallKind {
    Validators,
    ObserversSquad,
    MultikeySquad,
    Mixed,
}

impl InstallKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Validators => "validators",
            Self::ObserversSquad => "observers-squad",
            Self::MultikeySquad => "multikey-squad",
            Self::Mixed => "mixed",
        }
    }
}

impl fmt::Display for InstallKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where binaries come from for install/upgrade.
///
/// Default is `Source`: empirical evidence (April 2026) shows MultiversX ships
/// prebuilt assets on a minority of releases, so defaulting to `Release` would
/// fail most of the time. See plan §"Evidence behind the design", D2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactSource {
    #[default]
    Source,
    Release,
    Auto,
}

impl ArtifactSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Release => "release",
            Self::Auto => "auto",
        }
    }
}

impl fmt::Display for ArtifactSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ArtifactSource {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "source" => Ok(Self::Source),
            "release" => Ok(Self::Release),
            "auto" => Ok(Self::Auto),
            other => Err(Error::InvalidArtifactSource(other.to_string())),
        }
    }
}

/// A git tag (e.g. `v1.7.13` or `v1.7.13.0`). Stored as a normalized string
/// without the `tags/` prefix the GitHub API uses.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tag(String);

impl Tag {
    /// Strip a leading `tags/` prefix if present, then validate the remainder
    /// is non-empty and contains no whitespace.
    pub fn parse<S: AsRef<str>>(s: S) -> Result<Self, Error> {
        let raw = s.as_ref().trim();
        let stripped = raw.strip_prefix("tags/").unwrap_or(raw);
        if stripped.is_empty() || stripped.chars().any(char::is_whitespace) {
            return Err(Error::InvalidTag(raw.to_string()));
        }
        Ok(Self(stripped.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Form used when constructing GitHub API URLs that require the
    /// `tags/<tag>` prefix.
    pub fn api_form(&self) -> String {
        format!("tags/{}", self.0)
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Tag {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_round_trip() {
        for env in [
            Environment::Mainnet,
            Environment::Testnet,
            Environment::Devnet,
        ] {
            assert_eq!(env, env.as_str().parse().unwrap());
        }
        assert!("nope".parse::<Environment>().is_err());
    }

    #[test]
    fn environment_config_repo() {
        assert_eq!(
            Environment::Mainnet.config_repo(),
            "mx-chain-mainnet-config"
        );
        assert_eq!(Environment::Devnet.config_repo(), "mx-chain-devnet-config");
    }

    #[test]
    fn shard_round_trip_and_protocol_id() {
        for shard in [
            Shard::Zero,
            Shard::One,
            Shard::Two,
            Shard::Metachain,
            Shard::Disabled,
            Shard::Auto,
        ] {
            assert_eq!(shard, shard.as_str().parse().unwrap());
        }
        assert_eq!(Shard::Zero.protocol_id(), Some(0));
        assert_eq!(Shard::Metachain.protocol_id(), Some(4_294_967_295));
        assert_eq!(Shard::Auto.protocol_id(), None);
        assert_eq!(Shard::Disabled.protocol_id(), None);
    }

    #[test]
    fn role_round_trip() {
        for role in [Role::Validator, Role::Observer, Role::Multikey] {
            assert_eq!(role, role.as_str().parse().unwrap());
        }
        assert!("admin".parse::<Role>().is_err());
    }

    #[test]
    fn artifact_source_default_is_source() {
        assert_eq!(ArtifactSource::default(), ArtifactSource::Source);
    }

    #[test]
    fn tag_strips_api_prefix() {
        let tag = Tag::parse("tags/v1.7.13").unwrap();
        assert_eq!(tag.as_str(), "v1.7.13");
        assert_eq!(tag.api_form(), "tags/v1.7.13");
    }

    #[test]
    fn tag_rejects_whitespace_and_empty() {
        assert!(Tag::parse("").is_err());
        assert!(Tag::parse("   ").is_err());
        assert!(Tag::parse("v 1.0").is_err());
        assert!(Tag::parse("tags/").is_err());
    }

    #[test]
    fn node_index_parses() {
        assert_eq!(NodeIndex::from_str("0").unwrap().get(), 0);
        assert_eq!(NodeIndex::from_str("4").unwrap().get(), 4);
        assert!(NodeIndex::from_str("-1").is_err());
        assert!(NodeIndex::from_str("nope").is_err());
    }
}

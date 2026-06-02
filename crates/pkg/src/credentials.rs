//! Registry credentials — `~/.otter_fusion/credentials.toml` (`docs/23` §7).
//!
//! A private (`auth-required`) registry's requests carry a bearer token stored
//! per registry. `login`/`logout` mutate this file; the resolver and registry
//! API operations read it.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// The parsed credentials file: registry name → token.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credentials {
    pub tokens: BTreeMap<String, String>,
}

impl Credentials {
    /// The credentials file path, honoring `OTTER_FUSION_HOME`.
    pub fn path() -> PathBuf {
        let base = std::env::var_os("OTTER_FUSION_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".otter_fusion")))
            .or_else(|| {
                std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".otter_fusion"))
            })
            .unwrap_or_else(|| PathBuf::from(".otter_fusion"));
        base.join("credentials.toml")
    }

    /// Load the credentials file (empty if absent or unparseable).
    pub fn load() -> Credentials {
        let Ok(text) = std::fs::read_to_string(Self::path()) else {
            return Credentials::default();
        };
        Self::parse(&text)
    }

    /// Parse the credentials TOML: `[registries.<name>]\ntoken = "…"`.
    pub fn parse(text: &str) -> Credentials {
        let mut tokens = BTreeMap::new();
        let value: toml::Value = match text.parse() {
            Ok(v) => v,
            Err(_) => return Credentials::default(),
        };
        if let Some(regs) = value.get("registries").and_then(toml::Value::as_table) {
            for (name, entry) in regs {
                if let Some(tok) = entry.get("token").and_then(toml::Value::as_str) {
                    tokens.insert(name.clone(), tok.to_string());
                }
            }
        }
        Credentials { tokens }
    }

    /// The token for a registry, if logged in.
    pub fn token(&self, registry: &str) -> Option<&str> {
        self.tokens.get(registry).map(String::as_str)
    }

    /// Serialize to deterministic TOML.
    pub fn to_toml(&self) -> String {
        let mut out = String::new();
        for (name, token) in &self.tokens {
            out.push_str(&format!("[registries.{name}]\n"));
            out.push_str(&format!("token = {}\n\n", toml_str(token)));
        }
        out
    }

    /// Persist the credentials file (creating the parent directory).
    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_toml())
    }

    /// Set a registry's token (in memory).
    pub fn set(&mut self, registry: &str, token: &str) {
        self.tokens.insert(registry.to_string(), token.to_string());
    }

    /// Remove a registry's token; returns whether one was present.
    pub fn remove(&mut self, registry: &str) -> bool {
        self.tokens.remove(registry).is_some()
    }
}

fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_tokens() {
        let mut c = Credentials::default();
        c.set("public", "tok-abc");
        c.set("myco", "tok-xyz");
        let text = c.to_toml();
        let parsed = Credentials::parse(&text);
        assert_eq!(parsed.token("public"), Some("tok-abc"));
        assert_eq!(parsed.token("myco"), Some("tok-xyz"));
        assert_eq!(c, parsed);
    }

    #[test]
    fn remove_reports_presence() {
        let mut c = Credentials::default();
        c.set("public", "t");
        assert!(c.remove("public"));
        assert!(!c.remove("public"));
        assert!(c.token("public").is_none());
    }

    #[test]
    fn malformed_file_is_treated_as_empty() {
        assert!(Credentials::parse("not [valid").tokens.is_empty());
    }
}

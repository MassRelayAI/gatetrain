//! The plugin points the batch state machine reads from `gatetrain.toml` (design 7): regen, preflight, chain, deploy
//! targets and the protected branch. Read raw with `toml`; the full typed config loader belongs to the config lane.
use crate::{Error, Result};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct Target {
    pub command: Vec<String>,
    pub health: Vec<String>,
    /// `production` is never deployed automatically (floor.production-human).
    pub tier: String,
}

#[derive(Debug, Clone)]
pub struct BatchConfig {
    pub regen_command: Vec<String>,
    pub max_passes: usize,
    pub generated_dirs: Vec<String>,
    pub preflight_command: Vec<String>,
    pub chain_command: Vec<String>,
    /// Step names of the chain, for the memory admission (`[chain] steps`).
    pub chain_steps: Vec<String>,
    pub protected: String,
    pub targets: BTreeMap<String, Target>,
}

impl Default for BatchConfig {
    fn default() -> Self {
        BatchConfig {
            regen_command: vec![],
            max_passes: 3,
            generated_dirs: vec![],
            preflight_command: vec![],
            chain_command: vec![],
            chain_steps: vec![],
            protected: "main".into(),
            targets: BTreeMap::new(),
        }
    }
}

fn argv(v: Option<&toml::Value>) -> Vec<String> {
    v.and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

impl BatchConfig {
    pub fn load(path: &Path) -> Result<BatchConfig> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::Batch(format!("reading {}: {e}", path.display())))?;
        BatchConfig::parse(&text)
    }

    pub fn parse(text: &str) -> Result<BatchConfig> {
        let v: toml::Value = text.parse().map_err(|e| Error::Batch(format!("gatetrain.toml: {e}")))?;
        let mut c = BatchConfig::default();
        if let Some(r) = v.get("regen") {
            c.regen_command = argv(r.get("command"));
            if let Some(n) = r.get("max_passes").and_then(|n| n.as_integer()) {
                c.max_passes = n.max(1) as usize;
            }
            c.generated_dirs = argv(r.get("generated_dirs"));
        }
        c.preflight_command = argv(v.get("preflight").and_then(|p| p.get("command")));
        c.chain_command = argv(v.get("chain").and_then(|p| p.get("command")));
        c.chain_steps = argv(v.get("chain").and_then(|p| p.get("steps")));
        if let Some(p) = v.get("batch").and_then(|b| b.get("protected")).and_then(|p| p.as_str()) {
            c.protected = p.to_string();
        }
        if let Some(d) = v.get("deploy").and_then(|d| d.as_table()) {
            for (name, t) in d {
                c.targets.insert(
                    name.clone(),
                    Target {
                        command: argv(t.get("command")),
                        health: argv(t.get("health")),
                        tier: t.get("tier").and_then(|x| x.as_str()).unwrap_or("staging").to_string(),
                    },
                );
            }
        }
        Ok(c)
    }
}

//! Which workspaces live on another machine: the REMOTE WORKSPACES binding,
//! workspace id → ssh destination, backing the `w` switcher's `h` verb and
//! `nebula workspace host`.
//!
//! A plain JSON object in the data dir, a sibling of `ssh_hosts.json`. It is
//! client-side on purpose: the daemon's workspaces are plain project groups
//! and stay so, and the far end is an ordinary nebula daemon that needs to
//! know nothing about being someone's workspace — so a stock remote of the
//! same protocol version is reachable with no change on its side. Keyed by
//! id rather than name so a rename keeps the binding. A missing or
//! malformed file reads as empty.
//!
//! A value is the destination as a string — `"fm@motum"` — or, for a
//! machine whose nebula is launched with its own environment (an isolated
//! `NEBULA_DATA_DIR`, a `CLAUDE_CONFIG_DIR` for the agents it starts), an
//! object `{"host": "fm@motum", "env": {"NEBULA_DATA_DIR": "…"}}`. The
//! variables are exported on the remote before `nebula relay` runs, so the
//! relay reaches the instance those variables name, and a daemon it has to
//! spawn inherits them. A bare ssh command gets none of the login shell's
//! environment, which is why they have to travel with the binding.

use nebula_core::WorkspaceId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One workspace's machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Binding {
    /// The destination alone, exactly as typed (`user@server`, or a `Host`
    /// alias from `~/.ssh/config`).
    Host(String),
    /// The destination plus what to export on the remote first.
    WithEnv {
        host: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
}

impl Binding {
    pub fn new(host: String, env: BTreeMap<String, String>) -> Self {
        if env.is_empty() {
            Binding::Host(host)
        } else {
            Binding::WithEnv { host, env }
        }
    }

    pub fn host(&self) -> &str {
        match self {
            Binding::Host(host) | Binding::WithEnv { host, .. } => host,
        }
    }

    pub fn env(&self) -> &BTreeMap<String, String> {
        static NONE: std::sync::OnceLock<BTreeMap<String, String>> = std::sync::OnceLock::new();
        match self {
            Binding::Host(_) => NONE.get_or_init(BTreeMap::new),
            Binding::WithEnv { env, .. } => env,
        }
    }

    /// The same machine with a new address; the environment stays.
    pub fn with_host(&self, host: String) -> Self {
        Binding::new(host, self.env().clone())
    }
}

/// The whole file: workspace id → binding.
pub type Bindings = BTreeMap<String, Binding>;

pub fn load() -> Bindings {
    load_from(&store_path())
}

/// The machine `id` is bound to, if any.
pub fn binding_for(id: &WorkspaceId) -> Option<Binding> {
    load().get(id.as_str()).cloned()
}

/// Bind `id` to `binding`, or unbind it with `None`.
pub fn set(id: &WorkspaceId, binding: Option<&Binding>) -> std::io::Result<()> {
    set_at(&store_path(), id, binding)
}

/// A destination as typed in the prompt: trimmed, and `None` when empty —
/// an empty prompt is "unbind". Spaces inside are refused, since ssh takes
/// one word and anything after it would silently become a command.
pub fn parse_host(input: &str) -> Result<Option<String>, String> {
    let text = input.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if text.chars().any(char::is_whitespace) {
        return Err("an ssh destination is one word, like user@server".into());
    }
    if text.starts_with('-') {
        return Err("an ssh destination can't start with '-'".into());
    }
    Ok(Some(text.to_string()))
}

/// One `--env NAME=value` as typed: the name must be a shell variable
/// name, since the remote script runs `export NAME=value` with it.
pub fn parse_env(input: &str) -> Result<(String, String), String> {
    let Some((name, value)) = input.split_once('=') else {
        return Err(format!("expected NAME=value, got {input:?}"));
    };
    let valid = !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        return Err(format!("{name:?} is not a variable name"));
    }
    Ok((name.to_string(), value.to_string()))
}

pub(crate) fn store_path() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(path) = STORE_PATH_OVERRIDE.with(|p| p.borrow().clone()) {
            return path;
        }
    }
    nebula_core::paths::data_dir().join("workspace_hosts.json")
}

/// Entry by entry, so one unreadable value costs only itself.
fn load_from(path: &Path) -> Bindings {
    let Ok(Some(object)) = nebula_core::settings::read_object(path) else {
        return Bindings::new();
    };
    object
        .into_iter()
        .filter_map(|(id, value)| {
            let binding: Binding = serde_json::from_value(value).ok()?;
            let host = binding.host().trim();
            if host.is_empty() {
                return None;
            }
            // Re-pack so a trimmed host and an empty env store canonically.
            Some((id, Binding::new(host.to_string(), binding.env().clone())))
        })
        .collect()
}

fn set_at(store: &Path, id: &WorkspaceId, binding: Option<&Binding>) -> std::io::Result<()> {
    let mut bindings = load_from(store);
    match binding {
        Some(binding) => {
            bindings.insert(id.as_str().to_string(), binding.clone());
        }
        None => {
            bindings.remove(id.as_str());
        }
    }
    save_to(store, &bindings)
}

fn save_to(store: &Path, bindings: &Bindings) -> std::io::Result<()> {
    let value = serde_json::to_value(bindings)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    nebula_core::settings::write_json(store, &value)
}

#[cfg(test)]
thread_local! {
    static STORE_PATH_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Test hook (the `with_hosts_path` pattern): route this thread's binding
/// store at `path` for the duration of `f`.
#[cfg(test)]
pub fn with_store_path<T>(path: PathBuf, f: impl FnOnce() -> T) -> T {
    STORE_PATH_OVERRIDE.with(|slot| {
        let prev = slot.replace(Some(path));
        let out = f();
        slot.replace(prev);
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace_hosts.json");
        (dir, path)
    }

    fn id(s: &str) -> WorkspaceId {
        WorkspaceId(s.to_string())
    }

    fn host(s: &str) -> Binding {
        Binding::Host(s.to_string())
    }

    #[test]
    fn missing_or_malformed_file_reads_empty() {
        let (_dir, path) = store();
        assert!(load_from(&path).is_empty());
        std::fs::write(&path, "not json").unwrap();
        assert!(load_from(&path).is_empty());
        std::fs::write(&path, "[1, 2]").unwrap();
        assert!(load_from(&path).is_empty());
    }

    #[test]
    fn set_binds_rebinds_and_unbinds() {
        let (_dir, path) = store();
        set_at(&path, &id("ws1"), Some(&host("fm@motum"))).unwrap();
        set_at(&path, &id("ws2"), Some(&host("other"))).unwrap();
        assert_eq!(load_from(&path)["ws1"].host(), "fm@motum");
        set_at(&path, &id("ws1"), Some(&host("fm@motum2"))).unwrap();
        assert_eq!(load_from(&path)["ws1"].host(), "fm@motum2");
        set_at(&path, &id("ws1"), None).unwrap();
        let bindings = load_from(&path);
        assert!(!bindings.contains_key("ws1"));
        assert_eq!(bindings["ws2"].host(), "other");
    }

    /// A plain host stores as a string, one with variables as an object,
    /// and both read back — so a file written by hand in either shape
    /// works, and the common case stays one line.
    #[test]
    fn env_rides_with_the_host_and_stores_canonically() {
        let (_dir, path) = store();
        let env: BTreeMap<String, String> =
            [("NEBULA_DATA_DIR".to_string(), "/srv/neb".to_string())].into();
        set_at(
            &path,
            &id("a"),
            Some(&Binding::new("fm@box".into(), env.clone())),
        )
        .unwrap();
        set_at(&path, &id("b"), Some(&host("plain"))).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""b": "plain""#), "{text}");
        assert!(text.contains(r#""host": "fm@box""#), "{text}");
        let bindings = load_from(&path);
        assert_eq!(bindings["a"].host(), "fm@box");
        assert_eq!(bindings["a"].env(), &env);
        assert!(bindings["b"].env().is_empty());
        // A new address keeps the machine's environment.
        let moved = bindings["a"].with_host("fm@box2".into());
        assert_eq!(moved.host(), "fm@box2");
        assert_eq!(moved.env(), &env);
    }

    /// One bad value — a number, an empty string — drops that entry and
    /// keeps the rest, the way the hosts list reads entry by entry.
    #[test]
    fn a_bad_entry_costs_only_itself() {
        let (_dir, path) = store();
        std::fs::write(
            &path,
            r#"{"a": "one", "b": 3, "c": "  ", "d": {"host": "four"}, "e": {"env": {}}}"#,
        )
        .unwrap();
        let bindings = load_from(&path);
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings["a"].host(), "one");
        assert_eq!(bindings["d"].host(), "four");
    }

    #[test]
    fn override_routes_the_store() {
        let (_dir, path) = store();
        with_store_path(path.clone(), || {
            set(&id("ws"), Some(&host("box"))).unwrap();
            assert_eq!(binding_for(&id("ws")).unwrap().host(), "box");
            assert_eq!(binding_for(&id("nope")), None);
        });
        assert!(path.exists());
    }

    #[test]
    fn parse_host_trims_and_refuses_shapes_ssh_would_misread() {
        assert_eq!(
            parse_host("  fm@motum  ").unwrap().as_deref(),
            Some("fm@motum")
        );
        assert_eq!(parse_host("   ").unwrap(), None);
        assert!(parse_host("fm@motum /srv/app").is_err());
        assert!(parse_host("-oProxyCommand=x").is_err());
    }

    #[test]
    fn parse_env_wants_a_variable_name() {
        assert_eq!(
            parse_env("NEBULA_DATA_DIR=/a b").unwrap(),
            ("NEBULA_DATA_DIR".into(), "/a b".into())
        );
        assert_eq!(parse_env("X=").unwrap(), ("X".into(), String::new()));
        assert!(parse_env("novalue").is_err());
        assert!(parse_env("=v").is_err());
        assert!(parse_env("1X=v").is_err());
        assert!(parse_env("A-B=v").is_err());
    }
}

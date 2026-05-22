//! Workflow `[require]` manifest: declared inputs (params + secrets) used by
//! the install/configure flow and validated at every workflow-load entry point.
//!
//! See `require-plan.md` for the design.

use indexmap::IndexMap;
use serde::Deserialize;
use std::collections::BTreeSet;

use crate::types::{TriggerDef, WorkflowDef, WorkspaceSource};

/// A single `[require.<NAME>]` entry.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RequireEntry {
    pub description: String,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    pub default: Option<String>,
}

/// Map of declared requirement name → entry. Preserves TOML insertion order
/// so install/configure prompts run in the order the author wrote them.
pub type Requirements = IndexMap<String, RequireEntry>;

/// Env-var names that must not be shadowed by a declared requirement; these
/// are the safe-system-vars re-injected into the otherwise-clean child env.
pub const RESERVED_NAMES: &[&str] = &["PATH", "HOME", "USER", "SHELL", "TMPDIR", "PWD", "LANG"];

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("invalid workflow TOML: {0}")]
    Parse(String),

    #[error("workflow uses requires/template references but has no [require] section")]
    MissingManifest,

    #[error("undeclared template reference {{{{{name}}}}}: add [require.{name}] to the manifest")]
    UndeclaredParamRef { name: String },

    #[error(
        "undeclared `requires` reference '{name}': add [require.{name}] with `sensitive = true`"
    )]
    UndeclaredRequiresRef { name: String },

    #[error(
        "`requires = [..]` may only reference sensitive entries in v1; '{name}' is non-sensitive — \
         reference it via {{{{{name}}}}} substitution instead"
    )]
    NonSensitiveInRequires { name: String },

    #[error(
        "{{{{{name}}}}} substitution may only reference non-sensitive entries; \
         '{name}' is sensitive — reference it via `requires = [\"{name}\"]` instead"
    )]
    SensitiveInTemplate { name: String },

    #[error("requirement name '{name}' is invalid: must match ^[A-Z][A-Z0-9_]*$")]
    InvalidName { name: String },

    #[error(
        "requirement name '{name}' shadows a system environment variable; choose a different name"
    )]
    ReservedName { name: String },

    #[error("requirement '{name}' is sensitive; `default` is not allowed on sensitive entries")]
    SensitiveWithDefault { name: String },

    #[error("requirement '{name}': `default` may not contain '{{{{' (no transitive references)")]
    DefaultContainsTemplate { name: String },

    #[error(
        "legacy `secrets = [..]` field detected at {site}; rename it to `requires = [..]` and \
         add matching [require.<NAME>] declarations with `sensitive = true`"
    )]
    LegacySecretsField { site: &'static str },
}

/// Parse + validate a workflow TOML string. The primary entry point used by
/// both `otter workflow install` (fail-fast) and the daemon's load loop
/// (skip-with-warning on error).
pub fn validate_workflow(raw: &str) -> Result<WorkflowDef, ValidationError> {
    // Pre-flight scan for the legacy `secrets = [..]` field name. We catch
    // this before deserialization to give a clearer migration message than
    // `serde(deny_unknown_fields)` would produce.
    if let Some(site) = detect_legacy_secrets_field(raw) {
        return Err(ValidationError::LegacySecretsField { site });
    }

    let def: WorkflowDef =
        toml::from_str(raw).map_err(|e| ValidationError::Parse(e.to_string()))?;

    let template_refs = find_param_refs(raw);
    let requires_refs = collect_requires_refs(&def);

    let has_manifest = def.require.as_ref().map(|r| !r.is_empty()).unwrap_or(false);

    if !has_manifest && (!template_refs.is_empty() || !requires_refs.is_empty()) {
        return Err(ValidationError::MissingManifest);
    }

    let empty = Requirements::new();
    let manifest = def.require.as_ref().unwrap_or(&empty);

    // Per-entry declaration checks.
    for (name, entry) in manifest.iter() {
        if !is_valid_name(name) {
            return Err(ValidationError::InvalidName { name: name.clone() });
        }
        if is_reserved_name(name) {
            return Err(ValidationError::ReservedName { name: name.clone() });
        }
        if entry.sensitive && entry.default.is_some() {
            return Err(ValidationError::SensitiveWithDefault { name: name.clone() });
        }
        if let Some(d) = &entry.default {
            if d.contains("{{") {
                return Err(ValidationError::DefaultContainsTemplate { name: name.clone() });
            }
        }
    }

    // Every {{X}} reference must point to a declared, non-sensitive entry.
    for name in &template_refs {
        match manifest.get(name) {
            None => {
                return Err(ValidationError::UndeclaredParamRef { name: name.clone() });
            }
            Some(entry) if entry.sensitive => {
                return Err(ValidationError::SensitiveInTemplate { name: name.clone() });
            }
            _ => {}
        }
    }

    // Every `requires` entry must point to a declared, sensitive entry (v1).
    for name in &requires_refs {
        match manifest.get(name) {
            None => {
                return Err(ValidationError::UndeclaredRequiresRef { name: name.clone() });
            }
            Some(entry) if !entry.sensitive => {
                return Err(ValidationError::NonSensitiveInRequires { name: name.clone() });
            }
            _ => {}
        }
    }

    // Declared-but-unused entries: warn only (typo guard).
    for name in manifest.keys() {
        if !template_refs.contains(name) && !requires_refs.contains(name) {
            tracing::warn!(
                workflow = %def.name,
                name = %name,
                "declared in [require] but never referenced (typo?)"
            );
        }
    }

    Ok(def)
}

/// Hand-rolled scanner: collect every `{{NAME}}` reference in the raw TOML
/// text where NAME matches `^[A-Z][A-Z0-9_]*$`. Whitespace inside the braces
/// is tolerated; lowercase / mixed-case refs and single braces are ignored.
pub fn find_param_refs(raw: &str) -> BTreeSet<String> {
    let bytes = raw.as_bytes();
    let mut out = BTreeSet::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let mut j = i + 2;
            // skip leading whitespace
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            let name_start = j;
            // first char must be uppercase ASCII letter
            if j < bytes.len() && bytes[j].is_ascii_uppercase() {
                j += 1;
                while j < bytes.len()
                    && (bytes[j].is_ascii_uppercase()
                        || bytes[j].is_ascii_digit()
                        || bytes[j] == b'_')
                {
                    j += 1;
                }
                let name_end = j;
                // skip trailing whitespace
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                    j += 1;
                }
                if j + 1 < bytes.len() && bytes[j] == b'}' && bytes[j + 1] == b'}' {
                    let name = std::str::from_utf8(&bytes[name_start..name_end])
                        .expect("ASCII-only by construction")
                        .to_string();
                    out.insert(name);
                    i = j + 2;
                    continue;
                }
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

/// Collect all `requires = [..]` names referenced anywhere on the parsed def.
pub fn collect_requires_refs(def: &WorkflowDef) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for step in &def.steps {
        if let Some(list) = &step.requires {
            out.extend(list.iter().cloned());
        }
    }
    for fin in &def.finally {
        if let Some(list) = &fin.step.requires {
            out.extend(list.iter().cloned());
        }
    }
    if let Some(TriggerDef::Polling {
        requires: Some(list),
        ..
    }) = &def.trigger
    {
        out.extend(list.iter().cloned());
    }
    if let Some(ws) = &def.workspace {
        if let WorkspaceSource::Script {
            requires: Some(list),
            ..
        } = &ws.source
        {
            out.extend(list.iter().cloned());
        }
    }
    out
}

/// Substitute every `{{NAME}}` in the raw TOML text using the provided value
/// map. Unknown names are left untouched (the validator catches them first).
/// Whitespace inside the braces is tolerated.
pub fn substitute_params(raw: &str, values: &IndexMap<String, String>) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let mut j = i + 2;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            let name_start = j;
            if j < bytes.len() && bytes[j].is_ascii_uppercase() {
                j += 1;
                while j < bytes.len()
                    && (bytes[j].is_ascii_uppercase()
                        || bytes[j].is_ascii_digit()
                        || bytes[j] == b'_')
                {
                    j += 1;
                }
                let name_end = j;
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                    j += 1;
                }
                if j + 1 < bytes.len() && bytes[j] == b'}' && bytes[j + 1] == b'}' {
                    let name = std::str::from_utf8(&bytes[name_start..name_end])
                        .expect("ASCII-only by construction");
                    if let Some(val) = values.get(name) {
                        out.push_str(val);
                        i = j + 2;
                        continue;
                    }
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    while i < bytes.len() {
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

pub fn load_values_toml(path: &std::path::Path) -> anyhow::Result<IndexMap<String, String>> {
    if !path.exists() {
        return Ok(IndexMap::new());
    }
    let raw = std::fs::read_to_string(path)?;
    let table: toml::Table = toml::from_str(&raw)?;
    Ok(table
        .into_iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
        .collect())
}

pub fn missing_param_values(
    manifest: &Requirements,
    values: &IndexMap<String, String>,
) -> Vec<String> {
    manifest
        .iter()
        .filter(|(_, e)| !e.sensitive)
        .filter(|(name, _)| !values.contains_key(*name))
        .map(|(name, _)| name.clone())
        .collect()
}

/// Expand a single leading `~` to the user's home directory. Tilde anywhere
/// else in the value is left as-is.
pub fn expand_tilde(value: &str) -> String {
    let home = std::env::var("HOME").ok();
    expand_tilde_with_home(value, home.as_deref())
}

/// Test-friendly seam: same as `expand_tilde` but with an explicit home so
/// tests don't have to mutate process-wide `$HOME` (which races other tests).
pub fn expand_tilde_with_home(value: &str, home: Option<&str>) -> String {
    if let Some(rest) = value.strip_prefix('~') {
        if let Some(h) = home {
            let mut s = String::from(h);
            s.push_str(rest);
            return s;
        }
    }
    value.to_string()
}

/// Reject characters that would corrupt the substituted TOML or env injection.
pub fn validate_value_chars(value: &str) -> Result<(), char> {
    for c in value.chars() {
        if c == '"' || c == '\\' || c == '\n' || c == '\r' || c.is_control() {
            return Err(c);
        }
    }
    Ok(())
}

fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_uppercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn is_reserved_name(name: &str) -> bool {
    if RESERVED_NAMES.contains(&name) {
        return true;
    }
    name.starts_with("LC_")
}

/// Pre-flight scan for the legacy `secrets = [..]` TOML field name on
/// step / trigger / workspace, so the migration error is clearer than the
/// generic `unknown field` produced by `serde(deny_unknown_fields)`.
fn detect_legacy_secrets_field(raw: &str) -> Option<&'static str> {
    // Cheap line-scan: look for any line whose first non-whitespace tokens
    // are `secrets = [`. Toml values for sandbox-image et al. don't use this
    // form; the false-positive risk is low.
    for line in raw.lines() {
        let trimmed = line.trim_start();
        if let Some(after) = trimmed.strip_prefix("secrets") {
            if after.trim_start().starts_with('=') {
                return Some("step/trigger/workspace");
            }
        }
    }
    None
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn unwrap_err(raw: &str) -> ValidationError {
        validate_workflow(raw).expect_err("expected validation error")
    }

    fn ok(raw: &str) -> WorkflowDef {
        validate_workflow(raw).expect("expected validation to succeed")
    }

    const BASIC: &str = r#"
        name = "wf"
        type = "looping"
        [[steps]]
        type = "shell"
        command = ["echo", "hi"]
    "#;

    #[test]
    fn workflow_without_refs_and_without_manifest_is_ok() {
        // WHEN / THEN
        ok(BASIC);
    }

    #[test]
    fn workflow_with_requires_ref_but_no_manifest_errors() {
        // GIVEN
        let raw = r#"
            name = "wf"
            type = "looping"
            [[steps]]
            type = "shell"
            command = ["echo", "hi"]
            requires = ["JIRA_PAT"]
        "#;
        // WHEN / THEN
        assert!(matches!(unwrap_err(raw), ValidationError::MissingManifest));
    }

    #[test]
    fn workflow_with_template_ref_but_no_manifest_errors() {
        // GIVEN
        let raw = r#"
            name = "wf"
            type = "looping"
            [[steps]]
            type = "shell"
            command = ["echo", "{{REPO_PATH}}"]
        "#;
        // WHEN / THEN
        assert!(matches!(unwrap_err(raw), ValidationError::MissingManifest));
    }

    #[test]
    fn undeclared_requires_ref_is_rejected() {
        // GIVEN
        let raw = r#"
            name = "wf"
            type = "looping"
            [require.OTHER]
            description = "..."
            sensitive = true
            [[steps]]
            type = "shell"
            command = ["echo", "hi"]
            requires = ["JIRA_PAT"]
        "#;
        // WHEN / THEN
        match unwrap_err(raw) {
            ValidationError::UndeclaredRequiresRef { name } => assert_eq!(name, "JIRA_PAT"),
            e => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn undeclared_template_ref_is_rejected() {
        // GIVEN
        let raw = r#"
            name = "wf"
            type = "looping"
            [require.OTHER]
            description = "..."
            [[steps]]
            type = "shell"
            command = ["echo", "{{REPO_PATH}}"]
        "#;
        // WHEN / THEN
        match unwrap_err(raw) {
            ValidationError::UndeclaredParamRef { name } => assert_eq!(name, "REPO_PATH"),
            e => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn requires_must_reference_sensitive_entries_in_v1() {
        // GIVEN
        let raw = r#"
            name = "wf"
            type = "looping"
            [require.REPO_PATH]
            description = "..."
            [[steps]]
            type = "shell"
            command = ["echo", "hi"]
            requires = ["REPO_PATH"]
        "#;
        // WHEN / THEN
        match unwrap_err(raw) {
            ValidationError::NonSensitiveInRequires { name } => assert_eq!(name, "REPO_PATH"),
            e => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn template_must_reference_non_sensitive_entries() {
        // GIVEN
        let raw = r#"
            name = "wf"
            type = "looping"
            [require.JIRA_PAT]
            description = "..."
            sensitive = true
            [[steps]]
            type = "shell"
            command = ["echo", "{{JIRA_PAT}}"]
        "#;
        // WHEN / THEN
        match unwrap_err(raw) {
            ValidationError::SensitiveInTemplate { name } => assert_eq!(name, "JIRA_PAT"),
            e => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn unused_declarations_warn_but_succeed() {
        // GIVEN
        let raw = r#"
            name = "wf"
            type = "looping"
            [require.UNUSED]
            description = "..."
            [[steps]]
            type = "shell"
            command = ["echo", "hi"]
        "#;
        // WHEN / THEN
        ok(raw);
    }

    #[test]
    fn whitespace_inside_braces_is_tolerated() {
        // WHEN
        let refs = find_param_refs("base_repo = \"{{ REPO_PATH }}\"");
        // THEN
        assert!(refs.contains("REPO_PATH"));
    }

    #[test]
    fn lowercase_refs_are_ignored() {
        // WHEN
        let refs = find_param_refs("x = \"{{repo_path}}\" y = \"{{Mixed_Case}}\"");
        // THEN
        assert!(refs.is_empty());
    }

    #[test]
    fn single_brace_is_ignored() {
        // WHEN
        let refs = find_param_refs("x = \"{ABC} {DEF}\"");
        // THEN
        assert!(refs.is_empty());
    }

    #[test]
    fn requires_on_all_four_sites_is_collected() {
        // GIVEN
        let raw = r#"
            name = "wf"
            type = "triggered"
            [require.A]
            description = "a"
            sensitive = true
            [require.B]
            description = "b"
            sensitive = true
            [require.C]
            description = "c"
            sensitive = true
            [require.D]
            description = "d"
            sensitive = true
            [trigger]
            type = "polling"
            poll_command = ["poll"]
            requires = ["A"]
            [workspace]
            type = "script"
            command = ["setup"]
            requires = ["B"]
            [[steps]]
            type = "shell"
            command = ["echo", "hi"]
            requires = ["C"]
            [[finally]]
            type = "shell"
            command = ["echo", "bye"]
            requires = ["D"]
        "#;
        // WHEN
        let def = ok(raw);
        let refs = collect_requires_refs(&def);
        // THEN
        assert!(refs.contains("A"));
        assert!(refs.contains("B"));
        assert!(refs.contains("C"));
        assert!(refs.contains("D"));
    }

    #[test]
    fn substitution_into_git_workspace_block() {
        // GIVEN
        let raw = r#"
            name = "wf"
            type = "triggered"
            [require.REPO_PATH]
            description = "..."
            [require.POOL_DIR]
            description = "..."
            [trigger]
            type = "manual"
            [workspace]
            type = "git"
            base_repo = "{{REPO_PATH}}"
            [workspace.pool]
            dir = "{{POOL_DIR}}"
            [[steps]]
            type = "shell"
            command = ["echo", "hi"]
        "#;
        ok(raw);
        let mut values = IndexMap::new();
        values.insert("REPO_PATH".to_string(), "/home/me/repo".to_string());
        values.insert("POOL_DIR".to_string(), "/tmp/pool".to_string());

        // WHEN
        let substituted = substitute_params(raw, &values);

        // THEN
        assert!(substituted.contains("base_repo = \"/home/me/repo\""));
        assert!(substituted.contains("dir = \"/tmp/pool\""));
    }

    #[test]
    fn substitution_leaves_literal_text_untouched() {
        // GIVEN
        let mut values = IndexMap::new();
        values.insert("X".to_string(), "value".to_string());
        // WHEN
        let out = substitute_params("plain text {not_a_ref}", &values);
        // THEN
        assert_eq!(out, "plain text {not_a_ref}");
    }

    #[test]
    fn value_validation_rejects_quotes_backslash_newline_control() {
        // WHEN / THEN
        assert!(validate_value_chars(r#"has "quote""#).is_err());
        assert!(validate_value_chars(r"has \backslash").is_err());
        assert!(validate_value_chars("has\nnewline").is_err());
        assert!(validate_value_chars("has\rcr").is_err());
        assert!(validate_value_chars("has\x07bell").is_err());
        assert!(validate_value_chars("normal value 123").is_ok());
        assert!(validate_value_chars("/path/with/slashes").is_ok());
    }

    #[test]
    fn tilde_expansion_only_at_start() {
        // WHEN / THEN
        let h = Some("/home/test");
        assert_eq!(expand_tilde_with_home("~/foo", h), "/home/test/foo");
        assert_eq!(expand_tilde_with_home("foo/~bar", h), "foo/~bar");
        assert_eq!(expand_tilde_with_home("~", h), "/home/test");
        assert_eq!(expand_tilde_with_home("~/foo", None), "~/foo");
    }

    #[test]
    fn invalid_name_rejected_at_declaration() {
        // GIVEN
        let raw = r#"
            name = "wf"
            type = "looping"
            [require.lower_case]
            description = "..."
            [[steps]]
            type = "shell"
            command = ["echo", "hi"]
        "#;
        // WHEN / THEN — TOML parses lower_case fine; our validator should reject.
        // The {{X}} scanner won't match lowercase, but the validator still checks
        // declaration names. We expect either Parse (if toml rejects) or InvalidName.
        match validate_workflow(raw) {
            Err(ValidationError::InvalidName { name }) => assert_eq!(name, "lower_case"),
            Err(ValidationError::Parse(_)) => {} // also acceptable
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn reserved_name_rejected() {
        // GIVEN
        let raw = r#"
            name = "wf"
            type = "looping"
            [require.PATH]
            description = "..."
            [[steps]]
            type = "shell"
            command = ["echo", "hi"]
        "#;
        // WHEN / THEN
        match unwrap_err(raw) {
            ValidationError::ReservedName { name } => assert_eq!(name, "PATH"),
            e => panic!("unexpected: {e}"),
        }
    }

    #[test]
    fn reserved_lc_prefix_rejected() {
        // GIVEN
        let raw = r#"
            name = "wf"
            type = "looping"
            [require.LC_ALL]
            description = "..."
            [[steps]]
            type = "shell"
            command = ["echo", "hi"]
        "#;
        // WHEN / THEN
        match unwrap_err(raw) {
            ValidationError::ReservedName { name } => assert_eq!(name, "LC_ALL"),
            e => panic!("unexpected: {e}"),
        }
    }

    #[test]
    fn sensitive_entry_with_default_rejected() {
        // GIVEN
        let raw = r#"
            name = "wf"
            type = "looping"
            [require.TOKEN]
            description = "..."
            sensitive = true
            default = "x"
            [[steps]]
            type = "shell"
            command = ["echo", "hi"]
            requires = ["TOKEN"]
        "#;
        // WHEN / THEN
        match unwrap_err(raw) {
            ValidationError::SensitiveWithDefault { name } => assert_eq!(name, "TOKEN"),
            e => panic!("unexpected: {e}"),
        }
    }

    #[test]
    fn default_containing_template_rejected() {
        // GIVEN
        let raw = r#"
            name = "wf"
            type = "looping"
            [require.X]
            description = "..."
            default = "prefix-{{Y}}"
            [[steps]]
            type = "shell"
            command = ["echo", "{{X}}"]
        "#;
        // WHEN / THEN
        match unwrap_err(raw) {
            ValidationError::DefaultContainsTemplate { name } => assert_eq!(name, "X"),
            e => panic!("unexpected: {e}"),
        }
    }

    #[test]
    fn require_entry_rejects_unknown_fields() {
        // GIVEN
        let raw = r#"
            name = "wf"
            type = "looping"
            [require.X]
            description = "..."
            bogus = true
            [[steps]]
            type = "shell"
            command = ["echo", "hi"]
        "#;
        // WHEN / THEN
        match validate_workflow(raw) {
            Err(ValidationError::Parse(msg)) => assert!(msg.contains("bogus"), "msg = {msg}"),
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn legacy_secrets_field_produces_migration_error() {
        // GIVEN
        let raw = r#"
            name = "wf"
            type = "looping"
            [[steps]]
            type = "shell"
            command = ["echo", "hi"]
            secrets = ["JIRA_PAT"]
        "#;
        // WHEN / THEN
        match unwrap_err(raw) {
            ValidationError::LegacySecretsField { .. } => {}
            e => panic!("unexpected: {e}"),
        }
    }

    #[test]
    fn load_values_toml_returns_empty_when_missing() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.toml");
        // WHEN
        let m = load_values_toml(&path).unwrap();
        // THEN
        assert!(m.is_empty());
    }

    #[test]
    fn load_values_toml_round_trips_strings_and_ignores_non_strings() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("values.toml");
        std::fs::write(
            &path,
            r#"
REPO_PATH = "/home/me/repo"
POOL_DIR = "/tmp/pool"
NOT_A_STRING = 42
"#,
        )
        .unwrap();
        // WHEN
        let m = load_values_toml(&path).unwrap();
        // THEN
        assert_eq!(
            m.get("REPO_PATH").map(String::as_str),
            Some("/home/me/repo")
        );
        assert_eq!(m.get("POOL_DIR").map(String::as_str), Some("/tmp/pool"));
        assert!(!m.contains_key("NOT_A_STRING"));
    }

    #[test]
    fn missing_param_values_lists_only_non_sensitive_missing() {
        // GIVEN
        let raw = r#"
            name = "wf"
            type = "looping"
            [require.A]
            description = "a"
            [require.B]
            description = "b"
            sensitive = true
            [require.C]
            description = "c"
            [[steps]]
            type = "shell"
            command = ["echo", "{{A}} {{C}}"]
            requires = ["B"]
        "#;
        let def = ok(raw);
        let manifest = def.require.unwrap();
        let mut values = IndexMap::new();
        values.insert("A".to_string(), "alpha".to_string());
        // C is missing; B is sensitive (not checked here).

        // WHEN
        let missing = missing_param_values(&manifest, &values);

        // THEN
        assert_eq!(missing, vec!["C".to_string()]);
    }

    #[test]
    fn manifest_order_preserved() {
        // GIVEN
        let raw = r#"
            name = "wf"
            type = "looping"
            [require.ZEBRA]
            description = "z"
            [require.APPLE]
            description = "a"
            [require.MANGO]
            description = "m"
            [[steps]]
            type = "shell"
            command = ["echo", "{{ZEBRA}} {{APPLE}} {{MANGO}}"]
        "#;
        // WHEN
        let def = ok(raw);
        let manifest = def.require.unwrap();
        let names: Vec<&str> = manifest.keys().map(String::as_str).collect();
        // THEN — author wrote Z, A, M; that's the prompt order.
        assert_eq!(names, vec!["ZEBRA", "APPLE", "MANGO"]);
    }
}

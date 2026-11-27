//! Dependency extraction from package manifests.

use serde_json::Value as Json;
use toml::Value as Toml;

/// A declared project dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub ecosystem: &'static str,
    pub name: String,
    /// Version requirement as written, or a description of the source (`path`, `git`, `workspace`).
    pub version_req: String,
    /// `normal`, `dev`, `build`, `peer`, `optional` or `server`.
    pub kind: String,
}

/// Parses dependencies from a manifest. Returns a human-readable message on malformed input.
pub fn parse(path: &str, text: &str) -> Result<Vec<Dependency>, String> {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let deps = match name.as_str() {
        "cargo.toml" => cargo(&parse_toml(path, text)?),
        "package.json" => npm(&parse_json(path, text)?),
        "pyproject.toml" => pyproject(&parse_toml(path, text)?),
        "wally.toml" => wally(&parse_toml(path, text)?),
        n if n.starts_with("requirements") && n.ends_with(".txt") => requirements(text),
        _ => Vec::new(),
    };
    Ok(deps)
}

fn parse_toml(path: &str, text: &str) -> Result<Toml, String> {
    text.parse::<toml::Table>()
        .map(Toml::Table)
        .map_err(|e| format!("{path}: invalid TOML, dependencies not extracted: {e}"))
}

fn parse_json(path: &str, text: &str) -> Result<Json, String> {
    serde_json::from_str(text)
        .map_err(|e| format!("{path}: invalid JSON, dependencies not extracted: {e}"))
}

fn cargo(doc: &Toml) -> Vec<Dependency> {
    let mut out = Vec::new();
    let sections = [
        ("dependencies", "normal"),
        ("dev-dependencies", "dev"),
        ("build-dependencies", "build"),
    ];
    let push_table = |table: Option<&Toml>, kind: &str, out: &mut Vec<Dependency>| {
        let Some(table) = table.and_then(Toml::as_table) else {
            return;
        };
        for (name, spec) in table {
            out.push(Dependency {
                ecosystem: "cargo",
                name: name.clone(),
                version_req: cargo_version(spec),
                kind: kind.to_owned(),
            });
        }
    };
    for (section, kind) in sections {
        push_table(doc.get(section), kind, &mut out);
    }
    if let Some(ws) = doc.get("workspace") {
        push_table(ws.get("dependencies"), "workspace", &mut out);
    }
    if let Some(targets) = doc.get("target").and_then(Toml::as_table) {
        for target in targets.values() {
            for (section, kind) in sections {
                push_table(target.get(section), kind, &mut out);
            }
        }
    }
    out
}

fn cargo_version(spec: &Toml) -> String {
    match spec {
        Toml::String(v) => v.clone(),
        Toml::Table(t) => {
            if let Some(v) = t.get("version").and_then(Toml::as_str) {
                v.to_owned()
            } else if t.get("workspace").and_then(Toml::as_bool) == Some(true) {
                "workspace".to_owned()
            } else if let Some(p) = t.get("path").and_then(Toml::as_str) {
                format!("path:{p}")
            } else if let Some(g) = t.get("git").and_then(Toml::as_str) {
                format!("git:{g}")
            } else {
                "*".to_owned()
            }
        }
        _ => "*".to_owned(),
    }
}

fn npm(doc: &Json) -> Vec<Dependency> {
    let mut out = Vec::new();
    for (section, kind) in [
        ("dependencies", "normal"),
        ("devDependencies", "dev"),
        ("peerDependencies", "peer"),
        ("optionalDependencies", "optional"),
    ] {
        if let Some(map) = doc.get(section).and_then(Json::as_object) {
            for (name, version) in map {
                out.push(Dependency {
                    ecosystem: "npm",
                    name: name.clone(),
                    version_req: version.as_str().unwrap_or("*").to_owned(),
                    kind: kind.to_owned(),
                });
            }
        }
    }
    out
}

fn pyproject(doc: &Toml) -> Vec<Dependency> {
    let mut out = Vec::new();
    let project = doc.get("project");
    let pep508_list = |list: Option<&Toml>, kind: &str, out: &mut Vec<Dependency>| {
        for item in list.and_then(Toml::as_array).into_iter().flatten() {
            if let Some(dep) = item.as_str().and_then(|s| pep508(s, kind)) {
                out.push(dep);
            }
        }
    };
    pep508_list(
        project.and_then(|p| p.get("dependencies")),
        "normal",
        &mut out,
    );
    if let Some(optional) = project
        .and_then(|p| p.get("optional-dependencies"))
        .and_then(Toml::as_table)
    {
        for list in optional.values() {
            pep508_list(Some(list), "optional", &mut out);
        }
    }
    if let Some(groups) = doc.get("dependency-groups").and_then(Toml::as_table) {
        for list in groups.values() {
            pep508_list(Some(list), "dev", &mut out);
        }
    }

    let poetry = doc.get("tool").and_then(|t| t.get("poetry"));
    let poetry_table = |table: Option<&Toml>, kind: &str, out: &mut Vec<Dependency>| {
        for (name, spec) in table.and_then(Toml::as_table).into_iter().flatten() {
            if name.eq_ignore_ascii_case("python") {
                continue;
            }
            out.push(Dependency {
                ecosystem: "pypi",
                name: name.clone(),
                version_req: cargo_version(spec),
                kind: kind.to_owned(),
            });
        }
    };
    poetry_table(
        poetry.and_then(|p| p.get("dependencies")),
        "normal",
        &mut out,
    );
    poetry_table(
        poetry.and_then(|p| p.get("dev-dependencies")),
        "dev",
        &mut out,
    );
    if let Some(groups) = poetry.and_then(|p| p.get("group")).and_then(Toml::as_table) {
        for group in groups.values() {
            poetry_table(group.get("dependencies"), "dev", &mut out);
        }
    }
    out
}

/// Parses the name and specifier from a PEP 508 requirement string.
fn pep508(s: &str, kind: &str) -> Option<Dependency> {
    let s = s.split(';').next()?.trim();
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .unwrap_or(s.len());
    let name = &s[..end];
    if name.is_empty() {
        return None;
    }
    let rest = s[end..].trim();
    // Skip extras such as `requests[socks]>=2`.
    let rest = if rest.starts_with('[') {
        rest.split_once(']').map_or("", |(_, r)| r).trim()
    } else {
        rest
    };
    Some(Dependency {
        ecosystem: "pypi",
        name: name.to_owned(),
        version_req: if rest.is_empty() {
            "*".to_owned()
        } else {
            rest.to_owned()
        },
        kind: kind.to_owned(),
    })
}

fn requirements(text: &str) -> Vec<Dependency> {
    text.lines()
        .map(|l| l.split(" #").next().unwrap_or("").trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('-'))
        .filter_map(|l| pep508(l, "normal"))
        .collect()
}

fn wally(doc: &Toml) -> Vec<Dependency> {
    let mut out = Vec::new();
    for (section, kind) in [
        ("dependencies", "normal"),
        ("server-dependencies", "server"),
        ("dev-dependencies", "dev"),
    ] {
        for (alias, spec) in doc
            .get(section)
            .and_then(Toml::as_table)
            .into_iter()
            .flatten()
        {
            // Wally specs look like `scope/name@^1.2.0`.
            let spec = spec.as_str().unwrap_or("");
            let (package, version) = spec.split_once('@').unwrap_or((spec, "*"));
            out.push(Dependency {
                ecosystem: "wally",
                name: if package.is_empty() {
                    alias.clone()
                } else {
                    package.to_owned()
                },
                version_req: version.to_owned(),
                kind: kind.to_owned(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(deps: &[Dependency]) -> Vec<(&str, &str, &str)> {
        deps.iter()
            .map(|d| (d.name.as_str(), d.version_req.as_str(), d.kind.as_str()))
            .collect()
    }

    #[test]
    fn parses_cargo() {
        let text = r#"
[dependencies]
serde = { version = "1", features = ["derive"] }
anyhow = "1.0"
local = { path = "../local" }
[dev-dependencies]
tempfile = "3"
[target.'cfg(windows)'.dependencies]
winapi = "0.3"
"#;
        let deps = parse("Cargo.toml", text).expect("parse");
        assert_eq!(
            pairs(&deps),
            vec![
                ("anyhow", "1.0", "normal"),
                ("local", "path:../local", "normal"),
                ("serde", "1", "normal"),
                ("tempfile", "3", "dev"),
                ("winapi", "0.3", "normal"),
            ]
        );
    }

    #[test]
    fn parses_package_json() {
        let deps = parse(
            "web/package.json",
            r#"{"dependencies":{"react":"^19.0.0"},"devDependencies":{"vitest":"^3"}}"#,
        )
        .expect("parse");
        assert_eq!(
            pairs(&deps),
            vec![("react", "^19.0.0", "normal"), ("vitest", "^3", "dev")]
        );
    }

    #[test]
    fn parses_python_manifests() {
        let deps = parse(
            "pyproject.toml",
            "[project]\ndependencies = [\"requests[socks]>=2.31; python_version>'3.8'\", \"rich\"]\n",
        )
        .expect("parse");
        assert_eq!(
            pairs(&deps),
            vec![("requests", ">=2.31", "normal"), ("rich", "*", "normal")]
        );
        let reqs =
            parse("requirements.txt", "# c\nflask==3.0  # web\n-r other.txt\n").expect("parse");
        assert_eq!(pairs(&reqs), vec![("flask", "==3.0", "normal")]);
    }

    #[test]
    fn parses_wally() {
        let deps = parse(
            "wally.toml",
            "[dependencies]\nPromise = \"evaera/promise@^4.0.0\"\n",
        )
        .expect("parse");
        assert_eq!(pairs(&deps), vec![("evaera/promise", "^4.0.0", "normal")]);
    }

    #[test]
    fn reports_malformed_manifest() {
        assert!(parse("Cargo.toml", "[dependencies\n").is_err());
    }
}

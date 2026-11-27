//! Best-effort mapping of import specifiers to indexed files.
//!
//! Resolution is purely lexical: it never consults package managers, `tsconfig` path aliases
//! or Python's `sys.path`. Unresolved imports are kept with a `NULL` target.

use std::collections::HashSet;

use super::lang::Lang;

/// Resolves `module`, imported from `from_path`, to a project-relative file path.
pub fn resolve(
    lang: Lang,
    from_path: &str,
    module: &str,
    is_mod_decl: bool,
    files: &HashSet<String>,
) -> Option<String> {
    let dir = parent_dir(from_path);
    match lang {
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => {
            if !module.starts_with('.') {
                return None;
            }
            const EXTS: &[&str] = &["ts", "tsx", "d.ts", "js", "jsx", "mjs", "cjs", "mts", "cts"];
            let base = join(dir, module)?;
            // TypeScript ESM code imports `./x.js` while the file on disk is `./x.ts`.
            let without_js = base
                .strip_suffix(".js")
                .or_else(|| base.strip_suffix(".mjs"))
                .or_else(|| base.strip_suffix(".cjs"));
            first_existing(files, &base, EXTS)
                .or_else(|| without_js.and_then(|b| first_existing(files, b, EXTS)))
        }
        Lang::Python => resolve_python(dir, module, files),
        Lang::Lua | Lang::Luau => {
            const EXTS: &[&str] = &["luau", "lua"];
            if module.starts_with("./") || module.starts_with("../") {
                let base = join(dir, module)?;
                return first_existing(files, &base, EXTS);
            }
            if let Some(rest) = module.strip_prefix("@self/") {
                let base = join(&strip_init(from_path), rest)?;
                return first_existing(files, &base, EXTS);
            }
            let dotted = module.replace('.', "/");
            [
                String::new(),
                dir.to_owned(),
                "src".to_owned(),
                "lua".to_owned(),
            ]
            .iter()
            .filter_map(|prefix| join(prefix, &dotted))
            .find_map(|base| first_existing(files, &base, EXTS))
        }
        Lang::Rust => resolve_rust(from_path, module, is_mod_decl, files),
    }
}

fn resolve_python(dir: &str, module: &str, files: &HashSet<String>) -> Option<String> {
    const EXTS: &[&str] = &["py", "pyi"];
    let dots = module.bytes().take_while(|&b| b == b'.').count();
    let rest = module[dots..].replace('.', "/");
    if dots > 0 {
        let mut base = dir.to_owned();
        for _ in 1..dots {
            base = parent_dir(&base).to_owned();
        }
        let base = if rest.is_empty() {
            base
        } else {
            join(&base, &rest)?
        };
        return first_existing(files, &base, EXTS);
    }
    ["", "src"]
        .iter()
        .filter_map(|prefix| join(prefix, &rest))
        .find_map(|base| first_existing(files, &base, EXTS))
}

fn resolve_rust(
    from_path: &str,
    module: &str,
    is_mod_decl: bool,
    files: &HashSet<String>,
) -> Option<String> {
    let dir = parent_dir(from_path);
    let file_name = from_path.rsplit('/').next().unwrap_or(from_path);
    let is_mod_root = matches!(file_name, "lib.rs" | "main.rs" | "mod.rs");
    if is_mod_decl {
        // `mod foo;` in lib.rs/main.rs/mod.rs looks next to the file; elsewhere in a
        // directory named after the file.
        let base_dir = if is_mod_root {
            dir.to_owned()
        } else {
            join(dir, file_name.trim_end_matches(".rs"))?
        };
        let candidate = join(&base_dir, module)?;
        return rust_module_file(files, &candidate);
    }

    let path = module.split('{').next()?.trim_end_matches("::");
    let mut segments: Vec<&str> = path.split("::").map(str::trim).collect();
    let root = match segments.first().copied() {
        Some("crate") => {
            segments.remove(0);
            crate_root(from_path, files)?
        }
        // The children of a non-`mod.rs` module `a/b.rs` live in `a/b/`; its parent's in `a/`.
        Some("super") => {
            segments.remove(0);
            if is_mod_root {
                parent_dir(dir).to_owned()
            } else {
                dir.to_owned()
            }
        }
        Some("self") => {
            segments.remove(0);
            if is_mod_root {
                dir.to_owned()
            } else {
                join(dir, file_name.trim_end_matches(".rs"))?
            }
        }
        _ => return None,
    };
    // Try the longest module path first: `crate::a::b::Item` may live in `a/b.rs` or `a.rs`.
    for len in (1..=segments.len()).rev() {
        let candidate = join(&root, &segments[..len].join("/"))?;
        if let Some(found) = rust_module_file(files, &candidate) {
            return Some(found);
        }
    }
    None
}

fn rust_module_file(files: &HashSet<String>, base: &str) -> Option<String> {
    let direct = format!("{base}.rs");
    if files.contains(&direct) {
        return Some(direct);
    }
    let nested = format!("{base}/mod.rs");
    files.contains(&nested).then_some(nested)
}

/// Nearest ancestor directory of `from_path` holding `lib.rs` or `main.rs`.
fn crate_root(from_path: &str, files: &HashSet<String>) -> Option<String> {
    let mut dir = parent_dir(from_path).to_owned();
    loop {
        for entry in ["lib.rs", "main.rs"] {
            if files.contains(&join(&dir, entry)?) {
                return Some(dir);
            }
        }
        if dir.is_empty() {
            return None;
        }
        dir = parent_dir(&dir).to_owned();
    }
}

fn first_existing(files: &HashSet<String>, base: &str, exts: &[&str]) -> Option<String> {
    if files.contains(base) {
        return Some(base.to_owned());
    }
    for ext in exts {
        let candidate = format!("{base}.{ext}");
        if files.contains(&candidate) {
            return Some(candidate);
        }
    }
    for index in ["index", "__init__", "init"] {
        for ext in exts {
            let candidate = format!("{base}/{index}.{ext}");
            if files.contains(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn strip_init(path: &str) -> String {
    let dir = parent_dir(path);
    let name = path.rsplit('/').next().unwrap_or(path);
    if name.starts_with("init.") {
        dir.to_owned()
    } else {
        let stem = name.split('.').next().unwrap_or(name);
        if dir.is_empty() {
            stem.to_owned()
        } else {
            format!("{dir}/{stem}")
        }
    }
}

/// Joins a relative specifier onto `dir`, resolving `.` and `..`. Returns `None` if the
/// result would escape the project root.
fn join(dir: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(list: &[&str]) -> HashSet<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn resolves_typescript_relative_imports() {
        let f = files(&["src/util.ts", "src/lib/index.tsx", "src/esm.ts"]);
        assert_eq!(
            resolve(Lang::TypeScript, "src/a.ts", "./util", false, &f).as_deref(),
            Some("src/util.ts")
        );
        assert_eq!(
            resolve(Lang::TypeScript, "src/a.ts", "./lib", false, &f).as_deref(),
            Some("src/lib/index.tsx")
        );
        assert_eq!(
            resolve(Lang::TypeScript, "src/a.ts", "./esm.js", false, &f).as_deref(),
            Some("src/esm.ts")
        );
        assert_eq!(
            resolve(Lang::TypeScript, "src/a.ts", "react", false, &f),
            None
        );
        assert_eq!(
            resolve(Lang::TypeScript, "a.ts", "../../x", false, &f),
            None
        );
    }

    #[test]
    fn resolves_python_imports() {
        let f = files(&["app/models.py", "app/__init__.py", "pkg/core/__init__.py"]);
        assert_eq!(
            resolve(Lang::Python, "app/views.py", ".models", false, &f).as_deref(),
            Some("app/models.py")
        );
        assert_eq!(
            resolve(Lang::Python, "app/views.py", ".", false, &f).as_deref(),
            Some("app/__init__.py")
        );
        assert_eq!(
            resolve(Lang::Python, "main.py", "pkg.core", false, &f).as_deref(),
            Some("pkg/core/__init__.py")
        );
    }

    #[test]
    fn resolves_rust_modules() {
        let f = files(&[
            "src/lib.rs",
            "src/db.rs",
            "src/index/mod.rs",
            "src/index/walk.rs",
        ]);
        assert_eq!(
            resolve(Lang::Rust, "src/lib.rs", "db", true, &f).as_deref(),
            Some("src/db.rs")
        );
        assert_eq!(
            resolve(Lang::Rust, "src/lib.rs", "index", true, &f).as_deref(),
            Some("src/index/mod.rs")
        );
        assert_eq!(
            resolve(Lang::Rust, "src/index/mod.rs", "walk", true, &f).as_deref(),
            Some("src/index/walk.rs")
        );
        assert_eq!(
            resolve(
                Lang::Rust,
                "src/index/walk.rs",
                "crate::db::{open, Db}",
                false,
                &f
            )
            .as_deref(),
            Some("src/db.rs")
        );
        assert_eq!(
            resolve(
                Lang::Rust,
                "src/index/walk.rs",
                "super::walk::WalkEntry",
                false,
                &f
            )
            .as_deref(),
            Some("src/index/walk.rs")
        );
        assert_eq!(
            resolve(Lang::Rust, "src/index/mod.rs", "super::db::open", false, &f).as_deref(),
            Some("src/db.rs")
        );
        assert_eq!(resolve(Lang::Rust, "src/db.rs", "std::fs", false, &f), None);
    }

    #[test]
    fn resolves_lua_requires() {
        let f = files(&["lib/util.lua", "src/Signal.luau", "src/net/init.luau"]);
        assert_eq!(
            resolve(Lang::Lua, "main.lua", "lib.util", false, &f).as_deref(),
            Some("lib/util.lua")
        );
        assert_eq!(
            resolve(Lang::Luau, "src/a.luau", "./Signal", false, &f).as_deref(),
            Some("src/Signal.luau")
        );
        assert_eq!(
            resolve(Lang::Luau, "src/a.luau", "./net", false, &f).as_deref(),
            Some("src/net/init.luau")
        );
    }
}

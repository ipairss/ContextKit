//! Language and file-kind detection, plus the tree-sitter queries used for extraction.
//!
//! Query conventions:
//! * `@def.<kind>` marks the whole definition node; `@name` marks its name.
//! * `@import` marks a module specifier; `@import.mod` marks a Rust `mod foo;` declaration.
//! * `@ref` marks the callee name of a call expression.
//! * Captures starting with `_` are helpers used only by predicates.

use std::fmt;

/// Languages with a tree-sitter grammar compiled into ContextKit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    JavaScript,
    TypeScript,
    Tsx,
    Python,
    Lua,
    Luau,
}

impl Lang {
    pub const ALL: [Lang; 7] = [
        Lang::Rust,
        Lang::JavaScript,
        Lang::TypeScript,
        Lang::Tsx,
        Lang::Python,
        Lang::Lua,
        Lang::Luau,
    ];

    /// Detects the language from a file path's extension.
    pub fn from_path(path: &str) -> Option<Self> {
        let ext = extension(path)?;
        Some(match ext.as_str() {
            "rs" => Lang::Rust,
            "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "py" | "pyi" => Lang::Python,
            "lua" => Lang::Lua,
            "luau" => Lang::Luau,
            _ => return None,
        })
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::JavaScript => "javascript",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::Python => "python",
            Lang::Lua => "lua",
            Lang::Luau => "luau",
        }
    }

    /// Markdown code-fence tag.
    pub const fn fence(self) -> &'static str {
        match self {
            Lang::Tsx => "tsx",
            other => other.name(),
        }
    }

    pub fn grammar(self) -> tree_sitter::Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::Lua => tree_sitter_lua::LANGUAGE.into(),
            Lang::Luau => tree_sitter_luau::LANGUAGE.into(),
        }
    }

    pub const fn query(self) -> &'static str {
        match self {
            Lang::Rust => RUST_QUERY,
            Lang::JavaScript => JS_QUERY,
            Lang::TypeScript | Lang::Tsx => TS_QUERY,
            Lang::Python => PYTHON_QUERY,
            Lang::Lua => LUA_QUERY,
            Lang::Luau => LUAU_QUERY,
        }
    }

    /// Node kinds whose definitions own nested definitions (impl blocks, classes, ...).
    /// The boolean says whether a function nested inside is reported as a method.
    pub fn container(self, node_kind: &str) -> Option<ContainerRule> {
        let rule = |name_field, methods| {
            Some(ContainerRule {
                name_field,
                methods,
            })
        };
        match (self, node_kind) {
            (Lang::Rust, "impl_item") => rule("type", true),
            (Lang::Rust, "trait_item") => rule("name", true),
            (Lang::Rust, "mod_item") => rule("name", false),
            (Lang::JavaScript | Lang::TypeScript | Lang::Tsx, "class_declaration" | "class") => {
                rule("name", true)
            }
            (
                Lang::TypeScript | Lang::Tsx,
                "abstract_class_declaration" | "interface_declaration",
            ) => rule("name", true),
            (Lang::Python, "class_definition") => rule("name", true),
            _ => None,
        }
    }
}

impl fmt::Display for Lang {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// How to name an enclosing container node.
#[derive(Debug, Clone, Copy)]
pub struct ContainerRule {
    pub name_field: &'static str,
    pub methods: bool,
}

/// Broad classification of an indexed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    Source,
    Doc,
    Config,
    Manifest,
    Text,
}

impl FileKind {
    pub fn detect(path: &str) -> Self {
        let name = path.rsplit('/').next().unwrap_or(path);
        let lower = name.to_ascii_lowercase();
        if is_manifest(&lower) {
            return FileKind::Manifest;
        }
        if Lang::from_path(path).is_some() {
            return FileKind::Source;
        }
        let ext = extension(path).unwrap_or_default();
        match ext.as_str() {
            "md" | "markdown" | "mdx" | "rst" | "adoc" | "txt" => FileKind::Doc,
            "toml" | "yaml" | "yml" | "json" | "jsonc" | "json5" | "ini" | "cfg" | "conf"
            | "properties" | "xml" | "plist" => FileKind::Config,
            _ if matches!(
                lower.as_str(),
                "dockerfile"
                    | "makefile"
                    | "justfile"
                    | ".editorconfig"
                    | ".gitignore"
                    | ".gitattributes"
                    | ".contextkitignore"
                    | ".prettierrc"
                    | ".eslintrc"
                    | "license"
                    | "readme"
            ) =>
            {
                if lower == "license" || lower == "readme" {
                    FileKind::Doc
                } else {
                    FileKind::Config
                }
            }
            _ => FileKind::Text,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            FileKind::Source => "source",
            FileKind::Doc => "doc",
            FileKind::Config => "config",
            FileKind::Manifest => "manifest",
            FileKind::Text => "text",
        }
    }
}

fn is_manifest(lower_name: &str) -> bool {
    matches!(
        lower_name,
        "cargo.toml"
            | "package.json"
            | "pyproject.toml"
            | "setup.cfg"
            | "pipfile"
            | "wally.toml"
            | "rokit.toml"
            | "aftman.toml"
            | "foreman.toml"
            | "default.project.json"
    ) || (lower_name.starts_with("requirements") && lower_name.ends_with(".txt"))
        || lower_name.ends_with(".rockspec")
}

fn extension(path: &str) -> Option<String> {
    let name = path.rsplit('/').next()?;
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

const RUST_QUERY: &str = r#"
(function_item name: (identifier) @name) @def.function
(function_signature_item name: (identifier) @name) @def.function
(struct_item name: (type_identifier) @name) @def.struct
(union_item name: (type_identifier) @name) @def.struct
(enum_item name: (type_identifier) @name) @def.enum
(trait_item name: (type_identifier) @name) @def.trait
(type_item name: (type_identifier) @name) @def.type
(const_item name: (identifier) @name) @def.constant
(static_item name: (identifier) @name) @def.constant
(macro_definition name: (identifier) @name) @def.macro
(mod_item name: (identifier) @name body: (declaration_list)) @def.module
(impl_item type: (_) @name) @def.impl

(use_declaration argument: (_) @import)
(mod_item name: (identifier) @import.mod !body)
(extern_crate_declaration name: (identifier) @import)

(call_expression function: (identifier) @ref)
(call_expression function: (field_expression field: (field_identifier) @ref))
(call_expression function: (scoped_identifier name: (identifier) @ref))
(macro_invocation macro: (identifier) @ref)
"#;

const JS_QUERY: &str = r#"
(function_declaration name: (identifier) @name) @def.function
(generator_function_declaration name: (identifier) @name) @def.function
(class_declaration name: (identifier) @name) @def.class
(method_definition name: (property_identifier) @name) @def.method
(variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)]) @def.function

(import_statement source: (string) @import)
(export_statement source: (string) @import)
((call_expression function: (identifier) @_fn arguments: (arguments . (string) @import))
  (#eq? @_fn "require"))

(call_expression function: (identifier) @ref)
(call_expression function: (member_expression property: (property_identifier) @ref))
(new_expression constructor: (identifier) @ref)
"#;

const TS_QUERY: &str = r#"
(function_declaration name: (identifier) @name) @def.function
(generator_function_declaration name: (identifier) @name) @def.function
(function_signature name: (identifier) @name) @def.function
(class_declaration name: (type_identifier) @name) @def.class
(abstract_class_declaration name: (type_identifier) @name) @def.class
(interface_declaration name: (type_identifier) @name) @def.interface
(type_alias_declaration name: (type_identifier) @name) @def.type
(enum_declaration name: (identifier) @name) @def.enum
(method_definition name: (property_identifier) @name) @def.method
(method_signature name: (property_identifier) @name) @def.method
(variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)]) @def.function

(import_statement source: (string) @import)
(export_statement source: (string) @import)
((call_expression function: (identifier) @_fn arguments: (arguments . (string) @import))
  (#eq? @_fn "require"))

(call_expression function: (identifier) @ref)
(call_expression function: (member_expression property: (property_identifier) @ref))
(new_expression constructor: (identifier) @ref)
"#;

const PYTHON_QUERY: &str = r#"
(function_definition name: (identifier) @name) @def.function
(class_definition name: (identifier) @name) @def.class

(import_statement name: (dotted_name) @import)
(import_statement name: (aliased_import name: (dotted_name) @import))
(import_from_statement module_name: (_) @import)

(call function: (identifier) @ref)
(call function: (attribute attribute: (identifier) @ref))
"#;

const LUA_QUERY: &str = r#"
(function_declaration name: (identifier) @name) @def.function
(function_declaration name: (dot_index_expression field: (identifier) @name)) @def.function
(function_declaration name: (method_index_expression method: (identifier) @name)) @def.method
(assignment_statement
  (variable_list . name: [(identifier) (dot_index_expression field: (identifier))] @name)
  (expression_list . value: (function_definition))) @def.function

((function_call name: (identifier) @_fn arguments: (arguments . (string) @import))
  (#eq? @_fn "require"))

(function_call name: (identifier) @ref)
(function_call name: (dot_index_expression field: (identifier) @ref))
(function_call name: (method_index_expression method: (identifier) @ref))
"#;

const LUAU_QUERY: &str = r#"
(function_declaration name: (identifier) @name) @def.function
(function_declaration name: (dot_index_expression field: (identifier) @name)) @def.function
(function_declaration name: (method_index_expression method: (identifier) @name)) @def.method
(type_definition "type" . (type) @name) @def.type

((function_call name: (identifier) @_fn arguments: (arguments . (string) @import))
  (#eq? @_fn "require"))

(function_call name: (identifier) @ref)
(function_call name: (dot_index_expression field: (identifier) @ref))
(function_call name: (method_index_expression method: (identifier) @ref))
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_query_compiles_against_its_grammar() {
        for lang in Lang::ALL {
            let grammar = lang.grammar();
            if let Err(e) = tree_sitter::Query::new(&grammar, lang.query()) {
                panic!("query for {lang} failed to compile: {e}");
            }
        }
    }

    #[test]
    fn detects_languages_and_kinds() {
        assert_eq!(Lang::from_path("src/main.rs"), Some(Lang::Rust));
        assert_eq!(Lang::from_path("a/b.tsx"), Some(Lang::Tsx));
        assert_eq!(Lang::from_path("init.luau"), Some(Lang::Luau));
        assert_eq!(Lang::from_path(".rs"), None);
        assert_eq!(FileKind::detect("Cargo.toml"), FileKind::Manifest);
        assert_eq!(FileKind::detect("docs/guide.md"), FileKind::Doc);
        assert_eq!(FileKind::detect("config/app.yaml"), FileKind::Config);
        assert_eq!(FileKind::detect("requirements-dev.txt"), FileKind::Manifest);
        assert_eq!(FileKind::detect("main.go"), FileKind::Text);
    }
}

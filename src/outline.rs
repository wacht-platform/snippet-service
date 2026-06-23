//! Multi-language structural outline via tree-sitter. Extracts the definitions
//! (functions, types, methods, …) of a source file with their signatures, line
//! numbers and the language's doc-comment standard. Bundled languages parse
//! in-process; unknown extensions return `None` so callers fall back to the
//! line-prefix heuristic (and, later, an on-demand grammar loader).

/// One declaration found in a file.
pub struct Symbol {
    pub kind: &'static str,
    pub signature: String,
    pub line: usize,
    pub doc: Option<String>,
    /// Nesting depth (methods inside a class/impl are deeper) for indentation.
    pub depth: usize,
}

/// Resolve a bundled tree-sitter language for a file extension.
fn bundled_language(ext: &str) -> Option<(&'static str, tree_sitter::Language)> {
    let (name, lang): (&str, tree_sitter::Language) = match ext {
        "rs" => ("rust", tree_sitter_rust::LANGUAGE.into()),
        "py" | "pyi" => ("python", tree_sitter_python::LANGUAGE.into()),
        "js" | "jsx" | "mjs" | "cjs" => ("javascript", tree_sitter_javascript::LANGUAGE.into()),
        "ts" | "mts" | "cts" => ("typescript", tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "tsx" => ("tsx", tree_sitter_typescript::LANGUAGE_TSX.into()),
        "go" => ("go", tree_sitter_go::LANGUAGE.into()),
        "java" => ("java", tree_sitter_java::LANGUAGE.into()),
        "c" | "h" => ("c", tree_sitter_c::LANGUAGE.into()),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => ("cpp", tree_sitter_cpp::LANGUAGE.into()),
        _ => return None,
    };
    Some((name, lang))
}

/// True when a file extension has a bundled grammar.
pub fn is_supported(ext: &str) -> bool {
    bundled_language(ext).is_some()
}

/// Map a tree-sitter node kind to a displayed symbol kind for a language.
fn def_kind(lang: &str, kind: &str) -> Option<&'static str> {
    let lang = if lang == "tsx" { "typescript" } else { lang };
    let k = match lang {
        "rust" => match kind {
            "function_item" => "fn",
            "struct_item" => "struct",
            "enum_item" => "enum",
            "trait_item" => "trait",
            "impl_item" => "impl",
            "mod_item" => "mod",
            "type_item" => "type",
            "const_item" => "const",
            "static_item" => "static",
            "macro_definition" => "macro",
            "union_item" => "union",
            _ => return None,
        },
        "python" => match kind {
            "function_definition" => "def",
            "class_definition" => "class",
            _ => return None,
        },
        "javascript" | "typescript" => match kind {
            "function_declaration" | "generator_function_declaration" => "function",
            "class_declaration" | "abstract_class_declaration" => "class",
            "method_definition" => "method",
            "interface_declaration" => "interface",
            "type_alias_declaration" => "type",
            "enum_declaration" => "enum",
            _ => return None,
        },
        "go" => match kind {
            "function_declaration" => "func",
            "method_declaration" => "method",
            "type_declaration" => "type",
            _ => return None,
        },
        "java" => match kind {
            "class_declaration" => "class",
            "interface_declaration" => "interface",
            "method_declaration" => "method",
            "constructor_declaration" => "constructor",
            "enum_declaration" => "enum",
            "record_declaration" => "record",
            _ => return None,
        },
        "c" => match kind {
            "function_definition" => "fn",
            "struct_specifier" => "struct",
            "enum_specifier" => "enum",
            "union_specifier" => "union",
            "type_definition" => "typedef",
            _ => return None,
        },
        "cpp" => match kind {
            "function_definition" => "fn",
            "class_specifier" => "class",
            "struct_specifier" => "struct",
            "enum_specifier" => "enum",
            "namespace_definition" => "namespace",
            _ => return None,
        },
        _ => return None,
    };
    Some(k)
}

fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() > n {
        s.chars().take(n).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

fn slice<'a>(src: &'a [u8], range: std::ops::Range<usize>) -> &'a str {
    std::str::from_utf8(src.get(range).unwrap_or(&[])).unwrap_or("")
}

/// The declaration text up to its body, whitespace-collapsed — i.e. the signature.
fn signature_of(node: tree_sitter::Node, src: &[u8]) -> String {
    let end = node
        .child_by_field_name("body")
        .map(|b| b.start_byte())
        .unwrap_or_else(|| node.end_byte())
        .min(node.end_byte());
    let raw = slice(src, node.start_byte()..end);
    clip(&raw.split_whitespace().collect::<Vec<_>>().join(" "), 160)
}

/// Strip comment markers (///, //, /* */, *, #) and collapse to one line.
fn clean_doc(s: &str) -> String {
    s.lines()
        .map(|l| {
            l.trim()
                .trim_start_matches("///")
                .trim_start_matches("//!")
                .trim_start_matches("//")
                .trim_start_matches("/**")
                .trim_start_matches("/*")
                .trim_end_matches("*/")
                .trim_start_matches('*')
                .trim_start_matches('#')
                .trim()
        })
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Python docstring = the first string statement inside the body.
fn python_docstring(node: tree_sitter::Node, src: &[u8]) -> Option<String> {
    let body = node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() == "expression_statement" {
            let mut inner_cursor = child.walk();
            for inner in child.children(&mut inner_cursor) {
                if inner.kind() == "string" {
                    let raw = slice(src, inner.byte_range());
                    let cleaned = raw.trim_matches(|c| c == '"' || c == '\'').trim();
                    return (!cleaned.is_empty()).then(|| clip(cleaned, 220));
                }
            }
        }
        break; // only the first statement can be a docstring
    }
    None
}

/// The doc comment for a definition: a Python docstring, or the contiguous run of
/// comment lines immediately above it (Rust ///, JSDoc /** */, Go/Java/C //, …).
fn doc_of(node: tree_sitter::Node, src: &[u8], lang: &str) -> Option<String> {
    if lang == "python" {
        return python_docstring(node, src);
    }
    let mut comments: Vec<String> = Vec::new();
    let mut next_row = node.start_position().row;
    let mut sib = node.prev_sibling();
    while let Some(s) = sib {
        if !s.kind().contains("comment") {
            break;
        }
        // Must be directly above (no blank-line gap) to count as a doc comment.
        if next_row.saturating_sub(s.end_position().row) > 1 {
            break;
        }
        comments.push(slice(src, s.byte_range()).to_string());
        next_row = s.start_position().row;
        sib = s.prev_sibling();
    }
    if comments.is_empty() {
        return None;
    }
    comments.reverse();
    let cleaned = clean_doc(&comments.join("\n"));
    (!cleaned.is_empty()).then(|| clip(&cleaned, 220))
}

fn walk(node: tree_sitter::Node, src: &[u8], lang: &str, depth: usize, out: &mut Vec<Symbol>) {
    let mut child_depth = depth;
    if let Some(kind) = def_kind(lang, node.kind()) {
        let signature = signature_of(node, src);
        if !signature.is_empty() {
            out.push(Symbol {
                kind,
                signature,
                line: node.start_position().row + 1,
                doc: doc_of(node, src, lang),
                depth,
            });
            child_depth = depth + 1;
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, src, lang, child_depth, out);
    }
}

/// Outline a source file by extension. `None` = no bundled grammar (fall back).
pub fn outline_source(ext: &str, source: &str) -> Option<Vec<Symbol>> {
    let (lang_name, language) = bundled_language(ext)?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(source, None)?;
    let mut out = Vec::new();
    walk(tree.root_node(), source.as_bytes(), lang_name, 0, &mut out);
    Some(out)
}

//! Terraform / HCL parser plugin — full-parse mode.
//!
//! Handles `.tf`, `.tfvars`, and `.hcl` files.
//! The plugin parses source with Tree-sitter inside Rust/Wasm.

use intentumdiff_plugin_sdk::{
    cst::CstNode,
    hash::structural_hash_with_memo,
    tree::{SemanticNode, SemanticNodeBuilder},
};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentumdiff::plugin::parser::ExamplePair;
use crate::exports::intentumdiff::plugin::parser::Guest;
use crate::exports::intentumdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentumdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct TerraformParser;

const TRIVIA: &[&str] = &["comment", "whitespace"];

const SEMANTIC_TYPES: &[&str] = &[
    // Root
    "config_file",
    "body",
    // The primary HCL construct: resource "type" "name" { }
    "block",
    // Attributes: key = value
    "attribute",
    // Expressions
    "expression",
    "literal_value",
    "template_literal",
    "template_expr",
    "heredoc_template",
    // References and function calls
    "function_call",
    "variable_expr",
    "get_attr",
    "index_expr",
    "legacy_index",
    "splat",
    "attr_splat",
    "full_splat",
    // Collection expressions
    "object_expr",
    "object",
    "object_elem",
    "tuple_expr",
    "tuple",
    // For expressions
    "for_expr",
    "for_tuple_expr",
    "for_object_expr",
    "for_intro",
    "for_cond",
    // Conditional / operations
    "conditional",
    "binary_op",
    "unary_op",
    // Type references
    "type_expr",
    "string_lit",
    "number_lit",
    "bool_lit",
    "null_lit",
];

fn is_semantic(node_type: &str) -> bool {
    SEMANTIC_TYPES.contains(&node_type)
}

/// Collect all string/identifier children of a node as a space-joined label.
/// Used for blocks: `resource "aws_instance" "web"` → `"resource aws_instance web"`.
fn block_label(node: &CstNode) -> String {
    let mut parts: Vec<String> = Vec::new();
    for child in &node.children {
        match child.node_type.as_str() {
            "identifier" | "string_lit" => {
                let t = child.text_or_empty().trim_matches('"').to_string();
                if !t.is_empty() {
                    parts.push(t);
                }
            }
            "body" | "{" | "}" => {}
            _ => {
                // Could be a quoted string with inner node — check grandchildren
                if let Some(inner) = child.children.first() {
                    if inner.is_leaf() {
                        let t = inner.text_or_empty().trim_matches('"').to_string();
                        if !t.is_empty() {
                            parts.push(t);
                        }
                    }
                }
            }
        }
        if parts.len() >= 4 {
            break; // cap label length
        }
    }
    if parts.is_empty() {
        node.node_type.clone()
    } else {
        parts.join(" ")
    }
}

fn label_for(node: &CstNode) -> String {
    if node.is_leaf() {
        return node.text_or_empty().to_string();
    }
    // Literal containers label with their captured source text (SDK-shared, issue #47).
    if let Some(label) = intentumdiff_plugin_sdk::ts_convert::literal_label(node) {
        return label;
    }
    match node.node_type.as_str() {
        "block" => block_label(node),
        "attribute" => {
            if let Some(first) = node.children.first() {
                let t = first.text_or_empty().to_string();
                if !t.is_empty() {
                    return t;
                }
            }
            node.node_type.clone()
        }
        "function_call" => {
            if let Some(first) = node.children.first() {
                return first.text_or_empty().to_string();
            }
            node.node_type.clone()
        }
        "variable_expr" => {
            // First identifier child is the variable name
            for child in &node.children {
                if child.node_type == "identifier" {
                    return child.text_or_empty().to_string();
                }
            }
            node.text_or_empty().to_string()
        }
        "get_attr" => {
            // expression.attr → join with "."
            let mut parts: Vec<String> = Vec::new();
            for child in &node.children {
                if child.node_type == "identifier" || child.is_leaf() {
                    let t = child.text_or_empty().to_string();
                    if !t.is_empty() && t != "." {
                        parts.push(t);
                    }
                }
            }
            parts.join(".")
        }
        _ => {
            // Try identifier or string_lit child
            for child in &node.children {
                if matches!(child.node_type.as_str(), "identifier" | "string_lit") {
                    let t = child.text_or_empty().trim_matches('"').to_string();
                    if !t.is_empty() {
                        return t;
                    }
                }
            }
            node.node_type.clone()
        }
    }
}

fn is_class_like(node_type: &str) -> bool {
    node_type == "block" // resource, module, data, variable, locals, output, terraform
}

fn is_method_like(_node_type: &str) -> bool {
    false // HCL has no functions/methods in the user-defined sense
}

fn convert(
    node: &CstNode,
    id_prefix: &str,
    parent_class: Option<&str>,
    memo: &mut std::collections::HashMap<usize, String>,
) -> Option<SemanticNode> {
    // Class context threads for descendants but never sets parent_type here
    // (no method-like nodes in this grammar's review model).
    convert_semantic_classed(
        node,
        id_prefix,
        parent_class,
        memo,
        &|t| TRIVIA.contains(&t),
        &is_semantic,
        &is_class_like,
        &|_| false,
        &label_for,
    )
}



use intentumdiff_plugin_sdk::ts_convert::{convert_semantic_classed, node_to_cst};

fn parse_source(source: &str) -> Result<CstNode, String> {
    let mut parser = tree_sitter::Parser::new();
    let lang = tree_sitter_hcl::LANGUAGE.into();
    parser
        .set_language(&lang)
        .map_err(|_| "Failed to load terraform grammar".to_string())?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "Parse failed".to_string())?;
    Ok(node_to_cst(tree.root_node(), source.as_bytes()))
}

fn process_impl(source: &str) -> String {
    let root: CstNode = match parse_source(source) {
        Ok(n) => n,
        Err(e) => return format!(r#"{{\"error\":\"{}\"}}"#, e),
    };
    let mut memo: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let sem = match convert(&root, "0", None, &mut memo) {
        Some(n) => n,
        None => return r#"{"error":"Empty semantic tree"}"#.to_string(),
    };
    match serde_json::to_string(&sem) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for TerraformParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "hcl".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        let lower = filename.to_lowercase();
        if lower.ends_with(".tf") || lower.ends_with(".tfvars") || lower.ends_with(".hcl") {
            return "hcl".to_string();
        }
        String::new()
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        TRIVIA.iter().map(|s| s.to_string()).collect()
    }
    fn language_ids() -> Vec<String> {
        vec!["hcl".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }

    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "resource \"aws_instance\" \"web\" {\n  ami           = \"ami-0c55b159cbfafe1f0\"\n  instance_type = \"t2.micro\"\n}\n".to_string(),
            new: "resource \"aws_instance\" \"web\" {\n  ami           = \"ami-0c55b159cbfafe1f0\"\n  instance_type = \"t3.small\"\n\n  tags = {\n    Name        = \"web-server\"\n    Environment = \"production\"\n  }\n}\n\noutput \"instance_ip\" {\n  value = aws_instance.web.public_ip\n}\n".to_string(),
        }
    }
}
export!(TerraformParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentumdiff::plugin::parser::Guest;
    use intentumdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!TerraformParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = TerraformParser::grammar_id();
        let ids = TerraformParser::language_ids();
        assert!(
            ids.contains(&gid),
            "language_ids {:?} must contain {:?}",
            ids,
            gid
        );
    }

    #[test]
    fn detect_language_known_ext() {
        let r = TerraformParser::detect_language("test.tf".to_string(), "".to_string());
        assert_eq!(r.as_str(), "hcl");
    }

    #[test]
    fn detect_language_unknown_ext() {
        let r = TerraformParser::detect_language(
            "test.xyz_notareal_ext_9z8y".to_string(),
            "".to_string(),
        );
        assert_eq!(r.as_str(), "");
    }

    #[test]
    fn parser_mode_is_full_parse() {
        assert!(matches!(
            TerraformParser::get_parser_mode(),
            ParserMode::FullParse
        ));
    }

    #[test]
    fn process_impl_accepts_raw_example_source() {
        let example = TerraformParser::example(TerraformParser::grammar_id());
        let out = process_impl(&example.old);
        t::assert_valid_json(&out, "process(raw example)");
        assert!(!out.contains("\"error\""), "{out}");
    }
    #[test]
    fn process_impl_empty_returns_valid_json() {
        let out = process_impl("");
        t::assert_valid_json(&out, "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        let out = process_impl("   \n  ");
        t::assert_valid_json(&out, "process(whitespace)");
    }
}

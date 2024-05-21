mod resolve;

use resolve::Hoverable;
use schema_cache::SchemaCache;
use text_size::{TextRange, TextSize};

pub struct HoverParams {
    pub position: text_size::TextSize,
    pub source: String,
    pub ast: Option<sql_parser::EnrichedAst>,
    pub tree: tree_sitter::Tree,
    pub schema_cache: SchemaCache,
}

#[derive(Debug)]
pub struct HoverResult {
    range: Option<TextRange>,
    content: String,
}

pub fn hover(params: HoverParams) -> Option<HoverResult> {
    let elem = if params.ast.is_some() {
        resolve::resolve_from_enriched_ast(params.position, params.ast.unwrap())
    } else {
        resolve::resolve_from_tree_sitter(params.position, params.tree, &params.source)
    };

    if elem.is_none() {
        return None;
    }

    match elem.unwrap() {
        Hoverable::Relation(r) => {
            let table = params.schema_cache.find_table(&r.name, r.schema.as_deref());

            table.map(|t| HoverResult {
                range: Some(r.range),
                content: if t.comment.is_some() {
                    format!("{}\n{}", t.name, t.comment.as_ref().unwrap())
                } else {
                    t.name.clone()
                },
            })
        }
        _ => None,
    }
}

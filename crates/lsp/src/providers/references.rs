use crate::document::Document;
use crate::query_cache;
use crate::server::LspServerStateSnapshot;
use crate::treesitter_utils::{
    lsp_position_to_tree_sitter_point_range, text_for_tree_sitter_node,
    tree_sitter_node_to_lsp_range,
};
use anyhow::{Context, Result};
use lsp_types::Location;
use ropey::Rope;
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tracing::debug;
use tree_sitter::StreamingIterator;
use tree_sitter_beancount::NodeKind;
use tree_sitter_beancount::tree_sitter;

fn node_text_at_position(
    tree: &tree_sitter::Tree,
    content: &Rope,
    position: lsp_types::Position,
) -> Result<Option<String>> {
    let (start, end) = lsp_position_to_tree_sitter_point_range(content, position)?;
    let Some(node) = tree
        .root_node()
        .named_descendant_for_point_range(start, end)
    else {
        return Ok(None);
    };

    Ok(Some(text_for_tree_sitter_node(content, &node)))
}

/// Resolve the `account` node at POSITION, if the position is on one.
///
/// Two point ranges are tried, because no single one covers both ends of the
/// token. The widened `pos-1..pos` range from
/// `lsp_position_to_tree_sitter_point_range` resolves a cursor sitting just past
/// the end of the account, but on the account's FIRST character it reaches back
/// into the preceding whitespace and yields the enclosing `posting`/`open`
/// instead; the exact point covers that case. A cursor resting on the first
/// character is the normal state in a modal editor, so it has to work.
fn account_node_at_position<'t>(
    tree: &'t tree_sitter::Tree,
    content: &Rope,
    position: lsp_types::Position,
) -> Result<Option<tree_sitter::Node<'t>>> {
    let (wide_start, point) = lsp_position_to_tree_sitter_point_range(content, position)?;
    for (start, end) in [(wide_start, point), (point, point)] {
        if let Some(node) = tree
            .root_node()
            .named_descendant_for_point_range(start, end)
            && NodeKind::Account == node.kind().into()
        {
            return Ok(Some(node));
        }
    }
    Ok(None)
}

/// Provider function for `textDocument/prepareRename`.
///
/// Tells the client the exact range it should offer for editing instead of
/// letting it guess. Guessing is what broke account renames: a client that falls
/// back to its own word/symbol notion stops at the `:` separators and offers
/// only the last segment, while `rename` below replaces the whole account -- so
/// accepting the prompt silently truncated `Assets:CurrentAssets:Checking` to
/// `Checking`.
pub(crate) fn prepare_rename(
    snapshot: LspServerStateSnapshot,
    params: lsp_types::PrepareRenameParams,
) -> Result<Option<lsp_types::PrepareRenameResult>> {
    let uri = &params.text_document_position_params.text_document.uri;
    let (tree, doc) = match snapshot.tree_and_document_for_uri(uri) {
        Ok(v) => v,
        Err(e) => {
            debug!("PrepareRename: failed to get tree/document for uri: {e}");
            return Ok(None);
        }
    };

    let content = doc.content.clone();
    let position = params.text_document_position_params.position;
    let Some(node) = account_node_at_position(tree, &content, position).with_context(|| {
        format!(
            "failed to get account node at position for uri: {}",
            uri.as_str()
        )
    })?
    else {
        // Not on an account. `None` rather than an error is what the spec asks
        // for, and lets the client refuse up front instead of running a rename
        // that would match nothing.
        return Ok(None);
    };

    let range = tree_sitter_node_to_lsp_range(&content, &node);
    let placeholder = text_for_tree_sitter_node(&content, &node);
    Ok(Some(
        lsp_types::PrepareRenamePlaceholder::new(range, placeholder).into(),
    ))
}

/// Provider function for `textDocument/references`.
pub(crate) fn references(
    snapshot: LspServerStateSnapshot,
    params: lsp_types::ReferenceParams,
) -> Result<Option<Vec<lsp_types::Location>>> {
    let uri = &params.text_document_position_params.text_document.uri;
    let (tree, doc) = match snapshot.tree_and_document_for_uri(uri) {
        Ok(v) => v,
        Err(e) => {
            debug!("References: failed to get tree/document for uri: {e}");
            return Ok(None);
        }
    };
    let content = doc.content.clone();

    // Keep behavior consistent: references only works on open documents.
    let position = params.text_document_position_params.position;
    let Some(node_text) = node_text_at_position(tree, &content, position).with_context(|| {
        format!(
            "failed to get node text at position for uri: {}",
            uri.as_str()
        )
    })?
    else {
        return Ok(None);
    };

    let locs = find_references(
        &snapshot.forest,
        &snapshot.open_docs,
        &snapshot.forest_content,
        &node_text,
    );
    Ok(Some(locs))
}

/// Provider function for `textDocument/rename`.
///
/// Renames the account under the cursor and, because beancount account names are
/// hierarchical, every sub-account beneath it -- see [`find_account_matches`].
#[allow(clippy::mutable_key_type)]
pub(crate) fn rename(
    snapshot: LspServerStateSnapshot,
    params: lsp_types::RenameParams,
) -> Result<Option<lsp_types::WorkspaceEdit>> {
    let uri = &params.text_document_position_params.text_document.uri;
    let (tree, doc) = match snapshot.tree_and_document_for_uri(uri) {
        Ok(v) => v,
        Err(e) => {
            debug!("Rename: failed to get tree/document for uri: {e}");
            return Ok(None);
        }
    };

    let content = doc.content.clone();
    let position = params.text_document_position_params.position;
    // Account-only, so a client that skips `prepareRename` cannot prefix-match
    // against some unrelated token.
    let Some(node) = account_node_at_position(tree, &content, position).with_context(|| {
        format!(
            "failed to get account node at position for uri: {}",
            uri.as_str()
        )
    })?
    else {
        return Ok(None);
    };
    let node_text = text_for_tree_sitter_node(&content, &node);

    let matches = find_account_matches(
        &snapshot.forest,
        &snapshot.open_docs,
        &snapshot.forest_content,
        &node_text,
        true,
    );
    let new_name = params.new_name;

    // Group locations by URI string to avoid mutable key type warning
    let mut grouped_locs: std::collections::HashMap<String, Vec<(lsp_types::Location, String)>> =
        std::collections::HashMap::new();
    for (loc, matched) in matches {
        grouped_locs
            .entry(loc.uri.to_string())
            .or_default()
            .push((loc, matched));
    }

    let mut changes: std::collections::HashMap<lsp_types::Uri, Vec<lsp_types::TextEdit>> =
        std::collections::HashMap::new();
    for (uri_str, locations) in grouped_locs {
        let uri = match lsp_types::Uri::from_str(&uri_str) {
            Ok(uri) => uri,
            Err(e) => {
                debug!("Failed to parse URI string {}: {}", uri_str, e);
                continue;
            }
        };
        let mut edits: Vec<_> = locations
            .into_iter()
            .map(|(l, matched)| {
                // Keep whatever sits below the renamed account: renaming `A:B`
                // to `A:C` must rewrite `A:B:Sub` as `A:C:Sub`. `matched` starts
                // with `node_text` by construction, so slicing at its byte
                // length always lands on a char boundary; an exact match leaves
                // an empty suffix.
                let text = format!("{new_name}{}", &matched[node_text.len()..]);
                lsp_types::TextEdit::new(l.range, text)
            })
            .collect();
        // Send edits ordered from the back so we do not invalidate following positions.
        edits.sort_by_key(|edit| edit.range.start);
        edits.reverse();
        changes.insert(uri, edits);
    }
    Ok(Some(lsp_types::WorkspaceEdit::new(
        Some(changes),
        None,
        None,
    )))
}

/// Find all references to a given text in the project using tree-sitter queries.
///
/// Exact matches only -- a sub-account is not a reference to its parent.
fn find_references(
    forest: &HashMap<PathBuf, Arc<tree_sitter::Tree>>,
    open_docs: &HashMap<PathBuf, Document>,
    forest_content: &HashMap<PathBuf, Arc<Rope>>,
    node_text: &str,
) -> Vec<lsp_types::Location> {
    find_account_matches(forest, open_docs, forest_content, node_text, false)
        .into_iter()
        .map(|(loc, _)| loc)
        .collect()
}

/// Find every account node in the project whose text is NODE_TEXT or, when
/// INCLUDE_SUBACCOUNTS, a `:`-separated descendant of it. Each hit carries the
/// text that matched, which `rename` needs in order to preserve the part of the
/// name below the account being renamed.
///
/// The trailing `:` in the prefix is load-bearing: testing `starts_with(node_text)`
/// alone would also catch an unrelated sibling such as `Assets:CheckingOld`.
fn find_account_matches(
    forest: &HashMap<PathBuf, Arc<tree_sitter::Tree>>,
    open_docs: &HashMap<PathBuf, Document>,
    forest_content: &HashMap<PathBuf, Arc<Rope>>,
    node_text: &str,
    include_subaccounts: bool,
) -> Vec<(lsp_types::Location, String)> {
    let query = query_cache::account_query();
    let capture_account = query
        .capture_index_for_name("account")
        .expect("account should be captured");
    let child_prefix = format!("{node_text}:");

    forest
        .iter()
        .flat_map(|(url, tree)| {
            let (rope, text) = if let Some(doc) = open_docs.get(url) {
                let rope = doc.content.clone();
                let text = rope.to_string();
                (rope, text)
            } else if let Some(stored) = forest_content.get(url) {
                let rope = (**stored).clone();
                let text = rope.to_string();
                (rope, text)
            } else {
                debug!("No content available for: {:?}", url);
                return vec![];
            };

            let source = text.as_bytes();

            let mut query_cursor = tree_sitter::QueryCursor::new();
            let mut matches = query_cursor.matches(query, tree.root_node(), source);
            let mut results = Vec::new();
            while let Some(m) = matches.next() {
                if let Some(node) = m.nodes_for_capture_index(capture_account).next() {
                    let m_text = node.utf8_text(source).expect("");
                    if m_text == node_text
                        || (include_subaccounts && m_text.starts_with(&child_prefix))
                    {
                        results.push((url.clone(), rope.clone(), node, m_text.to_string()));
                    }
                }
            }

            results
        })
        .filter_map(
            |(url, rope, node, m_text): (PathBuf, Rope, tree_sitter::Node, String)| {
                let uri = lsp_types::Uri::from_file_path(&url).ok()?;
                let range = tree_sitter_node_to_lsp_range(&rope, &node);
                Some((Location::new(uri, range), m_text))
            },
        )
        .collect::<Vec<_>>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beancount_data::BeancountData;
    use crate::config::Config;
    use std::collections::HashMap;

    struct TestState {
        snapshot: LspServerStateSnapshot,
        path: PathBuf,
    }

    impl TestState {
        fn new(content: &str) -> anyhow::Result<Self> {
            let path = std::env::current_dir()?.join("test.beancount");
            let rope_content = ropey::Rope::from_str(content);

            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&tree_sitter_beancount::language())?;
            let tree = parser.parse(content, None).unwrap();

            let mut forest = HashMap::new();
            forest.insert(path.clone(), Arc::new(tree.clone()));

            let mut open_docs = HashMap::new();
            open_docs.insert(
                path.clone(),
                Document {
                    content: rope_content.clone(),
                    version: 0,
                },
            );

            let mut beancount_data = HashMap::new();
            beancount_data.insert(
                path.clone(),
                Arc::new(BeancountData::new(&tree, &rope_content)),
            );

            let config = Config::new(path.clone());

            Ok(Self {
                snapshot: LspServerStateSnapshot {
                    forest: Arc::new(forest),
                    forest_content: Arc::new(HashMap::new()),
                    open_docs: Arc::new(open_docs),
                    beancount_data: Arc::new(beancount_data),
                    config,
                    checker: None,
                },
                path,
            })
        }
    }

    #[test]
    fn test_find_references_single_account() {
        let content = r#"
2024-01-01 open Assets:Checking
2024-01-02 * "Test"
  Assets:Checking  100.00 USD
  Expenses:Food   -100.00 USD
"#;
        let state = TestState::new(content).unwrap();
        let locs = find_references(
            &state.snapshot.forest,
            &state.snapshot.open_docs,
            &state.snapshot.forest_content,
            "Assets:Checking",
        );

        assert_eq!(locs.len(), 2); // open + posting
        assert!(locs[0].range.start.line == 1 || locs[1].range.start.line == 1);
        assert!(locs[0].range.start.line == 3 || locs[1].range.start.line == 3);
    }

    #[test]
    fn test_find_references_no_matches() {
        let content = r#"
2024-01-01 open Assets:Checking
2024-01-02 * "Test"
  Assets:Checking  100.00 USD
"#;
        let state = TestState::new(content).unwrap();
        let locs = find_references(
            &state.snapshot.forest,
            &state.snapshot.open_docs,
            &state.snapshot.forest_content,
            "Assets:Nonexistent",
        );

        assert_eq!(locs.len(), 0);
    }

    #[test]
    fn test_find_references_multiple_files() {
        let content1 = r#"
2024-01-01 open Assets:Bank
2024-01-02 * "Test"
  Assets:Bank  100.00 USD
"#;
        let content2 = r#"
2024-01-03 * "Another"
  Assets:Bank  50.00 USD
"#;
        let path1 = std::env::current_dir().unwrap().join("test1.beancount");
        let path2 = std::env::current_dir().unwrap().join("test2.beancount");

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_beancount::language())
            .unwrap();

        let tree1 = parser.parse(content1, None).unwrap();
        let tree2 = parser.parse(content2, None).unwrap();

        let mut forest = HashMap::new();
        forest.insert(path1.clone(), Arc::new(tree1));
        forest.insert(path2.clone(), Arc::new(tree2));

        let mut open_docs = HashMap::new();
        open_docs.insert(
            path1,
            Document {
                content: ropey::Rope::from_str(content1),
                version: 0,
            },
        );
        open_docs.insert(
            path2,
            Document {
                content: ropey::Rope::from_str(content2),
                version: 0,
            },
        );

        let locs = find_references(&forest, &open_docs, &HashMap::new(), "Assets:Bank");

        assert_eq!(locs.len(), 3); // open in file1 + posting in file1 + posting in file2
    }

    #[test]
    fn test_references_handler() {
        let content = r#"
2024-01-01 open Assets:Checking
2024-01-02 * "Test"
  Assets:Checking  100.00 USD
  Expenses:Food   -100.00 USD
"#;
        let state = TestState::new(content).unwrap();

        let uri = lsp_types::Uri::from_file_path(&state.path).unwrap();
        let params = lsp_types::ReferenceParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri },
                position: lsp_types::Position {
                    line: 1,
                    character: 20,
                }, // Position in "Assets:Checking"
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: lsp_types::ReferenceContext {
                include_declaration: true,
            },
        };

        let result = references(state.snapshot, params).unwrap();
        assert!(result.is_some());
        let locs = result.unwrap();
        assert_eq!(locs.len(), 2); // open + posting
    }

    #[test]
    #[allow(clippy::mutable_key_type)]
    fn test_rename_handler() {
        let content = r#"
2024-01-01 open Assets:Checking
2024-01-02 * "Test"
  Assets:Checking  100.00 USD
  Expenses:Food   -100.00 USD
"#;
        let state = TestState::new(content).unwrap();

        let uri = lsp_types::Uri::from_file_path(&state.path).unwrap();
        let params = lsp_types::RenameParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: lsp_types::Position {
                    line: 1,
                    character: 20,
                },
            },
            new_name: "Assets:Bank".to_string(),
            work_done_progress_params: Default::default(),
        };

        let result = rename(state.snapshot, params).unwrap();
        assert!(result.is_some());
        let edit = result.unwrap();
        assert!(edit.changes.is_some());
        let changes = edit.changes.unwrap();
        assert_eq!(changes.len(), 1);
        let edits = changes.get(&uri).unwrap();
        assert_eq!(edits.len(), 2); // Rename in both locations
        assert_eq!(edits[0].new_text, "Assets:Bank");
        assert_eq!(edits[1].new_text, "Assets:Bank");
    }

    // ---- prepareRename + sub-account cascade -------------------------------

    /// Hierarchical fixture: a parent account, a sub-account of it, and a
    /// sibling whose name merely *starts with* the parent's.
    const HIER: &str = r#"
2024-01-01 open Assets:CurrentAssets:CheckingAccount
2024-01-01 open Assets:CurrentAssets:CheckingAccount:DebitOrders
2024-01-01 open Assets:CurrentAssets:CheckingAccountOld
2024-01-02 * "Test"
  Assets:CurrentAssets:CheckingAccount  100.00 USD
  Assets:CurrentAssets:CheckingAccount:DebitOrders  -60.00 USD
  Assets:CurrentAssets:CheckingAccountOld  -40.00 USD
"#;

    const PARENT: &str = "Assets:CurrentAssets:CheckingAccount";
    const CHILD: &str = "Assets:CurrentAssets:CheckingAccount:DebitOrders";
    const SIBLING: &str = "Assets:CurrentAssets:CheckingAccountOld";
    const NEW: &str = "Assets:CurrentAssets:TransactionalAccount";

    /// Position OFFSET characters into the first occurrence of NEEDLE on LINE.
    /// The fixtures are ASCII, so a byte offset is also the UTF-16 offset LSP wants.
    fn pos_in(content: &str, line: u32, needle: &str, offset: u32) -> lsp_types::Position {
        let text = content.lines().nth(line as usize).expect("line exists");
        let col = text.find(needle).expect("needle on line") as u32;
        lsp_types::Position {
            line,
            character: col + offset,
        }
    }

    fn prepare_rename_at(
        content: &str,
        position: lsp_types::Position,
    ) -> Option<lsp_types::PrepareRenameResult> {
        let state = TestState::new(content).unwrap();
        let uri = lsp_types::Uri::from_file_path(&state.path).unwrap();
        prepare_rename(
            state.snapshot,
            lsp_types::PrepareRenameParams {
                work_done_progress_params: Default::default(),
                text_document_position_params: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier { uri },
                    position,
                },
            },
        )
        .unwrap()
    }

    fn assert_prepares_to_parent(position: lsp_types::Position) {
        let result = prepare_rename_at(HIER, position).expect("should be renameable");
        match result {
            lsp_types::PrepareRenameResult::PrepareRenamePlaceholder(p) => {
                // The placeholder is the FULL dotted account, not the last
                // `:`-separated segment -- the whole point of the provider.
                assert_eq!(p.placeholder, PARENT);
                assert_eq!(p.range.start, pos_in(HIER, 1, PARENT, 0));
                assert_eq!(p.range.end, pos_in(HIER, 1, PARENT, PARENT.len() as u32));
            }
            other => panic!("expected a placeholder result, got {other:?}"),
        }
    }

    #[test]
    fn test_prepare_rename_mid_segment_spans_full_account() {
        // Cursor inside the LAST segment -- where a client guessing with its own
        // word/symbol notion would offer only "CheckingAccount".
        assert_prepares_to_parent(pos_in(HIER, 1, "CheckingAccount", 3));
    }

    #[test]
    fn test_prepare_rename_at_first_character() {
        // Regression: the widened `pos-1..pos` point range reaches back into the
        // preceding whitespace here and resolves the `open` ancestor, so the
        // exact-point retry in `account_node_at_position` is what makes this
        // work. A cursor resting on the first character is normal in a modal
        // editor.
        assert_prepares_to_parent(pos_in(HIER, 1, PARENT, 0));
    }

    #[test]
    fn test_prepare_rename_mid_segment_of_first_component() {
        assert_prepares_to_parent(pos_in(HIER, 1, PARENT, 3));
    }

    #[test]
    fn test_prepare_rename_rejects_non_accounts() {
        // A date, a currency and a narration string are all renameable-looking
        // tokens that must be refused. `currency` matters most: it is lexically
        // account-like, and rename on one used to return an empty edit set,
        // which reads as "the command did nothing".
        for position in [
            pos_in(HIER, 1, "2024", 2), // date
            pos_in(HIER, 5, "USD", 1),  // currency
            pos_in(HIER, 4, "Test", 1), // narration string
        ] {
            assert!(
                prepare_rename_at(HIER, position).is_none(),
                "expected no rename at {position:?}"
            );
        }
    }

    #[test]
    #[allow(clippy::mutable_key_type)]
    fn test_rename_cascades_to_subaccounts() {
        let state = TestState::new(HIER).unwrap();
        let uri = lsp_types::Uri::from_file_path(&state.path).unwrap();
        let result = rename(
            state.snapshot,
            lsp_types::RenameParams {
                text_document_position_params: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                    position: pos_in(HIER, 1, "CheckingAccount", 3),
                },
                new_name: NEW.to_string(),
                work_done_progress_params: Default::default(),
            },
        )
        .unwrap()
        .expect("rename should produce edits");

        let changes = result.changes.expect("changes");
        let edits = changes.get(&uri).expect("edits for the fixture file");
        let texts: Vec<&str> = edits.iter().map(|e| e.new_text.as_str()).collect();

        // parent: open + posting. child: open + posting. sibling: untouched.
        assert_eq!(edits.len(), 4, "unexpected edits: {texts:?}");
        assert_eq!(texts.iter().filter(|t| **t == NEW).count(), 2);
        assert_eq!(
            texts
                .iter()
                .filter(|t| **t == format!("{NEW}:DebitOrders"))
                .count(),
            2,
            "sub-account must keep its suffix: {texts:?}"
        );
    }

    #[test]
    #[allow(clippy::mutable_key_type)]
    fn test_rename_does_not_touch_prefix_sibling() {
        // `Assets:...:CheckingAccountOld` starts with the renamed account but is
        // NOT a sub-account of it. This is what the trailing `:` in
        // `child_prefix` guards; without it this account would be rewritten as
        // `...:TransactionalAccountOld`.
        let state = TestState::new(HIER).unwrap();
        let uri = lsp_types::Uri::from_file_path(&state.path).unwrap();
        let sibling_start = pos_in(HIER, 3, SIBLING, 0);

        let result = rename(
            state.snapshot,
            lsp_types::RenameParams {
                text_document_position_params: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                    position: pos_in(HIER, 1, "CheckingAccount", 3),
                },
                new_name: NEW.to_string(),
                work_done_progress_params: Default::default(),
            },
        )
        .unwrap()
        .expect("rename should produce edits");

        let changes = result.changes.expect("changes");
        let edits = changes.get(&uri).expect("edits");
        assert!(
            edits.iter().all(|e| !e.new_text.ends_with("Old")),
            "sibling was rewritten: {:?}",
            edits.iter().map(|e| &e.new_text).collect::<Vec<_>>()
        );
        assert!(
            edits.iter().all(|e| e.range.start != sibling_start),
            "an edit landed on the prefix sibling"
        );
    }

    #[test]
    fn test_find_account_matches_include_subaccounts() {
        let state = TestState::new(HIER).unwrap();
        let matches = find_account_matches(
            &state.snapshot.forest,
            &state.snapshot.open_docs,
            &state.snapshot.forest_content,
            PARENT,
            true,
        );
        let mut texts: Vec<String> = matches.into_iter().map(|(_, t)| t).collect();
        texts.sort();
        // Sorted: PARENT is a prefix of CHILD, so it orders first.
        assert_eq!(
            texts,
            vec![
                PARENT.to_string(),
                PARENT.to_string(),
                CHILD.to_string(),
                CHILD.to_string()
            ]
        );
    }

    #[test]
    fn test_find_references_excludes_subaccounts() {
        // `textDocument/references` semantics are unchanged by the cascade: a
        // sub-account is not a reference to its parent.
        let state = TestState::new(HIER).unwrap();
        let locs = find_references(
            &state.snapshot.forest,
            &state.snapshot.open_docs,
            &state.snapshot.forest_content,
            PARENT,
        );
        assert_eq!(locs.len(), 2); // open + posting only
    }

    #[test]
    fn test_prepare_rename_on_subaccount_uses_full_child_name() {
        let result = prepare_rename_at(HIER, pos_in(HIER, 2, "DebitOrders", 2))
            .expect("sub-account should be renameable");
        match result {
            lsp_types::PrepareRenameResult::PrepareRenamePlaceholder(p) => {
                assert_eq!(p.placeholder, CHILD);
            }
            other => panic!("expected a placeholder result, got {other:?}"),
        }
    }

    #[test]
    fn test_references_at_different_positions() {
        let content = r#"
2024-01-01 open Expenses:Food
2024-01-02 * "Lunch"
  Expenses:Food  10.00 USD
"#;
        let state = TestState::new(content).unwrap();

        let uri = lsp_types::Uri::from_file_path(&state.path).unwrap();

        // Test at line 1 (open directive)
        let params1 = lsp_types::ReferenceParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: lsp_types::Position {
                    line: 1,
                    character: 20,
                },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: lsp_types::ReferenceContext {
                include_declaration: true,
            },
        };

        let result1 = references(state.snapshot, params1).unwrap();
        assert!(result1.is_some());
        assert_eq!(result1.unwrap().len(), 2);
    }

    #[test]
    fn test_references_with_multiple_accounts() {
        let content = r#"
2024-01-01 open Expenses:Food
2024-01-01 open Assets:Cash
2024-01-02 * "Lunch"
  Expenses:Food  10.00 USD
  Assets:Cash   -10.00 USD
"#;
        let state = TestState::new(content).unwrap();

        let locs_food = find_references(
            &state.snapshot.forest,
            &state.snapshot.open_docs,
            &state.snapshot.forest_content,
            "Expenses:Food",
        );
        assert_eq!(locs_food.len(), 2); // open + posting

        let locs_cash = find_references(
            &state.snapshot.forest,
            &state.snapshot.open_docs,
            &state.snapshot.forest_content,
            "Assets:Cash",
        );
        assert_eq!(locs_cash.len(), 2); // open + posting
    }
}

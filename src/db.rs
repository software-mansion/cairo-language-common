use std::collections::VecDeque;
use std::sync::Arc;

use crate::syntax_ext::SyntaxNodeExt;
use cairo_lang_defs::db::DefsGroup;
use cairo_lang_defs::ids::{ModuleId, ModuleItemId};
use cairo_lang_diagnostics::ToOption;
use cairo_lang_filesystem::db::{ext_as_virtual, get_parent_and_mapping, translate_location};
use cairo_lang_filesystem::ids::{CodeOrigin, FileId, FileLongId};
use cairo_lang_filesystem::span::TextOffset;
use cairo_lang_parser::db::ParserGroup;
use cairo_lang_semantic::items::module::ModuleSemantic;
use cairo_lang_semantic::lsp_helpers::LspHelpers;
use cairo_lang_syntax::node::helpers::GetIdentifier;
use cairo_lang_syntax::node::kind::SyntaxKind;
use cairo_lang_syntax::node::{SyntaxNode, TypedSyntaxNode, ast};
use cairo_lang_utils::ordered_hash_set::OrderedHashSet;
use salsa::Database;

pub trait CommonGroup: Database {
    /// Finds the most specific [`SyntaxNode`] at the given [`TextOffset`] in the file.
    fn find_syntax_node_at_offset<'db>(
        &'db self,
        file: FileId<'db>,
        offset: TextOffset,
    ) -> Option<SyntaxNode<'db>> {
        find_syntax_node_at_offset(self.as_dyn_database(), file, offset)
    }

    /// Collects `file` and all its descendants together with modules from all these files. Does *not* includes inline macro expansions.
    fn file_and_subfiles_with_corresponding_modules_without_inline<'db>(
        &'db self,
        file: FileId<'db>,
    ) -> Option<&'db (OrderedHashSet<FileId<'db>>, OrderedHashSet<ModuleId<'db>>)> {
        file_and_subfiles_with_corresponding_modules_without_inline(self.as_dyn_database(), file)
            .as_ref()
    }

    /// Collects `file` and all its descendants together with modules from all these files. Includes inline macro expansions.
    fn file_and_subfiles_with_corresponding_modules<'db>(
        &'db self,
        file: FileId<'db>,
    ) -> Option<&'db (OrderedHashSet<FileId<'db>>, OrderedHashSet<ModuleId<'db>>)> {
        file_and_subfiles_with_corresponding_modules(self.as_dyn_database(), file).as_ref()
    }

    /// We use the term `resultants` to refer to generated nodes that are mapped to the original node and are not deleted.
    /// Effectively (user nodes + generated nodes - removed nodes) set always contains resultants for any user defined node.
    /// Semantic data may be available only for resultants.
    ///
    /// Consider the following foundry code as an example:
    /// ```ignore
    /// #[test]
    /// #[available_gas(123)]
    /// fn test_fn(){
    /// }
    /// ```
    /// This code expands to something like:
    /// ```ignore
    /// #[available_gas(123)]
    /// fn test_fn(){
    ///     if is_config_run {
    ///         // do config check
    ///         return;
    ///     }
    /// }
    /// ```
    /// It then further transforms to:
    /// ```ignore
    /// fn test_fn(){
    ///     if is_config_run {
    ///         // do config check
    ///         set_available_gas(123);
    ///         return;
    ///     }
    /// }
    /// ```
    ///
    /// Let's label these as files 1, 2 and 3, respectively. The macros used here are attribute proc macros. They delete old code and generate new code.
    /// In this process, `test_fn` from file 1 is deleted. However, `test_fn` from file 2 is mapped to it.
    /// Therefore, we should ignore `test_fn` from file 1 as it no longer exists and
    /// should use `test_fn` from file 2. But then, `test_fn` from file 2 is effectively replaced by `test_fn` from file 3, so `test_fn` from file 2 is now deleted.
    ///
    /// In this scenario, only `test_fn` from file 3 is a resultant. Both `test_fn` from files 1 and 2 were deleted.
    ///
    /// So for input being `test_fn` from file 1, only `test_fn` from file 3 is returned
    ///
    /// Now, consider another example:
    ///
    /// The `generate_trait` macro is a builtin macro that does not remove the original code. Thus, we have the following code:
    ///
    /// ```ignore
    /// #[generate_trait]
    /// impl FooImpl for FooTrait {}
    /// ```
    /// This code generates the following:
    /// ```ignore
    /// trait FooTrait {}
    /// ```
    ///
    /// Both the original and the generated files are considered when calculating semantics, since original `FooTrait` was not removed.
    /// Additionally `FooTrait` from file 2 is mapped to `FooTrait` from file 1.
    ///
    /// Therefore for `FooTrait` from file 1, `FooTrait` from file 1 and `FooTrait` from file 2 are returned.
    fn get_node_resultants<'db>(
        &'db self,
        node: SyntaxNode<'db>,
    ) -> Option<&'db Vec<SyntaxNode<'db>>> {
        get_node_resultants(self.as_dyn_database(), (), node).as_ref()
    }

    fn find_generated_nodes<'db>(
        &'db self,
        node_descendant_files: Arc<[FileId<'db>]>,
        node: SyntaxNode<'db>,
    ) -> &'db OrderedHashSet<SyntaxNode<'db>> {
        find_generated_nodes(self.as_dyn_database(), node_descendant_files, node)
    }
}

impl<T: Database + ?Sized> CommonGroup for T {}

/// Finds the most specific [`SyntaxNode`] at the given [`TextOffset`] in the file.
#[salsa::tracked]
fn find_syntax_node_at_offset<'db>(
    db: &'db dyn Database,
    file: FileId<'db>,
    offset: TextOffset,
) -> Option<SyntaxNode<'db>> {
    Some(db.file_syntax(file).to_option()?.lookup_offset(db, offset))
}

#[salsa::tracked(returns(ref))]
fn file_and_subfiles_with_corresponding_modules_without_inline<'db>(
    db: &'db dyn Database,
    file: FileId<'db>,
) -> Option<(OrderedHashSet<FileId<'db>>, OrderedHashSet<ModuleId<'db>>)> {
    // This will fail if crate is still not loaded.
    let file_modules = db.file_modules(file).ok()?;

    let mut modules: OrderedHashSet<_> = file_modules.iter().copied().collect();
    let mut files = OrderedHashSet::from_iter([file]);
    // Collect descendants of `file`
    // and modules from all virtual files that are descendants of `file`.
    //
    // Caveat: consider a situation `file1` --(child)--> `file2` with file contents:
    // - `file1`: `mod file2_origin_module { #[file2]fn sth() {} }`
    // - `file2`: `mod mod_from_file2 { }`
    //  It is important that `file2` content contains a module.
    //
    // Problem: in this situation it is not enough to call `db.file_modules(file1_id)` since
    //  `mod_from_file2` won't be in the result of this query.
    // Solution: we can find file id of `file2`
    //  (note that we only have file id of `file1` at this point)
    //  in `db.module_files(mod_from_file1_from_which_file2_origins)`.
    //  Then we can call `db.file_modules(file2_id)` to obtain module id of `mod_from_file2`.
    //  We repeat this procedure until there is nothing more to collect.
    let mut modules_queue: VecDeque<_> = modules.iter().copied().collect();
    while let Some(module_id) = modules_queue.pop_front() {
        let Ok(module_files) = db.module_files(module_id) else {
            continue;
        };

        for &file_id in module_files {
            if files.insert(file_id)
                && let Ok(file_modules) = db.file_modules(file_id)
            {
                for module_id in file_modules {
                    if modules.insert(*module_id) {
                        modules_queue.push_back(*module_id);
                    }
                }
            }
        }
    }
    Some((files, modules))
}

#[salsa::tracked(returns(ref))]
fn file_and_subfiles_with_corresponding_modules<'db>(
    db: &'db dyn Database,
    file: FileId<'db>,
) -> Option<(OrderedHashSet<FileId<'db>>, OrderedHashSet<ModuleId<'db>>)> {
    let (mut files, modules) = db
        .file_and_subfiles_with_corresponding_modules_without_inline(file)?
        .clone();

    for &module_id in modules.iter() {
        let inline_macro_files = db.inline_macro_expansion_files(module_id);

        let mut path_buffer = vec![];

        for &inline_macro_file in inline_macro_files {
            let mut inline_macro_file = inline_macro_file;
            while let Some((parent, _)) = get_parent_and_mapping(db, inline_macro_file) {
                path_buffer.push(inline_macro_file);

                if files.contains(&parent.file_id) {
                    files.extend(path_buffer.iter().copied());
                    break;
                }

                inline_macro_file = parent.file_id;
            }
            path_buffer.clear();
        }
    }
    Some((files, modules))
}

#[tracing::instrument(skip_all)]
#[salsa::tracked(returns(ref))]
fn get_node_resultants<'db>(
    db: &'db dyn Database,
    _: (),
    node: SyntaxNode<'db>,
) -> Option<Vec<SyntaxNode<'db>>> {
    let main_file = node.stable_ptr(db).file_id(db);

    let (files, _) = db.file_and_subfiles_with_corresponding_modules(main_file)?;

    let files: Arc<[FileId]> = files
        .iter()
        .filter(|file| **file != main_file)
        .copied()
        .collect();
    let resultants = db.find_generated_nodes(files, node);

    Some(resultants.into_iter().copied().collect())
}

#[tracing::instrument(skip_all)]
#[salsa::tracked(returns(ref))]
/// See [`get_node_resultants`].
fn find_generated_nodes<'db>(
    db: &'db dyn Database,
    node_descendant_files: Arc<[FileId<'db>]>,
    node: SyntaxNode<'db>,
) -> OrderedHashSet<SyntaxNode<'db>> {
    let start_file = node.stable_ptr(db).file_id(db);

    let mut result = OrderedHashSet::default();

    let mut is_replaced = false;

    for file in node_descendant_files.iter().cloned() {
        let Some((parent, mappings)) = get_parent_and_mapping(db, file) else {
            continue;
        };

        if parent.file_id != start_file {
            continue;
        }

        let Ok(file_syntax) = db.file_syntax(file) else {
            continue;
        };

        let mappings: Vec<_> = mappings
            .iter()
            .filter(|mapping| match mapping.origin {
                // This is in fact default mapping containing whole file
                // Skip it as whole file content `ModuleItemList` or `SyntaxFile` node is found otherwise
                CodeOrigin::CallSite(_) => false,
                CodeOrigin::Start(start) => start == node.span(db).start,
                CodeOrigin::Span(span) => node.span(db).contains(span),
            })
            .cloned()
            .collect();
        if mappings.is_empty() {
            continue;
        }

        let is_replacing_og_item = match file.long(db) {
            FileLongId::Virtual(vfs) => vfs.original_item_removed,
            FileLongId::External(id) => ext_as_virtual(db, *id).original_item_removed,
            _ => unreachable!(),
        };

        let mut new_nodes: OrderedHashSet<_> = Default::default();

        for mapping in &mappings {
            let node_by_offset = file_syntax.lookup_offset(db, mapping.span.start);
            let node_parent = node_by_offset.parent(db);
            node_parent
                .unwrap_or(node_by_offset)
                .for_each_terminal(db, |terminal| {
                    // Skip end of the file terminal, which is also a syntax tree leaf.
                    // As `ModuleItemList` and `TerminalEndOfFile` have the same parent,
                    // which is the `SyntaxFile`, so we don't want to take the `SyntaxFile`
                    // as an additional resultant.
                    if terminal.kind(db) == SyntaxKind::TerminalEndOfFile {
                        return;
                    }
                    let nodes: Vec<_> = terminal
                        .ancestors_with_self(db)
                        .map_while(|new_node| {
                            translate_location(&mappings, new_node.span_without_trivia(db))
                                .map(|span_in_parent| (new_node, span_in_parent))
                        })
                        .take_while(|(_, span_in_parent)| {
                            node.span_without_trivia(db).contains(*span_in_parent)
                        })
                        .collect();

                    if let Some((last_node, _)) = nodes.last().cloned() {
                        let (new_node, _) = nodes
                            .into_iter()
                            .rev()
                            .take_while(|(node, _)| node.span(db) == last_node.span(db))
                            .last()
                            .unwrap();

                        new_nodes.insert(new_node);
                    }
                });
        }

        // If there is no node found, don't mark it as potentially replaced.
        if !new_nodes.is_empty() {
            is_replaced = is_replaced || is_replacing_og_item;
        }

        for new_node in new_nodes {
            result.extend(
                find_generated_nodes(db, Arc::clone(&node_descendant_files), new_node)
                    .into_iter()
                    .copied(),
            );
        }
    }

    if !is_replaced {
        result.insert(node);
    }

    result
}

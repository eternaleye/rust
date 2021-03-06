// Copyright 2012-2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::collections::HashSet;
use rustc::util::nodemap::NodeSet;
use std::cmp;
use std::string::String;
use std::usize;
use syntax::ast;
use syntax::ast_util;

use clean;
use clean::Item;
use plugins;
use fold;
use fold::DocFolder;

/// Strip items marked `#[doc(hidden)]`
pub fn strip_hidden(krate: clean::Crate) -> plugins::PluginResult {
    let mut stripped = HashSet::new();

    // strip all #[doc(hidden)] items
    let krate = {
        struct Stripper<'a> {
            stripped: &'a mut HashSet<ast::NodeId>
        };
        impl<'a> fold::DocFolder for Stripper<'a> {
            fn fold_item(&mut self, i: Item) -> Option<Item> {
                if i.is_hidden_from_doc() {
                    debug!("found one in strip_hidden; removing");
                    self.stripped.insert(i.def_id.node);

                    // use a dedicated hidden item for given item type if any
                    match i.inner {
                        clean::StructFieldItem(..) => {
                            return Some(clean::Item {
                                inner: clean::StructFieldItem(clean::HiddenStructField),
                                ..i
                            });
                        }
                        _ => {
                            return None;
                        }
                    }
                }

                self.fold_item_recur(i)
            }
        }
        let mut stripper = Stripper{ stripped: &mut stripped };
        stripper.fold_crate(krate)
    };

    // strip any traits implemented on stripped items
    let krate = {
        struct ImplStripper<'a> {
            stripped: &'a mut HashSet<ast::NodeId>
        };
        impl<'a> fold::DocFolder for ImplStripper<'a> {
            fn fold_item(&mut self, i: Item) -> Option<Item> {
                if let clean::ImplItem(clean::Impl{
                           for_: clean::ResolvedPath{ did, .. },
                           ref trait_, ..
                }) = i.inner {
                    // Impls for stripped types don't need to exist
                    if self.stripped.contains(&did.node) {
                        return None;
                    }
                    // Impls of stripped traits also don't need to exist
                    if let Some(clean::ResolvedPath { did, .. }) = *trait_ {
                        if self.stripped.contains(&did.node) {
                            return None;
                        }
                    }
                }
                self.fold_item_recur(i)
            }
        }
        let mut stripper = ImplStripper{ stripped: &mut stripped };
        stripper.fold_crate(krate)
    };

    (krate, None)
}

/// Strip private items from the point of view of a crate or externally from a
/// crate, specified by the `xcrate` flag.
pub fn strip_private(mut krate: clean::Crate) -> plugins::PluginResult {
    // This stripper collects all *retained* nodes.
    let mut retained = HashSet::new();
    let analysis = super::ANALYSISKEY.with(|a| a.clone());
    let analysis = analysis.borrow();
    let analysis = analysis.as_ref().unwrap();
    let exported_items = analysis.exported_items.clone();

    // strip all private items
    {
        let mut stripper = Stripper {
            retained: &mut retained,
            exported_items: &exported_items,
        };
        krate = stripper.fold_crate(krate);
    }

    // strip all private implementations of traits
    {
        let mut stripper = ImplStripper(&retained);
        krate = stripper.fold_crate(krate);
    }
    (krate, None)
}

struct Stripper<'a> {
    retained: &'a mut HashSet<ast::NodeId>,
    exported_items: &'a NodeSet,
}

impl<'a> fold::DocFolder for Stripper<'a> {
    fn fold_item(&mut self, i: Item) -> Option<Item> {
        match i.inner {
            // These items can all get re-exported
            clean::TypedefItem(..) | clean::StaticItem(..) |
            clean::StructItem(..) | clean::EnumItem(..) |
            clean::TraitItem(..) | clean::FunctionItem(..) |
            clean::VariantItem(..) | clean::MethodItem(..) |
            clean::ForeignFunctionItem(..) | clean::ForeignStaticItem(..) => {
                if ast_util::is_local(i.def_id) {
                    if !self.exported_items.contains(&i.def_id.node) {
                        return None;
                    }
                    // Traits are in exported_items even when they're totally private.
                    if i.is_trait() && i.visibility != Some(ast::Public) {
                        return None;
                    }
                }
            }

            clean::ConstantItem(..) => {
                if ast_util::is_local(i.def_id) &&
                   !self.exported_items.contains(&i.def_id.node) {
                    return None;
                }
            }

            clean::ExternCrateItem(..) | clean::ImportItem(_) => {
                if i.visibility != Some(ast::Public) {
                    return None
                }
            }

            clean::StructFieldItem(..) => {
                if i.visibility != Some(ast::Public) {
                    return Some(clean::Item {
                        inner: clean::StructFieldItem(clean::HiddenStructField),
                        ..i
                    })
                }
            }

            // handled below
            clean::ModuleItem(..) => {}

            // trait impls for private items should be stripped
            clean::ImplItem(clean::Impl{
                for_: clean::ResolvedPath{ did, .. }, ..
            }) => {
                if ast_util::is_local(did) &&
                   !self.exported_items.contains(&did.node) {
                    return None;
                }
            }
            clean::DefaultImplItem(..) | clean::ImplItem(..) => {}

            // tymethods/macros have no control over privacy
            clean::MacroItem(..) | clean::TyMethodItem(..) => {}

            // Primitives are never stripped
            clean::PrimitiveItem(..) => {}

            // Associated consts and types are never stripped
            clean::AssociatedConstItem(..) |
            clean::AssociatedTypeItem(..) => {}
        }

        let fastreturn = match i.inner {
            // nothing left to do for traits (don't want to filter their
            // methods out, visibility controlled by the trait)
            clean::TraitItem(..) => true,

            // implementations of traits are always public.
            clean::ImplItem(ref imp) if imp.trait_.is_some() => true,

            // Struct variant fields have inherited visibility
            clean::VariantItem(clean::Variant {
                kind: clean::StructVariant(..)
            }) => true,
            _ => false,
        };

        let i = if fastreturn {
            self.retained.insert(i.def_id.node);
            return Some(i);
        } else {
            self.fold_item_recur(i)
        };

        match i {
            Some(i) => {
                match i.inner {
                    // emptied modules/impls have no need to exist
                    clean::ModuleItem(ref m)
                        if m.items.is_empty() &&
                           i.doc_value().is_none() => None,
                    clean::ImplItem(ref i) if i.items.is_empty() => None,
                    _ => {
                        self.retained.insert(i.def_id.node);
                        Some(i)
                    }
                }
            }
            None => None,
        }
    }
}

// This stripper discards all private impls of traits
struct ImplStripper<'a>(&'a HashSet<ast::NodeId>);
impl<'a> fold::DocFolder for ImplStripper<'a> {
    fn fold_item(&mut self, i: Item) -> Option<Item> {
        if let clean::ImplItem(ref imp) = i.inner {
            match imp.trait_ {
                Some(clean::ResolvedPath{ did, .. }) => {
                    let ImplStripper(s) = *self;
                    if ast_util::is_local(did) && !s.contains(&did.node) {
                        return None;
                    }
                }
                Some(..) | None => {}
            }
        }
        self.fold_item_recur(i)
    }
}


pub fn unindent_comments(krate: clean::Crate) -> plugins::PluginResult {
    struct CommentCleaner;
    impl fold::DocFolder for CommentCleaner {
        fn fold_item(&mut self, i: Item) -> Option<Item> {
            let mut i = i;
            let mut avec: Vec<clean::Attribute> = Vec::new();
            for attr in &i.attrs {
                match attr {
                    &clean::NameValue(ref x, ref s)
                            if "doc" == *x => {
                        avec.push(clean::NameValue("doc".to_string(),
                                                   unindent(s)))
                    }
                    x => avec.push(x.clone())
                }
            }
            i.attrs = avec;
            self.fold_item_recur(i)
        }
    }
    let mut cleaner = CommentCleaner;
    let krate = cleaner.fold_crate(krate);
    (krate, None)
}

pub fn collapse_docs(krate: clean::Crate) -> plugins::PluginResult {
    struct Collapser;
    impl fold::DocFolder for Collapser {
        fn fold_item(&mut self, i: Item) -> Option<Item> {
            let mut docstr = String::new();
            let mut i = i;
            for attr in &i.attrs {
                match *attr {
                    clean::NameValue(ref x, ref s)
                            if "doc" == *x => {
                        docstr.push_str(s);
                        docstr.push('\n');
                    },
                    _ => ()
                }
            }
            let mut a: Vec<clean::Attribute> = i.attrs.iter().filter(|&a| match a {
                &clean::NameValue(ref x, _) if "doc" == *x => false,
                _ => true
            }).cloned().collect();
            if !docstr.is_empty() {
                a.push(clean::NameValue("doc".to_string(), docstr));
            }
            i.attrs = a;
            self.fold_item_recur(i)
        }
    }
    let mut collapser = Collapser;
    let krate = collapser.fold_crate(krate);
    (krate, None)
}

pub fn unindent(s: &str) -> String {
    let lines = s.lines_any().collect::<Vec<&str> >();
    let mut saw_first_line = false;
    let mut saw_second_line = false;
    let min_indent = lines.iter().fold(usize::MAX, |min_indent, line| {

        // After we see the first non-whitespace line, look at
        // the line we have. If it is not whitespace, and therefore
        // part of the first paragraph, then ignore the indentation
        // level of the first line
        let ignore_previous_indents =
            saw_first_line &&
            !saw_second_line &&
            !line.chars().all(|c| c.is_whitespace());

        let min_indent = if ignore_previous_indents {
            usize::MAX
        } else {
            min_indent
        };

        if saw_first_line {
            saw_second_line = true;
        }

        if line.chars().all(|c| c.is_whitespace()) {
            min_indent
        } else {
            saw_first_line = true;
            let mut spaces = 0;
            line.chars().all(|char| {
                // Only comparing against space because I wouldn't
                // know what to do with mixed whitespace chars
                if char == ' ' {
                    spaces += 1;
                    true
                } else {
                    false
                }
            });
            cmp::min(min_indent, spaces)
        }
    });

    if !lines.is_empty() {
        let mut unindented = vec![ lines[0].trim().to_string() ];
        unindented.push_all(&lines[1..].iter().map(|&line| {
            if line.chars().all(|c| c.is_whitespace()) {
                line.to_string()
            } else {
                assert!(line.len() >= min_indent);
                line[min_indent..].to_string()
            }
        }).collect::<Vec<_>>());
        unindented.join("\n")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod unindent_tests {
    use super::unindent;

    #[test]
    fn should_unindent() {
        let s = "    line1\n    line2".to_string();
        let r = unindent(&s);
        assert_eq!(r, "line1\nline2");
    }

    #[test]
    fn should_unindent_multiple_paragraphs() {
        let s = "    line1\n\n    line2".to_string();
        let r = unindent(&s);
        assert_eq!(r, "line1\n\nline2");
    }

    #[test]
    fn should_leave_multiple_indent_levels() {
        // Line 2 is indented another level beyond the
        // base indentation and should be preserved
        let s = "    line1\n\n        line2".to_string();
        let r = unindent(&s);
        assert_eq!(r, "line1\n\n    line2");
    }

    #[test]
    fn should_ignore_first_line_indent() {
        // The first line of the first paragraph may not be indented as
        // far due to the way the doc string was written:
        //
        // #[doc = "Start way over here
        //          and continue here"]
        let s = "line1\n    line2".to_string();
        let r = unindent(&s);
        assert_eq!(r, "line1\nline2");
    }

    #[test]
    fn should_not_ignore_first_line_indent_in_a_single_line_para() {
        let s = "line1\n\n    line2".to_string();
        let r = unindent(&s);
        assert_eq!(r, "line1\n\n    line2");
    }
}

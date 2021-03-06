// Copyright 2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Debugging code to test fingerprints computed for query results.
//! For each node marked with `#[rustc_clean]` or `#[rustc_dirty]`,
//! we will compare the fingerprint from the current and from the previous
//! compilation session as appropriate:
//!
//! - `#[rustc_dirty(label="TypeckTables", cfg="rev2")]` if we are
//!   in `#[cfg(rev2)]`, then the fingerprints associated with
//!   `DepNode::TypeckTables(X)` must be DIFFERENT (`X` is the def-id of the
//!   current node).
//! - `#[rustc_clean(label="TypeckTables", cfg="rev2")]` same as above,
//!   except that the fingerprints must be the SAME.
//!
//! Errors are reported if we are in the suitable configuration but
//! the required condition is not met.
//!
//! The `#[rustc_metadata_dirty]` and `#[rustc_metadata_clean]` attributes
//! can be used to check the incremental compilation hash (ICH) values of
//! metadata exported in rlibs.
//!
//! - If a node is marked with `#[rustc_metadata_clean(cfg="rev2")]` we
//!   check that the metadata hash for that node is the same for "rev2"
//!   it was for "rev1".
//! - If a node is marked with `#[rustc_metadata_dirty(cfg="rev2")]` we
//!   check that the metadata hash for that node is *different* for "rev2"
//!   than it was for "rev1".
//!
//! Note that the metadata-testing attributes must never specify the
//! first revision. This would lead to a crash since there is no
//! previous revision to compare things to.
//!

use std::collections::HashSet;
use std::vec::Vec;
use rustc::dep_graph::DepNode;
use rustc::hir;
use rustc::hir::def_id::DefId;
use rustc::hir::itemlikevisit::ItemLikeVisitor;
use rustc::hir::intravisit;
use rustc::ich::{Fingerprint, ATTR_DIRTY, ATTR_CLEAN, ATTR_DIRTY_METADATA,
                 ATTR_CLEAN_METADATA};
use syntax::ast::{self, Attribute, NestedMetaItem};
use rustc_data_structures::fx::{FxHashSet, FxHashMap};
use syntax_pos::Span;
use rustc::ty::TyCtxt;

const LABEL: &'static str = "label";
const CFG: &'static str = "cfg";

type Labels = HashSet<String>;

pub fn check_dirty_clean_annotations<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>) {
    // can't add `#[rustc_dirty]` etc without opting in to this feature
    if !tcx.sess.features.borrow().rustc_attrs {
        return;
    }

    let _ignore = tcx.dep_graph.in_ignore();
    let krate = tcx.hir.krate();
    let mut dirty_clean_visitor = DirtyCleanVisitor {
        tcx,
        checked_attrs: FxHashSet(),
    };
    krate.visit_all_item_likes(&mut dirty_clean_visitor);

    let mut all_attrs = FindAllAttrs {
        tcx,
        attr_names: vec![ATTR_DIRTY, ATTR_CLEAN],
        found_attrs: vec![],
    };
    intravisit::walk_crate(&mut all_attrs, krate);

    // Note that we cannot use the existing "unused attribute"-infrastructure
    // here, since that is running before trans. This is also the reason why
    // all trans-specific attributes are `Whitelisted` in syntax::feature_gate.
    all_attrs.report_unchecked_attrs(&dirty_clean_visitor.checked_attrs);
}

pub struct DirtyCleanVisitor<'a, 'tcx:'a> {
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    checked_attrs: FxHashSet<ast::AttrId>,
}

impl<'a, 'tcx> DirtyCleanVisitor<'a, 'tcx> {
    fn labels(&self, attr: &Attribute) -> Labels {
        for item in attr.meta_item_list().unwrap_or_else(Vec::new) {
            if item.check_name(LABEL) {
                let value = expect_associated_value(self.tcx, &item);
                return self.resolve_labels(&item, value.as_str().as_ref());
            }
        }
        self.tcx.sess.span_fatal(attr.span, "no `label` found");
    }

    fn resolve_labels(&self, item: &NestedMetaItem, value: &str) -> Labels {
        let mut out: Labels = HashSet::new();
        for label in value.split(',') {
            let label = label.trim();
            if DepNode::has_label_string(label) {
                if out.contains(label) {
                    self.tcx.sess.span_fatal(
                        item.span,
                        &format!("dep-node label `{}` is repeated", label));
                }
                out.insert(label.to_string());
            } else {
                self.tcx.sess.span_fatal(
                    item.span,
                    &format!("dep-node label `{}` not recognized", label));
            }
        }
        out
    }

    fn dep_nodes(&self, labels: &Labels, def_id: DefId) -> Vec<DepNode> {
        let mut out = Vec::with_capacity(labels.len());
        let def_path_hash = self.tcx.def_path_hash(def_id);
        for label in labels.iter() {
            match DepNode::from_label_string(label, def_path_hash) {
                Ok(dep_node) => out.push(dep_node),
                Err(()) => unreachable!(),
            }
        }
        out
    }

    fn dep_node_str(&self, dep_node: &DepNode) -> String {
        if let Some(def_id) = dep_node.extract_def_id(self.tcx) {
            format!("{:?}({})",
                    dep_node.kind,
                    self.tcx.item_path_str(def_id))
        } else {
            format!("{:?}({:?})", dep_node.kind, dep_node.hash)
        }
    }

    fn assert_dirty(&self, item_span: Span, dep_node: DepNode) {
        debug!("assert_dirty({:?})", dep_node);

        let current_fingerprint = self.tcx.dep_graph.fingerprint_of(&dep_node);
        let prev_fingerprint = self.tcx.dep_graph.prev_fingerprint_of(&dep_node);

        if Some(current_fingerprint) == prev_fingerprint {
            let dep_node_str = self.dep_node_str(&dep_node);
            self.tcx.sess.span_err(
                item_span,
                &format!("`{}` should be dirty but is not", dep_node_str));
        }
    }

    fn assert_clean(&self, item_span: Span, dep_node: DepNode) {
        debug!("assert_clean({:?})", dep_node);

        let current_fingerprint = self.tcx.dep_graph.fingerprint_of(&dep_node);
        let prev_fingerprint = self.tcx.dep_graph.prev_fingerprint_of(&dep_node);

        if Some(current_fingerprint) != prev_fingerprint {
            let dep_node_str = self.dep_node_str(&dep_node);
            self.tcx.sess.span_err(
                item_span,
                &format!("`{}` should be clean but is not", dep_node_str));
        }
    }

    fn check_item(&mut self, item_id: ast::NodeId, item_span: Span) {
        let def_id = self.tcx.hir.local_def_id(item_id);
        for attr in self.tcx.get_attrs(def_id).iter() {
            if attr.check_name(ATTR_DIRTY) {
                if check_config(self.tcx, attr) {
                    self.checked_attrs.insert(attr.id);
                    let labels = self.labels(attr);
                    for dep_node in self.dep_nodes(&labels, def_id) {
                        self.assert_dirty(item_span, dep_node);
                    }
                }
            } else if attr.check_name(ATTR_CLEAN) {
                if check_config(self.tcx, attr) {
                    self.checked_attrs.insert(attr.id);
                    let labels = self.labels(attr);
                    for dep_node in self.dep_nodes(&labels, def_id) {
                        self.assert_clean(item_span, dep_node);
                    }
                }
            }
        }
    }
}

impl<'a, 'tcx> ItemLikeVisitor<'tcx> for DirtyCleanVisitor<'a, 'tcx> {
    fn visit_item(&mut self, item: &'tcx hir::Item) {
        self.check_item(item.id, item.span);
    }

    fn visit_trait_item(&mut self, item: &hir::TraitItem) {
        self.check_item(item.id, item.span);
    }

    fn visit_impl_item(&mut self, item: &hir::ImplItem) {
        self.check_item(item.id, item.span);
    }
}

pub fn check_dirty_clean_metadata<'a, 'tcx>(
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    prev_metadata_hashes: &FxHashMap<DefId, Fingerprint>,
    current_metadata_hashes: &FxHashMap<DefId, Fingerprint>)
{
    if !tcx.sess.opts.debugging_opts.query_dep_graph {
        return;
    }

    tcx.dep_graph.with_ignore(||{
        let krate = tcx.hir.krate();
        let mut dirty_clean_visitor = DirtyCleanMetadataVisitor {
            tcx,
            prev_metadata_hashes,
            current_metadata_hashes,
            checked_attrs: FxHashSet(),
        };
        intravisit::walk_crate(&mut dirty_clean_visitor, krate);

        let mut all_attrs = FindAllAttrs {
            tcx,
            attr_names: vec![ATTR_DIRTY_METADATA, ATTR_CLEAN_METADATA],
            found_attrs: vec![],
        };
        intravisit::walk_crate(&mut all_attrs, krate);

        // Note that we cannot use the existing "unused attribute"-infrastructure
        // here, since that is running before trans. This is also the reason why
        // all trans-specific attributes are `Whitelisted` in syntax::feature_gate.
        all_attrs.report_unchecked_attrs(&dirty_clean_visitor.checked_attrs);
    });
}

pub struct DirtyCleanMetadataVisitor<'a, 'tcx: 'a, 'm> {
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    prev_metadata_hashes: &'m FxHashMap<DefId, Fingerprint>,
    current_metadata_hashes: &'m FxHashMap<DefId, Fingerprint>,
    checked_attrs: FxHashSet<ast::AttrId>,
}

impl<'a, 'tcx, 'm> intravisit::Visitor<'tcx> for DirtyCleanMetadataVisitor<'a, 'tcx, 'm> {

    fn nested_visit_map<'this>(&'this mut self) -> intravisit::NestedVisitorMap<'this, 'tcx> {
        intravisit::NestedVisitorMap::All(&self.tcx.hir)
    }

    fn visit_item(&mut self, item: &'tcx hir::Item) {
        self.check_item(item.id, item.span);
        intravisit::walk_item(self, item);
    }

    fn visit_variant(&mut self,
                     variant: &'tcx hir::Variant,
                     generics: &'tcx hir::Generics,
                     parent_id: ast::NodeId) {
        if let Some(e) = variant.node.disr_expr {
            self.check_item(e.node_id, variant.span);
        }

        intravisit::walk_variant(self, variant, generics, parent_id);
    }

    fn visit_variant_data(&mut self,
                          variant_data: &'tcx hir::VariantData,
                          _: ast::Name,
                          _: &'tcx hir::Generics,
                          _parent_id: ast::NodeId,
                          span: Span) {
        if self.tcx.hir.find(variant_data.id()).is_some() {
            // VariantData that represent structs or tuples don't have a
            // separate entry in the HIR map and checking them would error,
            // so only check if this is an enum or union variant.
            self.check_item(variant_data.id(), span);
        }

        intravisit::walk_struct_def(self, variant_data);
    }

    fn visit_trait_item(&mut self, item: &'tcx hir::TraitItem) {
        self.check_item(item.id, item.span);
        intravisit::walk_trait_item(self, item);
    }

    fn visit_impl_item(&mut self, item: &'tcx hir::ImplItem) {
        self.check_item(item.id, item.span);
        intravisit::walk_impl_item(self, item);
    }

    fn visit_foreign_item(&mut self, i: &'tcx hir::ForeignItem) {
        self.check_item(i.id, i.span);
        intravisit::walk_foreign_item(self, i);
    }

    fn visit_struct_field(&mut self, s: &'tcx hir::StructField) {
        self.check_item(s.id, s.span);
        intravisit::walk_struct_field(self, s);
    }
}

impl<'a, 'tcx, 'm> DirtyCleanMetadataVisitor<'a, 'tcx, 'm> {

    fn check_item(&mut self, item_id: ast::NodeId, item_span: Span) {
        let def_id = self.tcx.hir.local_def_id(item_id);

        for attr in self.tcx.get_attrs(def_id).iter() {
            if attr.check_name(ATTR_DIRTY_METADATA) {
                if check_config(self.tcx, attr) {
                    if self.checked_attrs.insert(attr.id) {
                        self.assert_state(false, def_id, item_span);
                    }
                }
            } else if attr.check_name(ATTR_CLEAN_METADATA) {
                if check_config(self.tcx, attr) {
                    if self.checked_attrs.insert(attr.id) {
                        self.assert_state(true, def_id, item_span);
                    }
                }
            }
        }
    }

    fn assert_state(&self, should_be_clean: bool, def_id: DefId, span: Span) {
        let item_path = self.tcx.item_path_str(def_id);
        debug!("assert_state({})", item_path);

        if let Some(&prev_hash) = self.prev_metadata_hashes.get(&def_id) {
            let hashes_are_equal = prev_hash == self.current_metadata_hashes[&def_id];

            if should_be_clean && !hashes_are_equal {
                self.tcx.sess.span_err(
                        span,
                        &format!("Metadata hash of `{}` is dirty, but should be clean",
                                 item_path));
            }

            let should_be_dirty = !should_be_clean;
            if should_be_dirty && hashes_are_equal {
                self.tcx.sess.span_err(
                        span,
                        &format!("Metadata hash of `{}` is clean, but should be dirty",
                                 item_path));
            }
        } else {
            self.tcx.sess.span_err(
                        span,
                        &format!("Could not find previous metadata hash of `{}`",
                                 item_path));
        }
    }
}

/// Given a `#[rustc_dirty]` or `#[rustc_clean]` attribute, scan
/// for a `cfg="foo"` attribute and check whether we have a cfg
/// flag called `foo`.
fn check_config(tcx: TyCtxt, attr: &Attribute) -> bool {
    debug!("check_config(attr={:?})", attr);
    let config = &tcx.sess.parse_sess.config;
    debug!("check_config: config={:?}", config);
    for item in attr.meta_item_list().unwrap_or_else(Vec::new) {
        if item.check_name(CFG) {
            let value = expect_associated_value(tcx, &item);
            debug!("check_config: searching for cfg {:?}", value);
            return config.contains(&(value, None));
        }
    }

    tcx.sess.span_fatal(
        attr.span,
        "no cfg attribute");
}

fn expect_associated_value(tcx: TyCtxt, item: &NestedMetaItem) -> ast::Name {
    if let Some(value) = item.value_str() {
        value
    } else {
        let msg = if let Some(name) = item.name() {
            format!("associated value expected for `{}`", name)
        } else {
            "expected an associated value".to_string()
        };

        tcx.sess.span_fatal(item.span, &msg);
    }
}


// A visitor that collects all #[rustc_dirty]/#[rustc_clean] attributes from
// the HIR. It is used to verfiy that we really ran checks for all annotated
// nodes.
pub struct FindAllAttrs<'a, 'tcx:'a> {
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    attr_names: Vec<&'static str>,
    found_attrs: Vec<&'tcx Attribute>,
}

impl<'a, 'tcx> FindAllAttrs<'a, 'tcx> {

    fn is_active_attr(&mut self, attr: &Attribute) -> bool {
        for attr_name in &self.attr_names {
            if attr.check_name(attr_name) && check_config(self.tcx, attr) {
                return true;
            }
        }

        false
    }

    fn report_unchecked_attrs(&self, checked_attrs: &FxHashSet<ast::AttrId>) {
        for attr in &self.found_attrs {
            if !checked_attrs.contains(&attr.id) {
                self.tcx.sess.span_err(attr.span, &format!("found unchecked \
                    #[rustc_dirty]/#[rustc_clean] attribute"));
            }
        }
    }
}

impl<'a, 'tcx> intravisit::Visitor<'tcx> for FindAllAttrs<'a, 'tcx> {
    fn nested_visit_map<'this>(&'this mut self) -> intravisit::NestedVisitorMap<'this, 'tcx> {
        intravisit::NestedVisitorMap::All(&self.tcx.hir)
    }

    fn visit_attribute(&mut self, attr: &'tcx Attribute) {
        if self.is_active_attr(attr) {
            self.found_attrs.push(attr);
        }
    }
}

//! The translation contexts.

#![allow(dead_code)]
use crate::formatter::Formatter;
use crate::get_mir::MirLevel;
use crate::meta;
use crate::meta::{FileId, FileName, LocalFileId, Meta, VirtualFileId};
use crate::names::Name;
use crate::reorder_decls::AnyTransId;
use crate::types as ty;
use crate::types::LiteralTy;
use crate::ullbc_ast as ast;
use crate::values as v;
use hax_frontend_exporter as hax;
use hax_frontend_exporter::SInto;
use linked_hash_set::LinkedHashSet;
use macros::VariantIndexArity;
use rustc_hir::def_id::DefId;
use rustc_middle::ty::TyCtxt;
use rustc_session::Session;
use std::cmp::{Ord, Ordering, PartialOrd};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;

pub struct CrateInfo {
    pub crate_name: String,
    pub opaque_mods: HashSet<String>,
}

impl CrateInfo {
    pub(crate) fn is_opaque_decl(&self, name: &Name) -> bool {
        name.is_in_modules(&self.crate_name, &self.opaque_mods)
    }

    fn is_transparent_decl(&self, name: &Name) -> bool {
        !self.is_opaque_decl(name)
    }
}

/// We use a special type to store the Rust identifiers in the stack, to
/// make sure we translate them in a specific order (top-level constants
/// before constant functions before functions...). This allows us to
/// avoid stealing issues when looking up the MIR bodies.
#[derive(Clone, Copy, Debug, Eq, PartialEq, VariantIndexArity)]
pub enum OrdRustId {
    Global(DefId),
    ConstFun(DefId),
    Trait(DefId),
    Fun(DefId),
    Type(DefId),
}

impl OrdRustId {
    fn get_id(&self) -> DefId {
        match self {
            OrdRustId::Global(id)
            | OrdRustId::ConstFun(id)
            | OrdRustId::Trait(id)
            | OrdRustId::Fun(id)
            | OrdRustId::Type(id) => *id,
        }
    }
}

impl PartialOrd for OrdRustId {
    fn partial_cmp(&self, other: &OrdRustId) -> Option<Ordering> {
        let (vid0, _) = self.variant_index_arity();
        let (vid1, _) = other.variant_index_arity();
        if vid0 != vid1 {
            Option::Some(vid0.cmp(&vid1))
        } else {
            let id0 = self.get_id();
            let id1 = other.get_id();
            Option::Some(id0.cmp(&id1))
        }
    }
}

impl Ord for OrdRustId {
    fn cmp(&self, other: &OrdRustId) -> Ordering {
        self.partial_cmp(other).unwrap()
    }
}

/// Translation context containing the top-level definitions.
pub struct TransCtx<'tcx, 'ctx> {
    /// The compiler session
    pub sess: &'ctx Session,
    /// The Rust compiler type context
    pub tcx: TyCtxt<'tcx>,
    /// The Hax context
    pub hax_state: hax::State<hax::Base<'tcx>, (), (), ()>,
    /// The level at which to extract the MIR
    pub mir_level: MirLevel,
    ///
    pub crate_info: CrateInfo,
    /// All the ids, in the order in which we encountered them
    pub all_ids: LinkedHashSet<AnyTransId>,
    /// The declarations we came accross and which we haven't translated yet.
    /// We use an ordered set to make sure we translate them in a specific
    /// order (this avoids stealing issues when querying the MIR bodies).
    pub stack: BTreeSet<OrdRustId>,
    /// File names to ids and vice-versa
    pub file_to_id: HashMap<FileName, FileId::Id>,
    pub id_to_file: HashMap<FileId::Id, FileName>,
    pub real_file_counter: LocalFileId::Generator,
    pub virtual_file_counter: VirtualFileId::Generator,
    /// The map from Rust type ids to translated type ids
    pub type_id_map: ty::TypeDeclId::MapGenerator<DefId>,
    /// The translated type definitions
    pub type_defs: ty::TypeDecls,
    /// The map from Rust function ids to translated function ids
    pub fun_id_map: ast::FunDeclId::MapGenerator<DefId>,
    /// The translated function definitions
    pub fun_defs: ast::FunDecls,
    /// The map from Rust global ids to translated global ids
    pub global_id_map: ast::GlobalDeclId::MapGenerator<DefId>,
    /// The translated global definitions
    pub global_defs: ast::GlobalDecls,
    /// The map from Rust trait ids to translated trait ids
    pub trait_id_map: ast::TraitId::MapGenerator<DefId>,
    /// The translated trait definitions
    pub trait_defs: ast::TraitDecls,
}

/// A translation context for type/global/function bodies.
/// Simply augments the [TransCtx] with local variables.
///
/// Remark: for now we don't really need to use collections from the [im] crate,
/// because we don't need the O(1) clone operation, but we may need it once we
/// implement support for universally quantified traits, where we might need
/// to be able to dive in/out of universal quantifiers. Also, it doesn't cost
/// us to use those collections.
pub(crate) struct BodyTransCtx<'tcx, 'ctx, 'ctx1> {
    /// This is used in very specific situations.
    pub def_id: DefId,
    /// The translation context containing the top-level definitions/ids.
    pub t_ctx: &'ctx mut TransCtx<'tcx, 'ctx1>,
    /// The regions - TODO: rename to region_vars
    pub region_vars: ty::RegionVarId::Vector<ty::RegionVar>,
    /// The map from rust region to translated region indices
    pub region_vars_map: ty::RegionVarId::MapGenerator<hax::Region>,
    /// The type variables
    pub type_vars: ty::TypeVarId::Vector<ty::TypeVar>,
    /// The map from rust type variable indices to translated type variable
    /// indices.
    pub type_vars_map: ty::TypeVarId::MapGenerator<u32>,
    /// The "regular" variables
    pub vars: v::VarId::Vector<ast::Var>,
    /// The map from rust variable indices to translated variables indices.
    pub vars_map: v::VarId::MapGenerator<usize>,
    /// The const generic variables
    pub const_generic_vars: ty::ConstGenericVarId::Vector<ty::ConstGenericVar>,
    /// The map from rust const generic variables to translate const generic
    /// variable indices.
    pub const_generic_vars_map: ty::ConstGenericVarId::MapGenerator<u32>,
    ///
    pub trait_clauses_counter: ty::TraitClauseId::Generator,
    ///
    pub trait_clauses: ty::TraitClauseId::Vector<ty::TraitClause>,
    /// The translated blocks. We can't use `ast::BlockId::Vector<ast::BlockData>`
    /// here because we might generate several fresh indices before actually
    /// adding the resulting blocks to the map.
    pub blocks: im::OrdMap<ast::BlockId::Id, ast::BlockData>,
    /// The map from rust blocks to translated blocks.
    /// Note that when translating terminators like DropAndReplace, we might have
    /// to introduce new blocks which don't appear in the original MIR.
    pub blocks_map: ast::BlockId::MapGenerator<hax::BasicBlock>,
    ///
    /// The stack of late-bound parameters (can only be lifetimes for now), which
    /// use DeBruijn indices (the other parameters use free variables).
    /// For explanations about what early-bound and late-bound parameters are, see:
    /// https://smallcultfollowing.com/babysteps/blog/2013/10/29/intermingled-parameter-lists/
    /// https://smallcultfollowing.com/babysteps/blog/2013/11/04/intermingled-parameter-lists/
    ///
    /// Remark: even though performance is not critical, the use of [im::Vec] allows
    /// us to push/pop and access indexed elements with very good performance.
    ///
    /// **Important**:
    /// ==============
    /// The Rust compiler uses De Bruijn indices to identify the *group* of
    /// universally quantified variables, and variable identifiers to identity
    /// the variables inside the group.
    ///
    /// For instance, we have the following:
    /// ```
    ///                     we compute the De Bruijn indices from here
    ///                            VVVVVVVVVVVVVVVVVVVVVVV
    /// fn f<'a, 'b>(x: for<'c> fn(&'a u8, &'b u16, &'c u32) -> u64) {}
    ///      ^^^^^^         ^^       ^       ^        ^
    ///        |      De Bruijn: 0   |       |        |
    ///  De Bruijn: 1                |       |        |
    ///                        De Bruijn: 1  |    De Bruijn: 0
    ///                           Var id: 0  |       Var id: 0
    ///                                      |
    ///                                De Bruijn: 1
    ///                                   Var id: 1
    /// ```
    ///
    /// For this reason, we use a stack of vectors to store the bound variables.
    pub bound_vars: im::Vector<im::Vector<ty::RegionVarId::Id>>,
}

impl<'tcx, 'ctx> TransCtx<'tcx, 'ctx> {
    /// Register a file if it is a "real" file and was not already registered
    fn register_file(&mut self, filename: FileName) -> FileId::Id {
        // Lookup the file if it was already registered
        match self.file_to_id.get(&filename) {
            Option::Some(id) => *id,
            Option::None => {
                // Generate the fresh id
                let id = match &filename {
                    FileName::Local(_) => FileId::Id::LocalId(self.real_file_counter.fresh_id()),
                    FileName::Virtual(_) => {
                        FileId::Id::VirtualId(self.virtual_file_counter.fresh_id())
                    }
                    FileName::NotReal(_) => unimplemented!(),
                };
                self.file_to_id.insert(filename.clone(), id);
                self.id_to_file.insert(id, filename);
                id
            }
        }
    }

    /// Compute the meta information for a Rust definition identified by its id.
    pub(crate) fn translate_meta_from_rid(&mut self, def_id: DefId) -> Meta {
        // Retrieve the span from the def id
        let rspan = meta::get_rspan_from_def_id(self.tcx, def_id);
        let rspan = rspan.sinto(&self.hax_state);
        self.translate_meta_from_rspan(rspan)
    }

    pub fn translate_span(&mut self, rspan: hax::Span) -> meta::Span {
        let filename = meta::convert_filename(&rspan.filename);
        let file_id = match &filename {
            FileName::NotReal(_) => {
                // For now we forbid not real filenames
                unimplemented!();
            }
            FileName::Virtual(_) | FileName::Local(_) => self.register_file(filename),
        };

        let beg = meta::convert_loc(rspan.lo);
        let end = meta::convert_loc(rspan.hi);

        // Put together
        meta::Span { file_id, beg, end }
    }

    /// Compute meta data from a Rust source scope
    pub fn translate_meta_from_source_info(
        &mut self,
        source_scopes: &hax::IndexVec<hax::SourceScope, hax::SourceScopeData>,
        source_info: &hax::SourceInfo,
    ) -> Meta {
        // Translate the span
        let mut scope_data = source_scopes.get(source_info.scope).unwrap();
        let span = self.translate_span(scope_data.span.clone());

        // Lookup the top-most inlined parent scope.
        if scope_data.inlined_parent_scope.is_some() {
            while scope_data.inlined_parent_scope.is_some() {
                let parent_scope = scope_data.inlined_parent_scope.unwrap();
                scope_data = source_scopes.get(parent_scope).unwrap();
            }

            let parent_span = self.translate_span(scope_data.span.clone());

            Meta {
                span: parent_span,
                generated_from_span: Some(span),
            }
        } else {
            Meta {
                span,
                generated_from_span: None,
            }
        }
    }

    // TODO: rename
    pub(crate) fn translate_meta_from_rspan(&mut self, rspan: hax::Span) -> Meta {
        // Translate the span
        let span = self.translate_span(rspan);

        Meta {
            span,
            generated_from_span: None,
        }
    }

    pub(crate) fn id_is_opaque(&self, id: DefId) -> bool {
        let name = crate::names_utils::item_def_id_to_name(self.tcx, id);
        self.crate_info.is_opaque_decl(&name)
    }

    pub(crate) fn id_is_transparent(&self, id: DefId) -> bool {
        !self.id_is_opaque(id)
    }

    pub(crate) fn push_id(&mut self, _rust_id: DefId, id: OrdRustId, trans_id: AnyTransId) {
        // Add the id to the stack of declarations to translate
        self.stack.insert(id);
        self.all_ids.insert(trans_id);
    }

    pub(crate) fn register_type_decl_id(&mut self, id: DefId) -> ty::TypeDeclId::Id {
        match self.type_id_map.get(&id) {
            Option::Some(id) => id,
            Option::None => {
                let rid = OrdRustId::Type(id);
                let trans_id = self.type_id_map.insert(id);
                self.push_id(id, rid, AnyTransId::Type(trans_id));
                trans_id
            }
        }
    }

    pub(crate) fn translate_type_decl_id(&mut self, id: DefId) -> ty::TypeDeclId::Id {
        self.register_type_decl_id(id)
    }

    pub(crate) fn register_fun_decl_id(&mut self, id: DefId) -> ast::FunDeclId::Id {
        match self.fun_id_map.get(&id) {
            Option::Some(id) => id,
            Option::None => {
                let rid = if self.tcx.is_const_fn_raw(id) {
                    OrdRustId::ConstFun(id)
                } else {
                    OrdRustId::Fun(id)
                };
                let trans_id = self.fun_id_map.insert(id);
                self.push_id(id, rid, AnyTransId::Fun(trans_id));
                trans_id
            }
        }
    }

    pub(crate) fn register_trait_id(&mut self, id: DefId) -> ast::TraitId::Id {
        match self.trait_id_map.get(&id) {
            Option::Some(id) => id,
            Option::None => {
                let rid = OrdRustId::Trait(id);
                let trans_id = self.trait_id_map.insert(id);
                self.push_id(id, rid, AnyTransId::Trait(trans_id));
                trans_id
            }
        }
    }

    pub(crate) fn translate_fun_decl_id(&mut self, id: DefId) -> ast::FunDeclId::Id {
        self.register_fun_decl_id(id)
    }

    pub(crate) fn translate_trait_id(&mut self, id: DefId) -> ast::TraitId::Id {
        self.register_trait_id(id)
    }

    pub(crate) fn register_global_decl_id(&mut self, id: DefId) -> ty::GlobalDeclId::Id {
        match self.global_id_map.get(&id) {
            Option::Some(id) => id,
            Option::None => {
                let rid = OrdRustId::Global(id);
                let trans_id = self.global_id_map.insert(id);
                self.push_id(id, rid, AnyTransId::Global(trans_id));
                trans_id
            }
        }
    }

    pub(crate) fn translate_global_decl_id(&mut self, id: DefId) -> ast::GlobalDeclId::Id {
        self.register_global_decl_id(id)
    }
}

impl<'tcx, 'ctx, 'ctx1> BodyTransCtx<'tcx, 'ctx, 'ctx1> {
    /// Create a new `ExecContext`.
    pub(crate) fn new(def_id: DefId, t_ctx: &'ctx mut TransCtx<'tcx, 'ctx1>) -> Self {
        BodyTransCtx {
            def_id,
            t_ctx,
            region_vars: ty::RegionVarId::Vector::new(),
            region_vars_map: ty::RegionVarId::MapGenerator::new(),
            type_vars: ty::TypeVarId::Vector::new(),
            type_vars_map: ty::TypeVarId::MapGenerator::new(),
            vars: v::VarId::Vector::new(),
            vars_map: v::VarId::MapGenerator::new(),
            const_generic_vars: ty::ConstGenericVarId::Vector::new(),
            const_generic_vars_map: ty::ConstGenericVarId::MapGenerator::new(),
            trait_clauses_counter: ty::TraitClauseId::Generator::new(),
            trait_clauses: ty::TraitClauseId::Vector::new(),
            blocks: im::OrdMap::new(),
            blocks_map: ast::BlockId::MapGenerator::new(),
            bound_vars: im::Vector::new(),
        }
    }

    pub(crate) fn translate_meta_from_rid(&mut self, def_id: DefId) -> Meta {
        self.t_ctx.translate_meta_from_rid(def_id)
    }

    pub(crate) fn translate_meta_from_rspan(&mut self, rspan: hax::Span) -> Meta {
        self.t_ctx.translate_meta_from_rspan(rspan)
    }

    pub(crate) fn get_local(&self, local: &hax::Local) -> Option<v::VarId::Id> {
        use rustc_index::Idx;
        self.vars_map.get(&local.index())
    }

    pub(crate) fn get_block_id_from_rid(&self, rid: hax::BasicBlock) -> Option<ast::BlockId::Id> {
        self.blocks_map.get(&rid)
    }

    pub(crate) fn get_var_from_id(&self, var_id: v::VarId::Id) -> Option<&ast::Var> {
        self.vars.get(var_id)
    }

    pub(crate) fn register_type_decl_id(&mut self, id: DefId) -> ty::TypeDeclId::Id {
        self.t_ctx.register_type_decl_id(id)
    }

    pub(crate) fn translate_type_decl_id(&mut self, id: DefId) -> ty::TypeDeclId::Id {
        self.t_ctx.translate_type_decl_id(id)
    }

    pub(crate) fn register_fun_decl_id(&mut self, id: DefId) -> ast::FunDeclId::Id {
        self.t_ctx.register_fun_decl_id(id)
    }

    pub(crate) fn translate_fun_decl_id(&mut self, id: DefId) -> ast::FunDeclId::Id {
        self.t_ctx.translate_fun_decl_id(id)
    }

    pub(crate) fn register_global_decl_id(&mut self, id: DefId) -> ty::GlobalDeclId::Id {
        self.t_ctx.register_global_decl_id(id)
    }

    pub(crate) fn translate_global_decl_id(&mut self, id: DefId) -> ast::GlobalDeclId::Id {
        self.t_ctx.translate_global_decl_id(id)
    }

    pub(crate) fn translate_trait_id(&mut self, id: DefId) -> ast::TraitId::Id {
        self.t_ctx.translate_trait_id(id)
    }

    pub(crate) fn get_region_from_rust(&self, r: hax::Region) -> Option<ty::RegionVarId::Id> {
        self.region_vars_map.get(&r)
    }

    pub(crate) fn push_region(
        &mut self,
        r: hax::Region,
        name: Option<String>,
    ) -> ty::RegionVarId::Id {
        use crate::id_vector::ToUsize;
        let rid = self.region_vars_map.insert(r);
        assert!(rid.to_usize() == self.region_vars.len());
        let var = ty::RegionVar { index: rid, name };
        self.region_vars.insert(rid, var);
        rid
    }

    /// Push a group of bound regions
    pub(crate) fn push_bound_regions_group(&mut self, names: Vec<Option<String>>) {
        use crate::id_vector::ToUsize;

        // Register the variables
        let var_ids: im::Vector<ty::RegionVarId::Id> = names
            .into_iter()
            .map(|name| {
                // Note that we don't insert a binding in the region_vars_map
                let rid = self.region_vars_map.fresh_id();
                assert!(rid.to_usize() == self.region_vars.len());
                let var = ty::RegionVar { index: rid, name };
                self.region_vars.insert(rid, var);
                rid
            })
            .collect();

        // Push the group
        self.bound_vars.push_front(var_ids);
    }

    pub(crate) fn push_type_var(&mut self, rindex: u32, name: String) -> ty::TypeVarId::Id {
        use crate::id_vector::ToUsize;
        let var_id = self.type_vars_map.insert(rindex);
        assert!(var_id.to_usize() == self.type_vars.len());
        let var = ty::TypeVar {
            index: var_id,
            name,
        };
        self.type_vars.insert(var_id, var);
        var_id
    }

    pub(crate) fn push_var(&mut self, rid: usize, ty: ty::ETy, name: Option<String>) {
        use crate::id_vector::ToUsize;
        let var_id = self.vars_map.insert(rid);
        assert!(var_id.to_usize() == self.vars.len());
        let var = ast::Var {
            index: var_id,
            name,
            ty,
        };
        self.vars.insert(var_id, var);
    }

    pub(crate) fn push_const_generic_var(&mut self, rid: u32, ty: LiteralTy, name: String) {
        use crate::id_vector::ToUsize;
        let var_id = self.const_generic_vars_map.insert(rid);
        assert!(var_id.to_usize() == self.vars.len());
        let var = ty::ConstGenericVar {
            index: var_id,
            name,
            ty,
        };
        self.const_generic_vars.insert(var_id, var);
    }

    pub(crate) fn fresh_block_id(&mut self, rid: hax::BasicBlock) -> ast::BlockId::Id {
        self.blocks_map.insert(rid)
    }

    pub(crate) fn push_block(&mut self, id: ast::BlockId::Id, block: ast::BlockData) {
        self.blocks.insert(id, block);
    }

    pub(crate) fn get_type_defs(&self) -> &ty::TypeDecls {
        &self.t_ctx.type_defs
    }

    pub(crate) fn fresh_trait_clause_id(&mut self) -> ty::TraitClauseId::Id {
        self.trait_clauses_counter.fresh_id()
    }
}

impl<'tcx, 'ctx> Formatter<ty::TypeDeclId::Id> for TransCtx<'tcx, 'ctx> {
    fn format_object(&self, id: ty::TypeDeclId::Id) -> String {
        self.type_defs.format_object(id)
    }
}

impl<'tcx, 'ctx> Formatter<ty::GlobalDeclId::Id> for TransCtx<'tcx, 'ctx> {
    fn format_object(&self, id: ty::GlobalDeclId::Id) -> String {
        self.global_defs.format_object(id)
    }
}

impl<'tcx, 'ctx> Formatter<ty::RegionVarId::Id> for TransCtx<'tcx, 'ctx> {
    fn format_object(&self, id: ty::RegionVarId::Id) -> String {
        id.to_pretty_string()
    }
}

impl<'tcx, 'ctx> Formatter<ty::TypeVarId::Id> for TransCtx<'tcx, 'ctx> {
    fn format_object(&self, id: ty::TypeVarId::Id) -> String {
        id.to_pretty_string()
    }
}

impl<'tcx, 'ctx> Formatter<&ty::Region<ty::RegionVarId::Id>> for TransCtx<'tcx, 'ctx> {
    fn format_object(&self, r: &ty::Region<ty::RegionVarId::Id>) -> String {
        r.fmt_with_ctx(self)
    }
}

impl<'tcx, 'ctx> Formatter<ty::ConstGenericVarId::Id> for TransCtx<'tcx, 'ctx> {
    fn format_object(&self, id: ty::ConstGenericVarId::Id) -> String {
        id.to_pretty_string()
    }
}

impl<'tcx, 'ctx, 'ctx1> Formatter<ty::TypeVarId::Id> for BodyTransCtx<'tcx, 'ctx, 'ctx1> {
    fn format_object(&self, id: ty::TypeVarId::Id) -> String {
        let v = self.type_vars.get(id).unwrap();
        v.to_string()
    }
}

impl<'tcx, 'ctx, 'ctx1> Formatter<ty::ConstGenericVarId::Id> for BodyTransCtx<'tcx, 'ctx, 'ctx1> {
    fn format_object(&self, id: ty::ConstGenericVarId::Id) -> String {
        let v = self.const_generic_vars.get(id).unwrap();
        v.to_string()
    }
}

impl<'tcx, 'ctx, 'ctx1> Formatter<v::VarId::Id> for BodyTransCtx<'tcx, 'ctx, 'ctx1> {
    fn format_object(&self, id: v::VarId::Id) -> String {
        let v = self.vars.get(id).unwrap();
        v.to_string()
    }
}

impl<'tcx, 'ctx, 'ctx1> Formatter<ty::RegionVarId::Id> for BodyTransCtx<'tcx, 'ctx, 'ctx1> {
    fn format_object(&self, id: ty::RegionVarId::Id) -> String {
        let v = self.region_vars.get(id).unwrap();
        v.to_string()
    }
}

impl<'tcx, 'ctx, 'ctx1> Formatter<&ty::Region<ty::RegionVarId::Id>>
    for BodyTransCtx<'tcx, 'ctx, 'ctx1>
{
    fn format_object(&self, r: &ty::Region<ty::RegionVarId::Id>) -> String {
        r.fmt_with_ctx(self)
    }
}

impl<'tcx, 'ctx, 'ctx1> Formatter<&ty::ErasedRegion> for BodyTransCtx<'tcx, 'ctx, 'ctx1> {
    fn format_object(&self, _: &ty::ErasedRegion) -> String {
        "'_".to_owned()
    }
}

impl<'tcx, 'ctx, 'ctx1> Formatter<ty::TypeDeclId::Id> for BodyTransCtx<'tcx, 'ctx, 'ctx1> {
    fn format_object(&self, id: ty::TypeDeclId::Id) -> String {
        self.t_ctx.type_defs.format_object(id)
    }
}

impl<'tcx, 'ctx, 'ctx1> Formatter<ty::GlobalDeclId::Id> for BodyTransCtx<'tcx, 'ctx, 'ctx1> {
    fn format_object(&self, id: ty::GlobalDeclId::Id) -> String {
        self.t_ctx.global_defs.format_object(id)
    }
}

impl<'tcx, 'ctx, 'ctx1> Formatter<&ty::Ty<ty::Region<ty::RegionVarId::Id>>>
    for BodyTransCtx<'tcx, 'ctx, 'ctx1>
{
    fn format_object(&self, ty: &ty::Ty<ty::Region<ty::RegionVarId::Id>>) -> String {
        ty.fmt_with_ctx(self)
    }
}

impl<'tcx, 'ctx, 'ctx1> Formatter<&ty::Ty<ty::ErasedRegion>> for BodyTransCtx<'tcx, 'ctx, 'ctx1> {
    fn format_object(&self, ty: &ty::Ty<ty::ErasedRegion>) -> String {
        ty.fmt_with_ctx(self)
    }
}

/// Auxiliary definition used to format definitions.
pub(crate) struct TypeDeclFormatter<'a> {
    pub type_defs: &'a ty::TypeDecls,
    pub global_defs: &'a ast::GlobalDecls,
    /// The region parameters of the definition we are printing (needed to
    /// correctly pretty print region var ids)
    pub region_params: &'a ty::RegionVarId::Vector<ty::RegionVar>,
    /// The type parameters of the definition we are printing (needed to
    /// correctly pretty print type var ids)
    pub type_params: &'a ty::TypeVarId::Vector<ty::TypeVar>,
    /// The const generic parameters of the definition we are printing (needed to
    /// correctly pretty print type var ids)
    pub const_generic_params: &'a ty::ConstGenericVarId::Vector<ty::ConstGenericVar>,
}

impl<'a> Formatter<ty::RegionVarId::Id> for TypeDeclFormatter<'a> {
    fn format_object(&self, id: ty::RegionVarId::Id) -> String {
        // Lookup the region parameter
        let v = self.region_params.get(id).unwrap();
        // Format
        v.to_string()
    }
}

impl<'a> Formatter<ty::ConstGenericVarId::Id> for TypeDeclFormatter<'a> {
    fn format_object(&self, id: ty::ConstGenericVarId::Id) -> String {
        // Lookup the region parameter
        let v = self.const_generic_params.get(id).unwrap();
        // Format
        v.to_string()
    }
}

impl<'a> Formatter<ty::TypeVarId::Id> for TypeDeclFormatter<'a> {
    fn format_object(&self, id: ty::TypeVarId::Id) -> String {
        // Lookup the type parameter
        let v = self.type_params.get(id).unwrap();
        // Format
        v.to_string()
    }
}

impl<'a> Formatter<&ty::Region<ty::RegionVarId::Id>> for TypeDeclFormatter<'a> {
    fn format_object(&self, r: &ty::Region<ty::RegionVarId::Id>) -> String {
        r.fmt_with_ctx(self)
    }
}

impl<'a> Formatter<&ty::ErasedRegion> for TypeDeclFormatter<'a> {
    fn format_object(&self, _: &ty::ErasedRegion) -> String {
        "".to_owned()
    }
}

impl<'a> Formatter<&ty::TypeDecl> for TypeDeclFormatter<'a> {
    fn format_object(&self, def: &ty::TypeDecl) -> String {
        def.fmt_with_ctx(self)
    }
}

impl<'a> Formatter<ty::TypeDeclId::Id> for TypeDeclFormatter<'a> {
    fn format_object(&self, id: ty::TypeDeclId::Id) -> String {
        self.type_defs.format_object(id)
    }
}

impl<'a> Formatter<ty::GlobalDeclId::Id> for TypeDeclFormatter<'a> {
    fn format_object(&self, id: ty::GlobalDeclId::Id) -> String {
        self.global_defs.format_object(id)
    }
}

impl<'tcx, 'ctx, 'ctx1> Formatter<&ty::TypeDecl> for BodyTransCtx<'tcx, 'ctx, 'ctx1> {
    fn format_object(&self, def: &ty::TypeDecl) -> String {
        // Create a type def formatter (which will take care of the
        // type parameters)
        let formatter = TypeDeclFormatter {
            type_defs: &self.t_ctx.type_defs,
            global_defs: &self.t_ctx.global_defs,
            region_params: &def.region_params,
            type_params: &def.type_params,
            const_generic_params: &def.const_generic_params,
        };
        formatter.format_object(def)
    }
}

impl<'tcx, 'ctx> fmt::Display for TransCtx<'tcx, 'ctx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // We do simple: types, globals, functions
        for (_, d) in &self.type_defs {
            // TODO: update to also use the type declaration (gives access
            // to the type variables and const generics...)
            writeln!(f, "{}\n", d.fmt_with_ctx(self))?
        }

        for (_, d) in &self.global_defs {
            writeln!(
                f,
                "{}\n",
                d.fmt_with_decls(
                    &self.type_defs,
                    &self.fun_defs,
                    &self.global_defs,
                    &self.trait_defs
                )
            )?
        }

        for (_, d) in &self.fun_defs {
            writeln!(
                f,
                "{}\n",
                d.fmt_with_decls(
                    &self.type_defs,
                    &self.fun_defs,
                    &self.global_defs,
                    &self.trait_defs
                )
            )?
        }

        fmt::Result::Ok(())
    }
}

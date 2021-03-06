// Copyright 2012 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.


use hir::def_id::DefId;
use ty::subst::{self, Subst};
use ty::{BrAnon, BrEnv, BrFresh, BrNamed};
use ty::{TyBool, TyChar, TyStruct, TyEnum};
use ty::{TyError, TyStr, TyArray, TySlice, TyFloat, TyFnDef, TyFnPtr};
use ty::{TyParam, TyRawPtr, TyRef, TyTuple};
use ty::TyClosure;
use ty::{TyBox, TyTrait, TyInt, TyUint, TyInfer};
use ty::{self, Ty, TyCtxt, TypeFoldable};
use ty::fold::{TypeFolder, TypeVisitor};

use std::cell::Cell;
use std::fmt;
use syntax::abi::Abi;
use syntax::parse::token;
use syntax::ast::CRATE_NODE_ID;
use hir;

pub fn verbose() -> bool {
    ty::tls::with(|tcx| tcx.sess.verbose())
}

fn fn_sig(f: &mut fmt::Formatter,
          inputs: &[Ty],
          variadic: bool,
          output: ty::FnOutput)
          -> fmt::Result {
    write!(f, "(")?;
    let mut inputs = inputs.iter();
    if let Some(&ty) = inputs.next() {
        write!(f, "{}", ty)?;
        for &ty in inputs {
            write!(f, ", {}", ty)?;
        }
        if variadic {
            write!(f, ", ...")?;
        }
    }
    write!(f, ")")?;

    match output {
        ty::FnConverging(ty) => {
            if !ty.is_nil() {
                write!(f, " -> {}", ty)?;
            }
            Ok(())
        }
        ty::FnDiverging => {
            write!(f, " -> !")
        }
    }
}

/// Namespace of the path given to parameterized to print.
#[derive(Copy, Clone, PartialEq)]
pub enum Ns {
    Type,
    Value
}

fn number_of_supplied_defaults<'a, 'gcx, 'tcx, GG>(tcx: TyCtxt<'a, 'gcx, 'tcx>,
                                                   substs: &subst::Substs,
                                                   space: subst::ParamSpace,
                                                   get_generics: GG)
                                                   -> usize
    where GG: FnOnce(TyCtxt<'a, 'gcx, 'tcx>) -> ty::Generics<'tcx>
{
    let generics = get_generics(tcx);

    let has_self = substs.self_ty().is_some();
    let ty_params = generics.types.get_slice(space);
    let tps = substs.types.get_slice(space);
    if ty_params.last().map_or(false, |def| def.default.is_some()) {
        let substs = tcx.lift(&substs);
        ty_params.iter().zip(tps).rev().take_while(|&(def, &actual)| {
            match def.default {
                Some(default) => {
                    if !has_self && default.has_self_ty() {
                        // In an object type, there is no `Self`, and
                        // thus if the default value references Self,
                        // the user will be required to give an
                        // explicit value. We can't even do the
                        // substitution below to check without causing
                        // an ICE. (#18956).
                        false
                    } else {
                        let default = tcx.lift(&default);
                        substs.and_then(|substs| default.subst(tcx, substs))
                            == Some(actual)
                    }
                }
                None => false
            }
        }).count()
    } else {
        0
    }
}

pub fn parameterized<GG>(f: &mut fmt::Formatter,
                         substs: &subst::Substs,
                         did: DefId,
                         ns: Ns,
                         projections: &[ty::ProjectionPredicate],
                         get_generics: GG)
                         -> fmt::Result
    where GG: for<'a, 'gcx, 'tcx> FnOnce(TyCtxt<'a, 'gcx, 'tcx>) -> ty::Generics<'tcx>
{
    if let (Ns::Value, Some(self_ty)) = (ns, substs.self_ty()) {
        write!(f, "<{} as ", self_ty)?;
    }

    let (fn_trait_kind, verbose, item_name) = ty::tls::with(|tcx| {
        let (did, item_name) = if ns == Ns::Value {
            // Try to get the impl/trait parent, if this is an
            // associated value item (method or constant).
            tcx.trait_of_item(did).or_else(|| tcx.impl_of_method(did))
               .map_or((did, None), |parent| (parent, Some(tcx.item_name(did))))
        } else {
            (did, None)
        };
        write!(f, "{}", tcx.item_path_str(did))?;
        Ok((tcx.lang_items.fn_trait_kind(did), tcx.sess.verbose(), item_name))
    })?;

    if !verbose && fn_trait_kind.is_some() && projections.len() == 1 {
        let projection_ty = projections[0].ty;
        if let TyTuple(ref args) = substs.types.get_slice(subst::TypeSpace)[0].sty {
            return fn_sig(f, args, false, ty::FnConverging(projection_ty));
        }
    }

    let empty = Cell::new(true);
    let start_or_continue = |f: &mut fmt::Formatter, start: &str, cont: &str| {
        if empty.get() {
            empty.set(false);
            write!(f, "{}", start)
        } else {
            write!(f, "{}", cont)
        }
    };
    let print_region = |f: &mut fmt::Formatter, region: &ty::Region| -> _ {
        if verbose {
            write!(f, "{:?}", region)
        } else {
            let s = region.to_string();
            if s.is_empty() {
                // This happens when the value of the region
                // parameter is not easily serialized. This may be
                // because the user omitted it in the first place,
                // or because it refers to some block in the code,
                // etc. I'm not sure how best to serialize this.
                write!(f, "'_")
            } else {
                write!(f, "{}", s)
            }
        }
    };

    for region in substs.regions.get_slice(subst::TypeSpace) {
        start_or_continue(f, "<", ", ")?;
        print_region(f, region)?;
    }

    let num_supplied_defaults = if verbose {
        0
    } else {
        // It is important to execute this conditionally, only if -Z
        // verbose is false. Otherwise, debug logs can sometimes cause
        // ICEs trying to fetch the generics early in the pipeline. This
        // is kind of a hacky workaround in that -Z verbose is required to
        // avoid those ICEs.
        ty::tls::with(|tcx| {
            number_of_supplied_defaults(tcx, substs, subst::TypeSpace, get_generics)
        })
    };

    let tps = substs.types.get_slice(subst::TypeSpace);

    for &ty in &tps[..tps.len() - num_supplied_defaults] {
        start_or_continue(f, "<", ", ")?;
        write!(f, "{}", ty)?;
    }

    for projection in projections {
        start_or_continue(f, "<", ", ")?;
        write!(f, "{}={}",
               projection.projection_ty.item_name,
               projection.ty)?;
    }

    start_or_continue(f, "", ">")?;

    // For values, also print their name and type parameters.
    if ns == Ns::Value {
        empty.set(true);

        if substs.self_ty().is_some() {
            write!(f, ">")?;
        }

        if let Some(item_name) = item_name {
            write!(f, "::{}", item_name)?;
        }

        for region in substs.regions.get_slice(subst::FnSpace) {
            start_or_continue(f, "::<", ", ")?;
            print_region(f, region)?;
        }

        // FIXME: consider being smart with defaults here too
        for ty in substs.types.get_slice(subst::FnSpace) {
            start_or_continue(f, "::<", ", ")?;
            write!(f, "{}", ty)?;
        }

        start_or_continue(f, "", ">")?;
    }

    Ok(())
}

fn in_binder<'a, 'gcx, 'tcx, T, U>(f: &mut fmt::Formatter,
                                   tcx: TyCtxt<'a, 'gcx, 'tcx>,
                                   original: &ty::Binder<T>,
                                   lifted: Option<ty::Binder<U>>) -> fmt::Result
    where T: fmt::Display, U: fmt::Display + TypeFoldable<'tcx>
{
    // Replace any anonymous late-bound regions with named
    // variants, using gensym'd identifiers, so that we can
    // clearly differentiate between named and unnamed regions in
    // the output. We'll probably want to tweak this over time to
    // decide just how much information to give.
    let value = if let Some(v) = lifted {
        v
    } else {
        return write!(f, "{}", original.0);
    };

    let mut empty = true;
    let mut start_or_continue = |f: &mut fmt::Formatter, start: &str, cont: &str| {
        if empty {
            empty = false;
            write!(f, "{}", start)
        } else {
            write!(f, "{}", cont)
        }
    };

    let new_value = tcx.replace_late_bound_regions(&value, |br| {
        let _ = start_or_continue(f, "for<", ", ");
        ty::ReLateBound(ty::DebruijnIndex::new(1), match br {
            ty::BrNamed(_, name) => {
                let _ = write!(f, "{}", name);
                br
            }
            ty::BrAnon(_) |
            ty::BrFresh(_) |
            ty::BrEnv => {
                let name = token::intern("'r");
                let _ = write!(f, "{}", name);
                ty::BrNamed(tcx.map.local_def_id(CRATE_NODE_ID), name)
            }
        })
    }).0;

    start_or_continue(f, "", "> ")?;
    write!(f, "{}", new_value)
}

/// This curious type is here to help pretty-print trait objects. In
/// a trait object, the projections are stored separately from the
/// main trait bound, but in fact we want to package them together
/// when printing out; they also have separate binders, but we want
/// them to share a binder when we print them out. (And the binder
/// pretty-printing logic is kind of clever and we don't want to
/// reproduce it.) So we just repackage up the structure somewhat.
///
/// Right now there is only one trait in an object that can have
/// projection bounds, so we just stuff them altogether. But in
/// reality we should eventually sort things out better.
#[derive(Clone, Debug)]
struct TraitAndProjections<'tcx>(ty::TraitRef<'tcx>, Vec<ty::ProjectionPredicate<'tcx>>);

impl<'tcx> TypeFoldable<'tcx> for TraitAndProjections<'tcx> {
    fn super_fold_with<'gcx: 'tcx, F: TypeFolder<'gcx, 'tcx>>(&self, folder: &mut F) -> Self {
        TraitAndProjections(self.0.fold_with(folder), self.1.fold_with(folder))
    }

    fn super_visit_with<V: TypeVisitor<'tcx>>(&self, visitor: &mut V) -> bool {
        self.0.visit_with(visitor) || self.1.visit_with(visitor)
    }
}

impl<'tcx> fmt::Display for TraitAndProjections<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let TraitAndProjections(ref trait_ref, ref projection_bounds) = *self;
        parameterized(f, trait_ref.substs,
                      trait_ref.def_id,
                      Ns::Type,
                      projection_bounds,
                      |tcx| tcx.lookup_trait_def(trait_ref.def_id).generics.clone())
    }
}

impl<'tcx> fmt::Display for ty::TraitTy<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let bounds = &self.bounds;

        // Generate the main trait ref, including associated types.
        ty::tls::with(|tcx| {
            let principal = tcx.lift(&self.principal.0)
                               .expect("could not lift TraitRef for printing");
            let projections = tcx.lift(&bounds.projection_bounds[..])
                                 .expect("could not lift projections for printing");
            let projections = projections.into_iter().map(|p| p.0).collect();

            let tap = ty::Binder(TraitAndProjections(principal, projections));
            in_binder(f, tcx, &ty::Binder(""), Some(tap))
        })?;

        // Builtin bounds.
        for bound in &bounds.builtin_bounds {
            write!(f, " + {:?}", bound)?;
        }

        // FIXME: It'd be nice to compute from context when this bound
        // is implied, but that's non-trivial -- we'd perhaps have to
        // use thread-local data of some kind? There are also
        // advantages to just showing the region, since it makes
        // people aware that it's there.
        let bound = bounds.region_bound.to_string();
        if !bound.is_empty() {
            write!(f, " + {}", bound)?;
        }

        Ok(())
    }
}

impl<'tcx> fmt::Debug for ty::TypeParameterDef<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "TypeParameterDef({}, {:?}, {:?}/{})",
               self.name,
               self.def_id,
               self.space, self.index)
    }
}

impl fmt::Debug for ty::RegionParameterDef {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "RegionParameterDef({}, {:?}, {:?}/{}, {:?})",
               self.name,
               self.def_id,
               self.space, self.index,
               self.bounds)
    }
}

impl<'tcx> fmt::Debug for ty::TyS<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", *self)
    }
}

impl<'tcx> fmt::Display for ty::TypeAndMut<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}{}",
               if self.mutbl == hir::MutMutable { "mut " } else { "" },
               self.ty)
    }
}

impl<'tcx> fmt::Debug for subst::Substs<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Substs[types={:?}, regions={:?}]",
               self.types, self.regions)
    }
}

impl<'tcx> fmt::Debug for ty::ItemSubsts<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ItemSubsts({:?})", self.substs)
    }
}

impl<'tcx> fmt::Debug for ty::TraitRef<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // when printing out the debug representation, we don't need
        // to enumerate the `for<...>` etc because the debruijn index
        // tells you everything you need to know.
        match self.substs.self_ty() {
            None => write!(f, "{}", *self),
            Some(self_ty) => write!(f, "<{:?} as {}>", self_ty, *self)
        }
    }
}

impl<'tcx> fmt::Debug for ty::TraitDef<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "TraitDef(generics={:?}, trait_ref={:?})",
               self.generics, self.trait_ref)
    }
}

impl<'tcx, 'container> fmt::Debug for ty::AdtDefData<'tcx, 'container> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        ty::tls::with(|tcx| {
            write!(f, "{}", tcx.item_path_str(self.did))
        })
    }
}

impl<'tcx> fmt::Debug for ty::adjustment::AutoAdjustment<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ty::adjustment::AdjustReifyFnPointer => {
                write!(f, "AdjustReifyFnPointer")
            }
            ty::adjustment::AdjustUnsafeFnPointer => {
                write!(f, "AdjustUnsafeFnPointer")
            }
            ty::adjustment::AdjustMutToConstPointer => {
                write!(f, "AdjustMutToConstPointer")
            }
            ty::adjustment::AdjustDerefRef(ref data) => {
                write!(f, "{:?}", data)
            }
        }
    }
}

impl<'tcx> fmt::Debug for ty::adjustment::AutoDerefRef<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "AutoDerefRef({}, unsize={:?}, {:?})",
               self.autoderefs, self.unsize, self.autoref)
    }
}

impl<'tcx> fmt::Debug for ty::TraitTy<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "TraitTy({:?},{:?})",
               self.principal,
               self.bounds)
    }
}

impl<'tcx> fmt::Debug for ty::Predicate<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ty::Predicate::Trait(ref a) => write!(f, "{:?}", a),
            ty::Predicate::Rfc1592(ref a) => {
                write!(f, "RFC1592({:?})", a)
            }
            ty::Predicate::Equate(ref pair) => write!(f, "{:?}", pair),
            ty::Predicate::RegionOutlives(ref pair) => write!(f, "{:?}", pair),
            ty::Predicate::TypeOutlives(ref pair) => write!(f, "{:?}", pair),
            ty::Predicate::Projection(ref pair) => write!(f, "{:?}", pair),
            ty::Predicate::WellFormed(ty) => write!(f, "WF({:?})", ty),
            ty::Predicate::ObjectSafe(trait_def_id) => {
                write!(f, "ObjectSafe({:?})", trait_def_id)
            }
            ty::Predicate::ClosureKind(closure_def_id, kind) => {
                write!(f, "ClosureKind({:?}, {:?})", closure_def_id, kind)
            }
        }
    }
}

impl fmt::Display for ty::BoundRegion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if verbose() {
            return write!(f, "{:?}", *self);
        }

        match *self {
            BrNamed(_, name) => write!(f, "{}", name),
            BrAnon(_) | BrFresh(_) | BrEnv => Ok(())
        }
    }
}

impl fmt::Debug for ty::BoundRegion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            BrAnon(n) => write!(f, "BrAnon({:?})", n),
            BrFresh(n) => write!(f, "BrFresh({:?})", n),
            BrNamed(did, name) => {
                write!(f, "BrNamed({:?}:{:?}, {:?})", did.krate, did.index, name)
            }
            BrEnv => "BrEnv".fmt(f),
        }
    }
}

impl fmt::Debug for ty::Region {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ty::ReEarlyBound(ref data) => {
                write!(f, "ReEarlyBound({:?}, {}, {})",
                       data.space,
                       data.index,
                       data.name)
            }

            ty::ReLateBound(binder_id, ref bound_region) => {
                write!(f, "ReLateBound({:?}, {:?})",
                       binder_id,
                       bound_region)
            }

            ty::ReFree(ref fr) => write!(f, "{:?}", fr),

            ty::ReScope(id) => {
                write!(f, "ReScope({:?})", id)
            }

            ty::ReStatic => write!(f, "ReStatic"),

            ty::ReVar(ref vid) => {
                write!(f, "{:?}", vid)
            }

            ty::ReSkolemized(id, ref bound_region) => {
                write!(f, "ReSkolemized({}, {:?})", id.index, bound_region)
            }

            ty::ReEmpty => write!(f, "ReEmpty")
        }
    }
}

impl<'tcx> fmt::Debug for ty::ClosureTy<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ClosureTy({},{:?},{})",
               self.unsafety,
               self.sig,
               self.abi)
    }
}

impl<'tcx> fmt::Debug for ty::ClosureUpvar<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ClosureUpvar({:?},{:?})",
               self.def,
               self.ty)
    }
}

impl<'tcx> fmt::Debug for ty::ParameterEnvironment<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ParameterEnvironment(\
            free_substs={:?}, \
            implicit_region_bound={:?}, \
            caller_bounds={:?})",
            self.free_substs,
            self.implicit_region_bound,
            self.caller_bounds)
    }
}

impl<'tcx> fmt::Debug for ty::ObjectLifetimeDefault {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ty::ObjectLifetimeDefault::Ambiguous => write!(f, "Ambiguous"),
            ty::ObjectLifetimeDefault::BaseDefault => write!(f, "BaseDefault"),
            ty::ObjectLifetimeDefault::Specific(ref r) => write!(f, "{:?}", r),
        }
    }
}

impl fmt::Display for ty::Region {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if verbose() {
            return write!(f, "{:?}", *self);
        }

        // These printouts are concise.  They do not contain all the information
        // the user might want to diagnose an error, but there is basically no way
        // to fit that into a short string.  Hence the recommendation to use
        // `explain_region()` or `note_and_explain_region()`.
        match *self {
            ty::ReEarlyBound(ref data) => {
                write!(f, "{}", data.name)
            }
            ty::ReLateBound(_, br) |
            ty::ReFree(ty::FreeRegion { bound_region: br, .. }) |
            ty::ReSkolemized(_, br) => {
                write!(f, "{}", br)
            }
            ty::ReScope(_) |
            ty::ReVar(_) => Ok(()),
            ty::ReStatic => write!(f, "'static"),
            ty::ReEmpty => write!(f, "'<empty>"),
        }
    }
}

impl fmt::Debug for ty::FreeRegion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ReFree({:?}, {:?})",
               self.scope, self.bound_region)
    }
}

impl fmt::Debug for ty::Variance {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match *self {
            ty::Covariant => "+",
            ty::Contravariant => "-",
            ty::Invariant => "o",
            ty::Bivariant => "*",
        })
    }
}

impl fmt::Debug for ty::ItemVariances {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ItemVariances(types={:?}, regions={:?})",
               self.types, self.regions)
    }
}

impl<'tcx> fmt::Debug for ty::GenericPredicates<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "GenericPredicates({:?})", self.predicates)
    }
}

impl<'tcx> fmt::Debug for ty::InstantiatedPredicates<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "InstantiatedPredicates({:?})",
               self.predicates)
    }
}

impl<'tcx> fmt::Debug for ty::ImplOrTraitItem<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ImplOrTraitItem(")?;
        match *self {
            ty::ImplOrTraitItem::MethodTraitItem(ref i) => write!(f, "{:?}", i),
            ty::ImplOrTraitItem::ConstTraitItem(ref i) => write!(f, "{:?}", i),
            ty::ImplOrTraitItem::TypeTraitItem(ref i) => write!(f, "{:?}", i),
        }?;
        write!(f, ")")
    }
}

impl<'tcx> fmt::Display for ty::FnSig<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "fn")?;
        fn_sig(f, &self.inputs, self.variadic, self.output)
    }
}

impl<'tcx> fmt::Debug for ty::ExistentialBounds<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut empty = true;
        let mut maybe_continue = |f: &mut fmt::Formatter| {
            if empty {
                empty = false;
                Ok(())
            } else {
                write!(f, " + ")
            }
        };

        let region_str = format!("{:?}", self.region_bound);
        if !region_str.is_empty() {
            maybe_continue(f)?;
            write!(f, "{}", region_str)?;
        }

        for bound in &self.builtin_bounds {
            maybe_continue(f)?;
            write!(f, "{:?}", bound)?;
        }

        for projection_bound in &self.projection_bounds {
            maybe_continue(f)?;
            write!(f, "{:?}", projection_bound)?;
        }

        Ok(())
    }
}

impl fmt::Display for ty::BuiltinBounds {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut bounds = self.iter();
        if let Some(bound) = bounds.next() {
            write!(f, "{:?}", bound)?;
            for bound in bounds {
                write!(f, " + {:?}", bound)?;
            }
        }
        Ok(())
    }
}

impl fmt::Debug for ty::TyVid {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "_#{}t", self.index)
    }
}

impl fmt::Debug for ty::IntVid {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "_#{}i", self.index)
    }
}

impl fmt::Debug for ty::FloatVid {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "_#{}f", self.index)
    }
}

impl fmt::Debug for ty::RegionVid {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "'_#{}r", self.index)
    }
}

impl<'tcx> fmt::Debug for ty::FnSig<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "({:?}; variadic: {})->{:?}", self.inputs, self.variadic, self.output)
    }
}

impl fmt::Debug for ty::InferTy {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ty::TyVar(ref v) => v.fmt(f),
            ty::IntVar(ref v) => v.fmt(f),
            ty::FloatVar(ref v) => v.fmt(f),
            ty::FreshTy(v) => write!(f, "FreshTy({:?})", v),
            ty::FreshIntTy(v) => write!(f, "FreshIntTy({:?})", v),
            ty::FreshFloatTy(v) => write!(f, "FreshFloatTy({:?})", v)
        }
    }
}

impl fmt::Debug for ty::IntVarValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ty::IntType(ref v) => v.fmt(f),
            ty::UintType(ref v) => v.fmt(f),
        }
    }
}

// The generic impl doesn't work yet because projections are not
// normalized under HRTB.
/*impl<T> fmt::Display for ty::Binder<T>
    where T: fmt::Display + for<'a> ty::Lift<'a>,
          for<'a> <T as ty::Lift<'a>>::Lifted: fmt::Display + TypeFoldable<'a>
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        ty::tls::with(|tcx| in_binder(f, tcx, self, tcx.lift(self)))
    }
}*/

impl<'tcx> fmt::Display for ty::Binder<ty::TraitRef<'tcx>> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        ty::tls::with(|tcx| in_binder(f, tcx, self, tcx.lift(self)))
    }
}

impl<'tcx> fmt::Display for ty::Binder<ty::TraitPredicate<'tcx>> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        ty::tls::with(|tcx| in_binder(f, tcx, self, tcx.lift(self)))
    }
}

impl<'tcx> fmt::Display for ty::Binder<ty::EquatePredicate<'tcx>> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        ty::tls::with(|tcx| in_binder(f, tcx, self, tcx.lift(self)))
    }
}

impl<'tcx> fmt::Display for ty::Binder<ty::ProjectionPredicate<'tcx>> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        ty::tls::with(|tcx| in_binder(f, tcx, self, tcx.lift(self)))
    }
}

impl<'tcx> fmt::Display for ty::Binder<ty::OutlivesPredicate<Ty<'tcx>, ty::Region>> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        ty::tls::with(|tcx| in_binder(f, tcx, self, tcx.lift(self)))
    }
}

impl fmt::Display for ty::Binder<ty::OutlivesPredicate<ty::Region, ty::Region>> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        ty::tls::with(|tcx| in_binder(f, tcx, self, tcx.lift(self)))
    }
}

impl<'tcx> fmt::Display for ty::TraitRef<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        parameterized(f, self.substs, self.def_id, Ns::Type, &[],
                      |tcx| tcx.lookup_trait_def(self.def_id).generics.clone())
    }
}

impl<'tcx> fmt::Display for ty::TypeVariants<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            TyBool => write!(f, "bool"),
            TyChar => write!(f, "char"),
            TyInt(t) => write!(f, "{}", t.ty_to_string()),
            TyUint(t) => write!(f, "{}", t.ty_to_string()),
            TyFloat(t) => write!(f, "{}", t.ty_to_string()),
            TyBox(typ) => write!(f, "Box<{}>",  typ),
            TyRawPtr(ref tm) => {
                write!(f, "*{} {}", match tm.mutbl {
                    hir::MutMutable => "mut",
                    hir::MutImmutable => "const",
                },  tm.ty)
            }
            TyRef(r, ref tm) => {
                write!(f, "&")?;
                let s = r.to_string();
                write!(f, "{}", s)?;
                if !s.is_empty() {
                    write!(f, " ")?;
                }
                write!(f, "{}", tm)
            }
            TyTuple(ref tys) => {
                write!(f, "(")?;
                let mut tys = tys.iter();
                if let Some(&ty) = tys.next() {
                    write!(f, "{},", ty)?;
                    if let Some(&ty) = tys.next() {
                        write!(f, " {}", ty)?;
                        for &ty in tys {
                            write!(f, ", {}", ty)?;
                        }
                    }
                }
                write!(f, ")")
            }
            TyFnDef(def_id, substs, ref bare_fn) => {
                if bare_fn.unsafety == hir::Unsafety::Unsafe {
                    write!(f, "unsafe ")?;
                }

                if bare_fn.abi != Abi::Rust {
                    write!(f, "extern {} ", bare_fn.abi)?;
                }

                write!(f, "{} {{", bare_fn.sig.0)?;
                parameterized(f, substs, def_id, Ns::Value, &[],
                              |tcx| tcx.lookup_item_type(def_id).generics)?;
                write!(f, "}}")
            }
            TyFnPtr(ref bare_fn) => {
                if bare_fn.unsafety == hir::Unsafety::Unsafe {
                    write!(f, "unsafe ")?;
                }

                if bare_fn.abi != Abi::Rust {
                    write!(f, "extern {} ", bare_fn.abi)?;
                }

                write!(f, "{}", bare_fn.sig.0)
            }
            TyInfer(infer_ty) => write!(f, "{}", infer_ty),
            TyError => write!(f, "[type error]"),
            TyParam(ref param_ty) => write!(f, "{}", param_ty),
            TyEnum(def, substs) | TyStruct(def, substs) => {
                ty::tls::with(|tcx| {
                    if def.did.is_local() &&
                          !tcx.tcache.borrow().contains_key(&def.did) {
                        write!(f, "{}<..>", tcx.item_path_str(def.did))
                    } else {
                        parameterized(f, substs, def.did, Ns::Type, &[],
                                      |tcx| tcx.lookup_item_type(def.did).generics)
                    }
                })
            }
            TyTrait(ref data) => write!(f, "{}", data),
            ty::TyProjection(ref data) => write!(f, "{}", data),
            TyStr => write!(f, "str"),
            TyClosure(did, substs) => ty::tls::with(|tcx| {
                write!(f, "[closure")?;

                if let Some(node_id) = tcx.map.as_local_node_id(did) {
                    write!(f, "@{:?}", tcx.map.span(node_id))?;
                    let mut sep = " ";
                    tcx.with_freevars(node_id, |freevars| {
                        for (freevar, upvar_ty) in freevars.iter().zip(substs.upvar_tys) {
                            let node_id = freevar.def.var_id();
                            write!(f,
                                        "{}{}:{}",
                                        sep,
                                        tcx.local_var_name_str(node_id),
                                        upvar_ty)?;
                            sep = ", ";
                        }
                        Ok(())
                    })?
                } else {
                    // cross-crate closure types should only be
                    // visible in trans bug reports, I imagine.
                    write!(f, "@{:?}", did)?;
                    let mut sep = " ";
                    for (index, upvar_ty) in substs.upvar_tys.iter().enumerate() {
                        write!(f, "{}{}:{}", sep, index, upvar_ty)?;
                        sep = ", ";
                    }
                }

                write!(f, "]")
            }),
            TyArray(ty, sz) => write!(f, "[{}; {}]",  ty, sz),
            TySlice(ty) => write!(f, "[{}]",  ty)
        }
    }
}

impl<'tcx> fmt::Display for ty::TyS<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.sty)
    }
}

impl fmt::Debug for ty::UpvarId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "UpvarId({};`{}`;{})",
               self.var_id,
               ty::tls::with(|tcx| tcx.local_var_name_str(self.var_id)),
               self.closure_expr_id)
    }
}

impl fmt::Debug for ty::UpvarBorrow {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "UpvarBorrow({:?}, {:?})",
               self.kind, self.region)
    }
}

impl fmt::Display for ty::InferTy {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let print_var_ids = verbose();
        match *self {
            ty::TyVar(ref vid) if print_var_ids => write!(f, "{:?}", vid),
            ty::IntVar(ref vid) if print_var_ids => write!(f, "{:?}", vid),
            ty::FloatVar(ref vid) if print_var_ids => write!(f, "{:?}", vid),
            ty::TyVar(_) | ty::IntVar(_) | ty::FloatVar(_) => write!(f, "_"),
            ty::FreshTy(v) => write!(f, "FreshTy({})", v),
            ty::FreshIntTy(v) => write!(f, "FreshIntTy({})", v),
            ty::FreshFloatTy(v) => write!(f, "FreshFloatTy({})", v)
        }
    }
}

impl fmt::Display for ty::ExplicitSelfCategory {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match *self {
            ty::ExplicitSelfCategory::Static => "static",
            ty::ExplicitSelfCategory::ByValue => "self",
            ty::ExplicitSelfCategory::ByReference(_, hir::MutMutable) => {
                "&mut self"
            }
            ty::ExplicitSelfCategory::ByReference(_, hir::MutImmutable) => "&self",
            ty::ExplicitSelfCategory::ByBox => "Box<self>",
        })
    }
}

impl fmt::Display for ty::ParamTy {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl fmt::Debug for ty::ParamTy {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}/{:?}.{}", self, self.space, self.idx)
    }
}

impl<'tcx, T, U> fmt::Display for ty::OutlivesPredicate<T,U>
    where T: fmt::Display, U: fmt::Display
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} : {}", self.0, self.1)
    }
}

impl<'tcx> fmt::Display for ty::EquatePredicate<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} == {}", self.0, self.1)
    }
}

impl<'tcx> fmt::Debug for ty::TraitPredicate<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "TraitPredicate({:?})",
               self.trait_ref)
    }
}

impl<'tcx> fmt::Display for ty::TraitPredicate<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}: {}", self.trait_ref.self_ty(), self.trait_ref)
    }
}

impl<'tcx> fmt::Debug for ty::ProjectionPredicate<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ProjectionPredicate({:?}, {:?})",
               self.projection_ty,
               self.ty)
    }
}

impl<'tcx> fmt::Display for ty::ProjectionPredicate<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} == {}",
               self.projection_ty,
               self.ty)
    }
}

impl<'tcx> fmt::Display for ty::ProjectionTy<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}::{}",
               self.trait_ref,
               self.item_name)
    }
}

impl fmt::Display for ty::ClosureKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ty::ClosureKind::Fn => write!(f, "Fn"),
            ty::ClosureKind::FnMut => write!(f, "FnMut"),
            ty::ClosureKind::FnOnce => write!(f, "FnOnce"),
        }
    }
}

impl<'tcx> fmt::Display for ty::Predicate<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ty::Predicate::Trait(ref data) => write!(f, "{}", data),
            ty::Predicate::Rfc1592(ref data) => write!(f, "{}", data),
            ty::Predicate::Equate(ref predicate) => write!(f, "{}", predicate),
            ty::Predicate::RegionOutlives(ref predicate) => write!(f, "{}", predicate),
            ty::Predicate::TypeOutlives(ref predicate) => write!(f, "{}", predicate),
            ty::Predicate::Projection(ref predicate) => write!(f, "{}", predicate),
            ty::Predicate::WellFormed(ty) => write!(f, "{} well-formed", ty),
            ty::Predicate::ObjectSafe(trait_def_id) =>
                ty::tls::with(|tcx| {
                    write!(f, "the trait `{}` is object-safe", tcx.item_path_str(trait_def_id))
                }),
            ty::Predicate::ClosureKind(closure_def_id, kind) =>
                ty::tls::with(|tcx| {
                    write!(f, "the closure `{}` implements the trait `{}`",
                           tcx.item_path_str(closure_def_id), kind)
                }),
        }
    }
}

//! Polymorphization Analysis
//! =========================
//!
//! This module implements an analysis of functions, methods and closures to determine which
//! generic parameters are unused (and eventually, in what ways generic parameters are used - only
//! for their size, offset of a field, etc.).

use rustc_hir::{def::DefKind, def_id::DefId};
use rustc_index::bit_set::FiniteBitSet;
use rustc_middle::mir::{
    visit::{TyContext, Visitor},
    Local, LocalDecl, Location,
};
use rustc_middle::ty::{
    self,
    fold::{TypeFoldable, TypeVisitor},
    query::Providers,
    subst::SubstsRef,
    Const, Ty, TyCtxt,
};
use rustc_span::symbol::sym;
use std::convert::TryInto;

/// Provide implementations of queries relating to polymorphization analysis.
pub fn provide(providers: &mut Providers) {
    providers.unused_generic_params = unused_generic_params;
}

/// Determine which generic parameters are used by the function/method/closure represented by
/// `def_id`. Returns a bitset where bits representing unused parameters are set (`is_empty`
/// indicates all parameters are used).
fn unused_generic_params(tcx: TyCtxt<'_>, def_id: DefId) -> FiniteBitSet<u32> {
    debug!("unused_generic_params({:?})", def_id);

    if !tcx.sess.opts.debugging_opts.polymorphize {
        // If polymorphization disabled, then all parameters are used.
        return FiniteBitSet::new_empty();
    }

    // Polymorphization results are stored in cross-crate metadata only when there are unused
    // parameters, so assume that non-local items must have only used parameters (else this query
    // would not be invoked, and the cross-crate metadata used instead).
    if !def_id.is_local() {
        return FiniteBitSet::new_empty();
    }

    let generics = tcx.generics_of(def_id);
    debug!("unused_generic_params: generics={:?}", generics);

    // Exit early when there are no parameters to be unused.
    if generics.count() == 0 {
        return FiniteBitSet::new_empty();
    }

    // Exit early when there is no MIR available.
    if !tcx.is_mir_available(def_id) {
        debug!("unused_generic_params: (no mir available) def_id={:?}", def_id);
        return FiniteBitSet::new_empty();
    }

    // Create a bitset with N rightmost ones for each parameter.
    let generics_count: u32 =
        generics.count().try_into().expect("more generic parameters than can fit into a `u32`");
    let mut unused_parameters = FiniteBitSet::<u32>::new_empty();
    unused_parameters.set_range(0..generics_count);
    debug!("unused_generic_params: (start) unused_parameters={:?}", unused_parameters);
    mark_used_by_default_parameters(tcx, def_id, generics, &mut unused_parameters);
    debug!("unused_generic_params: (after default) unused_parameters={:?}", unused_parameters);

    // Visit MIR and accumululate used generic parameters.
    let body = tcx.optimized_mir(def_id);
    let mut vis = MarkUsedGenericParams { tcx, def_id, unused_parameters: &mut unused_parameters };
    vis.visit_body(body);
    debug!("unused_generic_params: (after visitor) unused_parameters={:?}", unused_parameters);

    mark_used_by_predicates(tcx, def_id, &mut unused_parameters);
    debug!("unused_generic_params: (end) unused_parameters={:?}", unused_parameters);

    // Emit errors for debugging and testing if enabled.
    if !unused_parameters.is_empty() {
        emit_unused_generic_params_error(tcx, def_id, generics, &unused_parameters);
    }

    unused_parameters
}

/// Some parameters are considered used-by-default, such as non-generic parameters and the dummy
/// generic parameters from closures, this function marks them as used. `leaf_is_closure` should
/// be `true` if the item that `unused_generic_params` was invoked on is a closure.
fn mark_used_by_default_parameters<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: DefId,
    generics: &'tcx ty::Generics,
    unused_parameters: &mut FiniteBitSet<u32>,
) {
    if !tcx.is_trait(def_id) && (tcx.is_closure(def_id) || tcx.type_of(def_id).is_generator()) {
        for param in &generics.params {
            debug!("mark_used_by_default_parameters: (closure/gen) param={:?}", param);
            unused_parameters.clear(param.index);
        }
    } else {
        for param in &generics.params {
            debug!("mark_used_by_default_parameters: (other) param={:?}", param);
            if let ty::GenericParamDefKind::Lifetime = param.kind {
                unused_parameters.clear(param.index);
            }
        }
    }

    if let Some(parent) = generics.parent {
        mark_used_by_default_parameters(tcx, parent, tcx.generics_of(parent), unused_parameters);
    }
}

/// Search the predicates on used generic parameters for any unused generic parameters, and mark
/// those as used.
fn mark_used_by_predicates<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: DefId,
    unused_parameters: &mut FiniteBitSet<u32>,
) {
    let def_id = tcx.closure_base_def_id(def_id);
    let predicates = tcx.explicit_predicates_of(def_id);
    debug!("mark_used_by_predicates: predicates_of={:?}", predicates);

    let mut current_unused_parameters = FiniteBitSet::new_empty();
    // Run to a fixed point to support `where T: Trait<U>, U: Trait<V>`, starting with an empty
    // bit set so that this is skipped if all parameters are already used.
    while current_unused_parameters != *unused_parameters {
        debug!(
            "mark_used_by_predicates: current_unused_parameters={:?} = unused_parameters={:?}",
            current_unused_parameters, unused_parameters
        );
        current_unused_parameters = *unused_parameters;

        for (predicate, _) in predicates.predicates {
            // Consider all generic params in a predicate as used if any other parameter in the
            // predicate is used.
            let any_param_used = {
                let mut vis = HasUsedGenericParams { unused_parameters };
                predicate.visit_with(&mut vis)
            };

            if any_param_used {
                let mut vis = MarkUsedGenericParams { tcx, def_id, unused_parameters };
                predicate.visit_with(&mut vis);
            }
        }
    }

    if let Some(parent) = predicates.parent {
        mark_used_by_predicates(tcx, parent, unused_parameters);
    }
}

/// Emit errors for the function annotated by `#[rustc_polymorphize_error]`, labelling each generic
/// parameter which was unused.
fn emit_unused_generic_params_error<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: DefId,
    generics: &'tcx ty::Generics,
    unused_parameters: &FiniteBitSet<u32>,
) {
    debug!("emit_unused_generic_params_error: def_id={:?}", def_id);
    let base_def_id = tcx.closure_base_def_id(def_id);
    if !tcx
        .get_attrs(base_def_id)
        .iter()
        .any(|a| tcx.sess.check_name(a, sym::rustc_polymorphize_error))
    {
        return;
    }

    debug!("emit_unused_generic_params_error: unused_parameters={:?}", unused_parameters);
    let fn_span = match tcx.opt_item_name(def_id) {
        Some(ident) => ident.span,
        _ => tcx.def_span(def_id),
    };

    let mut err = tcx.sess.struct_span_err(fn_span, "item has unused generic parameters");

    let mut next_generics = Some(generics);
    while let Some(generics) = next_generics {
        for param in &generics.params {
            if unused_parameters.contains(param.index).unwrap_or(false) {
                debug!("emit_unused_generic_params_error: param={:?}", param);
                let def_span = tcx.def_span(param.def_id);
                err.span_label(def_span, &format!("generic parameter `{}` is unused", param.name));
            }
        }

        next_generics = generics.parent.map(|did| tcx.generics_of(did));
    }

    err.emit();
}

/// Visitor used to aggregate generic parameter uses.
struct MarkUsedGenericParams<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    def_id: DefId,
    unused_parameters: &'a mut FiniteBitSet<u32>,
}

impl<'a, 'tcx> MarkUsedGenericParams<'a, 'tcx> {
    /// Invoke `unused_generic_params` on a body contained within the current item (e.g.
    /// a closure, generator or constant).
    fn visit_child_body(&mut self, def_id: DefId, substs: SubstsRef<'tcx>) {
        let unused = self.tcx.unused_generic_params(def_id);
        debug!(
            "visit_child_body: unused_parameters={:?} unused={:?}",
            self.unused_parameters, unused
        );
        for (i, arg) in substs.iter().enumerate() {
            let i = i.try_into().unwrap();
            if !unused.contains(i).unwrap_or(false) {
                arg.visit_with(self);
            }
        }
        debug!("visit_child_body: unused_parameters={:?}", self.unused_parameters);
    }
}

impl<'a, 'tcx> Visitor<'tcx> for MarkUsedGenericParams<'a, 'tcx> {
    fn visit_local_decl(&mut self, local: Local, local_decl: &LocalDecl<'tcx>) {
        debug!("visit_local_decl: local_decl={:?}", local_decl);
        if local == Local::from_usize(1) {
            let def_kind = self.tcx.def_kind(self.def_id);
            if matches!(def_kind, DefKind::Closure | DefKind::Generator) {
                // Skip visiting the closure/generator that is currently being processed. This only
                // happens because the first argument to the closure is a reference to itself and
                // that will call `visit_substs`, resulting in each generic parameter captured being
                // considered used by default.
                debug!("visit_local_decl: skipping closure substs");
                return;
            }
        }

        self.super_local_decl(local, local_decl);
    }

    fn visit_const(&mut self, c: &&'tcx Const<'tcx>, _: Location) {
        c.visit_with(self);
    }

    fn visit_ty(&mut self, ty: Ty<'tcx>, _: TyContext) {
        ty.visit_with(self);
    }
}

impl<'a, 'tcx> TypeVisitor<'tcx> for MarkUsedGenericParams<'a, 'tcx> {
    fn visit_const(&mut self, c: &'tcx Const<'tcx>) -> bool {
        debug!("visit_const: c={:?}", c);
        if !c.has_param_types_or_consts() {
            return false;
        }

        match c.val {
            ty::ConstKind::Param(param) => {
                debug!("visit_const: param={:?}", param);
                self.unused_parameters.clear(param.index);
                false
            }
            ty::ConstKind::Unevaluated(def, _, Some(p))
                // Avoid considering `T` unused when constants are of the form:
                //   `<Self as Foo<T>>::foo::promoted[p]`
                if self.def_id == def.did && !self.tcx.generics_of(def.did).has_self =>
            {
                // If there is a promoted, don't look at the substs - since it will always contain
                // the generic parameters, instead, traverse the promoted MIR.
                let promoted = self.tcx.promoted_mir(def.did);
                self.visit_body(&promoted[p]);
                false
            }
            ty::ConstKind::Unevaluated(def, unevaluated_substs, None)
                if self.tcx.def_kind(def.did) == DefKind::AnonConst =>
            {
                self.visit_child_body(def.did, unevaluated_substs);
                false
            }
            _ => c.super_visit_with(self),
        }
    }

    fn visit_ty(&mut self, ty: Ty<'tcx>) -> bool {
        debug!("visit_ty: ty={:?}", ty);
        if !ty.has_param_types_or_consts() {
            return false;
        }

        match *ty.kind() {
            ty::Closure(def_id, substs) | ty::Generator(def_id, substs, ..) => {
                debug!("visit_ty: def_id={:?}", def_id);
                // Avoid cycle errors with generators.
                if def_id == self.def_id {
                    return false;
                }

                // Consider any generic parameters used by any closures/generators as used in the
                // parent.
                self.visit_child_body(def_id, substs);
                false
            }
            ty::Param(param) => {
                debug!("visit_ty: param={:?}", param);
                self.unused_parameters.clear(param.index);
                false
            }
            _ => ty.super_visit_with(self),
        }
    }
}

/// Visitor used to check if a generic parameter is used.
struct HasUsedGenericParams<'a> {
    unused_parameters: &'a FiniteBitSet<u32>,
}

impl<'a, 'tcx> TypeVisitor<'tcx> for HasUsedGenericParams<'a> {
    fn visit_const(&mut self, c: &'tcx Const<'tcx>) -> bool {
        debug!("visit_const: c={:?}", c);
        if !c.has_param_types_or_consts() {
            return false;
        }

        match c.val {
            ty::ConstKind::Param(param) => {
                !self.unused_parameters.contains(param.index).unwrap_or(false)
            }
            _ => c.super_visit_with(self),
        }
    }

    fn visit_ty(&mut self, ty: Ty<'tcx>) -> bool {
        debug!("visit_ty: ty={:?}", ty);
        if !ty.has_param_types_or_consts() {
            return false;
        }

        match ty.kind() {
            ty::Param(param) => !self.unused_parameters.contains(param.index).unwrap_or(false),
            _ => ty.super_visit_with(self),
        }
    }
}

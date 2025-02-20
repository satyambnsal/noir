/// If the cursor is on an custom attribute, this struct will try to resolve its
/// underlying function and return a ReferenceId to it.
/// This is needed in hover and go-to-definition because when an annotation generates
/// code, that code ends up residing in the attribute definition (it ends up having the
/// attribute's span) so using the usual graph to locate what points to that location
/// will give not only the attribute function but also any type generated by it.
use std::collections::BTreeMap;

use fm::FileId;
use noirc_errors::Span;
use noirc_frontend::{
    ast::{AttributeTarget, Visitor},
    graph::CrateId,
    hir::{
        def_map::{CrateDefMap, LocalModuleId, ModuleId},
        resolution::import::resolve_import,
    },
    node_interner::ReferenceId,
    parser::ParsedSubModule,
    token::MetaAttribute,
    usage_tracker::UsageTracker,
    ParsedModule,
};

use crate::modules::module_def_id_to_reference_id;

pub(crate) struct AttributeReferenceFinder<'a> {
    byte_index: usize,
    /// The module ID in scope. This might change as we traverse the AST
    /// if we are analyzing something inside an inline module declaration.
    module_id: ModuleId,
    def_maps: &'a BTreeMap<CrateId, CrateDefMap>,
    reference_id: Option<ReferenceId>,
}

impl<'a> AttributeReferenceFinder<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        file: FileId,
        byte_index: usize,
        krate: CrateId,
        def_maps: &'a BTreeMap<CrateId, CrateDefMap>,
    ) -> Self {
        // Find the module the current file belongs to
        let def_map = &def_maps[&krate];
        let local_id = if let Some((module_index, _)) =
            def_map.modules().iter().find(|(_, module_data)| module_data.location.file == file)
        {
            LocalModuleId(module_index)
        } else {
            def_map.root()
        };
        let module_id = ModuleId { krate, local_id };
        Self { byte_index, module_id, def_maps, reference_id: None }
    }

    pub(crate) fn find(&mut self, parsed_module: &ParsedModule) -> Option<ReferenceId> {
        parsed_module.accept(self);

        self.reference_id
    }

    fn includes_span(&self, span: Span) -> bool {
        span.start() as usize <= self.byte_index && self.byte_index <= span.end() as usize
    }
}

impl<'a> Visitor for AttributeReferenceFinder<'a> {
    fn visit_parsed_submodule(&mut self, parsed_sub_module: &ParsedSubModule, _span: Span) -> bool {
        // Switch `self.module_id` to the submodule
        let previous_module_id = self.module_id;

        let def_map = &self.def_maps[&self.module_id.krate];
        if let Some(module_data) = def_map.modules().get(self.module_id.local_id.0) {
            if let Some(child_module) = module_data.children.get(&parsed_sub_module.name) {
                self.module_id = ModuleId { krate: self.module_id.krate, local_id: *child_module };
            }
        }

        parsed_sub_module.accept_children(self);

        // Restore the old module before continuing
        self.module_id = previous_module_id;

        false
    }

    fn visit_meta_attribute(
        &mut self,
        attribute: &MetaAttribute,
        _target: AttributeTarget,
    ) -> bool {
        if !self.includes_span(attribute.location.span) {
            return false;
        }

        let path = attribute.name.clone();
        // The path here must resolve to a function and it's a simple path (can't have turbofish)
        // so it can (and must) be solved as an import.
        let Ok(Some((module_def_id, _, _))) = resolve_import(
            path,
            self.module_id,
            self.def_maps,
            &mut UsageTracker::default(),
            None, // references tracker
        )
        .map(|result| result.namespace.values) else {
            return true;
        };

        self.reference_id = Some(module_def_id_to_reference_id(module_def_id));

        true
    }
}
